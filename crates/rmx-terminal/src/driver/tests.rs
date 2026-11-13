use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{self, Term};
use parking_lot::Mutex;
use rmx_render::SizeInfo;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc as tokio_mpsc;

use super::*;
use crate::tap::{Direction, Dropped, Streams, Tap};
use crate::terminal::{Sink, Terminal};
use crate::theme::Theme;

const LIMIT: Duration = Duration::from_micros(500);

/// The time this thread has spent on a CPU, which a busy machine does not inflate.
fn cpu_time() -> Duration {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) }, 0);
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

fn size(columns: usize, lines: usize) -> SizeInfo {
    SizeInfo::new(columns as f32 * 8., lines as f32 * 16., 8., 16., 0., 0., false)
}

fn runtime() -> Runtime {
    Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap()
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A listener for driving a [`Parser`] without a [`Terminal`].
#[derive(Clone, Default)]
struct Events {
    wakeups: Arc<AtomicUsize>,
    titles: Arc<Mutex<Vec<String>>>,
}

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => {
                self.wakeups.fetch_add(1, Ordering::SeqCst);
            },
            Event::Title(title) => self.titles.lock().push(title),
            _ => (),
        }
    }
}

type Shared = Arc<FairMutex<Term<Events>>>;

struct Rig {
    parser: Parser<Events>,
    term: Shared,
    channel: Arc<Channel>,
    events: Events,
    session: Arc<Mutex<Vec<SessionEvent>>>,
}

fn rig(columns: usize, lines: usize, limits: Limits) -> Rig {
    let events = Events::default();
    let term = Arc::new(FairMutex::new(Term::new(
        term::Config::default(),
        &size(columns, lines),
        events.clone(),
    )));
    let channel = Arc::new(Channel::new(limits));
    let session = Arc::new(Mutex::new(Vec::new()));
    let log = session.clone();
    let parser = Parser::new(
        term.clone(),
        events.clone(),
        channel.clone(),
        Box::new(move |event| log.lock().push(event)),
    );
    Rig { parser, term, channel, events, session }
}

impl Rig {
    /// Parse everything queued, as batches.
    fn drain(&mut self) -> Vec<Batch> {
        let mut batches = Vec::new();
        while self.channel.inbox.lock().queued > 0 {
            assert!(matches!(self.parser.next_work(), Work::Parse));
            batches.push(self.parser.parse_batch());
        }
        batches
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<Batch> {
        assert!(self.channel.push(Bytes::copy_from_slice(bytes), false));
        self.drain()
    }
}

fn line_text<L>(term: &Term<L>, line: i32) -> String {
    let columns = term.columns();
    let grid = term.grid();
    (0..columns)
        .map(|column| grid[Line(line)][Column(column)].c)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn cells(term: &Term<Events>) -> (Vec<alacritty_terminal::term::cell::Cell>, usize, i32) {
    let grid = term.grid();
    let cursor = grid.cursor.point;
    (
        grid.display_iter().map(|indexed| indexed.cell.clone()).collect(),
        cursor.column.0,
        cursor.line.0,
    )
}

/// Text with escapes throughout, as `ls --color` and the like produce.
fn colored_text(bytes: usize) -> Vec<u8> {
    let mut stream = Vec::with_capacity(bytes + 128);
    let mut line = 0u64;
    while stream.len() < bytes {
        stream.extend_from_slice(
            format!(
                "\x1b[1;3{}m{line:>8}\x1b[0m some words of output 中文 \
                 \x1b[38;5;208m0x{line:x}\x1b[m\r\n",
                line % 8
            )
            .as_bytes(),
        );
        line += 1;
    }
    stream
}

#[test]
fn batches_hold_the_lock_within_the_bound() {
    const BYTES: usize = 4 * 1024 * 1024;
    // Sequences that redraw the screen take milliseconds each without optimizations.
    const SCREEN_WIDE_BYTES: usize = if cfg!(debug_assertions) { 4 * 1024 } else { 64 * 1024 };

    let plain: Vec<u8> = b"the quick brown fox jumps over the lazy dog 0123456789\r\n"
        .iter()
        .cycle()
        .take(BYTES)
        .copied()
        .collect();
    let wide: Vec<u8> =
        "中文字符输出的一行内容，宽字符占两列。\r\n".bytes().cycle().take(BYTES).collect();
    let scroll = vec![b'\n'; BYTES];
    // The worst there is: every few bytes redraws the whole of a 4K screen.
    let clear = b"\x1b[2J".iter().cycle().take(SCREEN_WIDE_BYTES).copied().collect::<Vec<u8>>();
    let delete = b"\x1b[1;1H\x1b[M\x1b[L"
        .iter()
        .cycle()
        .take(SCREEN_WIDE_BYTES)
        .copied()
        .collect::<Vec<u8>>();
    // A long sync update must not turn into one long hold at its end.
    let mut sync = b"\x1b[?2026h".to_vec();
    sync.extend(colored_text(BYTES));
    sync.extend_from_slice(b"\x1b[?2026l");

    let cases: [(&str, Vec<u8>); 7] = [
        ("plain", plain),
        ("colored", colored_text(BYTES)),
        ("wide", wide),
        ("scroll", scroll),
        ("clear", clear),
        ("delete-line", delete),
        ("sync-update", sync),
    ];
    for (name, stream) in cases {
        let mut rig = rig(400, 120, Limits::default());
        // Growing the history is the one thing a batch cannot split, see the test below.
        {
            let mut term = rig.term.lock();
            term.grid_mut().update_history(2000);
            term.grid_mut().initialize_all();
        }
        for chunk in stream.chunks(32 * 1024) {
            rig.channel.push(Bytes::copy_from_slice(chunk), false);
        }

        let (mut most, mut most_wall, mut batches) = (Duration::ZERO, Duration::ZERO, 0);
        loop {
            if rig.channel.inbox.lock().queued == 0 {
                break;
            }
            assert!(matches!(rig.parser.next_work(), Work::Parse));
            let before = cpu_time();
            let batch = rig.parser.parse_batch();
            let spent = cpu_time() - before;
            assert!(
                batch.parsed > 0 && batch.parsed < MAX_LOCKED_BYTES + MAX_SLICE,
                "{name}: {batch:?}"
            );
            most = most.max(spent);
            most_wall = most_wall.max(batch.held);
            batches += 1;
        }
        println!(
            "{name:>12}: {} bytes in {batches} batches, longest hold {most:?} on CPU, \
             {most_wall:?} on the clock",
            stream.len()
        );
        // One sequence cannot be split, and without optimizations one that redraws 48000 cells
        // takes milliseconds by itself. Those two are for `cargo test --release` to judge.
        // A shared CI machine's clock says nothing about the bound either.
        let judged = std::env::var_os("CI").is_none();
        if judged && (!cfg!(debug_assertions) || !matches!(name, "clear" | "delete-line")) {
            assert!(most <= LIMIT, "{name}: a batch held the lock for {most:?} of CPU time");
        }
    }
}

/// `alacritty_terminal` allocates scrollback 1000 rows at a time, under the lock. There is no way
/// to split that from outside, but it stops happening once the history is full.
#[test]
fn only_scrollback_growth_exceeds_the_bound() {
    let mut rig = rig(400, 120, Limits::default());
    let lines: Vec<u8> =
        b"a line of output\r\n".iter().cycle().take(18 * 12_000).copied().collect();
    rig.channel.push(Bytes::from(lines), false);

    let (mut over, mut most) = (0, Duration::ZERO);
    while rig.channel.inbox.lock().queued > 0 {
        assert!(matches!(rig.parser.next_work(), Work::Parse));
        let before = cpu_time();
        rig.parser.parse_batch();
        let spent = cpu_time() - before;
        if spent > LIMIT {
            over += 1;
        }
        most = most.max(spent);
    }
    println!("scrollback growth: {over} batches over the bound, longest {most:?} on CPU");
    assert!(over <= 11, "{over} batches over the bound");
}

#[test]
fn a_sync_update_stays_hidden_until_it_ends_or_times_out() {
    let mut rig = rig(80, 24, Limits::default());

    rig.feed(b"\x1b[?2026hhidden");
    assert_eq!(rig.events.wakeups.load(Ordering::SeqCst), 0);

    rig.feed(b"\x1b[?2026l");
    assert_eq!(rig.events.wakeups.load(Ordering::SeqCst), 1);

    // No end this time, and nothing else to do but wait for the timeout.
    rig.feed(b"\x1b[?2026hhidden");
    assert_eq!(rig.events.wakeups.load(Ordering::SeqCst), 1);
    let started = Instant::now();
    assert!(matches!(rig.parser.next_work(), Work::SyncExpired));
    assert!(started.elapsed() >= Duration::from_millis(120), "expired early");
}

#[test]
fn a_flood_during_a_sync_update_is_shown_after_the_timeout() {
    let mut rig = rig(80, 24, Limits::default());
    rig.feed(b"\x1b[?2026hhidden");
    rig.parser.sync_deadline = Some(Instant::now() - Duration::from_millis(1));
    rig.feed(b"more");
    assert_eq!(rig.events.wakeups.load(Ordering::SeqCst), 1);
    assert_eq!(rig.parser.sync_deadline, None);
}

#[test]
fn every_split_parses_the_same() {
    let stream = "中文 𝄞😀 \x1b[1;31mred\x1b[0m\x1b]0;the title\x07\x1b[2;5Hmoved\x1b[?25l\
                  \x1b]7;file://h/p\x1b\\\x1b[?2004h\r\nend"
        .as_bytes();
    let parse = |chunks: &[&[u8]]| {
        let mut rig = rig(80, 24, Limits::default());
        for chunk in chunks {
            rig.feed(chunk);
        }
        let dump = cells(&rig.term.lock());
        let title = rig.events.titles.lock().last().cloned();
        let cwd = rig.channel.working_directory();
        (dump, title, cwd, *rig.term.lock().mode())
    };

    let whole = parse(&[stream]);
    assert_eq!(whole.1.as_deref(), Some("the title"));
    assert_eq!(whole.2.as_ref().map(|cwd| cwd.path.as_str()), Some("/p"));

    for split in 1..stream.len() {
        let (head, tail) = stream.split_at(split);
        assert!(parse(&[head, tail]) == whole, "split at {split} parsed differently");
    }
    let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
    assert!(parse(&bytewise) == whole, "single bytes parsed differently");
}

/// Everything the reader hands over goes through the real tasks, in order.
struct ScriptedReader(tokio_mpsc::UnboundedReceiver<Result<Bytes, CloseReason>>);

impl TransportReader for ScriptedReader {
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        match self.0.recv().await {
            Some(result) => result,
            None => Err(CloseReason::Closed),
        }
    }
}

#[derive(Clone, Default)]
struct Writes {
    bytes: Arc<Mutex<Vec<u8>>>,
    sizes: Arc<Mutex<Vec<WindowSize>>>,
}

impl TransportWriter for Writes {
    async fn write(&self, bytes: Bytes) -> Result<(), CloseReason> {
        self.bytes.lock().extend_from_slice(&bytes);
        Ok(())
    }

    async fn resize(&self, size: WindowSize) -> Result<(), CloseReason> {
        self.sizes.lock().push(size);
        Ok(())
    }
}

/// Reads as fast as it is asked to, forever.
struct FloodReader {
    reads: Arc<AtomicU64>,
    chunk: Bytes,
}

impl TransportReader for FloodReader {
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        Ok(self.chunk.clone())
    }
}

fn flood_chunk(bytes: usize) -> Bytes {
    Bytes::from(b"flood\r\n".iter().cycle().take(bytes).copied().collect::<Vec<u8>>())
}

fn small_window() -> WindowSize {
    WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 }
}

#[test]
fn a_flood_keeps_reading_past_a_stalled_consumer() {
    const MAX_QUEUED: usize = 1024 * 1024;
    const CHUNK: usize = 64 * 1024;

    let runtime = runtime();
    let rig = rig(80, 24, Limits { max_queued: MAX_QUEUED, stall: Duration::from_millis(40) });
    let (term, channel, session) = (rig.term.clone(), rig.channel.clone(), rig.session.clone());
    let reads = Arc::new(AtomicU64::new(0));
    let reader = FloodReader { reads: reads.clone(), chunk: flood_chunk(CHUNK) };
    let remote = start(
        reader,
        Writes::default(),
        runtime.handle(),
        rig.parser,
        small_window(),
        Arc::default(),
    )
    .unwrap();

    // The slow consumer holds the terminal, far longer than the parser is waited for.
    let held = term.lock();
    thread::sleep(Duration::from_millis(100));
    let before = reads.load(Ordering::SeqCst);
    let started = Instant::now();
    let mut most_queued = 0;
    while started.elapsed() < Duration::from_millis(500) {
        most_queued = most_queued.max(channel.inbox.lock().queued);
        thread::sleep(Duration::from_millis(5));
    }
    let during_hold = reads.load(Ordering::SeqCst) - before;
    println!("{during_hold} reads while held, at most {most_queued} bytes queued");
    assert!(during_hold > 100, "reading stopped with the parser stalled: {during_hold} reads");
    assert!(most_queued <= MAX_QUEUED + CHUNK, "queued {most_queued} bytes");
    drop(held);

    // Parsing resumes, and says what it lost.
    wait_for("the discard to be reported", || {
        session
            .lock()
            .iter()
            .any(|event| matches!(event, SessionEvent::OutputDiscarded(bytes) if *bytes > 0))
    });
    wait_for("the parser to catch up", || channel.inbox.lock().queued < MAX_QUEUED);
    assert_eq!(line_text(&term.lock(), 0).chars().take(1).collect::<String>().len(), 1);
    drop(remote);
    channel.stop();
}

#[test]
fn a_slow_parser_loses_nothing_while_it_makes_progress() {
    const LINES: u64 = 120_000;
    const MAX_QUEUED: usize = 128 * 1024;

    let mut stream = Vec::new();
    for line in 0..LINES {
        stream.extend_from_slice(format!("{line}\r\n").as_bytes());
    }
    let total = stream.len();
    let (tx, rx) = tokio_mpsc::unbounded_channel();
    for chunk in stream.chunks(4096) {
        tx.send(Ok(Bytes::copy_from_slice(chunk))).unwrap();
    }
    tx.send(Err(CloseReason::Exited(0))).unwrap();

    let runtime = runtime();
    let rig = rig(80, 24, Limits { max_queued: MAX_QUEUED, stall: Duration::from_secs(30) });
    let (term, channel, session) = (rig.term.clone(), rig.channel.clone(), rig.session.clone());
    let remote = start(
        ScriptedReader(rx),
        Writes::default(),
        runtime.handle(),
        rig.parser,
        small_window(),
        Arc::default(),
    )
    .unwrap();

    // A consumer that keeps taking the lock, like the snapshot thread does: reading has to pause
    // for it, never to throw anything away.
    let mut most_queued = 0;
    while channel.close_reason().is_none() {
        let held = term.lock();
        let queued = channel.inbox.lock().queued;
        most_queued = most_queued.max(queued);
        thread::sleep(Duration::from_micros(300));
        drop(held);
        thread::sleep(Duration::from_micros(100));
    }
    assert!(most_queued <= MAX_QUEUED + 4096, "queued {most_queued} bytes");
    assert!(most_queued > 0);

    let events = session.lock().clone();
    assert_eq!(events, [SessionEvent::Closed(CloseReason::Exited(0))], "out of {total} bytes");
    let term = term.lock();
    assert_eq!(line_text(&term, 22), format!("{}", LINES - 1), "output was reordered or lost");
    assert_eq!(line_text(&term, 21), format!("{}", LINES - 2));
    drop(remote);
}

#[derive(Default)]
struct SinkLog {
    wakeups: AtomicUsize,
    events: Mutex<Vec<Event>>,
    session: Mutex<Vec<SessionEvent>>,
}

impl Sink for SinkLog {
    fn wakeup(&self, _id: u64) {
        self.wakeups.fetch_add(1, Ordering::SeqCst);
    }

    fn event(&self, _id: u64, event: Event) {
        self.events.lock().push(event);
    }

    fn session(&self, _id: u64, event: SessionEvent) {
        self.session.lock().push(event);
    }
}

struct Session {
    terminal: Terminal,
    sink: Arc<SinkLog>,
    writes: Writes,
    output: tokio_mpsc::UnboundedSender<Result<Bytes, CloseReason>>,
    _runtime: Runtime,
}

fn connect() -> Session {
    let runtime = runtime();
    let (output, rx) = tokio_mpsc::unbounded_channel();
    let writes = Writes::default();
    let sink = Arc::new(SinkLog::default());
    let terminal = Terminal::connect(
        1,
        size(80, 24),
        ScriptedReader(rx),
        writes.clone(),
        runtime.handle(),
        Theme::dark(),
        sink.clone(),
    )
    .unwrap();
    Session { terminal, sink, writes, output, _runtime: runtime }
}

impl Session {
    fn send(&self, bytes: &[u8]) {
        self.output.send(Ok(Bytes::copy_from_slice(bytes))).unwrap();
    }
}

#[test]
fn resize_reaches_the_terminal_and_the_transport() {
    let mut session = connect();
    session.send(b"before resize");

    let wide = size(120, 40);
    session.terminal.resize(size(100, 30));
    session.terminal.resize(wide);

    wait_for("the transport to hear of the size", || {
        session.writes.sizes.lock().last().is_some_and(|size| size.num_cols == 120)
    });
    wait_for("the terminal to take the size", || {
        session.terminal.refresh() && session.terminal.last_snapshot().size == wide
    });
    let sizes = session.writes.sizes.lock().clone();
    assert!(sizes.len() <= 2, "{} resizes for 2 changes", sizes.len());
    assert_eq!(sizes.last().map(|size| size.num_lines), Some(40));
    assert_eq!(session.terminal.term().columns(), 120);
}

#[test]
fn a_clean_close_parses_everything_and_stays_readable() {
    let session = connect();
    session.send(b"\x1b]7;file://box/srv/www\x07");
    session.send(b"last words of the shell");
    session.output.send(Err(CloseReason::Disconnected("connection reset".into()))).unwrap();

    wait_for("the close to be reported", || session.terminal.close_reason().is_some());
    let reason = CloseReason::Disconnected("connection reset".into());
    assert_eq!(session.terminal.close_reason().as_ref(), Some(&reason));

    let directory = WorkingDirectory { host: Some("box".into()), path: "/srv/www".into() };
    assert_eq!(session.terminal.working_directory().as_ref(), Some(&directory));
    assert_eq!(
        *session.sink.session.lock(),
        [SessionEvent::WorkingDirectory(directory), SessionEvent::Closed(reason)],
    );

    assert_eq!(line_text(&session.terminal.term(), 0), "last words of the shell");
    assert!(session.sink.events.lock().iter().all(|event| !matches!(event, Event::Exit)));

    wait_for("the last snapshot", || {
        session.terminal.refresh() && !session.terminal.last_snapshot().cells.is_empty()
    });

    // Everything still goes through, to nowhere.
    session.terminal.write(&b"too late"[..]);
    session.terminal.paste("too late");
    assert_eq!(session.terminal.selection(), None);
}

#[test]
fn a_write_failure_ends_the_session() {
    struct Failing;

    impl TransportWriter for Failing {
        async fn write(&self, _: Bytes) -> Result<(), CloseReason> {
            Err(CloseReason::Disconnected("broken pipe".into()))
        }

        async fn resize(&self, _: WindowSize) -> Result<(), CloseReason> {
            Ok(())
        }
    }

    let runtime = runtime();
    let (_output, rx) = tokio_mpsc::unbounded_channel();
    let sink = Arc::new(SinkLog::default());
    let terminal = Terminal::connect(
        1,
        size(80, 24),
        ScriptedReader(rx),
        Failing,
        runtime.handle(),
        Theme::dark(),
        sink.clone(),
    )
    .unwrap();

    terminal.write(&b"x"[..]);
    wait_for("the failure to end the session", || terminal.close_reason().is_some());
    assert_eq!(terminal.close_reason(), Some(CloseReason::Disconnected("broken pipe".into())));
}

#[test]
fn input_and_answers_reach_the_transport_in_order() {
    let session = connect();
    session.terminal.write(&b"first "[..]);
    session.terminal.write(b"second ".to_vec());
    session.send(b"\x1b]0;a title\x07\x1b[6n");

    let theme = Theme::dark();
    wait_for("the cursor report", || {
        session.sink.events.lock().iter().any(|event| matches!(event, Event::PtyWrite(_)))
    });
    let events: Vec<Event> = std::mem::take(&mut *session.sink.events.lock());
    assert!(events.iter().any(|event| matches!(event, Event::Title(title) if title == "a title")));
    for event in events {
        session.terminal.handle(event, &theme);
    }

    wait_for("the answer to be sent", || session.writes.bytes.lock().ends_with(b"\x1b[1;1R"));
    assert_eq!(&*session.writes.bytes.lock(), b"first second \x1b[1;1R");
}

#[test]
fn wakeups_collapse_until_the_snapshot_is_taken() {
    let session = connect();
    for _ in 0..50 {
        session.send(b"some output\r\n");
    }
    wait_for("the first wakeup", || session.sink.wakeups.load(Ordering::SeqCst) >= 1);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(session.sink.wakeups.load(Ordering::SeqCst), 1, "wakeups were not collapsed");

    assert!(session.terminal.refresh());
    session.send(b"more\r\n");
    wait_for("the next wakeup", || session.sink.wakeups.load(Ordering::SeqCst) == 2);
}

#[test]
fn terminal_can_move_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<Terminal>();
}

/// The shape of `rmx-ssh`'s `ShellReader` and `ShellWriter`, read off its source, and the adapter
/// that application code will have. Kept here as this crate does not depend on `rmx-ssh`.
mod shell_adapter {
    use super::*;

    #[derive(Clone, Debug)]
    pub enum ShellExit {
        Status(u32),
        Signal { name: String, message: String },
        Disconnected(String),
        Closed,
    }

    pub struct ShellReader {
        pub rx: tokio_mpsc::UnboundedReceiver<Bytes>,
        pub exit: Option<ShellExit>,
    }

    impl ShellReader {
        pub async fn recv(&mut self) -> Option<Bytes> {
            self.rx.recv().await
        }

        pub fn exit(&self) -> Option<&ShellExit> {
            self.exit.as_ref()
        }
    }

    pub struct Adapter(pub ShellReader);

    impl TransportReader for Adapter {
        async fn read(&mut self) -> Result<Bytes, CloseReason> {
            match self.0.recv().await {
                Some(bytes) => Ok(bytes),
                None => Err(match self.0.exit() {
                    Some(ShellExit::Status(status)) => CloseReason::Exited(*status),
                    Some(ShellExit::Signal { name, message }) => {
                        CloseReason::Killed { signal: name.clone(), message: message.clone() }
                    },
                    Some(ShellExit::Disconnected(reason)) => {
                        CloseReason::Disconnected(reason.clone())
                    },
                    Some(ShellExit::Closed) | None => CloseReason::Closed,
                }),
            }
        }
    }
}

#[test]
fn a_shell_shaped_transport_reports_how_it_ended() {
    use shell_adapter::{Adapter, ShellExit, ShellReader};

    let signal = ShellExit::Signal { name: "TERM".into(), message: "gone".into() };
    let endings = [
        (ShellExit::Status(3), CloseReason::Exited(3)),
        (signal, CloseReason::Killed { signal: "TERM".into(), message: "gone".into() }),
        (ShellExit::Disconnected("reset".into()), CloseReason::Disconnected("reset".into())),
        (ShellExit::Closed, CloseReason::Closed),
    ];
    for (exit, reason) in endings {
        let runtime = runtime();
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let terminal = Terminal::connect(
            9,
            size(80, 24),
            Adapter(ShellReader { rx, exit: Some(exit) }),
            Writes::default(),
            runtime.handle(),
            Theme::dark(),
            Arc::new(SinkLog::default()),
        )
        .unwrap();

        tx.send(Bytes::from_static(b"bye")).unwrap();
        drop(tx);
        wait_for("the end", || terminal.close_reason().is_some());
        assert_eq!(terminal.close_reason(), Some(reason));
        assert_eq!(line_text(&terminal.term(), 0), "bye");
    }
}

/// Everything `tap` hands over in `direction`, until `expected` bytes have come.
fn collect(tap: &mut Tap, direction: Direction, expected: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    wait_for("the tapped bytes", || {
        while let Some(tapped) = tap.try_recv() {
            if tapped.direction == direction {
                bytes.extend_from_slice(&tapped.bytes);
            }
        }
        bytes.len() >= expected
    });
    bytes
}

#[test]
fn an_output_tap_gets_every_byte_in_order() {
    let session = connect();
    let mut tap = session.terminal.tap(Streams::OUTPUT, 4096);
    let mut expected = Vec::new();
    for line in 0..1000 {
        let chunk = format!("line {line}\r\n");
        expected.extend_from_slice(chunk.as_bytes());
        session.send(chunk.as_bytes());
    }

    assert_eq!(collect(&mut tap, Direction::Output, expected.len()), expected);
    assert_eq!(tap.dropped(), Dropped::default());
}

#[test]
fn an_input_tap_gets_what_is_typed_and_pasted_but_not_answers() {
    let session = connect();
    let mut tap = session.terminal.tap(Streams::INPUT, 64);
    session.terminal.write(&b"ls\r"[..]);
    session.terminal.paste("echo hi\n");
    session.send(b"\x1b[6n");

    wait_for("the cursor report", || {
        session.sink.events.lock().iter().any(|event| matches!(event, Event::PtyWrite(_)))
    });
    for event in std::mem::take(&mut *session.sink.events.lock()) {
        session.terminal.handle(event, &Theme::dark());
    }
    wait_for("the answer to be sent", || session.writes.bytes.lock().ends_with(b"\x1b[1;1R"));

    assert_eq!(collect(&mut tap, Direction::Input, 11), b"ls\recho hi\r");
    assert_eq!(tap.try_recv(), None, "the answer was tapped as input");
}

#[test]
fn a_full_tap_drops_and_counts_without_holding_up_the_session() {
    let session = connect();
    let mut tap = session.terminal.tap(Streams::OUTPUT, 2);
    for line in 0..100 {
        session.send(format!("{line:02}\r\n").as_bytes());
    }

    wait_for("the last line to be parsed", || line_text(&session.terminal.term(), 22) == "99");
    assert_eq!(tap.dropped(), Dropped { chunks: 98, bytes: 98 * 4 });
    assert_eq!(tap.try_recv().map(|tapped| tapped.bytes), Some(Bytes::from_static(b"00\r\n")));
    assert_eq!(tap.try_recv().map(|tapped| tapped.bytes), Some(Bytes::from_static(b"01\r\n")));
    assert_eq!(tap.try_recv(), None);
}

#[test]
fn a_tap_ends_once_the_terminal_is_gone() {
    let session = connect();
    let mut tap = session.terminal.tap(Streams::BOTH, 16);
    session.send(b"bye");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        while let Some(tapped) = tap.blocking_recv() {
            bytes.extend_from_slice(&tapped.bytes);
        }
        bytes
    });
    wait_for("the output to be parsed", || line_text(&session.terminal.term(), 0) == "bye");
    drop(session);
    assert_eq!(reader.join().unwrap(), b"bye");
}

#[test]
fn a_local_shell_is_tapped_both_ways() {
    let script = "read line; echo \"got $line\"; read wait";
    let shell = ("sh".to_owned(), vec!["-c".to_owned(), script.to_owned()]);
    let sink = Arc::new(SinkLog::default());
    let terminal = Terminal::spawn(1, size(80, 24), Some(shell), Theme::dark(), sink).unwrap();
    let mut tap = terminal.tap(Streams::BOTH, 256);
    terminal.write(&b"tapped\r"[..]);

    let (mut input, mut output) = (Vec::new(), String::new());
    wait_for("the shell's answer", || {
        while let Some(tapped) = tap.try_recv() {
            match tapped.direction {
                Direction::Input => input.extend_from_slice(&tapped.bytes),
                Direction::Output => output.push_str(&String::from_utf8_lossy(&tapped.bytes)),
            }
        }
        output.contains("got tapped")
    });
    assert_eq!(input, b"tapped\r");
}
