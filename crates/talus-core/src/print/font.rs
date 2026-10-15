//! Glyph outlines for the PDF writer. The application provides its own fonts; tests use a stub.

use super::scene::FontStyle;

/// One segment of a glyph outline, in em units (1.0 = the font size), y pointing up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathOp {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GlyphOutline {
    pub path: Vec<PathOp>,
}

pub trait Fonts {
    /// Outline of `ch`, or `None` when the font has no glyph for it (drawn as nothing; spaces have an empty path).
    fn glyph(&self, ch: char, style: FontStyle) -> Option<GlyphOutline>;
    /// Horizontal advance of `ch` in em units.
    fn advance(&self, ch: char, style: FontStyle) -> f32;

    /// Width of `text` at `size`.
    fn width(&self, text: &str, size: f32, style: FontStyle) -> f32 {
        text.chars().map(|c| self.advance(c, style)).sum::<f32>() * size
    }

    /// `text` shortened with an ellipsis so that it fits `max_width` at `size`.
    fn fit(&self, text: &str, size: f32, style: FontStyle, max_width: f32) -> String {
        if self.width(text, size, style) <= max_width {
            return text.to_string();
        }
        let ellipsis = self.advance('…', style).max(self.advance('.', style) * 3.0) * size;
        let mut out = String::new();
        let mut used = 0.0;
        for c in text.chars() {
            let w = self.advance(c, style) * size;
            if used + w + ellipsis > max_width {
                break;
            }
            out.push(c);
            used += w;
        }
        out.push('…');
        out
    }
}
