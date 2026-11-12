//! What a pane shows over its content about its connection.

use egui::{Align2, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use crate::action::{PaneId, UiAction};
use crate::draw::{elided_at, icon, palette};
use crate::theme::{CONTROL_HEIGHT, RADIUS, TEXT_LG, TEXT_SM};
use crate::{fonts, icons};

use super::controls::{ButtonKind, button_full, paint_icon_button};

const CARD: Vec2 = vec2(360., 148.);
const BANNER_HEIGHT: f32 = 44.;
const NOTICE_HEIGHT: f32 = 28.;
const MARGIN: f32 = 8.;

/// The state of a pane's connection, when it has something to say.
#[derive(Clone, Copy, Debug)]
pub enum PaneOverlay<'a> {
    /// Connecting to `target`, at `stage`. With `screen`, an earlier session's text stays visible.
    Connecting { target: &'a str, stage: &'a str, screen: bool },
    /// The session is over, its screen still readable.
    Closed { reason: &'a str },
    /// Connecting did not work.
    Failed { target: &'a str, reason: &'a str, screen: bool },
    /// Connecting would take the user, who has not asked for it yet.
    Awaiting { target: &'a str, why: &'a str, screen: bool },
}

/// Where the overlay of `pane`, drawn over `area`, goes.
pub fn overlay_rect(area: Rect, overlay: &PaneOverlay<'_>) -> Rect {
    let card = match overlay {
        PaneOverlay::Connecting { screen, .. }
        | PaneOverlay::Failed { screen, .. }
        | PaneOverlay::Awaiting { screen, .. } => !*screen,
        PaneOverlay::Closed { .. } => false,
    };
    if card {
        Rect::from_center_size(area.center(), CARD.min(area.size()))
    } else {
        let height = BANNER_HEIGHT.min(area.height());
        let width = (area.width() - 2. * MARGIN).max(1.);
        Rect::from_min_size(
            pos2(area.left() + MARGIN, area.bottom() - height - MARGIN),
            vec2(width, height),
        )
    }
}

/// Draw `overlay` over the terminal area `area` of `pane`.
pub fn pane_overlay(
    ui: &mut Ui,
    area: Rect,
    pane: PaneId,
    overlay: &PaneOverlay<'_>,
) -> Option<UiAction> {
    let rect = overlay_rect(area, overlay);
    let p = palette(ui);
    let id = Id::unique(("pane-overlay", pane.0));
    let painter = ui.painter_at(rect.expand(1.));
    painter.rect(
        rect,
        CornerRadius::same(RADIUS),
        p.chrome,
        Stroke::new(1., p.border),
        StrokeKind::Inside,
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(id));
    let ui = &mut child;

    let (glyph, color, line, detail, busy) = match *overlay {
        PaneOverlay::Connecting { target, stage, .. } => {
            (icons::LOADER_CIRCLE, p.warning, target, stage, true)
        },
        PaneOverlay::Closed { reason } => (icons::PLUG, p.muted_fg, "Session ended", reason, false),
        PaneOverlay::Failed { target, reason, .. } => {
            (icons::CIRCLE_ALERT, p.danger, target, reason, false)
        },
        PaneOverlay::Awaiting { target, why, .. } => (icons::PLUG, p.muted_fg, target, why, false),
    };
    if busy {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }

    let mut action = None;
    let is_card = matches!(
        overlay,
        PaneOverlay::Connecting { screen: false, .. }
            | PaneOverlay::Failed { screen: false, .. }
            | PaneOverlay::Awaiting { screen: false, .. }
    );
    let primary = match overlay {
        _ if busy => "Cancel",
        PaneOverlay::Awaiting { .. } => "Connect",
        _ => "Reconnect",
    };
    let primary_action =
        if busy { UiAction::PaneCancel(pane) } else { UiAction::PaneReconnect(pane) };

    if is_card {
        let x = rect.left() + 16.;
        icon(&painter, pos2(x + 8., rect.top() + 26.), glyph, 16., color);
        let width = rect.width() - 62.;
        elided_at(
            &painter,
            x + 24.,
            rect.top() + 26.,
            line,
            fonts::medium(TEXT_LG),
            p.foreground,
            width,
        );
        painter.text(
            pos2(x, rect.top() + 56.),
            Align2::LEFT_CENTER,
            wrapped(detail, 54),
            fonts::regular(TEXT_SM),
            p.muted_fg,
        );
        let row = Rect::from_min_size(
            pos2(rect.left() + 16., rect.bottom() - CONTROL_HEIGHT - 14.),
            vec2(rect.width() - 32., CONTROL_HEIGHT),
        );
        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        if button_full(&mut row_ui, None, primary, ButtonKind::Primary, true, None).clicked() {
            action = Some(primary_action.clone());
        }
        if !busy
            && button_full(&mut row_ui, None, "Close", ButtonKind::Secondary, true, None).clicked()
        {
            action = Some(UiAction::PaneClose(pane));
        }
    } else {
        let y = rect.center().y;
        icon(&painter, pos2(rect.left() + 18., y), glyph, 14., color);
        let label = format!("{line}: {detail}");
        let room = rect.width() - 150.;
        let font = fonts::regular(TEXT_SM);
        elided_at(&painter, rect.left() + 34., y, &label, font, p.foreground, room);
        let row = Rect::from_min_size(
            pos2(rect.right() - 106., rect.top() + (rect.height() - CONTROL_HEIGHT) / 2.),
            vec2(100., CONTROL_HEIGHT),
        );
        let mut row_ui = ui.new_child(egui::UiBuilder::new().max_rect(row));
        let kind = ButtonKind::Secondary;
        if button_full(&mut row_ui, None, primary, kind, true, Some(100.)).clicked() {
            action = Some(primary_action);
        }
    }
    action
}

/// A reason short enough for one line of the card, keeping its end when it must be cut.
fn wrapped(text: &str, columns: usize) -> String {
    let count = text.chars().count();
    if count <= columns {
        text.to_owned()
    } else {
        let tail: String = text.chars().skip(count - (columns - 1)).collect();
        format!("…{tail}")
    }
}

/// Where the notice of a pane goes.
pub fn notice_rect(area: Rect) -> Rect {
    Rect::from_min_size(
        area.left_top() + vec2(MARGIN, MARGIN),
        vec2((area.width() - 2. * MARGIN).max(1.), NOTICE_HEIGHT.min(area.height())),
    )
}

/// A dismissible line across the top of `area`, saying that something happened to the pane.
pub fn pane_notice(ui: &mut Ui, area: Rect, pane: PaneId, text: &str) -> Option<UiAction> {
    let rect = notice_rect(area);
    let p = palette(ui);
    let id = Id::unique(("pane-notice", pane.0));
    let painter = ui.painter_at(rect.expand(1.));
    painter.rect(
        rect,
        CornerRadius::same(RADIUS),
        p.chrome,
        Stroke::new(1., p.warning),
        StrokeKind::Inside,
    );
    let y = rect.center().y;
    icon(&painter, pos2(rect.left() + 16., y), icons::TRIANGLE_ALERT, 13., p.warning);
    let font = fonts::regular(TEXT_SM);
    elided_at(&painter, rect.left() + 30., y, text, font, p.foreground, rect.width() - 62.);

    let cell = Rect::from_min_size(pos2(rect.right() - 26., rect.top() + 2.), Vec2::splat(24.));
    let response = ui.interact(cell, id, Sense::click());
    paint_icon_button(ui, cell, icons::X, 12., &response);
    response.clicked().then_some(UiAction::PaneDismissNotice(pane))
}
