//! SQL text editor: `TextEdit` with lexer-driven highlighting (cached per text), a line-number
//! gutter, error-line marker and byte-accurate selection/cursor reporting.

use std::ops::Range;

use egui::cache::{ComputerMut, FrameCache};
use egui::text::{CCursor, CCursorRange};
use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, TextEdit};
use talus_core::model::Engine;
use talus_core::sql::lexer::{TokenKind, tokenize};

use super::completion::{self, Candidate, Kind, MetaView};
use crate::theme;

const KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "analyze",
    "and",
    "any",
    "as",
    "asc",
    "auto_increment",
    "autoincrement",
    "begin",
    "between",
    "bigint",
    "boolean",
    "by",
    "call",
    "cascade",
    "case",
    "cast",
    "char",
    "check",
    "collate",
    "column",
    "commit",
    "constraint",
    "create",
    "cross",
    "current_date",
    "current_timestamp",
    "database",
    "date",
    "decimal",
    "declare",
    "default",
    "delete",
    "desc",
    "describe",
    "distinct",
    "do",
    "drop",
    "else",
    "end",
    "escape",
    "except",
    "exists",
    "explain",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "from",
    "full",
    "function",
    "grant",
    "group",
    "having",
    "if",
    "ilike",
    "in",
    "index",
    "inner",
    "insert",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "join",
    "json",
    "jsonb",
    "key",
    "language",
    "lateral",
    "left",
    "like",
    "limit",
    "materialized",
    "natural",
    "not",
    "null",
    "numeric",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "over",
    "partition",
    "pragma",
    "primary",
    "procedure",
    "real",
    "recursive",
    "references",
    "rename",
    "replace",
    "restrict",
    "returning",
    "returns",
    "revoke",
    "right",
    "rollback",
    "row",
    "rows",
    "savepoint",
    "schema",
    "select",
    "sequence",
    "serial",
    "set",
    "show",
    "smallint",
    "start",
    "table",
    "temp",
    "temporary",
    "text",
    "then",
    "timestamp",
    "to",
    "transaction",
    "trigger",
    "true",
    "truncate",
    "type",
    "union",
    "unique",
    "unsigned",
    "update",
    "use",
    "using",
    "uuid",
    "vacuum",
    "values",
    "varchar",
    "view",
    "when",
    "where",
    "window",
    "with",
    "without",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.binary_search(&word.to_ascii_lowercase().as_str()).is_ok()
}

#[derive(Default)]
struct Highlighter;

impl ComputerMut<(Engine, &FontId, bool, &str), LayoutJob> for Highlighter {
    fn compute(&mut self, (engine, font, dark, text): (Engine, &FontId, bool, &str)) -> LayoutJob {
        let p = if dark { &theme::DARK } else { &theme::LIGHT };
        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        for t in tokenize(text, engine) {
            let s = t.text(text);
            let color = match t.kind {
                TokenKind::Word if is_keyword(s) => p.syntax_keyword,
                TokenKind::String => p.syntax_string,
                TokenKind::Number => p.syntax_number,
                TokenKind::LineComment | TokenKind::BlockComment => p.syntax_comment,
                TokenKind::QuotedIdent => p.syntax_ident,
                TokenKind::Param => p.syntax_param,
                _ => p.text,
            };
            job.append(s, 0.0, TextFormat::simple(font.clone(), color));
        }
        job
    }
}

type HighlightCache = FrameCache<LayoutJob, Highlighter>;

pub fn highlight(ctx: &egui::Context, engine: Engine, font: &FontId, text: &str) -> LayoutJob {
    let dark = ctx.global_style().visuals.dark_mode;
    ctx.memory_mut(|m| m.caches.cache::<HighlightCache>().get((engine, font, dark, text)).clone())
}

pub struct EditorOutput {
    pub changed: bool,
    /// Selected byte range (empty selection → `None`).
    pub selection: Option<Range<usize>>,
    /// Cursor byte offset.
    pub cursor: Option<usize>,
    /// Tables whose columns completion needs but are not loaded yet.
    pub need_columns: Vec<String>,
}

/// Open completion popup.
#[derive(Debug, Clone)]
pub struct Popup {
    pub items: Vec<Candidate>,
    pub selected: usize,
    pub start: usize,
    anchor: egui::Pos2,
}

pub fn char_to_byte(text: &str, ci: usize) -> usize {
    text.char_indices().nth(ci).map_or(text.len(), |(b, _)| b)
}

fn byte_to_char(text: &str, b: usize) -> usize {
    text[..b.min(text.len())].chars().count()
}

/// Moves the editor cursor/selection (char offsets) without user interaction.
pub fn set_selection(ctx: &egui::Context, id: egui::Id, text: &str, bytes: Range<usize>) {
    if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) {
        let range = CCursorRange::two(
            CCursor::new(byte_to_char(text, bytes.start)),
            CCursor::new(byte_to_char(text, bytes.end)),
        );
        state.cursor.set_char_range(Some(range));
        state.store(ctx, id);
    }
}

fn completion_keys(ui: &egui::Ui, id: egui::Id, text: &mut String, engine: Engine, popup: &mut Option<Popup>) -> bool {
    let Some(pp) = popup.as_mut() else { return false };
    let take = |k: egui::Key| ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
    if take(egui::Key::Escape) {
        *popup = None;
        return false;
    }
    if take(egui::Key::ArrowDown) {
        pp.selected = (pp.selected + 1) % pp.items.len().max(1);
    }
    if take(egui::Key::ArrowUp) {
        pp.selected = (pp.selected + pp.items.len().max(1) - 1) % pp.items.len().max(1);
    }
    if take(egui::Key::Enter) || take(egui::Key::Tab) {
        accept(ui.ctx(), id, text, engine, popup);
        return true;
    }
    false
}

fn accept(ctx: &egui::Context, id: egui::Id, text: &mut String, engine: Engine, popup: &mut Option<Popup>) {
    let Some(pp) = popup.take() else { return };
    let Some(item) = pp.items.get(pp.selected) else { return };
    let cursor = egui::text_edit::TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map_or(text.len(), |r| char_to_byte(text, r.primary.index.0));
    let start = pp.start.min(cursor);
    let insert = completion::insertable(&item.text, item.kind, engine);
    text.replace_range(start..cursor, &insert);
    let end = start + insert.len();
    set_selection(ctx, id, text, end..end);
}

/// `error_line` is 0-based. `meta` feeds completion; `completion_on` follows the setting.
/// Per-tab editor state kept between frames.
#[derive(Debug, Default)]
pub struct EditorState {
    pub popup: Option<Popup>,
    /// Columns were requested for completion; recompute once they arrive.
    pub awaiting_columns: bool,
}

/// Read-only inputs for one editor frame.
pub struct EditorEnv<'a> {
    pub engine: Engine,
    /// 0-based line to mark as failing.
    pub error_line: Option<usize>,
    pub meta: &'a MetaView,
    pub completion: bool,
}

pub fn sql_editor(
    ui: &mut egui::Ui,
    id: egui::Id,
    text: &mut String,
    state: &mut EditorState,
    env: &EditorEnv<'_>,
) -> EditorOutput {
    let (engine, error_line, meta, completion_on) = (env.engine, env.error_line, env.meta, env.completion);
    let EditorState { popup, awaiting_columns } = state;
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let p = theme::current(ui.ctx());
    let mut out = EditorOutput { changed: false, selection: None, cursor: None, need_columns: Vec::new() };
    let focused = ui.memory(|m| m.has_focus(id));
    if !focused {
        *popup = None;
    }
    let accepted = focused && completion_keys(ui, id, text, engine, popup);
    let manual = focused && completion_on && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Space));
    // Columns requested earlier may have arrived: recompute once they are in `meta`.
    let recheck = focused && *awaiting_columns;
    egui::ScrollArea::both().id_salt(id.with("scroll")).auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            let lines = text.split('\n').count().max(1);
            let width = ui.fonts_mut(|f| f.glyph_width(&font, '0')) * (lines.to_string().len() as f32 + 1.5);
            let row_h = ui.fonts_mut(|f| f.row_height(&font));
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h * lines as f32 + 4.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            for i in 0..lines {
                let y = rect.top() + 2.0 + i as f32 * row_h;
                let is_err = error_line == Some(i);
                if is_err {
                    painter.rect_filled(
                        egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(width, row_h)),
                        2.0,
                        p.danger.gamma_multiply(0.35),
                    );
                }
                painter.text(
                    egui::pos2(rect.right() - 6.0, y),
                    egui::Align2::RIGHT_TOP,
                    (i + 1).to_string(),
                    font.clone(),
                    if is_err { p.danger } else { p.text_dim },
                );
            }
            let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                let job = highlight(ui.ctx(), engine, &font, buf.as_str());
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let te = TextEdit::multiline(text)
                .id(id)
                .code_editor()
                .font(font.clone())
                .frame(egui::Frame::NONE)
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .desired_rows(20)
                .hint_text("-- Write SQL here. ⌘R runs the selection or the whole script, ⇧⌘R the current statement.")
                .layouter(&mut layouter)
                .show(ui);
            out.changed = te.response.changed() || accepted;
            if let Some(r) = te.cursor_range {
                let (a, b) = (r.primary.index.0, r.secondary.index.0);
                let (lo, hi) = (a.min(b), a.max(b));
                out.cursor = Some(char_to_byte(text, a));
                if lo != hi {
                    out.selection = Some(char_to_byte(text, lo)..char_to_byte(text, hi));
                }
                // Completion: refresh on typing, open on ⌃Space, close on cursor moves.
                let typed = te.response.changed() && !accepted;
                if completion_on && (typed || manual || recheck) && lo == hi {
                    let cur = char_to_byte(text, a);
                    match completion::context_at(text, cur, engine, manual) {
                        Some(ctx) => {
                            let (items, need) = completion::candidates(text, cur, engine, &ctx, meta, KEYWORDS);
                            *awaiting_columns = !need.is_empty();
                            out.need_columns = need;
                            let caret = te.galley.pos_from_cursor(r.primary);
                            let anchor = te.galley_pos + caret.left_bottom().to_vec2();
                            *popup =
                                (!items.is_empty()).then_some(Popup { items, selected: 0, start: ctx.start, anchor });
                        }
                        None => {
                            *popup = None;
                            *awaiting_columns = false;
                        }
                    }
                } else if !typed && !accepted && popup.as_ref().is_some_and(|pp| pp.start > char_to_byte(text, a)) {
                    *popup = None;
                }
            }
        });
    });
    let mut clicked = None;
    if let Some(pp) = popup.as_ref() {
        egui::Area::new(id.with("completion")).order(egui::Order::Foreground).fixed_pos(pp.anchor).show(
            ui.ctx(),
            |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(240.0);
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for (i, c) in pp.items.iter().enumerate() {
                            let icon = match c.kind {
                                Kind::Column => egui_phosphor::regular::COLUMNS,
                                Kind::Table => egui_phosphor::regular::TABLE,
                                Kind::View => egui_phosphor::regular::EYE,
                                Kind::Schema => egui_phosphor::regular::TREE_STRUCTURE,
                                Kind::Keyword => egui_phosphor::regular::TEXT_T,
                            };
                            let r = ui.horizontal(|ui| {
                                let r = ui.selectable_label(i == pp.selected, format!("{icon}  {}", c.text));
                                ui.label(egui::RichText::new(&c.detail).small().color(p.text_dim));
                                r
                            });
                            if i == pp.selected {
                                r.inner.scroll_to_me(None);
                            }
                            if r.inner.clicked() {
                                clicked = Some(i);
                            }
                        }
                    });
                });
            },
        );
    }
    if let Some(i) = clicked {
        if let Some(pp) = popup.as_mut() {
            pp.selected = i;
        }
        accept(ui.ctx(), id, text, engine, popup);
        ui.memory_mut(|m| m.request_focus(id));
        out.changed = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_list_sorted_for_binary_search() {
        let mut sorted = KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KEYWORDS);
        assert!(is_keyword("SELECT"));
        assert!(!is_keyword("customers"));
    }

    #[test]
    fn char_to_byte_handles_multibyte() {
        assert_eq!(char_to_byte("aяb", 2), 3);
        assert_eq!(char_to_byte("ab", 5), 2);
    }
}
