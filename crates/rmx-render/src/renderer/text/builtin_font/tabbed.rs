//! Built-in glyphs of the tabbed fork: the rounded powerline caps.
//!
//! These are drawn here rather than taken from the font, since the font's outlines are
//! positioned relative to the baseline using the font's own ascent/descent, which after
//! rounding to the integer cell grid leaves them shifted by a pixel against the cell
//! (and thus against the segment background they're supposed to cap).

use crossfont::{BitmapBuffer, Metrics, RasterizedGlyph};

use super::{COLOR_FILL, Canvas, Pixel, calculate_stroke_size};
use crate::font::Delta;

const POWERLINE_HALF_CIRCLE_THIN_LTR: char = '\u{e0b5}';
const POWERLINE_HALF_CIRCLE_RTL: char = '\u{e0b6}';
const POWERLINE_HALF_CIRCLE_THIN_RTL: char = '\u{e0b7}';

/// Draw the powerline half circles ('', '', '', '') covering the entire cell.
///
/// The cap is a half ellipse centered on the cell's flat edge, spanning the full cell height
/// and width, so it joins seamlessly with the neighbouring cell's background.
pub(super) fn powerline_half_circle(
    character: char,
    metrics: &Metrics,
    offset: &Delta<i8>,
) -> RasterizedGlyph {
    // Number of samples per pixel axis used for anti-aliasing.
    const SAMPLES: usize = 8;

    let height = (metrics.line_height as i32 + offset.y as i32).max(1) as usize;
    let width = (metrics.average_advance as i32 + offset.x as i32).max(1) as usize;
    let stroke_size = calculate_stroke_size(width) as f32;

    let filled =
        !matches!(character, POWERLINE_HALF_CIRCLE_THIN_LTR | POWERLINE_HALF_CIRCLE_THIN_RTL);

    let mut canvas = Canvas::new(width, height);

    // Outer ellipse is centered on the left edge, touching the top, bottom and right edges.
    let rx = width as f32;
    let ry = height as f32 / 2.;
    // Inner ellipse for the thin variants.
    let inner_rx = (rx - stroke_size).max(0.);
    let inner_ry = (ry - stroke_size).max(0.);

    // Both ellipses share the center at the middle of the flat edge.
    let center_y = height as f32 / 2.;
    let inside = |x: f32, y: f32, rx: f32, ry: f32| -> bool {
        if rx <= 0. || ry <= 0. {
            return false;
        }
        let dx = x / rx;
        let dy = (y - center_y) / ry;
        dx * dx + dy * dy <= 1.
    };

    let step = 1. / SAMPLES as f32;
    for y in 0..height {
        for x in 0..width {
            let mut covered = 0;
            for sy in 0..SAMPLES {
                let py = y as f32 + (sy as f32 + 0.5) * step;
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) * step;
                    if inside(px, py, rx, ry) && (filled || !inside(px, py, inner_rx, inner_ry)) {
                        covered += 1;
                    }
                }
            }

            let value = covered * COLOR_FILL._r as usize / (SAMPLES * SAMPLES);
            canvas.put_pixel(x as f32, y as f32, Pixel::gray(value as u8));
        }
    }

    if filled {
        // The flat edge must be fully opaque from top to bottom to join the adjacent segment.
        canvas.draw_rect(0., 0., 1., height as f32, COLOR_FILL);
    } else {
        // The arc's ends sit in the corners of the flat edge, where the ellipse is only tangent
        // to the cell; make them reach the cell edge with full stroke.
        canvas.draw_rect(0., 0., 1., stroke_size, COLOR_FILL);
        canvas.draw_rect(0., height as f32 - stroke_size, 1., stroke_size, COLOR_FILL);
    }

    if matches!(character, POWERLINE_HALF_CIRCLE_RTL | POWERLINE_HALF_CIRCLE_THIN_RTL) {
        canvas.flip_horizontal();
    }

    let top = height as i32 + metrics.descent as i32;
    let buffer = BitmapBuffer::Rgb(canvas.into_raw());
    RasterizedGlyph {
        character,
        top,
        left: 0,
        height: height as i32,
        width: width as i32,
        buffer,
        advance: (width as i32, height as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::super::builtin_glyph;
    use super::*;

    // Dummy metrics values to test builtin glyphs coverage.
    const METRICS: Metrics = Metrics {
        average_advance: 6.,
        line_height: 16.,
        descent: 4.,
        underline_position: 2.,
        underline_thickness: 2.,
        strikeout_position: 2.,
        strikeout_thickness: 2.,
    };

    #[test]
    fn builtin_powerline_half_circles_coverage() {
        let offset = Default::default();
        let glyph_offset = Default::default();

        for character in '\u{e0b4}'..='\u{e0b7}' {
            assert!(builtin_glyph(character, &METRICS, &offset, &glyph_offset).is_some());
        }
    }

    /// Intensity of the pixel at (`x`, `y`) of the glyph.
    fn pixel(glyph: &RasterizedGlyph, x: usize, y: usize) -> u8 {
        let BitmapBuffer::Rgb(buffer) = &glyph.buffer else { unreachable!() };
        buffer[(y * glyph.width as usize + x) * 3]
    }

    /// Rows with a set pixel in column `x`.
    fn set_rows(glyph: &RasterizedGlyph, x: usize) -> Vec<usize> {
        (0..glyph.height as usize).filter(|&y| pixel(glyph, x, y) > 127).collect()
    }

    /// Columns with a set pixel in row `y`.
    fn set_cols(glyph: &RasterizedGlyph, y: usize) -> Vec<usize> {
        (0..glyph.width as usize).filter(|&x| pixel(glyph, x, y) > 127).collect()
    }

    /// Cell sizes covering odd/even combinations and typical HiDPI cells.
    fn cell_metrics() -> impl Iterator<Item = Metrics> {
        (4..30).flat_map(|width| {
            (8..60).map(move |height| Metrics {
                average_advance: width as f64,
                line_height: height as f64,
                ..METRICS
            })
        })
    }

    #[test]
    fn powerline_half_circles_fill_cell() {
        let offset = Default::default();

        for metrics in cell_metrics() {
            for character in '\u{e0b4}'..='\u{e0b7}' {
                let glyph = builtin_glyph(character, &metrics, &offset, &offset).unwrap();
                let width = metrics.average_advance as usize;
                let height = metrics.line_height as usize;

                // Glyph covers exactly the cell.
                assert_eq!(glyph.width as usize, width);
                assert_eq!(glyph.height as usize, height);
                assert_eq!(glyph.top, height as i32 + metrics.descent as i32);
                assert_eq!(glyph.left, 0);

                let rtl = character >= '\u{e0b6}';
                let (flat, round) = if rtl { (width - 1, 0) } else { (0, width - 1) };
                let thin = character == '\u{e0b5}' || character == '\u{e0b7}';

                if thin {
                    // The arc touches the top and bottom of the cell on the flat side...
                    assert_eq!(pixel(&glyph, flat, 0), 255, "{character:?} {width}x{height}");
                    assert_eq!(pixel(&glyph, flat, height - 1), 255);
                    // ...but is hollow in the middle, unless the stroke fills a very short cell.
                    if height >= 4 * calculate_stroke_size(width) {
                        assert_eq!(pixel(&glyph, flat, height / 2), 0);
                    }
                } else {
                    // The flat edge is fully opaque from top to bottom.
                    assert!((0..height).all(|y| pixel(&glyph, flat, y) == 255));
                }

                // The rounded side reaches the far edge of the cell around the middle.
                assert!(pixel(&glyph, round, height / 2) > 127, "{character:?} {width}x{height}");

                // Vertically symmetric.
                for y in 0..height / 2 {
                    for x in 0..width {
                        assert_eq!(pixel(&glyph, x, y), pixel(&glyph, x, height - 1 - y));
                    }
                }
            }
        }
    }

    #[test]
    fn powerline_half_circles_are_mirrored() {
        let offset = Default::default();

        for metrics in cell_metrics() {
            for (ltr, rtl) in [('\u{e0b4}', '\u{e0b6}'), ('\u{e0b5}', '\u{e0b7}')] {
                let ltr = builtin_glyph(ltr, &metrics, &offset, &offset).unwrap();
                let rtl = builtin_glyph(rtl, &metrics, &offset, &offset).unwrap();
                let width = ltr.width as usize;
                for y in 0..ltr.height as usize {
                    for x in 0..width {
                        assert_eq!(pixel(&ltr, x, y), pixel(&rtl, width - 1 - x, y));
                    }
                }
            }
        }
    }

    #[test]
    fn rounded_corners_join_straight_lines() {
        let offset = Default::default();

        for metrics in cell_metrics() {
            let glyph = |c| builtin_glyph(c, &metrics, &offset, &offset).unwrap();
            let width = metrics.average_advance as usize;
            let height = metrics.line_height as usize;

            let h_line = set_rows(&glyph('\u{2500}'), 0);
            let v_line = set_cols(&glyph('\u{2502}'), 0);

            // '╭', '╮', '╯', '╰'.
            for character in '\u{256d}'..='\u{2570}' {
                let corner = glyph(character);
                let right = character == '\u{256d}' || character == '\u{2570}';
                let top = character == '\u{256f}' || character == '\u{2570}';

                let edge_x = if right { width - 1 } else { 0 };
                let edge_y = if top { 0 } else { height - 1 };
                assert_eq!(set_rows(&corner, edge_x), h_line, "{character:?} {width}x{height}");
                assert_eq!(set_cols(&corner, edge_y), v_line, "{character:?} {width}x{height}");
            }
        }
    }
}
