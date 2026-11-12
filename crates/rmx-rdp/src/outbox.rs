//! What is still owed to the interface, kept small however far behind it falls.

use ironrdp_pdu::geometry::InclusiveRectangle;
use rmx_display::{DisplayEvent, FrameUpdate, RemoteCursor};

/// Beyond this many separate rectangles they are merged into one.
const MAX_DIRTY_RECTS: usize = 32;

/// An exclusive-edged rectangle of the framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Rect {
    fn is_empty(&self) -> bool {
        self.left >= self.right || self.top >= self.bottom
    }

    fn contains(&self, other: &Rect) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    fn union(&self, other: &Rect) -> Rect {
        Rect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    fn clamped_to(&self, width: u32, height: u32) -> Rect {
        Rect {
            left: self.left.min(width),
            top: self.top.min(height),
            right: self.right.min(width),
            bottom: self.bottom.min(height),
        }
    }
}

impl From<&InclusiveRectangle> for Rect {
    fn from(rect: &InclusiveRectangle) -> Self {
        Self {
            left: rect.left.into(),
            top: rect.top.into(),
            right: u32::from(rect.right) + 1,
            bottom: u32::from(rect.bottom) + 1,
        }
    }
}

/// The current framebuffer: BGRA8 rows of `stride` bytes.
#[derive(Clone, Copy)]
pub(crate) struct Frame<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

/// Events waiting for room in the channel. Nothing here grows without bound: rectangles merge,
/// and a newer size, clipboard or cursor replaces an older one.
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    width: u32,
    height: u32,
    dirty: Vec<Rect>,
    resized: bool,
    clipboard: Option<String>,
    cursor: Option<RemoteCursor>,
}

impl Outbox {
    /// Announces a new desktop size. All pixels are unknown, so earlier dirty rectangles, which
    /// refer to the old surface, are forgotten.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.dirty.clear();
        self.resized = true;
    }

    /// Announces `width` x `height` if the desktop is not already that size.
    pub fn fit(&mut self, width: u32, height: u32) {
        if (self.width, self.height) != (width, height) {
            self.resize(width, height);
        }
    }

    /// Records that `rect` of the framebuffer changed.
    pub fn mark_dirty(&mut self, rect: Rect) {
        let rect = rect.clamped_to(self.width, self.height);
        if rect.is_empty() || self.dirty.iter().any(|d| d.contains(&rect)) {
            return;
        }
        self.dirty.retain(|d| !rect.contains(d));
        self.dirty.push(rect);
        if self.dirty.len() > MAX_DIRTY_RECTS {
            let merged = self.dirty.iter().skip(1).fold(self.dirty[0], |a, d| a.union(d));
            self.dirty.clear();
            self.dirty.push(merged);
        }
    }

    /// Records that the remote clipboard now holds `text`.
    pub fn clipboard(&mut self, text: String) {
        self.clipboard = Some(text);
    }

    /// Records that the remote pointer now looks like `cursor`.
    pub fn cursor(&mut self, cursor: RemoteCursor) {
        self.cursor = Some(cursor);
    }

    pub fn has_pending(&self) -> bool {
        self.resized || self.clipboard.is_some() || self.cursor.is_some() || !self.dirty.is_empty()
    }

    /// Takes the next event to send, reading any pixels from `frame` as they are now.
    pub fn next_event(&mut self, frame: Frame<'_>) -> Option<DisplayEvent> {
        if self.resized {
            self.resized = false;
            return Some(DisplayEvent::Resized { width: self.width, height: self.height });
        }
        if let Some(text) = self.clipboard.take() {
            return Some(DisplayEvent::Clipboard(text));
        }
        if let Some(cursor) = self.cursor.take() {
            return Some(DisplayEvent::Cursor(cursor));
        }
        let rect = self.dirty.pop()?;
        Some(DisplayEvent::Update(copy_rect(frame, rect)))
    }
}

/// Copies `rect` out of `frame` as tightly packed, opaque BGRA8.
///
/// RDP carries no alpha, so the channel is forced opaque whatever the decoder left there.
fn copy_rect(frame: Frame<'_>, rect: Rect) -> FrameUpdate {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    let row_len = width as usize * 4;
    let mut pixels = Vec::with_capacity(row_len * height as usize);
    for y in rect.top..rect.bottom {
        let start = y as usize * frame.stride + rect.left as usize * 4;
        pixels.extend_from_slice(&frame.data[start..start + row_len]);
    }
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 0xFF;
    }
    FrameUpdate { x: rect.left, y: rect.top, width, height, pixels }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 64;
    const H: u32 = 48;

    fn rect(left: u32, top: u32, right: u32, bottom: u32) -> Rect {
        Rect { left, top, right, bottom }
    }

    fn sized() -> Outbox {
        let mut outbox = Outbox::default();
        outbox.resize(W, H);
        outbox.resized = false;
        outbox
    }

    fn frame(data: &[u8]) -> Frame<'_> {
        Frame { data, stride: W as usize * 4 }
    }

    fn paint(fb: &mut [u8], r: Rect, value: u8) {
        for y in r.top..r.bottom {
            for x in r.left..r.right {
                let offset = (y * W + x) as usize * 4;
                fb[offset..offset + 3].fill(value);
                fb[offset + 3] = 0;
            }
        }
    }

    fn apply(screen: &mut [u8], update: &FrameUpdate) {
        for row in 0..update.height {
            let from = (row * update.width) as usize * 4;
            let to = ((update.y + row) * W + update.x) as usize * 4;
            let len = update.width as usize * 4;
            screen[to..to + len].copy_from_slice(&update.pixels[from..from + len]);
        }
    }

    #[test]
    fn covered_rects_are_absorbed() {
        let mut outbox = sized();
        outbox.mark_dirty(rect(0, 0, 10, 10));
        outbox.mark_dirty(rect(2, 2, 4, 4));
        assert_eq!(outbox.dirty, vec![rect(0, 0, 10, 10)]);
        outbox.mark_dirty(rect(0, 0, 20, 20));
        assert_eq!(outbox.dirty, vec![rect(0, 0, 20, 20)]);
    }

    #[test]
    fn rects_merge_into_their_bounds_once_too_many_wait() {
        let mut outbox = sized();
        for i in 0..=MAX_DIRTY_RECTS as u32 {
            outbox.mark_dirty(rect(i, 0, i + 1, 1));
        }
        assert_eq!(outbox.dirty, vec![rect(0, 0, MAX_DIRTY_RECTS as u32 + 1, 1)]);
    }

    #[test]
    fn rects_are_clamped_to_the_surface() {
        let mut outbox = sized();
        outbox.mark_dirty(rect(60, 40, 1000, 1000));
        assert_eq!(outbox.dirty, vec![rect(60, 40, W, H)]);
        outbox.mark_dirty(rect(1000, 1000, 2000, 2000));
        assert_eq!(outbox.dirty.len(), 1);
    }

    #[test]
    fn a_stalled_interface_still_receives_the_final_picture() {
        let mut outbox = sized();
        let mut source = vec![0u8; (W * H * 4) as usize];
        let mut screen = source.clone();

        // The interface takes nothing while the source is repainted many times.
        let mut value = 1u8;
        for round in 0..2000u32 {
            let r = rect(round % 20, round % 13, (round % 20) + 25, (round % 13) + 11);
            paint(&mut source, r, value);
            outbox.mark_dirty(r);
            value = value.wrapping_add(7);
        }
        assert!(outbox.dirty.len() <= MAX_DIRTY_RECTS + 1, "the backlog stayed bounded");

        while let Some(event) = outbox.next_event(frame(&source)) {
            let DisplayEvent::Update(update) = event else { panic!("expected an update") };
            apply(&mut screen, &update);
        }
        for (screen_px, source_px) in screen.chunks(4).zip(source.chunks(4)) {
            if source_px != [0, 0, 0, 0] {
                assert_eq!(&screen_px[..3], &source_px[..3]);
                assert_eq!(screen_px[3], 0xFF, "pixels are delivered opaque");
            }
        }
        assert!(!outbox.has_pending());
    }

    #[test]
    fn resize_comes_first_and_forgets_rects_of_the_old_surface() {
        let mut outbox = sized();
        outbox.mark_dirty(rect(0, 0, 10, 10));
        outbox.clipboard("text".to_owned());
        outbox.resize(32, 24);
        outbox.mark_dirty(rect(0, 0, 4, 4));

        let data = vec![0u8; 32 * 24 * 4];
        let frame = Frame { data: &data, stride: 32 * 4 };
        assert_eq!(outbox.next_event(frame), Some(DisplayEvent::Resized { width: 32, height: 24 }));
        assert_eq!(outbox.next_event(frame), Some(DisplayEvent::Clipboard("text".to_owned())));
        let Some(DisplayEvent::Update(update)) = outbox.next_event(frame) else {
            panic!("expected an update")
        };
        assert_eq!((update.x, update.y, update.width, update.height), (0, 0, 4, 4));
        assert_eq!(outbox.next_event(frame), None);
    }

    #[test]
    fn fit_announces_only_a_size_that_changed() {
        let mut outbox = sized();
        outbox.fit(W, H);
        assert!(!outbox.has_pending());

        outbox.fit(32, 24);
        let data = vec![0u8; 32 * 24 * 4];
        let frame = Frame { data: &data, stride: 32 * 4 };
        assert_eq!(outbox.next_event(frame), Some(DisplayEvent::Resized { width: 32, height: 24 }));
    }

    #[test]
    fn a_new_cursor_replaces_the_unsent_one() {
        let mut outbox = sized();
        outbox.cursor(RemoteCursor::Hidden);
        outbox.cursor(RemoteCursor::Default);
        let data = vec![0u8; (W * H * 4) as usize];
        assert_eq!(
            outbox.next_event(frame(&data)),
            Some(DisplayEvent::Cursor(RemoteCursor::Default))
        );
        assert_eq!(outbox.next_event(frame(&data)), None);
    }

    #[test]
    fn a_new_clipboard_replaces_the_unsent_one() {
        let mut outbox = sized();
        outbox.clipboard("old".to_owned());
        outbox.clipboard("new".to_owned());
        let data = vec![0u8; (W * H * 4) as usize];
        assert_eq!(
            outbox.next_event(frame(&data)),
            Some(DisplayEvent::Clipboard("new".to_owned()))
        );
        assert_eq!(outbox.next_event(frame(&data)), None);
    }

    #[test]
    fn copy_rect_reads_tight_rows_from_a_stride() {
        let mut data = vec![0u8; 8 * 3];
        // 2x3 surface of 4-pixel rows, column 1 painted.
        for y in 0..3 {
            data[y * 8 + 4..y * 8 + 7].copy_from_slice(&[1, 2, 3]);
        }
        let update = copy_rect(Frame { data: &data, stride: 8 }, rect(1, 0, 2, 3));
        assert_eq!(update.pixels, [1, 2, 3, 0xFF, 1, 2, 3, 0xFF, 1, 2, 3, 0xFF]);
    }
}
