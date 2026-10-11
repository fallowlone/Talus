//! Shared helpers for headless UI tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use talus_app::{App, AppEnv};
use talus_core::engine::{ScriptOptions, Session, create_database_file};
use talus_core::model::{ConnectionConfig, Engine, Outcome};
use talus_core::store::config::AppPaths;
use talus_core::store::secrets::MemoryStore;

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub dir: PathBuf,
    pub db: PathBuf,
}

pub fn fixture() -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let dir = std::env::temp_dir().join(format!("talus-ui-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shop.sqlite");
    create_database_file(&db).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "seed");
    cfg.database = db.to_string_lossy().into_owned();
    rt.block_on(async {
        let mut s = Session::connect(&cfg, None, None).await.unwrap();
        let r = s
            .run_script(
                "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT);
                 CREATE TABLE notes (body TEXT);
                 CREATE VIEW v_customers AS SELECT name FROM customers;
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 250)
                 INSERT INTO customers (name, email) SELECT 'Customer ' || i, 'c' || i || '@example.test' FROM n;",
                &ScriptOptions::default(),
            )
            .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        s.close().await.unwrap();
    });
    Fixture { rt, dir, db }
}

pub fn harness(fx: &Fixture) -> Harness<'static, App> {
    let paths = AppPaths::at(fx.dir.join("support")).unwrap();
    let handle = fx.rt.handle().clone();
    // Small step so two clicks one frame apart fall inside egui's double-click window.
    Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_step_dt(0.01).build_eframe(move |cc| {
        App::new(&cc.egui_ctx, AppEnv { paths, secrets: Arc::new(MemoryStore::default()), rt: handle })
    })
}

/// Steps frames until `cond` holds (async work completes on the runtime threads).
pub fn wait_for(h: &mut Harness<'_, App>, what: &str, cond: impl Fn(&Harness<'_, App>) -> bool) {
    for _ in 0..600 {
        h.step();
        if cond(h) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let states: Vec<String> = h
        .state()
        .states
        .values()
        .map(|s| {
            format!(
                "{:?} dbs={:?}",
                s.status,
                s.databases
                    .iter()
                    .map(|d| (&d.name, d.expanded, format!("{:?}", d.objects).chars().take(60).collect::<String>()))
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    panic!("timed out waiting for: {what}; connection states: {states:?}");
}

/// Two clicks one frame apart (inside egui's double-click window).
pub fn double_click(
    h: &mut Harness<'static, App>,
    find: impl for<'a> Fn(&'a Harness<'static, App>) -> egui_kittest::Node<'a>,
) {
    // Let earlier clicks age out so this is not counted as a triple click.
    for _ in 0..100 {
        h.step();
    }
    find(h).click();
    h.step();
    find(h).click();
    h.step();
}

/// Saves a rendered PNG when `TALUS_SCREENSHOT_DIR` is set (visual review aid).
pub fn shot(h: &mut Harness<'_, App>, name: &str) {
    let Ok(dir) = std::env::var("TALUS_SCREENSHOT_DIR") else { return };
    h.step();
    let img = h.render().expect("render");
    img.save(std::path::Path::new(&dir).join(format!("{name}.png"))).expect("save png");
}

pub fn has_label_containing(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

/// Button whose label contains `text` (navigator items, toolbar buttons).
pub fn item<'h>(h: &'h Harness<'_, App>, text: &'h str) -> egui_kittest::Node<'h> {
    h.query_by(move |n| n.role() == Role::Button && n.label().is_some_and(|l| l.contains(text)))
        .unwrap_or_else(|| panic!("no button whose label contains {text:?}"))
}

pub fn type_into(h: &mut Harness<'_, App>, role: Role, label: &str, text: &str) {
    h.get_by_role_and_label(role, label).focus();
    h.step();
    h.get_by_role_and_label(role, label).type_text(text);
    h.step();
}

pub fn query_db(fx: &Fixture, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "check");
    cfg.database = fx.db.to_string_lossy().into_owned();
    fx.rt.block_on(async {
        let mut s = Session::connect(&cfg, None, None).await.unwrap();
        let r = s.run_script(sql, &ScriptOptions::default()).await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let rows = match &r.results[0].outcome {
            Outcome::Rows(q) => q.rows.clone(),
            other => panic!("{other:?}"),
        };
        s.close().await.unwrap();
        rows
    })
}

pub fn create_connection_via_dialog(h: &mut Harness<'_, App>, fx: &Fixture) {
    shot(h, "a0-empty");
    h.get_by_label_contains("New Connection").click();
    h.step();
    shot(h, "a1-engine-picker");
    click_enabled_containing(h, "SQLite");
    h.step();
    type_into(h, Role::TextInput, "Connection Name", "Shop DB");
    type_into(h, Role::TextInput, "Database File", &fx.db.to_string_lossy());
    h.get_by_label("Test Connection").click();
    wait_for(h, "test connection success", |h| has_label_containing(h, "Connection successful"));
    shot(h, "a2-connection-dialog");
    h.get_by_label("Save").click();
    wait_for(h, "dialog closed and connection listed", |h| {
        h.query_by_label("Test Connection").is_none() && has_label_containing(h, "Shop DB")
    });
    assert_eq!(h.state().connections.len(), 1);
}

/// Button whose label ends with `suffix` (disambiguates "Run" from "Run Current").
pub fn button_ending<'h>(h: &'h Harness<'_, App>, suffix: &'h str) -> egui_kittest::Node<'h> {
    h.query_by(move |n| n.role() == Role::Button && n.label().is_some_and(|l| l.ends_with(suffix)))
        .unwrap_or_else(|| panic!("no button whose label ends with {suffix:?}"))
}

/// Clicks the enabled button with exactly this label, waiting until it becomes enabled.
pub fn click_enabled(h: &mut Harness<'_, App>, label: &'static str) {
    let find = move |n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::Button && n.label().as_deref() == Some(label) && !n.is_disabled()
    };
    wait_for(h, label, |h| h.query_by(find).is_some());
    h.get_by(find).click();
}

pub fn item_exists(h: &Harness<'_, App>, text: &str) -> bool {
    h.query_all_by(|n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.label().is_some_and(|l| l.contains(text)) && n.role() != Role::Label
    })
    .next()
    .is_some()
}

/// Waits until an enabled button whose label contains `text` exists (new windows spend their
/// first frame in a disabled sizing pass), then clicks it.
pub fn click_enabled_containing(h: &mut Harness<'_, App>, text: &'static str) {
    let find = move |n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::Button && n.label().is_some_and(|l| l.contains(text)) && !n.is_disabled()
    };
    wait_for(h, text, |h| h.query_by(find).is_some());
    h.get_by(find).click();
}

/// Sidebar navigation item with exactly this label ("New Query", "History", …).
pub fn sidebar_item<'h>(h: &'h Harness<'_, App>, label: &'h str) -> egui_kittest::Node<'h> {
    h.query_by(move |n| n.role() == Role::Button && n.label().as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no sidebar item {label:?}"))
}

/// Minimal unauthenticated HTTP CONNECT proxy on 127.0.0.1 (for HTTP-tunnel tests); returns its port.
pub fn connect_proxy(fx: &Fixture) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fx.rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if client.read(&mut b).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(b[0]);
                    }
                    let target = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("").to_string();
                    let Ok(mut upstream) = tokio::net::TcpStream::connect(target.as_str()).await else { return };
                    if client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_ok() {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                });
            }
        });
        port
    })
}
