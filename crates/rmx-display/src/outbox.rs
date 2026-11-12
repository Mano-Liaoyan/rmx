//! Keeps the latest picture of a remote desktop and decides what the interface still has to be told.
//!
//! Nothing here does I/O or waits, so the backpressure behaviour can be tested exhaustively.

use std::mem;

use crate::{DisplayEvent, FrameUpdate};

const BYTES_PER_PIXEL: usize = 4;

/// How many separate regions are remembered before the closest ones are merged.
pub const MAX_DAMAGE_RECTS: usize = 32;

/// A region of the remote desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn right(self) -> u32 {
        self.x + self.width
    }

    fn bottom(self) -> u32 {
        self.y + self.height
    }

    fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn overlaps(self, other: Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    fn union(self, other: Rect) -> Rect {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        Rect {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    /// The area a merge of the two would resend without need.
    fn merge_waste(self, other: Rect) -> u64 {
        self.union(other).area().saturating_sub(self.area() + other.area())
    }

    fn should_merge(self, other: Rect) -> bool {
        self.overlaps(other) || self.merge_waste(other) == 0
    }
}

/// Adds `new` to the regions still owed to the interface and returns the result.
///
/// Every pixel of `pending` and `new` is covered by what is returned, and at most
/// [`MAX_DAMAGE_RECTS`] regions are returned. Merging may cover pixels that did not change,
/// which is harmless because the pixels sent for a region are always read from the current
/// picture. `new`, grown by what it absorbed, comes last.
pub fn coalesce(mut pending: Vec<Rect>, new: Rect) -> Vec<Rect> {
    if new.is_empty() {
        return pending;
    }

    // Growing can reach regions already passed, so repeat until nothing more is absorbed.
    let mut merged = new;
    loop {
        let before = pending.len();
        pending.retain(|&rect| {
            let absorb = merged.should_merge(rect);
            if absorb {
                merged = merged.union(rect);
            }
            !absorb
        });
        if pending.len() == before {
            break;
        }
    }
    pending.push(merged);

    while pending.len() > MAX_DAMAGE_RECTS {
        let (a, b) = cheapest_pair(&pending);
        let union = pending[a].union(pending[b]);
        pending[a] = union;
        pending.remove(b);
    }
    pending
}

/// The indices, in ascending order, of the two regions that cost least to merge.
fn cheapest_pair(rects: &[Rect]) -> (usize, usize) {
    let mut best = (0, 1);
    let mut best_waste = u64::MAX;
    for (a, &first) in rects.iter().enumerate() {
        for (b, &second) in rects.iter().enumerate().skip(a + 1) {
            let waste = first.merge_waste(second);
            if waste < best_waste {
                best = (a, b);
                best_waste = waste;
            }
        }
    }
    best
}

/// A rectangle that does not fit the desktop, or pixels that do not fit their rectangle.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidRect;

/// The latest picture of the remote desktop, plus what the interface has not been told.
///
/// The pending state has a fixed bound: a clipboard, a size, a bell and at most
/// [`MAX_DAMAGE_RECTS`] regions.
#[derive(Debug, Default)]
pub struct Outbox {
    width: u32,
    height: u32,
    picture: Vec<u8>,
    damage: Vec<Rect>,
    resized: bool,
    clipboard: Option<String>,
    bell: bool,
    closed: Option<Option<String>>,
}

impl Outbox {
    /// The desktop has this size now. Returns whether that is a change.
    ///
    /// Regions still owed refer to the old picture, so they are forgotten.
    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        if (width, height) == (self.width, self.height) {
            return false;
        }
        self.width = width;
        self.height = height;
        self.picture.clear();
        self.picture.resize(byte_len(width, height), 0);
        self.damage.clear();
        self.resized = true;
        true
    }

    /// Whether a new update could go straight to the interface without overtaking anything.
    pub fn is_idle(&self) -> bool {
        self.damage.is_empty()
            && !self.resized
            && self.clipboard.is_none()
            && !self.bell
            && self.closed.is_none()
    }

    /// Whether pixels are still owed.
    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }

    /// Whether [`next_event`](Self::next_event) has something to return.
    pub fn has_pending(&self) -> bool {
        !self.is_idle()
    }

    /// Paints `update` into the picture without marking it as owed.
    pub fn paint(&mut self, update: &FrameUpdate) -> Result<(), InvalidRect> {
        let rect = Rect { x: update.x, y: update.y, width: update.width, height: update.height };
        self.check(rect)?;
        if update.pixels.len() != byte_len(rect.width, rect.height) {
            return Err(InvalidRect);
        }
        let row_len = rect.width as usize * BYTES_PER_PIXEL;
        for (row, source) in update.pixels.chunks_exact(row_len.max(1)).enumerate() {
            let start = self.offset(rect.x, rect.y + row as u32);
            self.picture[start..start + row_len].copy_from_slice(source);
        }
        Ok(())
    }

    /// Copies a region of the picture onto another, as CopyRect asks.
    pub fn copy(&mut self, dst: Rect, src: Rect) -> Result<(), InvalidRect> {
        if (dst.width, dst.height) != (src.width, src.height) {
            return Err(InvalidRect);
        }
        self.check(dst)?;
        self.check(src)?;
        let row_len = src.width as usize * BYTES_PER_PIXEL;
        // Walk away from the destination so overlapping rows are read before being overwritten.
        let top_down = dst.y <= src.y;
        for step in 0..src.height {
            let row = if top_down { step } else { src.height - 1 - step };
            let from = self.offset(src.x, src.y + row);
            let to = self.offset(dst.x, dst.y + row);
            self.picture.copy_within(from..from + row_len, to);
        }
        Ok(())
    }

    /// Marks `rect` as owed to the interface.
    pub fn add_damage(&mut self, rect: Rect) {
        self.damage = coalesce(mem::take(&mut self.damage), rect);
    }

    /// Remembers the newest remote clipboard; an older one not yet delivered is obsolete.
    pub fn set_clipboard(&mut self, text: String) {
        self.clipboard = Some(text);
    }

    pub fn ring_bell(&mut self) {
        self.bell = true;
    }

    /// Ends the session. [`next_event`](Self::next_event) reports this after everything else.
    pub fn close(&mut self, reason: Option<String>) {
        self.closed = Some(reason);
    }

    /// Forgets the pixels still owed, for a session the user has ended.
    pub fn discard_damage(&mut self) {
        self.damage.clear();
    }

    /// The next event for the interface, for when the channel has room.
    ///
    /// A size change goes before any update, so updates never reach a stale surface.
    pub fn next_event(&mut self) -> Option<DisplayEvent> {
        if mem::take(&mut self.resized) {
            return Some(DisplayEvent::Resized { width: self.width, height: self.height });
        }
        if let Some(text) = self.clipboard.take() {
            return Some(DisplayEvent::Clipboard(text));
        }
        if mem::take(&mut self.bell) {
            return Some(DisplayEvent::Bell);
        }
        if !self.damage.is_empty() {
            let rect = self.damage.remove(0);
            return Some(DisplayEvent::Update(self.crop(rect)));
        }
        self.closed.take().map(DisplayEvent::Closed)
    }

    fn check(&self, rect: Rect) -> Result<(), InvalidRect> {
        let fits = rect.x.checked_add(rect.width).is_some_and(|right| right <= self.width)
            && rect.y.checked_add(rect.height).is_some_and(|bottom| bottom <= self.height);
        fits.then_some(()).ok_or(InvalidRect)
    }

    fn offset(&self, x: u32, y: u32) -> usize {
        (y as usize * self.width as usize + x as usize) * BYTES_PER_PIXEL
    }

    fn crop(&self, rect: Rect) -> FrameUpdate {
        let row_len = rect.width as usize * BYTES_PER_PIXEL;
        let mut pixels = Vec::with_capacity(row_len * rect.height as usize);
        for row in 0..rect.height {
            let start = self.offset(rect.x, rect.y + row);
            pixels.extend_from_slice(&self.picture[start..start + row_len]);
        }
        FrameUpdate { x: rect.x, y: rect.y, width: rect.width, height: rect.height, pixels }
    }
}

fn byte_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * BYTES_PER_PIXEL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    fn update(x: u32, y: u32, width: u32, height: u32, value: u8) -> FrameUpdate {
        let pixels = vec![value; byte_len(width, height)];
        FrameUpdate { x, y, width, height, pixels }
    }

    fn covers(rects: &[Rect], x: u32, y: u32) -> bool {
        rects.iter().any(|r| r.overlaps(rect(x, y, 1, 1)))
    }

    /// A small deterministic generator; tests need variety, not quality.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }

        fn below(&mut self, limit: u32) -> u32 {
            self.next() % limit.max(1)
        }

        fn rect_within(&mut self, width: u32, height: u32) -> Rect {
            let x = self.below(width);
            let y = self.below(height);
            rect(x, y, 1 + self.below(width - x), 1 + self.below(height - y))
        }
    }

    #[test]
    fn disjoint_regions_stay_separate() {
        let merged = coalesce(vec![rect(0, 0, 2, 2)], rect(10, 10, 2, 2));
        assert_eq!(merged, [rect(0, 0, 2, 2), rect(10, 10, 2, 2)]);
    }

    #[test]
    fn overlapping_regions_merge_into_the_newest() {
        let merged = coalesce(vec![rect(10, 10, 2, 2), rect(0, 0, 4, 4)], rect(2, 2, 4, 4));
        assert_eq!(merged, [rect(10, 10, 2, 2), rect(0, 0, 6, 6)]);
    }

    #[test]
    fn exact_tiles_merge_without_waste() {
        assert_eq!(coalesce(vec![rect(0, 0, 4, 4)], rect(4, 0, 4, 4)), [rect(0, 0, 8, 4)]);
    }

    #[test]
    fn merging_cascades_through_regions_it_grows_into() {
        // The second region only overlaps once the first merge has grown the new one.
        let merged = coalesce(vec![rect(8, 0, 2, 2), rect(0, 0, 4, 2)], rect(3, 0, 6, 1));
        assert_eq!(merged, [rect(0, 0, 10, 2)]);
    }

    #[test]
    fn a_full_frame_absorbs_everything() {
        let pending: Vec<_> = (0..20).map(|i| rect(i * 3, i * 3, 2, 2)).collect();
        assert_eq!(coalesce(pending, rect(0, 0, 100, 100)), [rect(0, 0, 100, 100)]);
    }

    #[test]
    fn an_empty_region_changes_nothing() {
        let pending = vec![rect(0, 0, 2, 2)];
        assert_eq!(coalesce(pending.clone(), rect(5, 5, 0, 9)), pending);
    }

    #[test]
    fn the_region_count_is_bounded() {
        let mut pending = Vec::new();
        for i in 0..200 {
            pending = coalesce(pending, rect(i * 10 % 1000, i * 7 % 700, 2, 2));
            assert!(pending.len() <= MAX_DAMAGE_RECTS);
        }
    }

    #[test]
    fn every_damaged_pixel_stays_covered() {
        let mut rng = Rng(7);
        let mut pending = Vec::new();
        let mut truth = vec![false; 200 * 100];
        for _ in 0..500 {
            let new = rng.rect_within(200, 100);
            pending = coalesce(pending, new);
            for y in new.y..new.y + new.height {
                for x in new.x..new.x + new.width {
                    truth[(y * 200 + x) as usize] = true;
                }
            }
            for (i, damaged) in truth.iter().enumerate() {
                let (x, y) = (i as u32 % 200, i as u32 / 200);
                assert!(!damaged || covers(&pending, x, y), "lost pixel {x},{y}");
            }
        }
    }

    #[test]
    fn copy_handles_overlap_in_every_direction() {
        for (src_x, src_y, dst_x, dst_y) in [(1, 1, 2, 2), (2, 2, 1, 1), (1, 2, 2, 1), (2, 1, 1, 2)]
        {
            let mut outbox = Outbox::default();
            outbox.resize(5, 5);
            let pixels: Vec<u8> = (0..100).collect();
            outbox
                .paint(&FrameUpdate { x: 0, y: 0, width: 5, height: 5, pixels: pixels.clone() })
                .unwrap();

            let (src, dst) = (rect(src_x, src_y, 3, 3), rect(dst_x, dst_y, 3, 3));
            outbox.copy(dst, src).unwrap();

            let mut expected = pixels.clone();
            for row in 0..3 {
                for col in 0..12 {
                    let from = ((src_y + row) * 20 + src_x * 4 + col) as usize;
                    let to = ((dst_y + row) * 20 + dst_x * 4 + col) as usize;
                    expected[to] = pixels[from];
                }
            }
            assert_eq!(outbox.picture, expected);
        }
    }

    #[test]
    fn invalid_rectangles_are_refused() {
        let mut outbox = Outbox::default();
        outbox.resize(4, 4);
        assert_eq!(outbox.paint(&update(3, 3, 2, 1, 0)), Err(InvalidRect));
        let short = FrameUpdate { x: 0, y: 0, width: 2, height: 2, pixels: vec![0; 15] };
        assert_eq!(outbox.paint(&short), Err(InvalidRect));
        assert_eq!(outbox.copy(rect(3, 0, 2, 1), rect(0, 0, 2, 1)), Err(InvalidRect));
        assert_eq!(outbox.copy(rect(0, 0, 1, 1), rect(0, 0, 2, 1)), Err(InvalidRect));
        assert_eq!(outbox.paint(&update(u32::MAX, 0, 2, 1, 0)), Err(InvalidRect));
    }

    #[test]
    fn a_stale_update_is_resent_with_the_newest_pixels() {
        let mut outbox = Outbox::default();
        outbox.resize(4, 4);
        assert!(matches!(outbox.next_event(), Some(DisplayEvent::Resized { .. })));

        for value in 1..=5 {
            outbox.paint(&update(0, 0, 2, 2, value)).unwrap();
            outbox.add_damage(rect(0, 0, 2, 2));
        }

        assert_eq!(outbox.next_event(), Some(DisplayEvent::Update(update(0, 0, 2, 2, 5))));
        assert_eq!(outbox.next_event(), None);
    }

    #[test]
    fn resize_goes_first_and_forgets_old_damage() {
        let mut outbox = Outbox::default();
        outbox.resize(4, 4);
        outbox.next_event();
        outbox.paint(&update(0, 0, 4, 4, 9)).unwrap();
        outbox.add_damage(rect(0, 0, 4, 4));

        assert!(outbox.resize(2, 2));

        assert_eq!(outbox.next_event(), Some(DisplayEvent::Resized { width: 2, height: 2 }));
        assert_eq!(outbox.next_event(), None);
    }

    #[test]
    fn repeating_the_size_is_not_a_resize() {
        let mut outbox = Outbox::default();
        assert!(outbox.resize(4, 4));
        outbox.next_event();
        outbox.paint(&update(0, 0, 4, 4, 9)).unwrap();
        outbox.add_damage(rect(0, 0, 4, 4));

        assert!(!outbox.resize(4, 4));

        assert_eq!(outbox.next_event(), Some(DisplayEvent::Update(update(0, 0, 4, 4, 9))));
    }

    #[test]
    fn only_the_newest_clipboard_and_one_bell_wait() {
        let mut outbox = Outbox::default();
        outbox.set_clipboard("old".into());
        outbox.set_clipboard("new".into());
        outbox.ring_bell();
        outbox.ring_bell();

        assert_eq!(outbox.next_event(), Some(DisplayEvent::Clipboard("new".into())));
        assert_eq!(outbox.next_event(), Some(DisplayEvent::Bell));
        assert_eq!(outbox.next_event(), None);
    }

    #[test]
    fn closed_comes_after_the_pixels_owed() {
        let mut outbox = Outbox::default();
        outbox.resize(2, 2);
        outbox.next_event();
        outbox.paint(&update(0, 0, 2, 2, 1)).unwrap();
        outbox.add_damage(rect(0, 0, 2, 2));
        outbox.close(Some("gone".into()));

        assert!(matches!(outbox.next_event(), Some(DisplayEvent::Update(_))));
        assert_eq!(outbox.next_event(), Some(DisplayEvent::Closed(Some("gone".into()))));
        assert_eq!(outbox.next_event(), None);
    }

    #[test]
    fn discarding_damage_keeps_the_close() {
        let mut outbox = Outbox::default();
        outbox.resize(2, 2);
        outbox.next_event();
        outbox.add_damage(rect(0, 0, 2, 2));
        outbox.close(None);
        outbox.discard_damage();

        assert_eq!(outbox.next_event(), Some(DisplayEvent::Closed(None)));
    }

    /// What the interface would show after applying `event`.
    fn apply(screen: &mut (u32, u32, Vec<u8>), event: DisplayEvent) {
        match event {
            DisplayEvent::Resized { width, height } => {
                *screen = (width, height, vec![0; byte_len(width, height)]);
            },
            DisplayEvent::Update(update) => {
                let row_len = update.width as usize * BYTES_PER_PIXEL;
                for (row, source) in update.pixels.chunks_exact(row_len).enumerate() {
                    let y = update.y as usize + row;
                    let start = (y * screen.0 as usize + update.x as usize) * BYTES_PER_PIXEL;
                    screen.2[start..start + row_len].copy_from_slice(source);
                }
            },
            _ => {},
        }
    }

    /// Plays random server traffic against an interface that reads at random moments.
    #[test]
    fn the_final_picture_is_correct_however_the_interface_lags() {
        for seed in 0..50 {
            let mut rng = Rng(seed);
            let mut outbox = Outbox::default();
            let mut screen = (0, 0, Vec::new());
            let (mut width, mut height) = (64, 48);
            outbox.resize(width, height);
            let mut truth = vec![0u8; byte_len(width, height)];

            for _ in 0..400 {
                match rng.below(20) {
                    0 => {
                        (width, height) = (8 + rng.below(80), 8 + rng.below(60));
                        if outbox.resize(width, height) {
                            truth = vec![0; byte_len(width, height)];
                        }
                    },
                    1..=4 => {
                        let src = rng.rect_within(width, height);
                        let dst = rect(
                            rng.below(width - src.width + 1),
                            rng.below(height - src.height + 1),
                            src.width,
                            src.height,
                        );
                        outbox.copy(dst, src).unwrap();
                        outbox.add_damage(dst);

                        let row_len = src.width as usize * BYTES_PER_PIXEL;
                        let old = truth.clone();
                        for row in 0..src.height as usize {
                            let from =
                                ((src.y as usize + row) * width as usize + src.x as usize) * 4;
                            let to = ((dst.y as usize + row) * width as usize + dst.x as usize) * 4;
                            truth[to..to + row_len].copy_from_slice(&old[from..from + row_len]);
                        }
                    },
                    5..=13 => {
                        let area = rng.rect_within(width, height);
                        let value = rng.below(255) as u8 + 1;
                        let new = update(area.x, area.y, area.width, area.height, value);
                        outbox.paint(&new).unwrap();
                        outbox.add_damage(area);

                        let row_len = area.width as usize * BYTES_PER_PIXEL;
                        for row in 0..area.height as usize {
                            let y = area.y as usize + row;
                            let start = (y * width as usize + area.x as usize) * 4;
                            truth[start..start + row_len].fill(value);
                        }
                    },
                    _ => {
                        for _ in 0..rng.below(3) {
                            if let Some(event) = outbox.next_event() {
                                apply(&mut screen, event);
                            }
                        }
                    },
                }
                assert!(outbox.damage.len() <= MAX_DAMAGE_RECTS);
            }

            while let Some(event) = outbox.next_event() {
                apply(&mut screen, event);
            }
            assert_eq!((screen.0, screen.1), (width, height), "seed {seed}");
            assert!(screen.2 == truth, "picture differs for seed {seed}");
        }
    }
}
