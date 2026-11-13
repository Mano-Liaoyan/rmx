//! Telnet over any byte stream, so a session can go through an SSH tunnel.
//!
//! Negotiates ECHO, SGA, TTYPE, NAWS and BINARY and refuses everything else. [`connect`] offers
//! those at once and does not wait for the server: anything received afterwards is handled as
//! it arrives, and a server that refuses them all still gets a working byte stream. One that
//! refuses ECHO has the terminal show what is typed, through
//! [`TransportWriter::local_echo`].
//!
//! # Backpressure
//!
//! The reader only reads the stream when asked for output. The writer hands bytes to a task over
//! a bounded channel, so a write waits while the server is not reading. Replies to negotiation
//! come from the reader but must not wait for the writer, or a stalled write would stop the
//! output that could drain it: they go into a buffer of their own, and a server that lets that
//! grow past a fixed size, which no real negotiation comes near, is disconnected.

mod engine;

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::{Bytes, BytesMut};
use rmx_terminal::{CloseReason, TransportReader, TransportWriter, WindowSize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf, split};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc};
use tokio::task::AbortHandle;

use engine::Engine;

const READ_SIZE: usize = 16 * 1024;

/// Replies waiting for the writer task above this size end the session.
const MAX_QUEUED_REPLIES: usize = 64 * 1024;

const FLOODED: &str = "the telnet server sent more negotiation than it reads replies to";

/// What a session starts with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelnetOptions {
    /// Reported to the server as the terminal type.
    pub term: String,
    /// Columns, until the first [`TelnetWriter`] resize.
    pub cols: u16,
    /// Rows, until the first [`TelnetWriter`] resize.
    pub rows: u16,
    /// How many writes may wait for the server. At least 1 is used.
    pub write_queue: usize,
}

impl Default for TelnetOptions {
    fn default() -> Self {
        Self { term: String::from("xterm-256color"), cols: 80, rows: 24, write_queue: 32 }
    }
}

struct Shared {
    state: Mutex<State>,
    /// There are replies for the writer task.
    wake: Notify,
}

struct State {
    engine: Engine,
    /// Why the writer task stopped.
    failure: Option<String>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take_replies(&self) -> Option<Bytes> {
        let replies = self.lock().engine.take_replies();
        (!replies.is_empty()).then_some(replies)
    }

    fn failure(&self) -> CloseReason {
        let failure = self.lock().failure.clone();
        CloseReason::Disconnected(failure.unwrap_or_else(|| String::from("connection closed")))
    }
}

/// Starts a session over `stream` and returns the pair for
/// [`Terminal::connect`](rmx_terminal::Terminal::connect).
///
/// Does no I/O itself: `runtime` runs a task that writes to the stream.
pub fn connect<S>(
    stream: S,
    options: TelnetOptions,
    runtime: &Handle,
) -> (TelnetReader<S>, TelnetWriter)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let TelnetOptions { term, cols, rows, write_queue } = options;
    let (read, write) = split(stream);
    let (input, input_rx) = mpsc::channel(write_queue.max(1));
    let shared = Arc::new(Shared {
        state: Mutex::new(State { engine: Engine::new(term, cols, rows), failure: None }),
        wake: Notify::new(),
    });
    shared.wake.notify_one();

    let task = runtime.spawn(write_task(write, input_rx, Arc::clone(&shared)));
    let reader = TelnetReader {
        stream: read,
        buffer: vec![0; READ_SIZE].into_boxed_slice(),
        data: BytesMut::new(),
        shared: Arc::clone(&shared),
    };
    (reader, TelnetWriter { input, shared, task: task.abort_handle() })
}

async fn write_task<S: AsyncWrite>(
    stream: WriteHalf<S>,
    input: mpsc::Receiver<Bytes>,
    shared: Arc<Shared>,
) {
    if let Err(error) = write_loop(stream, input, &shared).await {
        shared.lock().failure = Some(error.to_string());
    }
}

async fn write_loop<S: AsyncWrite>(
    mut stream: WriteHalf<S>,
    mut input: mpsc::Receiver<Bytes>,
    shared: &Shared,
) -> io::Result<()> {
    loop {
        tokio::select! {
            biased;
            () = shared.wake.notified() => {},
            bytes = input.recv() => match bytes {
                Some(bytes) => stream.write_all(&bytes).await?,
                None => return Ok(()),
            },
        }
        while let Some(replies) = shared.take_replies() {
            stream.write_all(&replies).await?;
        }
        stream.flush().await?;
    }
}

/// The output half of a session.
pub struct TelnetReader<S> {
    stream: ReadHalf<S>,
    buffer: Box<[u8]>,
    data: BytesMut,
    shared: Arc<Shared>,
}

impl<S> TransportReader for TelnetReader<S>
where
    S: AsyncRead + Send + 'static,
{
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        loop {
            let read = self
                .stream
                .read(&mut self.buffer)
                .await
                .map_err(|error| CloseReason::Disconnected(error.to_string()))?;
            if read == 0 {
                return Err(CloseReason::Closed);
            }

            self.data.reserve(read);
            let queued = {
                let mut state = self.shared.lock();
                state.engine.feed(&self.buffer[..read], &mut self.data);
                state.engine.queued_replies()
            };
            if queued > MAX_QUEUED_REPLIES {
                return Err(CloseReason::Disconnected(String::from(FLOODED)));
            }
            if queued > 0 {
                self.shared.wake.notify_one();
            }
            if !self.data.is_empty() {
                return Ok(self.data.split().freeze());
            }
        }
    }
}

/// The input half of a session. Dropping it stops writing to the stream.
pub struct TelnetWriter {
    input: mpsc::Sender<Bytes>,
    shared: Arc<Shared>,
    task: AbortHandle,
}

impl Drop for TelnetWriter {
    fn drop(&mut self) {
        // A server that stopped reading would otherwise hold the task, and the stream, forever.
        self.task.abort();
    }
}

impl TransportWriter for TelnetWriter {
    async fn write(&self, bytes: Bytes) -> Result<(), CloseReason> {
        let encoded = self.shared.lock().engine.encode_input(bytes);
        self.input.send(encoded).await.map_err(|_| self.shared.failure())
    }

    async fn resize(&self, size: WindowSize) -> Result<(), CloseReason> {
        if self.input.is_closed() {
            return Err(self.shared.failure());
        }
        let queued = {
            let mut state = self.shared.lock();
            state.engine.resize(size.num_cols, size.num_lines);
            state.engine.queued_replies()
        };
        if queued > MAX_QUEUED_REPLIES {
            return Err(CloseReason::Disconnected(String::from(FLOODED)));
        }
        if queued > 0 {
            self.shared.wake.notify_one();
        }
        Ok(())
    }

    /// Whether the server refused ECHO, or gave it up.
    fn local_echo(&self) -> bool {
        self.shared.lock().engine.echoes_locally()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rmx_render::SizeInfo;
    use rmx_terminal::{Sink, TermEvent, Terminal, Theme};
    use tokio::io::{DuplexStream, duplex};
    use tokio::time::{sleep, timeout};

    use super::engine::*;
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn start(capacity: usize) -> (TelnetReader<DuplexStream>, TelnetWriter, DuplexStream) {
        let (near, far) = duplex(capacity);
        let options = TelnetOptions { cols: 100, rows: 30, ..TelnetOptions::default() };
        let (reader, writer) = connect(near, options, &Handle::current());
        (reader, writer, far)
    }

    async fn read_exactly(far: &mut DuplexStream, len: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        timeout(WAIT, far.read_exact(&mut bytes)).await.expect("nothing to read").unwrap();
        bytes
    }

    fn size(cols: u16, rows: u16) -> WindowSize {
        WindowSize { num_cols: cols, num_lines: rows, cell_width: 8, cell_height: 16 }
    }

    struct NoSink;

    impl Sink for NoSink {
        fn wakeup(&self, _: u64) {}

        fn event(&self, _: u64, _: TermEvent) {}
    }

    /// A terminal on a Telnet session, and the server's end of it after the opening offers.
    async fn terminal() -> (Terminal, DuplexStream) {
        let (near, mut far) = duplex(4096);
        let (reader, writer) = connect(near, TelnetOptions::default(), &Handle::current());
        let offers = Engine::new("xterm-256color".into(), 80, 24).take_replies();
        read_exactly(&mut far, offers.len()).await;

        let size = SizeInfo::new(640., 384., 8., 16., 0., 0., false);
        let sink = Arc::new(NoSink);
        let terminal =
            Terminal::connect(1, size, reader, writer, &Handle::current(), Theme::dark(), sink)
                .unwrap();
        (terminal, far)
    }

    /// The text on the first line, once it ends with `end`.
    async fn first_line(terminal: &Terminal, end: &str) -> String {
        let read = async {
            loop {
                if terminal.refresh() {
                    // Blank cells are left out of a snapshot.
                    let mut line = vec![' '; 80];
                    for cell in terminal.last_snapshot().cells.iter() {
                        if cell.point.line == 0 {
                            line[cell.point.column.0] = cell.character;
                        }
                    }
                    let line = String::from_iter(line).trim_end().to_owned();
                    if line.ends_with(end) {
                        return line;
                    }
                }
                sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(WAIT, read).await.expect("the line never got there")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_shows_once_when_the_server_refuses_to_echo() {
        let (terminal, mut far) = terminal().await;
        far.write_all(&[IAC, WONT, ECHO]).await.unwrap();
        far.write_all(b"login: ").await.unwrap();
        first_line(&terminal, "login:").await;

        terminal.write(&b"root"[..]);
        assert_eq!(read_exactly(&mut far, 4).await, b"root");
        far.write_all(b"!").await.unwrap();
        assert_eq!(first_line(&terminal, "!").await, "login: root!");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_is_not_doubled_when_the_server_echoes() {
        let (terminal, mut far) = terminal().await;
        far.write_all(&[IAC, WILL, ECHO]).await.unwrap();
        far.write_all(b"$ ").await.unwrap();
        first_line(&terminal, "$").await;

        terminal.write(&b"ls"[..]);
        assert_eq!(read_exactly(&mut far, 2).await, b"ls");
        far.write_all(b"ls!").await.unwrap();
        assert_eq!(first_line(&terminal, "!").await, "$ ls!");
    }

    #[tokio::test]
    async fn offers_then_exchanges_escaped_bytes() {
        let (mut reader, writer, mut far) = start(4096);
        let offers = Engine::new("xterm-256color".into(), 100, 30).take_replies();
        assert_eq!(read_exactly(&mut far, offers.len()).await, offers);

        far.write_all(&[b'h', IAC, IAC, b'i']).await.unwrap();
        assert_eq!(&reader.read().await.unwrap()[..], &[b'h', IAC, b'i']);

        writer.write(Bytes::from_static(&[1, IAC, 2])).await.unwrap();
        assert_eq!(read_exactly(&mut far, 4).await, [1, IAC, IAC, 2]);
    }

    #[tokio::test]
    async fn resize_sends_naws_once_the_server_agreed() {
        let (mut reader, writer, mut far) = start(4096);
        let offers = Engine::new("xterm-256color".into(), 100, 30).take_replies();
        read_exactly(&mut far, offers.len()).await;

        writer.resize(size(120, 40)).await.unwrap();
        far.write_all(&[IAC, DO, NAWS, b'x']).await.unwrap();
        assert_eq!(&reader.read().await.unwrap()[..], b"x");
        assert_eq!(
            read_exactly(&mut far, 9).await,
            [IAC, SB, NAWS, 0, 120, 0, 40, IAC, SE],
            "the agreement reports the latest size"
        );

        writer.resize(size(200, 50)).await.unwrap();
        assert_eq!(read_exactly(&mut far, 9).await, [IAC, SB, NAWS, 0, 200, 0, 50, IAC, SE]);
    }

    #[tokio::test]
    async fn a_stalled_write_does_not_stop_output() {
        let (mut reader, writer, mut far) = start(16);
        // The offers alone fill the pipe, and nobody on the far side reads.
        let flood = Bytes::from(vec![b'a'; 64 * 1024]);
        let writes = tokio::spawn(async move {
            loop {
                if writer.write(flood.clone()).await.is_err() {
                    break;
                }
            }
        });
        sleep(Duration::from_millis(50)).await;

        far.write_all(b"out").await.unwrap();
        let output = timeout(WAIT, reader.read()).await.expect("output waited for the write");
        assert_eq!(&output.unwrap()[..], b"out");
        writes.abort();
    }

    #[tokio::test]
    async fn writes_wait_while_the_server_is_not_reading() {
        let (near, _far) = duplex(16);
        let options = TelnetOptions { write_queue: 1, ..TelnetOptions::default() };
        let (_reader, writer) = connect(near, options, &Handle::current());

        let queue_some = async {
            for _ in 0..16 {
                writer.write(Bytes::from(vec![b'a'; 4096])).await.unwrap();
            }
        };

        assert!(timeout(Duration::from_millis(100), queue_some).await.is_err());
    }

    #[tokio::test]
    async fn a_negotiation_flood_ends_the_session_instead_of_growing() {
        let (mut reader, _writer, mut far) = start(16);
        let flood: Vec<u8> =
            (0..MAX_QUEUED_REPLIES / 3 + 1024).flat_map(|_| [IAC, WILL, 99]).collect();
        // The far side never reads, so the replies have nowhere to go.
        tokio::spawn(async move {
            let _ = far.write_all(&flood).await;
            std::future::pending::<()>().await;
        });

        let outcome = timeout(WAIT, async {
            loop {
                if let Err(reason) = reader.read().await {
                    return reason;
                }
            }
        })
        .await
        .expect("the flood was not noticed");
        assert!(matches!(outcome, CloseReason::Disconnected(_)), "{outcome:?}");
    }

    #[tokio::test]
    async fn eof_closes_the_session_and_a_lost_stream_fails_writes() {
        let (mut reader, writer, far) = start(4096);
        drop(far);

        assert_eq!(timeout(WAIT, reader.read()).await.unwrap().unwrap_err(), CloseReason::Closed);
        let write = async {
            loop {
                if let Err(reason) = writer.write(Bytes::from_static(b"x")).await {
                    return reason;
                }
                sleep(Duration::from_millis(10)).await;
            }
        };
        let reason = timeout(WAIT, write).await.expect("writes kept succeeding");
        assert!(matches!(reason, CloseReason::Disconnected(_)), "{reason:?}");
    }
}
