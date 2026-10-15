//! Cutting a diagram into sheets of paper: paper sizes, margins, scale and where one sheet ends.

use super::font::Fonts;
use super::scene::Scene;

/// A scene unit is a CSS pixel: 96 to the inch, 0.75 point.
pub const PT_PER_UNIT: f32 = 0.75;
const PT_PER_MM: f32 = 72.0 / 25.4;
/// Height reserved for the page header / footer line, in points.
const HEADER_PT: f32 = 22.0;
const FOOTER_PT: f32 = 18.0;
/// The diagram's smallest text is 11 px; printed below this size (points) it is hard to read.
const MIN_LEGIBLE_PT: f32 = 4.5;
const SMALLEST_TEXT_PX: f32 = 11.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Paper {
    A5,
    #[default]
    A4,
    A3,
    Letter,
    Legal,
    Tabloid,
}

impl Paper {
    pub const ALL: [Paper; 6] = [Paper::A5, Paper::A4, Paper::A3, Paper::Letter, Paper::Legal, Paper::Tabloid];

    pub fn label(self) -> &'static str {
        match self {
            Paper::A5 => "A5",
            Paper::A4 => "A4",
            Paper::A3 => "A3",
            Paper::Letter => "Letter",
            Paper::Legal => "Legal",
            Paper::Tabloid => "Tabloid",
        }
    }

    /// `(width, height)` in millimetres, portrait.
    pub fn size_mm(self) -> (f32, f32) {
        match self {
            Paper::A5 => (148.0, 210.0),
            Paper::A4 => (210.0, 297.0),
            Paper::A3 => (297.0, 420.0),
            Paper::Letter => (215.9, 279.4),
            Paper::Legal => (215.9, 355.6),
            Paper::Tabloid => (279.4, 431.8),
        }
    }

    /// The media name CUPS (`lp -o media=`) knows.
    pub fn cups_media(self) -> &'static str {
        match self {
            Paper::A5 => "A5",
            Paper::A4 => "A4",
            Paper::A3 => "A3",
            Paper::Letter => "Letter",
            Paper::Legal => "Legal",
            Paper::Tabloid => "Tabloid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fit {
    /// As wide as one sheet's printable width, but never larger than actual size.
    Width,
    /// Scaled into a grid of this many sheets across and down.
    Pages { cols: u32, rows: u32 },
    /// A fixed scale in percent of actual size (100 = 96 px per inch).
    Percent(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSetup {
    pub paper: Paper,
    pub landscape: bool,
    pub margin_mm: f32,
    pub fit: Fit,
    /// Move sheet boundaries into the gaps between tables where there is room.
    pub keep_whole: bool,
    pub header: bool,
    pub footer: bool,
}

impl Default for PageSetup {
    fn default() -> Self {
        PageSetup {
            paper: Paper::A4,
            landscape: false,
            margin_mm: 12.0,
            fit: Fit::Width,
            keep_whole: true,
            header: true,
            footer: true,
        }
    }
}

/// One sheet: where it lies in the grid and which part of the scene it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSlot {
    pub col: usize,
    pub row: usize,
    /// 1-based, counting only the sheets that print something.
    pub number: usize,
    /// Scene region `(x0, y0, x1, y1)`.
    pub src: (f32, f32, f32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub page_w: f32,
    pub page_h: f32,
    /// Printable area on the sheet, top-down points: `(x, y, w, h)`.
    pub content: (f32, f32, f32, f32),
    pub header_h: f32,
    pub footer_h: f32,
    /// Points per scene unit.
    pub scale: f32,
    pub cols: Vec<(f32, f32)>,
    pub rows: Vec<(f32, f32)>,
    pub pages: Vec<PageSlot>,
    /// Tables a sheet boundary runs through.
    pub split_cards: usize,
    pub warning: Option<String>,
}

impl Layout {
    pub fn percent(&self) -> f32 {
        self.scale / PT_PER_UNIT * 100.0
    }
}

/// Splits `0..total` into stretches of at most `span`. With `ranges` (the extents of boxes along this axis) the
/// boundary moves into the latest gap between boxes that lies in the second half of a stretch, so boxes stay
/// whole when they can; otherwise it falls where the paper ends.
fn cuts(total: f32, span: f32, ranges: &[(f32, f32)], keep: bool) -> Vec<(f32, f32)> {
    // Boxes along the axis, merged where they overlap.
    let mut merged: Vec<(f32, f32)> = Vec::new();
    let mut sorted = ranges.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (a, b) in sorted {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let inside = |c: f32| merged.iter().any(|(a, b)| c > a + 0.01 && c < b - 0.01);
    let mut out = Vec::new();
    let mut start = 0.0f32;
    while start < total - 0.01 {
        let max_end = start + span;
        if max_end >= total - 0.01 {
            out.push((start, total));
            break;
        }
        let mut end = max_end;
        if keep && inside(max_end) {
            let lo = start + span * 0.5;
            let mut best = None;
            let mut gap_start = f32::MIN;
            for (a, b) in merged.iter().copied().chain(std::iter::once((f32::MAX, f32::MAX))) {
                let (g0, g1) = (gap_start.max(lo), a.min(max_end));
                if g1 - g0 >= 2.0 {
                    best = Some((g0 + g1) / 2.0);
                }
                gap_start = b;
            }
            if let Some(c) = best {
                end = c;
            }
        }
        out.push((start, end));
        start = end;
    }
    if out.is_empty() {
        out.push((0.0, total.max(1.0)));
    }
    out
}

/// Fits `scene` onto sheets as `setup` asks.
pub fn paginate(scene: &Scene, setup: &PageSetup, fonts: &dyn Fonts) -> Layout {
    let (mut pw, mut ph) = setup.paper.size_mm();
    if setup.landscape {
        std::mem::swap(&mut pw, &mut ph);
    }
    let (page_w, page_h) = (pw * PT_PER_MM, ph * PT_PER_MM);
    let margin = (setup.margin_mm.clamp(0.0, 50.0)) * PT_PER_MM;
    let header_h = if setup.header { HEADER_PT } else { 0.0 };
    let footer_h = if setup.footer { FOOTER_PT } else { 0.0 };
    let content = (
        margin,
        margin + header_h,
        (page_w - 2.0 * margin).max(36.0),
        (page_h - 2.0 * margin - header_h - footer_h).max(36.0),
    );
    let (sw, sh) = (scene.width.max(1.0), scene.height.max(1.0));
    let scale = match setup.fit {
        Fit::Width => (content.2 / sw).min(PT_PER_UNIT),
        Fit::Pages { cols, rows } => ((cols.max(1) as f32 * content.2) / sw).min((rows.max(1) as f32 * content.3) / sh),
        Fit::Percent(p) => PT_PER_UNIT * p.clamp(5.0, 800.0) / 100.0,
    }
    .max(0.02);
    let xs: Vec<(f32, f32)> = scene.keep_whole.iter().map(|(x, _, w, _)| (*x, x + w)).collect();
    let ys: Vec<(f32, f32)> = scene.keep_whole.iter().map(|(_, y, _, h)| (*y, y + h)).collect();
    let mut scale = scale;
    let (mut cols, mut rows);
    let mut tries = 0;
    loop {
        let (span_x, span_y) = (content.2 / scale, content.3 / scale);
        cols = cuts(sw, span_x, &xs, setup.keep_whole);
        rows = cuts(sh, span_y, &ys, setup.keep_whole);
        // A fixed grid is a promise: smart cuts may need an extra sheet, then the scale gives a little.
        let too_many = matches!(setup.fit, Fit::Pages { cols: c, rows: r } if cols.len() > c.max(1) as usize || rows.len() > r.max(1) as usize);
        tries += 1;
        if !too_many || tries > 60 {
            break;
        }
        scale *= 0.97;
    }
    let bounds: Vec<(f32, f32, f32, f32)> = scene.prims.iter().map(|p| p.bounds(fonts)).collect();
    let mut pages = Vec::new();
    for (r, (y0, y1)) in rows.iter().enumerate() {
        for (c, (x0, x1)) in cols.iter().enumerate() {
            let busy = bounds.iter().any(|(a, b, c2, d)| *c2 > *x0 && *a < *x1 && *d > *y0 && *b < *y1);
            if busy || (pages.is_empty() && r == rows.len() - 1 && c == cols.len() - 1) {
                pages.push(PageSlot { col: c, row: r, number: pages.len() + 1, src: (*x0, *y0, *x1, *y1) });
            }
        }
    }
    let split_cards = scene
        .keep_whole
        .iter()
        .filter(|(x, y, w, h)| {
            cols.iter().skip(1).any(|(a, _)| *a > x + 0.5 && *a < x + w - 0.5)
                || rows.iter().skip(1).any(|(a, _)| *a > y + 0.5 && *a < y + h - 0.5)
        })
        .count();
    let warning = (SMALLEST_TEXT_PX * scale < MIN_LEGIBLE_PT).then(|| {
        format!(
            "At {:.0}% the smallest text is {:.1} pt: use a larger sheet, more pages or a bigger scale",
            scale / PT_PER_UNIT * 100.0,
            SMALLEST_TEXT_PX * scale
        )
    });
    Layout { page_w, page_h, content, header_h, footer_h, scale, cols, rows, pages, split_cards, warning }
}
