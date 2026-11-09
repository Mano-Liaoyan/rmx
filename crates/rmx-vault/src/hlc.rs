//! Hybrid logical clocks: a total order on writes that survives device clock skew.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Error;

/// One point in the hybrid logical clock.
///
/// Ordered by wall time, then counter, then node. The text form is fixed-width lowercase hex, so
/// SQLite can compare HLCs as plain strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc {
    /// Milliseconds since the Unix epoch, possibly borrowed from a faster device.
    pub wall_ms: u64,
    /// Tie-breaker among events within the same millisecond.
    pub counter: u32,
    /// The device that issued this value.
    pub node: u64,
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}-{:08x}-{:016x}", self.wall_ms, self.counter, self.node)
    }
}

impl FromStr for Hlc {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        fn field<T>(
            s: &str,
            parse: fn(&str, u32) -> Result<T, std::num::ParseIntError>,
        ) -> Result<T, Error> {
            parse(s, 16).map_err(|_| Error::InvalidHlc)
        }
        let bytes = s.as_bytes();
        if bytes.len() != 42 || bytes[16] != b'-' || bytes[25] != b'-' {
            return Err(Error::InvalidHlc);
        }
        Ok(Self {
            wall_ms: field(&s[..16], u64::from_str_radix)?,
            counter: field(&s[17..25], u32::from_str_radix)?,
            node: field(&s[26..], u64::from_str_radix)?,
        })
    }
}

type TimeSource = Box<dyn Fn() -> u64 + Send>;

/// Issues strictly increasing [`Hlc`] values for one device.
///
/// After [`observe`](Self::observe)-ing a value, every later [`now`](Self::now) sorts above it,
/// however far behind this device's wall clock is.
pub struct HlcClock {
    node: u64,
    wall_ms: u64,
    counter: u32,
    time: TimeSource,
}

impl HlcClock {
    /// A clock for `node` reading the system time.
    pub fn new(node: u64) -> Self {
        Self::with_time_source(node, system_time_ms)
    }

    /// A clock for `node` reading milliseconds from `time`.
    pub fn with_time_source(node: u64, time: impl Fn() -> u64 + Send + 'static) -> Self {
        Self { node, wall_ms: 0, counter: 0, time: Box::new(time) }
    }

    /// Replaces the wall-clock source, keeping everything already issued or observed.
    pub fn set_time_source(&mut self, time: impl Fn() -> u64 + Send + 'static) {
        self.time = Box::new(time);
    }

    /// Issues the next value.
    pub fn now(&mut self) -> Hlc {
        let physical = (self.time)();
        if physical > self.wall_ms {
            self.wall_ms = physical;
            self.counter = 0;
        } else if let Some(next) = self.counter.checked_add(1) {
            self.counter = next;
        } else {
            self.wall_ms += 1;
            self.counter = 0;
        }
        Hlc { wall_ms: self.wall_ms, counter: self.counter, node: self.node }
    }

    /// Records a value seen elsewhere so later writes sort after it.
    pub fn observe(&mut self, seen: Hlc) {
        if (seen.wall_ms, seen.counter) > (self.wall_ms, self.counter) {
            self.wall_ms = seen.wall_ms;
            self.counter = seen.counter;
        }
    }
}

impl fmt::Debug for HlcClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlcClock")
            .field("node", &self.node)
            .field("wall_ms", &self.wall_ms)
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

pub(crate) fn system_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_sorts_like_the_value() {
        let values = [
            Hlc { wall_ms: 1, counter: 9, node: u64::MAX },
            Hlc { wall_ms: 2, counter: 0, node: 0 },
            Hlc { wall_ms: 2, counter: 1, node: 0 },
            Hlc { wall_ms: 0x1_0000, counter: 0, node: 0 },
        ];
        for pair in values.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(pair[0].to_string() < pair[1].to_string());
        }
        for value in values {
            assert_eq!(value.to_string().parse::<Hlc>().unwrap(), value);
        }
        assert!("nonsense".parse::<Hlc>().is_err());
    }

    #[test]
    fn stalled_clock_still_increases() {
        let mut clock = HlcClock::with_time_source(1, || 1000);
        let (a, b, c) = (clock.now(), clock.now(), clock.now());
        assert!(a < b && b < c);
    }

    #[test]
    fn write_after_observing_a_fast_device_wins_despite_a_slow_clock() {
        let mut fast = HlcClock::with_time_source(u64::MAX, || 10_000_000);
        let mut slow = HlcClock::with_time_source(0, || 1_000);
        let remote = fast.now();
        assert!(slow.now() < remote, "before observing, the slow clock is simply behind");
        slow.observe(remote);
        let reply = slow.now();
        assert!(reply > remote, "causally later write must win even from node 0");
        assert_eq!(reply.wall_ms, remote.wall_ms);
    }

    #[test]
    fn observing_the_past_changes_nothing() {
        let mut clock = HlcClock::with_time_source(5, || 2000);
        let before = clock.now();
        clock.observe(Hlc { wall_ms: 1, counter: 7, node: 3 });
        let after = clock.now();
        assert_eq!((after.wall_ms, after.counter), (before.wall_ms, before.counter + 1));
    }

    #[test]
    fn counter_overflow_advances_wall_time() {
        let mut clock = HlcClock::with_time_source(1, || 5);
        clock.observe(Hlc { wall_ms: 50, counter: u32::MAX, node: 2 });
        let next = clock.now();
        assert_eq!((next.wall_ms, next.counter), (51, 0));
    }
}
