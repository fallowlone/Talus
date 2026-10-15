//! Glyph outlines for printing, from the fonts the application ships (Ubuntu for text, Hack for code,
//! Noto Emoji for symbols they lack), so the PDF needs no installed fonts and matches the screen.

use ab_glyph::{Font, FontRef, GlyphId, OutlineCurve, Point};
use talus_core::print::{FontStyle, Fonts, GlyphOutline, PathOp};

pub struct PrintFonts {
    text: FontRef<'static>,
    mono: FontRef<'static>,
    symbols: FontRef<'static>,
}

impl Default for PrintFonts {
    fn default() -> Self {
        // The bundled fonts are valid: a parse failure would be a broken build, not a runtime condition.
        let load = |bytes: &'static [u8]| FontRef::try_from_slice(bytes).expect("bundled font parses");
        Self {
            text: load(epaint_default_fonts::UBUNTU_LIGHT),
            mono: load(epaint_default_fonts::HACK_REGULAR),
            symbols: load(epaint_default_fonts::NOTO_EMOJI_REGULAR),
        }
    }
}

impl PrintFonts {
    fn primary(&self, style: FontStyle) -> &FontRef<'static> {
        if style == FontStyle::Mono { &self.mono } else { &self.text }
    }

    /// The font that has `ch`, with its glyph id: the style's own, then the text font, Hack (arrows, geometric
    /// shapes) and last the emoji font.
    fn find(&self, ch: char, style: FontStyle) -> Option<(&FontRef<'static>, GlyphId)> {
        [self.primary(style), &self.text, &self.mono, &self.symbols].into_iter().find_map(|f| {
            let id = f.glyph_id(ch);
            (id.0 != 0).then_some((f, id))
        })
    }
}

fn em(f: &FontRef<'static>) -> f32 {
    f.units_per_em().unwrap_or(1000.0)
}

impl Fonts for PrintFonts {
    fn glyph(&self, ch: char, style: FontStyle) -> Option<GlyphOutline> {
        if ch.is_whitespace() {
            return Some(GlyphOutline::default());
        }
        let (font, id) = self.find(ch, style)?;
        let upem = em(font);
        let outline = font.outline(id)?;
        // Font units with y up (the bounds say so: `H` reaches y = 693 of 1000), normalised to em.
        let pt = |p: Point| (p.x / upem, p.y / upem);
        let mut path = Vec::new();
        let mut end: Option<(f32, f32)> = None;
        for curve in &outline.curves {
            let start = match curve {
                OutlineCurve::Line(a, _) | OutlineCurve::Quad(a, ..) | OutlineCurve::Cubic(a, ..) => pt(*a),
            };
            // A curve that does not continue the previous one starts a new contour.
            if end.is_none_or(|e| (e.0 - start.0).abs() > 1e-4 || (e.1 - start.1).abs() > 1e-4) {
                if end.is_some() {
                    path.push(PathOp::Close);
                }
                path.push(PathOp::Move(start.0, start.1));
            }
            end = Some(match curve {
                OutlineCurve::Line(_, b) => {
                    let b = pt(*b);
                    path.push(PathOp::Line(b.0, b.1));
                    b
                }
                OutlineCurve::Quad(_, c, b) => {
                    let (c, b) = (pt(*c), pt(*b));
                    path.push(PathOp::Quad(c.0, c.1, b.0, b.1));
                    b
                }
                OutlineCurve::Cubic(_, c1, c2, b) => {
                    let (c1, c2, b) = (pt(*c1), pt(*c2), pt(*b));
                    path.push(PathOp::Cubic(c1.0, c1.1, c2.0, c2.1, b.0, b.1));
                    b
                }
            });
        }
        if end.is_some() {
            path.push(PathOp::Close);
        }
        Some(GlyphOutline { path })
    }

    fn advance(&self, ch: char, style: FontStyle) -> f32 {
        match self.find(ch, style) {
            Some((font, id)) => font.h_advance_unscaled(id) / em(font),
            // Nothing is drawn for a character no font has; keep its place.
            None => 0.55,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_cyrillic_and_the_diagram_symbols_have_outlines() {
        let f = PrintFonts::default();
        for ch in "Hello_id привет ΑΒΓ…→".chars().filter(|c| *c != ' ') {
            let g = f.glyph(ch, FontStyle::Regular).unwrap_or_else(|| panic!("no glyph for {ch:?}"));
            assert!(!g.path.is_empty(), "{ch:?}");
            assert!(f.advance(ch, FontStyle::Regular) > 0.05, "{ch:?}");
        }
        assert!(f.glyph('◆', FontStyle::Regular).is_some_and(|g| !g.path.is_empty()), "key marker");
        assert!(f.glyph(' ', FontStyle::Regular).is_some_and(|g| g.path.is_empty()));
    }

    #[test]
    fn outlines_stand_upright_in_em_units() {
        let f = PrintFonts::default();
        // `H` is capital height tall, above the baseline (y up), and narrower than the em box.
        let g = f.glyph('H', FontStyle::Regular).unwrap();
        let ys: Vec<f32> = g
            .path
            .iter()
            .filter_map(|p| match p {
                PathOp::Move(_, y) | PathOp::Line(_, y) => Some(*y),
                _ => None,
            })
            .collect();
        let (lo, hi) = ys.iter().fold((f32::MAX, f32::MIN), |(l, h), y| (l.min(*y), h.max(*y)));
        assert!((-0.05..0.05).contains(&lo), "sits on the baseline: {lo}");
        assert!(hi > 0.55 && hi < 0.85, "capital height in em: {hi}");
    }

    #[test]
    fn widths_and_ellipsis_use_real_advances() {
        let f = PrintFonts::default();
        let w = f.width("iiii", 12.0, FontStyle::Regular);
        let wide = f.width("WWWW", 12.0, FontStyle::Regular);
        assert!(w > 0.0 && wide > w * 1.5);
        let mono_i = f.width("i", 11.0, FontStyle::Mono);
        let mono_w = f.width("W", 11.0, FontStyle::Mono);
        assert!((mono_i - mono_w).abs() < 0.01, "Hack is monospaced");
        let cut = f.fit("a_really_long_column_name_here", 11.5, FontStyle::Mono, 80.0);
        assert!(cut.ends_with('…') && f.width(&cut, 11.5, FontStyle::Mono) <= 80.0);
        assert_eq!(f.fit("short", 11.5, FontStyle::Mono, 80.0), "short");
    }
}
