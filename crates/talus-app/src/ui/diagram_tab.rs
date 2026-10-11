//! ER diagram of one database/schema: tables with columns and foreign-key links.
//! Layered auto-layout (referenced tables to the left), drag to move, pan, zoom.

use std::collections::{HashMap, HashSet};

use egui::{FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use egui_phosphor::regular as icons;
use talus_core::model::{ObjectRef, TableStructure};
use uuid::Uuid;

use crate::actions::Action;
use crate::state::Loadable;
use crate::theme;
use crate::worker::JobId;

pub(super) const BOX_W: f32 = 210.0;
pub(super) const HEADER_H: f32 = 26.0;
pub(super) const ROW_H: f32 = 18.0;
const GAP_X: f32 = 90.0;
const GAP_Y: f32 = 36.0;

pub type Model = Vec<(ObjectRef, TableStructure)>;

pub struct DiagramTab {
    pub conn: Uuid,
    pub database: String,
    pub schema: Option<String>,
    pub model: Loadable<Model>,
    pub loading: Option<JobId>,
    pub positions: HashMap<String, Pos2>,
    zoom: f32,
    pan: Vec2,
    dragging: Option<String>,
}

pub enum DiagramAction {
    Reload,
    /// Write the diagram as an SVG file.
    ExportSvg(String),
    Nav(Action),
}

/// Table name a foreign key points to, without schema qualifier or quotes.
pub fn ref_name(ref_table: &str) -> String {
    let last = ref_table.rsplit('.').next().unwrap_or(ref_table);
    last.trim_matches(|c| c == '"' || c == '`').to_string()
}

pub(super) fn box_height(st: &TableStructure) -> f32 {
    HEADER_H + ROW_H * st.columns.len().max(1) as f32 + 6.0
}

/// Layered layout: a table's layer is one more than the deepest table it references, so
/// parents sit left of children. Cycles are cut. Deterministic for a given model.
pub fn auto_layout(model: &Model) -> HashMap<String, Pos2> {
    let names: HashSet<&str> = model.iter().map(|(o, _)| o.name.as_str()).collect();
    let refs: HashMap<&str, Vec<String>> = model
        .iter()
        .map(|(o, st)| {
            let r = st
                .foreign_keys
                .iter()
                .map(|f| ref_name(&f.ref_table))
                .filter(|r| r != &o.name && names.contains(r.as_str()))
                .collect();
            (o.name.as_str(), r)
        })
        .collect();
    fn depth<'a>(
        n: &'a str,
        refs: &'a HashMap<&str, Vec<String>>,
        memo: &mut HashMap<String, usize>,
        stack: &mut HashSet<String>,
    ) -> usize {
        if let Some(d) = memo.get(n) {
            return *d;
        }
        if !stack.insert(n.to_string()) {
            return 0; // cycle
        }
        let d = refs.get(n).map_or(0, |rs| rs.iter().map(|r| depth(r, refs, memo, stack) + 1).max().unwrap_or(0));
        stack.remove(n);
        memo.insert(n.to_string(), d);
        d
    }
    let mut memo = HashMap::new();
    let mut layers: Vec<Vec<&(ObjectRef, TableStructure)>> = Vec::new();
    for entry in model {
        let d = depth(&entry.0.name, &refs, &mut memo, &mut HashSet::new());
        if layers.len() <= d {
            layers.resize(d + 1, Vec::new());
        }
        layers[d].push(entry);
    }
    let mut pos = HashMap::new();
    for (li, layer) in layers.iter().enumerate() {
        let mut y = 0.0;
        for (o, st) in layer {
            pos.insert(o.name.clone(), pos2(li as f32 * (BOX_W + GAP_X), y));
            y += box_height(st) + GAP_Y;
        }
    }
    pos
}

impl DiagramTab {
    pub fn new(conn: Uuid, database: String, schema: Option<String>) -> Self {
        Self {
            conn,
            database,
            schema,
            model: Loadable::NotLoaded,
            loading: None,
            positions: HashMap::new(),
            zoom: 1.0,
            pan: vec2(20.0, 20.0),
            dragging: None,
        }
    }

    pub fn title(&self) -> String {
        format!("ER — {}", self.schema.as_deref().unwrap_or(&self.database))
    }

    pub fn set_model(&mut self, model: Model) {
        let auto = auto_layout(&model);
        // Keep positions the user arranged for tables that still exist.
        for (k, v) in auto {
            self.positions.entry(k).or_insert(v);
        }
        let names: HashSet<String> = model.iter().map(|(o, _)| o.name.clone()).collect();
        self.positions.retain(|k, _| names.contains(k));
        self.model = Loadable::Loaded(model);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tab_id: u64) -> Vec<DiagramAction> {
        let p = theme::current(ui.ctx());
        let mut out = Vec::new();
        if matches!(self.model, Loadable::NotLoaded) && self.loading.is_none() {
            out.push(DiagramAction::Reload);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.loading.is_none(), egui::Button::new(format!("{} Refresh", icons::ARROW_CLOCKWISE)))
                .clicked()
            {
                out.push(DiagramAction::Reload);
            }
            if ui.button(format!("{} Auto Layout", icons::TREE_STRUCTURE)).clicked()
                && let Some(m) = self.model.loaded()
            {
                self.positions = auto_layout(m);
                self.pan = vec2(20.0, 20.0);
            }
            if ui.button(format!("{} Export SVG", icons::EXPORT)).clicked()
                && let Some(m) = self.model.loaded()
            {
                out.push(DiagramAction::ExportSvg(super::diagram_svg::to_svg(m, &self.positions)));
            }
            ui.separator();
            if ui.button(icons::MAGNIFYING_GLASS_MINUS).on_hover_text("Zoom out").clicked() {
                self.zoom = (self.zoom / 1.2).max(0.3);
            }
            if ui.button(format!("{:.0}%", self.zoom * 100.0)).on_hover_text("Reset zoom").clicked() {
                self.zoom = 1.0;
            }
            if ui.button(icons::MAGNIFYING_GLASS_PLUS).on_hover_text("Zoom in").clicked() {
                self.zoom = (self.zoom * 1.2).min(2.5);
            }
            ui.label(
                RichText::new(
                    "Drag tables to arrange · drag background to pan · ⌘ + scroll to zoom · double-click to open",
                )
                .small()
                .color(p.text_dim),
            );
            if self.loading.is_some() {
                ui.spinner();
            }
        });
        ui.separator();
        let model = match &self.model {
            Loadable::Loaded(m) => m.clone(),
            Loadable::Failed(e) => {
                ui.label(RichText::new(e).color(p.danger));
                return out;
            }
            _ => {
                ui.spinner();
                return out;
            }
        };
        if model.is_empty() {
            ui.label(RichText::new("No tables in this schema.").color(p.text_dim));
            return out;
        }
        let (canvas, bg) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 0.0, p.bg);
        // Zoom with ⌘ + scroll around the pointer.
        if bg.hovered() {
            let (scroll, cmd, hover) =
                ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.command, i.pointer.hover_pos()));
            if cmd && scroll != 0.0 {
                let old = self.zoom;
                self.zoom = (self.zoom * (1.0 + scroll * 0.002)).clamp(0.3, 2.5);
                if let Some(h) = hover {
                    let world = (h - canvas.min - self.pan) / old;
                    self.pan = h - canvas.min - world * self.zoom;
                }
            }
        }
        let z = self.zoom;
        let to_screen = |w: Pos2, pan: Vec2| canvas.min + pan + w.to_vec2() * z;
        let rects: HashMap<String, Rect> = model
            .iter()
            .filter_map(|(o, st)| {
                self.positions.get(&o.name).map(|pp| {
                    (o.name.clone(), Rect::from_min_size(to_screen(*pp, self.pan), vec2(BOX_W, box_height(st)) * z))
                })
            })
            .collect();
        // Edges first (under the boxes).
        let edge = Stroke::new(1.5 * z.max(0.6), p.accent.gamma_multiply(0.8));
        for (o, st) in &model {
            let Some(src) = rects.get(&o.name) else { continue };
            for fk in &st.foreign_keys {
                let target = ref_name(&fk.ref_table);
                let Some(dst) = rects.get(&target) else { continue };
                let row = fk.columns.first().and_then(|c| st.columns.iter().position(|x| &x.name == c)).unwrap_or(0);
                let y = src.top() + (HEADER_H + ROW_H * (row as f32 + 0.5)) * z;
                let (from, to) = if dst.center().x < src.center().x {
                    (pos2(src.left(), y), pos2(dst.right(), dst.top() + HEADER_H * 0.5 * z))
                } else {
                    (pos2(src.right(), y), pos2(dst.left(), dst.top() + HEADER_H * 0.5 * z))
                };
                let mid = (from.x + to.x) / 2.0;
                painter.line(vec![from, pos2(mid, from.y), pos2(mid, to.y), to], edge);
                painter.circle_filled(to, 3.5 * z, p.accent);
            }
        }
        let header_font = FontId::proportional(13.0 * z);
        let row_font = FontId::monospace(11.5 * z);
        let mut drag_target = None;
        for (o, st) in &model {
            let Some(r) = rects.get(&o.name).copied() else { continue };
            let resp = ui.interact(r, egui::Id::new(("er-box", tab_id, &o.name)), Sense::click_and_drag());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("table {}", o.name)));
            if resp.drag_started() {
                drag_target = Some(o.name.clone());
            }
            if resp.double_clicked() {
                out.push(DiagramAction::Nav(Action::OpenData { conn: self.conn, obj: o.clone() }));
            }
            let active = resp.hovered() || self.dragging.as_deref() == Some(&o.name);
            super::widgets::table_card(&painter, r, active);
            let header = Rect::from_min_size(r.min, vec2(r.width(), HEADER_H * z));
            painter.text(
                header.left_center() + vec2(8.0 * z, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("{}  {}", icons::TABLE, o.name),
                header_font.clone(),
                p.text,
            );
            let fk_cols: HashSet<&str> =
                st.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
            for (i, c) in st.columns.iter().enumerate() {
                let y = r.top() + (HEADER_H + ROW_H * (i as f32 + 0.5)) * z;
                let (marker, color) = if c.primary_key {
                    (icons::KEY, p.warn)
                } else if fk_cols.contains(c.name.as_str()) {
                    (icons::LINK, p.accent)
                } else {
                    ("", p.text_dim)
                };
                if !marker.is_empty() {
                    painter.text(
                        pos2(r.left() + 8.0 * z, y),
                        egui::Align2::LEFT_CENTER,
                        marker,
                        row_font.clone(),
                        color,
                    );
                }
                painter.text(
                    pos2(r.left() + 24.0 * z, y),
                    egui::Align2::LEFT_CENTER,
                    &c.name,
                    row_font.clone(),
                    p.text,
                );
                painter.text(
                    pos2(r.right() - 8.0 * z, y),
                    egui::Align2::RIGHT_CENTER,
                    short_type(&c.data_type),
                    row_font.clone(),
                    p.text_dim,
                );
            }
            if resp.dragged() && self.dragging.as_deref() == Some(&o.name) {
                let d = resp.drag_delta() / z;
                if let Some(pp) = self.positions.get_mut(&o.name) {
                    *pp += d;
                }
            }
            if resp.drag_stopped() {
                self.dragging = None;
            }
        }
        if let Some(t) = drag_target {
            self.dragging = Some(t);
        }
        if bg.dragged() && self.dragging.is_none() {
            self.pan += bg.drag_delta();
        }
        out
    }
}

pub(super) fn short_type(t: &str) -> String {
    let t = t.replace("character varying", "varchar").replace("timestamp without time zone", "timestamp");
    if t.chars().count() > 16 { format!("{}…", t.chars().take(15).collect::<String>()) } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;
    use talus_core::model::{ColumnInfo, ForeignKeyInfo, ObjectKind};

    fn table(name: &str, refs: &[&str]) -> (ObjectRef, TableStructure) {
        let col = |n: &str| ColumnInfo {
            name: n.into(),
            data_type: "int".into(),
            nullable: true,
            default: None,
            primary_key: false,
            comment: None,
            extra: None,
        };
        let st = TableStructure {
            columns: vec![col("id"), col("ref_id")],
            indexes: vec![],
            foreign_keys: refs
                .iter()
                .map(|r| ForeignKeyInfo {
                    name: format!("fk_{r}"),
                    columns: vec!["ref_id".into()],
                    ref_table: format!("public.\"{r}\""),
                    ref_columns: vec!["id".into()],
                    on_update: String::new(),
                    on_delete: String::new(),
                })
                .collect(),
        };
        (
            ObjectRef {
                database: "d".into(),
                schema: Some("public".into()),
                name: name.into(),
                kind: ObjectKind::Table,
            },
            st,
        )
    }

    #[test]
    fn layers_follow_foreign_keys_and_survive_cycles() {
        let model = vec![
            table("order_items", &["orders", "products"]),
            table("orders", &["customers"]),
            table("customers", &[]),
            table("products", &[]),
            table("a", &["b"]),
            table("b", &["a"]),
        ];
        let pos = auto_layout(&model);
        assert_eq!(pos.len(), 6);
        assert_eq!(pos["customers"].x, 0.0);
        assert_eq!(pos["products"].x, 0.0);
        assert!(pos["orders"].x > pos["customers"].x);
        assert!(pos["order_items"].x > pos["orders"].x);
        // Tables in the same layer do not overlap.
        assert!(pos["products"].y >= pos["customers"].y + box_height(&model[2].1));
        assert_eq!(ref_name("public.\"orders\""), "orders");
        assert_eq!(ref_name("`shop`.`t`"), "t");
    }
}
