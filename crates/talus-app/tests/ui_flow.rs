//! Headless UI tests (egui_kittest + AccessKit) driving the real App against a real
//! SQLite file. No mocks: the worker, sessions and SQLite engine are the production ones;
//! only the Keychain is replaced by an in-memory secret store.

mod common;

use common::*;
use egui::accesskit::Role;
use egui_kittest::kittest::Queryable;

#[test]
fn vertical_slice_connect_browse_query_error_disconnect() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    assert!(has_label_containing(&h, "No connections yet"));

    create_connection_via_dialog(&mut h, &fx);
    // Persisted without any secret material.
    let saved = std::fs::read_to_string(fx.dir.join("support").join("connections.json")).unwrap();
    assert!(saved.contains("Shop DB"));

    // Connect by double-clicking the connection; the tree auto-expands `main`.
    double_click(&mut h, |h| item(h, "Shop DB"));
    wait_for(&mut h, "tables listed", |h| {
        has_label_containing(h, "customers") && has_label_containing(h, "v_customers")
    });
    assert!(has_label_containing(&h, "Tables (2)"));
    assert!(has_label_containing(&h, "Views (1)"));
    // Connection selected: the Home tab shows the dashboard.
    shot(&mut h, "00-home");

    // Selecting a table loads its DDL into the information pane.
    item(&h, "  customers").click();
    wait_for(&mut h, "DDL in info pane", |h| has_label_containing(h, "CREATE TABLE customers"));
    shot(&mut h, "00-tree-and-ddl");

    // New query tab bound to the selected connection.
    // The sidebar item (exact label "New Query").
    sidebar_item(&h, "New Query").click();
    wait_for(&mut h, "query editor", |h| h.query_by_role(Role::MultilineTextInput).is_some());
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text(
        "SELECT id, name FROM customers WHERE id <= 3 ORDER BY id;\nSELECT count(*) AS total FROM customers;",
    );
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
    wait_for(&mut h, "first result grid", |h| has_label_containing(h, "Customer 3"));
    assert!(has_label_containing(&h, "Result 2"), "two result sets expected");
    assert!(h.query_by_label_contains("Customer 4").is_none(), "WHERE clause respected");
    shot(&mut h, "01-query-results");

    // An error is reported with its line number; the UI stays usable.
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_role(Role::MultilineTextInput).type_text("SELECT 1;\nSELEC nonsense;");
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
    wait_for(&mut h, "error message", |h| has_label_containing(h, "[ERROR] line 2"));
    shot(&mut h, "02-query-error");
    // Back on Home the dashboard lists the statements just run.
    sidebar_item(&h, "Home").click();
    item(&h, "Shop DB").click();
    wait_for(&mut h, "recent queries on Home", |h| has_label_containing(h, "SELEC nonsense"));
    shot(&mut h, "04-home-activity");
    assert!(has_label_containing(&h, "1 connection · 1 open"));
    // Caption links under the cards are actions.
    item(&h, "Open history").click();
    wait_for(&mut h, "history tab", |h| {
        h.state().tabs.iter().any(|t| matches!(t.kind, talus_app::app::TabKind::History(_)))
    });

    // History recorded both the successful and the failed statements.
    let hist = std::fs::read_to_string(fx.dir.join("support").join("history.jsonl")).unwrap();
    assert!(hist.contains("SELECT count(*) AS total FROM customers"));
    assert!(hist.contains("\"ok\":false"));

    // Disconnect via the context menu.
    item(&h, "Shop DB").click_secondary();
    h.step();
    h.get_by_label("Close Connection").click();
    wait_for(&mut h, "tree collapsed", |h| h.query_by_label_contains("v_customers").is_none());
    assert!(!h.state().states.values().any(|s| s.status.is_open()));
}

#[test]
fn data_editor_edit_apply_and_readonly_without_key() {
    let fx = fixture();
    let mut h = harness(&fx);
    h.step();
    create_connection_via_dialog(&mut h, &fx);
    double_click(&mut h, |h| item(h, "Shop DB"));
    wait_for(&mut h, "tables listed", |h| has_label_containing(h, "customers"));

    // Open table data (double-click in the navigator).
    double_click(&mut h, |h| item(h, "  customers"));
    wait_for(&mut h, "page loaded", |h| has_label_containing(h, "c1@example.test"));
    assert!(has_label_containing(&h, "Records 1–250"));

    // Edit a cell inline and apply.
    double_click(&mut h, |h| h.get_by_label("c2@example.test"));
    h.step();
    h.step();
    assert!(h.query_all_by_role(Role::TextInput).any(|n| n.is_focused()), "inline cell editor has focus");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.query_all_by_role(Role::TextInput)
        .find(|n| n.is_focused())
        .expect("editor still focused")
        .type_text("changed@example.test");
    h.step();
    h.key_press(egui::Key::Enter);
    wait_for(&mut h, "pending change", |h| has_label_containing(h, "1 pending change"));
    shot(&mut h, "03-data-pending-edit");
    item(&h, "Apply").click();
    wait_for(&mut h, "saved", |h| has_label_containing(h, "Saved: 1 updated"));
    assert_eq!(
        query_db(&fx, "SELECT email FROM customers WHERE id = 2")[0][0].as_deref(),
        Some("changed@example.test")
    );

    // Sorting by header reloads from the server (descending after two clicks).
    h.get_by_label("id").click();
    wait_for(&mut h, "sorted asc", |h| has_label_containing(h, "id ▲"));
    h.get_by_label("id ▲").click();
    wait_for(&mut h, "sorted desc", |h| {
        has_label_containing(h, "id ▼") && has_label_containing(h, "c250@example.test")
    });

    // SQLite tables without a primary key are editable through rowid; views are read-only.
    double_click(&mut h, |h| item(h, "  v_customers"));
    wait_for(&mut h, "view opened", |h| has_label_containing(h, "Views are read-only"));
}
