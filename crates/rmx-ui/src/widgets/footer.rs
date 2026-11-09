//! The strip under each pane.

use egui::{Id, Rect, Sense, Ui, Vec2, pos2};

use crate::action::{PaneId, UiAction};
use crate::draw::{elided_at, hline, palette, snap};
use crate::theme::{FOOTER_HEIGHT, TEXT_SM};
use crate::{fonts, icons};

use super::controls::paint_icon_button;

/// The state of a pane's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneStatus {
    Connected,
    Connecting,
    Disconnected,
    Failed,
}

/// What a pane's footer shows.
#[derive(Clone, Copy, Debug)]
pub struct PaneFooterView<'a> {
    pub pane: PaneId,
    pub status: PaneStatus,
    pub title: &'a str,
    /// Short facts shown at the right: size, latency, encoding.
    pub facts: &'a [&'a str],
}

/// Draw a pane's footer in `rect`, which is [`FOOTER_HEIGHT`] tall.
pub fn pane_footer(ui: &mut Ui, rect: Rect, view: &PaneFooterView<'_>) -> Option<UiAction> {
    let p = palette(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0., p.chrome);
    hline(&painter, rect.left(), rect.right(), rect.top(), p.border);

    let y = rect.center().y + 0.5;
    let dot = match view.status {
        PaneStatus::Connected => p.success,
        PaneStatus::Connecting => p.warning,
        PaneStatus::Disconnected => p.muted_fg,
        PaneStatus::Failed => p.danger,
    };
    painter.circle_filled(pos2(rect.left() + 12., y), 3., dot);

    let id = Id::unique(("pane-footer", view.pane.0));
    let mut action = None;
    let grab = ui.interact(rect, id.with("grab"), Sense::click_and_drag());
    if grab.drag_started_by(egui::PointerButton::Primary) {
        action = Some(UiAction::PaneDragStart(view.pane));
    }
    let mut right = rect.right() - 2.;
    let button = |ui: &mut Ui, right: &mut f32, salt: &str, glyph: char| {
        let side = FOOTER_HEIGHT - 4.;
        *right -= side;
        let cell = Rect::from_min_size(pos2(*right, rect.top() + 2.), Vec2::splat(side));
        *right -= 2.;
        let response = ui.interact(cell, id.with(salt), Sense::click());
        paint_icon_button(ui, cell, glyph, 12., &response);
        response
    };
    let more = button(ui, &mut right, "more", icons::ELLIPSIS);
    if more.clicked() {
        let pos = more.rect.left_top();
        action = Some(UiAction::PaneMenu { id: view.pane, pos });
    }
    if button(ui, &mut right, "down", icons::ROWS_2).clicked() {
        action = Some(UiAction::PaneSplitDown(view.pane));
    }
    if button(ui, &mut right, "right", icons::COLUMNS_2).clicked() {
        action = Some(UiAction::PaneSplitRight(view.pane));
    }
    right -= 6.;

    for fact in view.facts.iter().rev() {
        let galley =
            painter.layout_no_wrap((*fact).to_owned(), fonts::regular(TEXT_SM), p.muted_fg);
        right -= galley.size().x;
        if right < rect.left() + 120. {
            break;
        }
        let top = snap(&painter, y - galley.size().y / 2.);
        painter.galley(pos2(right, top), galley, p.muted_fg);
        right -= 14.;
    }

    let x = rect.left() + 22.;
    elided_at(&painter, x, y, view.title, fonts::regular(TEXT_SM), p.foreground, right - x);
    action
}
