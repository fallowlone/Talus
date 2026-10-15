//! Small shared widgets of the visual language: sidebar items, section headers, card headers,
//! quiet icon buttons, badges and avatars. All are drawn with the palette in `theme`.

use egui::{Align2, Color32, FontId, Response, Sense, Ui, WidgetInfo, WidgetType, vec2};
use egui_phosphor::regular as icons;

use crate::theme::{self, Palette};

pub const ROW_H: f32 = 30.0;

fn fill_for(p: &Palette, resp: &Response, selected: bool) -> Option<Color32> {
    if selected {
        Some(p.selected)
    } else if resp.hovered() {
        Some(p.hover)
    } else {
        None
    }
}

/// Sidebar navigation item: icon, label, optional trailing chevron; grey fill when selected.
/// The accessible label is the plain `label`.
pub fn nav_item(ui: &mut Ui, icon: &str, label: &str, selected: bool, chevron: bool) -> Response {
    let p = theme::current(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, ui.is_enabled(), selected, label));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if let Some(fill) = fill_for(p, &resp, selected) {
            painter.rect_filled(rect, 7.0, fill);
        }
        let y = rect.center().y;
        painter.text(egui::pos2(rect.left() + 10.0, y), Align2::LEFT_CENTER, icon, FontId::proportional(16.0), p.text);
        painter.text(egui::pos2(rect.left() + 34.0, y), Align2::LEFT_CENTER, label, FontId::proportional(13.5), p.text);
        if chevron {
            painter.text(
                egui::pos2(rect.right() - 10.0, y),
                Align2::RIGHT_CENTER,
                icons::CARET_RIGHT,
                FontId::proportional(13.0),
                p.text_dim,
            );
        }
    }
    resp
}

/// Collapsible sidebar section title ("Connections ▾"). Toggles `open` on click.
pub fn section_header(ui: &mut Ui, label: &str, open: &mut bool) -> Response {
    let p = theme::current(ui.ctx());
    ui.add_space(10.0);
    let font = theme::semibold(12.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font.clone(), p.text_dim);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 34.0, 22.0), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), label));
    if resp.clicked() {
        *open = !*open;
    }
    let color = if resp.hovered() { p.text } else { p.text_dim };
    let painter = ui.painter();
    painter.text(egui::pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, label, font, color);
    painter.text(
        egui::pos2(rect.left() + 16.0 + galley.size().x, rect.center().y),
        Align2::LEFT_CENTER,
        if *open { icons::CARET_DOWN } else { icons::CARET_RIGHT },
        FontId::proportional(10.0),
        color,
    );
    resp
}

/// Title row above a card: icon, title, chevron on the right.
/// Returns the row response (clickable when `clickable`).
pub fn card_header(ui: &mut Ui, icon: &str, title: &str, clickable: bool) -> Response {
    let p = theme::current(ui.ctx());
    let sense = if clickable { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), sense);
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), title));
    let painter = ui.painter();
    let sq = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - 11.0), vec2(22.0, 22.0));
    painter.text(sq.center(), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), p.text_dim);
    painter.text(
        egui::pos2(sq.right() + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.5),
        if resp.hovered() && clickable { p.text } else { p.text_dim },
    );
    if clickable {
        painter.text(
            egui::pos2(rect.right() - 4.0, rect.center().y),
            Align2::RIGHT_CENTER,
            icons::CARET_RIGHT,
            FontId::proportional(13.0),
            p.text_faint,
        );
    }
    resp
}

/// Quiet square icon button (top bar, panel headers).
pub fn icon_button(ui: &mut Ui, icon: &str, tip: &str, selected: bool) -> Response {
    let p = theme::current(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, ui.is_enabled(), selected, tip));
    if let Some(fill) = fill_for(p, &resp, selected) {
        ui.painter().rect_filled(rect, 7.0, fill);
    }
    let color = if resp.hovered() || selected { p.text } else { p.text_dim };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(17.0), color);
    resp.on_hover_text(tip)
}

/// Small rounded badge ("Admin", "SQLite").
pub fn badge(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> Response {
    let font = theme::semibold(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(10.0, 4.0), Sense::hover());
    ui.painter().rect_filled(rect, 4.0, bg);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
    resp
}

/// Keyboard shortcut hint ("⌘", "K") drawn right-aligned at `right_center`; returns its width.
pub fn key_cap(painter: &egui::Painter, p: &Palette, right_center: egui::Pos2, key: &str) -> f32 {
    let galley = painter.layout_no_wrap(key.to_string(), FontId::proportional(12.0), p.text_faint);
    let w = galley.size().x;
    painter.galley(right_center - vec2(w, galley.size().y / 2.0), galley, p.text_faint);
    w
}

/// Filled circle with a centred glyph (workspace / connection avatar).
pub fn avatar(ui: &mut Ui, glyph: &str, size: f32, fill: Color32, fg: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, theme::semibold(size * 0.5), fg);
    resp
}

/// White card lifted by `theme::card_shadows` (painted behind the frame once its size is known).
pub fn card<R>(ui: &mut Ui, margin: egui::Margin, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let p = theme::current(ui.ctx());
    let slots = [ui.painter().add(egui::Shape::Noop), ui.painter().add(egui::Shape::Noop)];
    let inner = theme::card_frame(p).inner_margin(margin).show(ui, add);
    for (slot, shadow) in slots.into_iter().zip(theme::card_shadows(p)) {
        ui.painter().set(slot, shadow.as_shape(inner.response.rect, theme::CARD_RADIUS));
    }
    inner
}

/// Soft shadow cast leftwards from a pane edge at `x` over `rect` (separates a pane without a line).
pub fn edge_shadow(painter: &egui::Painter, rect: egui::Rect, x: f32) {
    let p = theme::current(painter.ctx());
    let dark = p.bg == theme::DARK.bg;
    let strong = Color32::from_black_alpha(if dark { 60 } else { 12 });
    let mut mesh = egui::Mesh::default();
    let (x0, y0, y1) = (x - 14.0, rect.top(), rect.bottom());
    mesh.colored_vertex(egui::pos2(x0, y0), Color32::TRANSPARENT);
    mesh.colored_vertex(egui::pos2(x, y0), strong);
    mesh.colored_vertex(egui::pos2(x, y1), strong);
    mesh.colored_vertex(egui::pos2(x0, y1), Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(mesh);
}

/// Drop-down with a thin chevron instead of egui's filled triangle (one style for every combo box).
pub fn combo(id_salt: impl egui::AsIdSalt) -> egui::ComboBox {
    egui::ComboBox::from_id_salt(id_salt).icon(|ui, rect, visuals, _open| {
        let p = theme::current(ui.ctx());
        let color = if visuals.fg_stroke.color == p.text { p.text_dim } else { visuals.fg_stroke.color };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icons::CARET_DOWN, FontId::proportional(12.0), color);
    })
}

/// A table drawn on a canvas (ER diagram, model): the app's card surface, with the black focus ring
/// when `ring` (selected, hovered or dragged).
pub fn table_card(painter: &egui::Painter, rect: egui::Rect, ring: bool) {
    let p = theme::current(painter.ctx());
    for shadow in theme::card_shadows(p) {
        painter.add(shadow.as_shape(rect, 8));
    }
    painter.rect_filled(rect, 8.0, p.surface);
    if ring {
        painter.rect_stroke(rect, 8.0, egui::Stroke::new(1.5, p.accent), egui::StrokeKind::Inside);
    }
}

/// Ink (text colour) at `alpha`, for hairlines and tints that read on any surface in both themes.
fn edge(p: &Palette, alpha: u8) -> Color32 {
    let v = if p.bg == theme::DARK.bg { 0xff } else { 0x14 };
    let c = (v as u16 * alpha as u16 / 255) as u8;
    Color32::from_rgba_premultiplied(c, c, c, alpha)
}

/// iOS-style switch. `label` is the accessible name only (draw the visible label yourself).
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let p = theme::current(ui.ctx());
    let (rect, mut resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let value = *on;
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, ui.is_enabled(), value, label));
    let t = ui.ctx().animate_bool_responsive(resp.id, value);
    let track = if value { p.accent } else { edge(p, if resp.hovered() { 60 } else { 44 }) };
    ui.painter().rect_filled(rect, rect.height() / 2.0, track);
    let r = rect.height() / 2.0 - 3.0;
    let x = egui::lerp((rect.left() + r + 3.0)..=(rect.right() - r - 3.0), t);
    let knob = if value { p.on_accent } else { p.raised };
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), r, knob);
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            rect.height() / 2.0 + 2.0,
            egui::Stroke::new(1.5, p.accent),
            egui::StrokeKind::Outside,
        );
    }
    resp
}

/// Segmented control: one pill with a raised thumb under the selected option. Returns true when `value` changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let p = theme::current(ui.ctx());
    let font = FontId::proportional(13.0);
    let pad = vec2(14.0, 0.0);
    let h = 28.0;
    let galleys: Vec<_> =
        options.iter().map(|(_, l)| ui.painter().layout_no_wrap((*l).to_string(), font.clone(), p.text)).collect();
    let total: f32 = galleys.iter().map(|g| g.size().x + pad.x * 2.0).sum::<f32>() + 4.0;
    let (outer, _) = ui.allocate_exact_size(vec2(total, h + 4.0), Sense::hover());
    ui.painter().rect_filled(outer, 9.0, edge(p, 20));
    let mut changed = false;
    let mut x = outer.left() + 2.0;
    for ((opt, label), galley) in options.iter().zip(galleys) {
        let w = galley.size().x + pad.x * 2.0;
        let rect = egui::Rect::from_min_size(egui::pos2(x, outer.top() + 2.0), vec2(w, h));
        x += w;
        let resp = ui.interact(rect, ui.id().with(label), Sense::click());
        let selected = *value == *opt;
        resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, ui.is_enabled(), selected, *label));
        if resp.clicked() && !selected {
            *value = *opt;
            changed = true;
        }
        if selected {
            for shadow in theme::card_shadows(p).into_iter().take(1) {
                ui.painter().add(shadow.as_shape(rect, 7));
            }
            ui.painter().rect_filled(rect, 7.0, p.raised);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 7.0, edge(p, 14));
        }
        let color = if selected { p.text } else { p.text_dim };
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    changed
}

/// Selectable card for choosing one of several options: icon tile, title, caption, check when selected.
pub fn option_card(ui: &mut Ui, width: f32, icon: &str, title: &str, caption: &str, selected: bool) -> Response {
    let p = theme::current(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 54.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, ui.is_enabled(), selected, title));
    let painter = ui.painter();
    let fill = if selected {
        edge(p, 14)
    } else if resp.hovered() && ui.is_enabled() {
        edge(p, 9)
    } else {
        p.raised
    };
    painter.rect_filled(rect, 10.0, fill);
    let stroke = if selected {
        egui::Stroke::new(1.5, p.accent)
    } else {
        egui::Stroke::new(1.0, edge(p, if resp.hovered() { 80 } else { 40 }))
    };
    painter.rect_stroke(rect.shrink(0.5), 10.0, stroke, egui::StrokeKind::Inside);
    let tile = egui::Rect::from_min_size(rect.left_center() + vec2(10.0, -17.0), vec2(34.0, 34.0));
    painter.rect_filled(tile, 9.0, if selected { p.accent } else { edge(p, 18) });
    let icon_color = if selected { p.on_accent } else { p.text };
    painter.text(tile.center(), Align2::CENTER_CENTER, icon, FontId::proportional(18.0), icon_color);
    let text_x = tile.right() + 10.0;
    let dim = if ui.is_enabled() { p.text_dim } else { p.text_faint };
    let title_color = if ui.is_enabled() { p.text } else { p.text_faint };
    painter.text(
        egui::pos2(text_x, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        title,
        theme::semibold(13.5),
        title_color,
    );
    painter.text(
        egui::pos2(text_x, rect.center().y + 9.0),
        Align2::LEFT_CENTER,
        caption,
        FontId::proportional(11.5),
        dim,
    );
    if selected {
        painter.text(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            Align2::RIGHT_CENTER,
            icons::CHECK_CIRCLE,
            FontId::proportional(18.0),
            p.accent,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Checkbox / radio body shared by `check` and `radio`: a 16 px box (circle for radios), accent-filled
/// with a glyph when selected, plus an optional clickable label.
fn choice(ui: &mut Ui, selected: bool, label: &str, is_radio: bool) -> Response {
    const BOX: f32 = 16.0;
    let p = theme::current(ui.ctx());
    let enabled = ui.is_enabled();
    let text_color = if enabled { p.text } else { p.text_faint };
    let galley = (!label.is_empty())
        .then(|| ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.5), text_color));
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x + 8.0);
    let h = ui.spacing().interact_size.y.max(BOX);
    let (rect, resp) = ui.allocate_exact_size(vec2(BOX + text_w, h), Sense::click());
    let kind = if is_radio { WidgetType::RadioButton } else { WidgetType::Checkbox };
    resp.widget_info(|| WidgetInfo::selected(kind, enabled, selected, label));
    let b = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - BOX / 2.0), vec2(BOX, BOX));
    let radius = if is_radio { BOX / 2.0 } else { 5.0 };
    let (fill, stroke) = if selected {
        (if enabled { p.accent } else { edge(p, 60) }, egui::Stroke::NONE)
    } else {
        (p.bg, egui::Stroke::new(1.0, edge(p, if resp.hovered() && enabled { 120 } else { 80 })))
    };
    let painter = ui.painter();
    painter.rect(b, radius, fill, stroke, egui::StrokeKind::Inside);
    if selected {
        if is_radio {
            painter.circle_filled(b.center(), 3.0, p.on_accent);
        } else {
            painter.text(b.center(), Align2::CENTER_CENTER, icons::CHECK, FontId::proportional(12.0), p.on_accent);
        }
    }
    if resp.has_focus() {
        painter.rect_stroke(b.expand(2.0), radius + 2.0, egui::Stroke::new(1.5, p.accent), egui::StrokeKind::Outside);
    }
    if let Some(g) = galley {
        painter.galley(egui::pos2(b.right() + 8.0, rect.center().y - g.size().y / 2.0), g, text_color);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Checkbox in the app's style (replaces `ui.checkbox`).
pub fn check(ui: &mut Ui, checked: &mut bool, label: impl AsRef<str>) -> Response {
    let mut resp = choice(ui, *checked, label.as_ref(), false);
    if resp.clicked() {
        *checked = !*checked;
        resp.mark_changed();
    }
    resp
}

/// Radio button in the app's style (replaces `ui.radio`).
pub fn radio(ui: &mut Ui, selected: bool, label: impl AsRef<str>) -> Response {
    choice(ui, selected, label.as_ref(), true)
}

/// Radio button bound to `value` (replaces `ui.radio_value`).
pub fn radio_value<T: PartialEq>(ui: &mut Ui, value: &mut T, option: T, label: impl AsRef<str>) -> Response {
    let mut resp = radio(ui, *value == option, label);
    if resp.clicked() && *value != option {
        *value = option;
        resp.mark_changed();
    }
    resp
}

/// Single-line text input in the app's style: roomier padding than egui's default.
pub fn field(text: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).margin(egui::Margin::symmetric(8, 5))
}

/// Centered placeholder for a view with nothing to show yet: icon tile, title, one-line hint.
pub fn empty_state(ui: &mut Ui, icon: &str, title: &str, hint: &str) {
    let p = theme::current(ui.ctx());
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);
        let (rect, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
        ui.painter().rect_filled(rect, 14.0, edge(p, 14));
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(24.0), p.text_dim);
        ui.add_space(8.0);
        ui.label(egui::RichText::new(title).font(theme::semibold(14.0)).color(p.text));
        if !hint.is_empty() {
            ui.label(egui::RichText::new(hint).size(12.0).color(p.text_dim));
        }
    });
}

/// Numeric input in one rounded field: `−  value  +`. The value is still editable and draggable.
/// Returns a response that reports `changed()` when any of the three parts changed `value`.
pub fn stepper(
    ui: &mut Ui,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    suffix: &str,
) -> Response {
    let p = theme::current(ui.ctx());
    let mut changed = false;
    let inner = egui::Frame::NONE
        .fill(p.bg)
        .stroke(egui::Stroke::new(1.0, edge(p, 40)))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(2, 1))
        .show(ui, |ui| {
            // The parent row lays out right-to-left; the stepper always reads − value +.
            // Fixed width (two 28 px buttons + 64 px value): a free layout would stretch the frame.
            ui.allocate_ui_with_layout(vec2(120.0, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if icon_button(ui, icons::MINUS, "Decrease", false).clicked() {
                    *value = (*value - step).max(*range.start());
                    changed = true;
                }
                let drag = egui::DragValue::new(value).range(range.clone()).speed(step).suffix(suffix);
                changed |= ui.add_sized(vec2(64.0, 24.0), drag).changed();
                if icon_button(ui, icons::PLUS, "Increase", false).clicked() {
                    *value = (*value + step).min(*range.end());
                    changed = true;
                }
            });
        });
    let mut resp = inner.response;
    if changed {
        resp.mark_changed();
    }
    resp
}
