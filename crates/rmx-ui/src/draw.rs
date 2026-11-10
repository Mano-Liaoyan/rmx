//! Painting helpers shared by the widgets.

use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, FontId, Galley, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
    TextFormat, Ui, pos2, vec2,
};

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

/// Lucide's `lock-open`, which is not in the font subset, drawn from the same geometry.
pub(crate) fn lock_open(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 24.;
    let at = |x: f32, y: f32| center + vec2((x - 12.) * unit, (y - 12.) * unit);
    let stroke = Stroke::new(2. * unit, color);
    let body = Rect::from_two_pos(at(3., 11.), at(21., 22.));
    painter.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Middle);

    // Up from the body and over, stopping short of coming back down.
    const STEPS: u8 = 12;
    let (start, end) = (std::f32::consts::PI, std::f32::consts::TAU - 0.2);
    let mut shackle = vec![at(7., 11.)];
    shackle.extend((0..=STEPS).map(|step| {
        let angle = start + (end - start) * f32::from(step) / f32::from(STEPS);
        at(12. + 5. * angle.cos(), 7. + 5. * angle.sin())
    }));
    painter.add(Shape::line(shackle, stroke));
}

/// Lucide's `sun`, which is not in the font subset, drawn from the same geometry.
pub(crate) fn sun(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 24.;
    let stroke = Stroke::new(2. * unit, color);
    painter.circle_stroke(center, 4. * unit, stroke);
    for step in 0..8u8 {
        let angle = std::f32::consts::FRAC_PI_4 * f32::from(step);
        let along = vec2(angle.cos(), angle.sin());
        // The diagonal rays are a little shorter, as in Lucide.
        let (inner, outer) = if step % 2 == 0 { (8., 10.) } else { (7.4, 8.8) };
        painter.line_segment([center + along * inner * unit, center + along * outer * unit], stroke);
    }
}

/// Lucide's `moon`, which is not in the font subset: a crescent opening to the upper right.
pub(crate) fn moon(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 24.;
    let stroke = Stroke::new(2. * unit, color);
    let radius = 9. * unit;
    // The rim of a circle, less what a second one up and to the right bites off.
    let bite = center + vec2(5., -5.) * unit;
    let bite_radius = 7. * unit;
    const STEPS: u16 = 32;
    let around = |middle: Pos2, r: f32, step: u16| {
        let angle = std::f32::consts::TAU * f32::from(step) / f32::from(STEPS);
        middle + vec2(angle.cos(), angle.sin()) * r
    };
    let mut points: Vec<Pos2> =
        (0..=STEPS).map(|step| around(center, radius, step)).filter(|at| at.distance(bite) >= bite_radius).collect();
    points.extend(
        (0..=STEPS).rev().map(|step| around(bite, bite_radius, step)).filter(|at| at.distance(center) <= radius),
    );
    painter.add(Shape::closed_line(points, stroke));
}

/// Lucide's `file`, which is not in the font subset, drawn from the same geometry.
pub(crate) fn file_icon(painter: &Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 24.;
    let at = |x: f32, y: f32| center + vec2((x - 12.) * unit, (y - 12.) * unit);
    let stroke = Stroke::new(1.25, color);
    let outline = vec![at(6., 2.), at(15., 2.), at(20., 7.), at(20., 20.), at(18., 22.), at(6., 22.), at(4., 20.), at(4., 4.), at(6., 2.)];
    painter.add(Shape::line(outline, stroke));
    painter.add(Shape::line(vec![at(14., 2.5), at(14., 8.), at(19.5, 8.)], stroke));
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
