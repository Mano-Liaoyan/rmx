//! Where the main thread's time goes within a frame, for `xtask bench`.
//!
//! [`enter`] times a stretch of the main thread, by the wall clock and by the thread's CPU clock:
//! the difference is time the thread wasn't running. Without the `bench` feature everything here
//! is empty and compiles to nothing.

/// A stretch of a frame. The ones marked as within [`Phase::Cells`] are counted in it as well.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Phase {
    /// Running egui and doing what came of it.
    Chrome,
    /// Asking how old the back buffer is.
    BufferAge,
    /// Clearing the window and each pane.
    Clear,
    /// Taking a terminal's snapshot.
    Snapshot,
    /// Drawing a terminal's cells.
    Cells,
    /// Within [`Phase::Cells`]: rasterizing glyphs, and putting them in the atlas.
    Raster,
    /// Within [`Phase::Cells`]: writing instances to the buffer the GPU reads.
    Upload,
    /// Within [`Phase::Cells`]: taking fresh storage for that buffer.
    Orphan,
    /// Within [`Phase::Cells`]: the draw calls.
    Draw,
    /// Drawing a terminal's lines and cursor.
    Rects,
    /// Handing the GPU what was drawn so far.
    Flush,
    /// Drawing a remote desktop.
    Display,
    /// Replaying the interface.
    ChromePaint,
    /// Telling the windowing system a frame is coming.
    PrePresent,
    /// Swapping buffers.
    Swap,
    /// What follows presenting: the input method, uploads of remote desktops.
    Post,
}

/// How many kinds of [`Phase`] there are.
pub const PHASES: usize = Phase::Post as usize + 1;

/// The name of each [`Phase`], in the report.
pub const PHASE_NAMES: [&str; PHASES] = [
    "chrome", "age", "clear", "snapshot", "cells", "raster", "upload", "orphan", "draw", "rects",
    "flush", "display", "paint", "notify", "swap", "post",
];

/// Time spent in a phase, in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    pub wall: u32,
    pub cpu: u32,
}

#[cfg(feature = "bench")]
pub use on::{Scope, enter, take, thread_cpu_us};

#[cfg(not(feature = "bench"))]
pub use off::{Scope, enter};

#[cfg(not(feature = "bench"))]
mod off {
    use super::Phase;

    /// Does nothing, in a build that doesn't measure.
    pub struct Scope;

    #[inline(always)]
    pub fn enter(_: Phase) -> Scope {
        Scope
    }
}

#[cfg(feature = "bench")]
mod on {
    use std::cell::Cell;
    use std::time::Instant;

    use super::{PHASES, Phase, Spent};

    thread_local! {
        static SPENT: [Cell<Spent>; PHASES] = const { [const { Cell::new(Spent { wall: 0, cpu: 0 }) }; PHASES] };
    }

    /// CPU time this thread has used so far, in microseconds.
    pub fn thread_cpu_us() -> u64 {
        #[repr(C)]
        struct Timespec {
            seconds: i64,
            nanos: i64,
        }
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        unsafe extern "C" {
            fn clock_gettime(clock: i32, time: *mut Timespec) -> i32;
        }

        let mut time = Timespec { seconds: 0, nanos: 0 };
        unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut time) };
        time.seconds as u64 * 1_000_000 + time.nanos as u64 / 1_000
    }

    /// A phase in progress, which ends when this is dropped.
    pub struct Scope {
        phase: Phase,
        wall: Instant,
        cpu: u64,
    }

    pub fn enter(phase: Phase) -> Scope {
        Scope { phase, wall: Instant::now(), cpu: thread_cpu_us() }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            let wall = self.wall.elapsed().as_micros() as u32;
            let cpu = (thread_cpu_us() - self.cpu) as u32;
            SPENT.with(|spent| {
                let cell = &spent[self.phase as usize];
                let before = cell.get();
                cell.set(Spent { wall: before.wall + wall, cpu: before.cpu + cpu });
            });
        }
    }

    /// What this thread spent in each phase since this was last called.
    pub fn take() -> [Spent; PHASES] {
        SPENT.with(|spent| std::array::from_fn(|phase| spent[phase].take()))
    }
}
