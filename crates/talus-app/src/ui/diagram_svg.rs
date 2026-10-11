//! SVG rendering of the ER diagram: same geometry as the on-screen canvas, light palette so it
//! prints and embeds cleanly.

use std::collections::HashMap;
use std::fmt::Write;

use egui::Pos2;

use super::diagram_tab::{BOX_W, HEADER_H, Model, ROW_H, box_height, ref_name, short_type};

const MARGIN: f32 = 24.0;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Renders `model` at `positions` (world coordinates). Tables without a position are skipped.
pub fn to_svg(model: &Model, positions: &HashMap<String, Pos2>) -> String {
    let placed: Vec<_> = model.iter().filter(|(o, _)| positions.contains_key(&o.name)).collect();
    let (min_x, min_y) = placed.iter().fold((f32::MAX, f32::MAX), |(x, y), (o, _)| {
        let p = positions[&o.name];
        (x.min(p.x), y.min(p.y))
    });
    let (min_x, min_y) = if placed.is_empty() { (0.0, 0.0) } else { (min_x, min_y) };
    let at = |p: Pos2| (p.x - min_x + MARGIN, p.y - min_y + MARGIN);
    let (mut w, mut h) = (MARGIN * 2.0, MARGIN * 2.0);
    for (o, st) in &placed {
        let (x, y) = at(positions[&o.name]);
        w = w.max(x + BOX_W + MARGIN);
        h = h.max(y + box_height(st) + MARGIN);
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.0}\" height=\"{h:.0}\" viewBox=\"0 0 {w:.0} {h:.0}\" font-family=\"-apple-system, Helvetica, Arial, sans-serif\">"
    );
    let _ = writeln!(s, "<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>");
    // Foreign keys under the boxes.
    for (o, st) in &placed {
        let (sx, sy) = at(positions[&o.name]);
        for fk in &st.foreign_keys {
            let target = ref_name(&fk.ref_table);
            let Some(dp) = positions.get(&target) else { continue };
            let (dx, dy) = at(*dp);
            let row = fk.columns.first().and_then(|c| st.columns.iter().position(|x| &x.name == c)).unwrap_or(0);
            let y = sy + HEADER_H + ROW_H * (row as f32 + 0.5);
            let ty = dy + HEADER_H * 0.5;
            let (fx, tx) = if dx < sx { (sx, dx + BOX_W) } else { (sx + BOX_W, dx) };
            let mid = (fx + tx) / 2.0;
            let _ = writeln!(
                s,
                "<polyline points=\"{fx:.1},{y:.1} {mid:.1},{y:.1} {mid:.1},{ty:.1} {tx:.1},{ty:.1}\" fill=\"none\" stroke=\"#555\" stroke-width=\"1.4\"/><circle cx=\"{tx:.1}\" cy=\"{ty:.1}\" r=\"3.5\" fill=\"#111\"/>"
            );
        }
    }
    for (o, st) in &placed {
        let (x, y) = at(positions[&o.name]);
        let bh = box_height(st);
        let _ = writeln!(
            s,
            "<g><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{BOX_W}\" height=\"{bh:.1}\" rx=\"6\" fill=\"#fff\" stroke=\"#222\" stroke-width=\"1\"/><path d=\"M{x:.1},{:.1} v-{:.1} a6,6 0 0 1 6,-6 h{:.1} a6,6 0 0 1 6,6 v{:.1} z\" fill=\"#111\"/><text x=\"{:.1}\" y=\"{:.1}\" font-size=\"13\" font-weight=\"600\" fill=\"#fff\">{}</text>",
            y + HEADER_H,
            HEADER_H - 6.0,
            BOX_W - 12.0,
            HEADER_H - 6.0,
            x + 10.0,
            y + HEADER_H * 0.68,
            esc(&o.name)
        );
        for (i, c) in st.columns.iter().enumerate() {
            let cy = y + HEADER_H + ROW_H * (i as f32 + 0.5) + 4.0;
            let key = if c.primary_key { "◆ " } else { "" };
            let _ = writeln!(
                s,
                "<text x=\"{:.1}\" y=\"{cy:.1}\" font-size=\"11.5\" font-family=\"Menlo, monospace\" fill=\"#111\">{key}{}</text><text x=\"{:.1}\" y=\"{cy:.1}\" font-size=\"11\" font-family=\"Menlo, monospace\" fill=\"#777\" text-anchor=\"end\">{}</text>",
                x + 10.0,
                esc(&c.name),
                x + BOX_W - 8.0,
                esc(&short_type(&c.data_type))
            );
        }
        s.push_str("</g>\n");
    }
    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use talus_core::model::{ColumnInfo, ForeignKeyInfo, ObjectKind, ObjectRef, TableStructure};

    fn table(name: &str, fk_to: Option<&str>) -> (ObjectRef, TableStructure) {
        let col = |n: &str, pk: bool| ColumnInfo {
            name: n.into(),
            data_type: "integer".into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            comment: None,
            extra: None,
        };
        (
            ObjectRef { database: "d".into(), schema: None, name: name.into(), kind: ObjectKind::Table },
            TableStructure {
                columns: vec![col("id", true), col("<ref>", false)],
                foreign_keys: fk_to
                    .map(|t| ForeignKeyInfo {
                        name: "fk".into(),
                        columns: vec!["<ref>".into()],
                        ref_table: t.into(),
                        ref_columns: vec!["id".into()],
                        on_update: "NO ACTION".into(),
                        on_delete: "NO ACTION".into(),
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn svg_has_one_box_per_table_one_edge_per_fk_and_escapes_names() {
        let model = vec![table("parent", None), table("child", Some("parent"))];
        let pos = super::super::diagram_tab::auto_layout(&model);
        let svg = to_svg(&model, &pos);
        assert!(svg.starts_with("<svg ") && svg.trim_end().ends_with("</svg>"));
        assert_eq!(svg.matches("<g>").count(), 2);
        assert_eq!(svg.matches("<polyline").count(), 1);
        assert!(svg.contains("&lt;ref&gt;") && !svg.contains("<ref>"));
        assert!(to_svg(&Vec::new(), &HashMap::new()).contains("<svg "));
    }
}
