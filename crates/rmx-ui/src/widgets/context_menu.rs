//! The context menu.

use egui::{
    Align2, Color32, Context, CornerRadius, Id, Key, Margin, Pos2, Rect, Sense, pos2, vec2,
};

use crate::action::UiAction;
use crate::draw::{elided_at, hline, icon, palette, snap};
use crate::theme::{RADIUS_SM, TEXT_SM};
use crate::{float, fonts};

const WIDTH: f32 = 200.;
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
}

impl ContextMenu {
    /// Open at `pos`.
    pub fn open(&mut self, pos: Pos2) {
        self.at = Some(pos);
    }

    /// Whether it is showing.
    pub fn is_open(&self) -> bool {
        self.at.is_some()
    }

    /// Close without choosing.
    pub fn close(&mut self) {
        self.at = None;
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
    let (chosen, response) = float::show_at(ctx, id, at, Align2::LEFT_TOP, Margin::same(4), |ui| {
        ui.spacing_mut().item_spacing.y = 0.;
        let mut chosen = None;
        for (i, item) in items.iter().enumerate() {
            chosen = chosen.or(menu_row(ui, id.with(i), item));
        }
        chosen
    });

    // The click that opened the menu is over by now, so any press outside means leave.
    let pressed_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer.interact_pos().is_some_and(|pos| !response.rect.contains(pos))
    });
    if chosen.is_some() || pressed_outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
        menu.at = None;
        ctx.request_repaint();
    }
    chosen
}

fn menu_row(ui: &mut egui::Ui, id: Id, item: &MenuItem) -> Option<UiAction> {
    let p = palette(ui);
    let MenuItem::Action { glyph, label, shortcut, action, danger } = item else {
        let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, 9.), Sense::hover());
        hline(ui.painter(), rect.left() - 4., rect.right() + 4., rect.center().y, p.border);
        return None;
    };

    let rect = Rect::from_min_size(ui.cursor().min, vec2(WIDTH, ROW));
    ui.advance_cursor_after_rect(rect);
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter();
    if response.hovered() {
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
        let hint = painter.layout_no_wrap(shortcut.clone(), fonts::regular(TEXT_SM), p.muted_fg);
        right -= hint.size().x;
        let top = snap(painter, y - hint.size().y / 2.);
        painter.galley(pos2(right, top), hint, Color32::PLACEHOLDER);
        right -= 12.;
    }
    elided_at(painter, x, y, label, fonts::regular(TEXT_SM), text, right - x);
    response.clicked().then(|| action.clone())
}
