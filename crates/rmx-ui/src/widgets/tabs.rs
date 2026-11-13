//! The tab strip: tabs slide to their places, and can be dragged into a new order.

use std::collections::HashMap;
use std::time::Instant;

use egui::{CornerRadius, Id, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use rmx_layout::anim::{Curve, Tween};

use crate::action::{Protocol, TabId, UiAction};
use crate::draw::{elided_at, icon, palette};
use crate::theme::{RADIUS_SM, STRIP_HEIGHT, TAB_HEIGHT, TAB_SLIDE_MS, TEXT_SM};
use crate::{fonts, icons};

use super::controls::{InputOptions, paint_close_button, paint_icon_button, submitted, text_input};
use super::pane_bar::PaneStatus;

/// The widest a tab grows.
pub const TAB_MAX_WIDTH: f32 = 200.;
/// The narrowest a tab shrinks; beyond that the strip scrolls.
pub const TAB_MIN_WIDTH: f32 = 96.;
/// The space between tabs.
pub const TAB_GAP: f32 = 2.;
const NEW_BUTTON: f32 = 28.;
/// How far off the strip the pointer goes before a dragged tab is taken out of it, in points.
const TAB_OUT_MARGIN: f32 = 16.;

/// What a tab shows.
#[derive(Clone, Copy, Debug)]
pub struct TabView<'a> {
    pub id: TabId,
    pub label: &'a str,
    pub glyph: char,
    /// What kind of connection it is, for the color of its icon.
    pub protocol: Option<Protocol>,
    pub active: bool,
    pub status: PaneStatus,
}

struct Drag {
    id: TabId,
    from: usize,
    to: usize,
    /// Where in the tab the pointer took hold.
    grab: f32,
    /// The left edge of the dragged tab, relative to the strip.
    x: f32,
}

/// What a tab strip remembers between frames.
#[derive(Default)]
pub struct TabStripState {
    positions: HashMap<TabId, Tween<f32>>,
    drag: Option<Drag>,
    scroll: f32,
    last_active: Option<TabId>,
    /// Where each tab was last drawn, in points.
    rects: Vec<(TabId, Rect)>,
    /// The tab being named where it shows, and what was typed.
    renaming: Option<(TabId, String)>,
    /// The field has been given the keyboard.
    rename_focused: bool,
}

impl TabStripState {
    /// Edit the name of tab `id`, starting from `name`.
    pub fn start_rename(&mut self, id: TabId, name: &str) {
        self.renaming = Some((id, name.to_owned()));
        self.rename_focused = false;
    }

    /// Whether a tab is being dragged.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The index a tab put down at `x`, in points, goes to: before the first tab it is left of.
    pub fn slot_at(&self, x: f32) -> usize {
        self.rects.iter().position(|(_, rect)| x < rect.center().x).unwrap_or(self.rects.len())
    }

    /// Where the tabs were last drawn, in points and in the order of the strip.
    pub fn rects(&self) -> &[(TabId, Rect)] {
        &self.rects
    }
}

/// The width of each of `count` tabs in `available` points, leaving room for the new-tab button.
pub fn tab_width(available: f32, count: usize) -> f32 {
    if count == 0 {
        return TAB_MAX_WIDTH;
    }
    let room = available - NEW_BUTTON - TAB_GAP;
    ((room - TAB_GAP * (count - 1) as f32) / count as f32).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// The left edge of slot `index`.
pub fn slot_x(index: usize, width: f32) -> f32 {
    index as f32 * (width + TAB_GAP)
}

/// The slot a tab dragged to left edge `x` falls into: the one its center is over.
pub fn drop_index(x: f32, width: f32, count: usize) -> usize {
    let center = x + width / 2.;
    let slot = (center / (width + TAB_GAP)).floor();
    (slot.max(0.) as usize).min(count.saturating_sub(1))
}

/// The slot of the tab at `index` while the tab at `from` is held over slot `to`.
pub fn preview_slot(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < to && (from + 1..=to).contains(&index) {
        index - 1
    } else if to < from && (to..from).contains(&index) {
        index + 1
    } else {
        index
    }
}

/// Draw the strip of `tabs` in `rect`. `rect` is the strip's full 40 px height.
pub fn tab_strip(
    ui: &mut Ui,
    rect: Rect,
    tabs: &[TabView<'_>],
    state: &mut TabStripState,
    now: Instant,
) -> Option<UiAction> {
    let id = Id::unique("tab-strip");
    let mut action = None;
    let width = tab_width(rect.width(), tabs.len());
    let top = rect.top() + (STRIP_HEIGHT - TAB_HEIGHT) / 2.;
    let content = slot_x(tabs.len(), width) + NEW_BUTTON;
    let max_scroll = (content - rect.width()).max(0.);

    // Keep the active tab in view as it changes, and let the wheel move the rest.
    let active = tabs.iter().position(|tab| tab.active);
    let active_id = active.map(|i| tabs[i].id);
    if let Some(index) = active.filter(|_| active_id != state.last_active) {
        let left = slot_x(index, width);
        if left < state.scroll {
            state.scroll = left;
        } else if left + width > state.scroll + rect.width() - NEW_BUTTON {
            state.scroll = left + width + NEW_BUTTON - rect.width();
        }
        state.last_active = active_id;
    }
    if ui.rect_contains_pointer(rect) && max_scroll > 0. {
        let delta = ui.input(|i| i.smooth_scroll_delta);
        state.scroll -= delta.y + delta.x;
    }
    state.scroll = state.scroll.clamp(0., max_scroll);

    // Forget tabs that are gone; a tab that has just appeared starts in its own slot.
    state.positions.retain(|id, _| tabs.iter().any(|tab| tab.id == *id));
    if state
        .drag
        .as_ref()
        .is_some_and(|drag| tabs.get(drag.from).is_none_or(|tab| tab.id != drag.id))
    {
        state.drag = None;
    }
    if let Some(drag) = &mut state.drag {
        drag.to = drop_index(drag.x, width, tabs.len());
    }

    let painter = ui.painter_at(rect);
    let mut moving = false;
    let mut drawn = Vec::with_capacity(tabs.len());
    let (from, to) = state.drag.as_ref().map_or((0, 0), |drag| (drag.from, drag.to));
    for (index, tab) in tabs.iter().enumerate() {
        let slot = preview_slot(index, from, to);
        let target = slot_x(slot, width);
        let held = state.drag.as_ref().filter(|drag| drag.id == tab.id).map(|drag| drag.x);
        let tween = state.positions.entry(tab.id).or_insert_with(|| Tween::new(target));
        let x = match held {
            Some(x) => {
                // The tab under the pointer follows it exactly; it slides from here on release.
                *tween = Tween::new(x);
                x
            },
            None => {
                tween.to(target, now, TAB_SLIDE_MS, Curve::SNAP);
                moving |= tween.running(now);
                tween.get(now)
            },
        };
        let tab_rect = Rect::from_min_size(
            pos2((rect.left() + x - state.scroll).round(), top),
            vec2(width, TAB_HEIGHT),
        );
        drawn.push((index, tab, tab_rect));
    }

    state.rects.clear();
    state
        .rects
        .extend(drawn.iter().map(|&(_, tab, rect): &(usize, &TabView<'_>, Rect)| (tab.id, rect)));

    // The held tab goes last so that it passes over the others.
    if let Some(drag) = &state.drag {
        let held = drag.id;
        drawn.sort_by_key(|(_, tab, _)| tab.id == held);
    }

    for (index, tab, tab_rect) in drawn {
        let response =
            ui.interact(tab_rect.intersect(rect), id.with(tab.id.0), Sense::click_and_drag());
        let close_rect = Rect::from_center_size(
            pos2(tab_rect.right() - 14., tab_rect.center().y),
            Vec2::splat(20.),
        );
        let hovered =
            response.hovered() || ui.rect_contains_pointer(close_rect.intersect(tab_rect));
        let held = state.drag.as_ref().is_some_and(|drag| drag.id == tab.id);

        let p = palette(ui);
        if tab.active || held {
            painter.rect_filled(tab_rect, CornerRadius::same(RADIUS_SM), p.accent);
        } else if hovered {
            painter.rect_filled(tab_rect, CornerRadius::same(RADIUS_SM), p.hover);
        }

        let color = if tab.active { p.foreground } else { p.muted_fg };
        let y = tab_rect.center().y;
        let tint = tab.protocol.map_or(color, |protocol| p.protocol(protocol));
        icon(&painter, pos2(tab_rect.left() + 15., y), tab.glyph, 14., tint);
        let dot = match tab.status {
            PaneStatus::Connected => None,
            PaneStatus::Connecting => Some(p.warning),
            PaneStatus::Disconnected => Some(p.muted_fg),
            PaneStatus::Failed => Some(p.danger),
        };
        if let Some(dot) = dot {
            painter.circle(pos2(tab_rect.left() + 21., y + 5.), 3., dot, Stroke::new(1., p.chrome));
        }
        let show_close = tab.active || hovered;
        let right = tab_rect.right() - if show_close { 26. } else { 8. };
        let x = tab_rect.left() + 28.;
        let font = if tab.active { fonts::medium(TEXT_SM) } else { fonts::regular(TEXT_SM) };
        let renaming = state.renaming.as_ref().is_some_and(|(id, _)| *id == tab.id);
        if !renaming {
            elided_at(&painter, x, y, tab.label, font, color, right - x);
        }

        if show_close && !held {
            let close = ui.interact(close_rect, id.with(("close", tab.id.0)), Sense::click());
            paint_close_button(ui, close_rect.shrink(1.), 12., &close);
            if close.clicked() {
                action = Some(UiAction::TabClose(tab.id));
            }
        }

        if response.double_clicked() {
            action = Some(UiAction::TabStartRename(tab.id));
        } else if response.clicked() {
            action = Some(UiAction::TabActivate(tab.id));
        }
        if response.middle_clicked() {
            action = Some(UiAction::TabClose(tab.id));
        }
        if response.secondary_clicked() {
            let pos = response.interact_pointer_pos().unwrap_or(tab_rect.center());
            action = Some(UiAction::TabContextMenu { id: tab.id, pos });
        }

        if response.drag_started_by(egui::PointerButton::Primary) && !renaming {
            let pointer = response.interact_pointer_pos().map_or(tab_rect.left(), |pos| pos.x);
            state.drag = Some(Drag {
                id: tab.id,
                from: index,
                to: index,
                grab: pointer - tab_rect.left(),
                x: tab_rect.left() - rect.left() + state.scroll,
            });
        }
        if let Some(drag) = state.drag.as_mut().filter(|drag| drag.id == tab.id) {
            if response.dragged_by(egui::PointerButton::Primary)
                && let Some(pos) = response.interact_pointer_pos()
            {
                let limit = slot_x(tabs.len().saturating_sub(1), width);
                drag.x = (pos.x - drag.grab - rect.left() + state.scroll).clamp(0., limit);
                ui.ctx().request_repaint();
                // Off the strip it is no longer about the order of the tabs.
                if !rect.expand2(vec2(TAB_OUT_MARGIN, TAB_OUT_MARGIN)).contains(pos) {
                    let drag = state.drag.take().expect("this tab is being dragged");
                    if let Some(tween) = state.positions.get_mut(&tab.id) {
                        tween.to(slot_x(drag.from, width), now, TAB_SLIDE_MS, Curve::SNAP);
                    }
                    moving = true;
                    action = Some(UiAction::TabDragOut(tab.id));
                }
            }
            if response.drag_stopped() {
                let drag = state.drag.take().expect("this tab is being dragged");
                if drag.to != drag.from {
                    action = Some(UiAction::TabReorder { id: drag.id, to: drag.to });
                }
                // Slide the dropped tab into its slot from where it was let go.
                if let Some(tween) = state.positions.get_mut(&tab.id) {
                    tween.to(slot_x(drag.to, width), now, TAB_SLIDE_MS, Curve::SNAP);
                }
                moving = true;
            }
        }
    }

    action = action.or_else(|| rename_field(ui, id, state));

    let new_rect = Rect::from_min_size(
        pos2(rect.left() + slot_x(tabs.len(), width) - state.scroll, top),
        Vec2::splat(TAB_HEIGHT),
    );
    let new_tab = ui.interact(new_rect.intersect(rect), id.with("new"), Sense::click());
    paint_icon_button(ui, new_rect, icons::PLUS, 14., &new_tab);
    if new_tab.clicked() {
        action = Some(UiAction::TabNew);
    }

    if moving {
        ui.ctx().request_repaint();
    }
    action
}

/// The field naming a tab, over its title. Enter or leaving it commits, Escape keeps the old name.
fn rename_field(ui: &mut Ui, id: Id, state: &mut TabStripState) -> Option<UiAction> {
    let tab = state.renaming.as_ref()?.0;
    let Some(&(_, rect)) = state.rects.iter().find(|(id, _)| *id == tab) else {
        state.renaming = None;
        return None;
    };
    let field = Rect::from_min_max(
        pos2(rect.left() + 24., rect.top() + 2.),
        pos2(rect.right() - 4., rect.bottom() - 2.),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id.with("rename")).max_rect(field));
    let (_, text) = state.renaming.as_mut()?;
    let input = text_input(
        &mut child,
        id.with("rename-field"),
        text,
        InputOptions {
            width: Some(field.width()),
            height: Some(field.height()),
            ..Default::default()
        },
    );
    if !std::mem::replace(&mut state.rename_focused, true) {
        input.request_focus();
        // Typing replaces the old name.
        if let Some(mut edit) = egui::TextEdit::load_state(ui.ctx(), input.id) {
            let (start, end) =
                (egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
            edit.cursor.set_char_range(Some(egui::text::CCursorRange::two(start, end)));
            edit.store(ui.ctx(), input.id);
        }
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.renaming = None;
        return None;
    }
    if submitted(ui, &input) || input.lost_focus() {
        let (id, text) = state.renaming.take()?;
        let name = text.trim();
        return Some(UiAction::TabRename { id, name: (!name.is_empty()).then(|| name.to_owned()) });
    }
    None
}
