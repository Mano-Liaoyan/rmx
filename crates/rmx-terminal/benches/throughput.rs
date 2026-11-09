//! How fast the remote driver gets through output, against the local PTY path on the same bytes.
//!
//! Both run a real [`Terminal`] on a consumer that takes a snapshot every frame at 240 Hz, as the
//! application would, and stop when a title set by the last bytes arrives.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use std::{fs, process};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::Processor;
use bytes::Bytes;
use rmx_render::SizeInfo;
use rmx_terminal::{
    CloseReason, Sink, Terminal, Theme, TransportReader, TransportWriter, WindowSize,
};
use tokio::runtime::Builder;

const DONE_TITLE: &str = "rmx-bench-done";
const PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const RUNS: usize = 5;
const CHUNK: usize = 32 * 1024;
const FRAME: Duration = Duration::from_micros(4167);

enum Signal {
    Wakeup,
    Done,
}

struct BenchSink(Sender<Signal>);

impl Sink for BenchSink {
    fn wakeup(&self, _: u64) {
        let _ = self.0.send(Signal::Wakeup);
    }

    fn event(&self, _: u64, event: Event) {
        if matches!(&event, Event::Title(title) if title == DONE_TITLE) {
            let _ = self.0.send(Signal::Done);
        }
    }
}

fn size() -> SizeInfo {
    SizeInfo::new(1600., 900., 8., 16., 0., 0., false)
}

/// Mostly colored text with some wide characters, ending by setting the title.
fn payload() -> Vec<u8> {
    let mut stream = Vec::with_capacity(PAYLOAD_BYTES + 256);
    let mut line = 0u64;
    while stream.len() < PAYLOAD_BYTES {
        stream.extend_from_slice(
            format!(
                "\x1b[1;3{}m{line:>10}\x1b[0m drwxr-xr-x 2 user group 4096 Nov  8 file-{line}.txt \
                 文件 \x1b[38;5;208m{:x}\x1b[m\r\n",
                line % 8,
                line.wrapping_mul(0x9E37_79B9)
            )
            .as_bytes(),
        );
        line += 1;
    }
    stream.extend_from_slice(format!("\x1b]0;{DONE_TITLE}\x07").as_bytes());
    stream
}

/// Take snapshots at frame rate until the title arrives. Returns the seconds taken from `start`.
fn consume(terminal: &Terminal, signals: &mpsc::Receiver<Signal>, start: Instant) -> f64 {
    let mut next_frame = Instant::now();
    loop {
        match signals.recv_timeout(Duration::from_secs(60)) {
            Ok(Signal::Done) => return start.elapsed().as_secs_f64(),
            Ok(Signal::Wakeup) => {},
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("the benchmark stopped making progress")
            },
        }
        let now = Instant::now();
        if now < next_frame {
            std::thread::sleep(next_frame - now);
        }
        next_frame = Instant::now() + FRAME;
        terminal.refresh();
    }
}

/// One run of the local path: the file goes through a real PTY with `cat`.
fn run_pty(path: &str, bytes: usize) -> f64 {
    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    // Raw mode, so that the bytes parsed are exactly those of the payload.
    let script = format!("stty raw -echo; cat '{path}'; exec sleep 3600");
    let shell = ("sh".to_owned(), vec!["-c".to_owned(), script]);
    let terminal =
        Terminal::spawn(1, size(), Some(shell), Theme::dark(), Arc::new(BenchSink(tx))).unwrap();
    let seconds = consume(&terminal, &rx, start);
    bytes as f64 / seconds
}

struct ChunkReader {
    chunks: std::vec::IntoIter<Bytes>,
}

impl TransportReader for ChunkReader {
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        match self.chunks.next() {
            Some(chunk) => Ok(chunk),
            None => std::future::pending().await,
        }
    }
}

struct NullWriter;

impl TransportWriter for NullWriter {
    async fn write(&self, _: Bytes) -> Result<(), CloseReason> {
        Ok(())
    }

    async fn resize(&self, _: WindowSize) -> Result<(), CloseReason> {
        Ok(())
    }
}

/// One run of the remote driver, fed chunks the size of SSH packets.
fn run_driver(payload: &[u8], bytes: usize) -> f64 {
    let chunks: Vec<Bytes> = payload.chunks(CHUNK).map(Bytes::copy_from_slice).collect();
    let runtime = Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    let terminal = Terminal::connect(
        1,
        size(),
        ChunkReader { chunks: chunks.into_iter() },
        NullWriter,
        runtime.handle(),
        Theme::dark(),
        Arc::new(BenchSink(tx)),
    )
    .unwrap();
    let seconds = consume(&terminal, &rx, start);
    bytes as f64 / seconds
}

#[derive(Clone, Copy)]
struct Void;

impl EventListener for Void {}

/// What the parser alone does, for how much of the rest is the path around it.
fn run_parse_only(payload: &[u8], bytes: usize) -> f64 {
    let mut term = Term::new(term::Config::default(), &size(), Void);
    let mut processor: Processor = Processor::new();
    let start = Instant::now();
    for chunk in payload.chunks(CHUNK) {
        processor.advance(&mut term, chunk);
    }
    bytes as f64 / start.elapsed().as_secs_f64()
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn report(name: &str, mut run: impl FnMut() -> f64) -> f64 {
    let rates: Vec<f64> = (0..RUNS).map(|_| run()).collect();
    let megabytes: Vec<String> = rates.iter().map(|rate| format!("{:.0}", rate / 1e6)).collect();
    let median = median(rates);
    println!("{name:>12}: median {:.1} MB/s, runs [{}]", median / 1e6, megabytes.join(", "));
    median
}

fn main() {
    if std::env::args().any(|argument| argument == "--list") {
        println!("throughput: benchmark");
        return;
    }

    let payload = payload();
    let bytes = payload.len();
    let path: PathBuf = std::env::temp_dir().join(format!("rmx-bench-{}", process::id()));
    fs::File::create(&path).unwrap().write_all(&payload).unwrap();

    println!("{bytes} bytes, {RUNS} runs each");
    let parse = report("parse only", || run_parse_only(&payload, bytes));
    let pty = report("pty", || run_pty(path.to_str().unwrap(), bytes));
    let driver = report("driver", || run_driver(&payload, bytes));
    println!("driver / pty: {:.1}%", driver / pty * 100.);
    println!("driver / parse only: {:.1}%", driver / parse * 100.);

    fs::remove_file(path).unwrap();
}
