//! Client of `tunnel/talus-tunnel.php`: a local port forward for hosts where only a web server can reach
//! the database. The tunnel is one long `open` request that keeps a PHP process alive; every connection to
//! the local port becomes a *stream* inside it (`dial`, `send`, `close`) and everything the databases answer
//! comes back through one long-polling `recv`. The PHP process holds the real database connections, so the
//! engine's own wire protocol runs through unchanged — transactions, cancel requests (the engine opens
//! another connection to the local port) and logins all work as on a direct connection.
//!
//! Every request is signed (HMAC-SHA256 over operation, tunnel, stream, time, nonce, sequence, target and
//! body hash) with the tunnel key; `send` and `recv` carry a sequence number so a request repeated after a
//! lost response is recognised by the script instead of duplicating or dropping bytes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::hmac;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::error::{DbError, DbResult};
use crate::model::PhpTunnelConfig;
use crate::secret::Secret;
use crate::tunnel::Tunnel;

/// Longest response head or body line the client will read.
const MAX_HEAD: usize = 32 * 1024;
/// Bytes read from the local socket per `send` request.
const SEND_CHUNK: usize = 256 * 1024;
/// Requests that may be in flight at once (each occupies a PHP worker on the host).
const MAX_IN_FLIGHT: usize = 3;
/// Largest response body accepted (the script's `max_chunk` is 1 MiB by default).
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Long-poll length of `recv`; the script caps it at its own `max_wait_ms`.
const RECV_WAIT_MS: u64 = 4_000;
/// Clock helper for tests.
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Where the script lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Endpoint {
    /// Parses and checks a script address: `https://` anywhere, plain `http://` only for loopback (the
    /// signed requests are not encrypted).
    pub fn parse(url: &str) -> Result<Endpoint, String> {
        let url = url.trim();
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err("the tunnel address must start with https://".into());
        };
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err("the tunnel address must not contain a user name or password".into());
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or("malformed IPv6 address in the tunnel address")?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| "invalid port in the tunnel address")?,
                None => default_port,
            };
            (h.to_string(), port)
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            (h.to_string(), p.parse().map_err(|_| "invalid port in the tunnel address")?)
        } else {
            (authority.to_string(), default_port)
        };
        if host.is_empty() || host.contains(char::is_whitespace) || port == 0 {
            return Err("the tunnel address has no host".into());
        }
        let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1");
        if !tls && !loopback {
            return Err("a plain http:// tunnel address is only accepted for localhost; use https://".into());
        }
        let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
        if path.contains(['\r', '\n']) {
            return Err("the tunnel address contains a line break".into());
        }
        Ok(Endpoint { tls, host, port, path })
    }

    fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default = if self.tls { 443 } else { 80 };
        if self.port == default { host } else { format!("{host}:{}", self.port) }
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// One HTTP/1.1 connection (kept alive between requests).
struct HttpConn {
    io: BufReader<Box<dyn Stream>>,
}

#[derive(Debug)]
struct Response {
    status: u16,
    /// The script's `X-Talus-Status`.
    talus: String,
    body: Vec<u8>,
    close: bool,
}

impl HttpConn {
    async fn connect(ep: &Endpoint, timeout: Duration) -> DbResult<Self> {
        let attempt = async {
            let tcp = TcpStream::connect((ep.host.as_str(), ep.port)).await.map_err(|e| {
                DbError::Connect(format!("the tunnel script at {} is unreachable: {e}", ep.authority()))
            })?;
            tcp.set_nodelay(true).ok();
            let io: Box<dyn Stream> = if ep.tls {
                let connector = native_tls::TlsConnector::new().map_err(|e| DbError::Connect(format!("TLS: {e}")))?;
                let tls = tokio_native_tls::TlsConnector::from(connector)
                    .connect(&ep.host, tcp)
                    .await
                    .map_err(|e| DbError::Connect(format!("TLS handshake with {} failed: {e}", ep.host)))?;
                Box::new(tls)
            } else {
                Box::new(tcp)
            };
            Ok::<_, DbError>(HttpConn { io: BufReader::new(io) })
        };
        tokio::time::timeout(timeout, attempt)
            .await
            .map_err(|_| DbError::Connect(format!("the tunnel script at {} did not answer in time", ep.authority())))?
    }

    /// Sends one POST and reads the whole response. `Err(retryable)` is returned when nothing of a
    /// response arrived (typically a keep-alive connection the server had already closed).
    async fn post(
        &mut self,
        ep: &Endpoint,
        headers: &[(String, String)],
        body: &[u8],
        timeout: Duration,
    ) -> Result<Response, (DbError, bool)> {
        let mut head = format!("POST {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n", ep.path, ep.authority());
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", body.len()));
        let work = async {
            let io = self.io.get_mut();
            io.write_all(head.as_bytes()).await.map_err(|e| (DbError::Io(e.to_string()), true))?;
            io.write_all(body).await.map_err(|e| (DbError::Io(e.to_string()), true))?;
            io.flush().await.map_err(|e| (DbError::Io(e.to_string()), true))?;
            read_response(&mut self.io).await
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| (DbError::Io("the tunnel script did not answer in time".into()), false))?
    }
}

async fn read_line(io: &mut BufReader<Box<dyn Stream>>, limit: usize) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let n = io.take(limit as u64).read_until(b'\n', &mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string()))
}

/// Reads a response head, then its body (`Content-Length`, chunked, or until the connection closes).
async fn read_response(io: &mut BufReader<Box<dyn Stream>>) -> Result<Response, (DbError, bool)> {
    let io_err = |e: std::io::Error, first: bool| (DbError::Io(e.to_string()), first);
    let status_line = match read_line(io, MAX_HEAD).await {
        Ok(Some(l)) => l,
        Ok(None) => return Err((DbError::ConnectionLost("the tunnel script closed the connection".into()), true)),
        Err(e) => return Err(io_err(e, true)),
    };
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| (DbError::Io(format!("not an HTTP response: {}", status_line.trim())), false))?;
    let (mut length, mut chunked, mut close, mut talus) = (None::<usize>, false, false, String::new());
    loop {
        let line = match read_line(io, MAX_HEAD).await {
            Ok(Some(l)) => l,
            Ok(None) => {
                return Err((DbError::ConnectionLost("truncated response from the tunnel script".into()), false));
            }
            Err(e) => return Err(io_err(e, false)),
        };
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "content-length" => length = v.parse().ok(),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "connection" => close = v.eq_ignore_ascii_case("close"),
            "x-talus-status" => talus = v.to_string(),
            _ => {}
        }
    }
    let body = if status == 204 || status == 304 {
        Vec::new()
    } else if chunked {
        let mut body = Vec::new();
        loop {
            let size_line = match read_line(io, 1024).await {
                Ok(Some(l)) => l,
                _ => return Err((DbError::ConnectionLost("truncated response from the tunnel script".into()), false)),
            };
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| (DbError::Io("malformed chunked response".into()), false))?;
            if size == 0 {
                // Trailers, then the blank line.
                while let Ok(Some(l)) = read_line(io, MAX_HEAD).await {
                    if l.is_empty() {
                        break;
                    }
                }
                break;
            }
            if body.len() + size > MAX_BODY {
                return Err((DbError::Io("the tunnel script sent an oversized response".into()), false));
            }
            let start = body.len();
            body.resize(start + size, 0);
            io.read_exact(&mut body[start..]).await.map_err(|e| io_err(e, false))?;
            let mut crlf = [0u8; 2];
            io.read_exact(&mut crlf).await.map_err(|e| io_err(e, false))?;
        }
        body
    } else if let Some(n) = length {
        if n > MAX_BODY {
            return Err((DbError::Io("the tunnel script sent an oversized response".into()), false));
        }
        let mut body = vec![0u8; n];
        io.read_exact(&mut body).await.map_err(|e| io_err(e, false))?;
        body
    } else {
        let mut body = Vec::new();
        io.take(MAX_BODY as u64).read_to_end(&mut body).await.map_err(|e| io_err(e, false))?;
        close = true;
        body
    };
    Ok(Response { status, talus, body, close })
}

/// Everything the requests of one tunnel share.
struct Ctx {
    ep: Endpoint,
    key: hmac::Key,
    /// This tunnel's id (the script keeps its state under it).
    tid: String,
    /// `host:port` the streams dial.
    target: String,
    timeout: Duration,
    /// Longest `recv` long-poll the script accepts.
    wait_ms: AtomicU64,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

const NO_STREAM: &str = "00000000000000000000000000000000";

impl Ctx {
    /// The headers of one signed request. A fresh nonce and timestamp every time; `seq` is what the script
    /// uses to recognise a repeated request.
    fn headers(
        &self,
        op: &str,
        sid: &str,
        seq: u64,
        target: &str,
        body: &[u8],
        wait: Option<u64>,
    ) -> Vec<(String, String)> {
        let (ts, nonce) = (now_secs(), random_hex());
        let material = format!(
            "talus2\n{op}\n{}\n{sid}\n{ts}\n{nonce}\n{seq}\n{target}\n{}",
            self.tid,
            hex(ring::digest::digest(&ring::digest::SHA256, body).as_ref())
        );
        let sig = hex(hmac::sign(&self.key, material.as_bytes()).as_ref());
        let mut h = vec![
            ("X-Talus-Op".to_string(), op.to_string()),
            ("X-Talus-Tid".to_string(), self.tid.clone()),
            ("X-Talus-Sid".to_string(), sid.to_string()),
            ("X-Talus-Ts".to_string(), ts.to_string()),
            ("X-Talus-Nonce".to_string(), nonce),
            ("X-Talus-Seq".to_string(), seq.to_string()),
            ("X-Talus-Target".to_string(), target.to_string()),
            ("X-Talus-Sig".to_string(), sig),
        ];
        if let Some(w) = wait {
            h.push(("X-Talus-Wait".to_string(), w.to_string()));
        }
        h
    }
}

/// One keep-alive connection used for a series of requests; a connection the server dropped is replaced
/// once per request (the request is re-signed with a new nonce and the same sequence number).
struct Lane {
    conn: Option<HttpConn>,
}

/// One signed request to the tunnel script.
struct Call<'a> {
    op: &'a str,
    sid: &'a str,
    seq: u64,
    target: &'a str,
    body: &'a [u8],
    wait: Option<u64>,
}

impl Lane {
    async fn call(&mut self, ctx: &Ctx, c: Call<'_>) -> DbResult<Response> {
        let timeout = match (c.op, c.wait) {
            ("recv", Some(w)) => Duration::from_millis(w) + Duration::from_secs(15),
            ("send", _) => Duration::from_secs(80),
            ("dial", _) => ctx.timeout + Duration::from_secs(20),
            _ => ctx.timeout.max(Duration::from_secs(20)),
        };
        for attempt in 0..2 {
            let reused = self.conn.is_some();
            let mut conn = match self.conn.take() {
                Some(conn) => conn,
                None => HttpConn::connect(&ctx.ep, ctx.timeout).await?,
            };
            let headers = ctx.headers(c.op, c.sid, c.seq, c.target, c.body, c.wait);
            match conn.post(&ctx.ep, &headers, c.body, timeout).await {
                Ok(r) => {
                    if !r.close {
                        self.conn = Some(conn);
                    }
                    return Ok(r);
                }
                Err((e, no_response)) if attempt == 0 && reused && no_response => {
                    tracing::debug!(error = %e, "tunnel connection was stale; retrying on a new one");
                }
                Err((e, _)) => return Err(e),
            }
        }
        Err(DbError::Io("the tunnel script is unreachable".into()))
    }
}

/// What the script said about a failed request.
fn script_error(r: &Response) -> String {
    let text = String::from_utf8_lossy(&r.body);
    let msg = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
        .unwrap_or_else(|| text.chars().take(200).collect());
    match r.status {
        401 => {
            format!("the tunnel script rejected the request ({msg}): check the tunnel key and that the clocks agree")
        }
        404 => "the tunnel script was not found at this address (HTTP 404)".to_string(),
        503 => format!("the tunnel script is not configured ({msg})"),
        s if msg.is_empty() => format!("the tunnel script answered HTTP {s}"),
        s => format!("the tunnel script answered HTTP {s}: {msg}"),
    }
}

/// A stream's incoming side.
enum Chunk {
    Data(Vec<u8>),
    /// The database closed the connection.
    End,
    /// The tunnel failed; the text says why.
    Failed(String),
}

/// One open tunnel: the owner request, the receive loop and the pool of request lanes.
struct Mux {
    ctx: Arc<Ctx>,
    streams: Mutex<HashMap<String, mpsc::UnboundedSender<Chunk>>>,
    /// Why the tunnel is unusable (set once).
    dead: Mutex<Option<String>>,
    /// Failure text the script gave on the `open` request.
    open_error: Mutex<Option<String>>,
    lanes: tokio::sync::Mutex<Vec<Lane>>,
    in_flight: Semaphore,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Mux {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.lock() {
            for t in tasks.iter() {
                t.abort();
            }
        }
        // Ask the script to end the tunnel now instead of when its idle timer runs out.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let ctx = self.ctx.clone();
            handle.spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    Lane { conn: None }
                        .call(&ctx, Call { op: "quit", sid: NO_STREAM, seq: 1, target: "", body: b"", wait: None }),
                )
                .await;
            });
        }
    }
}

impl Mux {
    async fn start(ctx: Arc<Ctx>) -> Arc<Self> {
        let mux = Arc::new(Mux {
            ctx,
            streams: Mutex::new(HashMap::new()),
            dead: Mutex::new(None),
            open_error: Mutex::new(None),
            lanes: tokio::sync::Mutex::new(Vec::new()),
            in_flight: Semaphore::new(MAX_IN_FLIGHT),
            tasks: Mutex::new(Vec::new()),
        });
        let owner = tokio::spawn(Self::run_owner(mux.clone()));
        let down = tokio::spawn(Self::run_receiver(mux.clone()));
        if let Ok(mut t) = mux.tasks.lock() {
            t.push(owner);
            t.push(down);
        }
        mux
    }

    fn fail_all(&self, why: String) {
        if let Ok(mut dead) = self.dead.lock()
            && dead.is_none()
        {
            *dead = Some(why.clone());
        }
        if let Ok(mut streams) = self.streams.lock() {
            for (_, tx) in streams.drain() {
                let _ = tx.send(Chunk::Failed(why.clone()));
            }
        }
    }

    fn is_dead(&self) -> Option<String> {
        self.dead.lock().ok().and_then(|d| d.clone())
    }

    /// The `open` request: answered at once only if the script cannot serve the tunnel, otherwise it
    /// streams until the tunnel ends.
    async fn run_owner(mux: Arc<Mux>) {
        let ctx = &mux.ctx;
        let result: DbResult<()> = async {
            let mut conn = HttpConn::connect(&ctx.ep, ctx.timeout).await?;
            let headers = ctx.headers("open", NO_STREAM, 0, "", b"", None);
            let mut head =
                format!("POST {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", ctx.ep.path, ctx.ep.authority());
            for (k, v) in &headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("Content-Type: application/octet-stream\r\nContent-Length: 0\r\n\r\n");
            conn.io.get_mut().write_all(head.as_bytes()).await.map_err(|e| DbError::Io(e.to_string()))?;
            let status_line = read_line(&mut conn.io, MAX_HEAD)
                .await
                .map_err(|e| DbError::Io(e.to_string()))?
                .ok_or_else(|| DbError::ConnectionLost("the tunnel script closed the tunnel request".into()))?;
            let status: u16 = status_line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
            let (mut chunked, mut length) = (false, None::<usize>);
            while let Some(l) = read_line(&mut conn.io, MAX_HEAD).await.map_err(|e| DbError::Io(e.to_string()))? {
                if l.is_empty() {
                    break;
                }
                let lower = l.to_ascii_lowercase();
                chunked |= lower.starts_with("transfer-encoding:") && lower.contains("chunked");
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().ok();
                }
            }
            if status != 200 {
                let mut body = Vec::new();
                if !chunked && let Some(n) = length {
                    body.resize(n.min(MAX_BODY), 0);
                    let _ = conn.io.read_exact(&mut body).await;
                } else {
                    let _ = (&mut conn.io).take(4096).read_to_end(&mut body).await;
                }
                let why = script_error(&Response { status, talus: String::new(), body, close: true });
                if let Ok(mut slot) = mux.open_error.lock() {
                    *slot = Some(why);
                }
                return Ok(());
            }
            // Alive: read (and discard) until the script ends the request.
            let mut sink = [0u8; 4096];
            while conn.io.read(&mut sink).await.map_err(|e| DbError::Io(e.to_string()))? > 0 {}
            Ok(())
        }
        .await;
        let why = match (result, mux.open_error.lock().ok().and_then(|e| e.clone())) {
            (_, Some(why)) => why,
            (Err(e), None) => e.to_string(),
            (Ok(()), None) => "the tunnel script ended the tunnel (idle or lifetime limit)".to_string(),
        };
        mux.fail_all(why);
    }

    /// Long-polls `recv` and hands the frames to their streams.
    async fn run_receiver(mux: Arc<Mux>) {
        let ctx = mux.ctx.clone();
        let mut lane = Lane { conn: None };
        let mut seq = 1u64;
        let started = tokio::time::Instant::now();
        loop {
            let wait = RECV_WAIT_MS.min(ctx.wait_ms.load(Ordering::Relaxed));
            let r = match lane
                .call(&ctx, Call { op: "recv", sid: NO_STREAM, seq, target: "", body: b"", wait: Some(wait) })
                .await
            {
                Ok(r) => r,
                Err(e) => return mux.fail_all(e.to_string()),
            };
            if r.status == 410 && seq == 1 && started.elapsed() < ctx.timeout {
                // The script is still starting the tunnel: ask again.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if r.status != 200 {
                let why = mux.open_error.lock().ok().and_then(|e| e.clone()).unwrap_or_else(|| script_error(&r));
                return mux.fail_all(why);
            }
            if let Err(e) = mux.dispatch(&r.body) {
                return mux.fail_all(e);
            }
            seq += 1;
        }
    }

    /// Frames: stream(16) flags(1) len(4) data.
    fn dispatch(&self, mut body: &[u8]) -> Result<(), String> {
        while !body.is_empty() {
            if body.len() < 21 {
                return Err("the tunnel script sent a truncated frame".into());
            }
            let sid = hex(&body[..16]);
            let ended = body[16] & 1 == 1;
            let len = u32::from_be_bytes([body[17], body[18], body[19], body[20]]) as usize;
            let rest = &body[21..];
            if rest.len() < len {
                return Err("the tunnel script sent a truncated frame".into());
            }
            let mut streams = self.streams.lock().map_err(|_| "tunnel state poisoned".to_string())?;
            if let Some(tx) = streams.get(&sid) {
                if len > 0 {
                    let _ = tx.send(Chunk::Data(rest[..len].to_vec()));
                }
                if ended {
                    let _ = tx.send(Chunk::End);
                    streams.remove(&sid);
                }
            }
            body = &rest[len..];
        }
        Ok(())
    }

    /// Runs one request on a free lane (at most `MAX_IN_FLIGHT` at a time).
    async fn request(&self, op: &str, sid: &str, seq: u64, target: &str, body: &[u8]) -> DbResult<Response> {
        let _permit = self.in_flight.acquire().await.map_err(|_| DbError::Io("tunnel closed".into()))?;
        let mut lane = self.lanes.lock().await.pop().unwrap_or(Lane { conn: None });
        let r = lane.call(&self.ctx, Call { op, sid, seq, target, body, wait: None }).await;
        self.lanes.lock().await.push(lane);
        r
    }

    /// Opens a stream to the target; the script's answer says why when it refuses.
    async fn dial(&self, sid: &str) -> DbResult<mpsc::UnboundedReceiver<Chunk>> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(why) = self.is_dead() {
            return Err(DbError::Connect(why));
        }
        self.streams.lock().map_err(|_| DbError::Io("tunnel state poisoned".into()))?.insert(sid.to_string(), tx);
        let started = tokio::time::Instant::now();
        loop {
            let r = self.request("dial", sid, 0, &self.ctx.target, b"").await;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    self.forget(sid);
                    return Err(e);
                }
            };
            if r.status == 410 && started.elapsed() < self.ctx.timeout && self.is_dead().is_none() {
                // The owner request is still starting.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if r.status != 200 {
                self.forget(sid);
                let why = self.open_error.lock().ok().and_then(|e| e.clone()).unwrap_or_else(|| script_error(&r));
                return Err(DbError::Connect(why));
            }
            return Ok(rx);
        }
    }

    fn forget(&self, sid: &str) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.remove(sid);
        }
    }
}

/// Moves bytes between a local socket and one stream until either side ends.
async fn pump(local: TcpStream, mux: Arc<Mux>) -> DbResult<()> {
    let sid = random_hex();
    let mut incoming = mux.dial(&sid).await?;
    let (mut lr, mut lw) = local.into_split();
    let up = {
        let (mux, sid) = (mux.clone(), sid.clone());
        async move {
            let mut buf = vec![0u8; SEND_CHUNK];
            let mut seq = 1u64;
            loop {
                let n = lr.read(&mut buf).await.map_err(|e| DbError::Io(e.to_string()))?;
                if n == 0 {
                    return Ok::<_, DbError>(());
                }
                let r = mux.request("send", &sid, seq, "", &buf[..n]).await?;
                if r.status != 200 {
                    return Err(DbError::Io(mux.is_dead().unwrap_or_else(|| script_error(&r))));
                }
                if r.talus == "closed" {
                    return Ok(());
                }
                seq += 1;
            }
        }
    };
    let down = async {
        loop {
            match incoming.recv().await {
                Some(Chunk::Data(d)) => lw.write_all(&d).await.map_err(|e| DbError::Io(e.to_string()))?,
                Some(Chunk::End) | None => {
                    let _ = lw.shutdown().await;
                    return Ok::<_, DbError>(());
                }
                Some(Chunk::Failed(why)) => return Err(DbError::ConnectionLost(why)),
            }
        }
    };
    tokio::pin!(up, down);
    // Either direction ending ends the stream.
    let result = tokio::select! {
        r = &mut up => r,
        r = &mut down => r,
    };
    mux.forget(&sid);
    let _ = mux.request("close", &sid, 1, "", b"").await;
    result
}

/// The script's self-report (`ping`): proves address, TLS, key and PHP setup in one request.
async fn ping(ctx: &Ctx) -> DbResult<serde_json::Value> {
    let r = Lane { conn: None }
        .call(ctx, Call { op: "ping", sid: NO_STREAM, seq: 0, target: "", body: b"", wait: None })
        .await?;
    if r.status != 200 {
        return Err(DbError::Connect(script_error(&r)));
    }
    serde_json::from_slice(&r.body)
        .map_err(|_| DbError::Connect("the address does not answer like the Talus tunnel script".into()))
}

/// Forwards `127.0.0.1:<local_port>` to `target_host:target_port` through the tunnel script. A probe runs
/// first (ping, then one stream that connects to the target), so a wrong address, key, allow-list entry or
/// unreachable database fail here with the script's answer instead of as a dropped database connection.
pub async fn open_php_tunnel(
    cfg: &PhpTunnelConfig,
    key: Option<&Secret>,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> DbResult<Tunnel> {
    let ep = Endpoint::parse(&cfg.url).map_err(DbError::Connect)?;
    let key = key.filter(|k| !k.is_empty()).ok_or_else(|| DbError::Connect("the tunnel key is required".into()))?;
    let target = if target_host.contains(':') {
        format!("[{target_host}]:{target_port}")
    } else {
        format!("{target_host}:{target_port}")
    };
    let ctx = Arc::new(Ctx {
        ep,
        key: hmac::Key::new(hmac::HMAC_SHA256, key.expose().as_bytes()),
        tid: random_hex(),
        target,
        timeout,
        wait_ms: AtomicU64::new(RECV_WAIT_MS),
    });
    let info = ping(&ctx).await?;
    if let Some(ms) = info.get("max_wait_ms").and_then(serde_json::Value::as_u64) {
        ctx.wait_ms.store(ms.saturating_sub(2000).clamp(500, RECV_WAIT_MS), Ordering::Relaxed);
    }
    let mux = Mux::start(ctx).await;
    // Probe: one stream that reaches the database and ends again (dropping `mux` ends the tunnel on failure).
    let sid = random_hex();
    drop(mux.dial(&sid).await?);
    mux.forget(&sid);
    let _ = mux.request("close", &sid, 1, "", b"").await;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        let mut streams = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((sock, _)) = accepted else { break };
                    let mux = mux.clone();
                    streams.spawn(async move {
                        if let Err(e) = pump(sock, mux).await {
                            tracing::warn!(error = %e, "PHP tunnel stream ended");
                        }
                    });
                }
                Some(_) = streams.join_next() => {}
            }
        }
    });
    Ok(Tunnel::new(local_port, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_parsed_and_plain_http_is_loopback_only() {
        let e = Endpoint::parse("https://example.com/db/tunnel.php").unwrap();
        assert_eq!((e.tls, e.host.as_str(), e.port, e.path.as_str()), (true, "example.com", 443, "/db/tunnel.php"));
        let e = Endpoint::parse("https://example.com:8443").unwrap();
        assert_eq!((e.port, e.path.as_str()), (8443, "/"));
        let e = Endpoint::parse("http://127.0.0.1:8099/t.php?x=1").unwrap();
        assert_eq!((e.tls, e.port, e.path.as_str()), (false, 8099, "/t.php?x=1"));
        assert_eq!(Endpoint::parse("https://[::1]:9/t.php").unwrap().host, "::1");
        for bad in [
            "http://example.com/t.php",
            "ftp://example.com/",
            "example.com/t.php",
            "https://user:pw@example.com/t.php",
            "https:///t.php",
            "https://example.com:0/",
            "https://example.com:x/",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    fn test_ctx() -> Ctx {
        Ctx {
            ep: Endpoint::parse("https://h/t.php").unwrap(),
            key: hmac::Key::new(hmac::HMAC_SHA256, b"k"),
            tid: "b".repeat(32),
            target: "db:3306".into(),
            timeout: Duration::from_secs(1),
            wait_ms: AtomicU64::new(1),
        }
    }

    #[test]
    fn requests_are_signed_over_every_field() {
        let ctx = test_ctx();
        let h = ctx.headers("send", &"a".repeat(32), 7, "db:3306", b"body", None);
        let get = |name: &str| h.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap();
        let material = format!(
            "talus2\nsend\n{}\n{}\n{}\n{}\n7\ndb:3306\n{}",
            "b".repeat(32),
            "a".repeat(32),
            get("X-Talus-Ts"),
            get("X-Talus-Nonce"),
            hex(ring::digest::digest(&ring::digest::SHA256, b"body").as_ref())
        );
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"k");
        assert_eq!(get("X-Talus-Sig"), hex(hmac::sign(&key, material.as_bytes()).as_ref()));
        // A second request never repeats a nonce.
        let again = ctx.headers("send", &"a".repeat(32), 7, "db:3306", b"body", None);
        assert_ne!(get("X-Talus-Nonce"), again.iter().find(|(k, _)| k == "X-Talus-Nonce").unwrap().1);
    }

    #[test]
    fn frames_are_split_per_stream_and_ends_are_reported() {
        let mux = Mux {
            ctx: Arc::new(test_ctx()),
            streams: Mutex::new(HashMap::new()),
            dead: Mutex::new(None),
            open_error: Mutex::new(None),
            lanes: tokio::sync::Mutex::new(Vec::new()),
            in_flight: Semaphore::new(1),
            tasks: Mutex::new(Vec::new()),
        };
        let (a, b) = ("a".repeat(32), "b".repeat(32));
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        mux.streams.lock().unwrap().extend([(a.clone(), tx_a), (b.clone(), tx_b)]);
        let frame = |sid: &str, ended: bool, data: &[u8]| {
            let mut f: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&sid[i * 2..i * 2 + 2], 16).unwrap()).collect();
            f.push(u8::from(ended));
            f.extend((data.len() as u32).to_be_bytes());
            f.extend(data);
            f
        };
        let mut body = frame(&a, false, b"hello");
        body.extend(frame(&b, true, b"bye"));
        body.extend(frame(&a, false, b" world"));
        body.extend(frame(&"c".repeat(32), false, b"nobody listens"));
        mux.dispatch(&body).unwrap();
        let mut got_a = Vec::new();
        while let Ok(Chunk::Data(d)) = rx_a.try_recv() {
            got_a.extend(d);
        }
        assert_eq!(got_a, b"hello world");
        assert!(matches!(rx_b.try_recv(), Ok(Chunk::Data(d)) if d == b"bye"));
        assert!(matches!(rx_b.try_recv(), Ok(Chunk::End)));
        assert!(!mux.streams.lock().unwrap().contains_key(&b), "an ended stream is forgotten");
        assert!(mux.dispatch(&body[..20]).is_err(), "a truncated frame is an error");
    }

    #[test]
    fn script_errors_are_explained() {
        let r = |status: u16, body: &str| Response {
            status,
            talus: String::new(),
            body: body.as_bytes().to_vec(),
            close: false,
        };
        assert!(script_error(&r(401, "{\"error\":\"bad signature\"}")).contains("tunnel key"));
        assert!(script_error(&r(403, "{\"error\":\"this target is not allowed\"}")).contains("not allowed"));
        assert!(script_error(&r(404, "")).contains("not found"));
        assert!(script_error(&r(500, "<html>boom</html>")).contains("HTTP 500"));
    }
}
