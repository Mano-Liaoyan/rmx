//! How a tab's panes share its area: a binary tree of splits, like tmux's.
//!
//! The tree only knows the panes' ids; the tab keeps the panes themselves.

use crate::Rect;

/// Fewest columns and lines a split may leave either pane.
pub const MIN_COLUMNS: f32 = 20.;
pub const MIN_LINES: f32 = 5.;

/// How two panes are placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Side by side, with a vertical divider.
    Row,
    /// Stacked, with a horizontal divider.
    Column,
}

/// Which side of a pane another one goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Left or above.
    Before,
    /// Right or below.
    After,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Split<P> {
    Pane(P),
    /// Two parts, `first` taking `ratio` of the space.
    Pair {
        axis: Axis,
        ratio: f32,
        first: Box<Split<P>>,
        second: Box<Split<P>>,
    },
}

impl<P> Split<P> {
    /// The same tree, each pane given by `f`, in the order of the tree.
    pub fn map<Q>(self, f: &mut impl FnMut(P) -> Q) -> Split<Q> {
        match self {
            Split::Pane(pane) => Split::Pane(f(pane)),
            Split::Pair { axis, ratio, first, second } => Split::Pair {
                axis,
                ratio,
                first: Box::new(first.map(f)),
                second: Box::new(second.map(f)),
            },
        }
    }
}

impl<P: Copy + PartialEq> Split<P> {
    /// The panes in the order of the tree: those of the first part, then of the second, so
    /// panes side by side or stacked come left to right or top to bottom.
    pub fn panes(&self) -> Vec<P> {
        self.rects(Rect::default(), 0.).into_iter().map(|(pane, _)| pane).collect()
    }

    /// This and `new` side by side or stacked, sharing the space evenly.
    pub fn join(self, new: Split<P>, axis: Axis, side: Side) -> Self {
        let (first, second) = match side {
            Side::Before => (new, self),
            Side::After => (self, new),
        };
        Split::Pair { axis, ratio: 0.5, first: Box::new(first), second: Box::new(second) }
    }

    /// Put `new` beside the pane `target`, returning whether it is there.
    pub fn insert(&mut self, target: P, new: Split<P>, axis: Axis, side: Side) -> bool {
        let Some(leaf) = self.leaf_mut(target) else { return false };
        *leaf = Split::Pane(target).join(new, axis, side);
        true
    }

    /// The tree without `pane`, whose neighbour takes its space; none when it was the only one.
    pub fn remove(self, pane: P) -> Option<Self> {
        match self {
            Split::Pane(p) if p == pane => None,
            Split::Pane(_) => Some(self),
            Split::Pair { axis, ratio, first, second } => {
                match (first.remove(pane), second.remove(pane)) {
                    (Some(first), Some(second)) => Some(Split::Pair {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(rest), None) | (None, Some(rest)) => Some(rest),
                    (None, None) => None,
                }
            },
        }
    }

    /// Each pane's part of `area`, which they tile in whole pixels, `spacing` apart.
    ///
    /// A negative `spacing` makes neighbours overlap by as much.
    pub fn rects(&self, area: Rect, spacing: f32) -> Vec<(P, Rect)> {
        let mut rects = Vec::new();
        self.tile(area, spacing, &mut rects);
        rects
    }

    /// The divider of each pair of parts laid out in `area`, in the order of the tree.
    pub fn dividers(&self, area: Rect, spacing: f32) -> Vec<Divider> {
        let mut dividers = Vec::new();
        self.walk(area, spacing, &mut |split, area| {
            if let Split::Pair { axis, ratio, .. } = *split {
                let (a, _) = halves(area, axis, ratio, spacing);
                let at = match axis {
                    Axis::Row => a.right() + spacing / 2.,
                    Axis::Column => a.bottom() + spacing / 2.,
                };
                dividers.push(Divider { index: dividers.len(), axis, area, at });
            }
        });
        dividers
    }

    /// Give the pair `index`, in the order of the tree, `ratio` of its space; false when there is
    /// no such pair.
    pub fn set_ratio(&mut self, index: usize, ratio: f32) -> bool {
        let mut index = index;
        match self.pair_mut(&mut index) {
            Some(Split::Pair { ratio: old, .. }) => {
                *old = ratio;
                true
            },
            _ => false,
        }
    }

    /// Smallest size this can be laid out in, keeping its ratios, with every pane at least
    /// `min`, `spacing` apart.
    pub fn min_size(&self, min: (f32, f32), spacing: f32) -> (f32, f32) {
        match self {
            Split::Pane(_) => min,
            Split::Pair { axis, ratio, first, second } => {
                let (a, b) = (first.min_size(min, spacing), second.min_size(min, spacing));
                let len = |a: f32, b: f32| spacing + (a / ratio).max(b / (1. - ratio));
                match axis {
                    Axis::Row => (len(a.0, b.0), a.1.max(b.1)),
                    Axis::Column => (a.0.max(b.0), len(a.1, b.1)),
                }
            },
        }
    }

    /// The ratios the pair `index` may take in `area` with every pane at least `min`, from
    /// lowest to highest; the middle when it cannot leave them that.
    pub fn ratio_range(
        &self,
        index: usize,
        area: Rect,
        min: (f32, f32),
        spacing: f32,
    ) -> Option<(f32, f32)> {
        let mut range = None;
        let mut count = 0;
        self.walk(area, spacing, &mut |split, area| {
            let Split::Pair { axis, first, second, .. } = split else { return };
            if count == index {
                let (a, b) = (first.min_size(min, spacing), second.min_size(min, spacing));
                let (len, a, b) = match axis {
                    Axis::Row => (area.w, a.0, b.0),
                    Axis::Column => (area.h, a.1, b.1),
                };
                let (lo, hi) = (a / (len - spacing), 1. - b / (len - spacing));
                range = Some(if lo <= hi { (lo, hi) } else { (0.5, 0.5) });
            }
            count += 1;
        });
        range
    }

    /// Where `new` joins this group laid out in `area`, every pane keeping at least `min`,
    /// `spacing` apart: after all of its panes, `None`, while they keep that much, or else after
    /// the roomiest pane it fits after, as a split would. None when it fits nowhere.
    ///
    /// Halving the whole group soon leaves too little for a group of a few panes, while one of
    /// them still has room enough to share.
    pub fn spot(
        &self,
        new: &Split<P>,
        area: Rect,
        min: (f32, f32),
        spacing: f32,
    ) -> Option<(Option<P>, Axis)> {
        let axis = auto_axis(area);
        if self.clone().join(new.clone(), axis, Side::After).fits_in(area, min, spacing) {
            return Some((None, axis));
        }
        let mut panes = self.rects(area, spacing);
        // Roomiest first; a stable sort keeps the order of the tree among equals.
        panes.sort_by(|(_, a), (_, b)| (b.w * b.h).total_cmp(&(a.w * a.h)));
        panes.into_iter().find_map(|(pane, rect)| {
            let other = match auto_axis(rect) {
                Axis::Row => Axis::Column,
                Axis::Column => Axis::Row,
            };
            [auto_axis(rect), other]
                .into_iter()
                .find(|&axis| fits(rect, axis, Side::After, new, min, spacing))
                .map(|axis| (Some(pane), axis))
        })
    }

    /// Whether every pane laid out in `area` is at least `min` in size.
    pub fn fits_in(&self, area: Rect, min: (f32, f32), spacing: f32) -> bool {
        self.rects(area, spacing).iter().all(|(_, rect)| rect.w >= min.0 && rect.h >= min.1)
    }

    /// The pane that takes the space of `pane` once it closes: the nearest one beside it.
    pub fn neighbour(&self, pane: P) -> Option<P> {
        let Split::Pair { first, second, .. } = self else { return None };
        if **first == Split::Pane(pane) {
            Some(second.edge(false))
        } else if **second == Split::Pane(pane) {
            Some(first.edge(true))
        } else {
            first.neighbour(pane).or_else(|| second.neighbour(pane))
        }
    }

    fn leaf_mut(&mut self, pane: P) -> Option<&mut Self> {
        if *self == Split::Pane(pane) {
            return Some(self);
        }
        match self {
            Split::Pane(_) => None,
            Split::Pair { first, second, .. } => {
                first.leaf_mut(pane).or_else(|| second.leaf_mut(pane))
            },
        }
    }

    /// The first pane, or the `last`.
    fn edge(&self, last: bool) -> P {
        match self {
            Split::Pane(pane) => *pane,
            Split::Pair { first, second, .. } => if last { second } else { first }.edge(last),
        }
    }

    /// The pair `index` counts down to, in the order of the tree.
    fn pair_mut(&mut self, index: &mut usize) -> Option<&mut Self> {
        if let Split::Pane(_) = self {
            return None;
        }
        if *index == 0 {
            return Some(self);
        }
        *index -= 1;
        let Split::Pair { first, second, .. } = self else { return None };
        match first.pair_mut(index) {
            Some(pair) => Some(pair),
            None => second.pair_mut(index),
        }
    }

    fn tile(&self, area: Rect, spacing: f32, rects: &mut Vec<(P, Rect)>) {
        self.walk(area, spacing, &mut |split, area| {
            if let Split::Pane(pane) = split {
                rects.push((*pane, area));
            }
        });
    }

    /// Visit this and every part below it in the order of the tree, with where it is laid out.
    fn walk(&self, area: Rect, spacing: f32, visit: &mut impl FnMut(&Self, Rect)) {
        visit(self, area);
        if let Split::Pair { axis, ratio, first, second } = self {
            let (a, b) = halves(area, *axis, *ratio, spacing);
            first.walk(a, spacing, visit);
            second.walk(b, spacing, visit);
        }
    }
}

/// Where the space between the two parts of a pair is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divider {
    /// The pair's place in the order of the tree.
    pub index: usize,
    pub axis: Axis,
    /// Where the pair is laid out.
    pub area: Rect,
    /// The middle of the space between its parts: an x for a row, a y for a column.
    pub at: f32,
}

/// `area` cut in two `spacing` apart along `axis`, the first part taking `ratio` of the rest.
pub fn halves(area: Rect, axis: Axis, ratio: f32, spacing: f32) -> (Rect, Rect) {
    match axis {
        Axis::Row => {
            let w = ((area.w - spacing) * ratio).round();
            (
                Rect::new(area.x, area.y, w, area.h),
                Rect::new(area.x + w + spacing, area.y, area.w - w - spacing, area.h),
            )
        },
        Axis::Column => {
            let h = ((area.h - spacing) * ratio).round();
            (
                Rect::new(area.x, area.y, area.w, h),
                Rect::new(area.x, area.y + h + spacing, area.w, area.h - h - spacing),
            )
        },
    }
}

/// How to split a pane in two when nothing says which way: along its longer side.
pub fn auto_axis(area: Rect) -> Axis {
    if area.w >= area.h { Axis::Row } else { Axis::Column }
}

/// Where a pane dropped at `point` on `pane` goes: beside the edge nearest to it, measured
/// relative to the pane's size, so the pane's diagonals divide it into the four choices.
pub fn drop_zone(pane: Rect, (x, y): (f32, f32)) -> (Axis, Side) {
    let (cx, cy) = pane.center();
    let (dx, dy) = ((x - cx) / (pane.w / 2.), (y - cy) / (pane.h / 2.));
    let side = |d: f32| if d < 0. { Side::Before } else { Side::After };
    if dx.abs() >= dy.abs() { (Axis::Row, side(dx)) } else { (Axis::Column, side(dy)) }
}

/// Whether `new` can go on `side` of `pane` along `axis` with every pane at least `min` pixels
/// in size, those of `new` included, `spacing` apart.
pub fn fits<P: Copy + PartialEq>(
    pane: Rect,
    axis: Axis,
    side: Side,
    new: &Split<P>,
    min: (f32, f32),
    spacing: f32,
) -> bool {
    let (a, b) = halves(pane, axis, 0.5, spacing);
    let (old, area) = match side {
        Side::Before => (b, a),
        Side::After => (a, b),
    };
    old.w >= min.0 && old.h >= min.1 && new.fits_in(area, min, spacing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(axis: Axis, first: Split<u32>, second: Split<u32>) -> Split<u32> {
        first.join(second, axis, Side::After)
    }

    /// ┌───┬───┐
    /// │ 1 │ 2 │
    /// │   ├───┤
    /// │   │ 3 │
    /// └───┴───┘
    fn three() -> Split<u32> {
        pair(Axis::Row, Split::Pane(1), pair(Axis::Column, Split::Pane(2), Split::Pane(3)))
    }

    #[test]
    fn trees_map_and_round_trip() {
        let mut split = three();
        split.set_ratio(0, 0.62);
        let mut seen = Vec::new();
        let mapped = split.clone().map(&mut |pane| {
            seen.push(pane);
            u64::from(pane) * 10
        });
        assert_eq!((seen, mapped.panes()), (vec![1, 2, 3], vec![10, 20, 30]));
        assert_eq!(mapped.dividers(Rect::new(0., 0., 100., 100.), 0.)[0].at, 62.);
    }

    #[test]
    fn panes_in_reading_order() {
        assert_eq!(three().panes(), [1, 2, 3]);
        let mut split = Split::Pane(1);
        assert!(split.insert(1, Split::Pane(2), Axis::Row, Side::Before));
        assert!(split.insert(1, Split::Pane(3), Axis::Column, Side::After));
        assert_eq!(split.panes(), [2, 1, 3]);
    }

    #[test]
    fn insert_beside_target() {
        let mut split = three();
        assert!(split.insert(3, Split::Pane(4), Axis::Row, Side::After));
        assert_eq!(split.panes(), [1, 2, 3, 4]);
        let rects = split.rects(Rect::new(0., 0., 400., 200.), 0.);
        assert_eq!(rects[2], (3, Rect::new(200., 100., 100., 100.)));
        assert_eq!(rects[3], (4, Rect::new(300., 100., 100., 100.)));
        assert!(!split.insert(9, Split::Pane(5), Axis::Row, Side::After));
    }

    #[test]
    fn remove_gives_space_to_sibling() {
        let split = three().remove(2).unwrap();
        assert_eq!(split, pair(Axis::Row, Split::Pane(1), Split::Pane(3)));
        let split = split.remove(1).unwrap();
        assert_eq!(split, Split::Pane(3));
        assert_eq!(split.remove(3), None);
        assert_eq!(three().remove(7), Some(three()));
    }

    #[test]
    fn neighbour_is_nearest_pane_beside() {
        let split = three();
        assert_eq!(split.neighbour(1), Some(2));
        assert_eq!(split.neighbour(2), Some(3));
        assert_eq!(split.neighbour(3), Some(2));
        let split = pair(Axis::Row, three(), Split::Pane(4));
        assert_eq!(split.neighbour(4), Some(3));
        assert_eq!(Split::Pane(1).neighbour(1), None);
    }

    #[test]
    fn rects_tile_area_exactly() {
        // A small linear congruential generator keeps the trees random but reproducible.
        let mut seed = 7u64;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..200 {
            let mut split = Split::Pane(0);
            let count = 1 + next(12) as u32;
            for id in 1..count {
                let target = next(u64::from(id)) as u32;
                let axis = if next(2) == 0 { Axis::Row } else { Axis::Column };
                let side = if next(2) == 0 { Side::Before } else { Side::After };
                assert!(split.insert(target, Split::Pane(id), axis, side));
            }
            let area = Rect::new(3., 41., 1000. + next(400) as f32, 500. + next(300) as f32);
            let rects = split.rects(area, 0.);
            assert_eq!(rects.len(), count as usize);

            let mut covered = 0.;
            for (i, (_, a)) in rects.iter().enumerate() {
                assert!(a.w >= 0. && a.h >= 0.);
                assert_eq!((a.x.fract(), a.y.fract(), a.w.fract(), a.h.fract()), (0., 0., 0., 0.));
                assert!(a.x >= area.x && a.y >= area.y);
                assert!(a.right() <= area.right() && a.bottom() <= area.bottom());
                for (_, b) in &rects[i + 1..] {
                    let overlap =
                        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                    assert!(!overlap, "{a:?} overlaps {b:?}");
                }
                covered += a.w * a.h;
            }
            assert_eq!(covered, area.w * area.h);
        }
    }

    #[test]
    fn drop_zone_follows_diagonals() {
        let wide = Rect::new(0., 0., 400., 100.);
        assert_eq!(drop_zone(wide, (10., 50.)), (Axis::Row, Side::Before));
        assert_eq!(drop_zone(wide, (390., 50.)), (Axis::Row, Side::After));
        assert_eq!(drop_zone(wide, (200., 5.)), (Axis::Column, Side::Before));
        assert_eq!(drop_zone(wide, (200., 95.)), (Axis::Column, Side::After));
        // Relative to its size, a point 100 px right of the middle of a wide pane is nearer the
        // middle than one 30 px below it.
        assert_eq!(drop_zone(wide, (300., 85.)), (Axis::Column, Side::After));

        let tall = Rect::new(0., 0., 100., 400.);
        assert_eq!(drop_zone(tall, (5., 200.)), (Axis::Row, Side::Before));
        assert_eq!(drop_zone(tall, (50., 390.)), (Axis::Column, Side::After));
        assert_eq!(drop_zone(tall, (90., 120.)), (Axis::Row, Side::After));
        // On a diagonal the sides win.
        assert_eq!(drop_zone(tall, (100., 400.)), (Axis::Row, Side::After));
        assert_eq!(drop_zone(tall, (0., 0.)), (Axis::Row, Side::Before));
    }

    #[test]
    fn auto_axis_splits_longer_side() {
        assert_eq!(auto_axis(Rect::new(0., 0., 800., 600.)), Axis::Row);
        assert_eq!(auto_axis(Rect::new(0., 0., 600., 600.)), Axis::Row);
        assert_eq!(auto_axis(Rect::new(0., 0., 500., 600.)), Axis::Column);
    }

    #[test]
    fn fits_refuses_small_halves() {
        let pane = Rect::new(0., 0., 400., 200.);
        let new = Split::Pane(9);
        assert!(fits(pane, Axis::Row, Side::After, &new, (200., 200.), 0.));
        assert!(!fits(pane, Axis::Row, Side::After, &new, (201., 10.), 0.));
        assert!(fits(pane, Axis::Column, Side::Before, &new, (400., 100.), 0.));
        assert!(!fits(pane, Axis::Column, Side::Before, &new, (10., 101.), 0.));
    }

    #[test]
    fn fits_refuses_small_panes_of_a_group() {
        let pane = Rect::new(0., 0., 400., 200.);
        let group = pair(Axis::Row, Split::Pane(8), Split::Pane(9));
        // Each half has room for one pane, but not for the group's two side by side.
        assert!(!fits(pane, Axis::Row, Side::After, &group, (150., 50.), 0.));
        assert!(fits(pane, Axis::Row, Side::After, &group, (100., 50.), 0.));
        // Docked above, its panes share the half's whole width.
        assert!(fits(pane, Axis::Column, Side::Before, &group, (150., 50.), 0.));
        assert!(!fits(pane, Axis::Column, Side::Before, &group, (150., 101.), 0.));
    }

    #[test]
    fn fits_with_spacing() {
        let pane = Rect::new(0., 0., 400., 200.);
        let new = Split::Pane(9);
        // 20 px between the halves leaves each 190.
        assert!(fits(pane, Axis::Row, Side::After, &new, (190., 10.), 20.));
        assert!(!fits(pane, Axis::Row, Side::After, &new, (191., 10.), 20.));
        // Overlapping halves are larger.
        assert!(fits(pane, Axis::Row, Side::After, &new, (205., 10.), -10.));
    }

    const AREA: Rect = Rect { x: 10., y: 40., w: 810., h: 600. };

    #[test]
    fn rects_keep_spacing_apart() {
        let split = three();
        // Without spacing they tile as before.
        let rects = split.rects(AREA, 0.);
        assert_eq!(rects[0].1, Rect::new(10., 40., 405., 600.));
        assert_eq!(rects[1].1, Rect::new(415., 40., 405., 300.));

        let rects = split.rects(AREA, 10.);
        assert_eq!(rects[0].1, Rect::new(10., 40., 400., 600.));
        assert_eq!(rects[1].1, Rect::new(420., 40., 400., 295.));
        assert_eq!(rects[2].1, Rect::new(420., 345., 400., 295.));
        // Halves and the space between them make the whole length.
        assert_eq!(rects[0].1.w + 10. + rects[1].1.w, AREA.w);
        assert_eq!(rects[1].1.h + 10. + rects[2].1.h, AREA.h);
        assert!(rects[0].1.right() < rects[1].1.x);

        // Negative spacing overlaps neighbours by as much.
        let rects = split.rects(AREA, -16.);
        assert_eq!(rects[0].1.right() - rects[1].1.x, 16.);
        assert_eq!(rects[1].1.bottom() - rects[2].1.y, 16.);
        assert_eq!(rects[2].1.right(), AREA.right());
    }

    #[test]
    fn dividers_sit_in_the_middle_of_gaps() {
        let split = three();
        assert!(Split::Pane(1).dividers(AREA, 10.).is_empty());
        let dividers = split.dividers(AREA, 10.);
        assert_eq!(dividers.len(), 2);
        assert_eq!(dividers[0], Divider { index: 0, axis: Axis::Row, area: AREA, at: 415. });
        let right = Rect::new(420., 40., 400., 600.);
        assert_eq!(dividers[1], Divider { index: 1, axis: Axis::Column, area: right, at: 340. });
        // Without spacing, on the edge the parts share.
        assert_eq!(split.dividers(AREA, 0.)[0].at, split.rects(AREA, 0.)[1].1.x);
    }

    #[test]
    fn set_ratio_by_order_of_the_tree() {
        let mut split = pair(Axis::Row, three(), Split::Pane(4));
        assert!(split.set_ratio(2, 0.25));
        assert!(split.set_ratio(0, 0.75));
        assert!(!split.set_ratio(3, 0.5));
        let rects = split.rects(Rect::new(0., 0., 800., 400.), 0.);
        assert_eq!(rects[2].1, Rect::new(300., 100., 300., 300.));
        assert_eq!(rects[3].1, Rect::new(600., 0., 200., 400.));
        // Ratios survive removing a pane elsewhere.
        let split = split.remove(4).unwrap();
        assert_eq!(split.rects(Rect::new(0., 0., 800., 400.), 0.)[1].1.h, 100.);
    }

    #[test]
    fn min_size_adds_up_parts() {
        let min = (100., 50.);
        assert_eq!(Split::Pane(1).min_size(min, 10.), min);
        // 1 beside the column of 2 and 3.
        assert_eq!(three().min_size(min, 10.), (210., 110.));
        assert_eq!(three().min_size(min, -16.), (184., 84.));
    }

    #[test]
    fn ratio_range_honours_nested_minimums() {
        let min = (100., 50.);
        let area = Rect::new(0., 0., 410., 200.);
        // The first part needs 100 of 400, the second (a column) 100 too.
        assert_eq!(three().ratio_range(0, area, min, 10.), Some((0.25, 0.75)));
        // A row of two on the right needs 210 of the 400.
        let split =
            pair(Axis::Row, Split::Pane(1), pair(Axis::Row, Split::Pane(2), Split::Pane(3)));
        let (lo, hi) = split.ratio_range(0, area, min, 10.).unwrap();
        assert_eq!((lo, hi), (0.25, 1. - 210. / 400.));
        let mut split = split;
        for ratio in [lo, hi] {
            split.set_ratio(0, ratio);
            assert!(split.fits_in(area, min, 10.), "{ratio}");
        }
        // The nested pair, laid out in the second part, has no room left to move.
        split.set_ratio(0, hi);
        assert_eq!(split.ratio_range(1, area, min, 10.), Some((0.5, 0.5)));
        assert_eq!(split.ratio_range(2, area, min, 10.), None);
        // A pair keeps its ratio as its parent moves, so its parts need room in that ratio.
        let mut split =
            pair(Axis::Row, Split::Pane(1), pair(Axis::Row, Split::Pane(2), Split::Pane(3)));
        split.set_ratio(1, 0.3);
        let area = Rect::new(0., 0., 1010., 200.);
        let (_, hi) = split.ratio_range(0, area, min, 10.).unwrap();
        split.set_ratio(0, hi);
        assert!(split.fits_in(area, min, 10.), "{:?}", split.rects(area, 10.));
        // Too small for both: the middle.
        let small = Rect::new(0., 0., 150., 200.);
        assert_eq!(three().ratio_range(0, small, min, 10.), Some((0.5, 0.5)));
    }

    #[test]
    fn fits_in_checks_every_pane() {
        let min = (100., 50.);
        assert!(three().fits_in(Rect::new(0., 0., 210., 110.), min, 10.));
        assert!(!three().fits_in(Rect::new(0., 0., 209., 110.), min, 10.));
        assert!(!three().fits_in(Rect::new(0., 0., 210., 109.), min, 10.));
        assert!(three().fits_in(Rect::new(0., 0., 200., 100.), min, 0.));
    }

    #[test]
    fn spot_halves_the_group_or_shares_its_roomiest_pane() {
        let (min, new) = ((100., 50.), Split::Pane(9));
        // Room enough: beside the whole group, along the area's longer side.
        let roomy = Rect::new(0., 0., 800., 300.);
        assert_eq!(three().spot(&new, roomy, min, 0.), Some((None, Axis::Row)));
        // Four side by side, each 100 px once halved, which is too little, but pane 1 of the four
        // can share its own 200 px.
        let four = pair(
            Axis::Row,
            pair(Axis::Row, Split::Pane(1), Split::Pane(2)),
            pair(Axis::Row, Split::Pane(3), Split::Pane(4)),
        );
        let min = (150., 50.);
        assert_eq!(four.spot(&new, roomy, min, 0.), Some((Some(1), Axis::Column)));
        // The roomiest pane goes first, the other way when its longer side leaves too little.
        let area = Rect::new(0., 0., 1000., 300.);
        let wide = (300., 50.);
        assert_eq!(three().spot(&new, area, wide, 0.), Some((Some(1), Axis::Column)));
        // Nowhere to go.
        assert_eq!(four.spot(&new, Rect::new(0., 0., 400., 100.), min, 0.), None);
    }
}
