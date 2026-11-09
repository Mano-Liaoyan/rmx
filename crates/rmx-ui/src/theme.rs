//! Design tokens, and the egui style built from them.
//!
//! Neutral monochrome with no brand hue; color appears only for status. Inside the shell, depth
//! comes from 1 px borders; only floating surfaces cast a shadow.

use egui::epaint::Shadow;
use egui::style::{Selection, TextCursorStyle, WidgetVisuals, Widgets};
use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme,
    Vec2, Visuals,
};

use crate::fonts;

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The color tokens of one theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: Color32,
    pub sidebar: Color32,
    pub chrome: Color32,
    pub foreground: Color32,
    pub muted_fg: Color32,
    pub accent: Color32,
    pub border: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub info: Color32,
    pub danger: Color32,
    /// Fill under a hovered item, a wash of the foreground. `accent` can't serve here: in the
    /// light theme it equals `chrome`, so a hovered tab would not change at all.
    pub hover: Color32,
    /// The dimming behind a modal dialog.
    pub scrim: Color32,
    /// The color of a floating surface's shadow.
    pub shadow: Color32,
}

/// The light theme.
pub const LIGHT: Palette = Palette {
    background: rgb(0xffffff),
    sidebar: rgb(0xfafafa),
    chrome: rgb(0xf5f5f5),
    foreground: rgb(0x0a0a0a),
    muted_fg: rgb(0x737373),
    accent: rgb(0xf5f5f5),
    border: rgb(0xe5e5e5),
    success: rgb(0x16a34a),
    warning: rgb(0xd97706),
    info: rgb(0x2563eb),
    danger: rgb(0xdc2626),
    hover: Color32::from_rgba_premultiplied(0, 0, 0, 10),
    scrim: Color32::from_rgba_premultiplied(0, 0, 0, 64),
    shadow: Color32::from_rgba_premultiplied(0, 0, 0, 36),
};

/// The dark theme.
pub const DARK: Palette = Palette {
    background: rgb(0x131416),
    sidebar: rgb(0x19191c),
    chrome: rgb(0x1b1b1e),
    foreground: rgb(0xd7d7db),
    muted_fg: rgb(0x97989d),
    accent: rgb(0x2e2f33),
    // rgb(110 110 114 / .28), premultiplied.
    border: Color32::from_rgba_premultiplied(31, 31, 32, 71),
    success: rgb(0x4ade80),
    warning: rgb(0xfbbf24),
    info: rgb(0x60a5fa),
    danger: rgb(0xf87171),
    hover: Color32::from_rgba_premultiplied(13, 13, 13, 13),
    scrim: Color32::from_rgba_premultiplied(0, 0, 0, 115),
    shadow: Color32::from_rgba_premultiplied(0, 0, 0, 110),
};

/// Corner radius of small items: tabs, tree rows, segments.
pub const RADIUS_SM: u8 = 4;
/// Corner radius of controls and floating surfaces.
pub const RADIUS: u8 = 6;
/// Height of buttons and inputs.
pub const CONTROL_HEIGHT: f32 = 32.;
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
/// Height of a pane's footer.
pub const FOOTER_HEIGHT: f32 = 24.;
/// Small text, used for almost everything.
pub const TEXT_SM: f32 = 12.;
/// Large text, for titles.
pub const TEXT_LG: f32 = 14.;
/// How long a floating surface takes to appear, in seconds.
pub const FLOAT_IN: f64 = 0.1;
/// The scale a floating surface appears from.
pub const FLOAT_SCALE: f32 = 0.95;
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
    Shadow { offset: [0, 4], blur: 16, spread: 0, color: p.shadow }
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
    visuals.selection = Selection { bg_fill: p.accent, stroke: Stroke::new(1., p.foreground) };
    visuals.text_cursor = TextCursorStyle {
        stroke: Stroke::new(1., p.foreground),
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
    visuals.window_fill = p.background;
    visuals.window_stroke = Stroke::new(1., p.border);
    visuals.window_corner_radius = CornerRadius::same(RADIUS);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS);
    visuals.window_shadow = float_shadow(p);
    visuals.popup_shadow = float_shadow(p);
    visuals.interact_cursor = None;

    let mut style = Style { visuals, ..Style::default() };

    style.text_styles = [
        (TextStyle::Small, FontId::new(TEXT_SM, FontFamily::Proportional)),
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
