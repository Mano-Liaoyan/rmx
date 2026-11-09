//! What a terminal needs of a connection, and what it reports about one.

use std::fmt;
use std::future::Future;

use alacritty_terminal::event::WindowSize;
use bytes::Bytes;

/// Why a remote session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// The remote program exited with this status.
    Exited(u32),
    /// The remote program was killed by a signal.
    Killed { signal: String, message: String },
    /// The connection was lost, for this reason.
    Disconnected(String),
    /// The session closed without saying why.
    Closed,
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(status) => write!(f, "exited with status {status}"),
            Self::Killed { signal, message } if message.is_empty() => {
                write!(f, "killed by {signal}")
            },
            Self::Killed { signal, message } => write!(f, "killed by {signal}: {message}"),
            Self::Disconnected(reason) => write!(f, "disconnected: {reason}"),
            Self::Closed => f.write_str("closed"),
        }
    }
}

/// A directory a shell announced with OSC 7.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDirectory {
    /// The host named in the announcement, if any.
    pub host: Option<String>,
    /// The absolute, percent-decoded path.
    pub path: String,
}

/// What happened to a terminal on a remote session, see [`Sink::session`](crate::Sink::session).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The shell moved to another directory.
    WorkingDirectory(WorkingDirectory),
    /// Output was lost: the parser stalled with the buffer full, and this many bytes were thrown
    /// away to keep reading the connection. The screen may be missing them.
    OutputDiscarded(u64),
    /// The session is over and all of its output has been parsed. The terminal stays as it is,
    /// readable and selectable, but writes and resizes go nowhere.
    Closed(CloseReason),
}

/// The output half of a connection.
///
/// Reading and writing are two traits as they cannot wait on each other: a write held up by a
/// full window must not stop the output that would drain it.
pub trait TransportReader: Send + 'static {
    /// The next bytes of output, as soon as there are any, or why there will be no more.
    ///
    /// The driver calls this again as soon as it has queued a result. Being dropped midway, which
    /// only happens when the terminal goes away, loses nothing that matters.
    fn read(&mut self) -> impl Future<Output = Result<Bytes, CloseReason>> + Send;
}

/// The input half of a connection, used by concurrent writes and resizes.
pub trait TransportWriter: Send + Sync + 'static {
    /// Send bytes to the program, in order. Failing ends the session with that reason.
    fn write(&self, bytes: Bytes) -> impl Future<Output = Result<(), CloseReason>> + Send;

    /// Tell the program about a new size. Only the latest of several quick changes gets here.
    /// Failing ends the session with that reason.
    fn resize(&self, size: WindowSize) -> impl Future<Output = Result<(), CloseReason>> + Send;
}
