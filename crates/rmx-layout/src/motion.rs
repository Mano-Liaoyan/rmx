//! Where panes are drawn as they move: only the drawing moves, their content is resized once.

use std::mem;
use std::time::{Duration, Instant};

use crate::anim::{Curve, Tween, scale};
use crate::{PaneId, Rect};

/// How long panes take to slide to their places, or to pop in, in milliseconds.
const MOVE_MS: u64 = 260;
/// How long a focus border takes to light up or fade.
const LIGHT_MS: u64 = 180;
/// How large a new pane starts, from its place.
const POP: f32 = 0.85;

struct Entry {
    id: PaneId,
    frame: Tween<Rect>,
    light: Tween<f32>,
}

/// The motion of the panes on screen.
#[derive(Default)]
pub struct Motion {
    panes: Vec<Entry>,
    /// Panes no longer laid out, drawn under the others until `Instant`.
    leaving: Vec<(Entry, Instant)>,
    /// What the panes tiled when they were last placed.
    area: Option<Rect>,
}

fn popped(rect: Rect) -> Rect {
    let (w, h) = (rect.w * POP, rect.h * POP);
    Rect::new(rect.x + (rect.w - w) / 2., rect.y + (rect.h - h) / 2., w, h)
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y)
}

impl Motion {
    /// The panes are to be at `rects` from now on, sliding there when `animate` is set.
    ///
    /// A change of the area they tile, like a resize of the window, never animates.
    pub fn place(&mut self, rects: &[(PaneId, Rect)], now: Instant, animate: bool) {
        let area = rects.iter().map(|&(_, rect)| rect).reduce(union);
        let shown = rects.iter().any(|&(id, _)| self.entry(id).is_some());
        // The last pane going has nothing to slide over, but it leaves like any other.
        let emptied = rects.is_empty() && !self.panes.is_empty();
        let animate = animate && (emptied || (shown && area == self.area));
        self.area = area;

        // Panes still leaving keep the moment they are covered; covered ones are gone.
        let leaving = mem::take(&mut self.leaving).into_iter().filter(|&(_, until)| until > now);
        let mut old: Vec<_> = mem::take(&mut self.panes).into_iter().map(|e| (e, None)).collect();
        old.extend(leaving.map(|(entry, until)| (entry, Some(until))));

        self.panes = rects
            .iter()
            .map(|&(id, rect)| {
                let found = old.iter().position(|(entry, _)| entry.id == id);
                let mut entry = found.map(|i| old.remove(i).0).unwrap_or_else(|| Entry {
                    id,
                    frame: Tween::new(if animate { popped(rect) } else { rect }),
                    light: Tween::new(0.),
                });
                if animate {
                    entry.frame.to(rect, now, MOVE_MS, Curve::SNAP);
                } else {
                    entry.frame = Tween::new(rect);
                }
                entry
            })
            .collect();

        if animate {
            if emptied {
                for (entry, _) in &mut old {
                    entry.frame.to(popped(entry.frame.get(now)), now, MOVE_MS, Curve::SNAP);
                }
            }
            let until = now + Duration::from_millis(MOVE_MS).mul_f32(scale());
            let old = old.into_iter().map(|(entry, since)| (entry, since.unwrap_or(until)));
            self.leaving = old.filter(|&(_, until)| until > now).collect();
        }
    }

    /// Light up the border of the pane `focused`, and fade the others.
    pub fn focus(&mut self, focused: PaneId, now: Instant) {
        for entry in &mut self.panes {
            let target = if entry.id == focused { 1. } else { 0. };
            entry.light.to(target, now, LIGHT_MS, Curve::EASE_OUT);
        }
    }

    /// Where to draw the pane `id` at `now`.
    pub fn frame(&self, id: PaneId, now: Instant) -> Option<Rect> {
        self.entry(id).map(|entry| entry.frame.get(now))
    }

    /// How lit the border of the pane `id` is at `now`.
    pub fn light(&self, id: PaneId, now: Instant) -> f32 {
        self.entry(id).map_or(0., |entry| entry.light.get(now))
    }

    /// Panes no longer laid out but still to be drawn at `now`, under the others, and where.
    pub fn leaving(&self, now: Instant) -> impl Iterator<Item = (PaneId, Rect)> + '_ {
        let leaving = self.leaving.iter().filter(move |&&(_, until)| until > now);
        leaving.map(move |(entry, _)| (entry.id, entry.frame.get(now)))
    }

    /// Whether anything is still moving at `now`.
    pub fn running(&self, now: Instant) -> bool {
        self.leaving.iter().any(|&(_, until)| until > now)
            || self.panes.iter().any(|e| e.frame.running(now) || e.light.running(now))
    }

    fn entry(&self, id: PaneId) -> Option<&Entry> {
        self.panes.iter().find(|entry| entry.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: Rect = Rect { x: 0., y: 0., w: 100., h: 100. };
    const RIGHT: Rect = Rect { x: 100., y: 0., w: 100., h: 100. };
    const WHOLE: Rect = Rect { x: 0., y: 0., w: 200., h: 100. };

    #[test]
    fn first_placement_does_not_animate() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.place(&[(1, WHOLE)], now, true);
        assert_eq!(motion.frame(1, now), Some(WHOLE));
        assert!(!motion.running(now));
    }

    #[test]
    fn a_split_slides_the_old_pane_and_pops_the_new_one() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.place(&[(1, WHOLE)], now, true);
        motion.place(&[(1, LEFT), (2, RIGHT)], now, true);

        assert_eq!(motion.frame(1, now), Some(WHOLE));
        assert_eq!(motion.frame(2, now), Some(popped(RIGHT)));
        assert!(motion.running(now));

        let later = now + Duration::from_millis(MOVE_MS + 1);
        assert_eq!(motion.frame(1, later), Some(LEFT));
        assert_eq!(motion.frame(2, later), Some(RIGHT));
        assert!(!motion.running(later));
    }

    #[test]
    fn the_last_pane_leaves_like_any_other_and_then_nothing_runs() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.place(&[(1, WHOLE)], now, false);
        motion.place(&[], now, true);

        assert_eq!(motion.frame(1, now), None);
        assert_eq!(motion.leaving(now).map(|(id, _)| id).collect::<Vec<_>>(), [1]);
        assert!(motion.running(now));
        let soon = now + Duration::from_millis(MOVE_MS / 2);
        let (_, frame) = motion.leaving(soon).next().unwrap();
        assert!(frame.w < WHOLE.w, "it shrinks as it goes");

        let later = now + Duration::from_millis(MOVE_MS + 1);
        assert_eq!(motion.leaving(later).count(), 0);
        assert!(!motion.running(later));
    }

    #[test]
    fn nothing_to_place_does_not_animate() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.place(&[], now, true);
        assert!(!motion.running(now));
    }

    #[test]
    fn a_resized_area_moves_panes_at_once() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.place(&[(1, LEFT)], now, true);
        motion.place(&[(1, WHOLE)], now, true);
        assert_eq!(motion.frame(1, now), Some(WHOLE));
    }

    #[test]
    fn a_pane_zoomed_over_others_leaves_them_drawn_until_covered() {
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.place(&[(1, LEFT), (2, RIGHT)], now, true);
        motion.place(&[(1, WHOLE)], now, false);
        motion.place(&[(1, LEFT), (2, RIGHT)], now, true);
        motion.place(&[(1, WHOLE)], now, true);
        assert_eq!(motion.leaving(now).map(|(id, _)| id).collect::<Vec<_>>(), [2]);

        let later = now + Duration::from_millis(MOVE_MS + 1);
        assert_eq!(motion.leaving(later).count(), 0);
    }
}
