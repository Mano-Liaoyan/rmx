//! The bar on top of each pane: which session this is and how it is doing, a handle for moving it,
//! and, under the pointer, what can be done with it.

use std::time::Instant;

use egui::{CornerRadius, CursorIcon, Id, Rect, Sense, Ui, Vec2, pos2};

use crate::action::{PaneId, UiAction};
use crate::draw::{elided_at, hline, palette};
use crate::theme::{PANE_BAR_HEIGHT, RADIUS_SM, TEXT_SM, TEXT_XS};
use crate::{fonts, icons};

use super::controls::paint_icon_button;
use super::panels::Reveal;

/// How far the controls drop into place as they appear.
const DROP: f32 = 4.;
const BUTTON: f32 = PANE_BAR_HEIGHT - 4.;

/// The state of a pane's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneStatus {
    Connected,
    Connecting,
    Disconnected,
    Failed,
}

/// What a pane's bar shows.
#[derive(Clone, Copy, Debug)]
pub struct PaneBarView<'a> {
    pub pane: PaneId,
    pub status: PaneStatus,
    pub title: &'a str,
    /// What there is to know besides the name, shown on hovering it.
    pub tooltip: &'a str,
    pub focused: bool,
    /// The tab has other panes, so this one can be zoomed.
    pub grouped: bool,
    pub zoomed: bool,
    /// Whether its files can be opened.
    pub files: bool,
    /// Whether it is in the broadcast group, for a pane that takes typing; `None` for one that
    /// cannot join.
    pub broadcast: Option<bool>,
}

/// What the badge of a pane in the broadcast group says.
const BROADCAST_BADGE: &str = "Broadcast";

/// Draw the bar of a pane in `rect`, which is [`PANE_BAR_HEIGHT`] tall.
pub fn pane_bar(
    ui: &mut Ui,
    rect: Rect,
    view: &PaneBarView<'_>,
    reveal: &mut Reveal,
    now: Instant,
) -> Option<UiAction> {
    let p = palette(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0., p.background);
    hline(&painter, rect.left(), rect.right(), rect.bottom() - 1., p.border);

    let y = rect.center().y - 0.5;
    let dot = match view.status {
        PaneStatus::Connected => p.success,
        PaneStatus::Connecting => p.warning,
        PaneStatus::Disconnected => p.muted_fg,
        PaneStatus::Failed => p.danger,
    };
    painter.circle_filled(pos2(rect.left() + 12., y), 3., dot);

    let pane = view.pane;
    let id = Id::unique(("pane-bar", pane.0));
    let mut action = None;
    let grab = ui.interact(rect, id.with("grab"), Sense::click_and_drag());
    if grab.drag_started_by(egui::PointerButton::Primary) {
        action = Some(UiAction::PaneDragStart(pane));
    } else if grab.double_clicked() && view.grouped {
        action = Some(UiAction::PaneZoom(pane));
    } else if grab.clicked() {
        action = Some(UiAction::PaneFocus(pane));
    }
    if grab.secondary_clicked() {
        let pos = grab.interact_pointer_pos().unwrap_or(rect.center());
        action = Some(UiAction::PaneMenu { id: pane, pos });
    }

    let near = ui.rect_contains_pointer(rect);
    let shown = reveal.level(near, now);
    match reveal.wake(now) {
        Some(at) if at <= now => ui.ctx().request_repaint(),
        Some(at) => ui.ctx().request_repaint_after(at - now),
        None => (),
    }

    // The buttons, from the right; what is not shown takes no room and no clicks.
    let reconnect = matches!(view.status, PaneStatus::Disconnected | PaneStatus::Failed);
    let zoom_glyph = if view.zoomed { icons::COPY } else { icons::SQUARE };
    let mut buttons = vec![(icons::X, "Close", UiAction::PaneClose(pane))];
    if view.grouped {
        buttons.push((zoom_glyph, "Zoom", UiAction::PaneZoom(pane)));
    }
    buttons.push((icons::ROWS_2, "Split down", UiAction::PaneSplitDown(pane)));
    buttons.push((icons::COLUMNS_2, "Split right", UiAction::PaneSplitRight(pane)));
    if let Some(joined) = view.broadcast {
        let tip = if joined { "Stop broadcasting" } else { "Broadcast input" };
        buttons.push((icons::CABLE, tip, UiAction::PaneBroadcast(pane)));
    }
    if view.files {
        buttons.push((icons::FOLDER_SYNC, "Files", UiAction::PaneOpenFiles(pane)));
    }
    if reconnect {
        buttons.push((icons::PLUG, "Reconnect", UiAction::PaneReconnect(pane)));
    }
    let room = buttons.len() as f32 * (BUTTON + 2.) + 4.;

    let mut x = rect.right() - 2.;
    if shown > 0. {
        let builder = egui::UiBuilder::new().id_salt(id.with("controls")).max_rect(rect);
        let mut child = ui.new_child(builder);
        child.set_opacity(shown);
        let dy = ((shown - 1.) * DROP).round();
        for (glyph, tip, button_action) in buttons {
            x -= BUTTON;
            let cell = Rect::from_min_size(pos2(x, rect.top() + 2. + dy), Vec2::splat(BUTTON));
            x -= 2.;
            let response = child.interact(cell, id.with(tip), Sense::click()).on_hover_text(tip);
            paint_icon_button(&child, cell, glyph, 12., &response);
            if response.clicked() {
                action = Some(button_action);
            }
        }
    }

    // The name and where it goes give way smoothly as the buttons come.
    let end = rect.right() - 8. - room * shown;
    let mut start = rect.left() + 22.;
    // In the broadcast group, a badge says so before the name, whether the pointer is there or not.
    if view.broadcast == Some(true) {
        let galley =
            painter.layout_no_wrap(BROADCAST_BADGE.to_owned(), fonts::medium(TEXT_XS), p.warning);
        let size = galley.size() + Vec2::new(12., 2.);
        let badge = Rect::from_min_size(pos2(start, y - size.y / 2.), size);
        painter.rect_filled(badge, CornerRadius::same(RADIUS_SM), p.warning.gamma_multiply(0.18));
        painter.galley(badge.min + Vec2::new(6., 1.), galley, p.warning);
        start = badge.right() + 8.;
    }
    let (font, color) = if view.focused {
        (fonts::medium(TEXT_SM), p.foreground)
    } else {
        (fonts::regular(TEXT_SM), p.muted_fg)
    };
    let title = elided_at(&painter, start, y, view.title, font, color, (end - start).max(0.));
    if grab.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
        let name = Rect::from_min_max(
            pos2(start, rect.top()),
            pos2((start + title).min(end), rect.bottom()),
        );
        let over_name = ui.input(|i| i.pointer.hover_pos()).is_some_and(|pos| name.contains(pos));
        if over_name && !view.tooltip.is_empty() {
            grab.on_hover_text(view.tooltip);
        }
    }
    action
}
