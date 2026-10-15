//! PDF 1.4 output of a paginated scene. Text is drawn as the glyph outlines the [`Fonts`] provide, so the file
//! needs no embedded fonts and looks the same everywhere. Page content is Flate-compressed.

use std::fmt::Write as _;
use std::io::Write as _;

use super::font::{Fonts, GlyphOutline, PathOp};
use super::layout::{Layout, PageSetup};
use super::scene::{Align, FontStyle, Prim, Rgb, Scene};

/// What the page header and footer say.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub title: String,
    pub subtitle: String,
    /// Printed in the footer, e.g. `2026-10-12 14:03`.
    pub printed_at: String,
}

/// Bezier circle constant for rounded corners.
const KAPPA: f32 = 0.552_284_8;

struct Content {
    buf: String,
}

fn n(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" || s == "-0" { "0".into() } else { s.to_string() }
}

fn rgb(c: Rgb) -> String {
    format!("{} {} {}", n(f32::from(c.0) / 255.0), n(f32::from(c.1) / 255.0), n(f32::from(c.2) / 255.0))
}

impl Content {
    fn op(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push('\n');
    }

    fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        if r <= 0.0 {
            self.op(&format!("{} {} {} {} re", n(x), n(y), n(w), n(h)));
            return;
        }
        let k = r * KAPPA;
        self.op(&format!("{} {} m", n(x + r), n(y)));
        self.op(&format!("{} {} l", n(x + w - r), n(y)));
        self.op(&format!("{} {} {} {} {} {} c", n(x + w - r + k), n(y), n(x + w), n(y + r - k), n(x + w), n(y + r)));
        self.op(&format!("{} {} l", n(x + w), n(y + h - r)));
        self.op(&format!(
            "{} {} {} {} {} {} c",
            n(x + w),
            n(y + h - r + k),
            n(x + w - r + k),
            n(y + h),
            n(x + w - r),
            n(y + h)
        ));
        self.op(&format!("{} {} l", n(x + r), n(y + h)));
        self.op(&format!("{} {} {} {} {} {} c", n(x + r - k), n(y + h), n(x), n(y + h - r + k), n(x), n(y + h - r)));
        self.op(&format!("{} {} l", n(x), n(y + r)));
        self.op(&format!("{} {} {} {} {} {} c", n(x), n(y + r - k), n(x + r - k), n(y), n(x + r), n(y)));
        self.op("h");
    }

    /// A glyph path in em units, placed with `cm` so that +y in the glyph is up on the page.
    fn glyph(&mut self, g: &GlyphOutline, x: f32, y: f32, size: f32) {
        if g.path.is_empty() {
            return;
        }
        self.op("q");
        self.op(&format!("{} 0 0 {} {} {} cm", n(size), n(-size), n(x), n(y)));
        let mut cur = (0.0f32, 0.0f32);
        for p in &g.path {
            match *p {
                PathOp::Move(x, y) => {
                    self.op(&format!("{} {} m", n(x), n(y)));
                    cur = (x, y);
                }
                PathOp::Line(x, y) => {
                    self.op(&format!("{} {} l", n(x), n(y)));
                    cur = (x, y);
                }
                PathOp::Quad(cx, cy, x, y) => {
                    // A quadratic curve as the equal cubic.
                    let (c1x, c1y) = (cur.0 + 2.0 / 3.0 * (cx - cur.0), cur.1 + 2.0 / 3.0 * (cy - cur.1));
                    let (c2x, c2y) = (x + 2.0 / 3.0 * (cx - x), y + 2.0 / 3.0 * (cy - y));
                    self.op(&format!("{} {} {} {} {} {} c", n(c1x), n(c1y), n(c2x), n(c2y), n(x), n(y)));
                    cur = (x, y);
                }
                PathOp::Cubic(a, b, c, d, x, y) => {
                    self.op(&format!("{} {} {} {} {} {} c", n(a), n(b), n(c), n(d), n(x), n(y)));
                    cur = (x, y);
                }
                PathOp::Close => self.op("h"),
            }
        }
    }

    /// Draws `text` with its baseline at `y`; `x` is the left, right or middle by `align`.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        fonts: &dyn Fonts,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        style: FontStyle,
        color: Rgb,
        align: Align,
    ) {
        let w = fonts.width(text, size, style);
        let mut pen = match align {
            Align::Left => x,
            Align::Right => x - w,
            Align::Center => x - w / 2.0,
        };
        self.op(&format!("{} rg", rgb(color)));
        let bold = style == FontStyle::Semibold;
        for ch in text.chars() {
            if let Some(g) = fonts.glyph(ch, style) {
                self.glyph(&g, pen, y, size);
                if bold {
                    // Thickens the outline: the bundled font has no semibold weight.
                    self.op(&format!("{} RG 0.035 w B", rgb(color)));
                } else {
                    self.op("f");
                }
                self.op("Q");
            }
            pen += fonts.advance(ch, style) * size;
        }
    }

    fn prim(&mut self, p: &Prim, fonts: &dyn Fonts) {
        match p {
            Prim::Rect { x, y, w, h, radius, fill, stroke } => {
                self.op("q");
                if let Some(f) = fill {
                    self.op(&format!("{} rg", rgb(*f)));
                }
                if let Some((c, sw)) = stroke {
                    self.op(&format!("{} RG {} w", rgb(*c), n(*sw)));
                }
                self.rounded_rect(*x, *y, *w, *h, *radius);
                self.op(match (fill.is_some(), stroke.is_some()) {
                    (true, true) => "B",
                    (true, false) => "f",
                    (false, true) => "S",
                    (false, false) => "n",
                });
                self.op("Q");
            }
            Prim::Header { x, y, w, h, radius, fill } => {
                let (r, k) = (radius.min(w / 2.0), radius.min(w / 2.0) * KAPPA);
                self.op("q");
                self.op(&format!("{} rg", rgb(*fill)));
                self.op(&format!("{} {} m", n(*x), n(y + h)));
                self.op(&format!("{} {} l", n(*x), n(y + r)));
                self.op(&format!("{} {} {} {} {} {} c", n(*x), n(y + r - k), n(x + r - k), n(*y), n(x + r), n(*y)));
                self.op(&format!("{} {} l", n(x + w - r), n(*y)));
                self.op(&format!(
                    "{} {} {} {} {} {} c",
                    n(x + w - r + k),
                    n(*y),
                    n(x + w),
                    n(y + r - k),
                    n(x + w),
                    n(y + r)
                ));
                self.op(&format!("{} {} l", n(x + w), n(y + h)));
                self.op("h f");
                self.op("Q");
            }
            Prim::Line { points, color, width } => {
                let mut it = points.iter();
                let Some((x0, y0)) = it.next() else { return };
                self.op("q");
                self.op(&format!("{} RG {} w 1 J 1 j", rgb(*color), n(*width)));
                self.op(&format!("{} {} m", n(*x0), n(*y0)));
                for (x, y) in it {
                    self.op(&format!("{} {} l", n(*x), n(*y)));
                }
                self.op("S");
                self.op("Q");
            }
            Prim::Dot { x, y, r, fill } => {
                let k = r * KAPPA;
                self.op("q");
                self.op(&format!("{} rg", rgb(*fill)));
                self.op(&format!("{} {} m", n(x + r), n(*y)));
                self.op(&format!("{} {} {} {} {} {} c", n(x + r), n(y + k), n(x + k), n(y + r), n(*x), n(y + r)));
                self.op(&format!("{} {} {} {} {} {} c", n(x - k), n(y + r), n(x - r), n(y + k), n(x - r), n(*y)));
                self.op(&format!("{} {} {} {} {} {} c", n(x - r), n(y - k), n(x - k), n(y - r), n(*x), n(y - r)));
                self.op(&format!("{} {} {} {} {} {} c", n(x + k), n(y - r), n(x + r), n(y - k), n(x + r), n(*y)));
                self.op("h f");
                self.op("Q");
            }
            Prim::Text { x, y, text, size, style, color, align } => {
                self.text(fonts, text, *x, *y, *size, *style, *color, *align);
            }
        }
    }
}

/// PDF text string: UTF-16BE in hex, so any language survives in the title.
fn pdf_string(s: &str) -> String {
    let mut out = String::from("<FEFF");
    for u in s.encode_utf16() {
        let _ = write!(out, "{u:04X}");
    }
    out.push('>');
    out
}

fn compress(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// Writes the pages of `layout` (cut from `scene`) as a PDF.
pub fn render_pdf(scene: &Scene, layout: &Layout, setup: &PageSetup, fonts: &dyn Fonts, meta: &Meta) -> Vec<u8> {
    let total = layout.pages.len().max(1);
    let (page_w, page_h) = (layout.page_w, layout.page_h);
    let (cx, cy, cw, ch) = layout.content;
    let ink = Rgb(30, 30, 30);
    let dim = Rgb(120, 120, 120);
    let bounds: Vec<(f32, f32, f32, f32)> = scene.prims.iter().map(|p| p.bounds(fonts)).collect();
    let mut streams: Vec<Vec<u8>> = Vec::new();
    for page in &layout.pages {
        let mut c = Content { buf: String::new() };
        // Top-down coordinates in points from here on.
        c.op(&format!("1 0 0 -1 0 {} cm", n(page_h)));
        if setup.header {
            c.text(fonts, &meta.title, cx, cy - layout.header_h + 12.0, 11.0, FontStyle::Semibold, ink, Align::Left);
            if !meta.subtitle.is_empty() {
                c.text(
                    fonts,
                    &meta.subtitle,
                    cx + cw,
                    cy - layout.header_h + 12.0,
                    8.5,
                    FontStyle::Regular,
                    dim,
                    Align::Right,
                );
            }
            c.op(&format!(
                "0.75 w {} RG {} {} m {} {} l S",
                rgb(Rgb(200, 200, 200)),
                n(cx),
                n(cy - 4.0),
                n(cx + cw),
                n(cy - 4.0)
            ));
        }
        if setup.footer {
            let y = cy + ch + layout.footer_h - 5.0;
            c.op(&format!(
                "0.75 w {} RG {} {} m {} {} l S",
                rgb(Rgb(200, 200, 200)),
                n(cx),
                n(cy + ch + 4.0),
                n(cx + cw),
                n(cy + ch + 4.0)
            ));
            let grid = if layout.cols.len() * layout.rows.len() > 1 {
                format!(
                    "  ·  column {} of {}, row {} of {}",
                    page.col + 1,
                    layout.cols.len(),
                    page.row + 1,
                    layout.rows.len()
                )
            } else {
                String::new()
            };
            c.text(
                fonts,
                &format!("Page {} of {total}{grid}", page.number),
                cx,
                y,
                8.0,
                FontStyle::Regular,
                dim,
                Align::Left,
            );
            c.text(
                fonts,
                &format!("{} · Talus", meta.printed_at),
                cx + cw,
                y,
                8.0,
                FontStyle::Regular,
                dim,
                Align::Right,
            );
        }
        // The scene region of this sheet, clipped to the printable area.
        let (x0, y0, x1, y1) = page.src;
        c.op("q");
        c.op(&format!("{} {} {} {} re W n", n(cx), n(cy), n(cw), n(ch)));
        c.op(&format!(
            "{s} 0 0 {s} {} {} cm",
            n(cx - x0 * layout.scale),
            n(cy - y0 * layout.scale),
            s = n(layout.scale)
        ));
        for (p, b) in scene.prims.iter().zip(&bounds) {
            if b.2 > x0 && b.0 < x1 && b.3 > y0 && b.1 < y1 {
                c.prim(p, fonts);
            }
        }
        c.op("Q");
        streams.push(compress(c.buf.as_bytes()));
    }
    if streams.is_empty() {
        streams.push(compress(b""));
    }

    // Objects: 1 catalog, 2 page tree, 3 info, then (page, contents) pairs.
    let mut out: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut obj = |out: &mut Vec<u8>, body: &[u8]| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", offsets.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    obj(&mut out, b"<< /Type /Catalog /Pages 2 0 R >>");
    let kids: String = (0..streams.len()).map(|i| format!("{} 0 R ", 4 + 2 * i)).collect();
    obj(&mut out, format!("<< /Type /Pages /Kids [ {kids}] /Count {} >>", streams.len()).as_bytes());
    obj(
        &mut out,
        format!(
            "<< /Title {} /Creator {} /Producer {} >>",
            pdf_string(&meta.title),
            pdf_string("Talus"),
            pdf_string("Talus")
        )
        .as_bytes(),
    );
    for (i, data) in streams.iter().enumerate() {
        let (page_obj, content_obj) = (4 + 2 * i, 5 + 2 * i);
        obj(
            &mut out,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << >> /Contents {content_obj} 0 R >>",
                n(page_w),
                n(page_h)
            )
            .as_bytes(),
        );
        let _ = page_obj;
        let mut body = format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        obj(&mut out, &body);
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1)
            .as_bytes(),
    );
    out
}
