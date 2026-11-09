//! SFTP backend tests against a real OpenSSH server in a container.
//!
//! Most tests run the session over `ssh -s <host> sftp`, so a real SSH client carries the stream and
//! this crate only sees stdin and stdout. `sftp_throughput_over_russh` uses a minimal `russh` client
//! instead, to measure the native stack.
//!
//! Run from the workspace root:
//!
//! ```text
//! CARGO_TARGET_DIR=target/agents/transfer cargo test -p rmx-transfer --release --test sftp_container -- --ignored --nocapture
//! ```
//!
//! The server listens on 127.0.0.1 only. The key and the user exist only for one run.

mod common;

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::{Container, RemoveOnDrop, pattern, unique, wait_for_port};
use rmx_transfer::{
    EntryKind, Error, ListOptions, LocalFs, OverwritePolicy, QueueConfig, RemoteFs, SftpFs,
    SftpOptions, TransferQueue, TransferRequest, TransferState, WriteStart, read_all,
};
use tempfile::TempDir;
use tokio::process::{Child, Command};

const USER: &str = "rmxtest";

struct Server {
    dir: TempDir,
    port: u16,
    container: Container,
}

impl Server {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", ""])
            .arg("-f")
            .arg(&key)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(
            key.with_extension("pub"),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
        let port = common::free_port_range(0);
        let script = format!(
            r#"set -e
apk add --no-cache openssh-server openssh-sftp-server >/dev/null
ssh-keygen -A >/dev/null
adduser -D -h /srv/data -s /bin/sh {USER}
passwd -u {USER} >/dev/null 2>&1 || true
mkdir -p /etc/ssh/keys
cp /ephemeral/id_ed25519.pub /etc/ssh/keys/{USER}
chmod 644 /etc/ssh/keys/{USER}
chown {USER} /srv/data
exec /usr/sbin/sshd -D -e -p {port} -o ListenAddress=127.0.0.1 -o PasswordAuthentication=no -o AuthorizedKeysFile=/etc/ssh/keys/%u -o StrictModes=no
"#
        );
        let container = Container::start("rmx-transfer-sshd", dir.path(), &script);
        wait_for_port(port, Duration::from_secs(120));
        Self { dir, port, container }
    }

    fn key_path(&self) -> std::path::PathBuf {
        self.dir.path().join("id_ed25519")
    }

    fn ssh_args(&self) -> Vec<String> {
        vec![
            "-i".into(),
            self.key_path().display().to_string(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
        ]
    }

    async fn open_sftp(&self, options: SftpOptions) -> (Arc<SftpFs>, Child) {
        let mut child = Command::new("ssh")
            .args(self.ssh_args())
            .args(["-p", &self.port.to_string(), "-s", &format!("{USER}@127.0.0.1"), "sftp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("start ssh");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let fs = SftpFs::connect(tokio::io::join(stdout, stdin), options).await.expect("sftp init");
        (Arc::new(fs), child)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs podman or docker and pulls an Alpine image"]
async fn sftp_backend_works_against_openssh() {
    let server = Server::start();
    let (sftp, _child) = server.open_sftp(SftpOptions::default()).await;
    let root = sftp.canonicalize(".").await.unwrap();
    assert_eq!(root, "/srv/data");

    file_operations(sftp.as_ref()).await;
    pipelined_round_trip(sftp.clone()).await;
    cancel_then_resume_upload(sftp.clone()).await;
    recursive_copy_both_ways(sftp.clone()).await;
    listing_of_100k_entries(sftp.clone(), &server.container).await;
    connection_loss_is_transient(&server).await;
}

async fn file_operations(fs: &SftpFs) {
    let dir = "/srv/data/ops";
    fs.mkdir(dir).await.unwrap();
    assert_eq!(fs.stat(dir).await.unwrap().kind, EntryKind::Dir);
    assert!(matches!(fs.mkdir(dir).await, Err(Error::Protocol(_))));

    let file = format!("{dir}/a.bin");
    let data = pattern(600 * 1024);
    let mut writer = fs.write_range(&file, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::copy_from_slice(&data[..300 * 1024])).await.unwrap();
    writer.write(Bytes::copy_from_slice(&data[300 * 1024..])).await.unwrap();
    writer.finish().await.unwrap();

    let entry = fs.stat(&file).await.unwrap();
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, Some(data.len() as u64));
    assert!(entry.modified.is_some());

    assert_eq!(read_all(fs, &file, 0, None).await.unwrap(), data);
    assert_eq!(read_all(fs, &file, 100, Some(70_000)).await.unwrap(), data[100..70_100]);
    assert!(read_all(fs, &file, data.len() as u64, None).await.unwrap().is_empty());

    let mut writer = fs.write_range(&file, WriteStart::At(10)).await.unwrap();
    writer.write(Bytes::from_static(b"XY")).await.unwrap();
    writer.finish().await.unwrap();
    assert_eq!(&read_all(fs, &file, 9, Some(4)).await.unwrap(), &[data[9], b'X', b'Y', data[12]]);

    let replaced = format!("{dir}/b.bin");
    let mut writer = fs.write_range(&replaced, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::from_static(b"old")).await.unwrap();
    writer.finish().await.unwrap();
    fs.rename(&file, &replaced).await.unwrap();
    assert_eq!(fs.stat(&replaced).await.unwrap().size, Some(data.len() as u64));
    assert!(matches!(fs.stat(&file).await, Err(Error::NotFound(_))));

    let entries = fs.list(dir, ListOptions::default()).await.unwrap().collect_all().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "b.bin");

    assert!(matches!(
        fs.list("/srv/data/nope", ListOptions::default()).await,
        Err(Error::NotFound(_))
    ));
    assert!(fs.remove_dir(dir).await.is_err(), "not empty");
    fs.remove_file(&replaced).await.unwrap();
    fs.remove_dir(dir).await.unwrap();
    assert!(matches!(fs.remove_file(&replaced).await, Err(Error::NotFound(_))));
}

async fn pipelined_round_trip(sftp: Arc<SftpFs>) {
    let tmp = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalFs::new());
    // Sizes around a request boundary, and one spanning many windows.
    for size in [0usize, 1, 32_768, 261_120, 261_121, 1_048_576, 37_000_000] {
        let data = pattern(size);
        let up = tmp.path().join("up");
        std::fs::write(&up, &data).unwrap();
        let (queue, _rx) = TransferQueue::new(QueueConfig::default());
        let remote_path = "/srv/data/round.bin".to_owned();
        let id = queue.submit(TransferRequest {
            source: local.clone(),
            source_path: up.to_str().unwrap().into(),
            dest: sftp.clone(),
            dest_path: remote_path.clone(),
            overwrite: OverwritePolicy::Overwrite,
        });
        queue.wait_idle().await;
        assert_eq!(queue.state(id), Some(TransferState::Done), "upload {size}");
        let down = tmp.path().join("down");
        let id = queue.submit(TransferRequest {
            source: sftp.clone(),
            source_path: remote_path.clone(),
            dest: local.clone(),
            dest_path: down.to_str().unwrap().into(),
            overwrite: OverwritePolicy::Overwrite,
        });
        queue.wait_idle().await;
        assert_eq!(queue.state(id), Some(TransferState::Done), "download {size}");
        assert_eq!(std::fs::read(&down).unwrap(), data, "round trip {size}");
        sftp.remove_file(&remote_path).await.unwrap();
        assert!(!Path::new(&format!("{}.rmxpart", down.display())).exists());
    }
}

async fn cancel_then_resume_upload(sftp: Arc<SftpFs>) {
    let tmp = tempfile::tempdir().unwrap();
    let data = pattern(256 * 1024 * 1024);
    let up = tmp.path().join("big");
    std::fs::write(&up, &data).unwrap();
    let (queue, _rx) = TransferQueue::new(QueueConfig::default());
    let remote = "/srv/data/resume.bin".to_owned();
    let partial = format!("{remote}.rmxpart");
    let id = queue.submit(TransferRequest {
        source: Arc::new(LocalFs::new()),
        source_path: up.to_str().unwrap().into(),
        dest: sftp.clone(),
        dest_path: remote.clone(),
        overwrite: OverwritePolicy::Overwrite,
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let bytes = queue.snapshot().transfers[0].bytes_done;
        if bytes > 8 * 1024 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline, "no progress");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(queue.cancel(id));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Cancelled));
    // In-flight writes settle after the handle closes, so wait for the size to stop changing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let size = sftp.stat(&partial).await.unwrap().size.unwrap();
    assert!(size > 0 && size < data.len() as u64, "partial is {size} bytes");
    assert!(matches!(sftp.stat(&remote).await, Err(Error::NotFound(_))));

    assert!(queue.retry(id));
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    let back = read_all(sftp.as_ref(), &remote, 0, None).await.unwrap();
    assert_eq!(back.len(), data.len());
    assert!(back == data, "resumed upload differs from source");
    sftp.remove_file(&remote).await.unwrap();
}

async fn recursive_copy_both_ways(sftp: Arc<SftpFs>) {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("tree");
    std::fs::create_dir_all(source.join("sub/deep")).unwrap();
    std::fs::write(source.join("root.txt"), b"root").unwrap();
    std::fs::write(source.join("sub/mid.bin"), pattern(1_000_000)).unwrap();
    std::fs::write(source.join("sub/deep/end.bin"), pattern(10)).unwrap();
    let local = Arc::new(LocalFs::new());
    let (queue, _rx) = TransferQueue::new(QueueConfig::default());
    let id = queue.submit(TransferRequest {
        source: local.clone(),
        source_path: source.to_str().unwrap().into(),
        dest: sftp.clone(),
        dest_path: "/srv/data/tree".into(),
        overwrite: OverwritePolicy::Fail,
    });
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));

    let back = tmp.path().join("back");
    let id = queue.submit(TransferRequest {
        source: sftp.clone(),
        source_path: "/srv/data/tree".into(),
        dest: local,
        dest_path: back.to_str().unwrap().into(),
        overwrite: OverwritePolicy::Fail,
    });
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done));
    assert_eq!(std::fs::read(back.join("root.txt")).unwrap(), b"root");
    assert_eq!(std::fs::read(back.join("sub/mid.bin")).unwrap(), pattern(1_000_000));
    assert_eq!(std::fs::read(back.join("sub/deep/end.bin")).unwrap(), pattern(10));
}

async fn listing_of_100k_entries(sftp: Arc<SftpFs>, container: &Container) {
    container.exec("mkdir /srv/data/big && cd /srv/data/big && seq -w 1 100000 | xargs touch && chown -R rmxtest /srv/data/big");
    let started = Instant::now();
    let mut stream = sftp
        .list("/srv/data/big", ListOptions { batch_size: 1000, ..ListOptions::default() })
        .await
        .unwrap();
    let mut total = 0;
    let mut first = None;
    while let Some(batch) = stream.next_batch().await.unwrap() {
        first.get_or_insert_with(|| started.elapsed());
        assert!(batch.len() <= 1000);
        total += batch.len();
    }
    assert_eq!(total, 100_000);
    println!(
        "sftp list: 100000 entries in {:?}, first batch after {:?}",
        started.elapsed(),
        first.unwrap()
    );
}

async fn connection_loss_is_transient(server: &Server) {
    let (sftp, mut child) = server.open_sftp(SftpOptions::default()).await;
    assert!(sftp.stat("/srv/data").await.is_ok());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let error = sftp.stat("/srv/data").await.unwrap_err();
    assert!(error.is_transient(), "{error:?}");
}

fn mib_per_sec(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64()
}

/// Two gigabytes of incompressible data on tmpfs at both ends.
const BENCH_SIZE: u64 = 2 * 1024 * 1024 * 1024;
const BENCH_ROUNDS: usize = 3;

struct Bench {
    server: Server,
    local_up: std::path::PathBuf,
    local_down: std::path::PathBuf,
    _cleanup: RemoveOnDrop,
}

impl Bench {
    fn prepare() -> Self {
        let server = Server::start();
        let id = unique("rmx-transfer-bench");
        let shm = Path::new("/dev/shm");
        let (local_up, local_down) = (shm.join(format!("{id}.up")), shm.join(format!("{id}.down")));
        server.container.exec(&format!(
            "head -c {BENCH_SIZE} /dev/urandom > /srv/data/big.bin && chown rmxtest /srv/data/big.bin"
        ));
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("head -c {BENCH_SIZE} /dev/urandom > {}", local_up.display()))
            .status()
            .unwrap();
        assert!(status.success());
        let cleanup = RemoveOnDrop(vec![local_up.clone(), local_down.clone()]);
        Self { server, local_up, local_down, _cleanup: cleanup }
    }

    fn reset(&self) {
        let _ = std::fs::remove_file(&self.local_down);
        self.server.container.exec("rm -f /srv/data/up.bin");
    }

    /// Times a queue download and upload over an existing session.
    async fn run_ours(&self, sftp: Arc<SftpFs>) -> (Duration, Duration) {
        let local = Arc::new(LocalFs::new());
        let (queue, _rx) = TransferQueue::new(QueueConfig::default());
        let started = Instant::now();
        let id = queue.submit(TransferRequest {
            source: sftp.clone(),
            source_path: "/srv/data/big.bin".into(),
            dest: local.clone(),
            dest_path: self.local_down.to_str().unwrap().into(),
            overwrite: OverwritePolicy::Overwrite,
        });
        queue.wait_idle().await;
        let download = started.elapsed();
        assert_eq!(queue.state(id), Some(TransferState::Done));
        assert_eq!(std::fs::metadata(&self.local_down).unwrap().len(), BENCH_SIZE);
        let started = Instant::now();
        let id = queue.submit(TransferRequest {
            source: local,
            source_path: self.local_up.to_str().unwrap().into(),
            dest: sftp,
            dest_path: "/srv/data/up.bin".into(),
            overwrite: OverwritePolicy::Overwrite,
        });
        queue.wait_idle().await;
        let upload = started.elapsed();
        assert_eq!(queue.state(id), Some(TransferState::Done));
        (download, upload)
    }

    /// Times the OpenSSH `sftp` client doing the same transfers.
    fn run_openssh(&self) -> (Duration, Duration) {
        let run = |command: String| {
            let path = Path::new("/dev/shm").join(unique("rmx-transfer-batch"));
            std::fs::write(&path, command).unwrap();
            let started = Instant::now();
            let output = std::process::Command::new("sftp")
                .args(self.server.ssh_args())
                .args(["-b", path.to_str().unwrap()])
                .args(["-P", &self.server.port.to_string()])
                .arg(format!("{USER}@127.0.0.1"))
                .output()
                .unwrap();
            let elapsed = started.elapsed();
            let _ = std::fs::remove_file(&path);
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            elapsed
        };
        let download = run(format!("get /srv/data/big.bin {}\n", self.local_down.display()));
        assert_eq!(std::fs::metadata(&self.local_down).unwrap().len(), BENCH_SIZE);
        let upload = run(format!("put {} /srv/data/up.bin\n", self.local_up.display()));
        (download, upload)
    }
}

/// Prints the comparison and returns the worst ratio of this crate to `sftp`.
fn report(label: &str, ours: &[(Duration, Duration)], openssh: &[(Duration, Duration)]) -> f64 {
    let rate = |t: &[(Duration, Duration)], pick: fn(&(Duration, Duration)) -> Duration| {
        let mut samples: Vec<Duration> = t.iter().map(pick).collect();
        samples.sort();
        mib_per_sec(BENCH_SIZE, samples[0])
    };
    let down = (rate(ours, |s| s.0), rate(openssh, |s| s.0));
    let up = (rate(ours, |s| s.1), rate(openssh, |s| s.1));
    println!(
        "{label}: download rmx-transfer {:.0} MiB/s, sftp {:.0} MiB/s ({:.0}%)",
        down.0,
        down.1,
        down.0 / down.1 * 100.0
    );
    println!(
        "{label}: upload   rmx-transfer {:.0} MiB/s, sftp {:.0} MiB/s ({:.0}%)",
        up.0,
        up.1,
        up.0 / up.1 * 100.0
    );
    (down.0 / down.1).min(up.0 / up.1)
}

/// Compares this crate with the OpenSSH `sftp` client over loopback, using the same `ssh` client
/// process for the transport, and tmpfs at both ends. The 90% budget must hold in each direction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs podman or docker and about 2 GiB of tmpfs"]
async fn sftp_throughput_vs_openssh_client() {
    let bench = Bench::prepare();
    let (mut ours, mut openssh) = (Vec::new(), Vec::new());
    for _ in 0..BENCH_ROUNDS {
        let (sftp, child) = bench.server.open_sftp(SftpOptions::default()).await;
        ours.push(bench.run_ours(sftp).await);
        drop(child);
        bench.reset();
        openssh.push(bench.run_openssh());
        bench.reset();
    }
    let worst = report("over ssh child", &ours, &openssh);
    assert!(worst >= 0.9, "below the 90% budget: {worst}");
}

struct AcceptAnyKey;

impl russh::client::Handler for AcceptAnyKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Measures the whole native stack: `russh` with `ring` for SSH, this crate for SFTP. Numbers are
/// printed but not asserted, because they depend on SSH tuning that belongs to the SSH layer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs podman or docker and about 2 GiB of tmpfs"]
async fn sftp_throughput_over_russh() {
    use russh::keys::{PrivateKeyWithHashAlg, load_secret_key};

    let bench = Bench::prepare();
    let key = load_secret_key(bench.server.key_path(), None).unwrap();
    let (mut ours, mut openssh) = (Vec::new(), Vec::new());
    for _ in 0..BENCH_ROUNDS {
        let config = russh::client::Config {
            window_size: 16 * 1024 * 1024,
            nodelay: true,
            ..russh::client::Config::default()
        };
        let mut session = russh::client::connect(
            Arc::new(config),
            ("127.0.0.1", bench.server.port),
            AcceptAnyKey,
        )
        .await
        .unwrap();
        let authenticated = session
            .authenticate_publickey(USER, PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None))
            .await
            .unwrap();
        assert!(authenticated.success());
        let channel = session.channel_open_session().await.unwrap();
        channel.request_subsystem(true, "sftp").await.unwrap();
        let sftp =
            Arc::new(SftpFs::connect(channel.into_stream(), SftpOptions::default()).await.unwrap());
        ours.push(bench.run_ours(sftp).await);
        bench.reset();
        openssh.push(bench.run_openssh());
        bench.reset();
    }
    report("over russh", &ours, &openssh);
}
