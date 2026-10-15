//! Shared workspace window against a real in-process `talus-server`: sign in, create a workspace,
//! share a connection, and pull what another member shared.

mod common;

use std::sync::Arc;

use common::*;
use egui_kittest::kittest::Queryable;
use talus_core::Secret;
use talus_core::model::{ConnectionConfig, Engine};
use talus_core::store::config::{ConnectionsFile, load_json, save_json};
use talus_core::workspace::{Role, WorkspaceClient};
use talus_server::{Store, serve};

const PASSWORD: &str = "correct horse battery";

fn type_into(h: &mut egui_kittest::Harness<'static, talus_app::App>, label: &str, text: &str) {
    h.get_by_label(label).focus();
    h.step();
    h.get_by_label(label).type_text(text);
    h.step();
}

#[test]
fn share_and_pull_a_connection_through_the_dialog() {
    let fx = fixture();
    let store = Arc::new(Store::in_memory());
    store.set_user("alice", PASSWORD).unwrap();
    store.set_user("bob", PASSWORD).unwrap();
    let url = fx.rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, store));
        url
    });

    // One shareable connection already exists on this "Mac".
    let mut local = ConnectionConfig::new(Engine::Postgres, "analytics");
    local.host = "db.example.com".into();
    local.port = 5432;
    local.user = "app".into();
    local.init_sql = "SET search_path = mine".into();
    let connections_file = fx.dir.join("support").join("connections.json");
    std::fs::create_dir_all(connections_file.parent().unwrap()).unwrap();
    save_json(&connections_file, &ConnectionsFile { connections: vec![local.clone()] }).unwrap();

    let mut h = harness(&fx);
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Shared Workspace…").click();
    wait_for(&mut h, "sign-in form", |h| has_label_containing(h, "Passwords, tunnel"));

    type_into(&mut h, "Server", &url);
    type_into(&mut h, "User", "alice");
    type_into(&mut h, "Password", PASSWORD);
    click_enabled(&mut h, "Sign In");
    wait_for(&mut h, "signed in", |h| has_label_containing(h, "alice on http://"));

    type_into(&mut h, "New workspace", "Team");
    click_enabled(&mut h, "Create");
    wait_for(&mut h, "workspace created", |h| has_label_containing(h, "Created “Team”"));
    wait_for(&mut h, "own connections listed", |h| has_label_containing(h, "analytics  ·  local"));
    click_enabled(&mut h, "Share");
    wait_for(&mut h, "shared", |h| has_label_containing(h, "analytics  ·  shared"));

    // The server holds the definition, without the member's local SQL.
    let alice = fx.rt.block_on(WorkspaceClient::login(&url, "alice", &Secret::new(PASSWORD))).unwrap();
    let ws = fx.rt.block_on(alice.workspaces()).unwrap().remove(0);
    let remote = fx.rt.block_on(alice.connections(&ws.id)).unwrap();
    assert_eq!(remote.len(), 1);
    assert!(!remote[0].data.to_string().contains("mine"));

    // Another member shares a second connection; Pull brings it in and keeps the file consistent.
    fx.rt.block_on(alice.set_member(&ws.id, "bob", Role::Editor)).unwrap();
    let bob = fx.rt.block_on(WorkspaceClient::login(&url, "bob", &Secret::new(PASSWORD))).unwrap();
    let mut theirs = ConnectionConfig::new(Engine::MySql, "from-bob");
    theirs.host = "mysql.example.com".into();
    fx.rt.block_on(bob.push(&ws.id, &theirs, 0)).unwrap();
    click_enabled(&mut h, "Pull");
    wait_for(&mut h, "pulled", |h| has_label_containing(h, "Pulled: 1 added, 1 updated"));
    wait_for(&mut h, "saved", |_| {
        load_json::<ConnectionsFile>(&connections_file)
            .is_ok_and(|f| f.connections.iter().any(|c| c.name == "from-bob"))
    });
    let saved = load_json::<ConnectionsFile>(&connections_file).unwrap().connections;
    let mine = saved.iter().find(|c| c.name == "analytics").unwrap();
    assert_eq!(mine.init_sql, "SET search_path = mine", "pulling never overwrites local-only settings");
    let imported = saved.iter().find(|c| c.name == "from-bob").unwrap();
    assert!(!imported.save_password && imported.init_sql.is_empty());
}
