use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use tempfile::TempDir;
use tokio::sync::Semaphore;

use super::*;
use crate::fs::{DirStream, ListOptions, RangeRead, RangeWrite, RemoteFs, WriteStart};
use crate::local::LocalFs;

const CHUNK: usize = 64 * 1024;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(2654435761).to_le_bytes()[3]).collect()
}

fn config() -> QueueConfig {
    QueueConfig {
        retry_backoff: Duration::from_millis(1),
        progress_interval: Duration::from_millis(5),
        ..QueueConfig::default()
    }
}

/// A local backend with fault injection and instrumentation.
struct TestFs {
    inner: LocalFs,
    /// When set, every chunk read must first take a permit.
    gate: Option<Arc<Semaphore>>,
    /// The next reads fail with this error once each has produced `after` bytes.
    fail: StdMutex<Option<(u64, Error, usize)>>,
    bytes_read: AtomicU64,
    open_readers: AtomicUsize,
    max_open_readers: AtomicUsize,
    /// Names added to listings of the root directory, to imitate a hostile server.
    extra_entries: StdMutex<Vec<Entry>>,
}

impl TestFs {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: LocalFs::with_chunk_size(CHUNK),
            gate: None,
            fail: StdMutex::new(None),
            bytes_read: AtomicU64::new(0),
            open_readers: AtomicUsize::new(0),
            max_open_readers: AtomicUsize::new(0),
            extra_entries: StdMutex::new(Vec::new()),
        })
    }

    fn gated(gate: Arc<Semaphore>) -> Arc<Self> {
        let mut fs = Arc::into_inner(Self::new()).unwrap();
        fs.gate = Some(gate);
        Arc::new(fs)
    }
}

struct TestReader {
    fs: Arc<TestFs>,
    inner: Box<dyn RangeRead>,
    failure: Option<(u64, Error)>,
    produced: u64,
}

#[async_trait]
impl RangeRead for TestReader {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        if let Some((after, error)) = &self.failure
            && self.produced >= *after
        {
            return Err(error.clone());
        }
        if let Some(gate) = &self.fs.gate {
            gate.acquire().await.unwrap().forget();
        }
        let chunk = self.inner.next_chunk().await?;
        if let Some(chunk) = &chunk {
            self.produced += chunk.len() as u64;
            self.fs.bytes_read.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        Ok(chunk)
    }
}

impl Drop for TestReader {
    fn drop(&mut self) {
        self.fs.open_readers.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Wrapper so the trait can be implemented on a shared handle.
#[derive(Clone)]
struct Shared(Arc<TestFs>);

#[async_trait]
impl RemoteFs for Shared {
    async fn list(&self, path: &str, options: ListOptions) -> Result<DirStream> {
        let extra = self.0.extra_entries.lock().unwrap().clone();
        if extra.is_empty() {
            return self.0.inner.list(path, options).await;
        }
        let (tx, stream) = DirStream::channel();
        let mut entries = self.0.inner.list(path, options).await?.collect_all().await?;
        entries.extend(extra);
        tx.send(Ok(entries)).await.unwrap();
        Ok(stream)
    }

    async fn stat(&self, path: &str) -> Result<Entry> {
        self.0.inner.stat(path).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        self.0.inner.mkdir(path).await
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        self.0.inner.remove_file(path).await
    }

    async fn remove_dir(&self, path: &str) -> Result<()> {
        self.0.inner.remove_dir(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.0.inner.rename(from, to).await
    }

    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Box<dyn RangeRead>> {
        let inner = self.0.inner.read_range(path, offset, len).await?;
        let open = self.0.open_readers.fetch_add(1, Ordering::Relaxed) + 1;
        self.0.max_open_readers.fetch_max(open, Ordering::Relaxed);
        let mut slot = self.0.fail.lock().unwrap();
        let mut failure = None;
        if let Some((after, error, remaining)) = slot.as_mut() {
            failure = Some((*after, error.clone()));
            *remaining -= 1;
            if *remaining == 0 {
                *slot = None;
            }
        }
        Ok(Box::new(TestReader { fs: Arc::clone(&self.0), inner, failure, produced: 0 }))
    }

    async fn write_range(&self, path: &str, start: WriteStart) -> Result<Box<dyn RangeWrite>> {
        self.0.inner.write_range(path, start).await
    }
}

struct Fixture {
    _dir: TempDir,
    root: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_owned();
        Self { _dir: dir, root }
    }

    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.root)
    }

    fn write(&self, name: &str, data: &[u8]) {
        let path = self.path(name);
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path(name)).unwrap()
    }

    fn exists(&self, name: &str) -> bool {
        std::path::Path::new(&self.path(name)).exists()
    }
}

fn request(
    source: Arc<dyn RemoteFs>,
    dest: Arc<dyn RemoteFs>,
    fixture: &Fixture,
    from: &str,
    to: &str,
    overwrite: OverwritePolicy,
) -> TransferRequest {
    TransferRequest {
        source,
        source_path: fixture.path(from),
        dest,
        dest_path: fixture.path(to),
        overwrite,
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn copies_a_single_file_and_reports_completion() {
    let fixture = Fixture::new();
    let data = pattern(3 * CHUNK + 17);
    fixture.write("src.bin", &data);
    let fs = Arc::new(LocalFs::new());
    let (queue, mut progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        fs.clone(),
        fs,
        &fixture,
        "src.bin",
        "dst.bin",
        OverwritePolicy::Overwrite,
    ));
    queue.wait_idle().await;

    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("dst.bin"), data);
    assert!(!fixture.exists("dst.bin.rmxpart"), "the partial file takes the final name");

    let mut last = None;
    while last.as_ref().is_none_or(|t: &TransferProgress| !t.state.is_finished()) {
        let update = progress.recv().await.unwrap();
        if let Some(t) = update.transfers.iter().find(|t| t.id == id) {
            last = Some(t.clone());
        }
    }
    let last = last.unwrap();
    assert_eq!(last.bytes_done, data.len() as u64);
    assert_eq!(last.bytes_total, data.len() as u64);
    assert_eq!(last.files_done, 1);
}

#[tokio::test]
async fn copies_a_directory_tree_recursively() {
    let fixture = Fixture::new();
    fixture.write("tree/a.txt", b"aaa");
    fixture.write("tree/sub/b.txt", &pattern(CHUNK * 2));
    fixture.write("tree/sub/deep/c.txt", b"");
    std::fs::create_dir(fixture.path("tree/empty")).unwrap();

    let fs = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(fs.clone(), fs, &fixture, "tree", "out", OverwritePolicy::Fail));
    queue.wait_idle().await;

    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("out/a.txt"), b"aaa");
    assert_eq!(fixture.read("out/sub/b.txt"), pattern(CHUNK * 2));
    assert_eq!(fixture.read("out/sub/deep/c.txt"), b"");
    assert!(std::path::Path::new(&fixture.path("out/empty")).is_dir());
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.total.files_total, 3);
    assert_eq!(snapshot.total.files_finished, 3);
}

#[tokio::test]
async fn never_exceeds_the_concurrency_limit() {
    let fixture = Fixture::new();
    for i in 0..24 {
        fixture.write(&format!("many/{i}"), &pattern(CHUNK * 4));
    }
    let gate = Arc::new(Semaphore::new(0));
    let source = TestFs::gated(Arc::clone(&gate));
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) =
        TransferQueue::new(QueueConfig { max_concurrent_files: 3, ..config() });
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "many",
        "copy",
        OverwritePolicy::Overwrite,
    ));
    // With no permits, exactly the limit should be parked inside a read.
    wait_for(|| source.open_readers.load(Ordering::Relaxed) == 3).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(source.open_readers.load(Ordering::Relaxed), 3);

    gate.add_permits(1 << 20);
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(source.max_open_readers.load(Ordering::Relaxed), 3);
    for i in 0..24 {
        assert_eq!(fixture.read(&format!("copy/{i}")), pattern(CHUNK * 4));
    }
}

#[tokio::test]
async fn cancel_keeps_the_partial_and_retry_resumes_it() {
    let fixture = Fixture::new();
    let data = pattern(CHUNK * 40);
    fixture.write("big.bin", &data);
    let gate = Arc::new(Semaphore::new(10));
    let source = TestFs::gated(Arc::clone(&gate));
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "big.bin",
        "out.bin",
        OverwritePolicy::Overwrite,
    ));
    wait_for(|| source.bytes_read.load(Ordering::Relaxed) == (CHUNK * 10) as u64).await;
    wait_for(|| {
        queue
            .snapshot()
            .transfers
            .iter()
            .any(|t| t.bytes_done == (CHUNK * 10) as u64 && t.state == TransferState::Running)
    })
    .await;

    assert!(queue.cancel(id));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Cancelled));
    assert!(!fixture.exists("out.bin"), "the destination name appears only when complete");
    let partial = fixture.read("out.bin.rmxpart");
    assert_eq!(partial.len(), CHUNK * 10);
    assert_eq!(partial, data[..CHUNK * 10]);

    let read_before = source.bytes_read.load(Ordering::Relaxed);
    gate.add_permits(1 << 20);
    assert!(queue.retry(id));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("out.bin"), data);
    let read_after = source.bytes_read.load(Ordering::Relaxed) - read_before;
    let resume_check = RESUME_CHECK_BYTES;
    // Only the remainder, plus the resume-point verification, comes from the source.
    assert_eq!(read_after, (CHUNK * 30) as u64 + resume_check);

    // Resuming is reported in the byte counts rather than restarting from zero.
    let done = queue.snapshot();
    assert_eq!(done.transfers[0].bytes_done, data.len() as u64);
}

#[tokio::test]
async fn cancel_of_a_queued_transfer_never_starts_it() {
    let fixture = Fixture::new();
    fixture.write("a", &pattern(CHUNK * 8));
    fixture.write("b", &pattern(CHUNK));
    let gate = Arc::new(Semaphore::new(0));
    let source = TestFs::gated(Arc::clone(&gate));
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) =
        TransferQueue::new(QueueConfig { max_concurrent_files: 1, ..config() });
    let first = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest.clone(),
        &fixture,
        "a",
        "a.out",
        OverwritePolicy::Overwrite,
    ));
    wait_for(|| source.open_readers.load(Ordering::Relaxed) == 1).await;
    let second = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "b",
        "b.out",
        OverwritePolicy::Overwrite,
    ));
    assert_eq!(queue.state(second), Some(TransferState::Queued));
    assert!(queue.cancel(second));
    wait_for(|| queue.state(second) == Some(TransferState::Cancelled)).await;
    assert!(!fixture.exists("b.out.rmxpart"));

    gate.add_permits(1 << 20);
    queue.wait_idle().await;
    assert_eq!(queue.state(first), Some(TransferState::Done));
    assert!(!queue.retry(first), "a finished transfer has nothing to retry");
}

#[tokio::test]
async fn transient_errors_retry_automatically_and_resume() {
    let fixture = Fixture::new();
    let data = pattern(CHUNK * 8);
    fixture.write("src", &data);
    let source = TestFs::new();
    *source.fail.lock().unwrap() =
        Some((CHUNK as u64 * 4, Error::Connection("link reset".into()), 2));
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "src",
        "dst",
        OverwritePolicy::Overwrite,
    ));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("dst"), data);
    assert!(source.bytes_read.load(Ordering::Relaxed) < (CHUNK * 16) as u64, "restarted from zero");
}

#[tokio::test]
async fn permanent_errors_fail_without_retrying_then_manual_retry_succeeds() {
    let fixture = Fixture::new();
    let data = pattern(CHUNK * 6);
    fixture.write("src", &data);
    let source = TestFs::new();
    *source.fail.lock().unwrap() = Some((CHUNK as u64, Error::PermissionDenied("nope".into()), 1));
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "src",
        "dst",
        OverwritePolicy::Overwrite,
    ));
    queue.wait_idle().await;
    assert!(matches!(queue.state(id), Some(TransferState::Failed(m)) if m.contains("nope")));
    assert_eq!(source.bytes_read.load(Ordering::Relaxed), CHUNK as u64);

    assert!(queue.retry(id));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("dst"), data);
}

#[tokio::test]
async fn gives_up_after_the_retry_limit() {
    let fixture = Fixture::new();
    fixture.write("src", &pattern(CHUNK));
    let source = TestFs::new();
    *source.fail.lock().unwrap() = Some((0, Error::Timeout("slow".into()), 99));
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(QueueConfig { max_retries: 2, ..config() });
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "src",
        "dst",
        OverwritePolicy::Overwrite,
    ));
    queue.wait_idle().await;
    assert!(matches!(queue.state(id), Some(TransferState::Failed(_))));
    assert_eq!(source.fail.lock().unwrap().as_ref().unwrap().2, 96, "one attempt plus two retries");
}

#[tokio::test]
async fn a_failed_directory_copy_retries_only_what_is_missing() {
    let fixture = Fixture::new();
    fixture.write("tree/ok", &pattern(CHUNK));
    fixture.write("tree/bad", &pattern(CHUNK));
    std::fs::create_dir(fixture.path("out")).unwrap();
    // A directory in the way makes the rename of this one file fail.
    std::fs::create_dir(fixture.path("out/bad")).unwrap();
    let source = TestFs::new();
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "tree",
        "out",
        OverwritePolicy::Overwrite,
    ));
    queue.wait_idle().await;
    assert!(matches!(queue.state(id), Some(TransferState::Failed(_))));
    assert_eq!(fixture.read("out/ok"), pattern(CHUNK));
    assert_eq!(queue.snapshot().transfers[0].files_failed, 1);

    std::fs::remove_dir(fixture.path("out/bad")).unwrap();
    let before = source.bytes_read.load(Ordering::Relaxed);
    assert!(queue.retry(id));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("out/bad"), pattern(CHUNK));
    // `ok` is not read again; `bad` continues from its complete partial without new data.
    assert!(source.bytes_read.load(Ordering::Relaxed) - before <= RESUME_CHECK_BYTES);
}

#[tokio::test]
async fn overwrite_policies_decide_what_happens_to_existing_files() {
    let fixture = Fixture::new();
    fixture.write("src", b"new contents");
    let fs: Arc<LocalFs> = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());

    let cases: [(OverwritePolicy, &[u8], bool); 4] = [
        (OverwritePolicy::Skip, b"old", false),
        (OverwritePolicy::Overwrite, b"old", true),
        (OverwritePolicy::Resume, b"different sz", false), // same length: treated as complete
        (OverwritePolicy::Resume, b"short", true),
    ];
    for (i, (policy, existing, replaced)) in cases.into_iter().enumerate() {
        let name = format!("dst{i}");
        fixture.write(&name, existing);
        let id = queue.submit(request(fs.clone(), fs.clone(), &fixture, "src", &name, policy));
        queue.wait_idle().await;
        assert_eq!(queue.state(id), Some(TransferState::Done), "{policy:?}");
        let expected: &[u8] = if replaced { b"new contents" } else { existing };
        assert_eq!(fixture.read(&name), expected, "{policy:?}");
    }

    fixture.write("exists", b"keep");
    let id = queue.submit(request(
        fs.clone(),
        fs.clone(),
        &fixture,
        "src",
        "exists",
        OverwritePolicy::Fail,
    ));
    queue.wait_idle().await;
    assert!(
        matches!(queue.state(id), Some(TransferState::Failed(m)) if m.contains("already exists"))
    );
    assert_eq!(fixture.read("exists"), b"keep");
}

#[tokio::test]
async fn if_newer_compares_modification_times() {
    let fixture = Fixture::new();
    fixture.write("src", b"source");
    fixture.write("newer_dst", b"dst");
    fixture.write("older_dst", b"dst");
    let now = std::time::SystemTime::now();
    let set = |name: &str, time| {
        let file = std::fs::File::open(fixture.path(name)).unwrap();
        file.set_modified(time).unwrap();
    };
    set("src", now);
    set("newer_dst", now + Duration::from_secs(60));
    set("older_dst", now - Duration::from_secs(60));

    let fs: Arc<LocalFs> = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    queue.submit(request(
        fs.clone(),
        fs.clone(),
        &fixture,
        "src",
        "newer_dst",
        OverwritePolicy::IfNewer,
    ));
    queue.submit(request(
        fs.clone(),
        fs.clone(),
        &fixture,
        "src",
        "older_dst",
        OverwritePolicy::IfNewer,
    ));
    queue.wait_idle().await;
    assert_eq!(fixture.read("newer_dst"), b"dst");
    assert_eq!(fixture.read("older_dst"), b"source");
}

#[tokio::test]
async fn resume_policy_continues_a_partial_from_an_earlier_session() {
    let fixture = Fixture::new();
    let data = pattern(CHUNK * 5);
    fixture.write("src", &data);
    fixture.write("dst.rmxpart", &data[..CHUNK * 2]);
    let source = TestFs::new();
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        Arc::new(Shared(Arc::clone(&source))),
        dest,
        &fixture,
        "src",
        "dst",
        OverwritePolicy::Resume,
    ));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("dst"), data);
    assert_eq!(source.bytes_read.load(Ordering::Relaxed), (CHUNK * 3) as u64 + RESUME_CHECK_BYTES);
}

#[tokio::test]
async fn resume_restarts_when_the_partial_does_not_match_the_source() {
    let fixture = Fixture::new();
    let data = pattern(CHUNK * 3);
    fixture.write("src", &data);
    // Same length as a valid partial, but different bytes.
    fixture.write("dst.rmxpart", &vec![0xAB; CHUNK * 2]);
    let fs: Arc<LocalFs> = Arc::new(LocalFs::with_chunk_size(CHUNK));
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(fs.clone(), fs, &fixture, "src", "dst", OverwritePolicy::Resume));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("dst"), data);
}

#[tokio::test]
async fn progress_is_rate_limited_and_final_state_survives_a_full_channel() {
    let fixture = Fixture::new();
    let data = pattern(CHUNK * 64);
    fixture.write("src", &data);
    let fs: Arc<LocalFs> = Arc::new(LocalFs::with_chunk_size(4096));
    let (queue, mut progress) = TransferQueue::new(QueueConfig {
        progress_capacity: 2,
        progress_interval: Duration::from_millis(20),
        ..config()
    });
    let id =
        queue.submit(request(fs.clone(), fs, &fixture, "src", "dst", OverwritePolicy::Overwrite));
    queue.wait_idle().await;
    // Give the reporter time to find the channel full, as with a consumer that has gone quiet.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut received = 0;
    let mut finished = false;
    while !finished {
        let update = tokio::time::timeout(Duration::from_secs(5), progress.recv())
            .await
            .expect("the final state is delivered")
            .unwrap();
        received += 1;
        finished = update.transfers.iter().any(|t| t.id == id && t.state.is_finished());
    }
    // 1,000 chunks of progress must not become 1,000 messages.
    assert!(received < 100, "received {received} messages");
    assert!(progress.try_recv().is_err());
}

#[tokio::test]
async fn walking_hostile_names_fails_only_those_entries() {
    let fixture = Fixture::new();
    fixture.write("tree/good", b"fine");
    std::fs::create_dir(fixture.path("out")).unwrap();
    let source = TestFs::new();
    source.extra_entries.lock().unwrap().push(Entry {
        name: "../escape".to_owned(),
        kind: EntryKind::File,
        size: Some(4),
        modified: None,
        mode: None,
    });
    let dest = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(
        Arc::new(Shared(source)),
        dest,
        &fixture,
        "tree",
        "out",
        OverwritePolicy::Overwrite,
    ));
    queue.wait_idle().await;
    assert!(matches!(queue.state(id), Some(TransferState::Failed(m)) if m.contains("unsafe name")));
    assert_eq!(fixture.read("out/good"), b"fine");
    assert!(!fixture.exists("escape"));
}

#[tokio::test]
async fn symlinks_are_copied_as_files_but_never_followed_into_directories() {
    let fixture = Fixture::new();
    fixture.write("tree/real", b"data");
    std::os::unix::fs::symlink(fixture.path("tree/real"), fixture.path("tree/link")).unwrap();
    std::os::unix::fs::symlink(fixture.path("tree"), fixture.path("tree/cycle")).unwrap();
    let fs = Arc::new(LocalFs::new());
    let (queue, _progress) = TransferQueue::new(config());
    let id = queue.submit(request(fs.clone(), fs, &fixture, "tree", "out", OverwritePolicy::Fail));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(fixture.read("out/link"), b"data");
    assert!(!fixture.exists("out/cycle"));
    assert_eq!(queue.snapshot().transfers[0].files_skipped, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listing_100k_entries_streams_in_bounded_batches() {
    let fixture = Fixture::new();
    let root = fixture.path("big");
    std::fs::create_dir(&root).unwrap();
    for i in 0..100_000 {
        std::fs::File::create(format!("{root}/f{i:06}")).unwrap();
    }
    let fs = LocalFs::new();
    let started = Instant::now();
    let mut stream = fs
        .list(&root, ListOptions { batch_size: 500, flush_after: Duration::from_secs(60) })
        .await
        .unwrap();
    let mut total = 0usize;
    let mut first_batch_after = None;
    let mut batches = 0usize;
    while let Some(batch) = stream.next_batch().await.unwrap() {
        first_batch_after.get_or_insert_with(|| started.elapsed());
        assert!(batch.len() <= 500);
        batches += 1;
        total += batch.len();
    }
    assert_eq!(total, 100_000);
    assert_eq!(batches, 200);
    let elapsed = started.elapsed();
    let first = first_batch_after.unwrap();
    assert!(first < elapsed / 2, "first batch at {first:?} of {elapsed:?}");
}

#[tokio::test]
async fn dropping_the_listing_stops_the_producer() {
    let fixture = Fixture::new();
    let root = fixture.path("dir");
    std::fs::create_dir(&root).unwrap();
    for i in 0..1000 {
        std::fs::File::create(format!("{root}/{i}")).unwrap();
    }
    let fs = LocalFs::new();
    let mut stream =
        fs.list(&root, ListOptions { batch_size: 1, ..ListOptions::default() }).await.unwrap();
    assert_eq!(stream.next_batch().await.unwrap().unwrap().len(), 1);
    drop(stream);
    // The producer's next send fails; nothing here can observe it except not hanging.
    tokio::time::sleep(Duration::from_millis(20)).await;
}
