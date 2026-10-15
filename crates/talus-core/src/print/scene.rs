//! A diagram as drawing primitives in scene units (CSS pixels, y pointing down).

use super::font::Fonts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontStyle {
    Regular,
    Semibold,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        fill: Option<Rgb>,
        stroke: Option<(Rgb, f32)>,
    },
    /// The band behind a card's title: rounded at the top corners only.
    Header {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        fill: Rgb,
    },
    Line {
        points: Vec<(f32, f32)>,
        color: Rgb,
        width: f32,
    },
    Dot {
        x: f32,
        y: f32,
        r: f32,
        fill: Rgb,
    },
    /// `y` is the baseline.
    Text {
        x: f32,
        y: f32,
        text: String,
        size: f32,
        style: FontStyle,
        color: Rgb,
        align: Align,
    },
}

impl Prim {
    /// Bounding box `(x0, y0, x1, y1)`; text uses its font size for the height and `width` for its extent.
    pub fn bounds(&self, fonts: &dyn Fonts) -> (f32, f32, f32, f32) {
        match self {
            Prim::Rect { x, y, w, h, .. } | Prim::Header { x, y, w, h, .. } => (*x, *y, x + w, y + h),
            Prim::Line { points, width, .. } => {
                let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for (x, y) in points {
                    (x0, y0, x1, y1) = (x0.min(*x), y0.min(*y), x1.max(*x), y1.max(*y));
                }
                (x0 - width, y0 - width, x1 + width, y1 + width)
            }
            Prim::Dot { x, y, r, .. } => (x - r, y - r, x + r, y + r),
            Prim::Text { x, y, text, size, style, align, .. } => {
                let w = fonts.width(text, *size, *style);
                let left = match align {
                    Align::Left => *x,
                    Align::Right => x - w,
                    Align::Center => x - w / 2.0,
                };
                (left, y - size, left + w, y + size * 0.3)
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    pub width: f32,
    pub height: f32,
    pub prims: Vec<Prim>,
    /// Boxes that should stay on one sheet when the diagram is split over several: `(x, y, w, h)`.
    pub keep_whole: Vec<(f32, f32, f32, f32)>,
    /// Shown in the page header.
    pub title: String,
    pub subtitle: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    None,
    Key,
    Link,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardRow {
    pub name: String,
    /// Right-aligned, dim.
    pub ty: String,
    pub marker: Marker,
}

/// A table box of a diagram: title, optional tag on the right of the title, rows, and where it sits.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub title: String,
    pub tag: Option<String>,
    pub rows: Vec<CardRow>,
    pub x: f32,
    pub y: f32,
}

/// A relation from a row of one card to another card's title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub from: usize,
    pub row: usize,
    pub to: usize,
}

pub const CARD_W: f32 = 210.0;
pub const HEADER_H: f32 = 26.0;
pub const ROW_H: f32 = 18.0;
const MARGIN: f32 = 24.0;
const INK: Rgb = Rgb(17, 17, 17);
const DIM: Rgb = Rgb(119, 119, 119);
const LINE: Rgb = Rgb(85, 85, 85);
const KEY: Rgb = Rgb(180, 120, 0);

pub fn card_height(rows: usize) -> f32 {
    HEADER_H + ROW_H * rows.max(1) as f32 + 6.0
}

/// Lays `cards` and `links` out the way the canvases draw them, in a light palette that prints well.
pub fn diagram_scene(cards: &[Card], links: &[Link], title: &str, subtitle: &str, fonts: &dyn Fonts) -> Scene {
    let (min_x, min_y) = cards.iter().fold((f32::MAX, f32::MAX), |(x, y), c| (x.min(c.x), y.min(c.y)));
    let (min_x, min_y) = if cards.is_empty() { (0.0, 0.0) } else { (min_x, min_y) };
    let at = |x: f32, y: f32| (x - min_x + MARGIN, y - min_y + MARGIN);
    let mut scene = Scene { title: title.to_string(), subtitle: subtitle.to_string(), ..Default::default() };
    let (mut w, mut h) = (MARGIN * 2.0, MARGIN * 2.0);
    for c in cards {
        let (x, y) = at(c.x, c.y);
        w = w.max(x + CARD_W + MARGIN);
        h = h.max(y + card_height(c.rows.len()) + MARGIN);
    }
    (scene.width, scene.height) = (w, h);
    // Relations under the cards.
    for l in links {
        let (Some(src), Some(dst)) = (cards.get(l.from), cards.get(l.to)) else { continue };
        let (sx, sy) = at(src.x, src.y);
        let (dx, dy) = at(dst.x, dst.y);
        let y = sy + HEADER_H + ROW_H * (l.row as f32 + 0.5);
        let ty = dy + HEADER_H * 0.5;
        let (fx, tx) = if dx < sx { (sx, dx + CARD_W) } else { (sx + CARD_W, dx) };
        let mid = (fx + tx) / 2.0;
        scene.prims.push(Prim::Line { points: vec![(fx, y), (mid, y), (mid, ty), (tx, ty)], color: LINE, width: 1.4 });
        scene.prims.push(Prim::Dot { x: tx, y: ty, r: 3.5, fill: INK });
    }
    for c in cards {
        let (x, y) = at(c.x, c.y);
        let bh = card_height(c.rows.len());
        scene.keep_whole.push((x, y, CARD_W, bh));
        scene.prims.push(Prim::Rect {
            x,
            y,
            w: CARD_W,
            h: bh,
            radius: 6.0,
            fill: Some(Rgb(255, 255, 255)),
            stroke: Some((Rgb(34, 34, 34), 1.0)),
        });
        scene.prims.push(Prim::Header { x, y, w: CARD_W, h: HEADER_H, radius: 6.0, fill: INK });
        let tag_w = c.tag.as_deref().map_or(0.0, |t| fonts.width(&t.to_uppercase(), 9.0, FontStyle::Semibold) + 10.0);
        scene.prims.push(Prim::Text {
            x: x + 10.0,
            y: y + HEADER_H * 0.68,
            text: fonts.fit(&c.title, 13.0, FontStyle::Semibold, CARD_W - 20.0 - tag_w),
            size: 13.0,
            style: FontStyle::Semibold,
            color: Rgb(255, 255, 255),
            align: Align::Left,
        });
        if let Some(tag) = &c.tag {
            scene.prims.push(Prim::Text {
                x: x + CARD_W - 8.0,
                y: y + HEADER_H * 0.66,
                text: tag.to_uppercase(),
                size: 9.0,
                style: FontStyle::Semibold,
                color: Rgb(200, 200, 200),
                align: Align::Right,
            });
        }
        for (i, r) in c.rows.iter().enumerate() {
            let base = y + HEADER_H + ROW_H * (i as f32 + 0.5) + 4.0;
            let marker = match r.marker {
                Marker::Key => "◆",
                Marker::Link => "→",
                Marker::None => "",
            };
            if !marker.is_empty() {
                scene.prims.push(Prim::Text {
                    x: x + 8.0,
                    y: base,
                    text: marker.to_string(),
                    size: 10.0,
                    style: FontStyle::Regular,
                    color: KEY,
                    align: Align::Left,
                });
            }
            let ty_w = fonts.width(&r.ty, 11.0, FontStyle::Mono);
            let room = CARD_W - 24.0 - 8.0 - ty_w - if ty_w > 0.0 { 8.0 } else { 0.0 };
            scene.prims.push(Prim::Text {
                x: x + 24.0,
                y: base,
                text: fonts.fit(&r.name, 11.5, FontStyle::Mono, room.max(30.0)),
                size: 11.5,
                style: FontStyle::Mono,
                color: INK,
                align: Align::Left,
            });
            if !r.ty.is_empty() {
                scene.prims.push(Prim::Text {
                    x: x + CARD_W - 8.0,
                    y: base,
                    text: fonts.fit(&r.ty, 11.0, FontStyle::Mono, (CARD_W - 40.0) * 0.5),
                    size: 11.0,
                    style: FontStyle::Mono,
                    color: DIM,
                    align: Align::Right,
                });
            }
        }
    }
    scene
}
