//! Visual design: palette tokens, typography and egui style. Monochrome and flat:
//! white content on a light-grey sidebar, black text. Neutrals are translucent "ink" (the text colour
//! at low alpha). Content has no outlines or tinted surfaces: cards are white
//! and lifted by a two-layer shadow, groups are separated by space and type. Only controls have an
//! edge (inputs: white + ink edge; buttons: faint ink fill + edge); focus is a thin black ring.
//! Colour is used only for state (ok/info/warn/danger). Fonts come from the running macOS system
//! (SF Pro, plus a semibold instance of the same variable font for headings).

use egui::epaint::text::VariationCoords;
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Stroke, TextStyle, Visuals};
use talus_core::model::Engine;
use talus_core::store::config::Theme;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub raised: Color32,
    pub border: Color32,
    /// Sidebar / navigation surface.
    pub sidebar: Color32,
    /// Card surface (lifted by `card_shadows`).
    pub surface: Color32,
    /// Fill of text inputs.
    pub field: Color32,
    /// Fill of small controls that need a body at rest (checkbox, radio, scrollbar).
    pub control: Color32,
    /// Hover and selected fills for list rows and quiet buttons.
    pub hover: Color32,
    pub selected: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    /// Tertiary text: hints, captions, axis labels.
    pub text_faint: Color32,
    pub accent: Color32,
    /// Text on an `accent` fill (primary buttons, avatars).
    pub on_accent: Color32,
    pub info: Color32,
    pub danger: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub edited: Color32,
    pub inserted: Color32,
    pub deleted: Color32,
    pub syntax_keyword: Color32,
    pub syntax_string: Color32,
    pub syntax_number: Color32,
    pub syntax_comment: Color32,
    pub syntax_ident: Color32,
    pub syntax_param: Color32,
}

const fn grey(v: u8) -> Color32 {
    Color32::from_rgb(v, v, v)
}

/// Translucent "ink": the text colour at `alpha` (every neutral fill and stroke is
/// built this way, e.g. hover/selection = #141414 at 8 %), so the same token reads correctly on
/// white content and on the grey sidebar alike.
const fn ink(v: u8, alpha: u8) -> Color32 {
    let c = ((v as u16 * alpha as u16) / 255) as u8;
    Color32::from_rgba_premultiplied(c, c, c, alpha)
}

pub const DARK: Palette = Palette {
    bg: grey(0x14),
    panel: grey(0x18),
    raised: grey(0x1c),
    border: grey(0x2a),
    sidebar: grey(0x18),
    surface: grey(0x1c),
    field: ink(0xff, 12),
    control: ink(0xff, 40),
    hover: ink(0xff, 16),
    selected: ink(0xff, 24),
    text: grey(0xe6),
    text_dim: grey(0x8c),
    text_faint: grey(0x6a),
    accent: grey(0xf2),
    on_accent: grey(0x14),
    info: Color32::from_rgb(0x6c, 0xa8, 0xf0),
    danger: Color32::from_rgb(0xe0, 0x7a, 0x7a),
    ok: Color32::from_rgb(0x7f, 0xbf, 0x8e),
    warn: Color32::from_rgb(0xd9, 0xb4, 0x5f),
    edited: Color32::from_rgb(0x2f, 0x2b, 0x1a),
    inserted: Color32::from_rgb(0x1a, 0x2b, 0x20),
    deleted: Color32::from_rgb(0x33, 0x1e, 0x1e),
    syntax_keyword: grey(0xfa),
    syntax_string: grey(0x9e),
    syntax_number: grey(0xd8),
    syntax_comment: grey(0x66),
    syntax_ident: grey(0xc4),
    syntax_param: grey(0xe0),
};

pub const LIGHT: Palette = Palette {
    bg: grey(0xff),
    panel: grey(0xff),
    raised: grey(0xff),
    border: grey(0xe6),
    sidebar: grey(0xf7),
    surface: grey(0xff),
    field: ink(0x14, 11),
    control: ink(0x14, 36),
    hover: ink(0x14, 13),
    selected: ink(0x14, 20),
    text: grey(0x11),
    text_dim: grey(0x6b),
    text_faint: grey(0x9c),
    accent: grey(0x11),
    on_accent: grey(0xff),
    info: Color32::from_rgb(0x1f, 0x7a, 0xe0),
    danger: Color32::from_rgb(0xc2, 0x3b, 0x40),
    ok: Color32::from_rgb(0x1f, 0x93, 0x4f),
    warn: Color32::from_rgb(0xa8, 0x6d, 0x08),
    edited: Color32::from_rgb(0xfb, 0xf0, 0xc8),
    inserted: Color32::from_rgb(0xdd, 0xf2, 0xe2),
    deleted: Color32::from_rgb(0xf8, 0xdc, 0xdc),
    syntax_keyword: grey(0x0a),
    syntax_string: grey(0x6b),
    syntax_number: grey(0x2a),
    syntax_comment: grey(0xa0),
    syntax_ident: grey(0x3d),
    syntax_param: grey(0x1a),
};

pub fn palette(theme: Theme) -> &'static Palette {
    match theme {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

/// Palette for the current egui visuals (dark/light).
pub fn current(ctx: &egui::Context) -> &'static Palette {
    if ctx.global_style().visuals.dark_mode { &DARK } else { &LIGHT }
}

pub fn engine_color(engine: Engine) -> Color32 {
    // Monochrome UI: engines are told apart by their icon, not by colour.
    match engine {
        Engine::Sqlite
        | Engine::Postgres
        | Engine::MySql
        | Engine::SqlServer
        | Engine::Oracle
        | Engine::Snowflake
        | Engine::Redis
        | Engine::MongoDb => grey(0x9a),
    }
}

pub fn engine_icon(engine: Engine) -> &'static str {
    match engine {
        Engine::Sqlite => egui_phosphor::regular::FEATHER,
        Engine::Postgres => egui_phosphor::regular::HARD_DRIVES,
        Engine::MySql => egui_phosphor::regular::DATABASE,
        Engine::SqlServer => egui_phosphor::regular::STACK,
        Engine::Oracle => egui_phosphor::regular::CYLINDER,
        Engine::Snowflake => egui_phosphor::regular::SNOWFLAKE,
        Engine::Redis => egui_phosphor::regular::KEY,
        Engine::MongoDb => egui_phosphor::regular::LEAF,
    }
}

const SEMIBOLD: &str = "semibold";

/// Semibold UI font (headings, titles, figures).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    // System fonts are read at runtime from this Mac; nothing is redistributed.
    for (name, path, family) in [
        ("sf", "/System/Library/Fonts/SFNS.ttf", FontFamily::Proportional),
        ("sf-mono", "/System/Library/Fonts/SFNSMono.ttf", FontFamily::Monospace),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            if name == "sf" {
                // SF Pro is a variable font: the semibold family is the same file at wght 600.
                let mut bold = FontData::from_owned(bytes.clone());
                bold.tweak = FontTweak { coords: VariationCoords::new([(b"wght", 600.0)]), ..FontTweak::default() };
                fonts.font_data.insert("sf-semibold".into(), bold.into());
                fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), vec!["sf-semibold".into()]);
            }
            fonts.font_data.insert(name.into(), FontData::from_owned(bytes).into());
            fonts.families.entry(family).or_default().insert(0, name.into());
        }
    }
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // Icons also appear inside monospace text (grid row markers, ER diagram columns).
    fonts.families.entry(FontFamily::Monospace).or_default().push("phosphor".into());
    // Without SF (other OSes, tests) the semibold family falls back to the default proportional fonts.
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    fonts.families.entry(FontFamily::Name(SEMIBOLD.into())).or_default().extend(fallback);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, theme: Theme, editor_font: f32) {
    let p = palette(theme);
    let mut v = match theme {
        Theme::Dark => Visuals::dark(),
        Theme::Light => Visuals::light(),
    };
    let dark = theme == Theme::Dark;
    v.panel_fill = p.panel;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.bg;
    v.text_edit_bg_color = Some(p.field);
    v.faint_bg_color = if dark { grey(0x19) } else { grey(0xfa) };
    v.code_bg_color = p.sidebar;
    // Windows and popups float on a shadow; nothing in the UI is outlined.
    v.window_stroke = Stroke::NONE;
    v.window_shadow = egui::Shadow {
        offset: [0, 16],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 120 } else { 34 }),
    };
    v.popup_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 100 } else { 30 }),
    };
    v.selection.bg_fill = if dark { ink(0xff, 40) } else { ink(0x14, 26) };
    // Focus ring of inputs and the text colour of selected items.
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.text;
    let press = if dark { ink(0xff, 28) } else { ink(0x14, 24) };
    // `weak_bg_fill` is a button's / combo's body: transparent at rest (ghost). `bg_fill` is the body of
    // checkboxes, radios and scrollbars, which need one to be seen.
    for (w, weak, fill) in [
        (&mut v.widgets.noninteractive, p.raised, p.raised),
        (&mut v.widgets.inactive, Color32::TRANSPARENT, p.control),
        (&mut v.widgets.hovered, p.hover, p.control),
        (&mut v.widgets.active, press, p.control),
        (&mut v.widgets.open, p.raised, p.hover),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = weak;
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke = Stroke::new(1.0, p.text);
        w.expansion = 0.0;
        w.corner_radius = 7.into();
    }
    // (`open.weak_bg_fill` is also the active window's title bar: kept the window colour, no band.)
    // Controls: inputs are white with an ink-18 % edge, buttons and combos carry a
    // faint ink fill plus that edge. Edges exist only on controls, never in content.
    v.text_edit_bg_color = Some(p.bg);
    v.widgets.inactive.weak_bg_fill = ink(0x14, 14);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, ink(0x14, 46));
    v.widgets.hovered.weak_bg_fill = ink(0x14, 22);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ink(0x14, 70));
    v.window_corner_radius = 14.into();
    v.menu_corner_radius = 10.into();
    ctx.set_theme(match theme {
        Theme::Dark => egui::Theme::Dark,
        Theme::Light => egui::Theme::Light,
    });
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.window_margin = egui::Margin::same(16);
        s.text_styles.insert(TextStyle::Body, FontId::proportional(13.5));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(13.5));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(11.5));
        s.text_styles.insert(TextStyle::Heading, semibold(18.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(editor_font));
    });
}

pub const CARD_RADIUS: u8 = 12;

/// Card surface: white, rounded, no outline; `widgets::card` adds the shadows.
pub fn card_frame(p: &Palette) -> egui::Frame {
    egui::Frame::NONE.fill(p.surface).corner_radius(CARD_RADIUS)
}

/// Two-layer elevation: a tight key shadow that defines the edge and a wide ambient one for depth.
pub fn card_shadows(p: &Palette) -> [egui::Shadow; 2] {
    let dark = p.bg == DARK.bg;
    let a = |light: u8, dark_a: u8| Color32::from_black_alpha(if dark { dark_a } else { light });
    [
        egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: a(20, 90) },
        egui::Shadow { offset: [0, 6], blur: 22, spread: 0, color: a(14, 70) },
    ]
}

/// Primary (filled black) button.
pub fn primary_button(p: &Palette, text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(p.on_accent)).fill(p.accent).stroke(Stroke::NONE)
}
