//! Floating surfaces: the only places in the interface with a shadow.

use egui::emath::TSTransform;
use egui::{
    Align2, Area, Context, CornerRadius, Frame, Id, Key, Margin, Order, Pos2, Sense, Stroke, Ui,
    Vec2,
};

use rmx_layout::anim::Curve;

use crate::draw::palette;
use crate::theme::{self, FLOAT_IN, FLOAT_SCALE, MENU_IN, MENU_SCALE, RADIUS_LG};
use crate::widgets::panels::resize_handle;

/// How far a float is through appearing, 0 to 1, restarting whenever it wasn't shown last frame.
pub(crate) fn appear_progress(ctx: &Context, id: Id) -> f32 {
    appear_over(ctx, id, FLOAT_IN)
}

fn appear_over(ctx: &Context, id: Id, seconds: f64) -> f32 {
    let key = id.with("appear");
    let pass = ctx.cumulative_pass_nr();
    let time = ctx.input(|i| i.time);
    let opened_at = ctx.data_mut(|data| {
        let (opened_at, seen) = data.get_temp::<(f64, u64)>(key).unwrap_or((time, 0));
        // A discarded pass runs twice in a frame, hence the tolerance.
        let opened_at = if seen + 2 >= pass && seen != 0 { opened_at } else { time };
        data.insert_temp(key, (opened_at, pass));
        opened_at
    });
    let progress = ((time - opened_at) / seconds).clamp(0., 1.) as f32;
    if progress < 1. {
        ctx.request_repaint();
    }
    progress
}

/// Fade a float's content in and grow it from 95% about `center`.
pub(crate) fn apply_appear(ui: &mut Ui, center: Pos2, progress: f32) {
    let eased = 1. - (1. - progress) * (1. - progress);
    grow(ui, center, progress, eased, FLOAT_SCALE);
}

fn grow(ui: &mut Ui, center: Pos2, progress: f32, eased: f32, from: f32) {
    ui.set_opacity(eased);
    let scale = from + (1. - from) * eased;
    let center = center.to_vec2();
    let transform =
        TSTransform::from_translation(center * (1. - scale)) * TSTransform::from_scaling(scale);
    let transform = if progress >= 1. { TSTransform::IDENTITY } else { transform };
    ui.ctx().set_transform_layer(ui.layer_id(), transform);
}

/// The frame of a floating surface.
pub fn float_frame(ui: &Ui) -> Frame {
    let p = palette(ui);
    Frame::new()
        .fill(p.float)
        .stroke(Stroke::new(1., p.border))
        .corner_radius(CornerRadius::same(RADIUS_LG))
        .shadow(ui.visuals().popup_shadow)
}

/// A menu at a fixed position, popping in from the pointer at `pos`.
pub(crate) fn show_at<R>(
    ctx: &Context,
    id: Id,
    pos: Pos2,
    pivot: Align2,
    margin: Margin,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, egui::Response) {
    let progress = appear_over(ctx, id, MENU_IN);
    let response =
        Area::new(id).order(Order::Foreground).fixed_pos(pos).pivot(pivot).constrain(true).show(
            ctx,
            |ui| {
                grow(ui, pos, progress, Curve::EASE_OUT.at(progress), MENU_SCALE);
                float_frame(ui).inner_margin(margin).show(ui, add).inner
            },
        );
    (response.inner, response.response)
}

/// What became of a modal this frame.
pub(crate) struct Modal<R> {
    pub inner: R,
    /// The user pressed Escape or clicked outside.
    pub dismissed: bool,
}

/// A dialog centered over a scrim that swallows input to the shell.
pub(crate) fn modal<R>(
    ctx: &Context,
    id: Id,
    width: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> Modal<R> {
    modal_at(ctx, id, width, None, add)
}

/// What a dialog that is resized by its edges and corner needs.
pub(crate) struct Resizable<'a> {
    /// Its size, as dragged, 0 being its default.
    pub size: &'a mut [f32; 2],
    pub default: [f32; 2],
    pub min: [f32; 2],
    /// Set once it is let go at a new size.
    pub changed: &'a mut bool,
}

impl Resizable<'_> {
    /// The size to draw at, inside the window.
    pub fn current(&self, screen: egui::Rect) -> [f32; 2] {
        let most = [(screen.width() - 32.).max(self.min[0]), (screen.height() - 32.).max(self.min[1])];
        [0, 1].map(|i| {
            let size = if self.size[i] > 0. { self.size[i] } else { self.default[i] };
            size.clamp(self.min[i], most[i])
        })
    }
}

/// [`modal`], resized by dragging its right and bottom edges and corner. It stays centered, so
/// each edge moves half as far as the size changes.
pub(crate) fn resizable_modal<R>(
    ctx: &Context,
    id: Id,
    resize: Resizable<'_>,
    add: impl FnOnce(&mut Ui, [f32; 2]) -> R,
) -> Modal<R> {
    let current = resize.current(ctx.content_rect());
    modal_at(ctx, id, current[0], Some(resize), |ui| add(ui, current))
}

fn modal_at<R>(
    ctx: &Context,
    id: Id,
    width: f32,
    resize: Option<Resizable<'_>>,
    add: impl FnOnce(&mut Ui) -> R,
) -> Modal<R> {
    let progress = appear_progress(ctx, id);
    let screen = ctx.content_rect();
    let p = if ctx.global_style().visuals.dark_mode { &theme::DARK } else { &theme::LIGHT };

    let scrim =
        Area::new(id.with("scrim")).order(Order::Middle).fixed_pos(screen.min).show(ctx, |ui| {
            let eased = progress.min(1.);
            ui.painter().rect_filled(screen, 0., p.scrim.gamma_multiply(eased));
            ui.allocate_rect(screen, Sense::click_and_drag())
        });

    let dialog = Area::new(id)
        .order(Order::Foreground)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .constrain(true)
        .show(ctx, |ui| {
            apply_appear(ui, screen.center(), progress);
            ui.set_width(width);
            let frame = float_frame(ui).show(ui, |ui| {
                ui.set_width(width - 2.);
                add(ui)
            });
            if let Some(resize) = resize {
                dialog_edges(ui, id, frame.response.rect, resize);
            }
            frame.inner
        });

    let dismissed = scrim.inner.clicked() || ctx.input(|i| i.key_pressed(Key::Escape));
    Modal { inner: dialog.inner, dismissed }
}

/// The right and bottom edges of a dialog at `rect`, and the corner where they meet.
fn dialog_edges(ui: &mut Ui, id: Id, rect: egui::Rect, resize: Resizable<'_>) {
    const BAND: f32 = 6.;
    let current = resize.current(ui.ctx().content_rect());
    let right = egui::Rect::from_min_max(egui::pos2(rect.right() - BAND, rect.top()), rect.max);
    let bottom = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - BAND), rect.max);
    let corner = egui::Rect::from_min_max(rect.max - Vec2::splat(BAND * 2.), rect.max);
    let width = resize_handle(ui, right, id.with("edge-right"), true, rect.right());
    let height = resize_handle(ui, bottom, id.with("edge-bottom"), false, rect.bottom());
    let corner = ui.interact(corner, id.with("edge-corner"), Sense::click_and_drag());
    if corner.hovered() || corner.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
    }

    let center = rect.center();
    let mut next = current;
    if let Some(x) = width.to {
        next[0] = (x - center.x) * 2.;
    }
    if let Some(y) = height.to {
        next[1] = (y - center.y) * 2.;
    }
    if corner.dragged()
        && let Some(pointer) = corner.interact_pointer_pos()
    {
        next = [(pointer.x - center.x) * 2., (pointer.y - center.y) * 2.];
    }
    for axis in 0..2 {
        if (next[axis] - current[axis]).abs() >= 1. {
            resize.size[axis] = next[axis].max(resize.min[axis]);
            ui.ctx().request_repaint();
        }
    }
    if width.reset || height.reset || corner.double_clicked() {
        *resize.size = [0., 0.];
        *resize.changed = true;
    }
    if width.released || height.released || corner.drag_stopped() {
        *resize.changed = true;
    }
}
