//! Visual review aid: renders key dialogs in both themes when `TALUS_SCREENSHOT_DIR` is set.
mod common;

use common::*;
use egui_kittest::kittest::Queryable;
use talus_core::store::config::Theme;

#[test]
fn dialogs_in_both_themes() {
    for (theme, tag) in [(Theme::Light, "light"), (Theme::Dark, "dark")] {
        let fx = fixture();
        let mut h = harness(&fx);
        h.state_mut().settings.theme = theme;
        h.step();
        h.get_by_label_contains("Settings").click();
        settle(&mut h);
        shot(&mut h, &format!("v-settings-{tag}"));
        click_first(&mut h, "Save");
        h.step();
        h.get_by_label_contains("New Connection").click();
        settle(&mut h);
        shot(&mut h, &format!("v-conn-{tag}"));
        click_first(&mut h, "Oracle");
        settle(&mut h);
        shot(&mut h, &format!("v-conn-oracle-{tag}"));
        h.step();
    }
}

fn settle(h: &mut egui_kittest::Harness<'_, talus_app::App>) {
    for _ in 0..8 {
        h.step();
    }
}

fn click_first(h: &mut egui_kittest::Harness<'_, talus_app::App>, label: &str) {
    h.query_all_by_role_and_label(egui::accesskit::Role::Button, label)
        .next()
        .unwrap_or_else(|| panic!("no {label}"))
        .click();
    h.step();
}

/// Tabs that normally need a live server (Redis keys, accounts) rendered from canned data.
#[test]
fn server_tabs_in_both_themes() {
    use talus_app::state::Loadable;
    use talus_app::theme;
    use talus_app::ui::{docs_tab::DocsTab, keys_tab::KeysTab, users_tab::UsersTab};
    use talus_core::model::Engine;
    use talus_core::nosql::NoSqlOut;
    use talus_core::nosql::mongo::DocPage;
    use talus_core::nosql::redis::KeyInfo;
    use talus_core::sql::users::UserInfo;
    use uuid::Uuid;

    let Ok(dir) = std::env::var("TALUS_SCREENSHOT_DIR") else { return };
    for (th, tag) in [(Theme::Light, "light"), (Theme::Dark, "dark")] {
        for (name, which) in [("redis", 0), ("users", 1), ("mongo", 2)] {
            let keys = KeysTab::new(Uuid::nil(), "Cache".into(), "db0".into());
            let mut users = UsersTab::new(Uuid::nil(), "Prod".into(), Engine::Postgres);
            let u = |n: &str, a: &[&str]| UserInfo {
                name: n.into(),
                host: None,
                attrs: a.iter().map(|s| s.to_string()).collect(),
            };
            users.users = Loadable::Loaded(vec![
                u("postgres", &["superuser", "login"]),
                u("app", &["login"]),
                u("readonly", &[]),
            ]);
            users.selected = Some(1);
            users.grants = Loadable::Loaded(vec!["GRANT SELECT ON ALL TABLES IN SCHEMA public TO app".into()]);
            let docs = DocsTab::new(Uuid::nil(), "Atlas".into(), "shop".into(), "orders".into());
            let state = (keys, users, docs);
            let mut fonts_done = false;
            let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 620.0)).build_ui_state(
                move |ui, (keys, users, docs): &mut (KeysTab, UsersTab, DocsTab)| {
                    // Fonts take effect on the next pass; the UI asks for the semibold family, so skip frame one.
                    if !fonts_done {
                        theme::install_fonts(ui.ctx());
                        fonts_done = true;
                        return;
                    }
                    theme::apply(ui.ctx(), th, 13.0);
                    match which {
                        0 => drop(keys.ui(ui, 1)),
                        1 => drop(users.ui(ui, 2)),
                        _ => drop(docs.ui(ui, 3)),
                    }
                },
                state,
            );
            h.step();
            h.state_mut().0.busy = false;
            let k = |n: &str, t: &str, ttl| KeyInfo { key: n.into(), ty: t.into(), ttl_ms: ttl };
            h.state_mut().0.on_result(
                "scan",
                Ok(NoSqlOut::Scan {
                    cursor: 0,
                    keys: vec![
                        k("session:42", "hash", Some(3_600_000)),
                        k("user:1001", "string", None),
                        k("queue:emails", "list", None),
                        k("leaderboard", "zset", None),
                    ],
                }),
            );
            h.state_mut().2.on_result(
                "find",
                Ok(NoSqlOut::Docs(DocPage {
                    docs: vec![
                        "{\n  \"_id\": {\"$oid\": \"650f1c\"},\n  \"customer\": \"Ada\",\n  \"total\": 42.5\n}".into(),
                        "{\n  \"_id\": {\"$oid\": \"650f1d\"},\n  \"customer\": \"Linus\",\n  \"total\": 17\n}".into(),
                    ],
                    ids: vec!["{\"$oid\":\"650f1c\"}".into(), "{\"$oid\":\"650f1d\"}".into()],
                    total: 2,
                })),
            );
            for _ in 0..4 {
                h.step();
            }
            h.render().expect("render").save(std::path::Path::new(&dir).join(format!("v-{name}-{tag}.png"))).unwrap();
        }
    }
}
