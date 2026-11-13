//! Runs a terminal on a [`TransportReader`] and [`TransportWriter`] instead of a local PTY.
//!
//! Three tokio tasks, on the caller's runtime, talk to the connection: one reads, one writes and
//! one resizes, so that none of them waits on another. The terminal's lock is never taken on the
//! runtime, as it can be held for milliseconds. A thread of its own parses instead, the same way
//! `alacritty_terminal`'s `EventLoop` has one.
//!
//! # Buffering
//!
//! `Limits::max_queued` is the bound: bytes read but not yet parsed never exceed it by more than
//! one read's worth, which is 16 MiB plus a packet with [`Limits::default`].
//!
//! An SSH connection only has one window, so a channel nobody reads holds up its siblings. Reading
//! therefore pauses only while the buffer is full *and* the parser is getting somewhere, at the
//! parser's own speed. Should the parser make no progress for `Limits::stall`, because something
//! holds the terminal, reading goes on and the oldest queued output is thrown away for what comes
//! in. The terminal then parses what is left from a clean state and
//! [`SessionEvent::OutputDiscarded`] says so.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{io, thread};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Processor, Timeout};
use bytes::{Buf, Bytes};
use memchr::memchr_iter;
use parking_lot::{Condvar, Mutex};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};

use crate::scan::{Found, Scanner};
use crate::tap::{Direction, Taps};
use crate::transport::{
    CloseReason, SessionEvent, TransportReader, TransportWriter, WorkingDirectory,
};

/// Stop a batch once this much time has gone. Clearing a 4K screen takes tens of microseconds and
/// a slice can hold a few of those, so this leaves the slice in flight room to end within the 500
/// microseconds a batch may hold the terminal's lock.
const SOFT_LOCK_HOLD: Duration = Duration::from_micros(250);

/// Bytes parsed under one lock at most, as `alacritty_terminal`'s `MAX_LOCKED_READ`.
const MAX_LOCKED_BYTES: usize = u16::MAX as usize;

/// Bytes parsed between looks at the clock.
const MAX_SLICE: usize = 512;

/// How long an update the program marked as synchronized stays hidden at most, as in `vte`.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

/// How much is buffered, and how long a stalled parser is waited for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) max_queued: usize,
    pub(crate) stall: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_queued: 16 * 1024 * 1024, stall: Duration::from_millis(200) }
    }
}

/// `vte`'s synchronized updates buffer everything until the end of the update and then parse it
/// in one go under the lock, up to 2 MiB. Keep it from starting: [`Parser`] hides the update
/// by not reporting it instead.
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}

    fn clear_timeout(&mut self) {}

    fn pending_timeout(&self) -> bool {
        false
    }
}

/// What the reader has handed over and the parser has not taken yet.
struct Inbox {
    chunks: VecDeque<Bytes>,
    /// Bytes read and not parsed yet, those the parser took for its next batch included.
    queued: usize,
    /// Bytes thrown away just before `chunks`.
    discarded: u64,
    /// When the parser last finished a batch, or had work again after running out of it.
    progress: Instant,
    closed: Option<CloseReason>,
    /// The terminal is gone.
    stop: bool,
}

/// What the tasks on the runtime and the parser thread share.
pub(crate) struct Channel {
    inbox: Mutex<Inbox>,
    /// There is something for the parser.
    more: Condvar,
    /// The parser has made room in the inbox.
    room: Notify,
    limits: Limits,
    working_directory: Mutex<Option<WorkingDirectory>>,
    close_reason: Mutex<Option<CloseReason>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl Channel {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            inbox: Mutex::new(Inbox {
                chunks: VecDeque::new(),
                queued: 0,
                discarded: 0,
                progress: Instant::now(),
                closed: None,
                stop: false,
            }),
            more: Condvar::new(),
            room: Notify::new(),
            limits,
            working_directory: Mutex::default(),
            close_reason: Mutex::default(),
            tasks: Mutex::default(),
        }
    }

    /// Wait until reading on is right. Returns whether what is read is to replace older output.
    async fn wait_for_room(&self) -> bool {
        loop {
            let deadline = {
                let inbox = self.inbox.lock();
                if inbox.queued < self.limits.max_queued {
                    return false;
                }
                inbox.progress + self.limits.stall
            };
            if Instant::now() >= deadline {
                return true;
            }
            tokio::select! {
                () = self.room.notified() => {},
                () = tokio::time::sleep_until(deadline.into()) => {},
            }
        }
    }

    /// Queue `chunk`. Returns `false` if nobody wants it anymore.
    fn push(&self, chunk: Bytes, replace_oldest: bool) -> bool {
        let mut inbox = self.inbox.lock();
        if inbox.stop || inbox.closed.is_some() {
            return false;
        }
        if replace_oldest {
            while inbox.queued + chunk.len() > self.limits.max_queued {
                let Some(oldest) = inbox.chunks.pop_front() else { break };
                inbox.queued -= oldest.len();
                inbox.discarded += oldest.len() as u64;
            }
        }
        if inbox.queued == 0 {
            inbox.progress = Instant::now();
        }
        inbox.queued += chunk.len();
        inbox.chunks.push_back(chunk);
        drop(inbox);
        self.more.notify_one();
        true
    }

    /// The session is over, for the first reason given.
    fn close(&self, reason: CloseReason) {
        {
            let mut inbox = self.inbox.lock();
            inbox.closed.get_or_insert(reason);
        }
        self.more.notify_one();
        self.abort_tasks();
    }

    /// The terminal is gone.
    pub(crate) fn stop(&self) {
        self.inbox.lock().stop = true;
        self.more.notify_one();
        self.abort_tasks();
    }

    fn abort_tasks(&self) {
        for task in self.tasks.lock().iter() {
            task.abort();
        }
    }

    /// Have `task` stopped with the session, which may be over already.
    fn track(&self, task: AbortHandle) {
        self.tasks.lock().push(task);
        let inbox = self.inbox.lock();
        if inbox.stop || inbox.closed.is_some() {
            drop(inbox);
            self.abort_tasks();
        }
    }

    pub(crate) fn working_directory(&self) -> Option<WorkingDirectory> {
        self.working_directory.lock().clone()
    }

    pub(crate) fn close_reason(&self) -> Option<CloseReason> {
        self.close_reason.lock().clone()
    }
}

/// The handle a [`Terminal`](crate::Terminal) keeps on its connection.
#[derive(Clone)]
pub(crate) struct Remote {
    input: mpsc::UnboundedSender<Bytes>,
    size: Arc<watch::Sender<WindowSize>>,
    pub(crate) channel: Arc<Channel>,
}

impl Remote {
    /// Send `bytes` to the program. Does nothing once the session is over.
    pub(crate) fn write(&self, bytes: Bytes) {
        let _ = self.input.send(bytes);
    }

    /// Tell the program about a new size. Does nothing once the session is over.
    pub(crate) fn resize(&self, size: WindowSize) {
        self.size.send_replace(size);
    }
}

/// Start `parser` on a thread and the tasks that serve it on `runtime`. `size` is what the
/// connection already has.
pub(crate) fn start<R, W, L>(
    reader: R,
    writer: W,
    runtime: &Handle,
    parser: Parser<L>,
    size: WindowSize,
    taps: Arc<Taps>,
) -> io::Result<Remote>
where
    R: TransportReader,
    W: TransportWriter,
    L: EventListener + Send + 'static,
{
    let channel = parser.channel.clone();
    thread::Builder::new().name("rmx-parser".into()).spawn(move || parser.run())?;

    let writer = Arc::new(writer);
    let (input, input_rx) = mpsc::unbounded_channel();
    let (size_tx, size_rx) = watch::channel(size);
    let track = |task: JoinHandle<()>| channel.track(task.abort_handle());
    track(runtime.spawn(read_loop(reader, channel.clone(), taps)));
    track(runtime.spawn(write_loop(writer.clone(), input_rx, channel.clone())));
    track(runtime.spawn(resize_loop(writer, size_rx, channel.clone())));

    Ok(Remote { input, size: Arc::new(size_tx), channel })
}

async fn read_loop<R: TransportReader>(mut reader: R, channel: Arc<Channel>, taps: Arc<Taps>) {
    loop {
        let replace_oldest = channel.wait_for_room().await;
        match reader.read().await {
            Ok(chunk) if chunk.is_empty() => {},
            Ok(chunk) => {
                taps.output.send_with(Direction::Output, || chunk.clone());
                if !channel.push(chunk, replace_oldest) {
                    return;
                }
            },
            Err(reason) => return channel.close(reason),
        }
    }
}

async fn write_loop<W: TransportWriter>(
    writer: Arc<W>,
    mut input: mpsc::UnboundedReceiver<Bytes>,
    channel: Arc<Channel>,
) {
    while let Some(bytes) = input.recv().await {
        if let Err(reason) = writer.write(bytes).await {
            return channel.close(reason);
        }
    }
}

async fn resize_loop<W: TransportWriter>(
    writer: Arc<W>,
    mut size: watch::Receiver<WindowSize>,
    channel: Arc<Channel>,
) {
    while size.changed().await.is_ok() {
        let latest = *size.borrow_and_update();
        if let Err(reason) = writer.resize(latest).await {
            return channel.close(reason);
        }
    }
}

/// What to do next on the parser thread.
enum Work {
    Parse,
    SyncExpired,
    Finished(CloseReason),
    Stopped,
}

/// What one batch of parsing did, for measuring it.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Batch {
    pub(crate) parsed: usize,
    pub(crate) held: Duration,
}

/// Parses queued output in batches, on a thread of its own.
pub(crate) struct Parser<L: EventListener> {
    term: Arc<FairMutex<Term<L>>>,
    listener: L,
    channel: Arc<Channel>,
    on_event: Box<dyn Fn(SessionEvent) + Send>,
    processor: Processor<NoSync>,
    scanner: Scanner,
    /// The next bytes to parse, taken from the inbox.
    pending: VecDeque<Bytes>,
    /// When the synchronized update in progress is shown regardless.
    sync_deadline: Option<Instant>,
}

impl<L: EventListener> Parser<L> {
    pub(crate) fn new(
        term: Arc<FairMutex<Term<L>>>,
        listener: L,
        channel: Arc<Channel>,
        on_event: Box<dyn Fn(SessionEvent) + Send>,
    ) -> Self {
        Self {
            term,
            listener,
            channel,
            on_event,
            processor: Processor::new(),
            scanner: Scanner::new(),
            pending: VecDeque::new(),
            sync_deadline: None,
        }
    }

    fn run(mut self) {
        loop {
            match self.next_work() {
                Work::Parse => {
                    self.parse_batch();
                },
                Work::SyncExpired => {
                    self.sync_deadline = None;
                    self.listener.send_event(Event::Wakeup);
                },
                Work::Finished(reason) => {
                    // Show the end of a synchronized update that is never going to end.
                    self.sync_deadline = None;
                    self.listener.send_event(Event::Wakeup);
                    *self.channel.close_reason.lock() = Some(reason.clone());
                    (self.on_event)(SessionEvent::Closed(reason));
                    return;
                },
                Work::Stopped => return,
            }
        }
    }

    /// Wait for something to do, taking the next bytes to parse if that is what there is.
    fn next_work(&mut self) -> Work {
        let channel = self.channel.clone();
        let mut inbox = channel.inbox.lock();
        let discarded = loop {
            if inbox.stop {
                return Work::Stopped;
            }
            if !self.pending.is_empty() {
                return Work::Parse;
            }
            if !inbox.chunks.is_empty() {
                let mut taken = 0;
                while taken < MAX_LOCKED_BYTES {
                    let Some(chunk) = inbox.chunks.pop_front() else { break };
                    taken += chunk.len();
                    self.pending.push_back(chunk);
                }
                break std::mem::take(&mut inbox.discarded);
            }
            if let Some(reason) = inbox.closed.clone() {
                return Work::Finished(reason);
            }
            match self.sync_deadline {
                Some(deadline) if Instant::now() >= deadline => return Work::SyncExpired,
                Some(deadline) => {
                    channel.more.wait_until(&mut inbox, deadline);
                },
                None => channel.more.wait(&mut inbox),
            }
        };
        drop(inbox);

        if discarded > 0 {
            // Whatever sequence was in progress has lost its middle.
            self.processor = Processor::new();
            self.scanner.reset();
            self.sync_deadline = None;
            (self.on_event)(SessionEvent::OutputDiscarded(discarded));
        }
        Work::Parse
    }

    /// Parse some of `pending` under one hold of the terminal's lock.
    ///
    /// `lock` is the lease of `EventLoop::pty_read` and the lock in one: from when it is called,
    /// others queue up behind this thread, and between batches they get their turn.
    pub(crate) fn parse_batch(&mut self) -> Batch {
        let mut working_directory = None;
        let term = self.term.clone();
        let mut term = term.lock();
        let started = Instant::now();
        let mut parsed = 0;

        let Self { processor, scanner, pending, sync_deadline, .. } = self;
        'batch: while let Some(chunk) = pending.front_mut() {
            while !chunk.is_empty() {
                let end = slice_end(chunk);
                scanner.feed(&chunk[..end], &mut |found| match found {
                    Found::SyncBegin => *sync_deadline = Some(Instant::now() + SYNC_TIMEOUT),
                    Found::SyncEnd => *sync_deadline = None,
                    Found::WorkingDirectory(directory) => working_directory = Some(directory),
                });
                processor.advance(&mut *term, &chunk[..end]);
                chunk.advance(end);
                parsed += end;

                if parsed >= MAX_LOCKED_BYTES || started.elapsed() >= SOFT_LOCK_HOLD {
                    break 'batch;
                }
            }
            pending.pop_front();
        }
        if pending.front().is_some_and(Bytes::is_empty) {
            pending.pop_front();
        }

        let held = started.elapsed();
        drop(term);

        if self.sync_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.sync_deadline = None;
        }
        if parsed > 0 && self.sync_deadline.is_none() {
            self.listener.send_event(Event::Wakeup);
        }
        if let Some(directory) = working_directory {
            self.announce_directory(directory);
        }
        {
            let mut inbox = self.channel.inbox.lock();
            inbox.queued -= parsed;
            inbox.progress = Instant::now();
        }
        self.channel.room.notify_one();

        Batch { parsed, held }
    }

    /// Report `directory` if the shell has moved. Most prompts repeat where they are.
    fn announce_directory(&self, directory: WorkingDirectory) {
        let mut current = self.channel.working_directory.lock();
        if current.as_ref() != Some(&directory) {
            *current = Some(directory.clone());
            drop(current);
            (self.on_event)(SessionEvent::WorkingDirectory(directory));
        }
    }
}

/// How much of `bytes`, which is not empty, to parse before looking at the clock: up to
/// [`MAX_SLICE`], and a sequence that redraws the screen all by itself.
///
/// Those take up to a few hundred microseconds on a 4K screen, so a slice with several of them
/// could overrun the lock's bound. Text and colors are never worth the clock.
fn slice_end(bytes: &[u8]) -> usize {
    let window = &bytes[..bytes.len().min(MAX_SLICE)];
    for index in memchr_iter(0x1b, window) {
        if let Some(length) = screen_wide_sequence(&window[index..]) {
            return if index == 0 { length } else { index };
        }
    }
    window.len()
}

/// The length of the sequence at the start of `bytes`, if it can touch the whole screen. A
/// sequence cut off by the end of `bytes` is taken not to, which costs one extra in a slice.
fn screen_wide_sequence(bytes: &[u8]) -> Option<usize> {
    match *bytes.get(1)? {
        // Full reset, and fill with `E`.
        b'c' => Some(2),
        b'#' => Some(3.min(bytes.len())),
        b'[' => {
            let params = bytes.get(2..)?;
            let last = params.iter().take(32).position(|byte| (0x40..=0x7e).contains(byte))?;
            let heavy = match params[last] {
                // Erase, insert or delete lines, scroll, insert or delete characters.
                b'J' | b'L' | b'M' | b'S' | b'T' | b'@' | b'P' | b'X' => true,
                // The alternate screen is a private mode.
                b'h' | b'l' => params[0] == b'?',
                _ => false,
            };
            heavy.then_some(2 + last + 1)
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests;
