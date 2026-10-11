//! Server monitor: live process list with cancel/terminate (PostgreSQL, MySQL).

use std::time::{Duration, Instant};

use egui::RichText;
use egui_phosphor::regular as icons;
use talus_core::model::QueryResult;
use uuid::Uuid;

use super::grid::{self, GridState, GridView, RowMark};
use crate::theme;
use crate::worker::JobId;

pub const AUTO_REFRESH: Duration = Duration::from_secs(5);

pub struct MonitorTab {
    pub conn: Uuid,
    pub conn_name: String,
    pub result: Option<QueryResult>,
    pub running: Option<JobId>,
    pub error: Option<String>,
    pub auto_refresh: bool,
    pub last_refresh: Option<Instant>,
    pub grid: GridState,
}

pub enum MonitorAction {
    Refresh,
    /// Process id; `terminate` ends the whole session instead of the current statement.
    Kill {
        id: String,
        terminate: bool,
    },
}

impl MonitorTab {
    pub fn new(conn: Uuid, conn_name: String) -> Self {
        Self {
            conn,
            conn_name,
            result: None,
            running: None,
            error: None,
            auto_refresh: false,
            last_refresh: None,
            grid: GridState::default(),
        }
    }

    fn selected_id(&self) -> Option<String> {
        let (r, _) = self.grid.selected?;
        self.result.as_ref()?.rows.get(r)?.first().cloned().flatten()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<MonitorAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        let due = self.auto_refresh
            && self.running.is_none()
            && self.last_refresh.is_none_or(|t| t.elapsed() >= AUTO_REFRESH);
        if self.result.is_none() && self.running.is_none() && self.error.is_none() || due {
            out.push(MonitorAction::Refresh);
        }
        if self.auto_refresh {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icons::PULSE, self.conn_name)).strong());
            ui.separator();
            if ui
                .add_enabled(self.running.is_none(), egui::Button::new(format!("{} Refresh", icons::ARROW_CLOCKWISE)))
                .clicked()
            {
                out.push(MonitorAction::Refresh);
            }
            ui.checkbox(&mut self.auto_refresh, format!("Auto-refresh every {}s", AUTO_REFRESH.as_secs()));
            ui.separator();
            let sel = self.selected_id();
            if ui
                .add_enabled(sel.is_some(), egui::Button::new(format!("{} Cancel Query", icons::STOP)))
                .on_hover_text("Cancel the selected process' running statement")
                .clicked()
                && let Some(id) = sel.clone()
            {
                out.push(MonitorAction::Kill { id, terminate: false });
            }
            if ui
                .add_enabled(
                    sel.is_some(),
                    egui::Button::new(RichText::new(format!("{} Kill Session", icons::X_CIRCLE)).color(p.danger)),
                )
                .on_hover_text("Disconnect the selected process")
                .clicked()
                && let Some(id) = sel
            {
                out.push(MonitorAction::Kill { id, terminate: true });
            }
            if self.running.is_some() {
                ui.spinner();
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(p.danger));
        }
        ui.separator();
        let Some(res) = &self.result else { return out };
        ui.label(RichText::new(format!("{} other session(s)", res.rows.len())).color(p.text_dim));
        let visible: Vec<usize> = (0..res.columns.len()).collect();
        let view = GridView {
            id: egui::Id::new(("monitor-grid", tab_id)),
            columns: &res.columns,
            visible: &visible,
            rows: res.rows.len(),
            first_row_number: 1,
            sort: &[],
            editable: false,
            can_filter: false,
            row_mark: None,
        };
        let rows = &res.rows;
        let cell =
            |r: usize, c: usize| (rows.get(r).and_then(|row| row.get(c)).and_then(|v| v.as_deref()), RowMark::Normal);
        let _ = grid::show(ui, &view, &mut self.grid, &cell);
        out
    }
}
