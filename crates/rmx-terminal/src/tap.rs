//! Copies of the bytes going into and out of a terminal, for whoever asks with
//! [`Terminal::tap`](crate::Terminal::tap).
//!
//! A tap never holds up the session: each has a bounded channel, and what does not fit is thrown
//! away and counted. With no tap attached, a stream costs one atomic load per chunk.

use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite};
use bytes::Bytes;
use parking_lot::Mutex;
use polling::{Event, PollMode, Poller};
use tokio::sync::mpsc::{self, error::TrySendError};

/// Which way bytes went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Typed or pasted into the terminal, on their way to the program.
    Input,
    /// From the program, as the remote or the PTY delivered them.
    Output,
}

/// Which streams a tap gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Streams {
    pub input: bool,
    pub output: bool,
}

impl Streams {
    pub const INPUT: Self = Self { input: true, output: false };
    pub const OUTPUT: Self = Self { input: false, output: true };
    pub const BOTH: Self = Self { input: true, output: true };
}

/// Bytes a tap was handed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tapped {
    pub direction: Direction,
    pub bytes: Bytes,
}

/// What a tap missed because its channel was full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// Chunks thrown away.
    pub chunks: u64,
    /// The bytes in them.
    pub bytes: u64,
}

#[derive(Default)]
struct Counts {
    chunks: AtomicU64,
    bytes: AtomicU64,
}

struct Subscriber {
    id: u64,
    sender: mpsc::Sender<Tapped>,
    dropped: Arc<Counts>,
}

/// One stream of a terminal, and the taps on it.
#[derive(Default)]
pub(crate) struct Point {
    /// How many taps are attached, read before anything else is done.
    attached: AtomicUsize,
    subscribers: Mutex<Vec<Subscriber>>,
}

impl Point {
    /// Whether any tap would take bytes now.
    #[inline]
    pub(crate) fn is_attached(&self) -> bool {
        self.attached.load(Ordering::Acquire) != 0
    }

    /// Hand the bytes `make` returns to every tap, calling it only if there is one.
    #[inline]
    pub(crate) fn send_with(&self, direction: Direction, make: impl FnOnce() -> Bytes) {
        if self.is_attached() {
            self.send(Tapped { direction, bytes: make() });
        }
    }

    #[cold]
    fn send(&self, tapped: Tapped) {
        if tapped.bytes.is_empty() {
            return;
        }
        for subscriber in self.subscribers.lock().iter() {
            match subscriber.sender.try_send(tapped.clone()) {
                Ok(()) | Err(TrySendError::Closed(_)) => {},
                Err(TrySendError::Full(lost)) => {
                    subscriber.dropped.chunks.fetch_add(1, Ordering::Relaxed);
                    subscriber.dropped.bytes.fetch_add(lost.bytes.len() as u64, Ordering::Relaxed);
                },
            }
        }
    }

    fn attach(&self, subscriber: Subscriber) {
        let mut subscribers = self.subscribers.lock();
        subscribers.push(subscriber);
        self.attached.store(subscribers.len(), Ordering::Release);
    }

    fn detach(&self, id: u64) {
        let mut subscribers = self.subscribers.lock();
        subscribers.retain(|subscriber| subscriber.id != id);
        self.attached.store(subscribers.len(), Ordering::Release);
    }
}

/// Both streams of a terminal.
#[derive(Default)]
pub(crate) struct Taps {
    pub(crate) input: Point,
    pub(crate) output: Point,
    next_id: AtomicU64,
}

impl Taps {
    pub(crate) fn tap(self: &Arc<Self>, streams: Streams, capacity: usize) -> Tap {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let dropped = Arc::new(Counts::default());
        for (wanted, point) in [(streams.input, &self.input), (streams.output, &self.output)] {
            if wanted {
                let (sender, dropped) = (sender.clone(), dropped.clone());
                point.attach(Subscriber { id, sender, dropped });
            }
        }
        Tap { receiver, dropped, id, taps: Arc::downgrade(self) }
    }
}

/// Copies of what goes into and out of a terminal, from when it was attached until it is dropped.
///
/// At most the capacity it was made with waits in it: chunks that come while it is full are
/// thrown away and counted in [`Self::dropped`], so that a slow reader never slows the session.
/// Once the terminal is gone and its connection has wound down, receiving returns `None`.
pub struct Tap {
    receiver: mpsc::Receiver<Tapped>,
    dropped: Arc<Counts>,
    id: u64,
    taps: Weak<Taps>,
}

impl Tap {
    /// The next bytes, if some are waiting.
    pub fn try_recv(&mut self) -> Option<Tapped> {
        self.receiver.try_recv().ok()
    }

    /// The next bytes, waiting for them. `None` once the terminal is gone.
    pub async fn recv(&mut self) -> Option<Tapped> {
        self.receiver.recv().await
    }

    /// The next bytes, blocking this thread for them. `None` once the terminal is gone.
    ///
    /// Not for the main thread, nor inside an async runtime, which panics.
    pub fn blocking_recv(&mut self) -> Option<Tapped> {
        self.receiver.blocking_recv()
    }

    /// What has been thrown away so far as the channel was full.
    pub fn dropped(&self) -> Dropped {
        Dropped {
            chunks: self.dropped.chunks.load(Ordering::Relaxed),
            bytes: self.dropped.bytes.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        if let Some(taps) = self.taps.upgrade() {
            taps.input.detach(self.id);
            taps.output.detach(self.id);
        }
    }
}

/// A PTY whose output is tapped as `alacritty_terminal`'s event loop reads it.
///
/// It is its own reader, so that reading can go through the PTY's and be copied on the way.
pub(crate) struct TappedPty<P> {
    pty: P,
    taps: Arc<Taps>,
}

impl<P> TappedPty<P> {
    pub(crate) fn new(pty: P, taps: Arc<Taps>) -> Self {
        Self { pty, taps }
    }
}

impl<P: EventedReadWrite> Read for TappedPty<P> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.pty.reader().read(buf)?;
        self.taps.output.send_with(Direction::Output, || Bytes::copy_from_slice(&buf[..read]));
        Ok(read)
    }
}

impl<P: EventedReadWrite> EventedReadWrite for TappedPty<P> {
    type Reader = Self;
    type Writer = P::Writer;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        event: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        // SAFETY: the PTY is registered for as long as it would be without the tap.
        unsafe { self.pty.register(poll, event, mode) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.pty.reregister(poll, event, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut Self {
        self
    }

    fn writer(&mut self) -> &mut P::Writer {
        self.pty.writer()
    }
}

impl<P: EventedPty> EventedPty for TappedPty<P> {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}

impl<P: OnResize> OnResize for TappedPty<P> {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Sends `bytes` on `point`, counting how often the bytes had to be made.
    fn send(point: &Point, bytes: &'static [u8], made: &Cell<usize>) {
        point.send_with(Direction::Output, || {
            made.set(made.get() + 1);
            Bytes::from_static(bytes)
        });
    }

    #[test]
    fn nothing_is_made_or_sent_without_a_tap() {
        let taps = Arc::new(Taps::default());
        let made = Cell::new(0);
        send(&taps.output, b"unseen", &made);
        assert_eq!(made.get(), 0);

        // A tap on the other stream changes nothing for this one.
        let _input = taps.tap(Streams::INPUT, 4);
        send(&taps.output, b"unseen", &made);
        assert_eq!(made.get(), 0);
    }

    #[test]
    fn a_dropped_tap_is_detached() {
        let taps = Arc::new(Taps::default());
        let made = Cell::new(0);
        let mut kept = taps.tap(Streams::OUTPUT, 4);
        let gone = taps.tap(Streams::BOTH, 4);
        send(&taps.output, b"both", &made);
        drop(gone);
        send(&taps.output, b"kept", &made);
        assert_eq!(made.get(), 2);
        assert_eq!(kept.try_recv().unwrap().bytes, &b"both"[..]);
        assert_eq!(kept.try_recv().unwrap().bytes, &b"kept"[..]);

        drop(kept);
        send(&taps.output, b"unseen", &made);
        send(&taps.input, b"unseen", &made);
        assert_eq!(made.get(), 2, "bytes were made with every tap detached");
    }
}
