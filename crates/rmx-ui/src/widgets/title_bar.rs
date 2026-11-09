//! The single row at the top of the window: drag region, tabs and window controls.

use std::time::Instant;

use egui::{Id, Rect, Sense, Ui, Vec2, pos2};

use crate::action::UiAction;
use crate::draw::{hline, palette, vline};
use crate::icons;
use crate::theme::{SIDEBAR_WIDTH, STRIP_HEIGHT, TAB_HEIGHT};

use super::controls::paint_icon_button;
use super::tabs::{TabStripState, TabView, tab_strip};

const CONTROL: f32 = TAB_HEIGHT;
const CONTROL_GAP: f32 = 4.;
const EDGE: f32 = 6.;
/// Empty strip always left to grab, however many tabs are open.
const MIN_DRAG: f32 = 48.;

/// What the title bar shows.
#[derive(Clone, Copy, Debug)]
pub struct TitleBarView<'a> {
    pub tabs: &'a [TabView<'a>],
    pub maximized: bool,
    pub sidebar_open: bool,
}

/// Draw the title bar in `rect`, which is [`STRIP_HEIGHT`] tall.
pub fn title_bar(
    ui: &mut Ui,
    rect: Rect,
    view: &TitleBarView<'_>,
    tabs: &mut TabStripState,
    now: Instant,
) -> Option<UiAction> {
    let id = Id::unique("title-bar");
    let p = palette(ui);
    let painter = ui.painter_at(rect);
    let top = rect.top() + (STRIP_HEIGHT - CONTROL) / 2.;
    let mut action = None;

    painter.rect_filled(rect, 0., p.chrome);
    let lead = if view.sidebar_open { SIDEBAR_WIDTH } else { CONTROL + EDGE * 2. };
    let lead_rect = Rect::from_min_size(rect.min, egui::vec2(lead, rect.height()));
    if view.sidebar_open {
        painter.rect_filled(lead_rect, 0., p.sidebar);
        vline(&painter, lead_rect.right() - 1., rect.top(), rect.bottom(), p.border);
    }
    hline(&painter, rect.left(), rect.right(), rect.bottom() - 1., p.border);

    // Everything else is laid on top of this, and egui gives the topmost widget the pointer.
    let drag = ui.interact(rect, id.with("drag"), Sense::click_and_drag());
    if drag.double_clicked() {
        action = Some(UiAction::WindowToggleMaximize);
    } else if drag.drag_started_by(egui::PointerButton::Primary) {
        action = Some(UiAction::WindowDrag);
    }

    let toggle = Rect::from_min_size(pos2(rect.left() + EDGE, top), Vec2::splat(CONTROL));
    let response = ui.interact(toggle, id.with("sidebar"), Sense::click());
    paint_icon_button(ui, toggle, icons::PANEL_LEFT, 14., &response);
    if response.clicked() {
        action = Some(UiAction::ToggleSidebar);
    }

    let mut right = rect.right() - EDGE;
    let controls = [
        ("close", icons::X, UiAction::WindowClose),
        (
            "maximize",
            if view.maximized { icons::COPY } else { icons::SQUARE },
            UiAction::WindowToggleMaximize,
        ),
        ("minimize", icons::MINUS, UiAction::WindowMinimize),
    ];
    for (salt, glyph, control_action) in controls {
        right -= CONTROL;
        let cell = Rect::from_min_size(pos2(right, top), Vec2::splat(CONTROL));
        right -= CONTROL_GAP;
        let response = ui.interact(cell, id.with(salt), Sense::click());
        paint_icon_button(ui, cell, glyph, 14., &response);
        if response.clicked() {
            action = Some(control_action);
        }
    }

    let strip = Rect::from_min_max(
        pos2(lead_rect.right() + EDGE, rect.top()),
        pos2((right - MIN_DRAG).max(lead_rect.right() + EDGE), rect.bottom()),
    );
    action.or(tab_strip(ui, strip, view.tabs, tabs, now))
}
