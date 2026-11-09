//! The worker thread behind persistent buffer uploads. It knows nothing of OpenGL: it only ever
//! writes to memory the main thread mapped, and never touches a GL or EGL entry point.
//!
//! Each update first lands in `shadow`, a full copy of the desktop only this thread reads. That
//! is what makes merging safe: an upload may cover pixels no recent update touched, and every
//! one of them is read from the shadow rather than from a staging buffer holding an older frame.

use std::collections::VecDeque;
use std::sync::atomic::{Ordering, fence};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::{mem, ptr};

use log::warn;
use rmx_display::FrameUpdate;

use super::dirty::{DirtyRegion, Rect};

/// Staging buffers of a texture: one the GPU reads, one waiting, one being written.
pub const SLOTS: usize = 3;

/// Updates queued before the queue has to grow, which only a stalled worker makes it do.
const QUEUE_CAPACITY: usize = 64;

/// Spent pixel buffers kept for the session to fill again, beyond which they are freed.
pub const MAX_SPARES: usize = 8;

/// Bytes of a BGRA8 pixel.
pub const BYTES_PER_PIXEL: usize = 4;

/// Called from the worker once there is something new to upload.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Where an update is drawn once clipped to the desktop, and how far apart its rows are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clipped {
    pub rect: Rect,
    pub stride: usize,
}

/// `update` clipped to a desktop of `width` by `height`, or `None` if nothing of it is left or
/// it brings too few pixels. Sessions race resizes, so neither is a bug worth a panic.
pub fn clip_update(update: &FrameUpdate, width: u32, height: u32) -> Option<Clipped> {
    let stride = (update.width as usize).checked_mul(BYTES_PER_PIXEL)?;
    let needed = stride.checked_mul(update.height as usize)?;
    if update.pixels.len() < needed {
        warn!("Remote update of {} bytes is short of {needed}", update.pixels.len());
        return None;
    }

    let rect = Rect {
        x: update.x,
        y: update.y,
        width: update.width.min(width.saturating_sub(update.x)),
        height: update.height.min(height.saturating_sub(update.y)),
    };
    (!rect.is_empty()).then_some(Clipped { rect, stride })
}

/// What a staging buffer is being used for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Free,
    Writing,
    Ready,
    /// Named in a `glTexSubImage2D` the GPU may not have finished: untouchable until its fence.
    InFlight,
}

/// The state of the staging buffers. Only ever touched under the lock.
#[derive(Debug)]
struct Ring {
    slots: [Slot; SLOTS],
    /// The newest finished buffer, and the pixels of it to upload.
    ready: Option<(usize, DirtyRegion)>,
}

impl Ring {
    fn new() -> Self {
        Self { slots: [Slot::Free; SLOTS], ready: None }
    }

    /// Claim a buffer to write to.
    fn acquire(&mut self) -> Option<usize> {
        let slot = self.slots.iter().position(|slot| *slot == Slot::Free)?;
        self.slots[slot] = Slot::Writing;
        Some(slot)
    }

    /// Offer a written buffer in place of the one waiting, which the GPU has never seen and so
    /// can be reused at once. The caller's `region` covers the waiting buffer's.
    fn publish(&mut self, slot: usize, region: DirtyRegion) {
        debug_assert_eq!(self.slots[slot], Slot::Writing);
        self.slots[slot] = Slot::Ready;
        if let Some((replaced, _)) = self.ready.replace((slot, region)) {
            self.slots[replaced] = Slot::Free;
        }
    }

    fn take_ready(&mut self) -> Option<(usize, DirtyRegion)> {
        let (slot, region) = self.ready.take()?;
        self.slots[slot] = Slot::InFlight;
        Some((slot, region))
    }

    /// The GPU is done with `slot`.
    fn release(&mut self, slot: usize) {
        debug_assert_eq!(self.slots[slot], Slot::InFlight);
        self.slots[slot] = Slot::Free;
    }
}

/// A mapped staging buffer.
#[derive(Clone, Copy, Debug)]
pub struct Mapped(pub *mut u8);

// Safety: the slot state under the lock gives the worker exclusive use of the memory.
unsafe impl Send for Mapped {}

#[derive(Debug)]
struct State {
    queue: VecDeque<FrameUpdate>,
    ring: Ring,
    spares: Vec<Vec<u8>>,
    /// Whether the worker has pixels not yet offered for upload.
    busy: bool,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// The main thread's handle on a worker.
pub struct Stream {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

impl Stream {
    /// Start a worker for a desktop of `width` by `height`, writing to `buffers`, each holding a
    /// whole frame and valid until the worker has been joined.
    pub fn spawn(width: u32, height: u32, buffers: [Mapped; SLOTS], notify: Notify) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::with_capacity(QUEUE_CAPACITY),
                ring: Ring::new(),
                spares: Vec::with_capacity(MAX_SPARES),
                busy: true,
                stop: false,
            }),
            wake: Condvar::new(),
        });

        let worker = Worker::new(width, height, buffers);
        let state = shared.clone();
        let thread = thread::Builder::new()
            .name("rmx-remote-upload".into())
            .spawn(move || worker.run(&state, &notify))
            .expect("spawning the remote upload worker");

        Self { shared, thread: Some(thread) }
    }

    /// Hand an update over. Never waits for the worker's copying, which happens unlocked.
    pub fn submit(&self, update: FrameUpdate) {
        let mut state = self.shared.lock();
        state.queue.push_back(update);
        state.busy = true;
        drop(state);
        self.shared.wake.notify_one();
    }

    /// Take the newest finished buffer, which stays the caller's until [`Self::release`].
    pub fn take_ready(&self) -> Option<(usize, DirtyRegion)> {
        self.shared.lock().ring.take_ready()
    }

    pub fn release(&self, slot: usize) {
        self.shared.lock().ring.release(slot);
        self.shared.wake.notify_one();
    }

    /// Whether more is coming, so that the caller should come back.
    pub fn pending(&self) -> bool {
        let state = self.shared.lock();
        state.busy || state.ring.ready.is_some()
    }

    pub fn take_spare(&self) -> Option<Vec<u8>> {
        self.shared.lock().spares.pop()
    }

    /// Ask the worker to finish, without waiting for it.
    pub fn stop(&self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_one();
    }

    /// Whether the worker is gone, after which nothing writes to the buffers.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wait for a stopped worker to exit.
    pub fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    width: u32,
    height: u32,
    shadow: Vec<u8>,
    /// Pixels of the shadow not yet in any buffer.
    dirty: DirtyRegion,
    buffers: [Mapped; SLOTS],
}

impl Worker {
    fn new(width: u32, height: u32, buffers: [Mapped; SLOTS]) -> Self {
        let mut dirty = DirtyRegion::default();
        // The texture starts out undefined: black until updates say otherwise.
        dirty.add(Rect { x: 0, y: 0, width, height });
        Self { width, height, shadow: Vec::new(), dirty, buffers }
    }

    fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }

    fn run(mut self, shared: &Shared, notify: &Notify) {
        // Allocated here so that the main thread never pays for the pages.
        self.shadow = vec![0; self.stride() * self.height as usize];
        let mut batch = VecDeque::with_capacity(QUEUE_CAPACITY);

        let mut state = shared.lock();
        loop {
            if state.stop {
                return;
            }

            // Everything queued goes into the shadow first, so that a buffer holds the newest.
            if !state.queue.is_empty() {
                mem::swap(&mut state.queue, &mut batch);
                drop(state);
                for update in batch.drain(..) {
                    let spent = self.apply(update);
                    let mut state = shared.lock();
                    if state.spares.len() < MAX_SPARES {
                        state.spares.push(spent);
                    }
                }
                state = shared.lock();
                continue;
            }

            if self.dirty.is_empty() {
                state.busy = false;
                state = shared.wake.wait(state).unwrap_or_else(|poison| poison.into_inner());
                continue;
            }

            // Every buffer is on its way to the GPU: updates keep merging in the shadow, and the
            // main thread wakes this once a fence has passed.
            let Some(slot) = state.ring.acquire() else {
                state = shared.wake.wait(state).unwrap_or_else(|poison| poison.into_inner());
                continue;
            };

            // A buffer the main thread has not taken yet is about to be replaced by this one,
            // whose pixels must then include its own.
            let mut region = self.dirty;
            if let Some((_, waiting)) = &state.ring.ready {
                region.merge(waiting);
            }
            self.dirty.clear();
            drop(state);

            self.copy_to_buffer(slot, &region);
            // Staging memory may be write combined: empty the CPU's buffers before the GPU can
            // be told to read it.
            fence(Ordering::SeqCst);

            shared.lock().ring.publish(slot, region);
            notify();

            state = shared.lock();
        }
    }

    /// Apply `update` to the shadow, returning the pixel buffer it no longer needs.
    fn apply(&mut self, mut update: FrameUpdate) -> Vec<u8> {
        let Some(Clipped { rect, stride }) = clip_update(&update, self.width, self.height) else {
            return update.pixels;
        };

        let whole = Rect { x: 0, y: 0, width: self.width, height: self.height };
        if rect == whole && stride == self.stride() && update.pixels.len() == self.shadow.len() {
            // A full frame replaces the shadow for free.
            mem::swap(&mut update.pixels, &mut self.shadow);
        } else {
            let row_bytes = rect.width as usize * BYTES_PER_PIXEL;
            let shadow_stride = self.stride();
            let start = (rect.y as usize * shadow_stride) + rect.x as usize * BYTES_PER_PIXEL;
            let rows = update.pixels.chunks_exact(stride).take(rect.height as usize);
            for (row, source) in rows.enumerate() {
                let start = start + row * shadow_stride;
                self.shadow[start..start + row_bytes].copy_from_slice(&source[..row_bytes]);
            }
        }

        self.dirty.add(rect);
        update.pixels
    }

    /// Copy `region` from the shadow to a staging buffer, at the same place in it.
    fn copy_to_buffer(&self, slot: usize, region: &DirtyRegion) {
        let stride = self.stride();
        let buffer = self.buffers[slot].0;
        for rect in region.rects() {
            let row_bytes = rect.width as usize * BYTES_PER_PIXEL;
            let start = rect.y as usize * stride + rect.x as usize * BYTES_PER_PIXEL;
            // A full width rectangle is one contiguous run.
            let (runs, run_bytes, step) = if row_bytes == stride {
                (1, row_bytes * rect.height as usize, 0)
            } else {
                (rect.height as usize, row_bytes, stride)
            };
            for run in 0..runs {
                let offset = start + run * step;
                debug_assert!(offset + run_bytes <= self.shadow.len());
                // Safety: rects are clipped to the desktop, which both buffers hold whole, and
                // this slot is `Writing`, so nothing else is using it.
                unsafe {
                    ptr::copy_nonoverlapping(
                        self.shadow.as_ptr().add(offset),
                        buffer.add(offset),
                        run_bytes,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn update(x: u32, y: u32, width: u32, height: u32, fill: u8) -> FrameUpdate {
        FrameUpdate { x, y, width, height, pixels: vec![fill; (width * height * 4) as usize] }
    }

    #[test]
    fn ring_never_offers_an_in_flight_buffer() {
        let mut ring = Ring::new();
        let first = ring.acquire().unwrap();
        ring.publish(first, DirtyRegion::default());
        assert_eq!(ring.take_ready().unwrap().0, first);

        let second = ring.acquire().unwrap();
        let third = ring.acquire().unwrap();
        assert!(first != second && first != third && second != third);
        assert_eq!(ring.acquire(), None);

        ring.release(first);
        assert_eq!(ring.acquire(), Some(first));
    }

    #[test]
    fn ring_reuses_a_replaced_buffer_at_once() {
        let mut ring = Ring::new();
        let first = ring.acquire().unwrap();
        ring.publish(first, DirtyRegion::default());
        let second = ring.acquire().unwrap();
        ring.publish(second, DirtyRegion::default());

        // The GPU never saw the first, and only the newest is offered.
        assert_eq!(ring.slots[first], Slot::Free);
        assert_eq!(ring.take_ready().unwrap().0, second);
        assert!(ring.take_ready().is_none());
    }

    #[test]
    fn clipping() {
        let clip = |update: &FrameUpdate| clip_update(update, 100, 50);
        let rect = |x, y, width, height| Rect { x, y, width, height };

        assert_eq!(clip(&update(10, 10, 20, 20, 0)).unwrap().rect, rect(10, 10, 20, 20));
        let past = clip(&update(90, 40, 20, 20, 0)).unwrap();
        assert_eq!((past.rect, past.stride), (rect(90, 40, 10, 10), 80));
        assert_eq!(clip(&update(100, 0, 20, 20, 0)), None);
        assert_eq!(clip(&update(0, 0, 0, 20, 0)), None);

        let mut short = update(0, 0, 10, 10, 0);
        short.pixels.pop();
        assert_eq!(clip(&short), None);
    }

    /// Plays the GPU: copies what is ready out of the staging buffers, late and in a hurry, and
    /// checks that no buffer changes while it holds it.
    #[test]
    fn nothing_is_lost_while_frames_back_up() {
        const WIDTH: u32 = 320;
        const HEIGHT: u32 = 200;
        let stride = WIDTH as usize * BYTES_PER_PIXEL;
        let len = stride * HEIGHT as usize;

        let mut seed = 0x1234_5679_u32;
        let mut next = move |bound: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % bound
        };

        for round in 0..30 {
            let mut buffers: Vec<Vec<u8>> = (0..SLOTS).map(|_| vec![0xAA; len]).collect();
            let mapped: [Mapped; SLOTS] =
                [0, 1, 2].map(|index| Mapped(buffers[index].as_mut_ptr()));
            let mut stream = Stream::spawn(WIDTH, HEIGHT, mapped, Arc::new(|| {}));

            let mut model = vec![0u8; len];
            let mut texture = vec![0u8; len];
            let mut held: Option<(usize, Vec<u8>)> = None;
            let mut upload = |stream: &Stream, texture: &mut [u8]| {
                if let Some((slot, snapshot)) = held.take() {
                    assert_eq!(buffers[slot], snapshot, "buffer changed in flight");
                    stream.release(slot);
                }
                if let Some((slot, region)) = stream.take_ready() {
                    for rect in region.rects() {
                        for row in rect.y..rect.y + rect.height {
                            let start = row as usize * stride + rect.x as usize * 4;
                            let end = start + rect.width as usize * 4;
                            texture[start..end].copy_from_slice(&buffers[slot][start..end]);
                        }
                    }
                    held = Some((slot, buffers[slot].clone()));
                }
            };

            for index in 0..120 {
                let (x, y) = (next(WIDTH), next(HEIGHT));
                let (width, height) = (next(WIDTH - x) + 1, next(HEIGHT - y) + 1);
                let fill = (index % 251 + 1) as u8;
                for row in y..y + height {
                    let start = row as usize * stride + x as usize * 4;
                    model[start..start + width as usize * 4].fill(fill);
                }
                stream.submit(update(x, y, width, height, fill));

                // Most of the time the main thread is busy elsewhere.
                if next(5) == 0 {
                    upload(&stream, &mut texture);
                }
                if round % 3 == 0 && next(8) == 0 {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }

            let deadline = Instant::now() + Duration::from_secs(10);
            while stream.pending() {
                assert!(Instant::now() < deadline, "the worker never finished");
                upload(&stream, &mut texture);
                std::thread::sleep(Duration::from_micros(100));
            }
            upload(&stream, &mut texture);

            assert!(texture == model, "round {round} lost pixels");
            stream.stop();
            stream.join();
        }
    }

    #[test]
    fn a_full_frame_swaps_into_the_shadow() {
        let mut worker = Worker::new(4, 4, [Mapped(ptr::null_mut()); SLOTS]);
        worker.shadow = vec![0; 64];
        worker.dirty.clear();
        let spent = worker.apply(update(0, 0, 4, 4, 7));
        assert_eq!(spent, vec![0; 64]);
        assert!(worker.shadow.iter().all(|byte| *byte == 7));
        assert_eq!(worker.dirty.rects(), &[Rect { x: 0, y: 0, width: 4, height: 4 }]);
    }
}
