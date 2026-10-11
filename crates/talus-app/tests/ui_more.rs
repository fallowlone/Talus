//! More headless UI tests: cancellation, session restore, destructive confirmations,
//! connection deletion, and the streaming export job through the real worker.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use talus_app::app::TabKind;
use talus_app::worker::{ConnInfo, Purpose, Request, Response, Worker};
use talus_core::export::ExportFormat;
use talus_core::model::{ConnectionConfig, Engine, ObjectKind, ObjectRef};

fn connect(h: &mut egui_kittest::Harness<'static, talus_app::App>, fx: &Fixture) {
    create_connection_via_dialog(h, fx);
    double_click(h, |h| item(h, "Shop DB"));
    wait_for(h, "tables listed", |h| has_label_containing(h, "customers"));
}

fn new_query(h: &mut egui_kittest::Harness<'static, talus_app::App>, sql: &str) {
    sidebar_item(h, "New Query").click();
    wait_for(h, "query editor", |h| h.query_by_role(Role::MultilineTextInput).is_some());
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text(sql);
    h.step();
}

#[test]
fn stop_button_cancels_running_query() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    new_query(&mut h, "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c;");
    button_ending(&h, " Run").click();
    wait_for(&mut h, "query running", |h| has_label_containing(h, "Running…"));
    let started = Instant::now();
    button_ending(&h, " Stop").click();
    wait_for(&mut h, "cancelled message", |h| has_label_containing(h, "Execution was cancelled"));
    assert!(started.elapsed() < Duration::from_secs(5));
    // The tab is usable again afterwards.
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text("SELECT 'after-cancel' AS v;");
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
    wait_for(&mut h, "query after cancel", |h| has_label_containing(h, "after-cancel"));
}

#[test]
fn session_restore_reopens_query_tabs() {
    let fx = fixture();
    {
        let mut h = harness(&fx);
        h.step();
        create_connection_via_dialog(&mut h, &fx);
        new_query(&mut h, "SELECT 'restored text';");
        h.state().save_session();
    }
    let mut h = harness(&fx);
    h.step();
    let restored: Vec<String> = h
        .state()
        .tabs
        .iter()
        .filter_map(|t| match &t.kind {
            TabKind::Sql(s) => Some(s.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(restored, vec!["SELECT 'restored text';".to_string()]);
    let conn_bound = h.state().tabs.iter().any(|t| matches!(&t.kind, TabKind::Sql(s) if s.conn.is_some()));
    assert!(conn_bound, "restored tab keeps its connection");
}

#[test]
fn drop_table_requires_confirmation_and_cancel_does_nothing() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);

    item(&h, "  notes").click_secondary();
    h.step();
    item(&h, "Drop Table…").click();
    wait_for(&mut h, "confirmation", |h| has_label_containing(h, "DROP TABLE \"main\".\"notes\""));
    click_enabled(&mut h, "Cancel");
    wait_for(&mut h, "modal closed", |h| !has_label_containing(h, "DROP TABLE"));
    assert_eq!(query_db(&fx, "SELECT count(*) FROM sqlite_master WHERE name = 'notes'")[0][0].as_deref(), Some("1"));

    item(&h, "  notes").click_secondary();
    h.step();
    item(&h, "Drop Table…").click();
    wait_for(&mut h, "confirmation", |h| has_label_containing(h, "DROP TABLE"));
    click_enabled(&mut h, "Drop");
    wait_for(&mut h, "table gone from tree", |h| {
        h.query_all_by_label_contains("  notes").next().is_none() && has_label_containing(h, "customers")
    });
    assert_eq!(query_db(&fx, "SELECT count(*) FROM sqlite_master WHERE name = 'notes'")[0][0].as_deref(), Some("0"));
}

#[test]
fn delete_connection_after_confirmation() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    create_connection_via_dialog(&mut h, &fx);
    item(&h, "Shop DB").click_secondary();
    h.step();
    item(&h, "Delete Connection…").click();
    wait_for(&mut h, "confirmation", |h| has_label_containing(h, "Delete the connection"));
    click_enabled(&mut h, "Delete");
    wait_for(&mut h, "connection removed", |h| has_label_containing(h, "No connections yet"));
    let saved = std::fs::read_to_string(fx.dir.join("support").join("connections.json")).unwrap();
    assert!(!saved.contains("Shop DB"));
}

#[test]
fn export_job_streams_whole_table_to_csv() {
    let fx = fixture();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = Worker::new(
        fx.rt.handle().clone(),
        fx.dir.join("known_hosts"),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "export");
    cfg.database = fx.db.to_string_lossy().into_owned();
    let info = ConnInfo::plain(cfg, None);
    let out = fx.dir.join("customers.csv");
    let obj = ObjectRef { database: "main".into(), schema: None, name: "customers".into(), kind: ObjectKind::Table };
    let job = worker.submit(
        &info,
        Purpose::Meta,
        None,
        Request::Export { obj, format: ExportFormat::Csv, path: out.clone() },
    );
    rx.recv_timeout(Duration::from_secs(20)).expect("export finished");
    let ev = worker.poll().into_iter().find(|e| e.id == job).expect("event");
    match ev.result {
        Ok(Response::Exported { rows, .. }) => assert_eq!(rows, 250),
        other => panic!("{other:?}"),
    }
    let text = std::fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "id,name,email");
    assert_eq!(lines.len(), 251);
    assert_eq!(lines[1], "1,Customer 1,c1@example.test");
    assert_eq!(lines[250], "250,Customer 250,c250@example.test");
}

fn editor_text(h: &egui_kittest::Harness<'static, talus_app::App>) -> String {
    let s = h.state();
    match &s.tabs[s.active].kind {
        TabKind::Sql(t) => t.text.clone(),
        _ => panic!("active tab is not a query tab"),
    }
}

#[test]
fn completion_format_and_comment_toggle() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    new_query(&mut h, "SELECT * FROM cu");
    wait_for(&mut h, "table completion", |h| {
        item_exists(h, "  customers") && h.query_all_by_label_contains("table").next().is_some()
    });
    h.key_press(egui::Key::Enter);
    h.step();
    assert_eq!(editor_text(&h), "SELECT * FROM customers");

    // Alias-qualified columns load lazily and are offered after `c.`.
    h.get_by_role(Role::MultilineTextInput).type_text(" c WHERE c.");
    wait_for(&mut h, "column completion", |h| has_label_containing(h, "customers · TEXT"));
    h.key_press(egui::Key::ArrowDown);
    h.step();
    h.key_press(egui::Key::Enter);
    h.step();
    // Columns are listed alphabetically: email, id, name → one step down is `id`.
    assert_eq!(editor_text(&h), "SELECT * FROM customers c WHERE c.id");

    // ⌘/ comments the current line and back.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Slash);
    h.step();
    assert_eq!(editor_text(&h), "-- SELECT * FROM customers c WHERE c.id");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Slash);
    h.step();
    assert_eq!(editor_text(&h), "SELECT * FROM customers c WHERE c.id");

    // Format button.
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text("select id,name from customers where id=1");
    h.step();
    // Close any completion popup before clicking the toolbar.
    h.key_press(egui::Key::Escape);
    h.step();
    button_ending(&h, " Format").click();
    h.step();
    assert_eq!(editor_text(&h), "SELECT id, name\nFROM customers\nWHERE id = 1\n");
}

fn input_with_value<'h>(
    h: &'h egui_kittest::Harness<'_, talus_app::App>,
    value: &'h str,
) -> Option<egui_kittest::Node<'h>> {
    h.query_by(move |n| n.role() == Role::TextInput && n.value().as_deref() == Some(value))
}

fn replace_text(h: &mut egui_kittest::Harness<'static, talus_app::App>, current_value: &str, new_text: &str) {
    input_with_value(h, current_value).expect("input").focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    input_with_value(h, current_value).expect("input").type_text(new_text);
    h.step();
}

#[test]
fn table_designer_create_rename_and_drop_field() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    // Select the database so the Objects tab offers "New Table".
    item(&h, "  main").click();
    wait_for(&mut h, "objects toolbar", |h| item_exists(h, "New Table"));
    item(&h, "New Table").click();
    wait_for(&mut h, "designer", |h| has_label_containing(h, "SQL Preview"));
    type_into(&mut h, Role::TextInput, "Table name", "gadgets");
    item(&h, "Add Field").click();
    h.step();
    replace_text(&mut h, "field2", "title");
    wait_for(&mut h, "preview", |h| has_label_containing(h, "CREATE TABLE \"main\".\"gadgets\""));
    button_ending(&h, " Save").click();
    wait_for(&mut h, "created", |h| has_label_containing(h, "Saved (1 statement)"));
    assert_eq!(query_db(&fx, "SELECT count(*) FROM pragma_table_info('gadgets')")[0][0].as_deref(), Some("2"));
    wait_for(&mut h, "tree shows new table", |h| item_exists(h, "  gadgets"));

    // Now in alter mode: rename a field (non-destructive, no confirmation).
    wait_for(&mut h, "designer reloaded", |h| input_with_value(h, "title").is_some());
    replace_text(&mut h, "title", "name");
    wait_for(&mut h, "rename preview", |h| has_label_containing(h, "RENAME COLUMN \"title\" TO \"name\""));
    button_ending(&h, " Save").click();
    wait_for(&mut h, "renamed", |_| {
        query_db(&fx, "SELECT count(*) FROM pragma_table_info('gadgets') WHERE name = 'name'")[0][0].as_deref()
            == Some("1")
    });

    // Deleting a field is destructive and asks first.
    wait_for(&mut h, "designer reloaded again", |h| {
        input_with_value(h, "name").is_some() && !has_label_containing(h, "RENAME COLUMN")
    });
    input_with_value(&h, "name").expect("name input").focus();
    h.step();
    item(&h, "Delete Field").click();
    wait_for(&mut h, "drop preview", |h| has_label_containing(h, "DROP COLUMN \"name\""));
    button_ending(&h, " Save").click();
    click_enabled(&mut h, "Apply Changes");
    wait_for(&mut h, "dropped", |_| {
        query_db(&fx, "SELECT count(*) FROM pragma_table_info('gadgets')")[0][0].as_deref() == Some("1")
    });
    shot(&mut h, "15-table-designer");
}

#[test]
fn csv_import_wizard_loads_rows() {
    let fx = fixture();
    let csv = fx.dir.join("more.csv");
    let mut text = String::from("email;name;id\n");
    for i in 1001..=1010 {
        text.push_str(&format!("u{i}@example.test;User {i};{i}\n"));
    }
    std::fs::write(&csv, text).unwrap();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    item(&h, "  customers").click_secondary();
    h.step();
    item(&h, "Import CSV / JSON / XML…").click();
    wait_for(&mut h, "import dialog", |h| has_label_containing(h, "Import CSV into customers"));
    item(&h, "Semicolon").click();
    type_into(&mut h, Role::TextInput, "File", &csv.to_string_lossy());
    wait_for(&mut h, "preview + mapping", |h| {
        has_label_containing(h, "u1001@example.test") && has_label_containing(h, "Column mapping")
    });
    click_enabled(&mut h, "Import");
    wait_for(&mut h, "imported", |h| has_label_containing(h, "Imported 10 rows into customers"));
    assert_eq!(
        query_db(&fx, "SELECT count(*), max(email) FROM customers WHERE id > 1000")[0],
        vec![Some("10".into()), Some("u1010@example.test".into())]
    );
}

#[test]
fn explain_inspector_and_copy_as_insert() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    new_query(&mut h, "SELECT * FROM customers WHERE id = 7");
    h.key_press(egui::Key::Escape);
    h.step();
    button_ending(&h, " Explain").click();
    wait_for(&mut h, "query plan", |h| {
        has_label_containing(h, "SEARCH customers") || has_label_containing(h, "SCAN customers")
    });
    let hist = std::fs::read_to_string(fx.dir.join("support").join("history.jsonl")).unwrap();
    assert!(hist.contains("EXPLAIN QUERY PLAN SELECT * FROM customers WHERE id = 7"));

    double_click(&mut h, |h| item(h, "  customers"));
    wait_for(&mut h, "page loaded", |h| has_label_containing(h, "c1@example.test"));
    h.get_by_label("c3@example.test").click();
    h.step();
    item(&h, "Inspector").click();
    wait_for(&mut h, "inspector shows value", |h| has_label_containing(h, "15 characters"));
    h.get_by_label("c3@example.test").click_secondary();
    h.step();
    item(&h, "Copy Row as INSERT").click();
    wait_for(&mut h, "copied", |h| has_label_containing(h, "INSERT statement copied"));
}

#[test]
fn find_in_database_data_and_structure() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    item(&h, "  main").click();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::F);
    wait_for(&mut h, "find tab", |h| has_label_containing(h, "Look in"));
    type_into(&mut h, Role::TextInput, "Find", "Customer 25");
    click_enabled(&mut h, "\u{e30c} Find");
    wait_for(&mut h, "data hits", |h| has_label_containing(h, "2 match(es)"));
    assert!(has_label_containing(&h, "  v_customers"));
    h.get_by_label("Structure (table/column names)").click();
    h.step();
    click_enabled(&mut h, "\u{e30c} Find");
    wait_for(&mut h, "no structure hits", |h| has_label_containing(h, "No matches."));
    shot(&mut h, "12-find");
}

#[test]
fn ssh_tunnel_test_connection_from_dialog() {
    let (Ok(ssh), Ok(_pg)) = (std::env::var("TALUS_TEST_SSH"), std::env::var("TALUS_TEST_PG")) else {
        eprintln!("TALUS_TEST_SSH / TALUS_TEST_PG not set; skipping");
        return;
    };
    let p: Vec<&str> = ssh.splitn(4, ':').collect();
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    h.get_by_label_contains("New Connection").click();
    click_enabled_containing(&mut h, "PostgreSQL");
    h.step();
    type_into(&mut h, Role::TextInput, "Connection Name", "PG via SSH");
    // Host/port as seen from the SSH server (docker network).
    replace_text(&mut h, "localhost", "postgres");
    type_into(&mut h, Role::TextInput, "User Name", "talus");
    type_into(&mut h, Role::PasswordInput, "Password", "talus_test_pw");
    type_into(&mut h, Role::TextInput, "Initial Database", "talus_test");
    h.get_by_label("disable").click();
    h.step();
    h.get_by_label("SSH Tunnel").click();
    h.step();
    h.get_by_label("Use SSH tunnel").click();
    h.step();
    type_into(&mut h, Role::TextInput, "SSH Host", p[0]);
    replace_text(&mut h, "22", p[1]);
    type_into(&mut h, Role::TextInput, "SSH User Name", p[2]);
    type_into(&mut h, Role::PasswordInput, "SSH Password", p[3]);
    h.get_by_label("Trust host key on first connect").click();
    h.step();
    h.get_by_label("Test Connection").click();
    let warn = egui_phosphor::regular::WARNING_CIRCLE;
    wait_for(&mut h, "tunnel test result", |h| has_label_containing(h, "(via SSH)") || has_label_containing(h, warn));
    let errors: Vec<String> = h.query_all_by_label_contains(warn).map(|n| format!("{n:?}")).collect();
    assert!(has_label_containing(&h, "(via SSH)"), "expected success through the tunnel, got {errors:?}");
    assert!(std::fs::read_to_string(fx.dir.join("support").join("known_hosts")).unwrap().contains("52222"));
}

#[test]
fn http_tunnel_test_connection_from_dialog() {
    let Ok(pg) = std::env::var("TALUS_TEST_PG") else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let p: Vec<&str> = pg.splitn(5, ':').collect();
    let fx = fixture();
    let proxy_port = connect_proxy(&fx);
    let mut h = harness(&fx);
    h.step();
    h.get_by_label_contains("New Connection").click();
    click_enabled_containing(&mut h, "PostgreSQL");
    h.step();
    type_into(&mut h, Role::TextInput, "Connection Name", "PG via HTTP");
    // Host/port as resolved by the proxy.
    replace_text(&mut h, "localhost", p[0]);
    replace_text(&mut h, "5432", p[1]);
    type_into(&mut h, Role::TextInput, "User Name", p[2]);
    type_into(&mut h, Role::PasswordInput, "Password", p[3]);
    type_into(&mut h, Role::TextInput, "Initial Database", p[4]);
    h.get_by_label("disable").click();
    h.step();
    h.get_by_label("HTTP Tunnel").click();
    h.step();
    h.get_by_label("Use HTTP tunnel").click();
    h.step();
    type_into(&mut h, Role::TextInput, "Proxy Host", "127.0.0.1");
    replace_text(&mut h, "3128", &proxy_port.to_string());
    h.get_by_label("Test Connection").click();
    let warn = egui_phosphor::regular::WARNING_CIRCLE;
    wait_for(&mut h, "tunnel test result", |h| {
        has_label_containing(h, "(via HTTP tunnel)") || has_label_containing(h, warn)
    });
    let errors: Vec<String> = h.query_all_by_label_contains(warn).map(|n| format!("{n:?}")).collect();
    assert!(has_label_containing(&h, "(via HTTP tunnel)"), "expected success through the proxy, got {errors:?}");
}

/// PostgreSQL through the UI: password prompt, create/drop database, server monitor.
#[test]
fn postgres_prompt_database_admin_and_monitor() {
    let Ok(pg) = std::env::var("TALUS_TEST_PG") else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let p: Vec<&str> = pg.splitn(5, ':').collect();
    let fx = fixture();
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Postgres, "Docker PG");
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = talus_core::model::SslMode::Disable;
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [cfg] }).to_string()).unwrap();

    let mut h = harness(&fx);
    h.step();
    double_click(&mut h, |h| item(h, "Docker PG"));
    wait_for(&mut h, "password prompt", |h| has_label_containing(h, "Enter the database password"));
    type_into(&mut h, Role::PasswordInput, "Password", p[3]);
    click_enabled(&mut h, "Connect");
    wait_for(&mut h, "connected, public schema", |h| item_exists(h, "  public"));

    let db = format!("ui_{}", uuid::Uuid::new_v4().simple().to_string().get(..8).unwrap());
    item(&h, "Docker PG").click_secondary();
    h.step();
    item(&h, "New Database…").click();
    wait_for(&mut h, "name dialog", |h| h.query_by_role_and_label(Role::TextInput, "Database Name").is_some());
    type_into(&mut h, Role::TextInput, "Database Name", &db);
    click_enabled(&mut h, "Create");
    let db_label: &'static str = Box::leak(format!("  {db}").into_boxed_str());
    wait_for(&mut h, "database listed", |h| item_exists(h, db_label) && has_label_containing(h, "Database created."));

    item(&h, db_label).click_secondary();
    h.step();
    item(&h, "Drop Database…").click();
    wait_for(&mut h, "drop confirmation", |h| has_label_containing(h, "DROP DATABASE"));
    click_enabled(&mut h, "Drop Database");
    wait_for(&mut h, "database removed", |h| !item_exists(h, db_label) && has_label_containing(h, "Database dropped."));

    item(&h, "Docker PG").click_secondary();
    h.step();
    item(&h, "Server Monitor").click();
    wait_for(&mut h, "process list", |h| has_label_containing(h, "other session(s)"));
    assert!(has_label_containing(&h, "client_addr") || has_label_containing(&h, "query"), "monitor columns rendered");
}

#[test]
fn er_diagram_lays_out_tables_and_opens_data() {
    let fx = fixture();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "fk");
    cfg.database = fx.db.to_string_lossy().into_owned();
    fx.rt.block_on(async {
        let mut s = talus_core::engine::Session::connect(&cfg, None, None).await.unwrap();
        s.execute_batch(
            &["CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INT REFERENCES customers(id), total REAL)"
                .into()],
            false,
        )
        .await
        .unwrap();
        s.close().await.unwrap();
    });
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    item(&h, "  main").click();
    wait_for(&mut h, "objects toolbar", |h| item_exists(h, "ER Diagram"));
    item(&h, "ER Diagram").click();
    wait_for(&mut h, "diagram boxes", |h| item_exists(h, "table orders") && item_exists(h, "table customers"));
    let (cx, ox) = {
        let s = h.state();
        let TabKind::Diagram(d) = &s.tabs[s.active].kind else { panic!("diagram tab not active") };
        (d.positions["customers"].x, d.positions["orders"].x)
    };
    assert!(ox > cx, "orders (child) laid out right of customers (parent)");
    shot(&mut h, "05-er-diagram");
    double_click(&mut h, |h| item(h, "table orders"));
    wait_for(&mut h, "orders data tab", |h| {
        has_label_containing(h, "Records 0–0") || has_label_containing(h, "No data")
    });
    shot(&mut h, "10-er-diagram");
}

fn pick(h: &mut egui_kittest::Harness<'static, talus_app::App>, combo_value: &'static str, option: &'static str) {
    // Topmost, then leftmost matching combo (tree order is not layout order).
    let mut combos: Vec<_> = h
        .query_all_by(|n: &egui_kittest::kittest::AccessKitNode<'_>| {
            n.role() == Role::ComboBox && n.value().as_deref() == Some(combo_value)
        })
        .collect();
    combos.sort_by(|a, b| (a.rect().top(), a.rect().left()).partial_cmp(&(b.rect().top(), b.rect().left())).unwrap());
    combos.first().expect("combo").click();
    // Popups lay out disabled for one frame; wait until the option is clickable.
    let opt = move |n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::Button && n.label().as_deref() == Some(option) && !n.is_disabled()
    };
    wait_for(h, option, |h| h.query_by(opt).is_some());
    h.get_by(opt).click();
    let gone = move |n: &egui_kittest::kittest::AccessKitNode<'_>| {
        n.role() == Role::Button && n.label().as_deref() == Some(option)
    };
    wait_for(h, "popup closed", |h| h.query_all_by(gone).next().is_none());
}

#[test]
fn structure_sync_creates_missing_tables_on_target() {
    let fx = fixture();
    let target_db = fx.dir.join("target.sqlite");
    talus_core::engine::create_database_file(&target_db).unwrap();
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    let mut src = ConnectionConfig::new(Engine::Sqlite, "Shop DB");
    src.database = fx.db.to_string_lossy().into_owned();
    let mut dst = ConnectionConfig::new(Engine::Sqlite, "Target");
    dst.database = target_db.to_string_lossy().into_owned();
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [src, dst] }).to_string())
        .unwrap();

    let mut h = harness(&fx);
    h.step();
    double_click(&mut h, |h| item(h, "Shop DB"));
    wait_for(&mut h, "source open", |h| item_exists(h, "  customers"));
    double_click(&mut h, |h| item(h, "Target"));
    wait_for(&mut h, "target open", |h| h.state().states.values().filter(|s| s.status.is_open()).count() == 2);
    // The source side starts from the navigator selection.
    item(&h, "Shop DB").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Structure Synchronization…").click();
    wait_for(&mut h, "sync tab", |h| has_label_containing(h, "Choose both sides"));
    // Topmost first: target connection, then source db (left), then target db (right).
    pick(&mut h, "— choose —", "Target");
    pick(&mut h, "— choose —", "main");
    pick(&mut h, "— choose —", "main");
    click_enabled_containing(&mut h, "Compare");
    wait_for(&mut h, "plan", |h| {
        has_label_containing(h, "+ create table customers")
            && has_label_containing(h, "+ create table notes")
            && has_label_containing(h, "+ create view v_customers")
    });
    click_enabled_containing(&mut h, "Execute on Target");
    wait_for(&mut h, "applied", |h| has_label_containing(h, "Applied 3 statement(s)"));
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "check");
    cfg.database = target_db.to_string_lossy().into_owned();
    let tables = fx.rt.block_on(async {
        let mut s = talus_core::engine::Session::connect(&cfg, None, None).await.unwrap();
        s.list_objects("main", None).await.unwrap().into_iter().map(|o| o.name).collect::<Vec<_>>()
    });
    assert_eq!(tables, ["customers", "notes", "v_customers"], "tables and the view were created");
    wait_for(&mut h, "recompared identical", |h| has_label_containing(h, "Structures are identical"));

    // Data sync: dry run reports the differences without writing, apply copies the rows.
    h.get_by_label("Data synchronization").click();
    h.step();
    h.get_by_role_and_label(Role::CheckBox, "customers").click();
    h.step();
    click_enabled_containing(&mut h, "Compare Data");
    wait_for(&mut h, "dry run result", |h| has_label_containing(h, "customers: differs +"));
    let target_count = |fx: &Fixture| {
        fx.rt.block_on(async {
            let mut s = talus_core::engine::Session::connect(&cfg, None, None).await.unwrap();
            let r = s.run_script("SELECT count(*) FROM customers", &Default::default()).await;
            match &r.results[0].outcome {
                talus_core::model::Outcome::Rows(q) => q.rows[0][0].clone().unwrap(),
                other => panic!("{other:?}"),
            }
        })
    };
    assert_eq!(target_count(&fx), "0", "dry run must not write");
    click_enabled_containing(&mut h, "Sync Data to Target");
    wait_for(&mut h, "confirm", |h| has_label_containing(h, "Confirm Data Sync"));
    click_enabled(&mut h, "Sync Data");
    wait_for(&mut h, "applied", |h| has_label_containing(h, "customers: applied +"));
    let source_count = query_db(&fx, "SELECT count(*) FROM customers")[0][0].clone().unwrap();
    assert_eq!(target_count(&fx), source_count);
    shot(&mut h, "11-structure-sync");
}

#[test]
fn manual_transaction_rollback_discards_and_commit_persists() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    new_query(&mut h, "CREATE TABLE tx_probe (a INTEGER); INSERT INTO tx_probe VALUES (1);");
    button_ending(&h, "Begin").click();
    wait_for(&mut h, "transaction open", |h| has_label_containing(h, "transaction open"));
    button_ending(&h, " Run").click();
    wait_for(&mut h, "script done", |h| has_label_containing(h, "INSERT INTO tx_probe"));
    // Uncommitted work is invisible to another connection.
    assert!(query_db(&fx, "SELECT name FROM sqlite_master WHERE name = 'tx_probe'").is_empty());
    button_ending(&h, "Rollback").click();
    wait_for(&mut h, "transaction closed", |h| !has_label_containing(h, "transaction open"));
    assert!(query_db(&fx, "SELECT name FROM sqlite_master WHERE name = 'tx_probe'").is_empty());

    button_ending(&h, "Begin").click();
    wait_for(&mut h, "transaction open again", |h| has_label_containing(h, "transaction open"));
    button_ending(&h, " Run").click();
    wait_for(&mut h, "script done again", |h| has_label_containing(h, "INSERT INTO tx_probe"));
    button_ending(&h, "Commit").click();
    wait_for(&mut h, "transaction closed after commit", |h| !has_label_containing(h, "transaction open"));
    assert_eq!(query_db(&fx, "SELECT a FROM tx_probe"), vec![vec![Some("1".to_string())]]);
}

#[test]
fn dump_job_and_execute_file_job_roundtrip_through_the_worker() {
    let fx = fixture();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = Worker::new(
        fx.rt.handle().clone(),
        fx.dir.join("known_hosts"),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "dump");
    cfg.database = fx.db.to_string_lossy().into_owned();
    let info = ConnInfo::plain(cfg, None);
    let run = |req: Request| {
        let job = worker.submit(&info, Purpose::Meta, None, req);
        rx.recv_timeout(Duration::from_secs(30)).expect("job finished");
        worker.poll().into_iter().find(|e| e.id == job).expect("event").result
    };
    let out = fx.dir.join("dump.sql");
    match run(Request::DumpSchema { database: "main".into(), schema: None, path: out.clone(), include_data: true }) {
        Ok(Response::Dumped(rep, _)) => assert_eq!((rep.tables, rep.views), (2, 1)),
        other => panic!("{other:?}"),
    }
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("CREATE TABLE customers") && text.contains("INSERT INTO \"customers\""), "{text:.400}");
    run(Request::ExecuteFile { path: out.clone(), transactional: true }).expect_err("tables already exist");
    // Wipe, then restore from the dump.
    run(Request::ExecuteBatch {
        statements: vec!["DROP VIEW v_customers".into(), "DROP TABLE notes".into(), "DROP TABLE customers".into()],
        transactional: true,
    })
    .unwrap();
    match run(Request::ExecuteFile { path: out, transactional: true }) {
        Ok(Response::BatchDone(n)) => assert!(n > 250, "{n}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(query_db(&fx, "SELECT count(*) FROM customers")[0][0].as_deref(), Some("250"));
}

#[test]
fn export_html_xml_and_xlsx_files_are_well_formed() {
    let fx = fixture();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = Worker::new(
        fx.rt.handle().clone(),
        fx.dir.join("known_hosts"),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "export");
    cfg.database = fx.db.to_string_lossy().into_owned();
    let info = ConnInfo::plain(cfg, None);
    let obj = ObjectRef { database: "main".into(), schema: None, name: "customers".into(), kind: ObjectKind::Table };
    let export = |format: ExportFormat| {
        let path = fx.dir.join(format!("customers.{}", format.extension()));
        let job =
            worker.submit(&info, Purpose::Meta, None, Request::Export { obj: obj.clone(), format, path: path.clone() });
        rx.recv_timeout(Duration::from_secs(30)).expect("export finished");
        match worker.poll().into_iter().find(|e| e.id == job).expect("event").result {
            Ok(Response::Exported { rows, .. }) => assert_eq!(rows, 250, "{format:?}"),
            other => panic!("{other:?}"),
        }
        std::fs::read(path).unwrap()
    };
    let html = String::from_utf8(export(ExportFormat::Html)).unwrap();
    assert_eq!(html.matches("<tr>").count(), 251, "header + 250 rows");
    let xml = String::from_utf8(export(ExportFormat::Xml)).unwrap();
    assert_eq!(xml.matches("<row>").count(), 250);
    let xlsx = export(ExportFormat::Xlsx);
    assert_eq!(&xlsx[..4], b"PK\x03\x04", "xlsx is a zip container");
    assert!(xlsx.windows(b"xl/worksheets/sheet1.xml".len()).any(|w| w == b"xl/worksheets/sheet1.xml"));
}

#[test]
fn routines_tab_lists_triggers_with_definition() {
    let fx = fixture();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "setup");
    cfg.database = fx.db.to_string_lossy().into_owned();
    fx.rt.block_on(async {
        let mut s = talus_core::engine::Session::connect(&cfg, None, None).await.unwrap();
        s.execute_batch(&["CREATE TRIGGER trg_probe AFTER INSERT ON customers BEGIN SELECT 1; END".to_string()], false)
            .await
            .unwrap();
    });
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    item(&h, "main").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Routines and Triggers").click();
    wait_for(&mut h, "trigger listed", |h| has_label_containing(h, "trg_probe"));
    wait_for(&mut h, "definition shown", |h| has_label_containing(h, "CREATE TRIGGER trg_probe"));
}

#[test]
fn users_tab_lists_roles_and_shows_grants_on_postgres() {
    let Ok(pg) = std::env::var("TALUS_TEST_PG") else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let p: Vec<&str> = pg.splitn(5, ':').collect();
    let fx = fixture();
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Postgres, "Docker PG");
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = talus_core::model::SslMode::Disable;
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [cfg] }).to_string()).unwrap();
    let mut h = harness(&fx);
    h.step();
    double_click(&mut h, |h| item(h, "Docker PG"));
    wait_for(&mut h, "password prompt", |h| has_label_containing(h, "Enter the database password"));
    type_into(&mut h, Role::PasswordInput, "Password", p[3]);
    click_enabled(&mut h, "Connect");
    wait_for(&mut h, "connected", |h| item_exists(h, "  public"));
    item(&h, "Docker PG").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Users and Privileges").click();
    wait_for(&mut h, "roles listed", |h| has_label_containing(h, "superuser"));
    // Selecting an account loads its grants without error.
    h.get_by_label_contains("superuser").click();
    h.step();
    wait_for(&mut h, "grants area settled", |h| !has_label_containing(h, "Select an account"));
    // The privilege editor builds the statement and asks before running it.
    item(&h, "Privileges…").click();
    wait_for(&mut h, "editor", |h| h.query_by_role_and_label(Role::TextInput, "Database").is_some());
    type_into(&mut h, Role::TextInput, "Database", p[4]);
    h.get_by_role_and_label(Role::CheckBox, "CONNECT").click();
    h.step();
    click_enabled(&mut h, "Grant");
    wait_for(&mut h, "confirmation", |h| has_label_containing(h, "GRANT CONNECT ON DATABASE"));
}

#[test]
fn query_builder_generates_sql_and_opens_it_in_a_query_tab() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    item(&h, "main").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Query Builder").click();
    wait_for(&mut h, "generated SQL", |h| has_label_containing(h, "FROM \"main\".\"customers\" AS \"customers\""));
    click_enabled_containing(&mut h, "Open in Query");
    wait_for(&mut h, "query editor", |h| h.query_by_role(Role::MultilineTextInput).is_some());
    assert!(
        editor_text(&h).contains("SELECT *\nFROM \"main\".\"customers\" AS \"customers\"\nLIMIT 1000"),
        "{}",
        editor_text(&h)
    );
    shot(&mut h, "14-query-builder");
}

#[test]
fn sql_server_connect_browse_query_and_users_tab() {
    let Ok(ms) = std::env::var("TALUS_TEST_MSSQL") else {
        eprintln!("TALUS_TEST_MSSQL not set; skipping");
        return;
    };
    let p: Vec<&str> = ms.splitn(5, ':').collect();
    let fx = fixture();
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::SqlServer, "Docker MSSQL");
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = talus_core::model::SslMode::Disable;
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [cfg] }).to_string()).unwrap();
    let mut h = harness(&fx);
    h.step();
    double_click(&mut h, |h| item(h, "Docker MSSQL"));
    wait_for(&mut h, "password prompt", |h| has_label_containing(h, "Enter the database password"));
    type_into(&mut h, Role::PasswordInput, "Password", p[3]);
    click_enabled(&mut h, "Connect");
    // The configured database opens with its default schema expanded.
    wait_for(&mut h, "schemas listed", |h| item_exists(h, "  dbo"));

    sidebar_item(&h, "New Query").click();
    wait_for(&mut h, "query editor", |h| h.query_by_role(Role::MultilineTextInput).is_some());
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text("SELECT 42 AS answer, N'häl' AS txt;");
    h.step();
    button_ending(&h, " Run").click();
    wait_for(&mut h, "result row", |h| has_label_containing(h, "answer") && has_label_containing(h, "häl"));

    item(&h, "Docker MSSQL").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Users and Privileges").click();
    wait_for(&mut h, "logins listed", |h| has_label_containing(h, "sql login"));
}

fn nosql_cfg(var: &str, engine: Engine, name: &str, fx: &Fixture) -> Option<(Vec<String>, ConnectionConfig)> {
    let v = std::env::var(var).ok()?;
    let p: Vec<String> = v.splitn(5, ':').map(String::from).collect();
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    let mut cfg = ConnectionConfig::new(engine, name);
    cfg.host = p[0].clone();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].clone();
    cfg.database = p[4].clone();
    cfg.ssl_mode = talus_core::model::SslMode::Disable;
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [cfg] }).to_string()).unwrap();
    Some((p, cfg))
}

fn connect_with_prompt(h: &mut egui_kittest::Harness<'static, talus_app::App>, name: &'static str, password: &str) {
    double_click(h, |h| item(h, name));
    wait_for(h, "password prompt", |h| has_label_containing(h, "Enter the database password"));
    type_into(h, Role::PasswordInput, "Password", password);
    click_enabled(h, "Connect");
}

#[test]
fn redis_key_browser_and_console() {
    let fx = fixture();
    let Some((p, cfg)) = nosql_cfg("TALUS_TEST_REDIS", Engine::Redis, "Docker Redis", &fx) else {
        eprintln!("TALUS_TEST_REDIS not set; skipping");
        return;
    };
    let key = format!("talus-ui:{}", uuid::Uuid::new_v4().simple());
    let seed = |cmd: String| {
        fx.rt.block_on(async {
            let pw = talus_core::Secret::new(p[3].clone());
            let mut r = talus_core::nosql::redis::RedisSession::connect(&cfg, Some(&pw), Some("db0")).await.unwrap();
            r.command(&cmd).await.unwrap()
        })
    };
    seed(format!("SET {key} 'hello from redis'"));
    let mut h = harness(&fx);
    h.step();
    connect_with_prompt(&mut h, "Docker Redis", &p[3]);
    // db0 opens by itself and lists its one pseudo-object.
    wait_for(&mut h, "keys object", |h| item_exists(h, "keys"));
    double_click(&mut h, |h| item(h, "keys"));
    wait_for(&mut h, "key listed", |h| has_label_containing(h, &key));
    h.get_by_label_contains(&key).click();
    wait_for(&mut h, "value loaded", |h| {
        h.query_all_by_role(Role::MultilineTextInput).any(|n| n.value().is_some_and(|v| v.contains("hello from redis")))
    });

    // The console runs raw commands.
    // The navigator row comes first; the open tab's title mentions the connection too.
    h.query_all_by(|n| n.role() == Role::Button && n.label().is_some_and(|l| l.contains("Docker Redis")))
        .next()
        .expect("navigator row")
        .click();
    h.step();
    sidebar_item(&h, "New Query").click();
    wait_for(&mut h, "console tab", |h| has_label_containing(h, "Console —"));
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text(&format!("GET {key}"));
    h.step();
    button_ending(&h, " Run").click();
    wait_for(&mut h, "reply", |h| has_label_containing(h, "\"hello from redis\""));
    let _ = seed(format!("DEL {key}"));
    shot(&mut h, "16-redis");
}

#[test]
fn mongo_document_browser_edit_and_console() {
    let fx = fixture();
    let Some((p, cfg)) = nosql_cfg("TALUS_TEST_MONGO", Engine::MongoDb, "Docker Mongo", &fx) else {
        eprintln!("TALUS_TEST_MONGO not set; skipping");
        return;
    };
    let coll = format!("talus_ui_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let pw = talus_core::Secret::new(p[3].clone());
    let connect_db = || async { talus_core::nosql::mongo::MongoSession::connect(&cfg, Some(&pw), None).await.unwrap() };
    fx.rt.block_on(async {
        let mut m = connect_db().await;
        m.insert("admin", &coll, r#"{"_id": 1, "name": "Ann", "city": "Oslo"}"#).await.unwrap();
        m.insert("admin", &coll, r#"{"_id": 2, "name": "Bob", "city": "Rome"}"#).await.unwrap();
    });
    let mut h = harness(&fx);
    h.step();
    connect_with_prompt(&mut h, "Docker Mongo", &p[3]);
    wait_for(&mut h, "collection listed", |h| item_exists(h, &coll));
    let label: &'static str = Box::leak(coll.clone().into_boxed_str());
    double_click(&mut h, |h| item(h, label));
    wait_for(&mut h, "documents", |h| has_label_containing(h, "Ann") && has_label_containing(h, "Bob"));
    assert!(has_label_containing(&h, "1–2 of 2"));
    fx.rt.block_on(async { connect_db().await.drop_collection("admin", &coll).await.unwrap() });
    shot(&mut h, "17-mongo");
}

#[test]
fn result_chart_suggests_columns_and_renders() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    new_query(&mut h, "SELECT name, id * 10 AS score, id AS rank FROM customers WHERE id <= 6 ORDER BY id;");
    button_ending(&h, " Run").click();
    wait_for(&mut h, "result", |h| has_label_containing(h, "Customer 3"));
    button_ending(&h, " Chart").click();
    // Bar chart of the two numeric columns against the name column.
    wait_for(&mut h, "chart controls", |h| has_label_containing(h, "score") && has_label_containing(h, "rank"));
    for _ in 0..3 {
        h.step();
    }
    shot(&mut h, "chart");
    assert!(!has_label_containing(&h, "no numeric column"));
}

#[test]
fn due_jobs_run_by_themselves_and_are_recorded() {
    use talus_core::jobs::{Job, JobAction, JobRun, JobsFile, Schedule, TIME_FORMAT};
    let fx = fixture();
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "Shop DB");
    cfg.database = fx.db.to_string_lossy().into_owned();
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [cfg] }).to_string()).unwrap();
    let out = fx.dir.join("scheduled.csv");
    let long_ago = chrono::Local::now().naive_local() - chrono::Duration::hours(3);
    let mut due = Job::new(
        "nightly export".into(),
        cfg.id,
        JobAction::ExportTable {
            database: "main".into(),
            schema: None,
            table: "customers".into(),
            format: "csv".into(),
            path: out.to_string_lossy().into_owned(),
        },
        Schedule::EveryMinutes(60),
        long_ago,
    );
    due.last_run =
        Some(JobRun { at: long_ago.format(TIME_FORMAT).to_string(), ok: true, message: "old".into(), duration_ms: 1 });
    let manual = Job::new(
        "manual only".into(),
        cfg.id,
        JobAction::RunSql { sql: "SELECT 1".into(), database: None },
        Schedule::Manual,
        long_ago,
    );
    std::fs::write(support.join("jobs.json"), serde_json::to_string(&JobsFile { jobs: vec![due, manual] }).unwrap())
        .unwrap();

    let mut h = harness(&fx);
    for _ in 0..200 {
        h.step();
        if out.exists() && has_label_containing(&h, "finished") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    wait_for(&mut h, "job finished notice", |h| {
        has_label_containing(h, "nightly export") && has_label_containing(h, "finished")
    });
    assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 251);
    let saved: JobsFile = serde_json::from_str(&std::fs::read_to_string(support.join("jobs.json")).unwrap()).unwrap();
    let ran = saved.jobs[0].last_run.as_ref().unwrap();
    assert!(ran.ok && ran.message.contains("exported 250 row(s)"), "{ran:?}");
    assert!(saved.jobs[1].last_run.is_none(), "manual jobs never start by themselves");
    assert!(support.join("jobs.log").exists());
}

#[test]
fn automation_tab_creates_a_job_and_runs_it() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Automation (Jobs)…").click();
    wait_for(&mut h, "automation tab", |h| has_label_containing(h, "No jobs yet"));
    item(&h, "New Job").click();
    wait_for(&mut h, "form", |h| h.query_by_role_and_label(Role::TextInput, "Name").is_some());
    type_into(&mut h, Role::TextInput, "Name", "probe");
    h.get_by_role_and_label(Role::MultilineTextInput, "SQL").focus();
    h.step();
    h.get_by_role_and_label(Role::MultilineTextInput, "SQL").type_text("SELECT count(*) FROM customers");
    h.step();
    click_enabled(&mut h, "Save");
    wait_for(&mut h, "job listed", |h| has_label_containing(h, "probe") && has_label_containing(h, "never"));
    // Run it by hand: the result is recorded in the list.
    h.query_all_by(|n| n.role() == Role::Button && n.label().is_some_and(|l| l.contains(egui_phosphor::regular::PLAY)))
        .next()
        .expect("run button")
        .click();
    wait_for(&mut h, "recorded", |h| has_label_containing(h, "finished"));
    assert!(fx.dir.join("support").join("jobs.json").exists());
    shot(&mut h, "13-automation");
}

/// First button whose label contains `text`: the navigator row precedes tabs and panels.
fn first_item<'h>(h: &'h egui_kittest::Harness<'static, talus_app::App>, text: &'static str) -> egui_kittest::Node<'h> {
    h.query_all_by(move |n| n.role() == Role::Button && n.label().is_some_and(|l| l.contains(text)))
        .next()
        .unwrap_or_else(|| panic!("no button containing {text:?}"))
}

#[test]
fn profile_tab_and_test_data_generation_from_the_table_menu() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    first_item(&h, "customers").click_secondary();
    h.step();
    item(&h, "Profile Columns").click();
    wait_for(&mut h, "profile grid", |h| has_label_containing(h, "NULL %") && has_label_containing(h, "Most frequent"));
    wait_for(&mut h, "email stats", |h| has_label_containing(h, "email") && has_label_containing(h, "250"));

    // Back to the objects: generate 20 rows into customers (the navigator row comes first in the tree).
    first_item(&h, "customers").click_secondary();
    h.step();
    item(&h, "Generate Test Data…").click();
    wait_for(&mut h, "dialog", |h| h.query_by_role_and_label(Role::TextInput, "Rows").is_some());
    h.get_by_role_and_label(Role::TextInput, "Rows").focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Rows").type_text("20");
    h.step();
    click_enabled(&mut h, "Generate");
    wait_for(&mut h, "done", |h| has_label_containing(h, "Inserted 20 generated row(s)"));
    assert_eq!(query_db(&fx, "SELECT count(*) FROM customers")[0][0].as_deref(), Some("270"));
    // Optional columns may stay NULL; every value that was generated looks like an e-mail address.
    assert_eq!(
        query_db(&fx, "SELECT count(*) FROM customers WHERE email LIKE '%@example.test'")[0][0],
        query_db(&fx, "SELECT count(email) FROM customers")[0][0]
    );
    shot(&mut h, "18-profile");
}

#[test]
fn data_dictionary_job_through_the_worker_writes_html_and_markdown() {
    let fx = fixture();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = Worker::new(
        fx.rt.handle().clone(),
        fx.dir.join("known_hosts"),
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    let mut cfg = ConnectionConfig::new(Engine::Sqlite, "dict");
    cfg.database = fx.db.to_string_lossy().into_owned();
    let info = ConnInfo::plain(cfg, None);
    for (format, name) in [
        (talus_core::dictionary::DictFormat::Html, "dict.html"),
        (talus_core::dictionary::DictFormat::Markdown, "dict.md"),
    ] {
        let path = fx.dir.join(name);
        let job = worker.submit(
            &info,
            Purpose::Meta,
            None,
            Request::Dictionary { database: "main".into(), schema: None, format, path: path.clone() },
        );
        rx.recv_timeout(Duration::from_secs(20)).expect("dictionary written");
        match worker.poll().into_iter().find(|e| e.id == job).expect("event").result {
            Ok(Response::DictionaryWritten { tables, .. }) => assert_eq!(tables, 2),
            other => panic!("{other:?}"),
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("customers") && text.contains("email") && text.contains("v_customers"), "{name}");
    }
}

#[test]
fn model_designer_reverses_a_schema_and_generates_sql() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    connect(&mut h, &fx);
    item(&h, "main").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Data Modeling").click();
    wait_for(&mut h, "model tab", |h| has_label_containing(h, "New Table"));
    click_enabled_containing(&mut h, "Reverse from Selection");
    wait_for(&mut h, "reversed tables", |h| h.query_by_label("model table customers").is_some());
    // A freshly reversed model has no differences from its own schema.
    click_enabled_containing(&mut h, "Compare with Selection");
    wait_for(&mut h, "comparison", |h| has_label_containing(h, "consistent with the database"));
    click_enabled_containing(&mut h, "Generate SQL");
    wait_for(&mut h, "forward SQL", |h| has_label_containing(h, "statement(s)"));
    click_enabled_containing(&mut h, "Open in Query");
    wait_for(&mut h, "query editor", |h| h.query_by_role(Role::MultilineTextInput).is_some());
    assert!(editor_text(&h).contains("CREATE TABLE"), "{}", editor_text(&h));
}

#[test]
fn cloud_preset_fills_engine_port_and_tls() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    h.get_by_label_contains("New Connection").click();
    wait_for(&mut h, "dialog", |h| has_label_containing(h, "Cloud preset"));
    // A pointer click does not open a combo inside a window under kittest; the accessibility action does.
    h.get_by_role(Role::ComboBox).click_accesskit();
    wait_for(&mut h, "preset list", |h| h.query_by_label("Amazon AWS — RDS for MySQL").is_some());
    h.get_by_label("Amazon AWS — RDS for MySQL").click_accesskit();
    h.step();
    h.step();
    assert!(input_with_value(&h, "3306").is_some());
    assert!(
        h.query_by(|n| n.role() == Role::ComboBox && n.value().as_deref() == Some("Amazon AWS — RDS for MySQL"))
            .is_some()
    );
    assert!(
        h.query_by(
            |n| n.label().as_deref() == Some("verify-full") && n.toggled() == Some(egui::accesskit::Toggled::True)
        )
        .is_some()
    );
}

#[test]
fn model_type_conversion_gates_sql_generation() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Data Modeling").click();
    wait_for(&mut h, "model tab", |h| has_label_containing(h, "New Table"));
    let pick = |h: &mut egui_kittest::Harness<'static, talus_app::App>, kind: &'static str| {
        h.get_by_role_and_label(Role::ComboBox, "Model type").click();
        wait_for(h, "model type list", |h| h.query_by_label(kind).is_some());
        h.get_by_label(kind).click();
        h.step();
        h.step();
    };
    pick(&mut h, "Conceptual");
    assert!(has_label_containing(&h, "New Entity"));
    assert!(h.query_by_label_contains("Generate SQL").is_none(), "conceptual models generate no SQL");
    assert!(has_label_containing(&h, "switch Model type to Physical"));
    pick(&mut h, "Physical");
    assert!(has_label_containing(&h, "Generate SQL"));
    assert!(has_label_containing(&h, "New Table"));
}

#[test]
fn plpgsql_debugger_steps_and_finishes_from_the_routines_tab() {
    let Ok(pg) = std::env::var("TALUS_TEST_PG") else {
        eprintln!("TALUS_TEST_PG not set; skipping");
        return;
    };
    let p: Vec<&str> = pg.splitn(5, ':').collect();
    let fx = fixture();
    let mut cfg = ConnectionConfig::new(Engine::Postgres, "Docker PG");
    cfg.host = p[0].into();
    cfg.port = p[1].parse().unwrap();
    cfg.user = p[2].into();
    cfg.database = p[4].into();
    cfg.ssl_mode = talus_core::model::SslMode::Disable;
    fx.rt.block_on(async {
        let pw = talus_core::Secret::new(p[3]);
        let mut s = talus_core::engine::Session::connect(&cfg, Some(&pw), None).await.unwrap();
        let r = s
            .run_script(
                "CREATE EXTENSION IF NOT EXISTS pldbgapi;
                 CREATE OR REPLACE FUNCTION public.ui_dbg_twice(n integer) RETURNS integer LANGUAGE plpgsql AS $b$
                 DECLARE r integer;
                 BEGIN
                   r := n * 2;
                   RETURN r;
                 END $b$;",
                &talus_core::engine::ScriptOptions::default(),
            )
            .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        s.close().await.unwrap();
    });
    let support = fx.dir.join("support");
    std::fs::create_dir_all(&support).unwrap();
    std::fs::write(support.join("connections.json"), serde_json::json!({ "connections": [cfg] }).to_string()).unwrap();
    let mut h = harness(&fx);
    h.step();
    double_click(&mut h, |h| item(h, "Docker PG"));
    wait_for(&mut h, "password prompt", |h| has_label_containing(h, "Enter the database password"));
    type_into(&mut h, Role::PasswordInput, "Password", p[3]);
    click_enabled(&mut h, "Connect");
    wait_for(&mut h, "connected", |h| item_exists(h, "  public"));
    item(&h, "  public").click();
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Routines and Triggers").click();
    wait_for(&mut h, "function listed", |h| has_label_containing(h, "ui_dbg_twice(n integer)"));
    assert!(!has_label_containing(&h, "pldbg_"), "extension routines are not listed");
    item(&h, "ui_dbg_twice(n integer)").click();
    h.step();
    click_enabled_containing(&mut h, "Debug");
    wait_for(&mut h, "debug tab", |h| has_label_containing(h, "Input parameters"));
    type_into(&mut h, Role::TextInput, "n  integer", "21");
    click_enabled_containing(&mut h, "Start");
    wait_for(&mut h, "paused", |h| has_label_containing(h, "Paused at line"));
    click_enabled_containing(&mut h, "Step Over");
    wait_for(&mut h, "stepped", |h| h.query_all_by_label_contains("Paused at line").count() >= 2);
    shot(&mut h, "05-debugger-paused");
    click_enabled_containing(&mut h, "Continue");
    wait_for(&mut h, "finished", |h| has_label_containing(h, "Finished."));
    assert!(has_label_containing(&h, "Result: ui_dbg_twice = 42"));
}

#[test]
fn data_vault_templates_build_a_consistent_model() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    h.get_by_label("Tools").click();
    h.step();
    item(&h, "Data Modeling").click();
    wait_for(&mut h, "model tab", |h| has_label_containing(h, "New Table"));
    let template = |h: &mut egui_kittest::Harness<'static, talus_app::App>, which: &'static str| {
        item(h, "Templates").click();
        h.step();
        click_enabled(h, which);
        h.step();
    };
    template(&mut h, "New Hub");
    template(&mut h, "New Hub");
    // The new link is selected, so the satellite hangs off it.
    template(&mut h, "New Link");
    template(&mut h, "New Satellite");
    for t in ["hub1", "hub2", "link3", "sat_link3_4"] {
        assert!(h.query_by_label(&format!("model table {t}")).is_some(), "{t} on the canvas");
    }
    click_enabled_containing(&mut h, "Check");
    wait_for(&mut h, "check result", |h| has_label_containing(h, "The model is consistent"));
    shot(&mut h, "06-model-data-vault");
}
