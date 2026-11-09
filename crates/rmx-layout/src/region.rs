//! Which parts of the window a frame changes, across the buffers a compositor cycles through.

use crate::Rect;

/// Rects kept separately before a region collapses into their bounding box.
const MAX_RECTS: usize = 16;
/// The oldest buffer age partial drawing supports; older buffers are redrawn in full.
const MAX_AGE: usize = 4;

/// A part of the window, without allocating.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Region {
    rects: [Rect; MAX_RECTS],
    len: usize,
}

impl Region {
    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.len]
    }

    pub fn push(&mut self, rect: Rect) {
        if rect.w <= 0. || rect.h <= 0. {
            return;
        }
        if self.len == MAX_RECTS {
            let bounds = self.rects().iter().fold(rect, |all, other| union(all, *other));
            self.rects[0] = bounds;
            self.len = 1;
            return;
        }
        self.rects[self.len] = rect;
        self.len += 1;
    }

    pub fn extend(&mut self, other: &Region) {
        for rect in other.rects() {
            self.push(*rect);
        }
    }

    pub fn intersects(&self, rect: &Rect) -> bool {
        self.rects().iter().any(|own| own.intersects(rect))
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y)
}

/// What the last frames changed, to bring an older back buffer up to date.
#[derive(Debug, Default)]
pub struct DamageHistory {
    /// The most recent frames, newest first.
    frames: [Option<Region>; MAX_AGE],
}

impl DamageHistory {
    /// Forget everything: the next buffer has to be drawn in full.
    pub fn reset(&mut self) {
        self.frames = [None; MAX_AGE];
    }

    /// Record that a frame changes `current`, on a back buffer `age` frames old, where 0 means
    /// its content is unknown.
    ///
    /// Returns what to redraw, or `None` to redraw everything.
    pub fn advance(&mut self, current: Option<Region>, age: usize) -> Option<Region> {
        let needed = if age == 0 || age > MAX_AGE {
            None
        } else {
            let older = self.frames[..age - 1].iter().copied();
            older.fold(current, |all, frame| {
                let (mut all, frame) = (all?, frame?);
                all.extend(&frame);
                Some(all)
            })
        };

        self.frames.rotate_right(1);
        self.frames[0] = current;
        needed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: f32) -> Region {
        let mut region = Region::default();
        region.push(Rect::new(x, 0., 10., 10.));
        region
    }

    #[test]
    fn unknown_buffer_is_redrawn_in_full() {
        let mut history = DamageHistory::default();
        assert_eq!(history.advance(Some(region(0.)), 0), None);
    }

    #[test]
    fn newest_buffer_needs_only_this_frame() {
        let mut history = DamageHistory::default();
        history.advance(Some(region(0.)), 1);
        assert_eq!(history.advance(Some(region(50.)), 1), Some(region(50.)));
    }

    #[test]
    fn older_buffer_also_needs_the_frames_it_missed() {
        let mut history = DamageHistory::default();
        history.advance(Some(region(0.)), 1);
        let needed = history.advance(Some(region(50.)), 2).unwrap();
        assert!(needed.intersects(&Rect::new(0., 0., 1., 1.)));
        assert!(needed.intersects(&Rect::new(50., 0., 1., 1.)));
    }

    #[test]
    fn a_full_frame_in_between_means_a_full_redraw() {
        let mut history = DamageHistory::default();
        history.advance(None, 1);
        assert_eq!(history.advance(Some(region(50.)), 2), None);
    }

    #[test]
    fn only_the_dirty_pane_needs_redrawing_at_any_buffer_age() {
        let panes: Vec<Rect> = (0..4).map(|i| Rect::new(i as f32 * 100., 0., 90., 90.)).collect();
        let mut history = DamageHistory::default();

        // Frames in which only the third pane has output, on a compositor cycling buffers.
        let mut dirty = Region::default();
        dirty.push(panes[2]);
        for age in [1, 2, 3, 4, 2, 1] {
            let needed = history.advance(Some(dirty), age);
            // The very first frames still have to fill unknown buffers.
            if let Some(needed) = needed {
                let redrawn: Vec<bool> = panes.iter().map(|pane| needed.intersects(pane)).collect();
                assert_eq!(redrawn, [false, false, true, false]);
            }
        }
    }

    #[test]
    fn too_many_rects_collapse_into_their_bounds() {
        let mut region = Region::default();
        for i in 0..=MAX_RECTS {
            region.push(Rect::new(i as f32 * 20., 0., 10., 10.));
        }
        assert_eq!(region.rects().len(), 1);
        assert!(region.intersects(&Rect::new(5., 0., 1., 1.)));
    }
}
