//! The diagrams the application can print, as [`Scene`]s.

use std::collections::HashMap;

use egui::Pos2;
use talus_core::model::{Engine, ObjectRef, TableStructure};
use talus_core::model_design::{DesignModel, MTable, ModelKind, TableRole};
use talus_core::print::{Card, CardRow, Fonts, Link, Marker, Scene, diagram_scene};

fn ref_name(ref_table: &str) -> String {
    let last = ref_table.rsplit('.').next().unwrap_or(ref_table);
    last.trim_matches(|c| c == '"' || c == '`').to_string()
}

/// The data model: tables where the canvas has them, types as the model's level shows them.
pub fn model_scene(model: &DesignModel, engine: Engine, fonts: &dyn Fonts) -> Scene {
    let cards: Vec<Card> = model
        .tables
        .iter()
        .map(|t: &MTable| {
            let fk_cols: Vec<&str> = t.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
            Card {
                title: t.name.clone(),
                tag: (t.role != TableRole::None).then(|| t.role.label().to_string()),
                rows: t
                    .columns
                    .iter()
                    .map(|c| CardRow {
                        name: c.name.clone(),
                        ty: match model.kind {
                            ModelKind::Conceptual => String::new(),
                            ModelKind::Logical => c.ty.label(),
                            ModelKind::Physical => c.ty.sql(engine),
                        },
                        marker: if c.primary_key {
                            Marker::Key
                        } else if fk_cols.contains(&c.name.as_str()) {
                            Marker::Link
                        } else {
                            Marker::None
                        },
                    })
                    .collect(),
                x: t.x,
                y: t.y,
            }
        })
        .collect();
    let mut links = Vec::new();
    for (i, t) in model.tables.iter().enumerate() {
        for f in &t.foreign_keys {
            let Some(to) = model.tables.iter().position(|x| x.name.eq_ignore_ascii_case(&f.ref_table)) else {
                continue;
            };
            let row = f
                .columns
                .first()
                .and_then(|c| t.columns.iter().position(|x| x.name.eq_ignore_ascii_case(c)))
                .unwrap_or(0);
            links.push(Link { from: i, row, to });
        }
    }
    let subtitle = match model.kind {
        ModelKind::Physical => format!("{} model · {}", model.kind.label(), engine.label()),
        k => format!("{} model", k.label()),
    };
    let title = if model.name.trim().is_empty() { "Data model".to_string() } else { model.name.clone() };
    diagram_scene(&cards, &links, &title, &subtitle, fonts)
}

/// The ER diagram of a schema, at the positions the canvas shows.
pub fn er_scene(
    model: &[(ObjectRef, TableStructure)],
    positions: &HashMap<String, Pos2>,
    title: &str,
    subtitle: &str,
    fonts: &dyn Fonts,
) -> Scene {
    let placed: Vec<&(ObjectRef, TableStructure)> =
        model.iter().filter(|(o, _)| positions.contains_key(&o.name)).collect();
    let cards: Vec<Card> = placed
        .iter()
        .map(|(o, st)| {
            let fk_cols: Vec<&str> =
                st.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
            let p = positions[&o.name];
            Card {
                title: o.name.clone(),
                tag: None,
                rows: st
                    .columns
                    .iter()
                    .map(|c| CardRow {
                        name: c.name.clone(),
                        ty: crate::ui::diagram_tab::short_type(&c.data_type),
                        marker: if c.primary_key {
                            Marker::Key
                        } else if fk_cols.contains(&c.name.as_str()) {
                            Marker::Link
                        } else {
                            Marker::None
                        },
                    })
                    .collect(),
                x: p.x,
                y: p.y,
            }
        })
        .collect();
    let mut links = Vec::new();
    for (i, (_, st)) in placed.iter().enumerate() {
        for f in &st.foreign_keys {
            let target = ref_name(&f.ref_table);
            let Some(to) = placed.iter().position(|(x, _)| x.name == target) else { continue };
            let row = f.columns.first().and_then(|c| st.columns.iter().position(|x| &x.name == c)).unwrap_or(0);
            links.push(Link { from: i, row, to });
        }
    }
    diagram_scene(&cards, &links, title, subtitle, fonts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print_fonts::PrintFonts;
    use talus_core::model_design::{LType, MColumn, MFk};
    use talus_core::print::Prim;

    fn column(name: &str, pk: bool) -> MColumn {
        MColumn {
            name: name.into(),
            ty: LType::Integer,
            nullable: !pk,
            primary_key: pk,
            auto_increment: false,
            default: None,
        }
    }

    #[test]
    fn a_model_becomes_cards_markers_types_and_relations() {
        let mut orders = MTable::new("orders", 300.0, 0.0);
        orders.columns = vec![column("id", true), column("customer_id", false)];
        orders.foreign_keys.push(MFk {
            name: "fk".into(),
            columns: vec!["customer_id".into()],
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
        });
        let mut customers = MTable::new("customers", 0.0, 0.0);
        customers.columns = vec![column("id", true)];
        let mut model = DesignModel { name: "Shop".into(), kind: ModelKind::Physical, tables: vec![orders, customers] };
        let fonts = PrintFonts::default();
        let s = model_scene(&model, Engine::Postgres, &fonts);
        let texts: Vec<&str> = s
            .prims
            .iter()
            .filter_map(|p| match p {
                Prim::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"orders") && texts.contains(&"customers") && texts.contains(&"customer_id"));
        assert!(texts.contains(&"integer"), "physical model shows the engine's types: {texts:?}");
        assert!(texts.contains(&"◆") && texts.contains(&"→"), "key and relation markers");
        assert_eq!(s.prims.iter().filter(|p| matches!(p, Prim::Line { .. })).count(), 1, "one relation");
        assert_eq!(s.keep_whole.len(), 2);
        assert_eq!((s.title.as_str(), s.subtitle.as_str()), ("Shop", "Physical model · PostgreSQL"));
        // A conceptual model shows names only.
        model.kind = ModelKind::Conceptual;
        let s = model_scene(&model, Engine::Postgres, &fonts);
        assert!(!s.prims.iter().any(|p| matches!(p, Prim::Text { text, .. } if text == "integer")));
    }
}
