//! Query editor tab: target connection/database, editor, run/stop, results and messages.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{RichText, Sense};
use talus_core::DbError;
use talus_core::engine::ScriptOutcome;
use talus_core::model::{ConnectionConfig, Engine, Outcome, QueryResult};
use talus_core::sql::split;
use uuid::Uuid;

use super::completion::MetaView;
use super::editor;
use super::grid::{self, GridState, GridView, RowMark};
use crate::state::ConnState;
use crate::theme;
use crate::worker::JobId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Selection if any, else the whole script.
    All,
    Current,
    /// Execution plan of the selection or the statement at the cursor.
    Explain,
}

/// Engine-specific EXPLAIN prefix.
pub fn explain_prefix(engine: Engine) -> &'static str {
    match engine {
        Engine::Sqlite => "EXPLAIN QUERY PLAN ",
        Engine::Postgres | Engine::MySql => "EXPLAIN ",
        Engine::SqlServer => "SET SHOWPLAN_TEXT ON; ",
        Engine::Redis | Engine::MongoDb => "",
    }
}

/// The text submitted for Explain. SQL Server's plan mode is a session switch that must be turned
/// off again, so the script carries both ends (on one line, which keeps error lines aligned).
pub fn explain_script(engine: Engine, sql: &str) -> String {
    match engine {
        Engine::SqlServer => format!("{}{sql}; SET SHOWPLAN_TEXT OFF", explain_prefix(engine)),
        _ => format!("{}{sql}", explain_prefix(engine)),
    }
}

pub struct ResultPane {
    pub title: String,
    pub sql: String,
    pub result: QueryResult,
    pub elapsed: Duration,
    pub grid: GridState,
    pub chart: super::chart_view::ChartState,
}

pub struct MessageLine {
    pub ok: bool,
    pub text: String,
}

pub struct SqlTab {
    pub conn: Option<Uuid>,
    pub database: Option<String>,
    pub text: String,
    pub file: Option<PathBuf>,
    pub dirty: bool,
    pub number: u64,
    pub running: Option<JobId>,
    pub started: Option<Instant>,
    pub results: Vec<ResultPane>,
    pub messages: Vec<MessageLine>,
    /// 0 = Messages, n = results[n-1].
    pub active: usize,
    pub continue_on_error: bool,
    /// A manual transaction is open on this tab's session.
    pub tx_open: bool,
    /// Value `tx_open` takes if the submitted Begin/Commit/Rollback succeeds.
    pub tx_next: Option<bool>,
    pub error_line: Option<usize>,
    pub selection: Option<Range<usize>>,
    pub cursor: Option<usize>,
    /// Line in `text` where the last submitted script started (maps error lines back).
    pub submitted_line: usize,
    pub editor: editor::EditorState,
}

#[derive(Clone, Copy)]
pub enum TxAction {
    Begin,
    Commit,
    Rollback,
}

pub enum SqlAction {
    Run(RunMode),
    Tx(TxAction),
    Stop,
    Save,
    /// The target connection changed; the tab's sessions must be dropped.
    TargetChanged,
    /// Completion needs column metadata for these tables.
    NeedColumns(Vec<String>),
}

pub struct SqlCtx<'a> {
    pub connections: &'a [ConnectionConfig],
    pub states: &'a HashMap<Uuid, ConnState>,
    pub meta: &'a MetaView,
    pub completion: bool,
}

impl SqlTab {
    pub fn new(number: u64, conn: Option<Uuid>, database: Option<String>) -> Self {
        Self {
            conn,
            database,
            text: String::new(),
            file: None,
            dirty: false,
            number,
            running: None,
            started: None,
            results: Vec::new(),
            messages: Vec::new(),
            active: 0,
            continue_on_error: false,
            tx_open: false,
            tx_next: None,
            error_line: None,
            selection: None,
            cursor: None,
            submitted_line: 0,
            editor: editor::EditorState::default(),
        }
    }

    pub fn editor_id(tab_id: u64) -> egui::Id {
        egui::Id::new(("editor", tab_id))
    }

    /// Beautifies the selection, or the whole script when nothing is selected.
    pub fn format(&mut self, ctx: &egui::Context, tab_id: u64, engine: Engine) {
        let range = self.selection.clone().filter(|r| r.start < r.end).unwrap_or(0..self.text.len());
        let formatted = talus_core::sql::format::format_sql(&self.text[range.clone()], engine);
        let formatted = if range.end < self.text.len() { formatted.trim_end().to_string() } else { formatted };
        self.text.replace_range(range.clone(), &formatted);
        self.dirty = true;
        editor::set_selection(ctx, Self::editor_id(tab_id), &self.text, range.start..range.start + formatted.len());
    }

    /// ⌘/ — toggles `--` comments on the selected lines (or the cursor line).
    pub fn toggle_comment(&mut self, ctx: &egui::Context, tab_id: u64) {
        let range = self.selection.clone().unwrap_or_else(|| {
            let c = self.cursor.unwrap_or(0).min(self.text.len());
            c..c
        });
        let (text, new_range) = talus_core::sql::format::toggle_comment(&self.text, range);
        self.text = text;
        self.dirty = true;
        self.selection = Some(new_range.clone());
        editor::set_selection(ctx, Self::editor_id(tab_id), &self.text, new_range);
    }

    pub fn title(&self) -> String {
        let name = self
            .file
            .as_ref()
            .and_then(|f| f.file_name())
            .map_or_else(|| format!("Query {}", self.number), |n| n.to_string_lossy().into_owned());
        if self.dirty { format!("{name} •") } else { name }
    }

    /// The script to submit for `mode`, and the 0-based line where it starts in `text`.
    pub fn script_for(&self, mode: RunMode, engine: Engine) -> Option<(String, usize)> {
        let line_of = |byte: usize| self.text[..byte.min(self.text.len())].bytes().filter(|&b| b == b'\n').count();
        match mode {
            RunMode::All => match &self.selection {
                Some(r) if !self.text.get(r.clone()).unwrap_or("").trim().is_empty() => {
                    Some((self.text[r.clone()].to_string(), line_of(r.start)))
                }
                _ if self.text.trim().is_empty() => None,
                _ => Some((self.text.clone(), 0)),
            },
            RunMode::Current => {
                let cur = self.cursor.unwrap_or(0).min(self.text.len());
                split::statement_at(&self.text, engine, cur).map(|s| (s.text.to_string(), s.line))
            }
            RunMode::Explain => {
                let (sql, line) = match &self.selection {
                    Some(r) if !self.text.get(r.clone()).unwrap_or("").trim().is_empty() => {
                        let first = split::split(&self.text[r.clone()], engine).into_iter().next()?;
                        (first.text.to_string(), line_of(r.start) + first.line)
                    }
                    _ => self.script_for(RunMode::Current, engine)?,
                };
                Some((explain_script(engine, &sql), line))
            }
        }
    }

    pub fn begin_run(&mut self, job: JobId, line: usize) {
        self.running = Some(job);
        self.started = Some(Instant::now());
        self.submitted_line = line;
        self.error_line = None;
    }

    pub fn finish(&mut self, outcome: Result<ScriptOutcome, DbError>) {
        self.running = None;
        let next = self.tx_next.take();
        match (&outcome, next) {
            (Ok(o), _) if o.connection_lost || o.cancelled => self.tx_open = false, // a killed session takes its transaction along
            (Ok(o), Some(open)) if o.errors.is_empty() && !o.cancelled => self.tx_open = open,
            (Err(_), _) => self.tx_open = false,
            _ => {}
        }
        let total = self.started.take().map(|s| s.elapsed()).unwrap_or_default();
        self.results.clear();
        self.messages.clear();
        match outcome {
            Err(e) => {
                self.messages.push(MessageLine { ok: false, text: format!("[ERROR] {e}") });
            }
            Ok(o) => {
                for r in &o.results {
                    let first_line = r.sql.lines().next().unwrap_or("");
                    let ms = r.elapsed.as_secs_f64() * 1000.0;
                    match &r.outcome {
                        Outcome::Rows(q) => {
                            let more = if q.truncated { " (truncated — row limit reached)" } else { "" };
                            self.messages.push(MessageLine {
                                ok: true,
                                text: format!("[OK] {} rows in {ms:.1} ms{more}  —  {first_line}", q.rows.len()),
                            });
                            self.results.push(ResultPane {
                                title: format!("Result {}", self.results.len() + 1),
                                sql: r.sql.clone(),
                                result: q.clone(),
                                elapsed: r.elapsed,
                                grid: GridState::default(),
                                chart: Default::default(),
                            });
                        }
                        Outcome::Affected(n) => {
                            self.messages.push(MessageLine {
                                ok: true,
                                text: format!("[OK] {n} rows affected in {ms:.1} ms  —  {first_line}"),
                            });
                        }
                    }
                }
                for e in &o.errors {
                    let line = self.submitted_line + e.line;
                    self.error_line.get_or_insert(line);
                    let pos = match &e.error {
                        DbError::Server { position: Some(p), .. } => format!(", position {p}"),
                        _ => String::new(),
                    };
                    self.messages.push(MessageLine {
                        ok: false,
                        text: format!(
                            "[ERROR] line {}{pos}: {}  —  {}",
                            line + 1,
                            e.error,
                            e.sql.lines().next().unwrap_or("")
                        ),
                    });
                }
                if o.cancelled {
                    self.messages.push(MessageLine { ok: false, text: "Execution was cancelled.".into() });
                }
                if o.connection_lost {
                    self.messages.push(MessageLine {
                        ok: false,
                        text: "Connection lost. The next run reconnects; the failed statement was not retried.".into(),
                    });
                }
                self.messages.push(MessageLine {
                    ok: o.errors.is_empty(),
                    text: format!("Finished in {:.1} ms", total.as_secs_f64() * 1000.0),
                });
            }
        }
        self.active = if self.messages.iter().any(|m| !m.ok) || self.results.is_empty() { 0 } else { 1 };
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64, ctx: &SqlCtx<'_>) -> Vec<SqlAction> {
        let mut actions = Vec::new();
        let p = theme::current(ui.ctx());
        let engine =
            self.conn.and_then(|c| ctx.connections.iter().find(|x| x.id == c)).map_or(Engine::Sqlite, |c| c.engine);

        ui.horizontal_wrapped(|ui| {
            let conn_name = self
                .conn
                .and_then(|c| ctx.connections.iter().find(|x| x.id == c))
                .map_or("— choose connection —".to_string(), |c| c.name.clone());
            let before = (self.conn, self.database.clone());
            super::widgets::combo(("conn", tab_id)).selected_text(conn_name).width(170.0).show_ui(ui, |ui| {
                for c in ctx.connections {
                    let label = format!("{}  {}", theme::engine_icon(c.engine), c.name);
                    if ui.selectable_label(self.conn == Some(c.id), label).clicked() {
                        ui.close();
                        self.conn = Some(c.id);
                        self.database = None;
                    }
                }
            });
            if let Some(cid) = self.conn {
                let dbs: Vec<String> = ctx
                    .states
                    .get(&cid)
                    .map(|s| s.databases.iter().map(|d| d.name.clone()).collect())
                    .unwrap_or_default();
                let shown = self.database.clone().unwrap_or_else(|| "default".into());
                super::widgets::combo(("db", tab_id)).selected_text(shown).width(150.0).show_ui(ui, |ui| {
                    if ui.selectable_label(self.database.is_none(), "default").clicked() {
                        ui.close();
                        self.database = None;
                    }
                    for d in dbs {
                        if ui.selectable_label(self.database.as_deref() == Some(&d), &d).clicked() {
                            ui.close();
                            self.database = Some(d);
                        }
                    }
                });
            }
            if before != (self.conn, self.database.clone()) {
                actions.push(SqlAction::TargetChanged);
            }
            ui.separator();
            let running = self.running.is_some();
            let can_run = self.conn.is_some() && !running;
            if ui
                // The screen's one primary action (filled black), as in the reference layout.
                .add_enabled(
                    can_run,
                    theme::primary_button(theme::current(ui.ctx()), format!("{} Run", egui_phosphor::regular::PLAY)),
                )
                .on_hover_text("Run selection or whole script (⌘R)")
                .clicked()
            {
                actions.push(SqlAction::Run(RunMode::All));
            }
            if ui
                .add_enabled(can_run, egui::Button::new(format!("{} Run Current", egui_phosphor::regular::PLAY_CIRCLE)))
                .on_hover_text("Run the statement at the cursor (⇧⌘R)")
                .clicked()
            {
                actions.push(SqlAction::Run(RunMode::Current));
            }
            if ui
                .add_enabled(can_run, egui::Button::new(format!("{} Explain", egui_phosphor::regular::TREE_STRUCTURE)))
                .on_hover_text("Show the execution plan of the selection or current statement (⌥⌘E)")
                .clicked()
            {
                actions.push(SqlAction::Run(RunMode::Explain));
            }
            if ui
                .add_enabled(
                    running,
                    egui::Button::new(
                        RichText::new(format!("{} Stop", egui_phosphor::regular::STOP)).color(if running {
                            p.danger
                        } else {
                            p.text_dim
                        }),
                    ),
                )
                .on_hover_text("Stop (⌘.)")
                .clicked()
            {
                actions.push(SqlAction::Stop);
            }
            ui.checkbox(&mut self.continue_on_error, "Continue on error");
            ui.separator();
            let idle = self.conn.is_some() && !running;
            for (label, tip, act, on) in [
                ("Begin", "Start a transaction on this tab's session", TxAction::Begin, !self.tx_open),
                ("Commit", "Commit the open transaction", TxAction::Commit, self.tx_open),
                ("Rollback", "Roll back the open transaction", TxAction::Rollback, self.tx_open),
            ] {
                if ui.add_enabled(idle && on, egui::Button::new(label)).on_hover_text(tip).clicked() {
                    actions.push(SqlAction::Tx(act));
                }
            }
            if self.tx_open {
                ui.label(RichText::new("● transaction open").color(p.warn));
            }
            ui.separator();
            if ui
                .button(format!("{} Format", egui_phosphor::regular::TEXT_ALIGN_LEFT))
                .on_hover_text("Beautify the selection or the whole script (⌥⌘L)")
                .clicked()
            {
                self.format(ui.ctx(), tab_id, engine);
            }
            if ui
                .button(format!("{} Save", egui_phosphor::regular::FLOPPY_DISK))
                .on_hover_text("Save to .sql file (⌘S)")
                .clicked()
            {
                actions.push(SqlAction::Save);
            }
            if let Some(s) = self.started {
                ui.spinner();
                ui.label(RichText::new(format!("Running… {:.1}s", s.elapsed().as_secs_f32())).color(p.warn));
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        });

        egui::Panel::bottom(egui::Id::new(("results", tab_id)))
            .resizable(true)
            .default_size(ui.available_height() * 0.45)
            .min_size(80.0)
            .show(ui, |ui| {
                self.results_ui(ui, tab_id);
            });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg).inner_margin(4.0)).show(ui, |ui| {
            let env =
                editor::EditorEnv { engine, error_line: self.error_line, meta: ctx.meta, completion: ctx.completion };
            let out = editor::sql_editor(ui, Self::editor_id(tab_id), &mut self.text, &mut self.editor, &env);
            if out.changed {
                self.dirty = true;
                self.error_line = None;
            }
            if !out.need_columns.is_empty() {
                actions.push(SqlAction::NeedColumns(out.need_columns));
            }
            if out.cursor.is_some() {
                self.cursor = out.cursor;
                self.selection = out.selection;
            }
        });
        actions
    }

    fn results_ui(&mut self, ui: &mut egui::Ui, tab_id: u64) {
        let p = theme::current(ui.ctx());
        ui.horizontal(|ui| {
            let msg_label = if self.messages.iter().any(|m| !m.ok) {
                RichText::new("Messages").color(p.danger)
            } else {
                RichText::new("Messages")
            };
            if ui.selectable_label(self.active == 0, msg_label).clicked() {
                self.active = 0;
            }
            for (i, r) in self.results.iter().enumerate() {
                if ui.selectable_label(self.active == i + 1, &r.title).on_hover_text(&r.sql).clicked() {
                    self.active = i + 1;
                }
            }
        });
        ui.separator();
        if self.active == 0 || self.active > self.results.len() {
            egui::ScrollArea::vertical().id_salt(("msgs", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.label(RichText::new("Run a query to see results here.").color(p.text_dim));
                }
                for m in &self.messages {
                    let color = if m.ok { p.text } else { p.danger };
                    ui.add(
                        egui::Label::new(RichText::new(&m.text).monospace().color(color)).sense(Sense::click()).wrap(),
                    )
                    .context_menu(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(m.text.clone());
                            ui.close();
                        }
                    });
                }
            });
            return;
        }
        let pane = &mut self.results[self.active - 1];
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} rows · {:.1} ms",
                    pane.result.rows.len(),
                    pane.elapsed.as_secs_f64() * 1000.0
                ))
                .color(p.text_dim),
            );
            if pane.result.truncated {
                ui.label(RichText::new("truncated at row limit (Settings → Max result rows)").color(p.warn));
            }
            let label = format!("{} Chart", egui_phosphor::regular::CHART_BAR);
            if ui.selectable_label(pane.chart.open, label).on_hover_text("Plot this result").clicked() {
                pane.chart.open = !pane.chart.open;
            }
        });
        if pane.chart.open {
            pane.chart.ui(ui, egui::Id::new(("result-chart", tab_id, self.active)), &pane.result);
            return;
        }
        let visible: Vec<usize> = (0..pane.result.columns.len()).collect();
        let view = GridView {
            id: egui::Id::new(("result-grid", tab_id, self.active)),
            columns: &pane.result.columns,
            visible: &visible,
            rows: pane.result.rows.len(),
            first_row_number: 1,
            sort: &[],
            editable: false,
            can_filter: false,
            row_mark: None,
        };
        let rows = &pane.result.rows;
        if let Some((r, c)) = pane.grid.selected {
            let value = rows.get(r).and_then(|row| row.get(c)).cloned().flatten();
            egui::Panel::right(egui::Id::new(("result-inspector", tab_id))).resizable(true).default_size(240.0).show(
                ui,
                |ui| {
                    grid::inspector_readonly(ui, &pane.result.columns[c], value.as_deref());
                },
            );
        }
        let cell =
            |r: usize, c: usize| (rows.get(r).and_then(|row| row.get(c)).and_then(|v| v.as_deref()), RowMark::Normal);
        let _ = grid::show(ui, &view, &mut pane.grid, &cell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_selection_current_and_line_offsets() {
        let mut t = SqlTab::new(1, None, None);
        t.text = "select 1;\nselect 2;\nselect 3".into();
        assert_eq!(t.script_for(RunMode::All, Engine::Sqlite).unwrap(), (t.text.clone(), 0));
        t.selection = Some(10..18);
        assert_eq!(t.script_for(RunMode::All, Engine::Sqlite).unwrap(), ("select 2".into(), 1));
        t.selection = None;
        t.cursor = Some(22);
        assert_eq!(t.script_for(RunMode::Current, Engine::Sqlite).unwrap(), ("select 3".into(), 2));
        t.text = "   ".into();
        assert!(t.script_for(RunMode::All, Engine::Sqlite).is_none());
        t.text = "select 1;\nselect * from t where a = 1".into();
        t.cursor = Some(15);
        assert_eq!(
            t.script_for(RunMode::Explain, Engine::Sqlite).unwrap(),
            ("EXPLAIN QUERY PLAN select * from t where a = 1".into(), 1)
        );
        assert_eq!(t.script_for(RunMode::Explain, Engine::Postgres).unwrap().0, "EXPLAIN select * from t where a = 1");
    }
}
