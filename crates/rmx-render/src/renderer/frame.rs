//! Renderer additions of the tabbed fork: terminals drawn in a [`Frame`] of the window, rects
//! masking the window, and glyphs drawn straight from the atlas.

use crossfont::Metrics;

use crate::renderer::rects::{RectRenderer, RenderRect};
use crate::renderer::text::{TextRenderApi, TextRenderer};
use crate::renderer::{Renderer, TextRendererProvider};
use crate::{RenderableCell, Rgb, SizeInfo, gl};

pub use crate::renderer::text::glyph_cache::{Glyph, LoadGlyph};

/// Factors of `glBlendFuncSeparate` rects are drawn with.
const BLEND: [u32; 4] = [gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA, gl::SRC_ALPHA, gl::ONE];

/// Factors of `glBlendFuncSeparate` rects mask the window with.
const MASK: [u32; 4] = [gl::ZERO, gl::SRC_ALPHA, gl::ZERO, gl::SRC_ALPHA];

/// The fork's state of a [`Renderer`].
#[derive(Debug)]
pub struct State {
    /// Where terminals are drawn, or as they fill the window without one.
    frame: Option<Frame>,

    /// Factors of `glBlendFuncSeparate` the next rects are drawn with.
    blend: [u32; 4],
}

impl Default for State {
    fn default() -> Self {
        Self { frame: None, blend: BLEND }
    }
}

impl State {
    /// The frame terminals are drawn in, if any.
    pub(super) fn frame(&self) -> Option<Frame> {
        self.frame
    }

    /// Prepare the rect rendering state for drawing into the frame.
    pub fn prepare_rects(&self, size_info: &SizeInfo, rect_renderer: &mut RectRenderer) {
        let [x, y, width, height] = self.frame.unwrap_or(Frame::ORIGIN).area(size_info);
        let [src_rgb, dst_rgb, src_alpha, dst_alpha] = self.blend;
        unsafe {
            // Remove padding from viewport.
            gl::Viewport(x, y, width, height);
            gl::BlendFuncSeparate(src_rgb, dst_rgb, src_alpha, dst_alpha);
        }

        rect_renderer.origin = [x as f32, y as f32];
    }

    /// Set the viewport for cell rendering in the frame.
    pub fn set_viewport(&self, size: &SizeInfo) {
        let [x, y, width, height] = viewport(size, &self.frame.unwrap_or(Frame::ORIGIN));
        unsafe {
            gl::Viewport(x, y, width, height);
        }
    }
}

impl Renderer {
    /// Draw glyphs already loaded into the atlas, each placed as in the cell it comes with.
    pub fn draw_glyphs<I: Iterator<Item = (RenderableCell, Glyph)>>(
        &mut self,
        size_info: &SizeInfo,
        glyphs: I,
    ) {
        match &mut self.text_renderer {
            TextRendererProvider::Gles2(renderer) => renderer.with_api(size_info, |mut api| {
                glyphs.for_each(|(cell, glyph)| api.add_render_item(&cell, &glyph, size_info))
            }),
            TextRendererProvider::Glsl3(renderer) => renderer.with_api(size_info, |mut api| {
                glyphs.for_each(|(cell, glyph)| api.add_render_item(&cell, &glyph, size_info))
            }),
        }
    }

    /// Multiply what was drawn under the rectangles, alpha included, by their alpha, which makes
    /// the window transparent where it is zero.
    pub fn mask_rects(&mut self, size_info: &SizeInfo, metrics: &Metrics, rects: &[RenderRect]) {
        self.tabbed.blend = MASK;
        self.draw_rects(size_info, metrics, rects);
        self.tabbed.blend = BLEND;
    }

    /// Draw terminals into `frame`, or as they fill the window without one.
    ///
    /// Takes effect on the viewport with the next [`Self::set_viewport`] or [`Self::resize`].
    pub fn set_frame(&mut self, frame: Option<Frame>) {
        self.tabbed.frame = frame;
        Self::scissor(frame);
    }

    /// Fill `frame` with `color` and `alpha`.
    ///
    /// [`Self::clear`] fills only the frame's clip, if there is one.
    pub fn clear_rect(&self, frame: &Frame, color: Rgb, alpha: f32) {
        Self::scissor(Some(*frame));
        self.clear(color, alpha);
        Self::scissor(self.tabbed.frame);
    }

    /// Clip drawing to `frame`, or stop clipping without one.
    pub(super) fn scissor(frame: Option<Frame>) {
        unsafe {
            match frame {
                Some(Frame { clip: [x, y, width, height], .. }) => {
                    gl::Scissor(x, y, width, height);
                    gl::Enable(gl::SCISSOR_TEST);
                },
                None => gl::Disable(gl::SCISSOR_TEST),
            }
        }
    }
}

/// Where a terminal is drawn in the window, which its `SizeInfo` is stretched over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// Left edge, in window pixels.
    pub x: f32,
    /// Bottom edge, in window pixels from the window's bottom, as OpenGL counts them.
    pub y_bottom: f32,
    /// Scale from the terminal's size to the frame.
    pub sx: f32,
    pub sy: f32,
    /// Scissor rectangle `[x, y, width, height]` outside which nothing is drawn.
    pub clip: [i32; 4],
}

impl Frame {
    /// The window's bottom left corner, unscaled and unclipped, where terminals are drawn
    /// without a frame.
    const ORIGIN: Frame = Frame { x: 0., y_bottom: 0., sx: 1., sy: 1., clip: [0; 4] };

    /// Frame stretching `size` over `[x, y, width, height]`, in window pixels from the top left
    /// of a window `window_height` pixels tall.
    pub fn new(size: &SizeInfo, [x, y, width, height]: [f32; 4], window_height: f32) -> Self {
        let y_bottom = window_height - y - height;
        let (sx, sy) = (width / size.width(), height / size.height());
        let frame = Self { x, y_bottom, sx, sy, clip: [0; 4] };
        Self { clip: frame.area(size), ..frame }
    }

    /// This frame, drawing nothing outside `[x, y, width, height]` either, in window pixels from
    /// the top left of a window `window_height` pixels tall.
    pub fn clipped(self, [x, y, width, height]: [f32; 4], window_height: f32) -> Self {
        let [left, bottom, clip_width, clip_height] = self.clip;
        let (right, top) = (left + clip_width, bottom + clip_height);
        let left = (x.round() as i32).max(left);
        let bottom = ((window_height - y - height).round() as i32).max(bottom);
        let right = ((x + width).round() as i32).min(right);
        let top = ((window_height - y).round() as i32).min(top);
        Self { clip: [left, bottom, (right - left).max(0), (top - bottom).max(0)], ..self }
    }

    /// Viewport `[x, y, width, height]` of the whole of `size`, padding included, with its edges
    /// rounded to whole pixels.
    pub(super) fn area(&self, size: &SizeInfo) -> [i32; 4] {
        let (left, bottom) = (self.x.round(), self.y_bottom.round());
        let right = (self.x + size.width() * self.sx).round();
        let top = (self.y_bottom + size.height() * self.sy).round();
        [left, bottom, right - left, top - bottom].map(|value| value as i32)
    }
}

/// Viewport `[x, y, width, height]` of the cells of `size`, drawn in `frame`.
fn viewport(size: &SizeInfo, frame: &Frame) -> [i32; 4] {
    let (padding_x, padding_y) =
        ((size.padding_x() * frame.sx) as i32, (size.padding_y() * frame.sy) as i32);
    let [x, y, width, height] = frame.area(size);
    [x + padding_x, y + padding_y, width - 2 * padding_x, height - 2 * padding_y]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> SizeInfo {
        SizeInfo::new(800., 600., 10., 20., 5., 7., false)
    }

    #[test]
    fn viewport_without_frame() {
        let size = size();
        assert_eq!(viewport(&size, &Frame::ORIGIN), [5, 7, 790, 586]);
        assert_eq!(Frame::ORIGIN.area(&size), [0, 0, 800, 600]);
    }

    #[test]
    fn viewport_of_terminal_below_bar() {
        // Framed where it is drawn without a frame: below a 40 pixels tall bar.
        let size = size();
        let frame = Frame::new(&size, [0., 40., 800., 600.], 640.);
        assert_eq!(frame, Frame { clip: [0, 0, 800, 600], ..Frame::ORIGIN });
        assert_eq!(viewport(&size, &frame), viewport(&size, &Frame::ORIGIN));
    }

    #[test]
    fn viewport_offset() {
        // Right half of a 1600x640 window.
        let size = size();
        let frame = Frame::new(&size, [800., 40., 800., 600.], 640.);
        assert_eq!(viewport(&size, &frame), [805, 7, 790, 586]);
        assert_eq!(frame.area(&size), [800, 0, 800, 600]);

        // Top half of a 800x1240 window.
        let frame = Frame::new(&size, [0., 40., 800., 600.], 1240.);
        assert_eq!(viewport(&size, &frame), [5, 607, 790, 586]);
        assert_eq!(frame.area(&size), [0, 600, 800, 600]);
    }

    #[test]
    fn viewport_scaled() {
        // Popping in at 85 % about the centre of the window.
        let size = size();
        let frame = Frame::new(&size, [60., 45., 680., 510.], 600.);
        assert_eq!((frame.sx, frame.sy), (0.85, 0.85));
        assert_eq!(viewport(&size, &frame), [64, 50, 672, 500]);
        assert_eq!(frame.area(&size), [60, 45, 680, 510]);
    }

    #[test]
    fn clip() {
        let size = size();
        let frame = Frame::new(&size, [10.4, 20.6, 99.5, 50.2], 100.);
        assert_eq!(frame.clip, [10, 29, 100, 50]);
    }

    #[test]
    fn clip_matches_area_between_pixels() {
        // Halfway through a slide, edges rounded alike so neighbours neither overlap nor gap.
        let size = size();
        let frame = Frame::new(&size, [10.5, 20., 10.5, 50.], 100.);
        assert_eq!(frame.clip, [11, 30, 10, 50]);
        assert_eq!(frame.area(&size), frame.clip);
        let next = Frame::new(&size, [21., 20., 10.5, 50.], 100.);
        assert_eq!(next.clip[0], frame.clip[0] + frame.clip[2]);
    }

    #[test]
    fn clipped_to_card() {
        // A pane at [100, 40, 400, 300] of a 400 pixels tall window, its card inset by 8.
        let size = size();
        let frame = Frame::new(&size, [100., 40., 400., 300.], 400.);
        assert_eq!(frame.clip, [100, 60, 400, 300]);
        let card = frame.clipped([108., 48., 384., 284.], 400.);
        assert_eq!(card.clip, [108, 68, 384, 284]);
        // Only the clip changes, not where the terminal is drawn.
        assert_eq!(card.area(&size), frame.area(&size));

        // A rect reaching past the frame clips only where it overlaps.
        assert_eq!(frame.clipped([0., 0., 150., 100.], 400.).clip, [100, 300, 50, 60]);
        // Disjoint, nothing is drawn.
        assert_eq!(frame.clipped([600., 0., 50., 50.], 400.).clip[2], 0);
        assert_eq!(frame.clipped([200., 360., 50., 40.], 400.).clip[3], 0);
    }
}
