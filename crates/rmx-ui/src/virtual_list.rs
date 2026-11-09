//! Which rows of a long list are on screen.
//!
//! Offsets are `f64`: 100,000 rows of 28 px is 2.8 million px, past where `f32` resolves whole
//! sub-pixels. The widgets only ever turn the fraction into a screen position.

/// The rows to draw for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// Index of the first row to draw.
    pub first: usize,
    /// One past the last row to draw.
    pub end: usize,
    /// Screen offset of the first row from the top of the viewport; zero or negative.
    pub top: f32,
}

/// The longest scroll possible.
pub fn max_scroll(len: usize, row: f32, viewport: f32) -> f64 {
    (len as f64 * f64::from(row) - f64::from(viewport)).max(0.)
}

/// `scroll` held within the list.
pub fn clamp_scroll(scroll: f64, len: usize, row: f32, viewport: f32) -> f64 {
    scroll.clamp(0., max_scroll(len, row, viewport))
}

/// The rows visible at `scroll` in a viewport `viewport` high.
pub fn window(scroll: f64, len: usize, row: f32, viewport: f32) -> Window {
    let scroll = clamp_scroll(scroll, len, row, viewport);
    let row = f64::from(row);
    let first = (scroll / row).floor() as usize;
    let top = (first as f64 * row - scroll) as f32;
    let end = (((scroll + f64::from(viewport)) / row).ceil() as usize).min(len);
    Window { first: first.min(end), end, top }
}

/// The scroll that brings row `index` fully into view, moving as little as possible.
pub fn scroll_to_row(scroll: f64, index: usize, row: f32, viewport: f32) -> f64 {
    let top = index as f64 * f64::from(row);
    let bottom = top + f64::from(row);
    if top < scroll {
        top
    } else if bottom > scroll + f64::from(viewport) {
        bottom - f64::from(viewport)
    } else {
        scroll
    }
}

/// A scrollbar thumb's `(top, height)` within a track `track` high.
pub fn thumb(scroll: f64, len: usize, row: f32, track: f32, min_height: f32) -> (f32, f32) {
    let content = len as f64 * f64::from(row);
    let height = (f64::from(track) * f64::from(track) / content.max(1.)).max(f64::from(min_height));
    let height = height.min(f64::from(track));
    let room = f64::from(track) - height;
    let max = max_scroll(len, row, track);
    let top = if max > 0. { room * (scroll / max).clamp(0., 1.) } else { 0. };
    (top as f32, height as f32)
}

/// The scroll for a thumb whose top is at `top` in a track `track` high.
pub fn scroll_for_thumb(top: f32, len: usize, row: f32, track: f32, min_height: f32) -> f64 {
    let (_, height) = thumb(0., len, row, track, min_height);
    let room = f64::from(track - height);
    if room <= 0. {
        return 0.;
    }
    max_scroll(len, row, track) * (f64::from(top) / room).clamp(0., 1.)
}
