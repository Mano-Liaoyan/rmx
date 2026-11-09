//! Floating surfaces: the only places in the interface with a shadow.

use egui::emath::TSTransform;
use egui::{
    Align2, Area, Context, CornerRadius, Frame, Id, Key, Margin, Order, Pos2, Sense, Stroke, Ui,
    Vec2,
};

use crate::draw::palette;
use crate::theme::{self, FLOAT_IN, FLOAT_SCALE, RADIUS};

/// How far a float is through appearing, 0 to 1, restarting whenever it wasn't shown last frame.
pub(crate) fn appear_progress(ctx: &Context, id: Id) -> f32 {
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
    let progress = ((time - opened_at) / FLOAT_IN).clamp(0., 1.) as f32;
    if progress < 1. {
        ctx.request_repaint();
    }
    progress
}

/// Fade a float's content in and grow it from 95% about `center`.
pub(crate) fn apply_appear(ui: &mut Ui, center: Pos2, progress: f32) {
    let eased = 1. - (1. - progress) * (1. - progress);
    ui.set_opacity(eased);
    let scale = FLOAT_SCALE + (1. - FLOAT_SCALE) * eased;
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
        .fill(p.background)
        .stroke(Stroke::new(1., p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .shadow(ui.visuals().popup_shadow)
}

/// A float at a fixed position, for menus and the like.
pub(crate) fn show_at<R>(
    ctx: &Context,
    id: Id,
    pos: Pos2,
    pivot: Align2,
    margin: Margin,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, egui::Response) {
    let progress = appear_progress(ctx, id);
    let response =
        Area::new(id).order(Order::Foreground).fixed_pos(pos).pivot(pivot).constrain(true).show(
            ctx,
            |ui| {
                apply_appear(ui, pos, progress);
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
            float_frame(ui)
                .show(ui, |ui| {
                    ui.set_width(width - 2.);
                    add(ui)
                })
                .inner
        });

    let dismissed = scrim.inner.clicked() || ctx.input(|i| i.key_pressed(Key::Escape));
    Modal { inner: dialog.inner, dismissed }
}
