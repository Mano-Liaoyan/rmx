//! The error type shared by every backend and the transfer queue.

use std::io;

/// Result alias used across this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Failure of a file-system or transfer operation.
///
/// The type is `Clone` so that a failure can be stored in a transfer's state and also reported on
/// the progress channel.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The path does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The server or operating system refused access.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The target already exists and the operation may not replace it.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// The backend cannot perform this operation (for example a random-access write over FTP).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The path cannot be used with this backend or is unsafe to act on.
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// The transfer was cancelled by the caller.
    #[error("cancelled")]
    Cancelled,
    /// A request exceeded its time limit. Worth retrying.
    #[error("timed out: {0}")]
    Timeout(String),
    /// The connection or SSH channel failed. Worth retrying.
    #[error("connection failed: {0}")]
    Connection(String),
    /// The source ended early, or the written size did not match the source. Worth retrying.
    #[error("size mismatch: {0}")]
    SizeMismatch(String),
    /// No FTP connection became free in time. Worth retrying.
    #[error("no connection available: {0}")]
    Busy(String),
    /// The server replied with something that is not a success.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A local I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

impl Error {
    /// Returns whether repeating the same operation may succeed without any other change.
    ///
    /// [`TransferQueue`](crate::TransferQueue) only retries these automatically.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Error::Timeout(_) | Error::Connection(_) | Error::SizeMismatch(_) | Error::Busy(_)
        )
    }

    pub(crate) fn from_io(context: impl AsRef<str>, error: io::Error) -> Self {
        let message = format!("{}: {error}", context.as_ref());
        match error.kind() {
            io::ErrorKind::NotFound => Error::NotFound(message),
            io::ErrorKind::PermissionDenied => Error::PermissionDenied(message),
            io::ErrorKind::AlreadyExists => Error::AlreadyExists(message),
            io::ErrorKind::TimedOut => Error::Timeout(message),
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof => Error::Connection(message),
            _ => Error::Io(message),
        }
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(error: tokio::task::JoinError) -> Self {
        Error::Io(format!("background task failed: {error}"))
    }
}
