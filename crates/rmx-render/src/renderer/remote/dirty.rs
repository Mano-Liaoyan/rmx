//! Which pixels of the remote desktop changed, in a fixed amount of memory.

/// A rectangle of remote pixels, counted from the top left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn right(&self) -> u32 {
        self.x + self.width
    }

    fn bottom(&self) -> u32 {
        self.y + self.height
    }

    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Whether all of `other` is in this rectangle.
    pub fn contains(&self, other: &Rect) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && self.right() >= other.right()
            && self.bottom() >= other.bottom()
    }

    /// The smallest rectangle holding both.
    fn union(&self, other: &Rect) -> Rect {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        let (right, bottom) = (self.right().max(other.right()), self.bottom().max(other.bottom()));
        Rect { x, y, width: right - x, height: bottom - y }
    }

    fn overlap(&self, other: &Rect) -> u64 {
        let width = self.right().min(other.right()).saturating_sub(self.x.max(other.x));
        let height = self.bottom().min(other.bottom()).saturating_sub(self.y.max(other.y));
        u64::from(width) * u64::from(height)
    }

    /// Pixels the union of the two holds that neither does.
    fn waste(&self, other: &Rect) -> u64 {
        self.union(other).area() + self.overlap(other) - self.area() - other.area()
    }
}

/// Most rectangles a region keeps, and so most `glTexSubImage2D` calls an upload makes.
pub const MAX_RECTS: usize = 16;

/// Merging is worth uploading this many unchanged pixels, compared with one more upload call.
const CHEAP_WASTE: u64 = 64 * 64;

/// A set of pixels, at least those added: it may grow past them to stay within [`MAX_RECTS`], and
/// so only suits a source where every pixel of the region can be read, changed or not.
#[derive(Clone, Copy, Debug)]
pub struct DirtyRegion {
    rects: [Rect; MAX_RECTS + 1],
    len: usize,
}

impl Default for DirtyRegion {
    fn default() -> Self {
        Self { rects: [Rect::default(); MAX_RECTS + 1], len: 0 }
    }
}

impl DirtyRegion {
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.len]
    }

    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() || self.rects().iter().any(|held| held.contains(&rect)) {
            return;
        }

        self.rects[self.len] = rect;
        self.len += 1;

        while let Some((index, other, waste)) = self.cheapest_merge() {
            if waste > CHEAP_WASTE && self.len <= MAX_RECTS {
                break;
            }
            self.rects[index] = self.rects[index].union(&self.rects[other]);
            self.len -= 1;
            self.rects[other] = self.rects[self.len];
        }
    }

    pub fn merge(&mut self, other: &DirtyRegion) {
        other.rects().iter().for_each(|rect| self.add(*rect));
    }

    /// The two rectangles wasting the fewest pixels when merged.
    fn cheapest_merge(&self) -> Option<(usize, usize, u64)> {
        let rects = self.rects();
        let mut best = None;
        for (index, rect) in rects.iter().enumerate() {
            for (other, candidate) in rects.iter().enumerate().skip(index + 1) {
                let waste = rect.waste(candidate);
                if best.is_none_or(|(_, _, least)| waste < least) {
                    best = Some((index, other, waste));
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    #[test]
    fn empty_rects_are_ignored() {
        let mut region = DirtyRegion::default();
        region.add(rect(5, 5, 0, 10));
        region.add(rect(5, 5, 10, 0));
        assert!(region.is_empty());
    }

    #[test]
    fn a_held_rect_absorbs_what_it_contains() {
        let mut region = DirtyRegion::default();
        region.add(rect(0, 0, 500, 500));
        region.add(rect(10, 10, 20, 20));
        assert_eq!(region.rects(), &[rect(0, 0, 500, 500)]);

        // And the other way round.
        let mut region = DirtyRegion::default();
        region.add(rect(10, 10, 20, 20));
        region.add(rect(0, 0, 500, 500));
        assert_eq!(region.rects(), &[rect(0, 0, 500, 500)]);
    }

    #[test]
    fn touching_rects_merge_without_waste() {
        let mut region = DirtyRegion::default();
        region.add(rect(0, 0, 400, 300));
        region.add(rect(400, 0, 400, 300));
        region.add(rect(0, 300, 800, 300));
        assert_eq!(region.rects(), &[rect(0, 0, 800, 600)]);
    }

    #[test]
    fn distant_rects_stay_apart() {
        let mut region = DirtyRegion::default();
        region.add(rect(0, 0, 100, 100));
        region.add(rect(3000, 2000, 100, 100));
        assert_eq!(region.rects().len(), 2);
    }

    /// Every pixel added is still in the region, however many rects forced merges.
    #[test]
    fn merging_never_loses_a_pixel() {
        const SIZE: u32 = 256;
        let mut seed = 0x9e37_79b9_u32;
        let mut next = |bound: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % bound
        };

        for _ in 0..200 {
            let mut region = DirtyRegion::default();
            let mut model = vec![false; (SIZE * SIZE) as usize];
            for _ in 0..next(80) + 1 {
                let (x, y) = (next(SIZE), next(SIZE));
                let added = rect(x, y, next(SIZE - x) + 1, next(SIZE - y) + 1);
                region.add(added);
                for row in added.y..added.bottom() {
                    for column in added.x..added.right() {
                        model[(row * SIZE + column) as usize] = true;
                    }
                }
                assert!(region.rects().len() <= MAX_RECTS);
            }

            for row in 0..SIZE {
                for column in 0..SIZE {
                    if model[(row * SIZE + column) as usize] {
                        let pixel = rect(column, row, 1, 1);
                        assert!(region.rects().iter().any(|held| held.contains(&pixel)));
                    }
                }
            }
        }
    }

    #[test]
    fn the_cap_holds_with_rects_far_apart() {
        let mut region = DirtyRegion::default();
        for index in 0..40 {
            region.add(rect(index * 500, index * 500, 10, 10));
        }
        assert_eq!(region.rects().len(), MAX_RECTS);
    }

    #[test]
    fn merge_covers_both_regions() {
        let mut one = DirtyRegion::default();
        one.add(rect(0, 0, 10, 10));
        let mut other = DirtyRegion::default();
        other.add(rect(1000, 1000, 10, 10));
        one.merge(&other);
        assert_eq!(one.rects().len(), 2);
        assert!(one.rects().contains(&rect(1000, 1000, 10, 10)));
    }
}
