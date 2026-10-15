//! Read-only browser of stored routines and triggers of one database/schema.

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::model::Engine;
use talus_core::programmability::{ProgItem, ProgKind, template, view_template};
use uuid::Uuid;

use crate::state::Loadable;
use crate::theme;

pub struct ProgTab {
    pub conn: Uuid,
    pub engine: Engine,
    pub database: String,
    pub schema: Option<String>,
    pub items: Loadable<Vec<ProgItem>>,
    selected: Option<usize>,
}

pub enum ProgAction {
    Reload,
    /// Open this definition in a new query tab.
    OpenInQuery(String),
    Copy(String),
    /// Drop this object (the app confirms with the exact statement first).
    Drop(ProgItem),
    /// Open the PL/pgSQL debugger on routine `oid` (`name` for the tab title).
    Debug {
        oid: u32,
        name: String,
    },
}

fn kind_icon(k: ProgKind) -> &'static str {
    match k {
        ProgKind::Function => icons::FUNCTION,
        ProgKind::Procedure => icons::GEAR_SIX,
        ProgKind::Trigger => icons::LIGHTNING,
        ProgKind::Package => icons::PACKAGE,
    }
}

impl ProgTab {
    pub fn new(conn: Uuid, engine: Engine, database: String, schema: Option<String>) -> Self {
        Self { conn, engine, database, schema, items: Loadable::NotLoaded, selected: None }
    }

    pub fn title(&self) -> String {
        format!("Routines — {}", self.schema.as_deref().unwrap_or(&self.database))
    }

    pub fn set_items(&mut self, items: Vec<ProgItem>) {
        self.selected = (!items.is_empty()).then_some(0);
        self.items = Loadable::Loaded(items);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<ProgAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        if matches!(self.items, Loadable::NotLoaded) {
            out.push(ProgAction::Reload);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.items.is_loading(), egui::Button::new(format!("{} Refresh", icons::ARROW_CLOCKWISE)))
                .clicked()
            {
                out.push(ProgAction::Reload);
            }
            if self.items.is_loading() {
                ui.spinner();
            }
            // Creation skeletons open in the query editor, like an edit does.
            let kinds: &[ProgKind] = match self.engine {
                Engine::Sqlite => &[ProgKind::Trigger],
                Engine::Snowflake => &[ProgKind::Function, ProgKind::Procedure],
                Engine::Redis | Engine::MongoDb => &[],
                _ => &[ProgKind::Function, ProgKind::Procedure, ProgKind::Trigger],
            };
            for k in kinds {
                if ui.button(format!("New {}", k.label())).clicked() {
                    out.push(ProgAction::OpenInQuery(template(
                        self.engine,
                        *k,
                        &self.database,
                        self.schema.as_deref(),
                    )));
                }
            }
            if self.engine.is_sql() && ui.button("New View").clicked() {
                out.push(ProgAction::OpenInQuery(view_template(self.engine, &self.database, self.schema.as_deref())));
            }
        });
        ui.separator();
        let items = match &self.items {
            Loadable::Loaded(v) => v,
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return out;
            }
            _ => return out,
        };
        if items.is_empty() {
            ui.label(RichText::new("No routines or triggers.").color(p.text_dim));
            return out;
        }
        egui::Panel::left(egui::Id::new(("prog-list", tab_id))).resizable(true).default_size(280.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (i, it) in items.iter().enumerate() {
                    let label = match &it.table {
                        Some(t) => format!("{} {}  ({t})", kind_icon(it.kind), it.name),
                        None => format!("{} {}", kind_icon(it.kind), it.name),
                    };
                    if ui.selectable_label(self.selected == Some(i), label).on_hover_text(it.kind.label()).clicked() {
                        self.selected = Some(i);
                    }
                }
            });
        });
        let Some(it) = self.selected.and_then(|i| items.get(i)) else { return out };
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  {}", it.kind.label(), it.name)).strong());
            if ui.button(format!("{} Copy", icons::COPY)).clicked() {
                out.push(ProgAction::Copy(it.definition.clone()));
            }
            if ui.button(format!("{} Open in Query", icons::FILE_SQL)).clicked() {
                out.push(ProgAction::OpenInQuery(it.definition.clone()));
            }
            if let Some(oid) = it.debug_id
                && ui
                    .button(format!("{} Debug", icons::BUG))
                    .on_hover_text(if self.engine == Engine::Oracle { "PL/SQL debugger" } else { "PL/pgSQL debugger" })
                    .clicked()
            {
                out.push(ProgAction::Debug { oid, name: it.name.clone() });
            }
            if ui.button("Delete").clicked() {
                out.push(ProgAction::Drop(it.clone()));
            }
        });
        egui::ScrollArea::both().id_salt(("prog-def", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            if it.definition.is_empty() {
                ui.label(RichText::new("The server did not return a definition (check privileges).").color(p.text_dim));
            } else {
                let job = super::editor::highlight(
                    ui.ctx(),
                    self.engine,
                    &egui::TextStyle::Monospace.resolve(ui.style()),
                    &it.definition,
                );
                ui.add(egui::Label::new(job).selectable(true));
            }
        });
        out
    }
}
