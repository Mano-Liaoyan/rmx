//! What the pane area shows with no tabs open.

use egui::{Align2, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::action::UiAction;
use crate::draw::{icon, palette};
use crate::theme::{CONTROL_HEIGHT, RADIUS, TEXT_SM};
use crate::{fonts, icons};

const WIDTH: f32 = 280.;
const GAP: f32 = 8.;

/// A line offering something to open, with the key that does the same.
fn offer(ui: &mut Ui, at: Rect, glyph: char, label: &str, keys: &str) -> bool {
    let response = ui.allocate_rect(at, Sense::click());
    let p = palette(ui);
    let fill = if response.is_pointer_button_down_on() {
        p.accent
    } else if response.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect(
        at,
        CornerRadius::same(RADIUS),
        fill,
        Stroke::new(1., p.border),
        StrokeKind::Inside,
    );
    icon(painter, pos2(at.left() + 19., at.center().y), glyph, 14., p.muted_fg);
    painter.text(
        pos2(at.left() + 34., at.center().y),
        Align2::LEFT_CENTER,
        label,
        fonts::regular(TEXT_SM),
        p.foreground,
    );
    painter.text(
        pos2(at.right() - 12., at.center().y),
        Align2::RIGHT_CENTER,
        keys,
        fonts::regular(TEXT_SM),
        p.muted_fg,
    );
    response.clicked()
}

/// Draw the page, centered in `rect`.
pub fn empty_state(ui: &mut Ui, rect: Rect) -> Option<UiAction> {
    let height = 2. * CONTROL_HEIGHT + GAP;
    let top = rect.center().y - height / 2.;
    let left = rect.center().x - WIDTH / 2.;
    let first = Rect::from_min_size(pos2(left, top), vec2(WIDTH, CONTROL_HEIGHT));
    let second = first.translate(vec2(0., CONTROL_HEIGHT + GAP));
    let shell = offer(ui, first, icons::SQUARE_TERMINAL, "New local shell", "Ctrl+Shift+T");
    let connection = offer(ui, second, icons::PLUS, "New connection", "Ctrl+Shift+N");
    shell.then_some(UiAction::TabNew).or(connection.then_some(UiAction::NewConnection))
}
