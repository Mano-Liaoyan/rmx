//! When the next frame may be drawn.

use std::time::{Duration, Instant};

/// Paces frames to the display, drawing only what is dirty and never spinning when idle.
///
/// Wayland tells us when to draw through frame callbacks, which winit delivers as a redraw request
/// made after presenting. Elsewhere frames are spaced by the monitor's refresh interval.
#[derive(Debug)]
pub struct Pacer {
    /// Whether a frame may be drawn now.
    has_frame: bool,
    /// Whether there is anything new to show.
    dirty: bool,
    /// When the next frame may be drawn, off Wayland.
    deadline: Option<Instant>,
    /// Time between frames, off Wayland.
    refresh: Duration,
    wayland: bool,
    /// When there first was something new to show since the last frame, on the presentation clock.
    #[cfg(feature = "bench")]
    pending_since: Option<u64>,
}

impl Pacer {
    pub fn new(wayland: bool, refresh: Duration) -> Self {
        Self {
            has_frame: true,
            dirty: true,
            deadline: None,
            refresh,
            wayland,
            #[cfg(feature = "bench")]
            pending_since: None,
        }
    }

    pub fn set_refresh(&mut self, refresh: Duration) {
        self.refresh = refresh;
    }

    /// There is something new to show. Returns whether to request a redraw right away.
    pub fn mark_dirty(&mut self) -> bool {
        #[cfg(feature = "bench")]
        if !self.dirty {
            self.pending_since = Some(crate::bench::present::now_ns());
        }
        self.dirty = true;
        self.has_frame
    }

    /// A redraw request arrived. Returns whether to draw.
    pub fn on_redraw(&mut self) -> bool {
        if self.wayland {
            self.has_frame = true;
        }
        self.has_frame && self.dirty
    }

    /// A redraw found nothing to show after all. The frame stays available.
    pub fn on_nothing_to_draw(&mut self) {
        self.dirty = false;
        #[cfg(feature = "bench")]
        {
            self.pending_since = None;
        }
    }

    /// A frame was presented at `now`. Returns whether to request a redraw, for its frame callback.
    pub fn on_presented(&mut self, now: Instant) -> bool {
        self.dirty = false;
        #[cfg(feature = "bench")]
        {
            self.pending_since = None;
        }
        self.has_frame = false;
        if self.wayland {
            true
        } else {
            self.deadline = Some(now + self.refresh);
            false
        }
    }

    /// When the frame being drawn first had something to show.
    #[cfg(feature = "bench")]
    pub fn pending_since(&self) -> Option<u64> {
        self.pending_since
    }

    /// The event loop is about to sleep at `now`.
    ///
    /// Returns whether to request a redraw now, and when to wake up otherwise.
    pub fn before_wait(&mut self, now: Instant) -> (bool, Option<Instant>) {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                self.has_frame = true;
                (self.dirty, None)
            },
            Some(deadline) => (false, Some(deadline)),
            None => (false, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFRESH: Duration = Duration::from_millis(4);

    #[test]
    fn idle_after_presenting_on_wayland() {
        let now = Instant::now();
        let mut pacer = Pacer::new(true, REFRESH);
        assert!(pacer.on_redraw());
        assert!(pacer.on_presented(now));

        // The frame callback arrives with nothing new: nothing is drawn or requested again.
        assert!(!pacer.on_redraw());
        assert_eq!(pacer.before_wait(now), (false, None));
    }

    #[test]
    fn dirty_while_waiting_for_a_frame_callback_draws_on_it() {
        let now = Instant::now();
        let mut pacer = Pacer::new(true, REFRESH);
        pacer.on_redraw();
        pacer.on_presented(now);

        assert!(!pacer.mark_dirty());
        assert!(pacer.on_redraw());
    }

    #[test]
    fn many_wakeups_in_one_frame_draw_once() {
        let now = Instant::now();
        let mut pacer = Pacer::new(false, REFRESH);
        pacer.on_redraw();
        pacer.on_presented(now);

        for _ in 0..100 {
            assert!(!pacer.mark_dirty());
        }
        assert_eq!(pacer.before_wait(now), (false, Some(now + REFRESH)));
        assert_eq!(pacer.before_wait(now + REFRESH), (true, None));
    }

    #[test]
    fn timer_with_nothing_new_goes_idle() {
        let now = Instant::now();
        let mut pacer = Pacer::new(false, REFRESH);
        pacer.on_redraw();
        pacer.on_presented(now);

        assert_eq!(pacer.before_wait(now + REFRESH), (false, None));
        assert_eq!(pacer.before_wait(now + REFRESH * 2), (false, None));
    }
}
