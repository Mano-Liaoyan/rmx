//! The [`RemoteFs`] abstraction implemented by every backend.

use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::Result;

/// What kind of object a directory entry refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link that was not followed.
    Symlink,
    /// Anything else, such as a socket or a device node.
    Other,
}

/// Metadata for one file-system object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Last path component. Non-UTF-8 names are converted lossily and may not round-trip.
    pub name: String,
    /// Kind of object.
    pub kind: EntryKind,
    /// Size in bytes, if the backend reports one.
    pub size: Option<u64>,
    /// Last modification time, if the backend reports one.
    pub modified: Option<SystemTime>,
    /// POSIX mode bits including the file-type bits, if the backend reports them.
    pub mode: Option<u32>,
}

/// How a directory listing is split into batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListOptions {
    /// Largest number of entries in one batch. Values below 1 are treated as 1.
    pub batch_size: usize,
    /// A partial batch is delivered once its first entry is this old, so that a slow server does
    /// not hold back entries it has already sent.
    pub flush_after: Duration,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self { batch_size: 1000, flush_after: Duration::from_millis(50) }
    }
}

/// Number of batches a backend may have buffered ahead of the consumer.
const LIST_BUFFERED_BATCHES: usize = 4;

/// A directory listing delivered in batches.
///
/// The backend produces entries on a background task and stops when the consumer falls behind by
/// a few batches, so memory use does not grow with the size of the directory and a consumer never
/// has to wait for a whole listing. Entries are in server order and exclude `.` and `..`.
/// Dropping the stream stops the listing and releases its server resources.
#[derive(Debug)]
pub struct DirStream {
    rx: mpsc::Receiver<Result<Vec<Entry>>>,
}

impl DirStream {
    pub(crate) fn channel() -> (mpsc::Sender<Result<Vec<Entry>>>, Self) {
        let (tx, rx) = mpsc::channel(LIST_BUFFERED_BATCHES);
        (tx, Self { rx })
    }

    /// Waits for the next batch. Returns `Ok(None)` once the listing is complete.
    pub async fn next_batch(&mut self) -> Result<Option<Vec<Entry>>> {
        self.rx.recv().await.transpose()
    }

    /// Reads the remainder of the listing into one vector.
    pub async fn collect_all(mut self) -> Result<Vec<Entry>> {
        let mut all = Vec::new();
        while let Some(batch) = self.next_batch().await? {
            all.extend(batch);
        }
        Ok(all)
    }
}

/// Collects entries for a [`DirStream`] and decides when a batch is ready.
pub(crate) struct BatchBuffer {
    options: ListOptions,
    entries: Vec<Entry>,
    oldest: Option<Instant>,
}

impl BatchBuffer {
    pub(crate) fn new(options: ListOptions) -> Self {
        Self {
            options: ListOptions { batch_size: options.batch_size.max(1), ..options },
            entries: Vec::new(),
            oldest: None,
        }
    }

    /// Adds an entry and returns a batch if one is due.
    pub(crate) fn push(&mut self, entry: Entry) -> Option<Vec<Entry>> {
        self.oldest.get_or_insert_with(Instant::now);
        self.entries.push(entry);
        let full = self.entries.len() >= self.options.batch_size;
        let stale = self.oldest.is_some_and(|t| t.elapsed() >= self.options.flush_after);
        (full || stale).then(|| self.take())
    }

    /// Returns whatever has accumulated, or `None` when empty.
    pub(crate) fn drain(&mut self) -> Option<Vec<Entry>> {
        (!self.entries.is_empty()).then(|| self.take())
    }

    fn take(&mut self) -> Vec<Entry> {
        self.oldest = None;
        std::mem::replace(&mut self.entries, Vec::with_capacity(self.options.batch_size.min(4096)))
    }
}

/// Where a write starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStart {
    /// Create the file, or empty it if it exists, and write from the beginning.
    Truncate,
    /// Create the file if needed, keep its contents and write from this offset.
    ///
    /// FTP can only continue at the current end of the file and returns
    /// [`Error::Unsupported`](crate::Error::Unsupported) for any other offset.
    At(u64),
}

/// Sequential reader returned by [`RemoteFs::read_range`].
#[async_trait]
pub trait RangeRead: Send {
    /// Returns the next chunk, or `Ok(None)` at the end of the range or file. Chunk sizes are
    /// chosen by the backend and are never empty.
    async fn next_chunk(&mut self) -> Result<Option<Bytes>>;
}

/// Sequential writer returned by [`RemoteFs::write_range`].
#[async_trait]
pub trait RangeWrite: Send {
    /// Queues `chunk` to be written after everything written so far.
    ///
    /// The backend may acknowledge before the data reaches the server; a failure can surface on a
    /// later call or in [`finish`](RangeWrite::finish).
    async fn write(&mut self, chunk: Bytes) -> Result<()>;

    /// Waits for every write to complete and closes the file. A writer dropped without this
    /// is abandoned, and its data may be incomplete.
    async fn finish(self: Box<Self>) -> Result<()>;
}

/// An asynchronous file system: local disk, SFTP or FTP.
///
/// Paths are `/`-separated UTF-8 strings. Relative paths are resolved against the backend's start
/// directory. The trait is object-safe so that a [`TransferQueue`](crate::TransferQueue) can move
/// files between any two backends.
#[async_trait]
pub trait RemoteFs: Send + Sync {
    /// Starts listing a directory. Entries arrive in batches; see [`DirStream`].
    async fn list(&self, path: &str, options: ListOptions) -> Result<DirStream>;

    /// Returns metadata for `path`, following symbolic links.
    async fn stat(&self, path: &str) -> Result<Entry>;

    /// Creates one directory. The parent must exist.
    async fn mkdir(&self, path: &str) -> Result<()>;

    /// Removes a file or symbolic link.
    async fn remove_file(&self, path: &str) -> Result<()>;

    /// Removes an empty directory.
    async fn remove_dir(&self, path: &str) -> Result<()>;

    /// Renames `from` to `to`, replacing `to` if it exists.
    ///
    /// The replacement is atomic where the backend can do it. A server with no atomic rename gets
    /// a remove followed by a rename.
    async fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Opens `path` for sequential reading from `offset`, for at most `len` bytes (`None` reads to
    /// the end of the file).
    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Box<dyn RangeRead>>;

    /// Opens `path` for sequential writing.
    async fn write_range(&self, path: &str, start: WriteStart) -> Result<Box<dyn RangeWrite>>;
}

/// Reads an entire range into memory. Intended for small ranges.
pub async fn read_all(
    fs: &dyn RemoteFs,
    path: &str,
    offset: u64,
    len: Option<u64>,
) -> Result<Vec<u8>> {
    let mut reader = fs.read_range(path, offset, len).await?;
    let mut out = Vec::new();
    while let Some(chunk) = reader.next_chunk().await? {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Returns the final component of a `/`-separated path.
pub(crate) fn base_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some("") | None => "/".to_owned(),
        Some(name) => name.to_owned(),
    }
}

/// Joins a directory and a file name with exactly one `/`.
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}
