//! File browsing and transfer over the local disk, SFTP, FTP and FTPS, as a library with no UI.
//!
//! * [`RemoteFs`] is the common asynchronous file-system interface. [`LocalFs`], [`SftpFs`] and
//!   [`FtpFs`] implement it. Directory listings are delivered through [`DirStream`] in batches, so
//!   a consumer can show a 100,000-entry directory as it arrives, without waiting for all of it.
//! * [`TransferQueue`] runs copies between any two backends with bounded concurrency,
//!   rate-limited progress, cancellation, automatic and manual retry, resume of partial files,
//!   recursive directory copy and an [`OverwritePolicy`].
//!
//! No SSH client lives here. [`SftpFs::connect`] takes any `AsyncRead + AsyncWrite` stream, such as
//! an SSH channel running the `sftp` subsystem, and [`FtpFs::connect_with`] takes an [`FtpDialer`]
//! that can return tunnelled streams.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use rmx_transfer::{
//!     LocalFs, OverwritePolicy, QueueConfig, TransferQueue, TransferRequest,
//! };
//!
//! # async fn run() {
//! let local = Arc::new(LocalFs::new());
//! let (queue, mut progress) = TransferQueue::new(QueueConfig::default());
//! let id = queue.submit(TransferRequest {
//!     source: local.clone(),
//!     source_path: "/data/in".into(),
//!     dest: local,
//!     dest_path: "/data/out".into(),
//!     overwrite: OverwritePolicy::Resume,
//! });
//! while let Some(update) = progress.recv().await {
//!     println!("{} of {} bytes", update.total.bytes_done, update.total.bytes_total);
//!     if update.transfers.iter().any(|t| t.id == id && t.state.is_finished()) {
//!         break;
//!     }
//! }
//! # }
//! ```

#![warn(missing_docs)]

mod error;
mod fs;
mod ftp;
mod local;
mod queue;
mod sftp;

pub use error::{Error, Result};
pub use fs::{
    DirStream, Entry, EntryKind, ListOptions, RangeRead, RangeWrite, RemoteFs, WriteStart, read_all,
};
pub use ftp::{DialedStream, FtpConfig, FtpDialer, FtpFs, FtpTls, TcpDialer, TunnelStream};
pub use local::{DEFAULT_LOCAL_CHUNK, LocalFs};
pub use queue::{
    FileProgress, FileStatus, OverwritePolicy, ProgressUpdate, QueueConfig, TotalProgress,
    TransferId, TransferProgress, TransferQueue, TransferRequest, TransferState,
};
pub use sftp::{SftpFs, SftpOptions};

/// Re-exported so callers can build [`FtpTls`] without depending on matching `rustls` versions.
pub use rustls;
