//! Writing what a terminal shows, and optionally what is typed into it, to a file.
//!
//! The bytes come from a [`Tap`] on the terminal, whose channel holds at most [`CAPACITY`]
//! chunks: when the disk cannot keep up, chunks are thrown away and counted, and the log says how
//! much is missing where it is. Opening and writing the file happen on a thread of the log's own,
//! so the main thread only attaches the tap and starts that thread.

use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rmx_terminal::{Dropped, Streams, Tap, Tapped, Terminal};
use rmx_ui::widgets::SessionLogging;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

/// How many chunks may wait for the disk before new ones are thrown away.
pub const CAPACITY: usize = 1024;

/// What a log is told when it starts, about the secrets it may hold.
pub fn warning(path: &Path, streams: Streams) -> String {
    let path = path.display();
    if streams.input {
        format!(
            "Logging output and typing to {path}. Typed passwords end up in this plain file too."
        )
    } else {
        format!(
            "Logging output to {path}. Secrets shown on screen end up in this plain file; typing \
             is not logged."
        )
    }
}

/// Which streams a connection's logging choice asks for, if any.
pub fn streams(choice: SessionLogging) -> Option<Streams> {
    match choice {
        SessionLogging::Off => None,
        SessionLogging::Output => Some(Streams::OUTPUT),
        SessionLogging::Everything => Some(Streams::BOTH),
    }
}

/// A new log file in `dir` for the session `label` of pane `pane`, named so that it sorts by
/// when it began.
pub fn new_path(dir: &Path, label: &str, pane: u64) -> PathBuf {
    let seconds =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |now| now.as_secs());
    let when = rmx_ui::local_time(seconds).map_or_else(
        || seconds.to_string(),
        |t| format!("{}-{:02}-{:02}-{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute),
    );
    let name: String = label
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .take(64)
        .collect();
    let name = name.trim_matches('.');
    let name = if name.is_empty() { "session" } else { name };
    dir.join(format!("{name}-{when}-{pane}.log"))
}

/// A log being written. Dropping it stops the log, once what is waiting has been written.
pub struct SessionLog {
    pub path: PathBuf,
    pub streams: Streams,
    _stop: oneshot::Sender<()>,
}

impl SessionLog {
    /// Log `streams` of `terminal` at the end of the file at `path`, which is made if need be.
    ///
    /// The log ends when this is dropped or the terminal is gone. `failed` hears why, if it ends
    /// because the file could not be written.
    pub fn start(
        terminal: &Terminal,
        path: PathBuf,
        streams: Streams,
        runtime: &Handle,
        failed: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<Self> {
        let tap = terminal.tap(streams, CAPACITY);
        Self::start_with(tap, path, streams, runtime, failed)
    }

    fn start_with(
        source: impl Source,
        path: PathBuf,
        streams: Streams,
        runtime: &Handle,
        failed: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<Self> {
        let (stop, stopped) = oneshot::channel();
        let (runtime, file) = (runtime.clone(), path.clone());
        std::thread::Builder::new().name("rmx-log".into()).spawn(move || {
            let written =
                open(&file).and_then(|out| runtime.block_on(record(source, out, stopped)));
            if let Err(err) = written {
                failed(format!("Logging to {} stopped: {err}", file.display()));
            }
        })?;
        Ok(Self { path, streams, _stop: stop })
    }
}

/// Open the log at `path` to add to it, making its directory. Only its owner may read either.
fn open(path: &Path) -> io::Result<BufWriter<File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    Ok(BufWriter::with_capacity(64 * 1024, options.open(path)?))
}

/// Where a log's bytes come from: a [`Tap`], or what a test makes up.
trait Source: Send + 'static {
    fn recv(&mut self) -> impl Future<Output = Option<Tapped>> + Send;
    fn try_recv(&mut self) -> Option<Tapped>;
    fn dropped(&self) -> Dropped;
}

impl Source for Tap {
    fn recv(&mut self) -> impl Future<Output = Option<Tapped>> + Send {
        Tap::recv(self)
    }

    fn try_recv(&mut self) -> Option<Tapped> {
        Tap::try_recv(self)
    }

    fn dropped(&self) -> Dropped {
        Tap::dropped(self)
    }
}

/// Write what `source` hands over to `out` until it ends or `stop` says so, then what is left.
/// Each time nothing more is waiting, what has been written is flushed, so the file keeps up.
async fn record(
    mut source: impl Source,
    mut out: impl Write,
    mut stop: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut noted = Dropped::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = &mut stop => None,
            next = source.recv() => next,
        };
        let Some(first) = next else { break };
        let mut next = Some(first);
        while let Some(chunk) = next {
            note_dropped(&source, &mut noted, &mut out)?;
            out.write_all(&chunk.bytes)?;
            next = source.try_recv();
        }
        note_dropped(&source, &mut noted, &mut out)?;
        out.flush()?;
    }
    while let Some(chunk) = source.try_recv() {
        note_dropped(&source, &mut noted, &mut out)?;
        out.write_all(&chunk.bytes)?;
    }
    note_dropped(&source, &mut noted, &mut out)?;
    out.flush()
}

/// Say in the log how much went missing since it last did, if anything did.
fn note_dropped(source: &impl Source, noted: &mut Dropped, out: &mut impl Write) -> io::Result<()> {
    let now = source.dropped();
    if now.bytes > noted.bytes {
        let lost = now.bytes - noted.bytes;
        write!(out, "\r\n[rmx: {lost} bytes were not logged, the disk could not keep up]\r\n")?;
    }
    *noted = now;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use rmx_terminal::{CloseReason, Direction, Sink, TermEvent, Theme, TransportReader};
    use rmx_terminal::{TransportWriter, WindowSize};
    use tokio::sync::mpsc;

    use super::*;

    /// Chunks a test hands over, and what it says was thrown away.
    struct Fake {
        chunks: mpsc::UnboundedReceiver<Tapped>,
        dropped: Arc<AtomicU64>,
    }

    impl Source for Fake {
        fn recv(&mut self) -> impl Future<Output = Option<Tapped>> + Send {
            self.chunks.recv()
        }

        fn try_recv(&mut self) -> Option<Tapped> {
            self.chunks.try_recv().ok()
        }

        fn dropped(&self) -> Dropped {
            let bytes = self.dropped.load(Ordering::SeqCst);
            Dropped { chunks: u64::from(bytes > 0), bytes }
        }
    }

    fn output(text: &str) -> Tapped {
        Tapped { direction: Direction::Output, bytes: Bytes::copy_from_slice(text.as_bytes()) }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap()
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn output_is_written_in_order_and_kept_up_to_date() {
        let (dir, runtime) = (tempfile::tempdir().unwrap(), runtime());
        let path = dir.path().join("logs/a.log");
        let (send, chunks) = mpsc::unbounded_channel();
        let fake = Fake { chunks, dropped: Arc::default() };
        let log =
            SessionLog::start_with(fake, path.clone(), Streams::OUTPUT, runtime.handle(), |_| {})
                .unwrap();

        send.send(output("one ")).unwrap();
        send.send(output("two ")).unwrap();
        wait_for("the first chunks", || read(&path) == "one two ");
        send.send(output("three")).unwrap();
        wait_for("the next one", || read(&path) == "one two three");
        drop(log);
    }

    #[test]
    fn what_was_thrown_away_is_noted_where_it_went_missing() {
        let (dir, runtime) = (tempfile::tempdir().unwrap(), runtime());
        let path = dir.path().join("a.log");
        let (send, chunks) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let fake = Fake { chunks, dropped: dropped.clone() };
        let _log =
            SessionLog::start_with(fake, path.clone(), Streams::OUTPUT, runtime.handle(), |_| {})
                .unwrap();

        send.send(output("before")).unwrap();
        wait_for("the first chunk", || read(&path) == "before");
        dropped.store(4096, Ordering::SeqCst);
        send.send(output("after")).unwrap();
        wait_for("the second chunk", || read(&path).ends_with("after"));
        assert_eq!(
            read(&path),
            "before\r\n[rmx: 4096 bytes were not logged, the disk could not keep up]\r\nafter"
        );
    }

    #[test]
    fn a_log_adds_to_the_file_it_is_given() {
        let (dir, runtime) = (tempfile::tempdir().unwrap(), runtime());
        let path = dir.path().join("a.log");
        fs::write(&path, "earlier\n").unwrap();
        let (send, chunks) = mpsc::unbounded_channel();
        let fake = Fake { chunks, dropped: Arc::default() };
        let _log =
            SessionLog::start_with(fake, path.clone(), Streams::OUTPUT, runtime.handle(), |_| {})
                .unwrap();
        send.send(output("later")).unwrap();
        wait_for("the chunk", || read(&path) == "earlier\nlater");
    }

    #[test]
    fn a_file_that_cannot_be_made_is_reported() {
        let (dir, runtime) = (tempfile::tempdir().unwrap(), runtime());
        let blocker = dir.path().join("not-a-directory");
        fs::write(&blocker, "").unwrap();
        let (_send, chunks) = mpsc::unbounded_channel();
        let fake = Fake { chunks, dropped: Arc::default() };
        let (told, heard) = std::sync::mpsc::channel();
        let path = blocker.join("a.log");
        let _log =
            SessionLog::start_with(fake, path, Streams::OUTPUT, runtime.handle(), move |why| {
                let _ = told.send(why);
            })
            .unwrap();
        let why = heard.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(why.starts_with("Logging to "), "{why}");
    }

    #[test]
    fn paths_are_safe_file_names_that_sort_by_time() {
        let path = new_path(Path::new("logs"), "web/01: prod ..", 7);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(path.starts_with("logs"));
        assert!(name.starts_with("web_01__prod_"), "{name}");
        assert!(name.ends_with("-7.log"), "{name}");
        assert!(new_path(Path::new("logs"), "..", 1).to_str().unwrap().contains("session-"));
    }

    #[test]
    fn the_choices_are_the_streams_they_say() {
        assert_eq!(streams(SessionLogging::Off), None);
        assert_eq!(streams(SessionLogging::Output), Some(Streams::OUTPUT));
        assert_eq!(streams(SessionLogging::Everything), Some(Streams::BOTH));
        assert!(warning(Path::new("a.log"), Streams::OUTPUT).contains("typing is not logged"));
        assert!(warning(Path::new("a.log"), Streams::BOTH).contains("passwords"));
    }

    struct Reader(mpsc::UnboundedReceiver<Bytes>);

    impl TransportReader for Reader {
        async fn read(&mut self) -> Result<Bytes, CloseReason> {
            self.0.recv().await.ok_or(CloseReason::Closed)
        }
    }

    struct Writer;

    impl TransportWriter for Writer {
        async fn write(&self, _: Bytes) -> Result<(), CloseReason> {
            Ok(())
        }

        async fn resize(&self, _: WindowSize) -> Result<(), CloseReason> {
            Ok(())
        }
    }

    struct Quiet;

    impl Sink for Quiet {
        fn wakeup(&self, _: u64) {}
        fn event(&self, _: u64, _: TermEvent) {}
    }

    #[test]
    fn a_terminals_output_is_logged_and_its_typing_is_not() {
        let (dir, runtime) = (tempfile::tempdir().unwrap(), runtime());
        let path = dir.path().join("a.log");
        let (send, output) = mpsc::unbounded_channel();
        let size = rmx_render::SizeInfo::new(800., 480., 10., 20., 0., 0., false);
        let terminal = Terminal::connect(
            1,
            size,
            Reader(output),
            Writer,
            runtime.handle(),
            Theme::dark(),
            Arc::new(Quiet),
        )
        .unwrap();
        let log =
            SessionLog::start(&terminal, path.clone(), Streams::OUTPUT, runtime.handle(), |_| {})
                .unwrap();

        terminal.write(&b"hunter2\r"[..]);
        send.send(Bytes::from_static(b"$ ls\r\nfile\r\n")).unwrap();
        wait_for("the output", || read(&path) == "$ ls\r\nfile\r\n");
        assert!(!read(&path).contains("hunter2"));
        drop(log);
        drop(terminal);
    }
}
