//! Values that ease towards a target over time, for the interface's animations.
//!
//! Curves are CSS-style cubic Béziers, like Hyprland's, which this follows in feel.

use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::Rect;

/// A cubic Bézier from (0, 0) to (1, 1) through the control points (x1, y1) and (x2, y2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Curve(pub f32, pub f32, pub f32, pub f32);

impl Curve {
    /// Slow to start, then quick, for what leaves.
    pub const EASE_IN: Curve = Curve(0.4, 0.0, 1.0, 1.0);
    /// Quick, slowing down to the target.
    pub const EASE_OUT: Curve = Curve(0.23, 1.0, 0.32, 1.0);
    /// Quick, slightly overshooting the target before settling, like Hyprland's windows.
    pub const SNAP: Curve = Curve(0.05, 0.9, 0.1, 1.05);

    /// Progress of the animation once `t` of its time has passed.
    pub fn at(&self, t: f32) -> f32 {
        if t <= 0. || t >= 1. {
            return t.clamp(0., 1.);
        }
        let Curve(x1, y1, x2, y2) = *self;
        let bezier = |a: f32, b: f32, s: f32| {
            let r = 1. - s;
            3. * a * s * r * r + 3. * b * s * s * r + s * s * s
        };
        let slope = |a: f32, b: f32, s: f32| {
            let r = 1. - s;
            3. * a * r * r + 6. * (b - a) * s * r + 3. * (1. - b) * s * s
        };

        // Find where the curve reaches `t` across: by Newton's method, or else by bisection.
        let mut s = t;
        for _ in 0..8 {
            let error = bezier(x1, x2, s) - t;
            let slope = slope(x1, x2, s);
            if error.abs() < 1e-6 || slope.abs() < 1e-6 {
                break;
            }
            s = (s - error / slope).clamp(0., 1.);
        }
        if (bezier(x1, x2, s) - t).abs() > 1e-4 {
            let (mut low, mut high) = (0., 1.);
            for _ in 0..32 {
                s = (low + high) / 2.;
                if bezier(x1, x2, s) < t { low = s } else { high = s }
            }
        }
        bezier(y1, y2, s)
    }
}

/// A value that can be blended with another.
pub trait Lerp: Copy + PartialEq {
    /// The value `t` of the way from this to `to`; `t` may overshoot 0 to 1.
    fn lerp(self, to: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(self, to: Self, t: f32) -> Self {
        self + (to - self) * t
    }
}

impl Lerp for Rect {
    fn lerp(self, to: Self, t: f32) -> Self {
        Rect::new(
            self.x.lerp(to.x, t),
            self.y.lerp(to.y, t),
            self.w.lerp(to.w, t),
            self.h.lerp(to.h, t),
        )
    }
}

/// A value moving to a target along a curve.
#[derive(Clone, Copy, Debug)]
pub struct Tween<T> {
    from: T,
    to: T,
    start: Instant,
    duration: Duration,
    curve: Curve,
}

impl<T: Lerp> Tween<T> {
    /// A value at rest.
    pub fn new(value: T) -> Self {
        let (start, duration) = (Instant::now(), Duration::ZERO);
        Self { from: value, to: value, start, duration, curve: Curve::EASE_OUT }
    }

    /// Move to `value` in `ms` milliseconds, from wherever the value is now.
    pub fn to(&mut self, value: T, now: Instant, ms: u64, curve: Curve) {
        if value == self.to {
            return;
        }
        self.from = self.get(now);
        self.to = value;
        self.start = now;
        self.duration = Duration::from_millis(ms).mul_f32(scale());
        self.curve = curve;
    }

    /// The value at `now`.
    pub fn get(&self, now: Instant) -> T {
        if !self.running(now) {
            return self.to;
        }
        let t =
            now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32();
        self.from.lerp(self.to, self.curve.at(t))
    }

    /// Whether the value is still moving at `now`.
    pub fn running(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) < self.duration
    }
}

/// How much slower animations run, from `RMX_ANIMATION_SCALE`: 1 by default, 0 to turn
/// them off, higher to watch them in slow motion. Tests always run at 1.
pub fn scale() -> f32 {
    if cfg!(test) {
        return 1.;
    }
    static SCALE: OnceLock<f32> = OnceLock::new();
    *SCALE.get_or_init(|| {
        let scale = env::var("RMX_ANIMATION_SCALE").ok().and_then(|s| s.parse().ok());
        scale.filter(|scale: &f32| scale.is_finite()).unwrap_or(1.).clamp(0., 100.)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVES: [Curve; 4] =
        [Curve::SNAP, Curve::EASE_OUT, Curve::EASE_IN, Curve(0.42, 0., 0.58, 1.)];

    #[test]
    fn curves_start_and_end_in_place() {
        for curve in CURVES {
            assert_eq!(curve.at(0.), 0.);
            assert_eq!(curve.at(1.), 1.);
            assert_eq!(curve.at(-1.), 0.);
            assert_eq!(curve.at(2.), 1.);
        }
    }

    #[test]
    fn linear_curve_is_identity() {
        let linear = Curve(0.25, 0.25, 0.75, 0.75);
        for i in 0..=20 {
            let t = i as f32 / 20.;
            assert!((linear.at(t) - t).abs() < 1e-4, "{t}");
        }
    }

    #[test]
    fn snap_overshoots_then_settles() {
        let values: Vec<_> = (0..=100).map(|i| Curve::SNAP.at(i as f32 / 100.)).collect();
        assert!(values.iter().any(|&value| value > 1.));
        assert!(values.iter().all(|&value| value < 1.02));
        assert_eq!(values[100], 1.);
        // Most of the way there early on.
        assert!(values[25] > 0.85);
    }

    #[test]
    fn easing_is_monotonic() {
        for curve in [Curve::EASE_OUT, Curve::EASE_IN] {
            let mut last = 0.;
            for i in 0..=100 {
                let value = curve.at(i as f32 / 100.);
                assert!(value >= last, "{curve:?} {i}");
                assert!(value <= 1.);
                last = value;
            }
        }
        // Easing out is ahead of a straight line half way, easing in behind it.
        assert!(Curve::EASE_OUT.at(0.5) > 0.5 && Curve::EASE_IN.at(0.5) < 0.5);
    }

    #[test]
    fn tween_moves_and_stops() {
        let now = Instant::now();
        let mut tween = Tween::new(0f32);
        assert!(!tween.running(now));
        assert_eq!(tween.get(now), 0.);

        tween.to(10., now, 100, Curve::EASE_OUT);
        // Scaled like the tween's, which some toolchains round up by a nanosecond.
        let duration = Duration::from_millis(100).mul_f32(scale());
        assert_eq!(tween.get(now), 0.);
        assert!(tween.running(now));
        assert!(!tween.running(now + duration));
        assert_eq!(tween.get(now + duration), 10.);
    }

    #[test]
    fn tween_without_duration_jumps() {
        // As every tween does with `RMX_ANIMATION_SCALE=0`.
        let now = Instant::now();
        let mut tween = Tween::new(0f32);
        tween.to(10., now, 0, Curve::EASE_OUT);
        assert_eq!(tween.get(now), 10.);
        assert!(!tween.running(now));
    }

    #[test]
    fn tween_retargets_from_current_value() {
        let now = Instant::now();
        let mut tween = Tween::new(Rect::new(0., 0., 100., 100.));
        tween.to(Rect::new(100., 0., 100., 100.), now, 200, Curve::SNAP);
        let middle = now + Duration::from_millis(100).mul_f32(scale());
        let before = tween.get(middle);
        tween.to(Rect::new(0., 50., 100., 100.), middle, 200, Curve::SNAP);
        assert_eq!(tween.get(middle), before);
        // Retargeting to the same value keeps the animation going as it was.
        let again = tween;
        tween.to(
            Rect::new(0., 50., 100., 100.),
            middle + Duration::from_millis(1),
            200,
            Curve::SNAP,
        );
        assert_eq!(tween.get(middle), again.get(middle));
    }
}
