//! SSH tunnel against the disposable `sshd` container (see docker-compose.test.yml).
//! Runs when TALUS_TEST_SSH=host:port:user:password and TALUS_TEST_PG are set.

use std::time::Duration;

use talus_core::engine::{ScriptOptions, Session};
use talus_core::model::{ConnectionConfig, Engine, Outcome, SshAuth, SshConfig, SslMode};
use talus_core::ssh::open_tunnel;
use talus_core::{DbError, Secret};

fn ssh_env() -> Option<(SshConfig, Secret)> {
    let v = std::env::var("TALUS_TEST_SSH").ok()?;
    let p: Vec<&str> = v.splitn(4, ':').collect();
    let cfg = SshConfig {
        host: p[0].into(),
        port: p[1].parse().unwrap(),
        user: p[2].into(),
        auth: SshAuth::Password,
        save_secret: false,
        trust_new_host_key: false,
    };
    Some((cfg, Secret::new(p[3])))
}

fn temp_known_hosts() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("talus-kh-{}", uuid::Uuid::new_v4().simple())).join("known_hosts")
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_to_postgres_with_host_key_checks() {
    let Some((mut ssh, pw)) = ssh_env() else {
        eprintln!("TALUS_TEST_SSH not set; skipping");
        return;
    };
    let kh = temp_known_hosts();
    let t = Duration::from_secs(10);

    // Unknown host key is refused unless trust-on-first-use is enabled.
    let err = open_tunnel(&ssh, Some(&pw), "postgres", 5432, &kh, t).await.err().expect("must refuse unknown key");
    assert!(err.to_string().contains("unknown SSH host key") && err.to_string().contains("SHA256:"), "{err}");

    // Wrong password.
    ssh.trust_new_host_key = true;
    let err =
        open_tunnel(&ssh, Some(&Secret::new("nope")), "postgres", 5432, &kh, t).await.err().expect("bad password");
    assert!(matches!(err, DbError::Connect(_)), "{err:?}");
    assert!(!err.to_string().contains("nope"));

    // TOFU recorded the key; a second connection works without TOFU.
    ssh.trust_new_host_key = false;
    let tunnel = open_tunnel(&ssh, Some(&pw), "postgres", 5432, &kh, t).await.expect("tunnel");
    assert!(std::fs::read_to_string(&kh).unwrap().contains("[127.0.0.1]:52222"));

    let mut cfg = ConnectionConfig::new(Engine::Postgres, "via-ssh");
    cfg.host = "127.0.0.1".into();
    cfg.port = tunnel.local_port;
    cfg.user = "talus".into();
    cfg.database = "talus_test".into();
    cfg.ssl_mode = SslMode::Disable;
    // Two sessions share one tunnel.
    let mut a = Session::connect(&cfg, Some(&Secret::new("talus_test_pw")), None).await.expect("pg over ssh");
    let mut b = Session::connect(&cfg, Some(&Secret::new("talus_test_pw")), None).await.expect("second session");
    for s in [&mut a, &mut b] {
        let r = s.run_script("SELECT inet_server_addr() IS NOT NULL", &ScriptOptions::default()).await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(matches!(&r.results[0].outcome, Outcome::Rows(q) if q.rows[0][0].as_deref() == Some("t")));
    }
    drop(tunnel);
    // After the tunnel is gone, new connections fail fast.
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        Session::connect(&cfg, Some(&Secret::new("talus_test_pw")), None),
    )
    .await
    .unwrap();
    assert!(r.is_err());
}
