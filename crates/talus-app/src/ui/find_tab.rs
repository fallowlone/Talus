//! "Find in Database": search object/column names or row contents of one database/schema.

use std::collections::HashMap;

use egui::RichText;
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icons;
use talus_core::model::{ConnectionConfig, Engine, FindHit};
use uuid::Uuid;

use crate::actions::Action;
use crate::state::{ConnState, kind_icon};
use crate::theme;
use crate::worker::JobId;

#[derive(Default)]
pub struct FindTab {
    pub conn: Option<Uuid>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub needle: String,
    pub data: bool,
    pub running: Option<JobId>,
    pub hits: Option<Vec<FindHit>>,
    pub error: Option<String>,
}

pub enum FindAction {
    Run,
    Stop,
    Nav(Action),
}

impl FindTab {
    pub fn new(conn: Option<Uuid>, database: Option<String>, schema: Option<String>) -> Self {
        Self { conn, database, schema, data: true, ..Default::default() }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        tab_id: u64,
        connections: &[ConnectionConfig],
        states: &HashMap<Uuid, ConnState>,
    ) -> Vec<FindAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        let engine = self.conn.and_then(|c| connections.iter().find(|x| x.id == c)).map(|c| c.engine);
        ui.add_space(4.0);
        crate::ui::widgets::card(ui, egui::Margin::same(16), |ui| {
            egui::Grid::new(("find-form", tab_id)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Connection");
                let name = self
                    .conn
                    .and_then(|c| connections.iter().find(|x| x.id == c))
                    .map_or("Select…".into(), |c| c.name.clone());
                super::widgets::combo(("find-conn", tab_id)).selected_text(name).width(220.0).show_ui(ui, |ui| {
                    for c in connections {
                        if ui.selectable_label(self.conn == Some(c.id), &c.name).clicked() {
                            ui.close();
                            self.conn = Some(c.id);
                            self.database = None;
                            self.schema = None;
                        }
                    }
                });
                ui.end_row();
                if let Some(st) = self.conn.and_then(|c| states.get(&c)) {
                    ui.label("Database");
                    super::widgets::combo(("find-db", tab_id))
                        .selected_text(self.database.clone().unwrap_or_else(|| "Select…".into()))
                        .width(220.0)
                        .show_ui(ui, |ui| {
                            for d in &st.databases {
                                if ui.selectable_label(self.database.as_deref() == Some(&d.name), &d.name).clicked() {
                                    ui.close();
                                    self.database = Some(d.name.clone());
                                    self.schema = None;
                                }
                            }
                        });
                    ui.end_row();
                    if let Some(default_schema) = engine.and_then(Engine::default_schema) {
                        ui.label("Schema");
                        let schemas: Vec<String> = self
                            .database
                            .as_deref()
                            .and_then(|d| st.databases.iter().find(|x| x.name == d))
                            .and_then(|d| d.schemas.loaded())
                            .map(|l| l.iter().map(|s| s.name.clone()).collect())
                            .unwrap_or_default();
                        super::widgets::combo(("find-schema", tab_id))
                            .selected_text(self.schema.clone().unwrap_or_else(|| default_schema.into()))
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                for s in schemas {
                                    if ui.selectable_label(self.schema.as_deref() == Some(&s), &s).clicked() {
                                        ui.close();
                                        self.schema = Some(s);
                                    }
                                }
                            });
                        ui.end_row();
                    }
                }
                let l = ui.label("Find");
                let r = ui
                    .add(
                        crate::ui::widgets::field(&mut self.needle)
                            .desired_width(320.0)
                            .hint_text("text to search for"),
                    )
                    .labelled_by(l.id);
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && self.running.is_none() {
                    out.push(FindAction::Run);
                }
                ui.end_row();
                ui.label("Look in");
                ui.horizontal(|ui| {
                    crate::ui::widgets::radio_value(ui, &mut self.data, true, "Data");
                    crate::ui::widgets::radio_value(ui, &mut self.data, false, "Structure (table/column names)");
                });
                ui.end_row();
            });
            ui.horizontal(|ui| {
                let ready = self.conn.is_some()
                    && self.database.is_some()
                    && !self.needle.trim().is_empty()
                    && self.running.is_none();
                let find = theme::primary_button(theme::current(ui.ctx()), format!("{} Find", icons::MAGNIFYING_GLASS));
                if ui.add_enabled(ready, find).clicked() {
                    out.push(FindAction::Run);
                }
                if self.running.is_some() {
                    if ui.button(RichText::new(format!("{} Stop", icons::STOP)).color(p.danger)).clicked() {
                        out.push(FindAction::Stop);
                    }
                    ui.spinner();
                }
                if self.conn.is_some() && self.database.is_none() {
                    ui.label(RichText::new("Open the connection to choose a database.").color(p.text_dim));
                }
            });
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(p.danger));
            }
        });
        ui.add_space(8.0);
        let Some(hits) = &self.hits else { return out };
        if hits.is_empty() {
            crate::ui::widgets::empty_state(
                ui,
                icons::MAGNIFYING_GLASS,
                "No matches.",
                "Try another term or search structure instead of data.",
            );
            return out;
        }
        ui.label(RichText::new(format!("{} match(es)", hits.len())).color(p.text_dim));
        let conn = self.conn;
        TableBuilder::new(ui)
            .id_salt(("find-results", tab_id))
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .column(Column::initial(280.0))
            .column(Column::initial(200.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["Object", "Column", "Matching rows"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, hits.len(), |mut row| {
                    let hit = &hits[row.index()];
                    row.col(|ui| {
                        ui.label(format!("{}  {}", kind_icon(hit.obj.kind), hit.obj.display_name()));
                    });
                    row.col(|ui| {
                        ui.label(hit.column.clone().unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.label(hit.rows.map(|n| n.to_string()).unwrap_or_default());
                    });
                    if row.response().double_clicked()
                        && let Some(c) = conn
                    {
                        out.push(FindAction::Nav(Action::OpenData { conn: c, obj: hit.obj.clone() }));
                    }
                });
            });
        out
    }
}
