//! FTP and FTPS backend tests against vsftpd in a container.
//!
//! Run from the workspace root:
//!
//! ```text
//! CARGO_TARGET_DIR=target/agents/transfer cargo test -p rmx-transfer --test ftp_container -- --ignored --nocapture
//! ```
//!
//! The server listens on 127.0.0.1 only. The certificate, user and password exist only for one
//! run.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use common::{Container, pattern, random_secret, wait_for_port};
use rmx_transfer::rustls::pki_types::CertificateDer;
use rmx_transfer::rustls::{ClientConfig, RootCertStore, crypto};
use rmx_transfer::{
    DialedStream, EntryKind, Error, FtpConfig, FtpDialer, FtpFs, FtpTls, ListOptions, LocalFs,
    OverwritePolicy, QueueConfig, RemoteFs, TransferQueue, TransferRequest, TransferState,
    WriteStart, read_all,
};
use tempfile::TempDir;
use tokio::net::TcpStream;

const USER: &str = "rmxftp";

struct Server {
    _dir: TempDir,
    port: u16,
    password: String,
    certificate: CertificateDer<'static>,
    container: Container,
}

fn sh(command: &str) {
    let status = std::process::Command::new("sh").arg("-c").arg(command).status().unwrap();
    assert!(status.success(), "{command}");
}

impl Server {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().display().to_string();
        sh(&format!(
            "cd {path} && openssl ecparam -name prime256v1 -genkey -noout -out ca.key 2>/dev/null \
             && openssl req -x509 -new -key ca.key -days 2 -subj /CN=rmx-test-ca -addext \
             basicConstraints=critical,CA:TRUE -out ca.pem 2>/dev/null && openssl ecparam -name \
             prime256v1 -genkey -noout -out key.pem 2>/dev/null && openssl req -new -key key.pem \
             -subj /CN=localhost -out leaf.csr 2>/dev/null && printf \
             'basicConstraints=CA:FALSE\\nsubjectAltName=DNS:localhost\\n' > leaf.ext && openssl \
             x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 -extfile \
             leaf.ext -out cert.pem 2>/dev/null && chmod 755 . && chmod 644 key.pem cert.pem \
             ca.pem"
        ));
        let pem = std::fs::read(dir.path().join("ca.pem")).unwrap();
        let certificate = rustls_pemfile::certs(&mut pem.as_slice()).next().unwrap().unwrap();

        let port = common::free_port_range(32);
        let password = random_secret();
        let (low, high) = (port + 1, port + 32);
        let config = format!(
            "listen=YES\nlisten_ipv6=NO\nlisten_address=127.0.0.1\nlisten_port={port}\\
             nbackground=NO\nanonymous_enable=NO\nlocal_enable=YES\nwrite_enable=YES\\
             nlocal_umask=022\ncheck_shell=NO\nseccomp_sandbox=NO\nuse_localtime=NO\\
             npasv_enable=YES\npasv_address=127.0.0.1\npasv_min_port={low}\npasv_max_port={high}\\
             nssl_enable=YES\nallow_anon_ssl=NO\nforce_local_data_ssl=NO\\
             nforce_local_logins_ssl=NO\nrsa_cert_file=/ephemeral/cert.pem\nrsa_private_key_file=/\
             ephemeral/key.pem\n"
        );
        std::fs::write(dir.path().join("vsftpd.conf"), config).unwrap();
        let script = format!(
            r#"set -e
apk add --no-cache vsftpd >/dev/null
adduser -D -h /srv/data -s /bin/sh {USER}
echo "{USER}:{password}" | chpasswd >/dev/null 2>&1
chown {USER} /srv/data
cp /ephemeral/vsftpd.conf /etc/vsftpd.conf
chown root /etc/vsftpd.conf
# Not `exec`: vsftpd's parent process does not survive running as PID 1.
/usr/sbin/vsftpd /etc/vsftpd.conf 0>&2
echo vsftpd exited with $?
sleep 100000
"#
        );
        let container = Container::start("rmx-transfer-vsftpd", dir.path(), &script);
        wait_for_port(port, Duration::from_secs(120));
        Self { _dir: dir, port, password, certificate, container }
    }

    fn tls(&self) -> FtpTls {
        let mut roots = RootCertStore::empty();
        roots.add(self.certificate.clone()).unwrap();
        let config =
            ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        FtpTls { config: Arc::new(config), server_name: "localhost".to_owned() }
    }

    fn config(&self, tls: bool) -> FtpConfig {
        let mut config = FtpConfig::new("127.0.0.1", self.port, USER, self.password.clone());
        if tls {
            config.tls = Some(self.tls());
        }
        config
    }
}

async fn file_operations(fs: &FtpFs) {
    let dir = "/srv/data/ops";
    fs.mkdir(dir).await.unwrap();
    assert_eq!(fs.stat(dir).await.unwrap().kind, EntryKind::Dir);

    let file = format!("{dir}/a.bin");
    let data = pattern(600 * 1024);
    let mut writer = fs.write_range(&file, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::copy_from_slice(&data[..300 * 1024])).await.unwrap();
    writer.write(Bytes::copy_from_slice(&data[300 * 1024..])).await.unwrap();
    writer.finish().await.unwrap();
    let entry = fs.stat(&file).await.unwrap();
    assert_eq!((entry.kind, entry.size), (EntryKind::File, Some(data.len() as u64)));
    assert!(entry.modified.is_some());

    assert_eq!(read_all(fs, &file, 0, None).await.unwrap(), data);
    assert_eq!(read_all(fs, &file, 100, Some(70_000)).await.unwrap(), data[100..70_100]);

    // Continuing at the end of the file works; anywhere else is refused.
    let mut writer = fs.write_range(&file, WriteStart::At(data.len() as u64)).await.unwrap();
    writer.write(Bytes::from_static(b"TAIL")).await.unwrap();
    writer.finish().await.unwrap();
    assert_eq!(fs.stat(&file).await.unwrap().size, Some(data.len() as u64 + 4));
    assert!(matches!(
        fs.write_range(&file, WriteStart::At(5)).await.map(drop),
        Err(Error::Unsupported(_))
    ));

    let replaced = format!("{dir}/b.bin");
    let mut writer = fs.write_range(&replaced, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::from_static(b"old")).await.unwrap();
    writer.finish().await.unwrap();
    fs.rename(&file, &replaced).await.unwrap();
    assert_eq!(fs.stat(&replaced).await.unwrap().size, Some(data.len() as u64 + 4));
    assert!(matches!(fs.stat(&file).await, Err(Error::NotFound(_))));

    // Names with spaces and dot-files survive a listing.
    let spaced = format!("{dir}/a name with spaces.txt");
    let mut writer = fs.write_range(&spaced, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::from_static(b"x")).await.unwrap();
    writer.finish().await.unwrap();
    let hidden = format!("{dir}/.hidden");
    let mut writer = fs.write_range(&hidden, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::from_static(b"y")).await.unwrap();
    writer.finish().await.unwrap();
    let mut names: Vec<String> = fs
        .list(dir, ListOptions::default())
        .await
        .unwrap()
        .collect_all()
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    assert_eq!(names, [".hidden", "a name with spaces.txt", "b.bin"]);

    assert!(matches!(
        fs.list("/srv/data/nope", ListOptions::default()).await.map(drop),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(fs.stat("/srv/data/nope").await, Err(Error::NotFound(_))));
    for name in [&replaced, &spaced, &hidden] {
        fs.remove_file(name).await.unwrap();
    }
    fs.remove_dir(dir).await.unwrap();
}

async fn queue_round_trip(fs: Arc<FtpFs>, label: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalFs::new());
    let (queue, _rx) = TransferQueue::new(QueueConfig::default());

    // A directory tree up and back, several files at once.
    let source = tmp.path().join("tree");
    std::fs::create_dir_all(source.join("sub")).unwrap();
    for i in 0..8 {
        std::fs::write(source.join(format!("f{i}.bin")), pattern(300_000 + i)).unwrap();
    }
    std::fs::write(source.join("sub/inner.bin"), pattern(4_000_000)).unwrap();
    let id = queue.submit(TransferRequest {
        source: local.clone(),
        source_path: source.to_str().unwrap().into(),
        dest: fs.clone(),
        dest_path: format!("/srv/data/tree-{label}"),
        overwrite: OverwritePolicy::Fail,
    });
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done), "{:?}", queue.snapshot());
    let back = tmp.path().join("back");
    let id = queue.submit(TransferRequest {
        source: fs.clone(),
        source_path: format!("/srv/data/tree-{label}"),
        dest: local.clone(),
        dest_path: back.to_str().unwrap().into(),
        overwrite: OverwritePolicy::Fail,
    });
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done), "{:?}", queue.snapshot());
    assert_eq!(std::fs::read(back.join("f3.bin")).unwrap(), pattern(300_003));
    assert_eq!(std::fs::read(back.join("sub/inner.bin")).unwrap(), pattern(4_000_000));

    // Resume of a partial download (`REST`, then `RETR`).
    let data = pattern(3_000_000);
    let remote = format!("/srv/data/resume-{label}.bin");
    let mut writer = fs.write_range(&remote, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::from(data.clone())).await.unwrap();
    writer.finish().await.unwrap();
    let down = tmp.path().join("down.bin");
    std::fs::write(format!("{}.rmxpart", down.display()), &data[..1_000_000]).unwrap();
    let id = queue.submit(TransferRequest {
        source: fs.clone(),
        source_path: remote.clone(),
        dest: local.clone(),
        dest_path: down.to_str().unwrap().into(),
        overwrite: OverwritePolicy::Resume,
    });
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done), "{:?}", queue.snapshot());
    assert_eq!(std::fs::read(&down).unwrap(), data);

    // Resume of a partial upload (`APPE`).
    let source_file = tmp.path().join("up.bin");
    std::fs::write(&source_file, &data).unwrap();
    let target = format!("/srv/data/upload-{label}.bin");
    let partial = format!("{target}.rmxpart");
    let mut writer = fs.write_range(&partial, WriteStart::Truncate).await.unwrap();
    writer.write(Bytes::copy_from_slice(&data[..1_234_567])).await.unwrap();
    writer.finish().await.unwrap();
    let id = queue.submit(TransferRequest {
        source: local.clone(),
        source_path: source_file.to_str().unwrap().into(),
        dest: fs.clone(),
        dest_path: target.clone(),
        overwrite: OverwritePolicy::Resume,
    });
    queue.wait_idle().await;
    assert_eq!(queue.state(id), Some(TransferState::Done), "{:?}", queue.snapshot());
    assert_eq!(read_all(fs.as_ref(), &target, 0, None).await.unwrap(), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs podman or docker and pulls an Alpine image"]
async fn ftp_and_ftps_work_against_vsftpd() {
    let server = Server::start();
    for (tls, label) in [(false, "plain"), (true, "tls")] {
        let fs = Arc::new(FtpFs::connect(server.config(tls)).await.unwrap_or_else(|e| {
            panic!("{label} connect: {e}\n{}", server.container.logs());
        }));
        file_operations(&fs).await;
        queue_round_trip(fs, label).await;
    }
}

/// Carries an FTP connection over a stream that is not a `TcpStream`, as an SSH tunnel would.
struct WrappingDialer {
    dials: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl FtpDialer for WrappingDialer {
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<DialedStream> {
        self.dials.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tcp = TcpStream::connect((host, port)).await?;
        let (read, write) = tcp.into_split();
        Ok(DialedStream::Tunnel(Box::new(tokio::io::join(read, write))))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs podman or docker and pulls an Alpine image"]
async fn ftps_runs_through_a_tunnel_stream() {
    let server = Server::start();
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fs = Arc::new(
        FtpFs::connect_with(server.config(true), Arc::new(WrappingDialer { dials: dials.clone() }))
            .await
            .unwrap(),
    );
    file_operations(&fs).await;
    queue_round_trip(fs, "tunnel").await;
    // One control connection is not enough to get here; data connections must have been dialed.
    assert!(dials.load(std::sync::atomic::Ordering::Relaxed) > 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs podman or docker and pulls an Alpine image"]
async fn ftp_lists_100k_entries_in_batches() {
    let server = Server::start();
    server.container.exec(
        "mkdir /srv/data/big && cd /srv/data/big && seq -w 1 100000 | xargs touch && chown -R \
         rmxftp /srv/data/big",
    );
    let fs = FtpFs::connect(server.config(false)).await.unwrap();
    let started = Instant::now();
    let mut stream = fs
        .list("/srv/data/big", ListOptions { batch_size: 1000, ..ListOptions::default() })
        .await
        .unwrap();
    let (mut total, mut first) = (0, None);
    while let Some(batch) = stream.next_batch().await.unwrap() {
        first.get_or_insert_with(|| started.elapsed());
        assert!(batch.len() <= 1000);
        total += batch.len();
    }
    assert_eq!(total, 100_000);
    println!(
        "ftp list: 100000 entries in {:?}, first batch after {:?}",
        started.elapsed(),
        first.unwrap()
    );
}
