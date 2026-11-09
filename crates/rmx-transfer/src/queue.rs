//! The transfer queue: bounded, cancellable, resumable copies between any two [`RemoteFs`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::fs::{Entry, EntryKind, ListOptions, RemoteFs, WriteStart, join, read_all};

/// How many bytes before the resume point are compared with the source.
const RESUME_CHECK_BYTES: u64 = 64 * 1024;
/// Finished-file events kept for each transfer until the consumer receives them.
const FINISHED_EVENT_LIMIT: usize = 1024;

/// Identifies a transfer within one [`TransferQueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(u64);

/// What to do when the destination file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// Replace it.
    Overwrite,
    /// Leave it and count the file as skipped.
    Skip,
    /// Fail the file with [`Error::AlreadyExists`].
    Fail,
    /// Replace it only if the source is newer. Replaces when either time is unknown.
    IfNewer,
    /// Skip it if the sizes match, otherwise replace it. A partial file left by an earlier
    /// attempt, in any session, is continued.
    Resume,
}

/// One copy to perform.
#[derive(Clone)]
pub struct TransferRequest {
    /// Backend to read from.
    pub source: Arc<dyn RemoteFs>,
    /// File or directory to copy. A directory is copied recursively.
    pub source_path: String,
    /// Backend to write to.
    pub dest: Arc<dyn RemoteFs>,
    /// Exact destination path, not a directory to copy into. Its parent must exist.
    pub dest_path: String,
    /// What to do with destination files that exist.
    pub overwrite: OverwritePolicy,
}

impl std::fmt::Debug for TransferRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferRequest")
            .field("source_path", &self.source_path)
            .field("dest_path", &self.dest_path)
            .field("overwrite", &self.overwrite)
            .finish_non_exhaustive()
    }
}

/// Tuning for a [`TransferQueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    /// Files copied at once, across all transfers. Values below 1 are treated as 1.
    pub max_concurrent_files: usize,
    /// Automatic retries of a file after a transient failure. See [`Error::is_transient`].
    pub max_retries: u32,
    /// Wait before the first retry; doubles with each further retry.
    pub retry_backoff: Duration,
    /// Shortest time between two progress messages.
    pub progress_interval: Duration,
    /// Capacity of the progress channel. Values below 1 are treated as 1.
    pub progress_capacity: usize,
    /// Most entries in [`ProgressUpdate::files`] per message. Later ones wait for later messages.
    pub max_files_per_update: usize,
    /// Suffix of in-progress files. A file only takes its real name once completely written, so
    /// an interrupted copy never leaves a truncated file under the destination name.
    pub partial_suffix: String,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_concurrent_files: 4,
            max_retries: 3,
            retry_backoff: Duration::from_millis(250),
            progress_interval: Duration::from_millis(100),
            progress_capacity: 16,
            max_files_per_update: 64,
            partial_suffix: ".rmxpart".to_owned(),
        }
    }
}

/// Lifecycle of a transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Waiting for a concurrency slot.
    Queued,
    /// Copying.
    Running,
    /// Every file was copied or skipped.
    Done,
    /// At least one file could not be copied. Holds the most recent error message.
    Failed(String),
    /// Stopped by [`TransferQueue::cancel`].
    Cancelled,
}

impl TransferState {
    /// Returns whether the transfer has stopped running.
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done | Self::Failed(_) | Self::Cancelled)
    }
}

/// State of one file within a transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    /// Being copied.
    Active,
    /// Fully copied.
    Done,
    /// Left alone because of the [`OverwritePolicy`], or not a type that can be copied.
    Skipped,
    /// Could not be copied. Holds the error message.
    Failed(String),
    /// Stopped by cancellation. A partial file remains for a later resume.
    Cancelled,
}

/// Progress of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProgress {
    /// Transfer the file belongs to.
    pub transfer: TransferId,
    /// Path relative to the transfer's root; empty when the transfer is a single file.
    pub path: Arc<str>,
    /// Bytes present at the destination, including any resumed from an earlier attempt.
    pub bytes_done: u64,
    /// File size, if known.
    pub bytes_total: Option<u64>,
    /// Current state.
    pub status: FileStatus,
}

/// Progress of one transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    /// Which transfer this describes.
    pub id: TransferId,
    /// Current state.
    pub state: TransferState,
    /// Whether a directory is still being walked. While true the totals are still growing.
    pub scanning: bool,
    /// Files discovered so far.
    pub files_total: u64,
    /// Files copied.
    pub files_done: u64,
    /// Files skipped.
    pub files_skipped: u64,
    /// Files that failed.
    pub files_failed: u64,
    /// Bytes present at the destination.
    pub bytes_done: u64,
    /// Sum of the sizes of the files discovered so far.
    pub bytes_total: u64,
}

/// Totals across every transfer in the queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TotalProgress {
    /// Transfers not yet finished.
    pub transfers_active: u64,
    /// Transfers finished in any state.
    pub transfers_finished: u64,
    /// Files finished in any state.
    pub files_finished: u64,
    /// Files discovered.
    pub files_total: u64,
    /// Bytes present at destinations.
    pub bytes_done: u64,
    /// Bytes to transfer in total.
    pub bytes_total: u64,
}

/// A progress message. One is sent at most every [`QueueConfig::progress_interval`].
///
/// [`transfers`](Self::transfers) and [`files`](Self::files) hold only what changed since the last
/// message the consumer received. If the channel is full, nothing is lost: the changes merge into
/// the next message. So a final state always arrives, but a consumer that stops receiving never
/// slows a transfer down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// Totals as of this message.
    pub total: TotalProgress,
    /// Transfers whose progress or state changed.
    pub transfers: Vec<TransferProgress>,
    /// Files in progress, and files that finished, since the last message.
    pub files: Vec<FileProgress>,
}

struct ActiveFile {
    path: Arc<str>,
    bytes_done: AtomicU64,
    bytes_total: Option<u64>,
}

#[derive(Default)]
struct Counters {
    files_total: AtomicU64,
    files_done: AtomicU64,
    files_skipped: AtomicU64,
    files_failed: AtomicU64,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
}

impl Counters {
    fn reset(&self) {
        for counter in [
            &self.files_total,
            &self.files_done,
            &self.files_skipped,
            &self.files_failed,
            &self.bytes_done,
            &self.bytes_total,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

struct Job {
    id: TransferId,
    request: TransferRequest,
    state: Mutex<TransferState>,
    token: Mutex<CancellationToken>,
    counters: Counters,
    scanning: AtomicBool,
    /// Set when cancellation actually stopped work, as opposed to arriving just after the end.
    interrupted: AtomicBool,
    /// Whether this run was started by [`TransferQueue::retry`].
    retried: AtomicBool,
    active: Mutex<HashMap<Arc<str>, Arc<ActiveFile>>>,
    finished: Mutex<VecDeque<FileProgress>>,
    /// Files already copied by this transfer, so a retry of a directory does not repeat them.
    completed: Mutex<HashSet<Arc<str>>>,
    last_error: Mutex<Option<String>>,
    dirty: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("transfer queue lock")
}

impl Job {
    fn snapshot(&self) -> TransferProgress {
        let c = &self.counters;
        TransferProgress {
            id: self.id,
            state: lock(&self.state).clone(),
            scanning: self.scanning.load(Ordering::Relaxed),
            files_total: c.files_total.load(Ordering::Relaxed),
            files_done: c.files_done.load(Ordering::Relaxed),
            files_skipped: c.files_skipped.load(Ordering::Relaxed),
            files_failed: c.files_failed.load(Ordering::Relaxed),
            bytes_done: c.bytes_done.load(Ordering::Relaxed),
            bytes_total: c.bytes_total.load(Ordering::Relaxed),
        }
    }

    fn fail_message(&self, message: String) {
        *lock(&self.last_error) = Some(message);
    }
}

impl ActiveFile {
    fn progress(&self, transfer: TransferId, status: FileStatus) -> FileProgress {
        FileProgress {
            transfer,
            path: Arc::clone(&self.path),
            bytes_done: self.bytes_done.load(Ordering::Relaxed),
            bytes_total: self.bytes_total,
            status,
        }
    }
}

struct Inner {
    config: QueueConfig,
    runtime: Handle,
    permits: Arc<Semaphore>,
    jobs: Mutex<HashMap<TransferId, Arc<Job>>>,
    next_id: AtomicU64,
    dirty: Mutex<Vec<Arc<Job>>>,
    wake: Notify,
    progress: mpsc::Sender<ProgressUpdate>,
    running: watch::Sender<u64>,
}

impl Inner {
    /// Records that `job` has news for the next progress message.
    fn touch(&self, job: &Arc<Job>) {
        if !job.dirty.swap(true, Ordering::AcqRel) {
            lock(&self.dirty).push(Arc::clone(job));
            self.wake.notify_one();
        }
    }

    fn totals(&self) -> TotalProgress {
        let mut total = TotalProgress::default();
        let jobs = lock(&self.jobs);
        for job in jobs.values() {
            let c = &job.counters;
            if lock(&job.state).is_finished() {
                total.transfers_finished += 1;
            } else {
                total.transfers_active += 1;
            }
            total.files_finished += c.files_done.load(Ordering::Relaxed)
                + c.files_skipped.load(Ordering::Relaxed)
                + c.files_failed.load(Ordering::Relaxed);
            total.files_total += c.files_total.load(Ordering::Relaxed);
            total.bytes_done += c.bytes_done.load(Ordering::Relaxed);
            total.bytes_total += c.bytes_total.load(Ordering::Relaxed);
        }
        total
    }

    /// Sends one progress message if there is news. Never blocks.
    fn flush(&self) {
        let batch = std::mem::take(&mut *lock(&self.dirty));
        if batch.is_empty() {
            return;
        }
        // Clear the flags before reading, so a change made during the read marks the job again.
        for job in &batch {
            job.dirty.store(false, Ordering::Release);
        }
        let cap = self.config.max_files_per_update;
        let mut files = Vec::new();
        let mut taken: Vec<(usize, FileProgress)> = Vec::new();
        let mut more_events = vec![false; batch.len()];
        for (index, job) in batch.iter().enumerate() {
            for active in lock(&job.active).values() {
                if files.len() < cap {
                    files.push(active.progress(job.id, FileStatus::Active));
                }
            }
            let mut finished = lock(&job.finished);
            while files.len() < cap {
                let Some(event) = finished.pop_front() else { break };
                taken.push((index, event.clone()));
                files.push(event);
            }
            more_events[index] = !finished.is_empty();
        }
        let update = ProgressUpdate {
            total: self.totals(),
            transfers: batch.iter().map(|job| job.snapshot()).collect(),
            files,
        };
        match self.progress.try_send(update) {
            Ok(()) => {
                for (job, more) in batch.iter().zip(more_events) {
                    if more {
                        self.touch(job);
                    }
                }
            },
            Err(TrySendError::Full(_)) => {
                for (index, event) in taken.into_iter().rev() {
                    lock(&batch[index].finished).push_front(event);
                }
                for job in &batch {
                    self.touch(job);
                }
            },
            Err(TrySendError::Closed(_)) => {},
        }
    }

    fn finish_file(&self, job: &Arc<Job>, active: &Arc<ActiveFile>, status: FileStatus) {
        lock(&job.active).remove(&active.path);
        let c = &job.counters;
        match status {
            FileStatus::Done => c.files_done.fetch_add(1, Ordering::Relaxed),
            FileStatus::Skipped => c.files_skipped.fetch_add(1, Ordering::Relaxed),
            FileStatus::Failed(_) => c.files_failed.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        let event = active.progress(job.id, status);
        let mut finished = lock(&job.finished);
        if finished.len() == FINISHED_EVENT_LIMIT {
            finished.pop_front();
        }
        finished.push_back(event);
        drop(finished);
        self.touch(job);
    }

    fn register_file(&self, job: &Arc<Job>, path: Arc<str>, size: Option<u64>) -> Arc<ActiveFile> {
        let active = Arc::new(ActiveFile {
            path: Arc::clone(&path),
            bytes_done: 0.into(),
            bytes_total: size,
        });
        job.counters.files_total.fetch_add(1, Ordering::Relaxed);
        job.counters.bytes_total.fetch_add(size.unwrap_or(0), Ordering::Relaxed);
        lock(&job.active).insert(path, Arc::clone(&active));
        self.touch(job);
        active
    }

    /// Moves a file's byte count to `value`, keeping the job total in step.
    fn set_file_bytes(&self, job: &Job, active: &ActiveFile, value: u64) {
        let previous = active.bytes_done.swap(value, Ordering::Relaxed);
        job.counters.bytes_done.fetch_add(value.wrapping_sub(previous), Ordering::Relaxed);
    }

    async fn acquire(&self, token: &CancellationToken) -> Result<OwnedSemaphorePermit> {
        tokio::select! {
            biased;
            () = token.cancelled() => Err(Error::Cancelled),
            permit = Arc::clone(&self.permits).acquire_owned() => {
                permit.map_err(|_| Error::Cancelled)
            },
        }
    }

    async fn backoff(&self, attempt: u32, token: &CancellationToken) -> Result<()> {
        let delay = self.config.retry_backoff.saturating_mul(1 << attempt.min(16));
        tokio::select! {
            biased;
            () = token.cancelled() => Err(Error::Cancelled),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    }

    /// Runs `op`, repeating it after transient failures with backoff, until cancelled.
    async fn retrying<T, F, Fut>(&self, token: &CancellationToken, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempt = 0;
        loop {
            let result = tokio::select! {
                biased;
                () = token.cancelled() => Err(Error::Cancelled),
                result = op() => result,
            };
            match result {
                Err(error) if error.is_transient() && attempt < self.config.max_retries => {
                    log::debug!("retrying after: {error}");
                    self.backoff(attempt, token).await?;
                    attempt += 1;
                },
                other => return other,
            }
        }
    }
}

/// Whether a name from a listing is safe to append to a destination path.
fn is_safe_name(name: &str) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.contains('\0'))
}

enum CopyOutcome {
    Copied,
    Skipped,
}

/// Paths of one file to copy.
struct FilePaths {
    /// Path relative to the transfer's root.
    rel: Arc<str>,
    source: String,
    dest: String,
}

async fn tail_matches(job: &Job, source: &str, partial: &str, offset: u64) -> bool {
    let check = offset.min(RESUME_CHECK_BYTES);
    let start = offset - check;
    let source_tail = read_all(job.request.source.as_ref(), source, start, Some(check));
    let partial_tail = read_all(job.request.dest.as_ref(), partial, start, Some(check));
    match tokio::join!(source_tail, partial_tail) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Decides what happens to an existing destination. Returns `true` when the copy should be skipped.
async fn should_skip(job: &Job, dest: &str, source: &Entry) -> Result<bool> {
    let existing = match job.request.dest.stat(dest).await {
        Ok(existing) => existing,
        Err(Error::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    match job.request.overwrite {
        OverwritePolicy::Overwrite => Ok(false),
        OverwritePolicy::Skip => Ok(true),
        OverwritePolicy::Fail => Err(Error::AlreadyExists(dest.to_owned())),
        OverwritePolicy::IfNewer => {
            Ok(source.modified.zip(existing.modified).is_some_and(|(s, d)| s <= d))
        },
        OverwritePolicy::Resume => Ok(source.size.is_some() && source.size == existing.size),
    }
}

/// Runs one attempt at copying a file.
async fn copy_once(
    inner: &Inner,
    job: &Arc<Job>,
    active: &ActiveFile,
    paths: &FilePaths,
    source_entry: &Entry,
    resume: bool,
    token: &CancellationToken,
) -> Result<CopyOutcome> {
    if should_skip(job, &paths.dest, source_entry).await? {
        return Ok(CopyOutcome::Skipped);
    }
    let source = &job.request.source;
    let dest = &job.request.dest;
    let partial = format!("{}{}", paths.dest, inner.config.partial_suffix);
    let expected = source_entry.size;

    let mut offset = 0;
    if resume && let Ok(existing) = dest.stat(&partial).await {
        let size = existing.size.unwrap_or(0);
        if size > 0
            && expected.is_none_or(|expected| size <= expected)
            && tail_matches(job, &paths.source, &partial, size).await
        {
            offset = size;
        }
    }
    inner.set_file_bytes(job, active, offset);

    if expected != Some(offset) || offset == 0 {
        let start = if offset == 0 { WriteStart::Truncate } else { WriteStart::At(offset) };
        let mut writer = dest.write_range(&partial, start).await?;
        // A file already known to be complete has nothing to read; FTP servers refuse `REST` at
        // the end of a file, so the reader is opened only when bytes remain.
        let mut reader = match expected {
            Some(expected) if offset >= expected => None,
            _ => Some(source.read_range(&paths.source, offset, None).await?),
        };
        let mut done = offset;
        if let Some(reader) = &mut reader {
            loop {
                let chunk = tokio::select! {
                    biased;
                    () = token.cancelled() => return Err(Error::Cancelled),
                    chunk = reader.next_chunk() => chunk?,
                };
                let Some(chunk) = chunk else { break };
                let len = chunk.len() as u64;
                tokio::select! {
                    biased;
                    () = token.cancelled() => return Err(Error::Cancelled),
                    result = writer.write(chunk) => result?,
                }
                done += len;
                active.bytes_done.store(done, Ordering::Relaxed);
                job.counters.bytes_done.fetch_add(len, Ordering::Relaxed);
                inner.touch(job);
            }
        }
        drop(reader);
        tokio::select! {
            biased;
            () = token.cancelled() => return Err(Error::Cancelled),
            result = writer.finish() => result?,
        }
    }

    if let Some(expected) = expected {
        let written = dest.stat(&partial).await?;
        if written.size != Some(expected) {
            return Err(Error::SizeMismatch(format!(
                "{partial}: wrote {:?} of {expected} bytes",
                written.size
            )));
        }
    }
    dest.rename(&partial, &paths.dest).await?;
    inner.set_file_bytes(
        job,
        active,
        expected.unwrap_or(active.bytes_done.load(Ordering::Relaxed)),
    );
    Ok(CopyOutcome::Copied)
}

/// Copies one file with retries and records its outcome.
async fn run_file(
    inner: Arc<Inner>,
    job: Arc<Job>,
    active: Arc<ActiveFile>,
    paths: FilePaths,
    source_entry: Entry,
    token: CancellationToken,
    _permit: OwnedSemaphorePermit,
) {
    let mut attempt = 0;
    let result = loop {
        let resume = attempt > 0
            || job.retried.load(Ordering::Relaxed)
            || job.request.overwrite == OverwritePolicy::Resume;
        match copy_once(&inner, &job, &active, &paths, &source_entry, resume, &token).await {
            Err(error) if error.is_transient() && attempt < inner.config.max_retries => {
                log::debug!("retrying {}: {error}", paths.source);
                match inner.backoff(attempt, &token).await {
                    Ok(()) => attempt += 1,
                    Err(error) => break Err(error),
                }
            },
            other => break other,
        }
    };
    match result {
        Ok(CopyOutcome::Copied) => {
            lock(&job.completed).insert(Arc::clone(&active.path));
            inner.finish_file(&job, &active, FileStatus::Done);
        },
        Ok(CopyOutcome::Skipped) => {
            inner.set_file_bytes(&job, &active, active.bytes_total.unwrap_or(0));
            inner.finish_file(&job, &active, FileStatus::Skipped);
        },
        Err(Error::Cancelled) => {
            job.interrupted.store(true, Ordering::Relaxed);
            inner.finish_file(&job, &active, FileStatus::Cancelled);
        },
        Err(error) => {
            let message = error.to_string();
            job.fail_message(message.clone());
            inner.finish_file(&job, &active, FileStatus::Failed(message));
        },
    }
}

/// Starts [`run_file`] for a file, or counts it as done on a retry that already copied it.
fn start_file(
    inner: &Arc<Inner>,
    job: &Arc<Job>,
    tasks: &mut JoinSet<()>,
    paths: FilePaths,
    entry: Entry,
    token: &CancellationToken,
    permit: OwnedSemaphorePermit,
) {
    let active = inner.register_file(job, Arc::clone(&paths.rel), entry.size);
    if lock(&job.completed).contains(&paths.rel) {
        inner.set_file_bytes(job, &active, entry.size.unwrap_or(0));
        inner.finish_file(job, &active, FileStatus::Done);
        return;
    }
    tasks.spawn(run_file(
        Arc::clone(inner),
        Arc::clone(job),
        active,
        paths,
        entry,
        token.clone(),
        permit,
    ));
}

async fn ensure_dir(fs: &dyn RemoteFs, path: &str) -> Result<()> {
    match fs.stat(path).await {
        Ok(entry) if entry.kind == EntryKind::Dir => Ok(()),
        Ok(_) => Err(Error::AlreadyExists(format!("{path} exists and is not a directory"))),
        Err(Error::NotFound(_)) => match fs.mkdir(path).await {
            Ok(()) => Ok(()),
            // Another task may have created it in the meantime.
            Err(error) => match fs.stat(path).await {
                Ok(entry) if entry.kind == EntryKind::Dir => Ok(()),
                _ => Err(error),
            },
        },
        Err(error) => Err(error),
    }
}

fn record_walk_failure(inner: &Inner, job: &Arc<Job>, path: &str, error: Error) {
    if matches!(error, Error::Cancelled) {
        job.interrupted.store(true, Ordering::Relaxed);
        return;
    }
    let message = format!("{path}: {error}");
    job.counters.files_failed.fetch_add(1, Ordering::Relaxed);
    job.fail_message(message.clone());
    let event = FileProgress {
        transfer: job.id,
        path: path.into(),
        bytes_done: 0,
        bytes_total: None,
        status: FileStatus::Failed(message),
    };
    let mut finished = lock(&job.finished);
    if finished.len() == FINISHED_EVENT_LIMIT {
        finished.pop_front();
    }
    finished.push_back(event);
    drop(finished);
    inner.touch(job);
}

async fn walk_dir(inner: &Arc<Inner>, job: &Arc<Job>, token: &CancellationToken) {
    let source = Arc::clone(&job.request.source);
    let dest = Arc::clone(&job.request.dest);
    let root_source = job.request.source_path.clone();
    let root_dest = job.request.dest_path.clone();

    job.scanning.store(true, Ordering::Relaxed);
    let created = inner.retrying(token, || ensure_dir(dest.as_ref(), &root_dest)).await;
    if let Err(error) = created {
        record_walk_failure(inner, job, "", error);
        job.scanning.store(false, Ordering::Relaxed);
        return;
    }

    let mut tasks = JoinSet::new();
    let mut pending_dirs = vec![String::new()];
    while let Some(rel_dir) = pending_dirs.pop() {
        if token.is_cancelled() {
            job.interrupted.store(true, Ordering::Relaxed);
            break;
        }
        let source_dir =
            if rel_dir.is_empty() { root_source.clone() } else { join(&root_source, &rel_dir) };
        // The listing is read whole and released before any file starts. Files need connections
        // from the same pool a listing may hold.
        let listing = {
            let permit = inner.acquire(token).await;
            match permit {
                Ok(_permit) => {
                    inner
                        .retrying(token, || async {
                            source
                                .list(&source_dir, ListOptions::default())
                                .await?
                                .collect_all()
                                .await
                        })
                        .await
                },
                Err(error) => Err(error),
            }
        };
        let entries = match listing {
            Ok(entries) => entries,
            Err(error) => {
                record_walk_failure(inner, job, &rel_dir, error);
                continue;
            },
        };

        for mut entry in entries {
            let rel =
                if rel_dir.is_empty() { entry.name.clone() } else { join(&rel_dir, &entry.name) };
            if !is_safe_name(&entry.name) {
                record_walk_failure(
                    inner,
                    job,
                    &rel,
                    Error::InvalidPath(format!("unsafe name in listing: {:?}", entry.name)),
                );
                continue;
            }
            let source_path = join(&source_dir, &entry.name);
            if entry.kind == EntryKind::Symlink {
                // Links to files are copied as files; links to directories are skipped so that a
                // cycle cannot trap the walk.
                match inner.retrying(token, || source.stat(&source_path)).await {
                    Ok(target) if target.kind == EntryKind::File => {
                        entry = Entry { name: entry.name, ..target };
                    },
                    Ok(_) => {
                        skip_entry(inner, job, &rel);
                        continue;
                    },
                    Err(error) => {
                        record_walk_failure(inner, job, &rel, error);
                        continue;
                    },
                }
            }
            match entry.kind {
                EntryKind::Dir => {
                    let dest_dir = join(&root_dest, &rel);
                    match inner.retrying(token, || ensure_dir(dest.as_ref(), &dest_dir)).await {
                        Ok(()) => pending_dirs.push(rel),
                        Err(error) => record_walk_failure(inner, job, &rel, error),
                    }
                },
                EntryKind::File => {
                    let permit = match inner.acquire(token).await {
                        Ok(permit) => permit,
                        Err(error) => {
                            record_walk_failure(inner, job, &rel, error);
                            continue;
                        },
                    };
                    while tasks.try_join_next().is_some() {}
                    let paths = FilePaths {
                        dest: join(&root_dest, &rel),
                        rel: rel.into(),
                        source: source_path,
                    };
                    start_file(inner, job, &mut tasks, paths, entry, token, permit);
                },
                EntryKind::Symlink | EntryKind::Other => skip_entry(inner, job, &rel),
            }
        }
    }
    job.scanning.store(false, Ordering::Relaxed);
    inner.touch(job);
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined {
            job.fail_message(format!("file task failed: {error}"));
            job.counters.files_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn skip_entry(inner: &Inner, job: &Arc<Job>, rel: &str) {
    let active = inner.register_file(job, rel.into(), None);
    inner.finish_file(job, &active, FileStatus::Skipped);
}

async fn run_job(inner: Arc<Inner>, job: Arc<Job>, token: CancellationToken) {
    let root = async {
        let permit = inner.acquire(&token).await?;
        *lock(&job.state) = TransferState::Running;
        inner.touch(&job);
        let entry =
            inner.retrying(&token, || job.request.source.stat(&job.request.source_path)).await?;
        Ok::<_, Error>((entry, permit))
    }
    .await;

    match root {
        Ok((entry, _permit)) if entry.kind == EntryKind::Dir => {
            drop(_permit);
            walk_dir(&inner, &job, &token).await;
        },
        Ok((entry, permit)) if entry.kind == EntryKind::File => {
            let paths = FilePaths {
                rel: Arc::from(""),
                source: job.request.source_path.clone(),
                dest: job.request.dest_path.clone(),
            };
            let mut tasks = JoinSet::new();
            start_file(&inner, &job, &mut tasks, paths, entry, &token, permit);
            while tasks.join_next().await.is_some() {}
        },
        Ok((entry, _permit)) => {
            let error = Error::Unsupported(format!("{} is not a file or directory", entry.name));
            record_walk_failure(&inner, &job, "", error);
        },
        Err(error) => record_walk_failure(&inner, &job, "", error),
    }

    let state = if job.interrupted.load(Ordering::Relaxed) {
        TransferState::Cancelled
    } else if job.counters.files_failed.load(Ordering::Relaxed) > 0 {
        TransferState::Failed(
            lock(&job.last_error).clone().unwrap_or_else(|| "transfer failed".to_owned()),
        )
    } else {
        TransferState::Done
    };
    if state == TransferState::Done {
        lock(&job.completed).clear();
    }
    *lock(&job.state) = state;
    inner.touch(&job);
    inner.running.send_modify(|count| *count -= 1);
}

/// Runs copies in the background and reports their progress.
///
/// Dropping the queue cancels everything in it. Partial files remain and can be resumed by a later
/// transfer with [`OverwritePolicy::Resume`].
pub struct TransferQueue {
    inner: Arc<Inner>,
    shutdown: CancellationToken,
}

impl std::fmt::Debug for TransferQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferQueue").field("config", &self.inner.config).finish_non_exhaustive()
    }
}

impl TransferQueue {
    /// Creates a queue and the receiver for its [`ProgressUpdate`]s.
    ///
    /// Must be called from within a Tokio runtime, which the queue then keeps using. Its other
    /// methods can be called from any thread, including a UI thread outside the runtime.
    pub fn new(config: QueueConfig) -> (Self, mpsc::Receiver<ProgressUpdate>) {
        let config = QueueConfig {
            max_concurrent_files: config.max_concurrent_files.max(1),
            max_files_per_update: config.max_files_per_update.max(1),
            ..config
        };
        let (progress, receiver) = mpsc::channel(config.progress_capacity.max(1));
        let (running, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            permits: Arc::new(Semaphore::new(config.max_concurrent_files)),
            runtime: Handle::current(),
            config,
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            dirty: Mutex::new(Vec::new()),
            wake: Notify::new(),
            progress,
            running,
        });
        let shutdown = CancellationToken::new();
        tokio::spawn(report_progress(Arc::clone(&inner), shutdown.clone()));
        (Self { inner, shutdown }, receiver)
    }

    /// Adds a transfer. It starts as soon as a concurrency slot is free.
    pub fn submit(&self, request: TransferRequest) -> TransferId {
        let id = TransferId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let token = self.shutdown.child_token();
        let job = Arc::new(Job {
            id,
            request,
            state: Mutex::new(TransferState::Queued),
            token: Mutex::new(token.clone()),
            counters: Counters::default(),
            scanning: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            retried: AtomicBool::new(false),
            active: Mutex::new(HashMap::new()),
            finished: Mutex::new(VecDeque::new()),
            completed: Mutex::new(HashSet::new()),
            last_error: Mutex::new(None),
            dirty: AtomicBool::new(false),
        });
        lock(&self.inner.jobs).insert(id, Arc::clone(&job));
        self.start(job, token);
        id
    }

    fn start(&self, job: Arc<Job>, token: CancellationToken) {
        self.inner.running.send_modify(|count| *count += 1);
        self.inner.touch(&job);
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(run_job(inner, job, token));
    }

    /// Requests that a transfer stop. Returns `false` if it is unknown or already finished.
    ///
    /// The transfer reaches [`TransferState::Cancelled`] once its running files stop. Partial files
    /// are kept so that [`retry`](Self::retry) can continue them.
    pub fn cancel(&self, id: TransferId) -> bool {
        let Some(job) = lock(&self.inner.jobs).get(&id).cloned() else { return false };
        if lock(&job.state).is_finished() {
            return false;
        }
        lock(&job.token).cancel();
        true
    }

    /// Requests that every unfinished transfer stop.
    pub fn cancel_all(&self) {
        let jobs: Vec<_> = lock(&self.inner.jobs).values().cloned().collect();
        for job in jobs {
            if !lock(&job.state).is_finished() {
                lock(&job.token).cancel();
            }
        }
    }

    /// Runs a failed or cancelled transfer again. Returns `false` if it cannot be retried.
    ///
    /// Partial files are continued rather than restarted, and files this transfer already copied
    /// are not copied again.
    pub fn retry(&self, id: TransferId) -> bool {
        let Some(job) = lock(&self.inner.jobs).get(&id).cloned() else { return false };
        {
            let mut state = lock(&job.state);
            if !matches!(*state, TransferState::Failed(_) | TransferState::Cancelled) {
                return false;
            }
            *state = TransferState::Queued;
        }
        job.counters.reset();
        job.interrupted.store(false, Ordering::Relaxed);
        job.retried.store(true, Ordering::Relaxed);
        *lock(&job.last_error) = None;
        let token = self.shutdown.child_token();
        *lock(&job.token) = token.clone();
        self.start(job, token);
        true
    }

    /// Returns the current state of a transfer.
    pub fn state(&self, id: TransferId) -> Option<TransferState> {
        let job = lock(&self.inner.jobs).get(&id).cloned()?;
        let state = lock(&job.state).clone();
        Some(state)
    }

    /// Returns the current progress of every transfer, without consuming progress messages.
    pub fn snapshot(&self) -> ProgressUpdate {
        let mut transfers: Vec<_> =
            lock(&self.inner.jobs).values().map(|job| job.snapshot()).collect();
        transfers.sort_by_key(|transfer| transfer.id);
        ProgressUpdate { total: self.inner.totals(), transfers, files: Vec::new() }
    }

    /// Forgets finished transfers. They no longer count toward totals and cannot be retried.
    pub fn clear_finished(&self) {
        lock(&self.inner.jobs).retain(|_, job| !lock(&job.state).is_finished());
    }

    /// Waits until no transfer is queued or running.
    pub async fn wait_idle(&self) {
        let mut running = self.inner.running.subscribe();
        let _ = running.wait_for(|count| *count == 0).await;
    }
}

impl Drop for TransferQueue {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn report_progress(inner: Arc<Inner>, shutdown: CancellationToken) {
    loop {
        if lock(&inner.dirty).is_empty() {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = inner.wake.notified() => {},
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(inner.config.progress_interval) => {},
        }
        inner.flush();
    }
}

#[cfg(test)]
mod tests;
