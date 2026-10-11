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
