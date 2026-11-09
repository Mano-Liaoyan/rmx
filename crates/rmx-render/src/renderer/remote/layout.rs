//! Where a remote desktop lands in its pane, and the way back from a window position to a remote
//! pixel. Pure arithmetic, so drawing and pointer input cannot disagree.

use crate::SizeInfo;
use crate::renderer::Frame;

/// How a remote desktop is scaled into its pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleMode {
    /// The whole desktop as large as the pane allows, centred between bars.
    Fit,
    /// One remote pixel to one window pixel, this many remote pixels from the top left, clamped
    /// to [`RemoteLayout::max_scroll`]. Centred in a pane larger than the desktop.
    Native { scroll: [u32; 2] },
}

/// Which texture filter a layout needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Every texel lands on one pixel: anything else would soften text.
    Nearest,
    Linear,
}

/// A remote pointer position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemotePointer {
    /// The remote pixel, clamped to the nearest one visible when off the picture.
    pub x: u32,
    pub y: u32,
    /// Whether the pointer is over the picture, rather than over a bar or outside the pane.
    pub inside: bool,
}

/// Where a remote desktop is drawn for a mode, frame and desktop size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RemoteLayout {
    /// Quad `[x, y, width, height]` in window pixels from the window's bottom left.
    pub dst: [i32; 4],
    /// Remote pixels `[x, y, width, height]` shown in it, from the desktop's top left.
    pub src: [u32; 4],
    pub filter: Filter,
    /// Half the distance between the samples of a pixel, in texels, for drawing smaller.
    pub tap: [f32; 2],
    /// Largest useful [`ScaleMode::Native`] scroll: zero on an axis that fits.
    pub max_scroll: [u32; 2],
}

impl RemoteLayout {
    /// Lay out a desktop of `remote` pixels in `frame`, which stretches `size` over the pane.
    pub fn new(mode: ScaleMode, remote: [u32; 2], frame: &Frame, size: &SizeInfo) -> Self {
        let [x, y, width, height] = frame.area(size);
        let pane = [size.width().round().max(1.) as u32, size.height().round().max(1.) as u32];

        // Mid-animation a frame may stretch its pane; at rest it must be exactly 1:1.
        let stretch = [axis_stretch(width, pane[0]), axis_stretch(height, pane[1])];

        let (dst_size, src, max_scroll) = match mode {
            ScaleMode::Fit => {
                let scale = (f64::from(width) / f64::from(remote[0].max(1)))
                    .min(f64::from(height) / f64::from(remote[1].max(1)));
                let side = |length: u32, room: i32| {
                    ((f64::from(length) * scale).round() as i32).clamp(1, room.max(1))
                };
                let dst_size = [side(remote[0], width), side(remote[1], height)];
                (dst_size, [0, 0, remote[0], remote[1]], [0, 0])
            },
            ScaleMode::Native { scroll } => {
                let visible = [remote[0].min(pane[0]), remote[1].min(pane[1])];
                let max_scroll = [remote[0] - visible[0], remote[1] - visible[1]];
                let offset = [scroll[0].min(max_scroll[0]), scroll[1].min(max_scroll[1])];
                let side = |axis: usize, room: i32| {
                    ((f64::from(visible[axis]) * stretch[axis]).round() as i32)
                        .clamp(1, room.max(1))
                };
                let dst_size = [side(0, width), side(1, height)];
                (dst_size, [offset[0], offset[1], visible[0], visible[1]], max_scroll)
            },
        };

        let [dst_width, dst_height] = dst_size;
        let dst =
            [x + (width - dst_width) / 2, y + (height - dst_height) / 2, dst_width, dst_height];

        let exact = dst_width as u32 == src[2] && dst_height as u32 == src[3];
        let filter = if exact { Filter::Nearest } else { Filter::Linear };

        let shrink = |shown: u32, drawn: i32| f64::from(shown) / f64::from(drawn.max(1));
        let tap = |shrink: f64| if shrink > 1. { (shrink * 0.25) as f32 } else { 0. };
        let tap = [tap(shrink(src[2], dst_width)), tap(shrink(src[3], dst_height))];

        Self { dst, src, filter, tap, max_scroll }
    }

    /// The remote pixel under a pointer at `x`, `y` in window pixels from the top left of a
    /// window `window_height` pixels tall.
    pub fn pointer(&self, window_height: f32, x: f64, y: f64) -> RemotePointer {
        let [dst_x, dst_y, dst_width, dst_height] = self.dst;
        let [src_x, src_y, src_width, src_height] = self.src;
        let top = f64::from(window_height) - f64::from(dst_y) - f64::from(dst_height);

        let (along_x, along_y) = (x - f64::from(dst_x), y - top);
        let inside = (0. ..f64::from(dst_width)).contains(&along_x)
            && (0. ..f64::from(dst_height)).contains(&along_y);

        let map = |along: f64, drawn: i32, start: u32, shown: u32| {
            let last = shown.saturating_sub(1);
            let pixel = (along * f64::from(shown) / f64::from(drawn.max(1))).floor();
            start + (pixel.max(0.) as u32).min(last)
        };
        RemotePointer {
            x: map(along_x, dst_width, src_x, src_width),
            y: map(along_y, dst_height, src_y, src_height),
            inside,
        }
    }
}

/// How much a frame stretches an axis, which is exactly 1 when it covers the pane's pixels.
fn axis_stretch(drawn: i32, pane: u32) -> f64 {
    if drawn == pane as i32 { 1. } else { f64::from(drawn) / f64::from(pane) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW_HEIGHT: f32 = 1200.;

    fn size(width: f32, height: f32) -> SizeInfo {
        SizeInfo::new(width, height, 10., 20., 0., 0., false)
    }

    /// A pane at `[x, y, width, height]` from the window's top left, unstretched.
    fn pane(x: f32, y: f32, width: f32, height: f32) -> (Frame, SizeInfo) {
        let size = size(width, height);
        (Frame::new(&size, [x, y, width, height], WINDOW_HEIGHT), size)
    }

    fn layout(mode: ScaleMode, remote: [u32; 2], (frame, size): (Frame, SizeInfo)) -> RemoteLayout {
        RemoteLayout::new(mode, remote, &frame, &size)
    }

    #[test]
    fn fit_letterboxes_a_wide_desktop() {
        // 16:9 into 5:4 at a third of the size: bars above and below.
        let fit = layout(ScaleMode::Fit, [3840, 2160], pane(100., 50., 1280., 1024.));
        assert_eq!(fit.dst, [100, 126 + 152, 1280, 720]);
        assert_eq!(fit.src, [0, 0, 3840, 2160]);
        assert_eq!(fit.filter, Filter::Linear);
        assert_eq!(fit.tap, [0.75, 0.75]);
    }

    #[test]
    fn fit_pillarboxes_a_tall_desktop() {
        let fit = layout(ScaleMode::Fit, [1000, 2000], pane(0., 0., 1000., 1000.));
        assert_eq!(fit.dst, [250, 200, 500, 1000]);
    }

    #[test]
    fn fit_at_exact_size_is_nearest() {
        let fit = layout(ScaleMode::Fit, [1280, 1024], pane(0., 0., 1280., 1024.));
        assert_eq!(fit.filter, Filter::Nearest);
        assert_eq!(fit.tap, [0., 0.]);
    }

    #[test]
    fn fit_magnifying_is_linear_without_taps() {
        let fit = layout(ScaleMode::Fit, [640, 512], pane(0., 0., 1280., 1024.));
        assert_eq!(fit.filter, Filter::Linear);
        assert_eq!(fit.tap, [0., 0.]);
    }

    #[test]
    fn fit_pointer_maps_corners_and_centre() {
        let fit = layout(ScaleMode::Fit, [3840, 2160], pane(100., 50., 1280., 1024.));
        // The picture's top is 50 + 152 pixels down the window.
        let at = |x, y| fit.pointer(WINDOW_HEIGHT, x, y);
        assert_eq!(at(100., 202.), RemotePointer { x: 0, y: 0, inside: true });
        assert_eq!(at(1379.5, 921.5), RemotePointer { x: 3838, y: 2158, inside: true });
        assert_eq!(at(740., 562.), RemotePointer { x: 1920, y: 1080, inside: true });
    }

    #[test]
    fn fit_pointer_on_a_bar_is_outside_and_clamped() {
        let fit = layout(ScaleMode::Fit, [3840, 2160], pane(100., 50., 1280., 1024.));
        assert_eq!(
            fit.pointer(WINDOW_HEIGHT, 740., 60.),
            RemotePointer { x: 1920, y: 0, inside: false }
        );
        assert_eq!(
            fit.pointer(WINDOW_HEIGHT, 740., 1000.),
            RemotePointer { x: 1920, y: 2159, inside: false }
        );
        assert_eq!(fit.pointer(WINDOW_HEIGHT, -500., 500.).x, 0);
        assert_eq!(fit.pointer(WINDOW_HEIGHT, 9000., 500.).x, 3839);
    }

    #[test]
    fn native_is_nearest_and_scrolls() {
        let native = ScaleMode::Native { scroll: [1000, 500] };
        let view = layout(native, [3840, 2160], pane(100., 50., 800., 600.));
        assert_eq!(view.dst, [100, 550, 800, 600]);
        assert_eq!(view.src, [1000, 500, 800, 600]);
        assert_eq!(view.filter, Filter::Nearest);
        assert_eq!(view.tap, [0., 0.]);
        assert_eq!(view.max_scroll, [3040, 1560]);

        assert_eq!(
            view.pointer(WINDOW_HEIGHT, 100., 50.),
            RemotePointer { x: 1000, y: 500, inside: true }
        );
        assert_eq!(
            view.pointer(WINDOW_HEIGHT, 899., 649.),
            RemotePointer { x: 1799, y: 1099, inside: true }
        );
    }

    #[test]
    fn native_scroll_clamps_to_the_desktop() {
        let native = ScaleMode::Native { scroll: [99_999, 99_999] };
        let view = layout(native, [3840, 2160], pane(0., 0., 800., 600.));
        assert_eq!(view.src, [3040, 1560, 800, 600]);
    }

    #[test]
    fn native_centres_a_small_desktop() {
        let native = ScaleMode::Native { scroll: [40, 40] };
        let view = layout(native, [400, 300], pane(0., 0., 800., 600.));
        // Scrolling means nothing where the desktop fits.
        assert_eq!(view.dst, [200, 750, 400, 300]);
        assert_eq!(view.src, [0, 0, 400, 300]);
        assert_eq!(view.max_scroll, [0, 0]);
        assert_eq!(view.filter, Filter::Nearest);

        // The picture starts 150 pixels down the pane.
        assert_eq!(
            view.pointer(WINDOW_HEIGHT, 200., 150.),
            RemotePointer { x: 0, y: 0, inside: true }
        );
        assert!(!view.pointer(WINDOW_HEIGHT, 100., 150.).inside);
    }

    #[test]
    fn a_stretched_frame_keeps_mapping_right() {
        // Popping in at half size: a linear filter, and the pointer still lands on the pixel
        // drawn under it.
        let size = size(800., 600.);
        let frame = Frame::new(&size, [0., 0., 400., 300.], WINDOW_HEIGHT);
        let native = ScaleMode::Native { scroll: [100, 100] };
        let view = RemoteLayout::new(native, [3840, 2160], &frame, &size);
        assert_eq!(view.dst, [0, 900, 400, 300]);
        assert_eq!(view.src, [100, 100, 800, 600]);
        assert_eq!(view.filter, Filter::Linear);
        assert_eq!(
            view.pointer(WINDOW_HEIGHT, 200., 150.),
            RemotePointer { x: 500, y: 400, inside: true }
        );
    }

    #[test]
    fn a_degenerate_frame_does_not_panic() {
        let (frame, size) = pane(0., 0., 800., 600.);
        let frame = Frame { sx: 0., sy: 0., ..frame };
        let view = RemoteLayout::new(ScaleMode::Fit, [3840, 2160], &frame, &size);
        assert!(view.dst[2] >= 1);
        let view = RemoteLayout::new(ScaleMode::Native { scroll: [0, 0] }, [1, 1], &frame, &size);
        assert!(view.dst[2] >= 1);
    }
}
