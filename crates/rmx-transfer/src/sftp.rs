//! [`RemoteFs`] over SFTP version 3, on any byte stream.
//!
//! The session runs over a caller-provided stream, normally an SSH channel opened on the `sftp`
//! subsystem. This backend uses the raw request API of `russh-sftp` so that a transfer can keep
//! many read or write requests outstanding at once. A window of requests is what lets SFTP use
//! a high-latency or high-bandwidth link; waiting for each reply limits throughput to one chunk
//! per round trip.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::FutureExt;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesOrdered;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::{Limits, SftpResult};
use russh_sftp::client::{Config, RawSftpSession};
use russh_sftp::protocol::{Data, FileAttributes, OpenFlags, Packet, Status, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{Error, Result};
use crate::fs::{
    BatchBuffer, DirStream, Entry, EntryKind, ListOptions, RangeRead, RangeWrite, RemoteFs,
    WriteStart, base_name,
};

/// Chunk size requested when the server does not advertise limits; every server accepts this.
const SAFE_CHUNK: u32 = 32 * 1024;
/// Read requests issued when a read starts; the window then doubles with each reply.
const INITIAL_READ_WINDOW: usize = 4;
/// Limit on packets accepted from the server. Directory replies can exceed the data chunk size.
const MAX_INBOUND_PACKET: u32 = 1 << 20;
/// Space reserved in each request for fields other than the file data.
const WRITE_HEADER: u32 = 1024;
const LIMITS_EXTENSION: &str = "limits@openssh.com";
const POSIX_RENAME_EXTENSION: &str = "posix-rename@openssh.com";

/// Tuning for an [`SftpFs`] session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SftpOptions {
    /// Upper bound on the size of one read or write request. The session uses less if the server
    /// advertises a lower limit, and 32 KiB if it advertises none.
    pub chunk_size: u32,
    /// Maximum requests outstanding for each open file.
    pub max_in_flight: usize,
    /// Time allowed for each request, including time spent queued behind the rest of the window.
    pub request_timeout: Duration,
}

impl Default for SftpOptions {
    fn default() -> Self {
        Self {
            chunk_size: 256 * 1024,
            max_in_flight: 64,
            request_timeout: Duration::from_secs(120),
        }
    }
}

/// An SFTP session.
pub struct SftpFs {
    raw: Arc<RawSftpSession>,
    read_chunk: u32,
    write_chunk: u32,
    max_in_flight: usize,
    posix_rename: bool,
}

impl std::fmt::Debug for SftpFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpFs")
            .field("read_chunk", &self.read_chunk)
            .field("write_chunk", &self.write_chunk)
            .field("max_in_flight", &self.max_in_flight)
            .finish_non_exhaustive()
    }
}

fn status_error(context: &str, status: &Status) -> Error {
    let message = format!("{context}: {}", status.error_message);
    match status.status_code {
        StatusCode::NoSuchFile => Error::NotFound(message),
        StatusCode::PermissionDenied => Error::PermissionDenied(message),
        StatusCode::OpUnsupported => Error::Unsupported(message),
        StatusCode::NoConnection | StatusCode::ConnectionLost => Error::Connection(message),
        StatusCode::Ok | StatusCode::Eof | StatusCode::Failure | StatusCode::BadMessage => {
            Error::Protocol(message)
        },
    }
}

fn map_error(context: &str, error: SftpError) -> Error {
    match &error {
        SftpError::Status(status) => status_error(context, status),
        SftpError::Timeout => Error::Timeout(context.to_owned()),
        // The library reports a stopped transport through these two variants.
        SftpError::IO(_) | SftpError::UnexpectedBehavior(_) => {
            Error::Connection(format!("{context}: {error}"))
        },
        SftpError::Limited(_) | SftpError::UnexpectedPacket => {
            Error::Protocol(format!("{context}: {error}"))
        },
    }
}

fn is_eof(error: &SftpError) -> bool {
    matches!(error, SftpError::Status(status) if status.status_code == StatusCode::Eof)
}

fn entry_from(name: String, attrs: &FileAttributes) -> Entry {
    const TYPE_MASK: u32 = 0o170000;
    let kind = match attrs.permissions.map(|mode| mode & TYPE_MASK) {
        Some(0o100000) => EntryKind::File,
        Some(0o040000) => EntryKind::Dir,
        Some(0o120000) => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    Entry {
        name,
        kind,
        size: attrs.size,
        modified: attrs.mtime.map(|secs| UNIX_EPOCH + Duration::from_secs(secs.into())),
        mode: attrs.permissions,
    }
}

/// Encodes values as SSH strings for an extension request body.
fn ssh_strings(values: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    out
}

impl SftpFs {
    /// Starts an SFTP session on `stream` and negotiates its limits and extensions.
    ///
    /// The stream must already be connected to an SFTP server, for example an SSH channel on which
    /// the `sftp` subsystem was requested. The session runs until the stream closes or
    /// [`close`](Self::close) is called.
    pub async fn connect<S>(stream: S, options: SftpOptions) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = Config {
            max_packet_len: MAX_INBOUND_PACKET,
            request_timeout_secs: options.request_timeout.as_secs().max(1),
            ..Config::default()
        };
        let mut raw = RawSftpSession::new_with_config(stream, config);
        let version = raw.init().await.map_err(|e| map_error("sftp init", e))?;
        let has = |name: &str| version.extensions.get(name).is_some_and(|v| v == "1");
        let posix_rename = has(POSIX_RENAME_EXTENSION);

        let limits = if has(LIMITS_EXTENSION) {
            let limits = Limits::from(raw.limits().await.map_err(|e| map_error("sftp limits", e))?);
            raw.set_limits(limits);
            limits
        } else {
            Limits::default()
        };

        let chunk = options.chunk_size.max(1);
        let negotiated = |len: Option<u64>, packet_room: Option<u64>| -> u32 {
            match len.into_iter().chain(packet_room).min() {
                Some(limit) if limit > 0 => chunk.min(limit.min(u32::MAX as u64) as u32),
                _ => chunk.min(SAFE_CHUNK),
            }
        };
        let room = limits.packet_len.map(|p| p.saturating_sub(WRITE_HEADER as u64));
        Ok(Self {
            raw: Arc::new(raw),
            read_chunk: negotiated(limits.read_len, room),
            write_chunk: negotiated(limits.write_len, room),
            max_in_flight: options.max_in_flight.max(1),
            posix_rename,
        })
    }

    /// Resolves `path` on the server to an absolute canonical path.
    pub async fn canonicalize(&self, path: &str) -> Result<String> {
        let mut name = self.raw.realpath(path).await.map_err(|e| map_error(path, e))?;
        match name.files.pop() {
            Some(file) => Ok(file.filename),
            None => Err(Error::Protocol(format!("{path}: empty realpath reply"))),
        }
    }

    /// Creates `path` holding `data`, unless something is already there. Returns whether it
    /// created the file, the only way to take a lock on a server with no compare-and-swap.
    pub async fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool> {
        let flags = OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE;
        let handle = match self.raw.open(path, flags, FileAttributes::empty()).await {
            Ok(reply) => reply.handle,
            // SFTP version 3 has no "already exists": OpenSSH answers a plain failure.
            Err(SftpError::Status(status))
                if status.status_code == StatusCode::Failure && self.stat(path).await.is_ok() =>
            {
                return Ok(false);
            },
            Err(error) => return Err(map_error(path, error)),
        };
        let mut offset = 0u64;
        let mut written = Ok(());
        for chunk in data.chunks(self.write_chunk as usize) {
            if let Err(error) = self.raw.write(handle.clone(), offset, chunk.to_vec()).await {
                written = Err(map_error(path, error));
                break;
            }
            offset += chunk.len() as u64;
        }
        let closed = self.raw.close(handle).await;
        written?;
        closed.map(drop).map_err(|error| map_error(path, error))?;
        Ok(true)
    }

    /// Asks the session to shut down. Outstanding requests fail.
    pub fn close(&self) -> Result<()> {
        self.raw.close_session().map_err(|e| map_error("close", e))
    }

    async fn plain_rename(&self, from: &str, to: &str) -> Result<()> {
        self.raw.rename(from, to).await.map(drop).map_err(|e| map_error(from, e))
    }
}

/// Closes `handle` in the background; used when a reader or writer is dropped early.
fn close_detached(raw: Arc<RawSftpSession>, handle: String) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let _ = raw.close(handle).await;
        });
    }
}

#[async_trait]
impl RemoteFs for SftpFs {
    async fn list(&self, path: &str, options: ListOptions) -> Result<DirStream> {
        let handle = self.raw.opendir(path).await.map_err(|e| map_error(path, e))?.handle;
        let (tx, stream) = DirStream::channel();
        let raw = Arc::clone(&self.raw);
        let context = path.to_owned();
        tokio::spawn(async move {
            let mut buffer = BatchBuffer::new(options);
            'listing: loop {
                match raw.readdir(handle.clone()).await {
                    Ok(name) => {
                        for file in name.files {
                            if file.filename == "." || file.filename == ".." {
                                continue;
                            }
                            let entry = entry_from(file.filename, &file.attrs);
                            if let Some(batch) = buffer.push(entry)
                                && tx.send(Ok(batch)).await.is_err()
                            {
                                break 'listing;
                            }
                        }
                    },
                    Err(error) if is_eof(&error) => {
                        if let Some(batch) = buffer.drain() {
                            let _ = tx.send(Ok(batch)).await;
                        }
                        break;
                    },
                    Err(error) => {
                        let _ = tx.send(Err(map_error(&context, error))).await;
                        break;
                    },
                }
            }
            let _ = raw.close(handle).await;
        });
        Ok(stream)
    }

    async fn stat(&self, path: &str) -> Result<Entry> {
        let attrs = self.raw.stat(path).await.map_err(|e| map_error(path, e))?.attrs;
        Ok(entry_from(base_name(path), &attrs))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let attrs = FileAttributes::empty();
        self.raw.mkdir(path, attrs).await.map(drop).map_err(|e| map_error(path, e))
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        self.raw.remove(path).await.map(drop).map_err(|e| map_error(path, e))
    }

    async fn remove_dir(&self, path: &str) -> Result<()> {
        self.raw.rmdir(path).await.map(drop).map_err(|e| map_error(path, e))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        if self.posix_rename {
            let body = ssh_strings(&[from, to]);
            let reply = self
                .raw
                .extended(POSIX_RENAME_EXTENSION, body)
                .await
                .map_err(|e| map_error(from, e))?;
            return match reply {
                Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
                Packet::Status(status) => Err(status_error(from, &status)),
                _ => Err(Error::Protocol(format!("{from}: unexpected rename reply"))),
            };
        }
        // Plain SFTP rename refuses an existing target, so make room for it and try again.
        match self.plain_rename(from, to).await {
            Ok(()) => Ok(()),
            Err(error) if error.is_transient() => Err(error),
            Err(_) => {
                self.remove_file(to).await?;
                self.plain_rename(from, to).await
            },
        }
    }

    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Box<dyn RangeRead>> {
        let handle = self
            .raw
            .open(path, OpenFlags::READ, FileAttributes::empty())
            .await
            .map_err(|e| map_error(path, e))?
            .handle;
        Ok(Box::new(SftpReader {
            raw: Arc::clone(&self.raw),
            handle: Some(handle),
            path: path.to_owned(),
            next_offset: offset,
            end: len.map(|len| offset.saturating_add(len)),
            chunk: self.read_chunk,
            window: INITIAL_READ_WINDOW.min(self.max_in_flight),
            max_window: self.max_in_flight,
            pending: FuturesOrdered::new(),
            ready: VecDeque::new(),
            eof: false,
        }))
    }

    async fn write_range(&self, path: &str, start: WriteStart) -> Result<Box<dyn RangeWrite>> {
        let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
        let offset = match start {
            WriteStart::Truncate => {
                flags |= OpenFlags::TRUNCATE;
                0
            },
            WriteStart::At(offset) => offset,
        };
        let handle = self
            .raw
            .open(path, flags, FileAttributes::empty())
            .await
            .map_err(|e| map_error(path, e))?
            .handle;
        Ok(Box::new(SftpWriter {
            raw: Arc::clone(&self.raw),
            handle: Some(handle),
            path: path.to_owned(),
            offset,
            chunk: self.write_chunk as usize,
            window: self.max_in_flight,
            pending: FuturesOrdered::new(),
        }))
    }
}

struct ReadDone {
    offset: u64,
    requested: u32,
    result: SftpResult<Data>,
}

struct SftpReader {
    raw: Arc<RawSftpSession>,
    handle: Option<String>,
    path: String,
    next_offset: u64,
    end: Option<u64>,
    chunk: u32,
    window: usize,
    max_window: usize,
    pending: FuturesOrdered<BoxFuture<'static, ReadDone>>,
    /// Replies that arrived while requests were being sent, in file order.
    ready: VecDeque<ReadDone>,
    eof: bool,
}

impl SftpReader {
    /// Issues requests until the window is full, then polls so that they are actually sent.
    fn fill_window(&mut self, handle: &str) {
        while !self.eof && self.pending.len() + self.ready.len() < self.window {
            let room = self.end.map_or(u64::MAX, |end| end.saturating_sub(self.next_offset));
            if room == 0 {
                break;
            }
            let requested = (self.chunk as u64).min(room) as u32;
            let offset = self.next_offset;
            self.next_offset += requested as u64;
            let raw = Arc::clone(&self.raw);
            let handle = handle.to_owned();
            self.pending.push_back(
                async move {
                    let result = raw.read(handle, offset, requested).await;
                    ReadDone { offset, requested, result }
                }
                .boxed(),
            );
        }
        // Each pass polls every request not yet started; stop when none can complete.
        while let Some(Some(done)) = self.pending.next().now_or_never() {
            self.ready.push_back(done);
        }
    }

    /// Forgets every outstanding request; late replies are discarded by the session.
    fn clear_window(&mut self) {
        self.pending = FuturesOrdered::new();
        self.ready.clear();
    }
}

#[async_trait]
impl RangeRead for SftpReader {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        let Some(handle) = self.handle.clone() else { return Ok(None) };
        loop {
            self.fill_window(&handle);
            let done = match self.ready.pop_front() {
                Some(done) => done,
                None => match self.pending.next().await {
                    Some(done) => done,
                    None => {
                        self.handle = None;
                        let _ = self.raw.close(handle).await;
                        return Ok(None);
                    },
                },
            };
            match done.result {
                Ok(Data { data, .. }) if !data.is_empty() => {
                    self.window = (self.window * 2).min(self.max_window);
                    let received = data.len() as u32;
                    if received < done.requested {
                        // A short reply means the server caps requests below our chunk size, or the
                        // file ends here. Later requests are based on the wrong offset either way.
                        self.clear_window();
                        self.next_offset = done.offset + received as u64;
                        if done.requested == self.chunk {
                            self.chunk = received;
                        }
                    }
                    return Ok(Some(Bytes::from(data)));
                },
                Ok(_) => self.finish_eof(),
                Err(error) if is_eof(&error) => self.finish_eof(),
                Err(error) => return Err(map_error(&self.path, error)),
            }
        }
    }
}

impl SftpReader {
    fn finish_eof(&mut self) {
        self.eof = true;
        self.clear_window();
    }
}

impl Drop for SftpReader {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            close_detached(Arc::clone(&self.raw), handle);
        }
    }
}

struct SftpWriter {
    raw: Arc<RawSftpSession>,
    handle: Option<String>,
    path: String,
    offset: u64,
    chunk: usize,
    window: usize,
    pending: FuturesOrdered<BoxFuture<'static, SftpResult<Status>>>,
}

impl SftpWriter {
    fn check(&self, result: SftpResult<Status>) -> Result<()> {
        result.map(drop).map_err(|e| map_error(&self.path, e))
    }

    /// Takes every acknowledgement that has already arrived, starting any requests queued.
    fn poll_acknowledgements(&mut self) -> Result<()> {
        while let Some(Some(result)) = self.pending.next().now_or_never() {
            self.check(result)?;
        }
        Ok(())
    }
}

#[async_trait]
impl RangeWrite for SftpWriter {
    async fn write(&mut self, mut chunk: Bytes) -> Result<()> {
        let handle = self.handle.clone().ok_or_else(|| Error::Io("writer is closed".into()))?;
        while !chunk.is_empty() {
            let part = chunk.split_to(self.chunk.min(chunk.len()));
            let len = part.len() as u64;
            let raw = Arc::clone(&self.raw);
            let handle = handle.clone();
            let offset = self.offset;
            // `Vec::from` moves the allocation when this is the only reference to it.
            let data = Vec::from(part);
            self.pending.push_back(async move { raw.write(handle, offset, data).await }.boxed());
            self.offset += len;
            self.poll_acknowledgements()?;
            while self.pending.len() >= self.window {
                match self.pending.next().await {
                    Some(result) => self.check(result)?,
                    None => break,
                }
            }
        }
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<()> {
        while let Some(result) = self.pending.next().await {
            self.check(result)?;
        }
        let Some(handle) = self.handle.take() else { return Ok(()) };
        self.raw.close(handle).await.map(drop).map_err(|e| map_error(&self.path, e))
    }
}

impl Drop for SftpWriter {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            close_detached(Arc::clone(&self.raw), handle);
        }
    }
}
