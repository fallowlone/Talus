//! Objects list (centre "Objects" tab), information pane, structure tab and history tab.

use std::collections::HashMap;

use egui::RichText;
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icons;
use talus_core::model::{ConnectionConfig, ObjectRef, TableStructure};
use talus_core::store::history::HistoryEntry;
use uuid::Uuid;

use crate::actions::Action;
use crate::state::{ConnState, ConnStatus, Loadable, NavSel, kind_icon};
use crate::theme;

pub fn objects_view(
    ui: &mut egui::Ui,
    connections: &[ConnectionConfig],
    states: &HashMap<Uuid, ConnState>,
    sel: &Option<NavSel>,
    filter: &mut String,
    actions: &mut Vec<Action>,
) {
    let p = theme::current(ui.ctx());
    let Some(sel) = sel else {
        empty_hint(ui, "Select a database or schema in the navigator to list its objects.");
        return;
    };
    let conn = sel.conn();
    let Some(cfg) = connections.iter().find(|c| c.id == conn) else { return };
    let Some(state) = states.get(&conn) else { return };
    if !state.status.is_open() {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.label(RichText::new(format!("{} is not connected", cfg.name)).color(p.text_dim));
            if ui.button(format!("{} Open Connection", icons::PLUGS)).clicked() {
                actions.push(Action::Open(conn));
            }
        });
        return;
    }
    let Some((db, schema)) = sel.object_scope() else {
        connection_summary(ui, cfg, state);
        return;
    };
    if cfg.engine.capabilities().schemas && schema.is_none() {
        empty_hint(ui, "Expand the database and choose a schema.");
        return;
    }
    let objs = state.objects(&db, schema.as_deref());
    let count = match objs {
        Some(Loadable::Loaded(list)) => format!("{} objects", list.len()),
        _ => "Loading…".into(),
    };
    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            let scope = schema.as_ref().map_or(db.clone(), |s| format!("{db}.{s}"));
            ui.label(RichText::new(scope).font(theme::semibold(26.0)).color(p.text));
            ui.label(RichText::new(format!("{count} · {} · {}", cfg.engine.label(), cfg.name)).color(p.text_dim));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(format!("{} ER Diagram", icons::GRAPH)).clicked() {
                actions.push(Action::OpenDiagram { conn, db: db.clone(), schema: schema.clone() });
            }
            if ui.button(format!("{} New Table", icons::PLUS_SQUARE)).clicked() {
                actions.push(Action::NewTable { conn, db: db.clone(), schema: schema.clone() });
            }
            if ui.button(format!("{} New Query", icons::FILE_SQL)).clicked() {
                actions.push(Action::NewQuery { conn: Some(conn), database: Some(db.clone()), text: String::new() });
            }
            if ui.button(format!("{} Refresh", icons::ARROW_CLOCKWISE)).clicked() {
                actions.push(Action::Refresh(sel.clone()));
            }
        });
    });
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        super::widgets::card_header(ui, icons::STACK, "Tables and Views", false);
    });
    ui.add_space(6.0);
    super::widgets::card(ui, egui::Margin::symmetric(12, 8), |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(ui.available_height() - 24.0);
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text(format!("{} Search objects", icons::MAGNIFYING_GLASS))
                .desired_width(240.0),
        );
        ui.add_space(4.0);
        match objs {
            None | Some(Loadable::NotLoaded) => {
                actions.push(Action::LoadObjects { conn, db, schema });
                ui.spinner();
            }
            Some(Loadable::Loading(_)) => {
                ui.spinner();
            }
            Some(Loadable::Failed(e)) => {
                ui.label(RichText::new(e).color(p.danger));
            }
            Some(Loadable::Loaded(list)) => {
                let f = filter.trim().to_lowercase();
                let rows: Vec<&ObjectRef> = list
                    .iter()
                    .map(|n| &n.obj)
                    .filter(|o| f.is_empty() || o.name.to_lowercase().contains(&f))
                    .collect();
                if rows.is_empty() {
                    empty_hint(ui, "No objects.");
                    return;
                }
                TableBuilder::new(ui)
                    .striped(false)
                    .resizable(true)
                    .auto_shrink([false, false])
                    .sense(egui::Sense::click())
                    .column(Column::initial(340.0).at_least(120.0))
                    .column(Column::remainder())
                    .header(30.0, |mut h| {
                        h.col(|ui| {
                            ui.label(RichText::new("Name").color(p.text_dim));
                        });
                        h.col(|ui| {
                            ui.label(RichText::new("Type").color(p.text_dim));
                        });
                    })
                    .body(|body| {
                        body.rows(34.0, rows.len(), |mut row| {
                            let o = rows[row.index()];
                            let selected = *sel == NavSel::Obj(conn, o.clone());
                            row.set_selected(selected);
                            row.col(|ui| {
                                ui.label(format!("{}  {}", kind_icon(o.kind), o.name));
                            });
                            row.col(|ui| {
                                ui.label(RichText::new(o.kind.label()).color(p.text_dim));
                            });
                            let resp = row.response();
                            if resp.clicked() {
                                actions.push(Action::Select(NavSel::Obj(conn, o.clone())));
                            }
                            if resp.double_clicked() {
                                actions.push(Action::OpenData { conn, obj: o.clone() });
                            }
                            super::nav::object_menu(&resp, cfg, o, actions);
                        });
                    });
            }
        }
    });
}

fn empty_hint(ui: &mut egui::Ui, text: &str) {
    let p = theme::current(ui.ctx());
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.label(RichText::new(text).color(p.text_dim));
    });
}

fn connection_summary(ui: &mut egui::Ui, cfg: &ConnectionConfig, state: &ConnState) {
    ui.heading(format!("{}  {}", theme::engine_icon(cfg.engine), cfg.name));
    egui::Grid::new("conn-summary").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
        kv(ui, "Type", cfg.engine.label());
        if cfg.engine.capabilities().network {
            kv(ui, "Host", &format!("{}:{}", cfg.host, cfg.port));
            kv(ui, "User", &cfg.user);
        } else {
            kv(ui, "File", &cfg.database);
        }
        if let ConnStatus::Open { version } = &state.status {
            kv(ui, "Server", version);
        }
        kv(ui, "Databases", &state.databases.len().to_string());
    });
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    let p = theme::current(ui.ctx());
    ui.label(RichText::new(k).color(p.text_dim));
    ui.add(egui::Label::new(v).wrap());
    ui.end_row();
}

/// Right-hand information pane for the current selection.
pub fn info_pane(
    ui: &mut egui::Ui,
    connections: &[ConnectionConfig],
    states: &HashMap<Uuid, ConnState>,
    sel: &Option<NavSel>,
    ddl: &HashMap<(Uuid, ObjectRef), Loadable<String>>,
    actions: &mut Vec<Action>,
) {
    let p = theme::current(ui.ctx());
    let Some(sel) = sel else {
        ui.label(RichText::new("Select a connection or an object to see its details.").color(p.text_faint));
        return;
    };
    let Some(cfg) = connections.iter().find(|c| c.id == sel.conn()) else { return };
    let Some(state) = states.get(&cfg.id) else { return };
    match sel {
        NavSel::Conn(_) => connection_summary(ui, cfg, state),
        NavSel::Db(_, d) => {
            egui::Grid::new("info-db").num_columns(2).show(ui, |ui| {
                kv(ui, "Database", d);
                kv(ui, "Connection", &cfg.name);
            });
        }
        NavSel::Schema(_, d, s) => {
            egui::Grid::new("info-schema").num_columns(2).show(ui, |ui| {
                kv(ui, "Schema", s);
                kv(ui, "Database", d);
            });
        }
        NavSel::Obj(conn, obj) => {
            egui::Grid::new("info-obj").num_columns(2).show(ui, |ui| {
                kv(ui, "Name", &obj.name);
                kv(ui, "Type", obj.kind.label());
                kv(ui, "Database", &obj.database);
                if let Some(s) = &obj.schema {
                    kv(ui, "Schema", s);
                }
                if let Some(st) = state.structure(obj) {
                    kv(ui, "Columns", &st.columns.len().to_string());
                    let pk = st.primary_key().join(", ");
                    kv(ui, "Primary key", if pk.is_empty() { "—" } else { &pk });
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new("DDL").strong());
            match ddl.get(&(*conn, obj.clone())) {
                None | Some(Loadable::NotLoaded) => {
                    actions.push(Action::Select(sel.clone()));
                    ui.spinner();
                }
                Some(Loadable::Loading(_)) => {
                    ui.spinner();
                }
                Some(Loadable::Failed(e)) => {
                    ui.label(RichText::new(e).color(p.danger));
                }
                Some(Loadable::Loaded(text)) => {
                    ui.horizontal(|ui| {
                        if ui.small_button(format!("{} Copy", icons::COPY)).clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                        if ui.small_button(format!("{} Open in Query", icons::FILE_SQL)).clicked() {
                            actions.push(Action::NewQuery {
                                conn: Some(*conn),
                                database: Some(obj.database.clone()),
                                text: text.clone(),
                            });
                        }
                    });
                    egui::ScrollArea::both().id_salt("ddl-scroll").auto_shrink([false, false]).show(ui, |ui| {
                        let job = super::editor::highlight(
                            ui.ctx(),
                            cfg.engine,
                            &egui::TextStyle::Monospace.resolve(ui.style()),
                            text,
                        );
                        ui.add(egui::Label::new(job).selectable(true));
                    });
                }
            }
        }
    }
}

pub struct StructureTab {
    pub conn: Uuid,
    pub obj: ObjectRef,
    pub structure: Loadable<TableStructure>,
}

impl StructureTab {
    pub fn ui(&self, ui: &mut egui::Ui) {
        let p = theme::current(ui.ctx());
        ui.heading(format!("{}  {}", kind_icon(self.obj.kind), self.obj.display_name()));
        let st = match &self.structure {
            Loadable::Loaded(s) => s,
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return;
            }
            _ => {
                ui.spinner();
                return;
            }
        };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("Fields").strong());
            simple_table(
                ui,
                "fields",
                &["", "Name", "Type", "Nullable", "Default", "Comment"],
                st.columns
                    .iter()
                    .map(|c| {
                        vec![
                            if c.primary_key { icons::KEY.to_string() } else { String::new() },
                            c.name.clone(),
                            c.data_type.clone(),
                            if c.nullable { "YES".into() } else { "NO".into() },
                            c.default.clone().unwrap_or_default(),
                            c.comment.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            );
            ui.add_space(10.0);
            ui.label(RichText::new("Indexes").strong());
            simple_table(
                ui,
                "indexes",
                &["Name", "Columns", "Unique", "Primary"],
                st.indexes
                    .iter()
                    .map(|i| vec![i.name.clone(), i.columns.join(", "), yes(i.unique), yes(i.primary)])
                    .collect(),
            );
            ui.add_space(10.0);
            ui.label(RichText::new("Foreign Keys").strong());
            simple_table(
                ui,
                "fks",
                &["Name", "Columns", "References", "On Update", "On Delete"],
                st.foreign_keys
                    .iter()
                    .map(|f| {
                        vec![
                            f.name.clone(),
                            f.columns.join(", "),
                            format!("{}({})", f.ref_table, f.ref_columns.join(", ")),
                            f.on_update.clone(),
                            f.on_delete.clone(),
                        ]
                    })
                    .collect(),
            );
        });
    }
}

fn yes(b: bool) -> String {
    if b { "YES".into() } else { String::new() }
}

fn simple_table(ui: &mut egui::Ui, id: &str, headers: &[&str], rows: Vec<Vec<String>>) {
    let p = theme::current(ui.ctx());
    if rows.is_empty() {
        ui.label(RichText::new("None").color(p.text_dim));
        return;
    }
    ui.push_id(id, |ui| {
        let mut tb = TableBuilder::new(ui).striped(true).resizable(true).vscroll(false);
        for (i, _) in headers.iter().enumerate() {
            tb = tb.column(if i == 0 && headers[0].is_empty() {
                Column::exact(22.0)
            } else {
                Column::auto().at_least(80.0).clip(true)
            });
        }
        tb.header(22.0, |mut h| {
            for t in headers {
                h.col(|ui| {
                    ui.strong(*t);
                });
            }
        })
        .body(|mut body| {
            for r in &rows {
                body.row(22.0, |mut row| {
                    for v in r {
                        row.col(|ui| {
                            ui.add(egui::Label::new(RichText::new(v).monospace()).truncate());
                        });
                    }
                });
            }
        });
    });
}

#[derive(Default)]
pub struct HistoryTab {
    pub entries: Vec<HistoryEntry>,
    pub filter: String,
    pub errors_only: bool,
    pub loaded: bool,
}

pub enum HistoryAction {
    Reload,
    Clear,
    Open(String),
}

impl HistoryTab {
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<HistoryAction> {
        let p = theme::current(ui.ctx());
        let mut action = None;
        ui.horizontal(|ui| {
            if ui.button(format!("{} Refresh", icons::ARROW_CLOCKWISE)).clicked() {
                action = Some(HistoryAction::Reload);
            }
            ui.checkbox(&mut self.errors_only, "Errors only");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{} Filter SQL", icons::MAGNIFYING_GLASS))
                    .desired_width(240.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("Clear History").color(p.danger)).clicked() {
                    action = Some(HistoryAction::Clear);
                }
            });
        });
        ui.separator();
        let f = self.filter.to_lowercase();
        let rows: Vec<&HistoryEntry> = self
            .entries
            .iter()
            .filter(|e| (!self.errors_only || !e.ok) && (f.is_empty() || e.sql.to_lowercase().contains(&f)))
            .collect();
        if rows.is_empty() {
            empty_hint(ui, "No history yet.");
            return action;
        }
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .auto_shrink([false, false])
            .sense(egui::Sense::click())
            .column(Column::initial(150.0))
            .column(Column::initial(140.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["Time", "Connection", "Duration", "SQL"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let e = rows[row.index()];
                    row.col(|ui| {
                        ui.label(RichText::new(format_ts(e.ts_ms)).monospace().color(p.text_dim));
                    });
                    row.col(|ui| {
                        ui.label(&e.connection);
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(format!("{} ms", e.elapsed_ms)).monospace());
                    });
                    row.col(|ui| {
                        let line = e.sql.lines().next().unwrap_or("");
                        let t = RichText::new(line).monospace();
                        ui.add(egui::Label::new(if e.ok { t } else { t.color(p.danger) }).truncate())
                            .on_hover_text(if e.ok { e.sql.clone() } else { format!("{}\n\n{}", e.sql, e.message) });
                    });
                    let resp = row.response();
                    if resp.double_clicked() {
                        action = Some(HistoryAction::Open(e.sql.clone()));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Open in New Query").clicked() {
                            action = Some(HistoryAction::Open(e.sql.clone()));
                            ui.close();
                        }
                        if ui.button("Copy SQL").clicked() {
                            ui.ctx().copy_text(e.sql.clone());
                            ui.close();
                        }
                    });
                });
            });
        action
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC from Unix milliseconds (no timezone database needed).
pub fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_unix_ms() {
        assert_eq!(super::format_ts(0), "1970-01-01 00:00:00");
        assert_eq!(super::format_ts(1_791_590_400_000), "2026-10-10 00:00:00");
        assert_eq!(super::format_ts(951_782_400_000 + 3_661_000), "2000-02-29 01:01:01");
    }
}
