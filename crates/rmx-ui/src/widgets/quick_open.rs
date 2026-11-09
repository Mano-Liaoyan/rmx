//! Quick Open: the command palette.

use egui::{Area, Context, CornerRadius, Id, Key, Modifiers, Order, Rect, Sense, Ui, pos2, vec2};

use crate::action::UiAction;
use crate::draw::{elided_at, hline, icon, palette};
use crate::theme::{RADIUS_SM, TEXT_SM};
use crate::virtual_list::{clamp_scroll, scroll_to_row, window};
use crate::{float, fonts, icons};

use super::controls::{InputOptions, text_input};

const WIDTH: f32 = 560.;
const ROW: f32 = 32.;
const MAX_ROWS: usize = 10;
const INPUT: f32 = 40.;
const TOP: f32 = 88.;

/// A candidate. `id` comes back in [`UiAction::QuickOpenPick`].
#[derive(Clone, Copy, Debug)]
pub struct QuickItem<'a> {
    pub id: u64,
    pub glyph: char,
    pub label: &'a str,
    pub detail: &'a str,
}

/// The palette's state.
#[derive(Clone, Debug, Default)]
pub struct QuickOpenState {
    open: bool,
    query: String,
    selected: usize,
    scroll: f64,
    matches: Vec<u32>,
    filtered_for: Option<(String, usize)>,
}

impl QuickOpenState {
    /// Show the palette, empty.
    pub fn open(&mut self) {
        *self = Self { open: true, ..Self::default() };
    }

    /// Hide it.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Whether it is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Replace the query, as if typed.
    pub fn set_query(&mut self, query: &str) {
        query.clone_into(&mut self.query);
    }
}

/// How well `query`, lowercased, fits `label`: lower is better, `None` is no match. An exact
/// substring beats a scattered subsequence, and an earlier one beats a later one.
pub fn match_score(query: &str, label: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let lower = label.to_lowercase();
    if let Some(at) = lower.find(query) {
        return Some(at as u32);
    }
    let mut chars = lower.char_indices();
    let mut last = 0;
    for wanted in query.chars() {
        (last, _) = chars.find(|&(_, c)| c == wanted)?;
    }
    Some(1000 + last as u32)
}

fn refilter(state: &mut QuickOpenState, items: &[QuickItem<'_>]) {
    if state.filtered_for.as_ref().is_some_and(|(q, n)| *q == state.query && *n == items.len()) {
        return;
    }
    let query = state.query.to_lowercase();
    let mut scored: Vec<(u32, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match_score(&query, item.label).map(|score| (score, i as u32)))
        .collect();
    scored.sort_unstable();
    state.matches.clear();
    state.matches.extend(scored.iter().map(|&(_, i)| i));
    state.filtered_for = Some((state.query.clone(), items.len()));
    state.selected = 0;
    state.scroll = 0.;
}

/// The modal palette, near the top of the window.
pub fn quick_open(
    ctx: &Context,
    state: &mut QuickOpenState,
    items: &[QuickItem<'_>],
) -> Option<UiAction> {
    if !state.open {
        return None;
    }
    let id = Id::unique("quick-open");
    let progress = float::appear_progress(ctx, id);
    let screen = ctx.content_rect();
    let x = screen.center().x - WIDTH / 2.;

    // A click outside dismisses it; this catcher lies under the palette and over the shell.
    let catcher = Area::new(id.with("catch"))
        .order(Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| ui.allocate_rect(screen, Sense::click()));

    let response = Area::new(id)
        .order(Order::Foreground)
        .fixed_pos(pos2(x, screen.top() + TOP))
        .show(ctx, |ui| {
            float::apply_appear(ui, pos2(screen.center().x, screen.top() + TOP), progress);
            float::float_frame(ui).show(ui, |ui| quick_open_body(ui, state, items)).inner
        });

    let dismissed = catcher.inner.clicked() || ctx.input(|i| i.key_pressed(Key::Escape));
    let action = response.inner.or(dismissed.then_some(UiAction::QuickOpenDismiss));
    if action.is_some() {
        state.close();
        ctx.request_repaint();
    }
    action
}

/// The palette's content, for hosting in any surface.
pub fn quick_open_body(
    ui: &mut Ui,
    state: &mut QuickOpenState,
    items: &[QuickItem<'_>],
) -> Option<UiAction> {
    let id = Id::unique("quick-open-body");
    let p = palette(ui);
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing.y = 0.;

    let input_id = id.with("input");
    let input = text_input(ui, input_id, &mut state.query, InputOptions {
        hint: "Search connections and commands",
        glyph: Some(icons::SEARCH),
        width: Some(WIDTH),
        borderless: true,
        height: Some(INPUT),
        ..Default::default()
    });
    if !input.has_focus() {
        input.request_focus();
    }
    refilter(state, items);

    let (line, _) = ui.allocate_exact_size(vec2(WIDTH, 1.), Sense::hover());
    hline(ui.painter(), line.left(), line.right(), line.top(), p.border);

    let len = state.matches.len();
    let (down, up, enter) = ui.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    let rows = len.min(MAX_ROWS);
    let viewport = rows as f32 * ROW;
    if len > 0 {
        if down {
            state.selected = (state.selected + 1).min(len - 1);
        }
        if up {
            state.selected = state.selected.saturating_sub(1);
        }
        if down || up {
            state.scroll = scroll_to_row(state.scroll, state.selected, ROW, viewport);
        }
    }

    let mut action = None;
    if enter && let Some(&index) = state.matches.get(state.selected) {
        action = Some(UiAction::QuickOpenPick(items[index as usize].id));
    }

    if rows == 0 {
        let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, 48.), Sense::hover());
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No results",
            fonts::regular(TEXT_SM),
            p.muted_fg,
        );
        return action;
    }

    let (area, _) = ui.allocate_exact_size(vec2(WIDTH, viewport + 8.), Sense::hover());
    let list = area.shrink2(vec2(4., 4.));
    if ui.rect_contains_pointer(list) {
        state.scroll -= f64::from(ui.input(|i| i.smooth_scroll_delta.y));
    }
    state.scroll = clamp_scroll(state.scroll, len, ROW, viewport);
    let visible = window(state.scroll, len, ROW, viewport);
    let painter = ui.painter_at(list);
    for row in visible.first..visible.end {
        let item = &items[state.matches[row] as usize];
        let y = list.top() + visible.top + (row - visible.first) as f32 * ROW;
        let rect = Rect::from_min_size(pos2(list.left(), y), vec2(list.width(), ROW));
        let response = ui.interact(rect.intersect(list), id.with(row), Sense::click());
        if response.hovered() && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) {
            state.selected = row;
        }
        if row == state.selected {
            painter.rect_filled(rect, CornerRadius::same(RADIUS_SM), p.accent);
        }
        let center = rect.center().y;
        icon(&painter, pos2(rect.left() + 17., center), item.glyph, 14., p.muted_fg);
        let x = rect.left() + 34.;
        let right = rect.right() - 12.;
        let detail_room = (right - x) * 0.4;
        let label_w = elided_at(
            &painter,
            x,
            center,
            item.label,
            fonts::regular(TEXT_SM),
            p.foreground,
            right - x - detail_room,
        );
        if !item.detail.is_empty() {
            let dx = x + label_w + 10.;
            elided_at(
                &painter,
                dx,
                center,
                item.detail,
                fonts::regular(TEXT_SM),
                p.muted_fg,
                right - dx,
            );
        }
        if response.clicked() {
            action = Some(UiAction::QuickOpenPick(item.id));
        }
    }
    action
}
