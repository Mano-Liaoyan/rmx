//! Design tokens, and the egui style built from them.
//!
//! Cool neutrals with one indigo. Besides it, color says only what a thing is or how it is doing.
//! The panes are raised off one dark frame by a hairline; only floating surfaces cast a shadow.

use egui::epaint::Shadow;
use egui::style::{Selection, TextCursorStyle, WidgetVisuals, Widgets};
use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme,
    Vec2, Visuals,
};

use crate::action::Protocol;
use crate::fonts;

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// What a run of source text is, for coloring it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Comment,
    Keyword,
    String,
    Number,
    Function,
    Type,
    Constant,
}

/// The color tokens of one theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Where work happens: panes and dialogs.
    pub background: Color32,
    pub sidebar: Color32,
    /// The frame around the panes, one surface with the sidebar.
    pub chrome: Color32,
    /// Menus and dialogs, a step above everything under them.
    pub float: Color32,
    /// The inside of a field.
    pub input: Color32,
    pub foreground: Color32,
    pub muted_fg: Color32,
    /// Under what is selected.
    pub accent: Color32,
    pub border: Color32,
    /// The one color of the interface: the main button, the focused field, selected text.
    pub brand: Color32,
    /// Text on `brand`.
    pub brand_fg: Color32,
    pub folder: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub info: Color32,
    pub danger: Color32,
    /// Fill under a hovered item, a wash of the foreground. `accent` can't serve here: a hovered
    /// row that is also selected would not change at all.
    pub hover: Color32,
    /// The dimming behind a modal dialog.
    pub scrim: Color32,
    /// The color of a floating surface's shadow.
    pub shadow: Color32,
    /// What tells the kinds of connection apart at a glance, in the order of [`Protocol::ALL`].
    protocols: [Color32; 8],
    /// In the order of [`Token`].
    syntax: [Color32; 7],
}

impl Palette {
    /// The color of the icons of `protocol`.
    pub fn protocol(&self, protocol: Protocol) -> Color32 {
        let index = Protocol::ALL.iter().position(|known| *known == protocol).unwrap_or(0);
        self.protocols[index]
    }

    /// The color of a run of source text.
    pub fn token(&self, token: Token) -> Color32 {
        self.syntax[token as usize]
    }
}

/// `alpha` of 255 of white, premultiplied.
const fn white(alpha: u8) -> Color32 {
    Color32::from_rgba_premultiplied(alpha, alpha, alpha, alpha)
}

/// `alpha` of 255 of black, premultiplied.
const fn black(alpha: u8) -> Color32 {
    Color32::from_rgba_premultiplied(0, 0, 0, alpha)
}

/// The light theme.
pub const LIGHT: Palette = Palette {
    background: rgb(0xfdfdfe),
    sidebar: rgb(0xf4f4f6),
    chrome: rgb(0xf4f4f6),
    float: rgb(0xffffff),
    input: black(8),
    foreground: rgb(0x1c1d21),
    muted_fg: rgb(0x6b6f76),
    accent: rgb(0xe6e7ea),
    border: black(22),
    brand: rgb(0x5e6ad2),
    brand_fg: rgb(0xffffff),
    folder: rgb(0x4f7fd6),
    success: rgb(0x2e9e6b),
    warning: rgb(0xc98a0b),
    info: rgb(0x2f7fd6),
    danger: rgb(0xd64545),
    hover: black(12),
    scrim: black(64),
    shadow: black(36),
    protocols: [
        rgb(0x2e9e6b),
        rgb(0x2f7fd6),
        rgb(0x1596a8),
        rgb(0xd9772b),
        rgb(0x8a63d2),
        rgb(0xc4502c),
        rgb(0xb5890d),
        rgb(0xc9557f),
    ],
    syntax: [
        rgb(0x8a8f98),
        rgb(0x6a4fc4),
        rgb(0x2b8a5f),
        rgb(0xb5641f),
        rgb(0x2f6fc2),
        rgb(0x12808f),
        rgb(0xb0487a),
    ],
};

/// The dark theme.
pub const DARK: Palette = Palette {
    background: rgb(0x121316),
    sidebar: rgb(0x0c0d0f),
    chrome: rgb(0x0c0d0f),
    float: rgb(0x1b1c20),
    input: white(10),
    foreground: rgb(0xe4e5e8),
    muted_fg: rgb(0x8a8f98),
    accent: rgb(0x26282d),
    border: white(20),
    brand: rgb(0x5e6ad2),
    brand_fg: rgb(0xffffff),
    folder: rgb(0x6f9ce8),
    success: rgb(0x4cb782),
    warning: rgb(0xf2c94c),
    info: rgb(0x4ea7fc),
    danger: rgb(0xeb5757),
    hover: white(13),
    scrim: black(115),
    shadow: black(130),
    protocols: [
        rgb(0x4cb782),
        rgb(0x4ea7fc),
        rgb(0x3fc1d4),
        rgb(0xf2994a),
        rgb(0xb18cf5),
        rgb(0xf07a5a),
        rgb(0xe8c15a),
        rgb(0xeb8dac),
    ],
    syntax: [
        rgb(0x6b7079),
        rgb(0xb4a5f7),
        rgb(0x82cfa4),
        rgb(0xe8ab7c),
        rgb(0x82b4f2),
        rgb(0x73cdd4),
        rgb(0xeb9cc0),
    ],
};

/// Corner radius of small items: tabs, tree rows, segments.
pub const RADIUS_SM: u8 = 4;
/// Corner radius of controls and panes.
pub const RADIUS: u8 = 6;
/// Corner radius of floating surfaces.
pub const RADIUS_LG: u8 = 10;
/// Height of buttons and inputs.
pub const CONTROL_HEIGHT: f32 = 30.;
/// Height of the single row holding the title bar and tabs.
pub const STRIP_HEIGHT: f32 = 40.;
/// Height of one tab.
pub const TAB_HEIGHT: f32 = 28.;
/// Height of a tree row.
pub const TREE_ROW_HEIGHT: f32 = 28.;
/// Size of a tree row's icon.
pub const TREE_ICON: f32 = 14.;
/// Width of the sidebar.
pub const SIDEBAR_WIDTH: f32 = 260.;
/// Narrowest and widest the sidebar is dragged to.
pub const SIDEBAR_MIN: f32 = 180.;
pub const SIDEBAR_MAX: f32 = 560.;
/// Height of the bar on top of each pane.
pub const PANE_BAR_HEIGHT: f32 = 28.;
/// Text about a thing rather than the thing: sizes, dates, keys.
pub const TEXT_XS: f32 = 12.;
/// Almost everything.
pub const TEXT_SM: f32 = 13.;
/// Titles.
pub const TEXT_LG: f32 = 15.;
/// How long a floating surface takes to appear, in seconds.
pub const FLOAT_IN: f64 = 0.1;
/// The scale a floating surface appears from.
pub const FLOAT_SCALE: f32 = 0.95;
/// How long a context menu takes to pop in from the pointer, in seconds, and the scale it grows from.
pub const MENU_IN: f64 = 0.14;
pub const MENU_SCALE: f32 = 0.92;
/// How long tabs take to slide, in milliseconds.
pub const TAB_SLIDE_MS: u64 = 200;

/// Which theme is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

impl ThemeMode {
    /// The tokens of this theme.
    pub const fn palette(self) -> &'static Palette {
        match self {
            ThemeMode::Light => &LIGHT,
            ThemeMode::Dark => &DARK,
        }
    }

    const fn egui(self) -> Theme {
        match self {
            ThemeMode::Light => Theme::Light,
            ThemeMode::Dark => Theme::Dark,
        }
    }
}

/// Set both themes' styles on `ctx` and show `mode`.
pub fn apply(ctx: &Context, mode: ThemeMode) {
    ctx.set_style_of(Theme::Light, style(ThemeMode::Light));
    ctx.set_style_of(Theme::Dark, style(ThemeMode::Dark));
    ctx.set_theme(mode.egui());
}

fn widget(bg: Color32, border: Color32, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1., border),
        corner_radius: CornerRadius::same(RADIUS),
        fg_stroke: Stroke::new(1., fg),
        expansion: 0.,
    }
}

fn float_shadow(p: &Palette) -> Shadow {
    Shadow { offset: [0, 8], blur: 28, spread: 0, color: p.shadow }
}

fn style(mode: ThemeMode) -> Style {
    let p = mode.palette();
    let mut visuals = match mode {
        ThemeMode::Light => Visuals::light(),
        ThemeMode::Dark => Visuals::dark(),
    };

    visuals.widgets = Widgets {
        noninteractive: widget(p.background, p.border, p.foreground),
        inactive: widget(Color32::TRANSPARENT, p.border, p.foreground),
        hovered: widget(p.hover, p.border, p.foreground),
        active: widget(p.accent, p.border, p.foreground),
        open: widget(p.accent, p.border, p.foreground),
    };
    visuals.selection = Selection { bg_fill: p.brand.gamma_multiply(0.45), stroke: Stroke::new(1., p.foreground) };
    visuals.text_cursor = TextCursorStyle {
        stroke: Stroke::new(1.5, p.foreground),
        blink: false,
        ..visuals.text_cursor
    };
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(p.muted_fg);
    visuals.hyperlink_color = p.foreground;
    visuals.faint_bg_color = p.sidebar;
    visuals.extreme_bg_color = p.background;
    visuals.code_bg_color = p.accent;
    visuals.warn_fg_color = p.warning;
    visuals.error_fg_color = p.danger;
    visuals.panel_fill = p.background;
    visuals.window_fill = p.float;
    visuals.window_stroke = Stroke::new(1., p.border);
    visuals.window_corner_radius = CornerRadius::same(RADIUS_LG);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_LG);
    visuals.window_shadow = float_shadow(p);
    visuals.popup_shadow = float_shadow(p);
    visuals.interact_cursor = None;

    let mut style = Style { visuals, ..Style::default() };

    style.text_styles = [
        (TextStyle::Small, FontId::new(TEXT_XS, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(TEXT_SM, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(TEXT_SM, fonts::medium_family())),
        (TextStyle::Heading, FontId::new(TEXT_LG, fonts::medium_family())),
        (TextStyle::Monospace, FontId::new(TEXT_SM, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(8., 8.);
    style.spacing.button_padding = Vec2::new(12., 8.);
    style.spacing.interact_size = Vec2::new(40., CONTROL_HEIGHT);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.menu_margin = Margin::same(4);
    style.spacing.icon_width = 14.;
    style.spacing.icon_width_inner = 8.;
    // No hover or press animations: each would keep egui running for frames after the input.
    style.animation_time = 0.;
    style.explanation_tooltips = false;
    style.url_in_tooltip = false;
    style
}
