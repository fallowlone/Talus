//! Local port forwards that carry a network engine's connection: through an SSH server
//! (`ssh::open_tunnel`) or through an HTTP proxy with the `CONNECT` method (this module).
//! Either way the engine connects to `127.0.0.1:<local_port>`.

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::error::{DbError, DbResult};
use crate::model::{ConnectionConfig, HttpProxyConfig};
use crate::secret::Secret;

/// Upper bound for a proxy's response head; anything longer is not a CONNECT reply.
const MAX_HEAD: usize = 16 * 1024;

/// A running local port forward. Dropping it stops forwarding (and closes an SSH session).
pub struct Tunnel {
    pub local_port: u16,
    task: JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Tunnel {
    pub(crate) fn new(local_port: u16, task: JoinHandle<()>) -> Self {
        Self { local_port, task }
    }

    pub fn is_alive(&self) -> bool {
        !self.task.is_finished()
    }
}

/// Opens the tunnel `cfg` asks for (SSH or HTTP proxy) to `cfg.host:cfg.port`; `None` when it uses neither.
pub async fn open_for(
    cfg: &ConnectionConfig,
    secret: Option<&Secret>,
    known_hosts: &Path,
    timeout: Duration,
) -> DbResult<Option<Tunnel>> {
    if let Some(ssh) = &cfg.ssh {
        return crate::ssh::open_tunnel(ssh, secret, &cfg.host, cfg.port, known_hosts, timeout).await.map(Some);
    }
    if let Some(proxy) = &cfg.http_proxy {
        return open_http_tunnel(proxy, secret, &cfg.host, cfg.port, timeout).await.map(Some);
    }
    Ok(None)
}

/// Forwards `127.0.0.1:<local_port>` to `target_host:target_port` through an HTTP proxy's `CONNECT`.
/// One probe request runs first, so a wrong proxy address or credentials fail here with the
/// proxy's answer instead of surfacing later as a dropped database connection.
pub async fn open_http_tunnel(
    proxy: &HttpProxyConfig,
    secret: Option<&Secret>,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> DbResult<Tunnel> {
    let connect = Connect {
        proxy: (proxy.host.clone(), proxy.port),
        request: connect_request(proxy, secret, target_host, target_port),
        timeout,
    };
    drop(connect.open().await?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let connect = connect.clone();
            tokio::spawn(async move {
                match connect.open().await {
                    Ok(mut upstream) => {
                        if let Err(e) = tokio::io::copy_bidirectional(&mut sock, &mut upstream).await {
                            tracing::debug!(error = %e, "HTTP tunnel stream ended");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "HTTP proxy CONNECT failed"),
                }
            });
        }
    });
    Ok(Tunnel::new(local_port, task))
}

#[derive(Clone)]
struct Connect {
    proxy: (String, u16),
    request: String,
    timeout: Duration,
}

impl Connect {
    /// A proxy connection that has accepted `CONNECT`; bytes now flow to the target unchanged.
    async fn open(&self) -> DbResult<TcpStream> {
        let (host, port) = (&self.proxy.0, self.proxy.1);
        let attempt = async {
            let mut s = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| DbError::Connect(format!("HTTP proxy {host}:{port} is unreachable: {e}")))?;
            s.write_all(self.request.as_bytes()).await?;
            let status = read_head(&mut s).await?;
            check_status(&status)?;
            Ok::<_, DbError>(s)
        };
        tokio::time::timeout(self.timeout, attempt)
            .await
            .map_err(|_| DbError::Connect(format!("HTTP proxy {host}:{port} did not answer in time")))?
    }
}

fn connect_request(proxy: &HttpProxyConfig, secret: Option<&Secret>, host: &str, port: u16) -> String {
    // IPv6 literals are bracketed in an authority.
    let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.user.is_empty() {
        let pw = secret.map(|s| s.expose().to_string()).unwrap_or_default();
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{pw}", proxy.user));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    req
}

/// Reads the response head byte by byte (so nothing of the tunnelled stream is consumed) and
/// returns its status line.
async fn read_head(s: &mut TcpStream) -> DbResult<String> {
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(DbError::Connect("HTTP proxy sent an oversized response".into()));
        }
        if s.read(&mut byte).await? == 0 {
            return Err(DbError::Connect("HTTP proxy closed the connection during CONNECT".into()));
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    Ok(text.lines().next().unwrap_or_default().to_string())
}

fn check_status(status_line: &str) -> DbResult<()> {
    let code = status_line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(200..=299) => Ok(()),
        Some(407) => Err(DbError::Connect(
            "HTTP proxy authentication failed (407): check the proxy user name and password".into(),
        )),
        Some(_) => Err(DbError::Connect(format!("HTTP proxy refused the tunnel: {}", status_line.trim()))),
        None => Err(DbError::Connect(format!("not an HTTP proxy response: {}", status_line.trim()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(user: &str) -> HttpProxyConfig {
        HttpProxyConfig { host: "proxy".into(), port: 3128, user: user.into(), save_secret: false }
    }

    #[test]
    fn connect_request_carries_basic_auth_only_with_a_user() {
        let pw = Secret::new("s3cret");
        let r = connect_request(&proxy("alice"), Some(&pw), "db.internal", 5432);
        assert!(r.starts_with("CONNECT db.internal:5432 HTTP/1.1\r\nHost: db.internal:5432\r\n"));
        // base64("alice:s3cret")
        assert!(r.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0\r\n"));
        assert!(r.ends_with("\r\n\r\n"));
        let anon = connect_request(&proxy(""), None, "::1", 6379);
        assert!(anon.starts_with("CONNECT [::1]:6379 HTTP/1.1"));
        assert!(!anon.contains("Proxy-Authorization"));
    }

    #[test]
    fn status_lines_map_to_clear_errors() {
        assert!(check_status("HTTP/1.1 200 Connection established").is_ok());
        assert!(check_status("HTTP/1.0 200 OK").is_ok());
        assert!(
            check_status("HTTP/1.1 407 Proxy Authentication Required")
                .unwrap_err()
                .to_string()
                .contains("authentication")
        );
        assert!(check_status("HTTP/1.1 403 Forbidden").unwrap_err().to_string().contains("403 Forbidden"));
        assert!(check_status("SSH-2.0-OpenSSH").unwrap_err().to_string().contains("not an HTTP proxy"));
    }
}
