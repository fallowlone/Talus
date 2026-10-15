//! Structure synchronization: compare a source and a target schema, preview, apply.

use std::collections::HashMap;

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::data_sync::DataSyncReport;
use talus_core::model::{ConnectionConfig, Engine, ObjectRef, TableStructure};
use talus_core::sql::sync::{SyncPlan, structure_sync};
use uuid::Uuid;

use crate::state::{ConnState, Loadable};
use crate::theme;
use crate::worker::JobId;

pub type Model = Vec<(ObjectRef, TableStructure)>;

#[derive(Default)]
pub struct Side {
    pub conn: Option<Uuid>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub model: Loadable<Model>,
    /// Views with their definitions (loaded next to the model).
    pub views: Loadable<Vec<(ObjectRef, String)>>,
}

impl Side {
    pub fn ready(&self) -> bool {
        self.conn.is_some() && self.database.is_some()
    }
}

/// A table present on both sides with the same primary key: eligible for data sync.
pub struct DataTable {
    pub name: String,
    pub src: ObjectRef,
    pub dst: ObjectRef,
    pub selected: bool,
}

#[derive(Default)]
pub struct SyncTab {
    pub source: Side,
    pub target: Side,
    pub include_drops: bool,
    pub plan: Option<Result<SyncPlan, String>>,
    pub running: Option<JobId>,
    pub info: Option<String>,
    pub error: Option<String>,
    pub data_tables: Vec<DataTable>,
    pub delete_missing: bool,
    /// Tables still to process in the current data run (processed from the back).
    pub data_queue: Vec<String>,
    pub data_apply: bool,
    pub data_results: Vec<(String, Result<DataSyncReport, String>)>,
}

pub enum SyncAction {
    Compare,
    Execute(SyncPlan),
    /// Dry-run / apply data sync for these tables.
    Data {
        tables: Vec<String>,
        apply: bool,
    },
}

/// Connection / database / schema pickers for one side.
fn scope_picker(
    ui: &mut egui::Ui,
    id: &str,
    side: &mut Side,
    connections: &[ConnectionConfig],
    states: &HashMap<Uuid, ConnState>,
) -> bool {
    let before = (side.conn, side.database.clone(), side.schema.clone());
    let engine = side.conn.and_then(|c| connections.iter().find(|x| x.id == c)).map(|c| c.engine);
    egui::Grid::new((id, "grid")).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Connection");
        let name = side
            .conn
            .and_then(|c| connections.iter().find(|x| x.id == c))
            .map_or("— choose —".into(), |c| c.name.clone());
        super::widgets::combo((id, "conn")).selected_text(name).width(200.0).show_ui(ui, |ui| {
            for c in connections {
                if ui.selectable_label(side.conn == Some(c.id), &c.name).clicked() {
                    ui.close();
                    side.conn = Some(c.id);
                    side.database = None;
                    side.schema = None;
                }
            }
        });
        ui.end_row();
        let st = side.conn.and_then(|c| states.get(&c));
        ui.label("Database");
        super::widgets::combo((id, "db"))
            .selected_text(side.database.clone().unwrap_or_else(|| "— choose —".into()))
            .width(200.0)
            .show_ui(ui, |ui| {
                for d in st.map(|s| s.databases.as_slice()).unwrap_or_default() {
                    if ui.selectable_label(side.database.as_deref() == Some(&d.name), &d.name).clicked() {
                        ui.close();
                        side.database = Some(d.name.clone());
                        side.schema = None;
                    }
                }
            });
        ui.end_row();
        if let Some(default_schema) = engine.and_then(Engine::default_schema) {
            ui.label("Schema");
            let schemas: Vec<String> = side
                .database
                .as_deref()
                .and_then(|d| st?.databases.iter().find(|x| x.name == d))
                .and_then(|d| d.schemas.loaded())
                .map(|l| l.iter().map(|s| s.name.clone()).collect())
                .unwrap_or_default();
            super::widgets::combo((id, "schema"))
                .selected_text(side.schema.clone().unwrap_or_else(|| default_schema.into()))
                .width(200.0)
                .show_ui(ui, |ui| {
                    for s in schemas {
                        if ui.selectable_label(side.schema.as_deref() == Some(&s), &s).clicked() {
                            ui.close();
                            side.schema = Some(s);
                        }
                    }
                });
            ui.end_row();
        }
    });
    if st_closed(side, states) {
        ui.label(
            RichText::new("Open this connection in the navigator to list its databases.")
                .small()
                .color(theme::current(ui.ctx()).text_dim),
        );
    }
    before != (side.conn, side.database.clone(), side.schema.clone())
}

fn st_closed(side: &Side, states: &HashMap<Uuid, ConnState>) -> bool {
    side.conn.is_some_and(|c| !states.get(&c).is_some_and(|s| s.status.is_open()))
}

impl SyncTab {
    pub fn new(conn: Option<Uuid>, database: Option<String>, schema: Option<String>) -> Self {
        Self { source: Side { conn, database, schema, ..Default::default() }, ..Default::default() }
    }

    /// Recomputes the plan once both models are loaded.
    pub fn recompute(&mut self, engine_of: impl Fn(Uuid) -> Option<Engine>) {
        let (Some(s), Some(t)) = (self.source.model.loaded(), self.target.model.loaded()) else { return };
        let (Some(se), Some(te)) = (self.source.conn.and_then(&engine_of), self.target.conn.and_then(&engine_of))
        else {
            return;
        };
        let previous: Vec<(String, bool)> = self.data_tables.iter().map(|d| (d.name.clone(), d.selected)).collect();
        self.data_tables = s
            .iter()
            .filter_map(|(so, sst)| {
                let (to, tst) = t.iter().find(|(to, _)| to.name == so.name)?;
                let key = sst.primary_key();
                (!key.is_empty() && key == tst.primary_key()).then(|| DataTable {
                    name: so.name.clone(),
                    src: so.clone(),
                    dst: to.clone(),
                    selected: previous.iter().find(|(n, _)| *n == so.name).is_some_and(|(_, sel)| *sel),
                })
            })
            .collect();
        self.plan = Some(if se != te {
            // Different engines: structure diffing does not translate, but missing
            // tables can be created with mapped types and then filled by data sync.
            let db = self.target.database.clone().unwrap_or_default();
            let schema = te.default_schema().map(|d| self.target.schema.clone().unwrap_or_else(|| d.into()));
            Ok(talus_core::transfer::plan(s, t, te, &db, schema.as_deref()))
        } else {
            let db = self.target.database.clone().unwrap_or_default();
            let schema = te.default_schema().map(|d| self.target.schema.clone().unwrap_or_else(|| d.into()));
            let tables =
                structure_sync(te, s, t, &db, schema.as_deref(), self.include_drops).map_err(|e| e.to_string());
            match (tables, self.source.views.loaded(), self.target.views.loaded()) {
                (Ok(mut plan), Some(sv), Some(tv)) => {
                    // Definitions embed their namespace; view_sync rewrites the
                    // source's qualifier to the target's when they differ.
                    let namespaces = if te.default_schema().is_some() {
                        match (
                            self.source.schema.clone().or_else(|| se.default_schema().map(String::from)),
                            schema.clone(),
                        ) {
                            (Some(a), Some(b)) => Some((a, b)),
                            _ => None,
                        }
                    } else {
                        match (self.source.database.clone(), self.target.database.clone()) {
                            (Some(a), Some(b)) => Some((a, b)),
                            _ => None,
                        }
                    };
                    let namespaces = namespaces.as_ref().map(|(a, b)| (a.as_str(), b.as_str()));
                    match talus_core::sql::sync::view_sync(te, sv, tv, self.include_drops, namespaces) {
                        Ok(v) => {
                            plan.statements.extend(v.statements);
                            plan.destructive.extend(v.destructive);
                            plan.summary.extend(v.summary);
                            if plan.statements.is_empty() && plan.summary.len() == 1 {
                                // Only the "identical" line: views agree as well.
                            }
                            Ok(plan)
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
                (other, _, _) => other,
            }
        });
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        connections: &[ConnectionConfig],
        states: &HashMap<Uuid, ConnState>,
    ) -> Vec<SyncAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        let mut changed = false;
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Source").strong());
            changed |= scope_picker(&mut cols[0], "sync-src", &mut self.source, connections, states);
            cols[1].label(RichText::new("Target").strong());
            changed |= scope_picker(&mut cols[1], "sync-dst", &mut self.target, connections, states);
        });
        if changed {
            self.plan = None;
            self.source.model = Loadable::NotLoaded;
            self.target.model = Loadable::NotLoaded;
            self.source.views = Loadable::NotLoaded;
            self.target.views = Loadable::NotLoaded;
        }
        ui.horizontal(|ui| {
            let busy = self.running.is_some() || self.source.model.is_loading() || self.target.model.is_loading();
            if ui
                .add_enabled(
                    self.source.ready() && self.target.ready() && !busy,
                    egui::Button::new(format!("{} Compare", icons::ARROWS_LEFT_RIGHT)),
                )
                .clicked()
            {
                out.push(SyncAction::Compare);
            }
            if ui.checkbox(&mut self.include_drops, "Include drops (tables/columns missing in source)").changed()
                && self.plan.is_some()
            {
                out.push(SyncAction::Compare);
            }
            let runnable = matches!(&self.plan, Some(Ok(pl)) if !pl.statements.is_empty()) && !busy;
            if ui
                .add_enabled(
                    runnable,
                    // The screen's primary action once there is something to run.
                    if runnable {
                        theme::primary_button(p, format!("{} Execute on Target", icons::PLAY))
                    } else {
                        egui::Button::new(format!("{} Execute on Target", icons::PLAY))
                    },
                )
                .clicked()
                && let Some(Ok(pl)) = &self.plan
            {
                out.push(SyncAction::Execute(pl.clone()));
            }
            if busy {
                ui.spinner();
            }
            if let Some(i) = &self.info {
                ui.label(RichText::new(i).color(p.ok));
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(p.danger));
        }
        for side in [&self.source, &self.target] {
            if let Loadable::Failed(e) = &side.model {
                ui.label(RichText::new(e).color(p.danger));
            }
        }
        ui.separator();
        if matches!(self.plan, Some(Ok(_))) {
            self.data_section(ui, &mut out);
        }
        match &self.plan {
            None => {
                ui.label(RichText::new("Choose both sides and press Compare.").color(p.text_dim));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(p.danger));
            }
            Some(Ok(plan)) => {
                egui::ScrollArea::vertical().id_salt("sync-plan").auto_shrink([false, false]).show(ui, |ui| {
                    for line in &plan.summary {
                        let color = if line.starts_with('!') {
                            p.warn
                        } else if line.starts_with('-') {
                            p.danger
                        } else {
                            p.text
                        };
                        ui.label(RichText::new(line).monospace().color(color));
                    }
                    for d in &plan.destructive {
                        ui.label(RichText::new(format!("{} {d}", icons::WARNING)).color(p.warn));
                    }
                    if !plan.statements.is_empty() {
                        ui.add_space(6.0);
                        ui.label(RichText::new("SQL to run on the target").strong());
                        let text = plan.statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n");
                        let engine = self
                            .target
                            .conn
                            .and_then(|c| connections.iter().find(|x| x.id == c))
                            .map_or(Engine::Sqlite, |c| c.engine);
                        let job = super::editor::highlight(
                            ui.ctx(),
                            engine,
                            &egui::TextStyle::Monospace.resolve(ui.style()),
                            &text,
                        );
                        ui.add(egui::Label::new(job).selectable(true));
                    }
                });
            }
        }
        out
    }

    fn data_section(&mut self, ui: &mut egui::Ui, out: &mut Vec<SyncAction>) {
        let p = theme::current(ui.ctx());
        egui::CollapsingHeader::new(RichText::new("Data synchronization").strong())
            .id_salt("sync-data")
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("sync-data-scroll").max_height(260.0).show(ui, |ui| {
                    if self.data_tables.is_empty() {
                        ui.label(
                            RichText::new("No table with the same primary key exists on both sides.").color(p.text_dim),
                        );
                        return;
                    }
                    let busy = self.running.is_some();
                    ui.horizontal(|ui| {
                        if ui.small_button("All").clicked() {
                            self.data_tables.iter_mut().for_each(|d| d.selected = true);
                        }
                        if ui.small_button("None").clicked() {
                            self.data_tables.iter_mut().for_each(|d| d.selected = false);
                        }
                        ui.checkbox(&mut self.delete_missing, "Delete rows missing in source");
                    });
                    for d in &mut self.data_tables {
                        ui.checkbox(&mut d.selected, &d.name);
                    }
                    let tables: Vec<String> =
                        self.data_tables.iter().filter(|d| d.selected).map(|d| d.name.clone()).collect();
                    ui.horizontal(|ui| {
                        let ready = !tables.is_empty() && !busy;
                        if ui.add_enabled(ready, egui::Button::new("Compare Data")).clicked() {
                            out.push(SyncAction::Data { tables: tables.clone(), apply: false });
                        }
                        let apply = ui.add_enabled(
                            ready,
                            if ready {
                                theme::primary_button(p, "Sync Data to Target")
                            } else {
                                egui::Button::new("Sync Data to Target")
                            },
                        );
                        if apply.clicked() {
                            out.push(SyncAction::Data { tables, apply: true });
                        }
                    });
                    for (name, r) in &self.data_results {
                        match r {
                            Ok(rep) => {
                                let verb = if rep.applied { "applied" } else { "differs" };
                                ui.label(
                                    RichText::new(format!(
                                        "{name}: {verb} +{} ~{} -{} ={}",
                                        rep.inserted, rep.updated, rep.deleted, rep.unchanged
                                    ))
                                    .monospace(),
                                );
                                for line in &rep.preview {
                                    ui.label(RichText::new(format!("    {line}")).monospace().color(p.text_dim));
                                }
                            }
                            Err(e) => {
                                ui.label(RichText::new(format!("{name}: {e}")).color(p.danger));
                            }
                        }
                    }
                });
            });
        ui.add_space(6.0);
    }
}
