//! The context menu.

use egui::{
    Align2, Color32, Context, CornerRadius, Id, Key, Margin, Modifiers, Pos2, Rect, Sense, pos2,
    vec2,
};

use crate::action::UiAction;
use crate::draw::{elided_at, hline, icon, palette, snap};
use crate::theme::{RADIUS_SM, TEXT_SM, TEXT_XS};
use crate::{float, fonts};

const WIDTH: f32 = 224.;
const ROW: f32 = 28.;

/// A row of a context menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuItem {
    Action {
        glyph: Option<char>,
        label: String,
        shortcut: Option<String>,
        action: UiAction,
        /// Drawn in the status color for destructive commands.
        danger: bool,
    },
    Separator,
}

/// Where a context menu is, while it is.
#[derive(Clone, Debug, Default)]
pub struct ContextMenu {
    at: Option<Pos2>,
    /// The row chosen with the keyboard or last under the pointer.
    selected: Option<usize>,
    /// Where it was last drawn.
    rect: Option<Rect>,
}

impl ContextMenu {
    /// Open at `pos`.
    pub fn open(&mut self, pos: Pos2) {
        self.at = Some(pos);
        self.selected = None;
        self.rect = None;
    }

    /// Whether it is showing.
    pub fn is_open(&self) -> bool {
        self.at.is_some()
    }

    /// Whether it covers `pos`, in points.
    pub fn contains(&self, pos: Pos2) -> bool {
        self.rect.is_some_and(|rect| rect.contains(pos))
    }

    /// Where the row labelled `label` of `items` is, for driving the menu from a script.
    pub fn row_center(&self, items: &[MenuItem], label: &str) -> Option<Pos2> {
        let rect = self.rect?;
        let mut y = rect.top() + 4. + 1.;
        for item in items {
            match item {
                MenuItem::Action { label: text, .. } if text == label => {
                    return Some(pos2(rect.center().x, y + ROW / 2.));
                },
                MenuItem::Action { .. } => y += ROW,
                MenuItem::Separator => y += 9.,
            }
        }
        None
    }

    /// Close without choosing.
    pub fn close(&mut self) {
        self.at = None;
    }

    fn step(&mut self, items: &[MenuItem], down: bool) {
        let rows = || (0..items.len()).filter(|&i| matches!(items[i], MenuItem::Action { .. }));
        self.selected = match (self.selected, down) {
            (None, true) => rows().next(),
            (None, false) => rows().next_back(),
            (Some(at), true) => rows().find(|&i| i > at).or(rows().next()),
            (Some(at), false) => rows().rfind(|&i| i < at).or(rows().next_back()),
        };
    }
}

/// Show `menu` if open. Choosing an item closes it and returns the item's action.
pub fn context_menu(
    ctx: &Context,
    id: Id,
    menu: &mut ContextMenu,
    items: &[MenuItem],
) -> Option<UiAction> {
    let at = menu.at?;
    let (down, up, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    if down || up {
        menu.step(items, down);
    }
    let selected = menu.selected;
    let (hit, response) = float::show_at(ctx, id, at, Align2::LEFT_TOP, Margin::same(4), |ui| {
        ui.spacing_mut().item_spacing.y = 0.;
        let (mut chosen, mut hovered) = (None, None);
        for (i, item) in items.iter().enumerate() {
            let (row_chosen, row_hovered) = menu_row(ui, id.with(i), item, selected == Some(i));
            chosen = chosen.or(row_chosen);
            hovered = hovered.or(row_hovered.then_some(i));
        }
        (chosen, hovered)
    });
    let (mut chosen, hovered) = hit;
    menu.rect = Some(response.rect);
    if hovered.is_some() {
        menu.selected = hovered;
    }
    if enter
        && let Some(MenuItem::Action { action, .. }) = menu.selected.and_then(|at| items.get(at))
    {
        chosen = Some(action.clone());
    }

    // The click that opened the menu is over by now, so any press outside means leave.
    let pressed_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer.interact_pos().is_some_and(|pos| !response.rect.contains(pos))
    });
    if chosen.is_some() || pressed_outside || enter || ctx.input(|i| i.key_pressed(Key::Escape)) {
        menu.at = None;
        ctx.request_repaint();
    }
    chosen
}

fn menu_row(
    ui: &mut egui::Ui,
    id: Id,
    item: &MenuItem,
    selected: bool,
) -> (Option<UiAction>, bool) {
    let p = palette(ui);
    let MenuItem::Action { glyph, label, shortcut, action, danger } = item else {
        let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, 9.), Sense::hover());
        hline(ui.painter(), rect.left() - 4., rect.right() + 4., rect.center().y, p.border);
        return (None, false);
    };

    let rect = Rect::from_min_size(ui.cursor().min, vec2(WIDTH, ROW));
    ui.advance_cursor_after_rect(rect);
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter();
    if response.hovered() || selected {
        painter.rect_filled(rect, CornerRadius::same(RADIUS_SM), p.accent);
    }
    let text = if *danger { p.danger } else { p.foreground };
    let y = rect.center().y;
    if let Some(glyph) = glyph {
        let color = if *danger { p.danger } else { p.muted_fg };
        icon(painter, pos2(rect.left() + 15., y), *glyph, 14., color);
    }
    let x = rect.left() + 30.;
    let mut right = rect.right() - 8.;
    if let Some(shortcut) = shortcut {
        let hint = painter.layout_no_wrap(shortcut.clone(), fonts::regular(TEXT_XS), p.muted_fg);
        right -= hint.size().x;
        let top = snap(painter, y - hint.size().y / 2.);
        painter.galley(pos2(right, top), hint, Color32::PLACEHOLDER);
        right -= 12.;
    }
    elided_at(painter, x, y, label, fonts::regular(TEXT_SM), text, right - x);
    (response.clicked().then(|| action.clone()), response.hovered())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<MenuItem> {
        let row = |label: &str| MenuItem::Action {
            glyph: None,
            label: label.to_owned(),
            shortcut: None,
            action: UiAction::TabNew,
            danger: false,
        };
        vec![row("a"), MenuItem::Separator, row("b"), row("c")]
    }

    #[test]
    fn arrows_skip_separators_and_wrap_around() {
        let (items, mut menu) = (items(), ContextMenu::default());
        menu.step(&items, true);
        assert_eq!(menu.selected, Some(0));
        menu.step(&items, true);
        assert_eq!(menu.selected, Some(2), "not the separator");
        menu.step(&items, true);
        menu.step(&items, true);
        assert_eq!(menu.selected, Some(0), "around");
        menu.step(&items, false);
        assert_eq!(menu.selected, Some(3));
    }
}
