//! Buttons, inputs and the segmented control.

use egui::{
    Align2, Color32, CornerRadius, Frame, Id, Margin, Rect, Response, RichText, Sense, Shape,
    Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};

use crate::draw::{icon, palette, snap};
use crate::{fonts, icons};
use crate::theme::{CONTROL_HEIGHT, RADIUS, RADIUS_SM, TEXT_SM};

/// How much a button draws the eye.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one main action: solid foreground color.
    Primary,
    /// Bordered.
    Secondary,
    /// Only visible when hovered.
    Ghost,
}

/// A 32 px button sized to its label.
pub fn button(ui: &mut Ui, label: &str, kind: ButtonKind) -> Response {
    button_full(ui, None, label, kind, true, None)
}

/// A button with every option: leading icon, enabled state and a fixed width.
pub fn button_full(
    ui: &mut Ui,
    glyph: Option<char>,
    label: &str,
    kind: ButtonKind,
    enabled: bool,
    width: Option<f32>,
) -> Response {
    let font = fonts::medium(TEXT_SM);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let icon_room = if glyph.is_some() { 20. } else { 0. };
    let natural = (galley.size().x + icon_room + 24.).max(64.);
    let size = vec2(width.unwrap_or(natural), CONTROL_HEIGHT);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let p = palette(ui);
    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let (fill, border, text) = match kind {
        ButtonKind::Primary => {
            let fill = if hovered { p.foreground.gamma_multiply(0.88) } else { p.foreground };
            (fill, Color32::TRANSPARENT, p.background)
        },
        ButtonKind::Secondary => {
            let fill = if pressed {
                p.accent
            } else if hovered {
                p.hover
            } else {
                Color32::TRANSPARENT
            };
            (fill, p.border, p.foreground)
        },
        ButtonKind::Ghost => {
            let fill = if pressed {
                p.accent
            } else if hovered {
                p.hover
            } else {
                Color32::TRANSPARENT
            };
            (fill, Color32::TRANSPARENT, p.foreground)
        },
    };
    let (fill, text) = match (enabled, kind) {
        (true, _) => (fill, text),
        (false, ButtonKind::Primary) => (p.accent, p.muted_fg),
        (false, _) => (fill, p.muted_fg),
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(RADIUS),
        fill,
        Stroke::new(1., border),
        StrokeKind::Inside,
    );
    let total = galley.size().x + icon_room;
    let left = rect.center().x - total / 2.;
    if let Some(glyph) = glyph {
        icon(painter, pos2(left + 7., rect.center().y), glyph, 14., text);
    }
    let top = snap(painter, rect.center().y - galley.size().y / 2.);
    painter.galley(pos2(snap(painter, left + icon_room), top), galley, text);
    response
}

/// A square button with a single icon.
pub fn icon_button(ui: &mut Ui, glyph: char, side: f32, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    paint_icon_button(ui, rect, glyph, size, &response);
    response
}

pub(crate) fn paint_icon_button(ui: &Ui, rect: Rect, glyph: char, size: f32, response: &Response) {
    let p = palette(ui);
    let painter = ui.painter();
    if response.is_pointer_button_down_on() {
        painter.rect_filled(rect, CornerRadius::same(RADIUS_SM), p.accent);
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(RADIUS_SM), p.hover);
    }
    let color = if response.hovered() { p.foreground } else { p.muted_fg };
    icon(painter, rect.center(), glyph, size, color);
}

/// A tick box with its label, filling `rect`. Clicking anywhere in it toggles `checked`.
pub fn checkbox(ui: &mut Ui, rect: Rect, salt: Id, checked: &mut bool, label: &str) -> Response {
    const SIDE: f32 = 16.;
    let mut response = ui.interact(rect, salt, Sense::click());
    if response.clicked() {
        *checked = !*checked;
        response.mark_changed();
    }
    let p = palette(ui);
    let painter = ui.painter();
    let center = pos2(rect.left() + SIDE / 2., rect.center().y);
    let square = Rect::from_center_size(center, Vec2::splat(SIDE));
    let edge = if *checked {
        p.foreground
    } else if response.hovered() {
        p.muted_fg
    } else {
        p.border
    };
    let fill = if *checked { p.foreground } else { Color32::TRANSPARENT };
    painter.rect(square, CornerRadius::same(RADIUS_SM), fill, Stroke::new(1., edge), StrokeKind::Inside);
    if *checked {
        icon(painter, center, icons::CHECK, 12., p.background);
    }
    painter.text(
        pos2(rect.left() + SIDE + 8., rect.center().y),
        Align2::LEFT_CENTER,
        label,
        fonts::regular(TEXT_SM),
        p.foreground,
    );
    response
}

/// Whether Enter was pressed in the [`text_input`] of `response`, which gives up the keyboard.
pub fn submitted(ui: &Ui, response: &Response) -> bool {
    let entered = response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if entered {
        response.surrender_focus();
    }
    entered
}

/// How a [`text_input`] looks and behaves.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputOptions<'a> {
    pub hint: &'a str,
    pub password: bool,
    /// Leading icon.
    pub glyph: Option<char>,
    /// Fixed width; the available width otherwise.
    pub width: Option<f32>,
    /// Draw without a border, for inputs in a surface that already has an edge.
    pub borderless: bool,
    pub mono: bool,
    /// Height; 32 px by default.
    pub height: Option<f32>,
}

/// A single-line input. `salt` keeps its cursor and focus apart from other inputs.
pub fn text_input(ui: &mut Ui, salt: Id, text: &mut String, options: InputOptions<'_>) -> Response {
    let p = palette(ui);
    let width = options.width.unwrap_or_else(|| ui.available_width());
    let height = options.height.unwrap_or(CONTROL_HEIGHT);
    let background = ui.painter().add(Shape::Noop);
    let left = 10. + if options.glyph.is_some() { 20. } else { 0. };
    let font = if options.mono { fonts::mono(TEXT_SM) } else { fonts::regular(TEXT_SM) };

    let mut edit = TextEdit::singleline(text)
        .id_salt(salt)
        .frame(Frame::NONE.inner_margin(Margin { left: left as i8, right: 10, top: 0, bottom: 0 }))
        .min_size(vec2(width, height))
        .desired_width(width - left - 10.)
        .align(egui::Align2::LEFT_CENTER)
        .font(font)
        .text_color(p.foreground)
        .password(options.password)
        .return_key(None);
    if !options.hint.is_empty() {
        edit = edit.hint_text(RichText::new(options.hint).color(p.muted_fg).size(TEXT_SM));
    }
    let response = edit.show(ui).response.response;

    let rect = response.rect;
    if !options.borderless {
        let border = if response.has_focus() { p.muted_fg } else { p.border };
        ui.painter().set(
            background,
            Shape::rect_stroke(
                rect,
                CornerRadius::same(RADIUS),
                Stroke::new(1., border),
                StrokeKind::Inside,
            ),
        );
    }
    if let Some(glyph) = options.glyph {
        icon(ui.painter(), pos2(rect.left() + 17., rect.center().y), glyph, 14., p.muted_fg);
    }
    response
}

/// A row of mutually exclusive options. Returns a response marked changed on selection.
pub fn segmented(ui: &mut Ui, salt: Id, options: &[&str], selected: &mut usize) -> Response {
    const HEIGHT: f32 = 32.;
    const INSET: f32 = 2.;
    let p = palette(ui);
    let width = ui.available_width();
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, HEIGHT), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect(
        rect,
        CornerRadius::same(RADIUS),
        p.accent,
        Stroke::new(1., p.border),
        StrokeKind::Inside,
    );

    let inner = rect.shrink(INSET);
    let segment = inner.width() / options.len() as f32;
    for (i, label) in options.iter().enumerate() {
        let cell = Rect::from_min_size(
            pos2(inner.left() + segment * i as f32, inner.top()),
            vec2(segment, inner.height()),
        );
        let cell_response = ui.interact(cell, salt.with(i), Sense::click());
        if cell_response.clicked() && *selected != i {
            *selected = i;
            response.mark_changed();
        }
        let active = *selected == i;
        if active {
            painter.rect(
                cell,
                CornerRadius::same(RADIUS_SM),
                p.background,
                Stroke::new(1., p.border),
                StrokeKind::Inside,
            );
        }
        let color = if active || cell_response.hovered() { p.foreground } else { p.muted_fg };
        let font = if active { fonts::medium(TEXT_SM) } else { fonts::regular(TEXT_SM) };
        painter.text(cell.center(), Align2::CENTER_CENTER, *label, font, color);
    }
    response
}
