//! Column profile of one table: counts, NULL share, distinct values, extremes, top values.

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::model::ObjectRef;
use talus_core::profile::ColumnProfile;
use uuid::Uuid;

use crate::state::Loadable;
use crate::theme;

pub struct ProfileTab {
    pub conn: Uuid,
    pub obj: ObjectRef,
    pub columns: Loadable<Vec<ColumnProfile>>,
}

pub enum ProfileAction {
    Reload,
}

fn short(v: &Option<String>) -> String {
    match v {
        None => "NULL".into(),
        Some(s) if s.chars().count() > 24 => format!("{}…", s.chars().take(23).collect::<String>()),
        Some(s) => s.clone(),
    }
}

impl ProfileTab {
    pub fn new(conn: Uuid, obj: ObjectRef) -> Self {
        Self { conn, obj, columns: Loadable::NotLoaded }
    }

    pub fn title(&self) -> String {
        format!("{} (profile)", self.obj.name)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<ProfileAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        if matches!(self.columns, Loadable::NotLoaded) {
            out.push(ProfileAction::Reload);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.columns.is_loading(),
                    egui::Button::new(format!("{} Refresh", icons::ARROW_CLOCKWISE)),
                )
                .clicked()
            {
                out.push(ProfileAction::Reload);
            }
            if self.columns.is_loading() {
                ui.spinner();
                ui.label(RichText::new("Scanning the table…").color(p.text_dim));
            }
        });
        ui.separator();
        let cols = match &self.columns {
            Loadable::Loaded(c) => c,
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return out;
            }
            _ => return out,
        };
        egui::ScrollArea::both().id_salt(("profile", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("profile-grid", tab_id)).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                for h in ["Column", "Type", "Rows", "NULL %", "Distinct", "Min", "Max", "Average", "Most frequent"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for c in cols {
                    ui.label(RichText::new(&c.name).strong());
                    ui.label(RichText::new(&c.data_type).monospace().color(p.text_dim));
                    ui.label(c.rows.to_string());
                    ui.label(format!("{:.1}", c.null_percent()));
                    ui.label(c.distinct.map_or("—".into(), |d| d.to_string()));
                    ui.label(RichText::new(c.min.clone().unwrap_or_else(|| "—".into())).monospace());
                    ui.label(RichText::new(c.max.clone().unwrap_or_else(|| "—".into())).monospace());
                    ui.label(RichText::new(c.avg.clone().unwrap_or_else(|| "—".into())).monospace());
                    match &c.note {
                        Some(n) if c.top.is_empty() => {
                            ui.label(RichText::new(n).color(p.text_dim));
                        }
                        _ => {
                            let line =
                                c.top.iter().map(|(v, n)| format!("{} ({n})", short(v))).collect::<Vec<_>>().join(", ");
                            ui.label(RichText::new(line).monospace());
                        }
                    }
                    ui.end_row();
                }
            });
        });
        out
    }
}
