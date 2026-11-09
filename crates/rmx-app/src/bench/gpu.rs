//! What the GPU spent on each frame, and when it finished, without ever waiting for it.
//!
//! A pool of timer queries and fences is handed out frame by frame and read back when ready, so
//! that nothing blocks: a frame that finds the pool empty goes unmeasured and is counted.
//! `RMX_BENCH_GPU=off` turns this off, to compare a run with and without it.

use std::collections::VecDeque;

use rmx_render::gl;
use rmx_render::gl::types::{GLint, GLint64, GLsync, GLuint, GLuint64};

use super::present::{Record, now_ns};

const POOL: usize = 32;
/// Frames between checks of how the GPU's clock relates to the CPU's.
const CALIBRATE_EVERY: u32 = 240;
const CALIBRATE_SAMPLES: u32 = 16;
/// A clock reading that took longer than this says little about when it was taken, in nanoseconds.
const CALIBRATE_MAX_BRACKET: u64 = 50_000;

struct Slot {
    elapsed: GLuint,
    stamp: GLuint,
    fence: GLsync,
    frame: usize,
    /// When the fence was first seen signalled: a bound on when the frame finished.
    fence_seen_ns: u64,
}

pub struct GpuProbe {
    slots: Vec<Slot>,
    free: Vec<usize>,
    flying: VecDeque<usize>,
    /// The frame being drawn, whose timer query is running.
    current: Option<usize>,
    /// What to add to a GPU timestamp to put it on the presentation clock.
    offset: i64,
    since_calibration: u32,
    pub skipped: u64,
}

impl GpuProbe {
    /// A probe for the current context, unless asked not to or the driver can't.
    pub fn new() -> Option<Self> {
        if std::env::var("RMX_BENCH_GPU").is_ok_and(|value| value == "off") {
            return None;
        }
        if !gl::QueryCounter::is_loaded() || !gl::FenceSync::is_loaded() {
            eprintln!("rmx: no GPU timing: the driver lacks timer queries or fences");
            return None;
        }
        let mut bits: GLint = 0;
        unsafe { gl::GetQueryiv(gl::TIMESTAMP, gl::QUERY_COUNTER_BITS, &mut bits) };
        if bits == 0 {
            eprintln!("rmx: no GPU timing: timestamps have no bits");
            return None;
        }

        let mut names = [0; POOL * 2];
        unsafe { gl::GenQueries(names.len() as i32, names.as_mut_ptr()) };
        let slots: Vec<Slot> = names
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| Slot {
                elapsed: pair[0],
                stamp: pair[1],
                fence: std::ptr::null(),
                frame: 0,
                fence_seen_ns: 0,
            })
            .collect();
        let mut probe = Self {
            free: (0..slots.len()).collect(),
            slots,
            flying: VecDeque::with_capacity(POOL),
            current: None,
            offset: 0,
            since_calibration: 0,
            skipped: 0,
        };
        let readings: Vec<_> = (0..CALIBRATE_SAMPLES).map(|_| clock_offset()).collect();
        let best = readings.iter().min_by_key(|reading| reading.1)?;
        probe.offset = best.0;
        Some(probe)
    }

    /// Whether a frame is still on its way through the GPU.
    pub fn busy(&self) -> bool {
        !self.flying.is_empty()
    }

    /// Start timing the commands of a frame.
    pub fn begin(&mut self) {
        if self.current.is_some() {
            return;
        }
        self.since_calibration += 1;
        if self.since_calibration >= CALIBRATE_EVERY {
            let (offset, bracket) = clock_offset();
            if bracket <= CALIBRATE_MAX_BRACKET {
                self.offset = offset;
                self.since_calibration = 0;
            }
        }
        let Some(index) = self.free.pop() else {
            self.skipped += 1;
            return;
        };
        unsafe { gl::BeginQuery(gl::TIME_ELAPSED, self.slots[index].elapsed) };
        self.current = Some(index);
    }

    /// The frame's commands have all been issued, but not its swap.
    pub fn end_draw(&mut self) {
        if self.current.is_some() {
            unsafe { gl::EndQuery(gl::TIME_ELAPSED) };
        }
    }

    /// The swap of `frame` has been issued: mark the point in the command stream right after it.
    pub fn mark(&mut self, frame: usize) {
        let Some(index) = self.current.take() else { return };
        let slot = &mut self.slots[index];
        unsafe {
            gl::QueryCounter(slot.stamp, gl::TIMESTAMP);
            slot.fence = gl::FenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0);
            // Without this the mark sits in the driver's buffer until the next frame.
            gl::Flush();
        }
        slot.frame = frame;
        slot.fence_seen_ns = 0;
        self.flying.push_back(index);
    }

    /// Read back what is ready, oldest first, and never wait for the rest.
    pub fn collect(&mut self, frames: &mut [Record]) {
        for &index in &self.flying {
            let slot = &mut self.slots[index];
            if slot.fence_seen_ns == 0 {
                let status = unsafe { gl::ClientWaitSync(slot.fence, 0, 0) };
                if status == gl::ALREADY_SIGNALED || status == gl::CONDITION_SATISFIED {
                    slot.fence_seen_ns = now_ns();
                }
            }
        }
        while let Some(&index) = self.flying.front() {
            let slot = &self.slots[index];
            if slot.fence_seen_ns == 0 || !available(slot.elapsed) || !available(slot.stamp) {
                return;
            }
            let (mut elapsed, mut stamp): (GLuint64, GLuint64) = (0, 0);
            unsafe {
                gl::GetQueryObjectui64v(slot.elapsed, gl::QUERY_RESULT, &mut elapsed);
                gl::GetQueryObjectui64v(slot.stamp, gl::QUERY_RESULT, &mut stamp);
                gl::DeleteSync(slot.fence);
            }
            if let Some(record) = frames.get_mut(slot.frame) {
                record.gpu_us = (elapsed / 1000) as u32;
                record.gpu_complete_ns = (stamp as i64 + self.offset) as u64;
                record.fence_seen_ns = slot.fence_seen_ns;
            }
            self.flying.pop_front();
            self.free.push(index);
        }
    }
}

fn available(query: GLuint) -> bool {
    let mut ready: GLint = 0;
    unsafe { gl::GetQueryObjectiv(query, gl::QUERY_RESULT_AVAILABLE, &mut ready) };
    ready != 0
}

/// What to add to a GPU timestamp to get the presentation clock, and how long reading it took.
fn clock_offset() -> (i64, u64) {
    let mut gpu: GLint64 = 0;
    let before = now_ns();
    unsafe { gl::GetInteger64v(gl::TIMESTAMP, &mut gpu) };
    let after = now_ns();
    (((before + after) / 2) as i64 - gpu, after - before)
}
