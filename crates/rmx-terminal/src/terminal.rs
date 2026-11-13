//! A terminal, and the shell behind it.

use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{io, mem, thread};

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Line, Point, Side};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, ClipboardType, Term, TermMode};
#[cfg(not(target_os = "linux"))]
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::NamedColor;
use bytes::Bytes;
use parking_lot::{Condvar, Mutex, MutexGuard};
use rmx_render::{RenderableCell, Rgb, SizeInfo};
use tokio::runtime::Handle;

use crate::content::{RenderableContent, RenderableCursor};
use crate::driver::{self, Channel, Limits, Parser, Remote};
use crate::mouse::MouseState;
use crate::tap::{Direction, Streams, Tap, TappedPty, Taps};
use crate::theme::Theme;
use crate::transport::{
    CloseReason, SessionEvent, TransportReader, TransportWriter, WorkingDirectory,
};

/// Where a terminal's events go.
pub trait Sink: Send + Sync + 'static {
    /// The terminal `id` has something new to draw.
    fn wakeup(&self, id: u64);

    /// Anything else from terminal `id`.
    fn event(&self, id: u64, event: Event);

    /// What happened to the connection of terminal `id`, if it has one. Comes from a thread of
    /// the terminal's.
    fn session(&self, _id: u64, _event: SessionEvent) {}
}

/// Reports a terminal's events, collapsing wakeups nobody has handled yet.
#[derive(Clone)]
pub(crate) struct Listener {
    id: u64,
    sink: Arc<dyn Sink>,
    shared: Arc<Shared>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => self.shared.output(),
            event => self.sink.event(self.id, event),
        }
    }
}

/// What the application is to do about a terminal event.
pub enum Output {
    None,
    Title(String),
    ResetTitle,
    Bell,
    Exit,
    /// A program put text on a clipboard.
    ClipboardStore(ClipboardType, String),
    /// A program asks for a clipboard's content, which goes through the function back to it.
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Sync + Send + 'static>),
}

/// What a terminal showed when it was last read, to draw without touching its lock.
pub struct Snapshot {
    pub cells: Vec<RenderableCell>,
    pub cursor: RenderableCursor,
    /// The size these cells are laid out for.
    pub size: SizeInfo,
}

/// A snapshot on its way from the terminal's snapshot thread to the main thread.
struct Published {
    /// Spare storage while `fresh` is false, and what is new when it is true.
    snapshot: Snapshot,
    /// Whether `snapshot` is newer than what the main thread has.
    fresh: bool,
    /// Whether the terminal has changed since a snapshot of it was last started.
    output: bool,
    /// The terminal is gone.
    stop: bool,
}

/// What the main thread, the parser and the snapshot thread share.
struct Shared {
    published: Mutex<Published>,
    wake: Condvar,
    /// The main thread has been told of a snapshot it has not taken yet.
    wakeup_pending: AtomicBool,
}

impl Shared {
    /// The terminal has changed.
    fn output(&self) {
        let mut published = self.published.lock();
        published.output = true;
        if !published.fresh {
            self.wake.notify_one();
        }
    }
}

/// Takes snapshots of the terminal, so that neither its lock nor walking its cells ever cost the
/// main thread any time. One at a time and only once the last was taken, which is at most once a
/// frame: the parser never loses more of the lock than drawing used to cost it.
fn snapshots(
    id: u64,
    shared: &Shared,
    term: &SharedTerm,
    size: &Mutex<SizeInfo>,
    theme: &Mutex<Theme>,
    sink: &dyn Sink,
) {
    loop {
        let mut cells = {
            let mut published = shared.published.lock();
            while !published.stop && (published.fresh || !published.output) {
                shared.wake.wait(&mut published);
            }
            if published.stop {
                return;
            }
            published.output = false;
            mem::take(&mut published.snapshot.cells)
        };

        let theme = *theme.lock();
        let term = term.lock();
        cells.clear();
        let mut content = RenderableContent::new(&term, &theme);
        cells.extend(content.by_ref());
        let cursor = content.cursor();
        let size = *size.lock();

        // Still holding the terminal, so that the main thread cannot read something newer from it
        // and then be handed this.
        let mut published = shared.published.lock();
        published.snapshot = Snapshot { cells, cursor, size };
        published.fresh = true;
        drop(published);
        drop(term);

        if !shared.wakeup_pending.swap(true, Ordering::AcqRel) {
            sink.wakeup(id);
        }
    }
}

/// Changes the terminal off the main thread: reflowing a long history takes many milliseconds.
#[derive(Default)]
struct Pending {
    /// The size still to apply.
    size: Option<SizeInfo>,
    /// Whether the terminal still has to be told it gained or lost the keyboard.
    focused: Option<bool>,
    /// Whether a thread is applying these, during which the terminal cannot be read.
    busy: bool,
}

type SharedTerm = FairMutex<Term<Listener>>;

/// What every terminal has before its program is attached.
struct Core {
    shared: Arc<Shared>,
    term: Arc<SharedTerm>,
    taps: Arc<Taps>,
}

/// What carries input and sizes to the program in a terminal.
#[derive(Clone)]
enum Control {
    Pty(EventLoopSender),
    Remote(Remote),
}

impl Control {
    /// Stop talking to the program.
    fn shutdown(&self) {
        match self {
            Self::Pty(sender) => {
                let _ = sender.send(Msg::Shutdown);
            },
            Self::Remote(remote) => remote.channel.stop(),
        }
    }

    /// Send `bytes`, which a tap has a copy of, to the program as typed.
    fn send(&self, bytes: Bytes) {
        match self {
            Self::Pty(sender) => Notifier(sender.clone()).notify(bytes.to_vec()),
            Self::Remote(remote) if !bytes.is_empty() => remote.write(bytes),
            Self::Remote(_) => (),
        }
    }

    /// Send `bytes` to the program as typed.
    fn type_in(&self, bytes: Cow<'static, [u8]>) {
        match self {
            Self::Pty(sender) => Notifier(sender.clone()).notify(bytes),
            Self::Remote(_) => self.send(into_bytes(bytes)),
        }
    }
}

/// `bytes`, without copying them.
fn into_bytes(bytes: Cow<'static, [u8]>) -> Bytes {
    match bytes {
        Cow::Borrowed(bytes) => Bytes::from_static(bytes),
        Cow::Owned(bytes) => Bytes::from(bytes),
    }
}

/// Answers to the program, which a local echo leaves out.
impl Notify for Control {
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        match self {
            Self::Pty(sender) => Notifier(sender.clone()).notify(bytes),
            Self::Remote(remote) => {
                let bytes = into_bytes(bytes.into());
                if !bytes.is_empty() {
                    remote.answer(bytes);
                }
            },
        }
    }
}

impl OnResize for Control {
    fn on_resize(&mut self, size: WindowSize) {
        match self {
            Self::Pty(sender) => Notifier(sender.clone()).on_resize(size),
            Self::Remote(remote) => remote.resize(size),
        }
    }
}

/// A terminal running a local shell, or attached to a remote session.
pub struct Terminal {
    term: Arc<SharedTerm>,
    control: Control,
    shared: Arc<Shared>,
    size: SizeInfo,
    /// The size the terminal has now: `size` once that has been applied, which takes a while.
    applied: Arc<Mutex<SizeInfo>>,
    theme: Theme,
    /// The theme, as the snapshot thread reads it.
    shared_theme: Arc<Mutex<Theme>>,
    pending: Arc<Mutex<Pending>>,
    snapshot: RefCell<Snapshot>,
    /// Whether the terminal was last told it has the keyboard, to spare locking it to find out.
    focused: Cell<Option<bool>>,
    pub(crate) mouse: MouseState,
    /// Where a selection would start, once the pointer moves.
    pub(crate) anchor: Option<(SelectionType, Point<Line>, Side)>,
    taps: Arc<Taps>,
}

impl Terminal {
    /// Start `shell`, or the user's shell without one, in a terminal of `size`.
    pub fn spawn(
        id: u64,
        size: SizeInfo,
        shell: Option<(String, Vec<String>)>,
        theme: Theme,
        sink: Arc<dyn Sink>,
    ) -> io::Result<Self> {
        let (core, listener) = Self::core(id, size, &sink);

        #[cfg(target_os = "linux")]
        let pty = crate::pty::spawn(shell, size.into(), id)?;
        #[cfg(not(target_os = "linux"))]
        let pty = {
            let shell = shell.map(|(program, args)| tty::Shell::new(program, args));
            let options = tty::Options { shell, ..Default::default() };
            tty::new(&options, size.into(), id)?
        };

        let pty = TappedPty::new(pty, core.taps.clone());
        let event_loop = EventLoop::new(core.term.clone(), listener, pty, false, false)?;
        let control = Control::Pty(event_loop.channel());
        event_loop.spawn();

        Self::assemble(id, size, theme, sink, core, control)
    }

    /// Attach a terminal of `size` to a remote session on `reader` and `writer`, which already
    /// have that size.
    ///
    /// Reading and writing run on `runtime`, whose time driver must be enabled. Everything else
    /// works as with [`Self::spawn`], except for how it ends: rather than [`Output::Exit`],
    /// [`Sink::session`] gets [`SessionEvent::Closed`] and the terminal stays readable.
    pub fn connect<R, W>(
        id: u64,
        size: SizeInfo,
        reader: R,
        writer: W,
        runtime: &Handle,
        theme: Theme,
        sink: Arc<dyn Sink>,
    ) -> io::Result<Self>
    where
        R: TransportReader,
        W: TransportWriter,
    {
        let (core, listener) = Self::core(id, size, &sink);

        let session_sink = sink.clone();
        let parser = Parser::new(
            core.term.clone(),
            listener,
            Arc::new(Channel::new(Limits::default())),
            Box::new(move |event| session_sink.session(id, event)),
        );
        let taps = core.taps.clone();
        let remote = driver::start(reader, writer, runtime, parser, size.into(), taps)?;

        Self::assemble(id, size, theme, sink, core, Control::Remote(remote))
    }

    /// What every terminal starts from.
    fn core(id: u64, size: SizeInfo, sink: &Arc<dyn Sink>) -> (Core, Listener) {
        let published =
            Published { snapshot: empty_snapshot(size), fresh: false, output: false, stop: false };
        let shared = Arc::new(Shared {
            published: Mutex::new(published),
            wake: Condvar::new(),
            wakeup_pending: AtomicBool::new(false),
        });
        let listener = Listener { id, sink: sink.clone(), shared: shared.clone() };

        let term = Term::new(term::Config::default(), &size, listener.clone());
        let core =
            Core { shared, term: Arc::new(FairMutex::new(term)), taps: Arc::new(Taps::default()) };
        (core, listener)
    }

    /// Start the snapshot thread for a terminal that has its program attached.
    fn assemble(
        id: u64,
        size: SizeInfo,
        theme: Theme,
        sink: Arc<dyn Sink>,
        core: Core,
        control: Control,
    ) -> io::Result<Self> {
        let Core { shared, term, taps } = core;
        let applied = Arc::new(Mutex::new(size));
        let shared_theme = Arc::new(Mutex::new(theme));
        let (thread_shared, thread_term, thread_applied, thread_theme) =
            (shared.clone(), term.clone(), applied.clone(), shared_theme.clone());
        thread::Builder::new().name("rmx-snapshot".into()).spawn(move || {
            snapshots(id, &thread_shared, &thread_term, &thread_applied, &thread_theme, &*sink);
        })?;

        Ok(Self {
            term,
            control,
            shared,
            size,
            applied,
            theme,
            shared_theme,
            pending: Arc::default(),
            snapshot: RefCell::new(empty_snapshot(size)),
            focused: Cell::new(None),
            mouse: MouseState::default(),
            anchor: None,
            taps,
        })
    }

    /// Copies of the bytes on `streams` from now on, with room for `capacity` chunks waiting.
    ///
    /// Output is what the program sent, as the connection or the PTY delivered it, before it is
    /// parsed. Input is what [`Self::write`] and [`Self::paste`] send: what was typed or pasted,
    /// not the terminal's own answers to the program. Several taps can be attached at once, and
    /// dropping one detaches it.
    ///
    /// The echo a terminal shows of what is typed, for a program that asks for one with
    /// [`TransportWriter::local_echo`], is on neither: its bytes are on the input tap already,
    /// and a log of the output should not look as if the program had sent them.
    pub fn tap(&self, streams: Streams, capacity: usize) -> Tap {
        self.taps.tap(streams, capacity)
    }

    pub fn size(&self) -> SizeInfo {
        self.size
    }

    /// Draw with `theme` from the next snapshot on, which this asks for.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        *self.shared_theme.lock() = theme;
        self.shared.output();
    }

    /// Where the shell last said it is, with OSC 7. Only remote sessions are looked at.
    pub fn working_directory(&self) -> Option<WorkingDirectory> {
        match &self.control {
            Control::Pty(_) => None,
            Control::Remote(remote) => remote.channel.working_directory(),
        }
    }

    /// Why the remote session ended, once its output has all been parsed.
    pub fn close_reason(&self) -> Option<CloseReason> {
        match &self.control {
            Control::Pty(_) => None,
            Control::Remote(remote) => remote.channel.close_reason(),
        }
    }

    /// What was last read of the terminal.
    pub fn last_snapshot(&self) -> Ref<'_, Snapshot> {
        self.snapshot.borrow()
    }

    /// Bring [`Self::last_snapshot`] up to date, never waiting for the terminal's lock. Returns
    /// whether it could be: if not, the last snapshot stands, and the caller should try again soon.
    ///
    /// Output gets here through the snapshot thread, which announces each snapshot with a wakeup.
    /// What changed on this thread, a selection or the focus, has no wakeup, and is read here.
    pub fn refresh(&self) -> bool {
        let mut snapshot = self.snapshot.borrow_mut();
        let mut published = self.shared.published.lock();
        if published.fresh {
            mem::swap(&mut *snapshot, &mut published.snapshot);
            published.fresh = false;
            self.shared.wakeup_pending.store(false, Ordering::Release);
            if published.output {
                self.shared.wake.notify_one();
            }
            return true;
        }
        drop(published);

        if self.pending.lock().busy {
            return false;
        }
        let Some(term) = self.term.try_lock_unfair() else { return false };

        let mut content = RenderableContent::new(&term, &self.theme);
        snapshot.cells.clear();
        snapshot.cells.extend(content.by_ref());
        snapshot.cursor = content.cursor();
        snapshot.size = *self.applied.lock();
        true
    }

    /// Change the size of the terminal, and tell the program in it.
    pub fn resize(&mut self, size: SizeInfo) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.control.on_resize(size.into());

        self.pending.lock().size = Some(size);
        self.apply_pending();
    }

    /// Have what is pending applied, on a thread of its own as the terminal may be busy.
    fn apply_pending(&self) {
        let mut pending = self.pending.lock();
        if pending.busy {
            return;
        }
        pending.busy = true;
        drop(pending);

        let (pending, term, applied, shared, control) = (
            self.pending.clone(),
            self.term.clone(),
            self.applied.clone(),
            self.shared.clone(),
            self.control.clone(),
        );
        thread::spawn(move || apply_pending(&pending, &term, &applied, &shared, control));
    }

    pub(crate) fn term(&self) -> MutexGuard<'_, Term<Listener>> {
        self.term.lock()
    }

    /// Tell the terminal, and the program in it if it asked, whether it has the keyboard.
    ///
    /// Returns whether that changes anything.
    pub fn set_focused(&self, focused: bool) -> bool {
        if self.focused.replace(Some(focused)) == Some(focused) {
            return false;
        }

        self.pending.lock().focused = Some(focused);
        self.apply_pending();
        true
    }

    /// The selected text.
    pub fn selection(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Type `text` in as pasted, so that the program cannot take it for keystrokes.
    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.write(encode_paste(text, bracketed));
    }

    /// The modes keys are encoded for.
    pub fn mode(&self) -> term::TermMode {
        *self.term.lock().mode()
    }

    /// Send bytes typed or pasted to the program in the terminal. Input taps get them.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if !self.taps.input.is_attached() {
            return self.control.type_in(bytes);
        }
        let bytes = into_bytes(bytes);
        self.taps.input.send_with(Direction::Input, || bytes.clone());
        self.control.send(bytes);
    }

    /// Send the program an answer to something it asked, which no input tap gets.
    pub fn answer(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.control.notify(bytes);
    }

    /// Scroll to the bottom, as typing does.
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// Answer `event`, telling the application what is left for it to do.
    pub fn handle(&self, event: Event, theme: &Theme) -> Output {
        match event {
            Event::PtyWrite(text) => self.answer(text.into_bytes()),
            Event::ColorRequest(index, format) => {
                let color = self.term.lock().colors()[index].map(Rgb);
                let color = match color {
                    Some(color) => color,
                    None if index == NamedColor::Cursor as usize => return Output::None,
                    None => theme[index],
                };
                self.answer(format(color.0).into_bytes());
            },
            Event::TextAreaSizeRequest(format) => {
                let size: WindowSize = self.size.into();
                self.answer(format(size).into_bytes());
            },
            Event::ClipboardStore(ty, text) => return Output::ClipboardStore(ty, text),
            Event::ClipboardLoad(ty, format) => return Output::ClipboardLoad(ty, format),
            Event::Title(title) => return Output::Title(title),
            Event::ResetTitle => return Output::ResetTitle,
            Event::Bell => return Output::Bell,
            Event::Exit | Event::ChildExit(_) => return Output::Exit,
            _ => (),
        }
        Output::None
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shared.published.lock().stop = true;
        self.shared.wake.notify_one();
        self.control.shutdown();
    }
}

/// A snapshot of nothing yet, with room for a terminal of `size`.
fn empty_snapshot(size: SizeInfo) -> Snapshot {
    Snapshot {
        cells: Vec::with_capacity(cell_count(&size)),
        cursor: RenderableCursor::default(),
        size,
    }
}

/// How many cells a terminal of `size` shows at most.
fn cell_count(size: &SizeInfo) -> usize {
    size.columns() * size.screen_lines()
}

/// Apply what is pending on `term`, until nothing is left.
fn apply_pending(
    pending: &Mutex<Pending>,
    term: &SharedTerm,
    applied: &Mutex<SizeInfo>,
    shared: &Shared,
    control: Control,
) {
    loop {
        let (size, focused) = {
            let mut pending = pending.lock();
            if pending.size.is_none() && pending.focused.is_none() {
                pending.busy = false;
                return;
            }
            (pending.size.take(), pending.focused.take())
        };

        let mut term = term.lock();
        if let Some(size) = size {
            term.resize(size);
            *applied.lock() = size;
        }
        let report = focused.map(|focused| {
            term.is_focused = focused;
            (focused, term.mode().contains(TermMode::FOCUS_IN_OUT))
        });
        drop(term);
        // Neither change comes with output of its own.
        shared.output();
        if let Some((focused, true)) = report {
            control.notify(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
    }
}

/// The bytes for pasting `text`.
fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // A pasted end sequence would end the paste early and run what follows it.
        let clean = text.replace(['\x1b', '\x03'], "");
        let mut bytes = Vec::with_capacity(clean.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(clean.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::encode_paste;

    #[test]
    fn plain_paste_turns_newlines_into_returns() {
        assert_eq!(encode_paste("a\nb\r\nc", false), b"a\rb\rc");
    }

    #[test]
    fn bracketed_paste_cannot_be_broken_out_of() {
        assert_eq!(encode_paste("x\x1b[201~; rm", true), b"\x1b[200~x[201~; rm\x1b[201~");
    }
}
