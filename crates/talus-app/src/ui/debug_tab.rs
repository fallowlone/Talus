//! PL/pgSQL debugger tab (opened from a routine's Debug button). Code pane with clickable line
//! numbers for breakpoints and the current line marked; Call Stack, Variables (double-click a value
//! to change it while paused, the eye pins it to the Watch List), Breakpoints and Log on the right.
//! F9 start / continue, F10 step over, F11 step into, ⇧F11 step out.

use std::collections::BTreeSet;

use egui::{Align2, RichText, Sense, vec2};
use egui_phosphor::regular as icons;
use talus_core::debugger::{DebugEvent, DebugTarget, Snapshot, Step};
use talus_core::model::{Engine, Outcome};
use uuid::Uuid;

use crate::state::Loadable;
use crate::theme;
use crate::worker::DebugOp;

const GUTTER: f32 = 46.0;

pub enum DebugAction {
    Describe,
    Op(DebugOp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Run {
    Idle,
    Paused(Snapshot),
    Finished,
}

pub struct DebugTab {
    pub conn: Uuid,
    pub database: String,
    pub oid: u32,
    pub name: String,
    pub target: Loadable<DebugTarget>,
    /// Argument text and its NULL flag, per input parameter.
    args: Vec<(String, bool)>,
    breakpoints: BTreeSet<(u32, i32)>,
    run: Run,
    /// A request is in flight; controls wait for it.
    pub busy: bool,
    log: Vec<(bool, String)>,
    editing: Option<(String, i32, String)>,
    /// Pinned variable names, shown with their value at every pause.
    watches: Vec<String>,
}

impl DebugTab {
    pub fn new(conn: Uuid, database: String, oid: u32, name: String) -> Self {
        Self {
            conn,
            database,
            oid,
            name,
            target: Loadable::NotLoaded,
            args: Vec::new(),
            breakpoints: BTreeSet::new(),
            run: Run::Idle,
            busy: false,
            log: Vec::new(),
            editing: None,
            watches: Vec::new(),
        }
    }

    pub fn title(&self) -> String {
        format!("Debug — {}", self.name)
    }

    pub fn set_target(&mut self, t: DebugTarget) {
        self.args = t.params.iter().map(|_| (String::new(), false)).collect();
        self.target = Loadable::Loaded(t);
    }

    pub fn fail(&mut self, msg: String) {
        self.busy = false;
        if matches!(self.target, Loadable::Loading(_) | Loadable::NotLoaded) {
            self.target = Loadable::Failed(msg);
        } else {
            self.log.push((true, msg));
        }
    }

    /// Result of a start / step / value change (`None`: breakpoint change or stop finished).
    pub fn on_event(&mut self, ev: Option<DebugEvent>) {
        self.busy = false;
        match ev {
            Some(DebugEvent::Paused(s)) => {
                self.log.push((false, format!("Paused at line {} of {}", s.line, frame_name(&s))));
                self.run = Run::Paused(s);
            }
            Some(DebugEvent::Finished(out)) => {
                for e in &out.errors {
                    self.log.push((true, e.error.to_string()));
                }
                for r in &out.results {
                    match &r.outcome {
                        // What an Oracle routine printed (its OUT parameters and result), one line per row.
                        Outcome::Rows(q) if q.columns.len() == 1 && q.columns[0].name == "DBMS_OUTPUT" => {
                            for row in q.rows.iter().take(20) {
                                self.log.push((false, format!("Result: {}", row[0].as_deref().unwrap_or(""))));
                            }
                        }
                        Outcome::Rows(q) => {
                            let cols: Vec<&str> = q.columns.iter().map(|c| c.name.as_str()).collect();
                            for row in q.rows.iter().take(20) {
                                let vals: Vec<String> = row
                                    .iter()
                                    .zip(&cols)
                                    .map(|(v, c)| format!("{c} = {}", v.as_deref().unwrap_or("NULL")))
                                    .collect();
                                self.log.push((false, format!("Result: {}", vals.join(", "))));
                            }
                        }
                        Outcome::Affected(_) => self.log.push((false, "Procedure completed.".into())),
                    }
                }
                if out.errors.is_empty() {
                    self.log.push((false, "Finished.".into()));
                }
                self.run = Run::Finished;
            }
            None => {
                if self.run == Run::Idle {
                    self.log.push((false, "Stopped.".into()));
                }
            }
        }
    }

    fn op(&mut self, out: &mut Vec<DebugAction>, op: DebugOp) {
        self.busy = !matches!(op, DebugOp::Breakpoint { .. });
        out.push(DebugAction::Op(op));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<DebugAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        let target = match &self.target {
            Loadable::NotLoaded => {
                out.push(DebugAction::Describe);
                return out;
            }
            Loadable::Loading(_) => {
                ui.spinner();
                return out;
            }
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return out;
            }
            Loadable::Loaded(t) => t.clone(),
        };
        let paused = matches!(self.run, Run::Paused(_));
        self.toolbar(ui, &target, paused, &mut out);
        if !paused {
            self.params_ui(ui, &target);
        }
        ui.add_space(6.0);
        egui::Panel::right(egui::Id::new(("debug-side", tab_id))).resizable(true).default_size(360.0).show(ui, |ui| {
            self.side_ui(ui, tab_id, paused, &mut out);
        });
        let (func, source, current) = match &self.run {
            Run::Paused(s) => (s.func, s.source.clone(), Some(s.line)),
            _ => (target.oid, target.source.clone(), None),
        };
        egui::ScrollArea::both().id_salt(("debug-code", tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            self.code_ui(ui, func, &source, current, &mut out);
        });
        out
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, target: &DebugTarget, paused: bool, out: &mut Vec<DebugAction>) {
        let p = theme::current(ui.ctx());
        let (f9, f10, sf11, f11) = ui.input_mut(|i| {
            // ⇧F11 first, so plain F11 does not also take it.
            let sf11 = i.consume_key(egui::Modifiers::SHIFT, egui::Key::F11);
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::F9),
                i.consume_key(egui::Modifiers::NONE, egui::Key::F10),
                sf11,
                i.consume_key(egui::Modifiers::NONE, egui::Key::F11),
            )
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&target.signature).font(theme::semibold(15.0)).color(p.text));
            ui.add_space(12.0);
            let idle = !self.busy;
            if !paused {
                let start = ui
                    .add_enabled(idle, theme::primary_button(p, format!("{} Start", icons::PLAY)))
                    .on_hover_text("Run in debug mode (F9)");
                if idle && (start.clicked() || f9) {
                    let args = self.args.iter().map(|(v, null)| (!null).then(|| v.clone())).collect();
                    let breakpoints =
                        self.breakpoints.iter().filter(|(f, _)| *f == target.oid).map(|(_, l)| *l).collect();
                    self.log.push((false, format!("Starting {}…", target.signature)));
                    self.op(out, DebugOp::Start { target: Box::new(target.clone()), args, breakpoints });
                }
            } else {
                let steps = [
                    (icons::ARROW_BEND_DOWN_RIGHT, "Step Over", "F10", Step::Over, f10),
                    (icons::ARROW_DOWN, "Step Into", "F11", Step::Into, f11),
                    (icons::ARROW_UP, "Step Out", "⇧F11", Step::Out, sf11),
                    (icons::FAST_FORWARD, "Continue", "F9", Step::Continue, f9),
                ];
                for (icon, label, key, step, pressed) in steps {
                    let r = ui.add_enabled(idle, egui::Button::new(format!("{icon} {label}"))).on_hover_text(key);
                    if idle && (r.clicked() || pressed) {
                        self.op(out, DebugOp::Step(step));
                    }
                }
            }
            if ui.add_enabled(paused || self.busy, egui::Button::new(format!("{} Stop", icons::STOP))).clicked() {
                self.run = Run::Idle;
                self.op(out, DebugOp::Stop);
            }
            if self.busy {
                ui.spinner();
                ui.label(RichText::new("Running…").color(p.text_dim));
            }
        });
    }

    fn params_ui(&mut self, ui: &mut egui::Ui, target: &DebugTarget) {
        if target.params.is_empty() {
            return;
        }
        let p = theme::current(ui.ctx());
        ui.add_space(4.0);
        ui.label(RichText::new("Input parameters").color(p.text_dim));
        egui::Grid::new("debug-params").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
            for (param, (value, null)) in target.params.iter().zip(self.args.iter_mut()) {
                let l = ui.label(format!("{}  {}", param.name, param.type_name));
                ui.add_enabled(!*null, crate::ui::widgets::field(value).desired_width(220.0)).labelled_by(l.id);
                crate::ui::widgets::check(ui, null, "NULL");
                ui.end_row();
            }
        });
    }

    fn code_ui(
        &mut self,
        ui: &mut egui::Ui,
        func: u32,
        source: &str,
        current: Option<i32>,
        out: &mut Vec<DebugAction>,
    ) {
        let p = theme::current(ui.ctx());
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 4.0;
        let width = ui.available_width().max(600.0);
        for (i, text) in source.lines().enumerate() {
            let line = i as i32 + 1;
            let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
            let gutter = egui::Rect::from_min_size(rect.min, vec2(GUTTER, row_h));
            let g = ui.interact(gutter, ui.id().with(("bp", func, line)), Sense::click());
            g.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("line {line}")));
            let on = self.breakpoints.contains(&(func, line));
            if g.on_hover_text("Toggle breakpoint").clicked() {
                if on {
                    self.breakpoints.remove(&(func, line));
                } else {
                    self.breakpoints.insert((func, line));
                }
                self.op(out, DebugOp::Breakpoint { func, line, on: !on });
            }
            let painter = ui.painter();
            if current == Some(line) {
                painter.rect_filled(rect, 4.0, p.edited);
            }
            if on {
                painter.circle_filled(egui::pos2(gutter.left() + 9.0, gutter.center().y), 4.5, p.danger);
            }
            painter.text(
                egui::pos2(gutter.right() - 8.0, gutter.center().y),
                Align2::RIGHT_CENTER,
                line.to_string(),
                font.clone(),
                if current == Some(line) { p.text } else { p.text_faint },
            );
            let job = super::editor::highlight(ui.ctx(), Engine::Postgres, &font, text);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            painter.galley(egui::pos2(gutter.right() + 6.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
        }
    }

    fn side_ui(&mut self, ui: &mut egui::Ui, tab_id: u64, paused: bool, out: &mut Vec<DebugAction>) {
        let p = theme::current(ui.ctx());
        egui::ScrollArea::vertical().id_salt(("debug-side-scroll", tab_id)).auto_shrink([false, false]).show(
            ui,
            |ui| {
                if let Run::Paused(s) = &self.run {
                    section(ui, "Call Stack");
                    for f in &s.stack {
                        ui.label(format!("{}  {} — line {}", f.level, f.name, f.line));
                    }
                    ui.add_space(10.0);
                    section(ui, "Variables");
                    let vars = s.variables.clone();
                    egui::Grid::new(("debug-vars", tab_id)).num_columns(4).spacing([12.0, 4.0]).show(ui, |ui| {
                        for v in &vars {
                            let kind = if v.class == 'A' { "param" } else { "local" };
                            ui.label(RichText::new(&v.name).color(p.text)).on_hover_text(kind);
                            ui.label(RichText::new(&v.type_name).color(p.text_dim));
                            let editing = self.editing.as_ref().is_some_and(|(n, l, _)| *n == v.name && *l == v.line);
                            if editing {
                                let Some((name, line, buf)) = self.editing.as_mut() else { continue };
                                let r = ui.add(crate::ui::widgets::field(buf).desired_width(140.0));
                                r.request_focus();
                                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    let op = DebugOp::SetValue { name: name.clone(), line: *line, value: buf.clone() };
                                    self.editing = None;
                                    self.op(out, op);
                                } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    self.editing = None;
                                }
                            } else {
                                let shown = v.value.clone().unwrap_or_else(|| "NULL".into());
                                let r =
                                    ui.add(egui::Label::new(RichText::new(&shown).monospace()).sense(Sense::click()));
                                if paused
                                    && !self.busy
                                    && r.on_hover_text("Double-click to set a value").double_clicked()
                                {
                                    self.editing = Some((v.name.clone(), v.line, v.value.clone().unwrap_or_default()));
                                }
                            }
                            let watched = self.watches.contains(&v.name);
                            let tip = format!("Watch {}", v.name);
                            if !watched && ui.small_button(icons::EYE).on_hover_text(&tip).clicked() {
                                self.watches.push(v.name.clone());
                            }
                            ui.end_row();
                        }
                    });
                    ui.add_space(10.0);
                }
                if !self.watches.is_empty() {
                    section(ui, "Watch List");
                    let vars = match &self.run {
                        Run::Paused(s) => s.variables.clone(),
                        _ => Vec::new(),
                    };
                    let mut unwatch = None;
                    for (i, name) in self.watches.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).color(p.text));
                            match vars.iter().find(|v| &v.name == name) {
                                Some(v) => {
                                    ui.label(RichText::new(v.value.as_deref().unwrap_or("NULL")).monospace());
                                }
                                None => {
                                    ui.label(RichText::new("not in scope").color(p.text_faint));
                                }
                            }
                            if ui.small_button(icons::X).on_hover_text(format!("Stop watching {name}")).clicked() {
                                unwatch = Some(i);
                            }
                        });
                    }
                    if let Some(i) = unwatch {
                        self.watches.remove(i);
                    }
                    ui.add_space(10.0);
                }
                section(ui, "Breakpoints");
                if self.breakpoints.is_empty() {
                    ui.label(RichText::new("Click a line number to add one.").color(p.text_faint));
                }
                for (f, l) in self.breakpoints.clone() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(p.danger));
                        ui.label(format!("line {l}"));
                        if ui.small_button(icons::X).on_hover_text("Remove breakpoint").clicked() {
                            self.breakpoints.remove(&(f, l));
                            self.op(out, DebugOp::Breakpoint { func: f, line: l, on: false });
                        }
                    });
                }
                ui.add_space(10.0);
                section(ui, "Log");
                for (err, line) in &self.log {
                    ui.label(RichText::new(line).color(if *err { p.danger } else { p.text }));
                }
            },
        );
    }
}

fn frame_name(s: &Snapshot) -> String {
    s.stack.first().map_or_else(|| s.func.to_string(), |f| f.name.clone())
}

fn section(ui: &mut egui::Ui, title: &str) {
    let p = theme::current(ui.ctx());
    ui.label(RichText::new(title).font(theme::semibold(12.5)).color(p.text_dim));
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use talus_core::engine::ScriptOutcome;

    #[test]
    fn events_drive_the_run_state_and_log() {
        let mut t = DebugTab::new(Uuid::nil(), "db".into(), 1, "f(integer)".into());
        t.busy = true;
        let snap = Snapshot { func: 1, line: 4, stack: Vec::new(), variables: Vec::new(), source: "x".into() };
        t.on_event(Some(DebugEvent::Paused(snap.clone())));
        assert!(!t.busy);
        assert_eq!(t.run, Run::Paused(snap));
        t.on_event(Some(DebugEvent::Finished(ScriptOutcome::default())));
        assert_eq!(t.run, Run::Finished);
        assert_eq!(t.log.last().map(|(_, l)| l.as_str()), Some("Finished."));
    }
}
