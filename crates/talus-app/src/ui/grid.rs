//! Virtualised data grid shared by query results and the table data editor. Only visible
//! rows are laid out, so result size does not affect frame time.

use egui::{Align, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use talus_core::model::{Cell, ColumnMeta, SortDir};

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMark {
    Normal,
    Edited,
    Inserted,
    Deleted,
}

#[derive(Debug, Default, Clone)]
pub struct GridState {
    pub selected: Option<(usize, usize)>,
    /// Cell being edited inline and its text buffer.
    pub editing: Option<(usize, usize, String)>,
    widths: Vec<f32>,
    edit_focus_pending: bool,
}

impl GridState {
    pub fn reset(&mut self) {
        *self = GridState::default();
    }
    pub fn begin_edit(&mut self, row: usize, col: usize, current: Option<&str>) {
        self.editing = Some((row, col, current.unwrap_or_default().to_string()));
        self.edit_focus_pending = true;
    }
}

pub enum GridEvent {
    HeaderClicked(usize),
    /// Double click / Enter on a cell (the owner decides whether it is editable).
    Activate(usize, usize),
    Commit {
        row: usize,
        col: usize,
        value: Cell,
    },
    SetNull(usize, usize),
    DeleteRow(usize),
    CopyAsInsert(usize),
    FilterByValue(usize, usize),
}

pub struct GridView<'a> {
    pub id: egui::Id,
    pub columns: &'a [ColumnMeta],
    /// Indices into `columns` that are displayed (hidden helper columns excluded).
    pub visible: &'a [usize],
    pub rows: usize,
    /// Number shown for row 0 (page offset + 1).
    pub first_row_number: u64,
    pub sort: &'a [(String, SortDir)],
    pub editable: bool,
    pub can_filter: bool,
    /// Row-header marker (edited/new/deleted); `None` for plain result grids.
    pub row_mark: Option<&'a dyn Fn(usize) -> RowMark>,
}

/// Horizontal padding inside each grid cell.
const CELL_PAD: f32 = 8.0;

pub type CellFn<'a> = dyn Fn(usize, usize) -> (Option<&'a str>, RowMark) + 'a;

fn initial_widths(view: &GridView<'_>, cell: &CellFn<'_>, char_w: f32) -> Vec<f32> {
    view.visible
        .iter()
        .map(|&c| {
            let header = view.columns[c].name.chars().count() + 3;
            let sample = (0..view.rows.min(60))
                .map(|r| cell(r, c).0.map_or(4, |s| s.chars().take(60).count()))
                .max()
                .unwrap_or(0);
            (header.max(sample) as f32 * char_w + 16.0).clamp(56.0, 380.0)
        })
        .collect()
}

pub fn tsv_escape(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

pub fn show(ui: &mut egui::Ui, view: &GridView<'_>, state: &mut GridState, cell: &CellFn<'_>) -> Vec<GridEvent> {
    let p = theme::current(ui.ctx());
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&mono, '0'));
    if state.widths.len() != view.visible.len() {
        state.widths = initial_widths(view, cell, char_w);
    }
    let mut events = Vec::new();
    let row_h = ui.fonts_mut(|f| f.row_height(&mono)) + 12.0;
    let num_w = (format!("{}", view.first_row_number + view.rows as u64).len() as f32 * char_w + 18.0).max(40.0);

    // Keyboard: copy, navigation and edit start when no text field owns focus.
    let no_focus = ui.ctx().memory(|m| m.focused().is_none());
    if no_focus
        && state.editing.is_none()
        && let Some((r, c)) = state.selected
    {
        let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy {
            ui.ctx().copy_text(cell(r, c).0.unwrap_or_default().to_string());
        }
        let (mut r2, mut c2) = (r as i64, c as i64);
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowDown) {
                r2 += 1;
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                r2 -= 1;
            }
            if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Tab) {
                c2 += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                c2 -= 1;
            }
        });
        let pos = view.visible.iter().position(|&v| v == c).unwrap_or(0) as i64;
        let new_pos = (pos + (c2 - c as i64)).clamp(0, view.visible.len() as i64 - 1) as usize;
        let new_r = r2.clamp(0, view.rows.saturating_sub(1) as i64) as usize;
        if view.rows > 0 && !view.visible.is_empty() {
            state.selected = Some((new_r, view.visible[new_pos]));
        }
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            events.push(GridEvent::Activate(r, c));
        }
        if view.editable && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Num0)) {
            events.push(GridEvent::SetNull(r, c));
        }
    }

    // No rules, stripes or tinted header: rows are separated by height, the header by type.
    egui::ScrollArea::horizontal().id_salt(view.id.with("h")).auto_shrink([false, false]).show(ui, |ui| {
        // Columns abut (selection fills are continuous); cells pad themselves.
        ui.spacing_mut().item_spacing.x = 0.0;
        let mut tb = TableBuilder::new(ui)
            .id_salt(view.id)
            .striped(false)
            .resizable(true)
            .auto_shrink([false, false])
            .max_scroll_height(f32::INFINITY)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(num_w).resizable(false));
        for w in &state.widths {
            tb = tb.column(Column::initial(*w).at_least(36.0).clip(true).resizable(true));
        }
        if let Some((r, _)) = state.selected {
            tb = tb.scroll_to_row(r, None);
        }

        tb.header(row_h + 4.0, |mut h| {
            h.col(|ui| {
                ui.add_space(CELL_PAD);
                ui.label(RichText::new("#").color(p.text_faint));
            });
            for &c in view.visible {
                h.col(|ui| {
                    ui.add_space(CELL_PAD);
                    let col = &view.columns[c];
                    let arrow = view.sort.iter().find(|(n, _)| *n == col.name).map(|(_, d)| match d {
                        SortDir::Asc => " ▲",
                        SortDir::Desc => " ▼",
                    });
                    let title = format!("{}{}", col.name, arrow.unwrap_or(""));
                    let r = ui.add(
                        egui::Label::new(RichText::new(title).font(theme::semibold(12.5)).color(p.text_dim))
                            .truncate()
                            .sense(Sense::click()),
                    );
                    let r = if col.type_name.is_empty() { r } else { r.on_hover_text(&col.type_name) };
                    if r.clicked() {
                        events.push(GridEvent::HeaderClicked(c));
                    }
                });
            }
        })
        .body(|body| {
            body.rows(row_h, view.rows, |mut row| {
                let r = row.index();
                let mark = view.row_mark.map_or(RowMark::Normal, |f| f(r));
                row.col(|ui| {
                    let (txt, color) = match mark {
                        RowMark::Inserted => (egui_phosphor::regular::PLUS.to_string(), p.ok),
                        RowMark::Deleted => (egui_phosphor::regular::MINUS.to_string(), p.danger),
                        RowMark::Edited => (egui_phosphor::regular::PENCIL_SIMPLE.to_string(), p.warn),
                        RowMark::Normal => ((view.first_row_number + r as u64).to_string(), p.text_faint),
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(CELL_PAD);
                        ui.label(RichText::new(txt).monospace().color(color))
                    });
                });
                for &c in view.visible {
                    row.col(|ui| {
                        ui.add_space(CELL_PAD);
                        let (value, cell_mark) = cell(r, c);
                        let bg = match cell_mark {
                            RowMark::Edited => Some(p.edited),
                            RowMark::Inserted => Some(p.inserted),
                            RowMark::Deleted => Some(p.deleted),
                            RowMark::Normal => None,
                        };
                        let selected = state.selected == Some((r, c));
                        let rect = ui.max_rect();
                        if let Some(bg) = bg {
                            ui.painter().rect_filled(rect, 0.0, bg);
                        }
                        if selected {
                            ui.painter().rect_stroke(
                                rect.shrink(0.5),
                                0.0,
                                egui::Stroke::new(1.5, p.accent),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some((er, ec, buf)) = state.editing.as_mut().filter(|(er, ec, _)| *er == r && *ec == c) {
                            let (er, ec) = (*er, *ec);
                            let te = ui.add(
                                egui::TextEdit::singleline(buf)
                                    .font(mono.clone())
                                    .desired_width(f32::INFINITY)
                                    .frame(egui::Frame::NONE),
                            );
                            if state.edit_focus_pending {
                                te.request_focus();
                                state.edit_focus_pending = false;
                            }
                            let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                            if esc {
                                state.editing = None;
                            } else if te.lost_focus() {
                                let value = Some(std::mem::take(buf));
                                state.editing = None;
                                events.push(GridEvent::Commit { row: er, col: ec, value });
                            }
                            return;
                        }
                        let text = match value {
                            None => RichText::new("NULL").italics().color(p.text_dim),
                            Some(s) => {
                                let shown: String = s
                                    .chars()
                                    .take(500)
                                    .map(|ch| if ch == '\n' || ch == '\t' { ' ' } else { ch })
                                    .collect();
                                let t = RichText::new(shown).monospace();
                                if cell_mark == RowMark::Deleted { t.strikethrough() } else { t.color(p.text) }
                            }
                        };
                        let resp = ui.add(egui::Label::new(text).truncate().sense(Sense::click()));
                        if resp.clicked() {
                            state.selected = Some((r, c));
                            ui.ctx().memory_mut(|m| {
                                if let Some(f) = m.focused() {
                                    m.surrender_focus(f);
                                }
                            });
                        }
                        if resp.double_clicked() {
                            state.selected = Some((r, c));
                            events.push(GridEvent::Activate(r, c));
                        }
                        resp.context_menu(|ui| {
                            state.selected = Some((r, c));
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(value.unwrap_or_default().to_string());
                                ui.close();
                            }
                            if ui.button("Copy Row (TSV)").clicked() {
                                let line: Vec<String> = view
                                    .visible
                                    .iter()
                                    .map(|&cc| tsv_escape(cell(r, cc).0.unwrap_or_default()))
                                    .collect();
                                ui.ctx().copy_text(line.join("\t"));
                                ui.close();
                            }
                            if ui.button("Copy Column Name").clicked() {
                                ui.ctx().copy_text(view.columns[c].name.clone());
                                ui.close();
                            }
                            if view.can_filter {
                                if ui.button("Copy Row as INSERT").clicked() {
                                    events.push(GridEvent::CopyAsInsert(r));
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Filter by This Value").clicked() {
                                    events.push(GridEvent::FilterByValue(r, c));
                                    ui.close();
                                }
                            }
                            if view.editable {
                                ui.separator();
                                if ui.button("Edit Cell").clicked() {
                                    events.push(GridEvent::Activate(r, c));
                                    ui.close();
                                }
                                if ui.button("Set to NULL   ⌃0").clicked() {
                                    events.push(GridEvent::SetNull(r, c));
                                    ui.close();
                                }
                                if ui.button("Set to Empty String").clicked() {
                                    events.push(GridEvent::Commit { row: r, col: c, value: Some(String::new()) });
                                    ui.close();
                                }
                                if ui.button(RichText::new("Delete Record").color(p.danger)).clicked() {
                                    events.push(GridEvent::DeleteRow(r));
                                    ui.close();
                                }
                            }
                        });
                    });
                }
            });
        });
    });
    events
}

/// Read-only value inspector for one cell (full text, length, NULL-aware).
pub fn inspector_readonly(ui: &mut egui::Ui, column: &ColumnMeta, value: Option<&str>) {
    let p = theme::current(ui.ctx());
    ui.label(RichText::new(&column.name).strong());
    if !column.type_name.is_empty() {
        ui.label(RichText::new(&column.type_name).small().color(p.text_dim));
    }
    ui.separator();
    match value {
        None => {
            ui.label(RichText::new("NULL").italics().color(p.text_dim));
        }
        Some(v) => {
            ui.label(RichText::new(format!("{} characters", v.chars().count())).small().color(p.text_dim));
            let pretty = pretty_json(v);
            let mut shown = pretty.as_deref().unwrap_or(v);
            egui::ScrollArea::both().id_salt("inspector-ro").auto_shrink([false, false]).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut shown).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY),
                );
            });
        }
    }
}

/// Indented JSON when `v` is a JSON object/array, for readability.
pub fn pretty_json(v: &str) -> Option<String> {
    let t = v.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(v).ok().and_then(|j| serde_json::to_string_pretty(&j).ok())
}
