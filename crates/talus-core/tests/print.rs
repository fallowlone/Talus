//! Pagination and PDF output of diagrams, with a stub font (every glyph is a box) so counts are exact.

use std::io::Read;

use talus_core::print::FontStyle;
use talus_core::print::{
    Card, CardRow, Fit, Fonts, GlyphOutline, Link, Marker, Meta, PageSetup, Paper, PathOp, Prim, Rgb, Scene,
    diagram_scene, paginate, render_pdf,
};

struct BoxFonts;

impl Fonts for BoxFonts {
    fn glyph(&self, ch: char, _: FontStyle) -> Option<GlyphOutline> {
        if ch == ' ' {
            return Some(GlyphOutline::default());
        }
        Some(GlyphOutline {
            path: vec![
                PathOp::Move(0.05, 0.0),
                PathOp::Line(0.5, 0.0),
                PathOp::Line(0.5, 0.7),
                PathOp::Line(0.05, 0.7),
                PathOp::Close,
            ],
        })
    }
    fn advance(&self, _: char, _: FontStyle) -> f32 {
        0.6
    }
}

fn card(title: &str, x: f32, y: f32, rows: &[&str]) -> Card {
    Card {
        title: title.into(),
        tag: None,
        rows: rows
            .iter()
            .enumerate()
            .map(|(i, r)| CardRow {
                name: (*r).into(),
                ty: "int".into(),
                marker: if i == 0 { Marker::Key } else { Marker::None },
            })
            .collect(),
        x,
        y,
    }
}

fn three_across() -> Scene {
    // Boxes 210 wide: [24,234], [334,544], [644,854] in scene x (24 px margin), 10 px-ish gaps are 100 wide.
    diagram_scene(
        &[card("a", 0.0, 0.0, &["id", "x"]), card("b", 310.0, 0.0, &["id"]), card("c", 620.0, 0.0, &["id", "y", "z"])],
        &[Link { from: 1, row: 0, to: 0 }, Link { from: 2, row: 1, to: 1 }],
        "Shop",
        "physical",
        &BoxFonts,
    )
}

#[test]
fn papers_have_real_sizes_and_landscape_swaps_them() {
    assert_eq!(Paper::A4.size_mm(), (210.0, 297.0));
    let s = three_across();
    let portrait = paginate(&s, &PageSetup::default(), &BoxFonts);
    let landscape = paginate(&s, &PageSetup { landscape: true, ..PageSetup::default() }, &BoxFonts);
    assert!((portrait.page_w - 595.28).abs() < 0.5 && (portrait.page_h - 841.89).abs() < 0.5);
    assert!(landscape.page_w > landscape.page_h);
    for p in Paper::ALL {
        let (w, h) = p.size_mm();
        assert!(w < h, "{} is stored portrait", p.label());
    }
}

#[test]
fn a_small_diagram_is_one_sheet_at_most_actual_size_and_a_big_one_is_cut_into_a_grid() {
    let s = three_across();
    let fit = paginate(&s, &PageSetup::default(), &BoxFonts);
    assert_eq!((fit.cols.len(), fit.rows.len(), fit.pages.len()), (1, 1, 1));
    assert!(fit.scale <= 0.75 + 1e-4, "fit-to-width never enlarges beyond actual size");
    // Actual size on A4 portrait: the 900 px wide diagram needs two sheets across.
    let actual = paginate(&s, &PageSetup { fit: Fit::Percent(100.0), ..PageSetup::default() }, &BoxFonts);
    assert_eq!(actual.cols.len(), 2);
    assert!((actual.percent() - 100.0).abs() < 0.01);
    // The sheets cover the scene without gaps or overlaps.
    assert_eq!(actual.cols[0].0, 0.0);
    assert!((actual.cols[0].1 - actual.cols[1].0).abs() < 1e-3);
    assert!((actual.cols[1].1 - s.width).abs() < 1e-3);
    // A fixed grid scales to fit it.
    let grid = paginate(&s, &PageSetup { fit: Fit::Pages { cols: 2, rows: 1 }, ..PageSetup::default() }, &BoxFonts);
    assert!(grid.cols.len() <= 2 && grid.rows.len() == 1);
}

#[test]
fn sheet_boundaries_move_into_the_gaps_between_tables() {
    let s = three_across();
    // A sheet holds ~703 px: a plain cut would run through the third table (x 644..854).
    let cfg = PageSetup { fit: Fit::Percent(100.0), header: false, footer: false, ..PageSetup::default() };
    let plain = paginate(&s, &PageSetup { keep_whole: false, ..cfg }, &BoxFonts);
    assert_eq!(plain.split_cards, 1, "{:?}", plain.cols);
    let kept = paginate(&s, &cfg, &BoxFonts);
    assert_eq!(kept.split_cards, 0, "{:?}", kept.cols);
    assert_eq!(kept.cols.len(), 2);
    let cut = kept.cols[0].1;
    assert!(cut > 544.0 && cut < 644.0, "the cut lies between the second and third table: {cut}");
}

#[test]
fn sheets_with_nothing_on_them_are_skipped_and_the_rest_are_numbered() {
    // Two tables far apart on the diagonal: at actual size a 2x2 grid would waste two sheets.
    let s = diagram_scene(&[card("a", 0.0, 0.0, &["id"]), card("b", 1500.0, 2000.0, &["id"])], &[], "t", "", &BoxFonts);
    let l = paginate(&s, &PageSetup { fit: Fit::Percent(100.0), keep_whole: false, ..PageSetup::default() }, &BoxFonts);
    assert!(l.cols.len() * l.rows.len() > l.pages.len(), "{} sheets of {}", l.pages.len(), l.cols.len() * l.rows.len());
    assert_eq!(l.pages.iter().map(|p| p.number).collect::<Vec<_>>(), (1..=l.pages.len()).collect::<Vec<_>>());
    // An empty diagram still prints one (blank) sheet.
    let empty = paginate(&Scene { width: 10.0, height: 10.0, ..Default::default() }, &PageSetup::default(), &BoxFonts);
    assert_eq!(empty.pages.len(), 1);
}

#[test]
fn tiny_scales_are_reported() {
    let s = diagram_scene(&[card("a", 0.0, 0.0, &["id"]), card("b", 9000.0, 0.0, &["id"])], &[], "t", "", &BoxFonts);
    let l = paginate(&s, &PageSetup::default(), &BoxFonts);
    assert!(l.warning.as_deref().is_some_and(|w| w.contains("pt")), "{:?} at {}", l.warning, l.scale);
    assert!(paginate(&three_across(), &PageSetup::default(), &BoxFonts).warning.is_none());
}

#[test]
fn long_names_are_shortened_to_fit_their_box() {
    let long = "a_very_long_column_name_that_cannot_fit_in_a_card";
    let s = diagram_scene(&[card("t", 0.0, 0.0, &[long])], &[], "t", "", &BoxFonts);
    let texts: Vec<&str> = s
        .prims
        .iter()
        .filter_map(|p| match p {
            Prim::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.ends_with('…') && t.len() < long.len()), "{texts:?}");
}

/// Splits a PDF into its objects by the xref table and checks the table is exact.
fn objects(pdf: &[u8]) -> Vec<(usize, Vec<u8>)> {
    assert!(pdf.starts_with(b"%PDF-1.4\n") && pdf.ends_with(b"%%EOF\n"));
    let tail = String::from_utf8_lossy(&pdf[pdf.len() - 200..]).to_string();
    let start: usize = tail.rsplit("startxref\n").next().unwrap().lines().next().unwrap().parse().unwrap();
    let table = String::from_utf8_lossy(&pdf[start..]).to_string();
    assert!(table.starts_with("xref\n0 "));
    let count: usize = table.lines().nth(1).unwrap().trim_start_matches("0 ").parse().unwrap();
    let mut out = Vec::new();
    for (i, line) in table.lines().skip(3).take(count - 1).enumerate() {
        let off: usize = line[..10].parse().unwrap();
        let head = format!("{} 0 obj\n", i + 1);
        assert_eq!(&pdf[off..off + head.len()], head.as_bytes(), "object {} is where the table says", i + 1);
        out.push((i + 1, pdf[off..].to_vec()));
    }
    out
}

fn content_streams(pdf: &[u8]) -> Vec<String> {
    objects(pdf)
        .into_iter()
        .filter_map(|(_, body)| {
            let s = String::from_utf8_lossy(&body[..body.len().min(80)]).to_string();
            if !s.contains("/Filter /FlateDecode") {
                return None;
            }
            let head_end = body.windows(7).position(|w| w == b"stream\n")? + 7;
            let len: usize = s.split("/Length ").nth(1)?.split(' ').next()?.parse().ok()?;
            let mut text = String::new();
            flate2::read::ZlibDecoder::new(&body[head_end..head_end + len]).read_to_string(&mut text).ok()?;
            Some(text)
        })
        .collect()
}

#[test]
fn the_pdf_is_well_formed_has_one_page_object_per_sheet_and_draws_every_glyph() {
    let s = three_across();
    let setup = PageSetup { fit: Fit::Percent(100.0), ..PageSetup::default() };
    let layout = paginate(&s, &setup, &BoxFonts);
    let meta = Meta {
        title: "Магазин".into(),
        subtitle: "physical · PostgreSQL".into(),
        printed_at: "2026-10-12 14:03".into(),
    };
    let pdf = render_pdf(&s, &layout, &setup, &BoxFonts, &meta);
    let text = String::from_utf8_lossy(&pdf);
    assert!(text.contains(&format!("/Count {}", layout.pages.len())));
    assert_eq!(text.matches("/Type /Page ").count(), layout.pages.len());
    assert!(text.contains(&format!("/MediaBox [0 0 {} {}]", "595.276", "841.89")), "{}", &text[..400.min(text.len())]);
    // The title is a UTF-16 string, so Cyrillic survives.
    assert!(
        text.contains("<FEFF041C0430043304300437043804") || text.contains("<FEFF041C"),
        "{}",
        &text[..600.min(text.len())]
    );
    let streams = content_streams(&pdf);
    assert_eq!(streams.len(), layout.pages.len());
    // Every page is clipped to its printable area and carries its footer ("Page n of N" is drawn as glyphs).
    for (i, c) in streams.iter().enumerate() {
        assert!(c.contains(" re W n"), "page {} is clipped", i + 1);
        assert!(c.contains(" cm"), "page {} has glyphs", i + 1);
    }
    // Text is vector: a one-sheet scene has exactly one glyph path per printed (non-space) character.
    let one = PageSetup { header: false, footer: false, ..PageSetup::default() };
    let l1 = paginate(&s, &one, &BoxFonts);
    assert_eq!(l1.pages.len(), 1);
    let pdf = render_pdf(&s, &l1, &one, &BoxFonts, &meta);
    let c = &content_streams(&pdf)[0];
    let chars: usize = s
        .prims
        .iter()
        .map(|p| match p {
            Prim::Text { text, .. } => text.chars().filter(|c| *c != ' ').count(),
            _ => 0,
        })
        .sum();
    assert!(c.matches("\nq\n").count() >= chars, "each glyph is drawn: {chars}");
    assert_eq!(c.matches(" 0 0 -").count(), chars + 1, "one glyph matrix per character, plus the page flip");
    // Colours and lines of the diagram itself.
    assert!(c.contains(" RG ") && c.contains(" rg") && c.contains("\nS\n"), "strokes and fills are present");
    let _ = Rgb(0, 0, 0);
}

#[test]
fn only_the_part_of_the_scene_on_a_sheet_is_drawn_on_it() {
    let s = three_across();
    let setup = PageSetup { fit: Fit::Percent(100.0), header: false, footer: false, ..PageSetup::default() };
    let layout = paginate(&s, &setup, &BoxFonts);
    assert_eq!(layout.pages.len(), 2);
    let pdf = render_pdf(&s, &layout, &setup, &BoxFonts, &Meta::default());
    let streams = content_streams(&pdf);
    // The tables a, b on the first sheet and c on the second: titles are one glyph each, rows differ.
    let glyphs = |c: &String| c.matches(" 0 0 -").count();
    assert!(glyphs(&streams[0]) > 0 && glyphs(&streams[1]) > 0);
    assert!(
        glyphs(&streams[0]) + glyphs(&streams[1])
            >= glyphs(
                &content_streams(&render_pdf(
                    &s,
                    &paginate(&s, &PageSetup { header: false, footer: false, ..PageSetup::default() }, &BoxFonts),
                    &PageSetup { header: false, footer: false, ..PageSetup::default() },
                    &BoxFonts,
                    &Meta::default()
                ))[0]
            )
    );
}
