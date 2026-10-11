//! HTTP tunnel (proxy CONNECT) against a minimal CONNECT proxy started by the test. The echo
//! test always runs; the PostgreSQL round trip runs when TALUS_TEST_PG is set.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use talus_core::engine::{ScriptOptions, Session};
use talus_core::model::{ConnectionConfig, Engine, HttpProxyConfig, Outcome, SslMode};
use talus_core::tunnel::open_http_tunnel;
use talus_core::{DbError, Secret};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Starts a CONNECT proxy that requires `alice:s3cret` and records every requested target.
async fn proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let target = head.split_whitespace().nth(1).unwrap().to_string();
                // base64("alice:s3cret")
                if !head.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0\r\n") {
                    let _ = client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").await;
                    return;
                }
                log.lock().unwrap().push(target.clone());
                let Ok(mut upstream) = TcpStream::connect(target.as_str()).await else {
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (port, seen)
}

fn proxy_cfg(port: u16) -> HttpProxyConfig {
    HttpProxyConfig { host: "127.0.0.1".into(), port, user: "alice".into(), save_secret: false }
}

#[tokio::test(flavor = "multi_thread")]
async fn bytes_flow_both_ways_and_bad_credentials_fail_early() {
    // Echo target.
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = echo.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    let (port, seen) = proxy().await;

    let err = open_http_tunnel(&proxy_cfg(port), Some(&Secret::new("wrong")), "127.0.0.1", echo_port, TIMEOUT)
        .await
        .err()
        .expect("wrong password must fail");
    assert!(matches!(err, DbError::Connect(_)) && err.to_string().contains("407"), "{err}");
    assert!(!err.to_string().contains("wrong"), "the password must not leak into the message");

    let tunnel = open_http_tunnel(&proxy_cfg(port), Some(&Secret::new("s3cret")), "127.0.0.1", echo_port, TIMEOUT)
        .await
        .unwrap();
    let mut s = TcpStream::connect(("127.0.0.1", tunnel.local_port)).await.unwrap();
    s.write_all(b"ping through the proxy").await.unwrap();
    let mut buf = [0u8; 22];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping through the proxy");
    // Probe + the real connection.
    assert_eq!(seen.lock().unwrap().iter().filter(|t| **t == format!("127.0.0.1:{echo_port}")).count(), 2);

    let refused = open_http_tunnel(&proxy_cfg(1), Some(&Secret::new("s3cret")), "127.0.0.1", echo_port, TIMEOUT).await;
    assert!(refused.err().is_some_and(|e| e.to_string().contains("unreachable")));
    drop(tunnel);
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_through_the_http_tunnel() {
    let Ok(v) = std::env::var("TALUS_TEST_PG") else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let p: Vec<&str> = v.splitn(5, ':').collect();
    let (port, _) = proxy().await;
    let tunnel = open_http_tunnel(&proxy_cfg(port), Some(&Secret::new("s3cret")), p[0], p[1].parse().unwrap(), TIMEOUT)
        .await
        .unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Postgres, "via-http");
    cfg.host = "127.0.0.1".into();
    cfg.port = tunnel.local_port;
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = SslMode::Disable;
    let mut s = Session::connect(&cfg, Some(&Secret::new(p[3])), None).await.unwrap();
    let r = s.run_script("SELECT 40 + 2 AS answer", &ScriptOptions::default()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    match &r.results[0].outcome {
        Outcome::Rows(q) => assert_eq!(q.rows[0][0].as_deref(), Some("42")),
        o => panic!("{o:?}"),
    }
    s.close().await.unwrap();
}
