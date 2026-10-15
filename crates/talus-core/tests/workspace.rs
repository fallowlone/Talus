//! Shared workspaces end to end: the real `talus-server` router on a loopback port, driven by the
//! real client. Covers sync between two members, conflicts, roles, revocation and that nothing
//! secret or machine-local reaches the server.

use std::sync::Arc;

use talus_core::Secret;
use talus_core::model::{ConnectionConfig, Engine, SshAuth, SshConfig};
use talus_core::workspace::{Role, WorkspaceClient, WorkspaceError, WorkspaceLink, apply_pull};
use talus_server::{Store, serve};

const PASSWORD: &str = "correct horse battery";

async fn start() -> (String, Arc<Store>) {
    let store = Arc::new(Store::in_memory());
    for u in ["alice", "bob", "carol"] {
        store.set_user(u, PASSWORD).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(serve(listener, store.clone()));
    (url, store)
}

async fn login(url: &str, user: &str) -> WorkspaceClient {
    WorkspaceClient::login(url, user, &Secret::new(PASSWORD)).await.unwrap()
}

fn conn(name: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(Engine::Postgres, name);
    c.host = "db.example.com".into();
    c.port = 5432;
    c.user = "app".into();
    c.init_sql = "SET search_path = secret_schema".into();
    c.ssl_ca_file = "/Users/alice/ca.pem".into();
    c.ssh = Some(SshConfig {
        host: "bastion".into(),
        port: 22,
        user: "alice".into(),
        auth: SshAuth::PrivateKey { path: "/Users/alice/.ssh/id_ed25519".into() },
        save_secret: true,
        trust_new_host_key: true,
    });
    c
}

fn status(e: &WorkspaceError) -> Option<u16> {
    match e {
        WorkspaceError::Server { status, .. } => Some(*status),
        _ => None,
    }
}

#[tokio::test]
async fn an_edit_by_one_member_reaches_another_without_secrets() {
    let (url, _) = start().await;
    let (alice, bob) = (login(&url, "alice").await, login(&url, "bob").await);
    let ws = alice.create_workspace("Team").await.unwrap();
    alice.set_member(&ws.id, "bob", Role::Editor).await.unwrap();

    let mut c = conn("prod");
    let rev = alice.push(&ws.id, &c, 0).await.unwrap();
    assert_eq!(rev, 1);

    // Bob pulls into an empty installation.
    let mut bobs: Vec<ConnectionConfig> = Vec::new();
    let mut link = WorkspaceLink::default();
    let remote = bob.connections(&ws.id).await.unwrap();
    let report = apply_pull(&mut bobs, &mut link, &remote);
    assert_eq!((report.added, report.updated), (1, 0));
    let got = &bobs[0];
    assert_eq!((got.id, got.host.as_str()), (c.id, "db.example.com"));
    assert!(got.init_sql.is_empty() && got.ssl_ca_file.is_empty() && !got.save_password);
    assert_eq!(got.ssh.as_ref().unwrap().auth, SshAuth::PrivateKey { path: String::new() });

    // Bob edits, Alice sees it on her next pull.
    let mut edited = got.clone();
    edited.host = "db2.example.com".into();
    let base = link.revs[&edited.id];
    assert_eq!(bob.push(&ws.id, &edited, base).await.unwrap(), 2);
    let mut alices = vec![c.clone()];
    let mut alink = WorkspaceLink::default();
    alink.revs.insert(c.id, 1);
    apply_pull(&mut alices, &mut alink, &alice.connections(&ws.id).await.unwrap());
    assert_eq!(alices[0].host, "db2.example.com");
    assert_eq!(alices[0].init_sql, "SET search_path = secret_schema", "her local settings survive the pull");
    assert_eq!(alices[0].ssl_ca_file, "/Users/alice/ca.pem");

    // The server never saw her paths or SQL.
    let raw =
        serde_json::to_string(&alice.connections(&ws.id).await.unwrap().iter().map(|r| &r.data).collect::<Vec<_>>())
            .unwrap();
    for leak in ["/Users/alice", "secret_schema", "trust_new_host_key", "save_secret"] {
        assert!(!raw.contains(leak), "{leak} reached the server: {raw}");
    }
    c.host = "x".into();
}

#[tokio::test]
async fn a_concurrent_edit_is_a_conflict_not_an_overwrite() {
    let (url, _) = start().await;
    let (alice, bob) = (login(&url, "alice").await, login(&url, "bob").await);
    let ws = alice.create_workspace("Team").await.unwrap();
    alice.set_member(&ws.id, "bob", Role::Editor).await.unwrap();
    let mut c = conn("prod");
    alice.push(&ws.id, &c, 0).await.unwrap();
    c.host = "alice.example.com".into();
    alice.push(&ws.id, &c, 1).await.unwrap();
    // Bob still thinks the revision is 1.
    c.host = "bob.example.com".into();
    match bob.push(&ws.id, &c, 1).await.unwrap_err() {
        WorkspaceError::Conflict { current } => {
            let current = current.unwrap();
            assert_eq!((current.rev, current.data["host"].as_str()), (2, Some("alice.example.com")));
        }
        e => panic!("expected a conflict, got {e:?}"),
    }
    // Creating over an existing id and deleting with a stale revision conflict as well.
    assert!(matches!(bob.push(&ws.id, &c, 0).await.unwrap_err(), WorkspaceError::Conflict { .. }));
    assert!(matches!(bob.unshare(&ws.id, c.id, 1).await.unwrap_err(), WorkspaceError::Conflict { .. }));
    bob.unshare(&ws.id, c.id, 2).await.unwrap();
    assert!(alice.connections(&ws.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn roles_are_enforced_on_the_server_and_revocation_is_immediate() {
    let (url, _) = start().await;
    let (alice, bob, carol) = (login(&url, "alice").await, login(&url, "bob").await, login(&url, "carol").await);
    let ws = alice.create_workspace("Team").await.unwrap();
    alice.set_member(&ws.id, "bob", Role::Viewer).await.unwrap();
    let c = conn("prod");
    alice.push(&ws.id, &c, 0).await.unwrap();

    // A viewer reads but cannot write, share, or manage members.
    assert_eq!(bob.connections(&ws.id).await.unwrap().len(), 1);
    assert_eq!(status(&bob.push(&ws.id, &conn("other"), 0).await.unwrap_err()), Some(403));
    assert_eq!(status(&bob.unshare(&ws.id, c.id, 1).await.unwrap_err()), Some(403));
    assert_eq!(status(&bob.set_member(&ws.id, "carol", Role::Owner).await.unwrap_err()), Some(403));
    assert_eq!(status(&bob.delete_workspace(&ws.id).await.unwrap_err()), Some(403));
    // A non-member cannot even tell the workspace exists.
    assert_eq!(status(&carol.connections(&ws.id).await.unwrap_err()), Some(404));
    assert_eq!(status(&carol.details(&ws.id).await.unwrap_err()), Some(404));
    assert!(carol.workspaces().await.unwrap().is_empty());

    // Promotion takes effect at once; so does revocation.
    alice.set_member(&ws.id, "bob", Role::Editor).await.unwrap();
    bob.push(&ws.id, &conn("other"), 0).await.unwrap();
    alice.remove_member(&ws.id, "bob").await.unwrap();
    assert_eq!(status(&bob.connections(&ws.id).await.unwrap_err()), Some(404));
    assert_eq!(status(&bob.push(&ws.id, &conn("late"), 0).await.unwrap_err()), Some(404));

    // The last owner can neither leave nor be demoted.
    assert_eq!(status(&alice.remove_member(&ws.id, "alice").await.unwrap_err()), Some(400));
    assert_eq!(status(&alice.set_member(&ws.id, "alice", Role::Editor).await.unwrap_err()), Some(400));
    // Unknown accounts cannot be added.
    assert_eq!(status(&alice.set_member(&ws.id, "mallory", Role::Viewer).await.unwrap_err()), Some(404));
}

#[tokio::test]
async fn login_rejects_bad_passwords_and_locks_out_guessing() {
    let (url, _) = start().await;
    let bad = Secret::new("not the password");
    let err = WorkspaceClient::login(&url, "alice", &bad).await.err().unwrap();
    assert_eq!(status(&err), Some(401));
    // An unknown user looks exactly like a wrong password.
    let err = WorkspaceClient::login(&url, "nobody", &bad).await.err().unwrap();
    assert!(
        matches!(&err, WorkspaceError::Server { status: 401, message } if message.contains("invalid user name or password"))
    );
    for _ in 0..4 {
        let _ = WorkspaceClient::login(&url, "alice", &bad).await;
    }
    // Five failures: even the right password waits.
    let err = WorkspaceClient::login(&url, "alice", &Secret::new(PASSWORD)).await.err().unwrap();
    assert_eq!(status(&err), Some(429));

    // No token, bad token, and a logged-out token are all refused.
    let anon = WorkspaceClient::new(&url, None).unwrap();
    assert_eq!(status(&anon.workspaces().await.unwrap_err()), Some(401));
    let forged = WorkspaceClient::new(&url, Some(Secret::new("ff".repeat(32)))).unwrap();
    assert_eq!(status(&forged.workspaces().await.unwrap_err()), Some(401));
    let bob = login(&url, "bob").await;
    bob.logout().await.unwrap();
    assert_eq!(status(&bob.workspaces().await.unwrap_err()), Some(401));
}

#[tokio::test]
async fn the_server_refuses_secrets_even_from_a_misbehaving_client() {
    let (url, _) = start().await;
    let alice = login(&url, "alice").await;
    let ws = alice.create_workspace("Team").await.unwrap();
    let http = reqwest::Client::new();
    let token = alice.token().unwrap().expose().to_string();
    let put = |body: serde_json::Value| {
        let id = uuid::Uuid::new_v4();
        http.put(format!("{url}/v1/workspaces/{}/connections/{id}", ws.id)).bearer_auth(&token).json(&body).send()
    };
    for data in [
        serde_json::json!({ "password": "hunter2" }),
        serde_json::json!({ "ssh": { "passphrase": "x" } }),
        serde_json::json!({ "ssl_ca_file": "/etc/ssl/ca.pem" }),
        serde_json::json!({ "init_sql": "DROP TABLE t" }),
        serde_json::json!({ "a": [{ "api_token": "t" }] }),
        serde_json::json!("not an object"),
    ] {
        let r = put(serde_json::json!({ "base_rev": 0, "data": data })).await.unwrap();
        assert_eq!(r.status(), 400, "{data}");
    }
    let r = put(serde_json::json!({ "base_rev": 0, "data": { "host": "x".repeat(20_000) } })).await.unwrap();
    assert!(r.status().is_client_error());
}
