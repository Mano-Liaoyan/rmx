//! Painting helpers shared by the widgets.

use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, FontId, Galley, Painter, Pos2, Rect, TextFormat, Ui, pos2};

use crate::theme::{DARK, LIGHT, Palette};

/// The active tokens.
pub(crate) fn palette(ui: &Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// `v` rounded to a physical pixel.
pub(crate) fn snap(painter: &Painter, v: f32) -> f32 {
    let ppp = painter.pixels_per_point();
    (v * ppp).round() / ppp
}

/// A one-physical-pixel-wide bar across `x0..x1` with its top at `y`, in any scale.
pub(crate) fn hline(painter: &Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    let ppp = painter.pixels_per_point();
    let top = snap(painter, y);
    painter.rect_filled(Rect::from_min_max(pos2(x0, top), pos2(x1, top + 1. / ppp)), 0., color);
}

/// A one-physical-pixel-wide bar down `y0..y1` with its left at `x`, in any scale.
pub(crate) fn vline(painter: &Painter, x: f32, y0: f32, y1: f32, color: Color32) {
    let ppp = painter.pixels_per_point();
    let left = snap(painter, x);
    painter.rect_filled(Rect::from_min_max(pos2(left, y0), pos2(left + 1. / ppp, y1)), 0., color);
}

/// A centered icon glyph.
pub(crate) fn icon(painter: &Painter, center: Pos2, glyph: char, size: f32, color: Color32) {
    painter.text(center, Align2::CENTER_CENTER, glyph, crate::fonts::icon(size), color);
}

/// Lay `text` out on one line, ending in an ellipsis past `max_width`.
pub(crate) fn elided(
    painter: &Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat::simple(font, color));
    job.wrap = TextWrapping::truncate_at_width(max_width.max(0.));
    painter.layout_job(job)
}

/// Draw elided text, vertically centered on `y`, from `x`. Returns its width.
pub(crate) fn elided_at(
    painter: &Painter,
    x: f32,
    y: f32,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> f32 {
    let galley = elided(painter, text, font, color, max_width);
    let width = galley.size().x;
    let top = y - galley.size().y / 2.;
    painter.galley(pos2(x, snap(painter, top)), galley, color);
    width
}
