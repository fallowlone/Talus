//! The PHP tunnel script (`tunnel/talus-tunnel.php`) against the real client, run by the PHP command line
//! server (`php -S`, several workers). Skips when `php` is not installed. The database tests need
//! `TALUS_TEST_PG` (PostgreSQL) / `TALUS_TEST_MYSQL`; the protocol tests need only PHP.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ring::hmac;
use talus_core::engine::{ScriptOptions, Session};
use talus_core::model::{ConnectionConfig, Engine, Outcome, PhpTunnelConfig, SslMode};
use talus_core::tunnel::open_for;
use talus_core::{DbError, Secret};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const KEY: &str = "0123456789abcdef0123456789abcdef-test-key";
const TIMEOUT: Duration = Duration::from_secs(10);
/// Tunnel id of the hand-made requests.
const TID: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

struct Php {
    child: Option<Child>,
    port: u16,
    /// A web server that was already running: its allow list is whatever it was started with.
    external: bool,
}

impl Drop for Php {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            // `php -S` with workers: the workers are children of the process that is killed.
            let _ = Command::new("pkill").args(["-P", &c.id().to_string()]).status();
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn php_available() -> bool {
    Command::new("php").arg("-v").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts the script with `allow` as its target list. With `TALUS_TEST_PHP_URL` set (see
/// `docker-compose.test.yml`, profile `php`) the tests use that already running web server instead.
async fn start_php(allow: &str) -> Php {
    if let Ok(url) = std::env::var("TALUS_TEST_PHP_URL") {
        let port =
            url.rsplit(':').next().and_then(|p| p.split('/').next()).and_then(|p| p.parse().ok()).expect("port in URL");
        return Php { child: None, port, external: true };
    }
    let port = free_port();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tunnel");
    let child = Command::new("php")
        .args(["-d", "variables_order=EGPCS", "-S", &format!("127.0.0.1:{port}"), "-t"])
        .arg(&dir)
        .env("TALUS_TUNNEL_KEY", KEY)
        .env("TALUS_TUNNEL_ALLOW", allow)
        .env("PHP_CLI_SERVER_WORKERS", "12")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create("/tmp/claude-501/phpsrv.log").unwrap())
        .spawn()
        .expect("start php");
    let php = Php { child: Some(child), port, external: false };
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return php;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("php server did not start");
}

fn tunnel_cfg(php: &Php, engine: Engine, host: &str, port: u16) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(engine, "tunnelled");
    cfg.host = host.into();
    cfg.port = port;
    cfg.ssl_mode = SslMode::Disable;
    cfg.php_tunnel =
        Some(PhpTunnelConfig { url: format!("http://127.0.0.1:{}/talus-tunnel.php", php.port), save_secret: false });
    cfg
}

fn pg_parts() -> Option<(String, u16, String, String, String)> {
    let v = std::env::var("TALUS_TEST_PG").ok()?;
    let p: Vec<&str> = v.splitn(5, ':').collect();
    Some((p[0].into(), p[1].parse().unwrap(), p[2].into(), p[3].into(), p[4].into()))
}

/// A PostgreSQL connection that goes through the tunnel script.
async fn pg_through(php: &Php) -> Option<(ConnectionConfig, Secret, talus_core::tunnel::Tunnel)> {
    let (mut host, port, user, pw, db) = pg_parts()?;
    if php.external {
        // The web server runs in a container: the database is the host's published port.
        host = std::env::var("TALUS_TEST_PHP_DB_HOST").unwrap_or_else(|_| "host.docker.internal".into());
    }
    let mut cfg = tunnel_cfg(php, Engine::Postgres, &host, port);
    cfg.user = user;
    cfg.database = db;
    let tunnel = open_for(&cfg, Some(&Secret::new(KEY)), std::path::Path::new("/nonexistent"), TIMEOUT)
        .await
        .expect("tunnel opens")
        .expect("a PHP tunnel");
    let mut local = cfg.clone();
    local.host = "127.0.0.1".into();
    local.port = tunnel.local_port;
    local.php_tunnel = None;
    Some((local, Secret::new(pw), tunnel))
}

/// The single number a `SELECT count(*)` returns.
async fn count(s: &mut Session, sql: &str) -> String {
    let r = s.run_script(sql, &ScriptOptions::default()).await;
    rows(&r.results[0].outcome).rows[0][0].clone().unwrap()
}

fn rows(o: &Outcome) -> &talus_core::model::QueryResult {
    match o {
        Outcome::Rows(r) => r,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_works_through_the_script_with_transactions_cancel_and_big_results() {
    if !php_available() {
        eprintln!("php not installed; skipping");
        return;
    }
    let Some((host, port, ..)) = pg_parts() else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let php = start_php(&format!("{host}:{port}")).await;
    let (cfg, pw, _tunnel) = pg_through(&php).await.unwrap();
    let opts = ScriptOptions::default();
    let mut s = Session::connect(&cfg, Some(&pw), None).await.expect("login through the tunnel");
    // The login handshake (SCRAM) travelled through the script: the password never reached it.
    assert!(s.server_version().await.unwrap().contains("PostgreSQL"));

    // A transaction lives across requests; a second connection does not see it until COMMIT.
    let t = format!("tun_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let r = s.run_script(&format!("CREATE TABLE {t} (id int PRIMARY KEY, v text)"), &opts).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let mut other = Session::connect(&cfg, Some(&pw), None).await.unwrap();
    let r =
        s.run_script(&format!("BEGIN; INSERT INTO {t} VALUES (1, 'a'); INSERT INTO {t} VALUES (2, 'b')"), &opts).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let count_sql = format!("SELECT count(*) FROM {t}");
    assert_eq!(count(&mut other, &count_sql).await, "0", "uncommitted rows are invisible to the other session");
    assert_eq!(count(&mut s, &count_sql).await, "2");
    s.run_script("ROLLBACK", &opts).await;
    assert_eq!(count(&mut s, &count_sql).await, "0", "rolled back");
    let r = s.run_script(&format!("BEGIN; INSERT INTO {t} VALUES (3, 'c'); COMMIT"), &opts).await;
    assert!(r.errors.is_empty());
    assert_eq!(count(&mut other, &count_sql).await, "1", "committed rows are visible");
    // A large result crosses many requests intact (every row's text is checked through a checksum).
    let r = s
        .run_script(
            "SELECT count(*), sum(length(repeat('x', 100 + (g % 50)))), md5(string_agg(g::text, ',' ORDER BY g)) \
             FROM generate_series(1, 120000) g",
            &opts,
        )
        .await;
    let row = &rows(&r.results[0].outcome).rows[0];
    assert_eq!(row[0].as_deref(), Some("120000"));
    let r = s
        .run_script(
            "SELECT g, repeat('y', 200) FROM generate_series(1, 60000) g",
            &ScriptOptions { limits: talus_core::model::FetchLimits { max_rows: 100_000 }, ..opts },
        )
        .await;
    assert_eq!(rows(&r.results[0].outcome).rows.len(), 60000, "{:?}", r.errors);
    // A running statement is cancelled with PostgreSQL's own protocol, which opens another tunnel session.
    let cancel = s.cancel_handle();
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let r = tokio::time::timeout(Duration::from_secs(20), s.run_script("SELECT pg_sleep(30)", &opts))
        .await
        .expect("cancelled");
    canceller.await.unwrap().unwrap();
    assert!(r.cancelled, "{:?}", r.errors);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(count(&mut s, &count_sql).await, "1", "the session is usable after the cancel");

    let r = s.run_script(&format!("DROP TABLE {t}"), &opts).await;
    assert!(r.errors.is_empty());
    other.close().await.unwrap();
    s.close().await.unwrap();
}

/// MySQL and Redis (and so any engine) take the very same road: only the wire protocol differs.
#[tokio::test(flavor = "multi_thread")]
async fn mysql_and_redis_work_through_the_same_script() {
    if !php_available() {
        eprintln!("php not installed; skipping");
        return;
    }
    let env = |var: &str| -> Option<Vec<String>> {
        std::env::var(var).ok().map(|v| v.splitn(5, ':').map(String::from).collect())
    };
    let (my, redis) = (env("TALUS_TEST_MYSQL"), env("TALUS_TEST_REDIS"));
    if my.is_none() && redis.is_none() {
        eprintln!("TALUS_TEST_MYSQL / TALUS_TEST_REDIS not set; skipping");
        return;
    }
    let allow: Vec<String> = [&my, &redis].into_iter().flatten().map(|p| format!("{}:{}", p[0], p[1])).collect();
    let php = start_php(&allow.join(",")).await;
    let dial = |engine: Engine, p: &[String]| {
        let host = if php.external {
            std::env::var("TALUS_TEST_PHP_DB_HOST").unwrap_or_else(|_| "host.docker.internal".into())
        } else {
            p[0].clone()
        };
        let mut cfg = tunnel_cfg(&php, engine, &host, p[1].parse().unwrap());
        cfg.user = p[2].clone();
        cfg.database = p[4].clone();
        cfg
    };
    let through = |cfg: ConnectionConfig| async move {
        let tunnel = open_for(&cfg, Some(&Secret::new(KEY)), std::path::Path::new("/nonexistent"), TIMEOUT)
            .await
            .expect("tunnel opens")
            .expect("a PHP tunnel");
        let mut local = cfg;
        (local.host, local.port, local.php_tunnel) = ("127.0.0.1".into(), tunnel.local_port, None);
        (local, tunnel)
    };
    if let Some(p) = my {
        let (cfg, _tunnel) = through(dial(Engine::MySql, &p)).await;
        let mut s =
            Session::connect(&cfg, Some(&Secret::new(&p[3])), None).await.expect("MySQL login through the tunnel");
        assert!(s.server_version().await.unwrap().starts_with("MySQL"));
        let r = s.run_script("SELECT REPEAT('é', 5000) AS big, 1 + 1 AS two", &ScriptOptions::default()).await;
        let row = &rows(&r.results[0].outcome).rows[0];
        assert_eq!((row[0].as_deref().map(|v| v.chars().count()), row[1].as_deref()), (Some(5000), Some("2")));
        s.close().await.unwrap();
    }
    if let Some(p) = redis {
        let (cfg, _tunnel) = through(dial(Engine::Redis, &p)).await;
        let mut r = talus_core::nosql::redis::RedisSession::connect(&cfg, Some(&Secret::new(&p[3])), Some("db8"))
            .await
            .expect("Redis login through the tunnel");
        r.command("SET tunnel:key 'through php'").await.unwrap();
        assert_eq!(r.dbsize().await.unwrap(), 1);
        r.flush_db().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_mistakes_are_reported_in_words() {
    if !php_available() {
        eprintln!("php not installed; skipping");
        return;
    }
    let php = start_php("db.example.test:5432").await;
    if php.external {
        return; // its allow list is the container's
    }
    let open = |cfg: ConnectionConfig, key: &'static str| async move {
        open_for(&cfg, Some(&Secret::new(key)), std::path::Path::new("/nonexistent"), Duration::from_secs(5)).await
    };
    // Wrong key: the signature is refused.
    let cfg = tunnel_cfg(&php, Engine::Postgres, "db.example.test", 5432);
    let err = open(cfg.clone(), "not-the-key-not-the-key-not-the-key-xx").await.err().expect("refused");
    assert!(matches!(&err, DbError::Connect(m) if m.contains("tunnel key")), "{err}");
    // A target that is not on the allow list is refused before anything is dialled.
    let err = open(tunnel_cfg(&php, Engine::Postgres, "10.0.0.5", 5432), KEY).await.err().expect("refused");
    assert!(matches!(&err, DbError::Connect(m) if m.contains("not allowed")), "{err}");
    // An allowed target that cannot be reached: the script says so.
    let err = open(cfg.clone(), KEY).await.err().expect("unreachable");
    assert!(matches!(&err, DbError::Connect(m) if m.contains("cannot reach")), "{err}");
    // No key at all, and a wrong address.
    let err = open_for(&cfg, None, std::path::Path::new("/nonexistent"), TIMEOUT).await.err().unwrap();
    assert!(err.to_string().contains("key is required"), "{err}");
    let mut wrong = cfg.clone();
    wrong.php_tunnel =
        Some(PhpTunnelConfig { url: format!("http://127.0.0.1:{}/missing.php", php.port), save_secret: false });
    let err = open(wrong, KEY).await.err().unwrap();
    assert!(err.to_string().contains("404") || err.to_string().contains("not found"), "{err}");
    // Plain http:// to a remote host is not even attempted.
    let mut remote = cfg.clone();
    remote.user = "u".into();
    remote.php_tunnel = Some(PhpTunnelConfig { url: "http://example.com/t.php".into(), save_secret: false });
    assert!(remote.validate().unwrap_err().contains("https"));
    // Only one tunnel kind per connection.
    let mut both = cfg.clone();
    both.ssh = Some(Default::default());
    assert!(both.validate().is_err());
}

// --- the script's own defences, with hand-made requests ---------------------------------------------------

struct Raw {
    port: u16,
}

impl Raw {
    // Eight wire header fields, in header order; a struct would only add noise at the call sites.
    #[allow(clippy::too_many_arguments)]
    fn sign(op: &str, sid: &str, ts: u64, nonce: &str, seq: u64, target: &str, body: &[u8], key: &str) -> String {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let material = format!(
            "talus2\n{op}\n{TID}\n{sid}\n{ts}\n{nonce}\n{seq}\n{target}\n{}",
            hex(ring::digest::digest(&ring::digest::SHA256, body).as_ref())
        );
        hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes()), material.as_bytes()).as_ref())
    }

    /// Returns `(status, body)`.
    async fn post(&self, headers: &[(&str, String)], body: &[u8], method: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let mut head = format!(
            "{method} /talus-tunnel.php HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
        let text = String::from_utf8_lossy(&out).to_string();
        let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, text.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn headers(
        op: &str,
        sid: &str,
        ts: u64,
        nonce: &str,
        seq: u64,
        target: &str,
        body: &[u8],
        key: &str,
    ) -> Vec<(&'static str, String)> {
        vec![
            ("X-Talus-Op", op.into()),
            ("X-Talus-Tid", TID.into()),
            ("X-Talus-Sid", sid.into()),
            ("X-Talus-Ts", ts.to_string()),
            ("X-Talus-Nonce", nonce.into()),
            ("X-Talus-Seq", seq.to_string()),
            ("X-Talus-Target", target.into()),
            ("X-Talus-Sig", Self::sign(op, sid, ts, nonce, seq, target, body, key)),
        ]
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_script_refuses_forged_replayed_stale_and_malformed_requests() {
    if !php_available() {
        eprintln!("php not installed; skipping");
        return;
    }
    let php = start_php("127.0.0.1:5432").await;
    if php.external {
        return;
    }
    let raw = Raw { port: php.port };
    // Nonces are marked as used for minutes on the host: every run needs its own.
    let nonce = || uuid::Uuid::new_v4().simple().to_string();
    let (sid, n1, n2, n3) = ("a".repeat(32), nonce(), nonce(), nonce());

    // A correct ping works once; the same request again is a replay.
    let h = Raw::headers("ping", &sid, now(), &n1, 0, "", b"", KEY);
    let (st, body) = raw.post(&h, b"", "POST").await;
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("\"tunnel\":\"talus2\"") && !body.contains("allowed_hosts"), "{body}");
    let (st, body) = raw.post(&h, b"", "POST").await;
    assert_eq!((st, body.contains("replayed")), (401, true), "{body}");

    // Wrong key, tampered body, wrong operation: all fail the signature.
    let (st, _) = raw
        .post(&Raw::headers("ping", &sid, now(), &n2, 0, "", b"", "another-key-another-key-another-key-1"), b"", "POST")
        .await;
    assert_eq!(st, 401);
    let signed = Raw::headers("send", &sid, now(), &n3, 1, "", b"hello", KEY);
    let (st, _) = raw.post(&signed, b"hellO", "POST").await;
    assert_eq!(st, 401, "a changed body breaks the signature");
    let mut swapped = Raw::headers("ping", &sid, now(), &nonce(), 0, "", b"", KEY);
    swapped[0].1 = "close".into();
    assert_eq!(raw.post(&swapped, b"", "POST").await.0, 401, "a changed operation breaks the signature");

    // Old timestamps, other methods, junk headers, oversized bodies.
    let stale = Raw::headers("ping", &sid, now() - 3600, &nonce(), 0, "", b"", KEY);
    let (st, body) = raw.post(&stale, b"", "POST").await;
    assert_eq!((st, body.contains("too old")), (401, true), "{body}");
    assert_eq!(raw.post(&Raw::headers("ping", &sid, now(), &nonce(), 0, "", b"", KEY), b"", "GET").await.0, 405);
    assert_eq!(raw.post(&[("X-Talus-Op", "ping".into())], b"", "POST").await.0, 400);
    let junk = Raw::headers("ping", "../../etc/passwd", now(), &nonce(), 0, "", b"", KEY);
    assert_eq!(raw.post(&junk, b"", "POST").await.0, 400, "session ids are hex only");
    let big = vec![0u8; 2 * 1024 * 1024];
    let h = Raw::headers("send", &sid, now(), &nonce(), 1, "", &big, KEY);
    assert_eq!(raw.post(&h, &big, "POST").await.0, 413);

    // Targets: malformed and disallowed ones never reach a socket; nothing is revealed about the host.
    for (i, target) in
        ["10.1.2.3:22", "127.0.0.1:22", "localhost:5432", "127.0.0.1:5432@evil", "bad target", "127.0.0.1:99999"]
            .iter()
            .enumerate()
    {
        let _ = i;
        let h = Raw::headers("dial", &"b".repeat(32), now(), &nonce(), 0, target, b"", KEY);
        let (st, body) = raw.post(&h, b"", "POST").await;
        assert!(matches!(st, 400 | 403), "{target}: {st} {body}");
        assert!(
            !body.contains("10.1.2.3") && !body.contains("/tmp") && !body.contains("var/"),
            "no details leak: {body}"
        );
    }
    // A tunnel that does not exist: send / recv / close answer "gone".
    for (i, op) in ["send", "recv", "close"].iter().enumerate() {
        let _ = i;
        let h = Raw::headers(op, &"c".repeat(32), now(), &nonce(), 1, "", b"", KEY);
        assert_eq!(raw.post(&h, b"", "POST").await.0, 410, "{op}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconfigured_script_refuses_to_run() {
    if !php_available() {
        eprintln!("php not installed; skipping");
        return;
    }
    // No TALUS_TUNNEL_KEY: the placeholder key in the file is not accepted.
    let port = free_port();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tunnel");
    let child = Command::new("php")
        .args(["-S", &format!("127.0.0.1:{port}"), "-t"])
        .arg(&dir)
        .env_remove("TALUS_TUNNEL_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let php = Php { child: Some(child), port, external: false };
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let raw = Raw { port: php.port };
    let h = Raw::headers(
        "ping",
        &"a".repeat(32),
        now(),
        &uuid::Uuid::new_v4().simple().to_string(),
        0,
        "",
        b"",
        "CHANGE-ME",
    );
    let (st, body) = raw.post(&h, b"", "POST").await;
    assert_eq!(st, 503, "{body}");
}
