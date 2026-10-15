//! A realistic model printed to PDF with the application's own fonts. With `TALUS_SCREENSHOT_DIR` set the
//! files are kept there for a visual check.

use talus_app::print_fonts::PrintFonts;
use talus_app::print_scene::model_scene;
use talus_core::model::Engine;
use talus_core::model_design::{DesignModel, LType, MColumn, MFk, MIndex, MTable, ModelKind, TableRole};
use talus_core::print::{Fit, Meta, PageSetup, Paper, paginate, render_pdf};

fn col(name: &str, ty: LType, pk: bool) -> MColumn {
    MColumn { name: name.into(), ty, nullable: !pk, primary_key: pk, auto_increment: false, default: None }
}

fn fk(name: &str, col: &str, to: &str) -> MFk {
    MFk {
        name: name.into(),
        columns: vec![col.into()],
        ref_table: to.into(),
        ref_columns: vec!["id".into()],
        on_update: "NO ACTION".into(),
        on_delete: "CASCADE".into(),
    }
}

/// A shop with enough tables to need several sheets at actual size, and names in two scripts.
fn shop() -> DesignModel {
    let mut tables = Vec::new();
    for (i, name) in [
        "customers",
        "orders",
        "order_items",
        "products",
        "categories",
        "suppliers",
        "invoices",
        "payments",
        "shipments",
        "Клиенты",
    ]
    .iter()
    .enumerate()
    {
        let mut t = MTable::new(name, (i % 4) as f32 * 300.0, (i / 4) as f32 * 340.0);
        t.columns = vec![col("id", LType::Integer, true)];
        for (c, ty) in [
            ("name", LType::Varchar(80)),
            ("created_at", LType::DateTime),
            ("total", LType::Decimal { precision: 12, scale: 2 }),
            ("note", LType::Text),
        ] {
            t.columns.push(col(c, ty, false));
        }
        match *name {
            "orders" => {
                t.columns.push(col("customer_id", LType::Integer, false));
                t.foreign_keys.push(fk("fk_orders_customer", "customer_id", "customers"));
            }
            "order_items" => {
                t.columns.push(col("order_id", LType::Integer, false));
                t.columns.push(col("product_id", LType::Integer, false));
                t.foreign_keys.push(fk("fk_items_order", "order_id", "orders"));
                t.foreign_keys.push(fk("fk_items_product", "product_id", "products"));
            }
            "products" => {
                t.columns.push(col("category_id", LType::Integer, false));
                t.foreign_keys.push(fk("fk_products_category", "category_id", "categories"));
                t.indexes.push(MIndex { name: "ix_products_name".into(), columns: vec!["name".into()], unique: false });
            }
            "invoices" => t.role = TableRole::None,
            _ => {}
        }
        tables.push(t);
    }
    DesignModel { name: "Shop".into(), kind: ModelKind::Physical, tables }
}

#[test]
fn a_model_prints_to_a_well_formed_multi_sheet_pdf() {
    let fonts = PrintFonts::default();
    let scene = model_scene(&shop(), Engine::Postgres, &fonts);
    assert_eq!(scene.keep_whole.len(), 10);
    let meta = Meta {
        title: "Shop".into(),
        subtitle: "Physical model · PostgreSQL".into(),
        printed_at: "2026-10-12 14:03".into(),
    };
    let mut out = Vec::new();
    for (name, setup) in [
        ("fit", PageSetup { landscape: true, ..PageSetup::default() }),
        ("actual", PageSetup { fit: Fit::Percent(100.0), ..PageSetup::default() }),
        (
            "a3-grid",
            PageSetup {
                paper: Paper::A3,
                landscape: true,
                fit: Fit::Pages { cols: 2, rows: 1 },
                ..PageSetup::default()
            },
        ),
    ] {
        let layout = paginate(&scene, &setup, &fonts);
        assert!(!layout.pages.is_empty(), "{name}");
        let pdf = render_pdf(&scene, &layout, &setup, &fonts, &meta);
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF-1.4") && text.contains(&format!("/Count {}", layout.pages.len())), "{name}");
        assert!(pdf.len() > 4000, "{name}: {} bytes", pdf.len());
        out.push((name, layout, pdf));
    }
    // Actual size is several sheets; fit-to-width never needs more than a couple.
    assert!(out[0].1.pages.len() <= 2);
    assert!(out[1].1.pages.len() >= 2, "{} sheets", out[1].1.pages.len());
    assert!(out[2].1.pages.len() <= 2);
    assert_eq!(out[1].1.split_cards, 0, "no table is cut: {:?}", out[1].1.cols);
    if let Ok(dir) = std::env::var("TALUS_SCREENSHOT_DIR") {
        for (name, _, pdf) in &out {
            std::fs::write(std::path::Path::new(&dir).join(format!("print-{name}.pdf")), pdf).unwrap();
        }
    }
}
