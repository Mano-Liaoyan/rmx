//! WebDAV and S3 backends against real servers in throwaway containers.
//!
//! These are `#[ignore]`d because they need podman (or docker) and pull images on first use. Run
//! them with:
//!
//! ```text
//! CARGO_TARGET_DIR=target/agents/vault cargo test -p rmx-vault \
//!     --features vendored-openssl --test containers -- --ignored --test-threads=1
//! ```
//!
//! Each server publishes on an ephemeral port of 127.0.0.1 only, has a unique name and random
//! credentials, and is removed when its guard drops.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{creds, fast_kdf};
use rmx_vault::{
    BackendError, Folder, Id, RemoteVersion, S3Backend, S3Config, Secret, SyncBackend, Vault,
    WebDavBackend, WebDavConfig,
};
use rusty_s3::actions::{CreateBucket, S3Action};

fn runtime() -> &'static str {
    let available = |tool: &str| {
        Command::new(tool)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if available("podman") {
        "podman"
    } else if available("docker") {
        "docker"
    } else {
        panic!("neither podman nor docker is available");
    }
}

struct Container {
    name: String,
    port: u16,
}

impl Container {
    fn start(prefix: &str, container_port: u16, args: &[&str], command: &[&str]) -> Self {
        let name = format!("rmx-vault-test-{prefix}-{}", Id::new());
        let publish = format!("127.0.0.1::{container_port}");
        let output = Command::new(runtime())
            .args(["run", "-d", "--rm", "--name", &name, "-p", &publish])
            .args(args)
            .args(command)
            .output()
            .expect("start container");
        // Built first so that a failure below still removes the container.
        let mut container = Self { name, port: 0 };
        assert!(
            output.status.success(),
            "container failed to start: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let port = Command::new(runtime())
            .args(["port", &container.name, &container_port.to_string()])
            .output()
            .expect("query port");
        let text = String::from_utf8_lossy(&port.stdout);
        let line = text.lines().next().expect("port mapping");
        assert!(line.starts_with("127.0.0.1:"), "must publish on loopback only: {line}");
        container.port = line.rsplit(':').next().unwrap().parse().unwrap();
        container
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(runtime())
            .args(["rm", "-f", "-t", "0", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn ephemeral_secret() -> String {
    Id::new().to_string().replace('-', "")
}

fn wait_until_ready(mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !probe() {
        assert!(Instant::now() < deadline, "server did not become ready");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The compare-and-swap contract, then two working copies converging through `backend`.
fn exercise(backend: &dyn SyncBackend, dir: &std::path::Path) {
    assert!(backend.head().unwrap().is_none());
    assert!(backend.get().unwrap().is_none());
    backend.put_if_match(b"first", None).unwrap();
    assert!(
        matches!(backend.put_if_match(b"nope", None), Err(BackendError::Conflict)),
        "create-only put must fail once the file exists"
    );
    let current = backend.get().unwrap().unwrap();
    assert_eq!(current.data, b"first");
    assert_eq!(backend.head().unwrap().unwrap(), current.version);
    assert!(
        matches!(
            backend.put_if_match(b"nope", Some(&RemoteVersion("\"stale\"".to_owned()))),
            Err(BackendError::Conflict)
        ),
        "a stale version must be refused"
    );
    assert_eq!(backend.get().unwrap().unwrap().data, b"first");
    // The same version is accepted once, and is stale after that.
    backend.put_if_match(b"second", Some(&current.version)).unwrap();
    assert!(matches!(
        backend.put_if_match(b"third", Some(&current.version)),
        Err(BackendError::Conflict)
    ));
    assert_eq!(backend.get().unwrap().unwrap().data, b"second");

    // Replace the test bytes with a real vault.
    let latest = backend.get().unwrap().unwrap().version;
    let path_a = dir.join("a.rmxdb");
    let mut a = Vault::create(&path_a, &creds(), fast_kdf()).unwrap();
    let seed = dir.join("seed");
    a.snapshot(&seed).unwrap();
    backend.put_if_match(&std::fs::read(&seed).unwrap(), Some(&latest)).unwrap();

    let mut b = Vault::join(&dir.join("b.rmxdb"), &creds(), backend).unwrap();
    let from_a = Folder { name: "from a".to_owned(), ..Default::default() };
    let from_b = Folder { name: "from b".to_owned(), ..Default::default() };
    a.put(&from_a).unwrap();
    b.put(&from_b).unwrap();
    a.sync(backend).unwrap();
    b.sync(backend).unwrap();
    a.sync(backend).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert!(a.get::<Folder>(from_b.id).unwrap().is_some());
    assert!(b.get::<Folder>(from_a.id).unwrap().is_some());

    // A real server rejecting a stale If-Match drives the engine's retry.
    let snapshot_before = backend.get().unwrap().unwrap();
    a.put(&Folder { name: "bump".to_owned(), ..Default::default() }).unwrap();
    a.sync(backend).unwrap();
    assert!(matches!(
        backend.put_if_match(&snapshot_before.data, Some(&snapshot_before.version)),
        Err(BackendError::Conflict)
    ));
    b.sync(backend).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
}

const HTTPD_SETUP: &str = r#"
mkdir -p /srv/dav /srv/lock && chown daemon:daemon /srv/dav /srv/lock
htpasswd -bc /srv/users "$DAV_USER" "$DAV_PASS"
cat > /usr/local/apache2/conf/httpd.conf <<'CONF'
ServerRoot /usr/local/apache2
Listen 80
ServerName localhost
LoadModule mpm_event_module modules/mod_mpm_event.so
LoadModule authn_file_module modules/mod_authn_file.so
LoadModule authn_core_module modules/mod_authn_core.so
LoadModule authz_user_module modules/mod_authz_user.so
LoadModule authz_core_module modules/mod_authz_core.so
LoadModule auth_basic_module modules/mod_auth_basic.so
LoadModule unixd_module modules/mod_unixd.so
LoadModule dav_module modules/mod_dav.so
LoadModule dav_fs_module modules/mod_dav_fs.so
User daemon
Group daemon
ErrorLog /proc/self/fd/2
PidFile /tmp/httpd.pid
DavLockDB /srv/lock/DavLock
DocumentRoot /srv/dav
<Directory /srv/dav>
  Dav On
  AuthType Basic
  AuthName dav
  AuthUserFile /srv/users
  Require valid-user
</Directory>
CONF
exec httpd-foreground
"#;

#[test]
#[ignore = "needs podman or docker"]
fn webdav_against_apache() {
    let (user, password) = ("rmxtest".to_owned(), ephemeral_secret());
    let container = Container::start(
        "webdav",
        80,
        &["-e", &format!("DAV_USER={user}"), "-e", &format!("DAV_PASS={password}")],
        &["docker.io/library/httpd:2.4", "sh", "-c", HTTPD_SETUP],
    );
    let url = format!("http://127.0.0.1:{}/vault.rmxdb", container.port);
    let config = WebDavConfig {
        url: url.clone(),
        username: Some(user),
        password: Secret::new(password),
        allow_plain_http: false,
    };
    let backend = WebDavBackend::new(config.clone()).unwrap();
    wait_until_ready(|| backend.head().is_ok());

    let denied =
        WebDavBackend::new(WebDavConfig { password: Secret::new("wrong"), ..config }).unwrap();
    assert!(matches!(denied.get(), Err(BackendError::Http(401))));

    let dir = tempfile::tempdir().unwrap();
    exercise(&backend, dir.path());
}

#[test]
#[ignore = "needs podman or docker"]
fn s3_against_minio() {
    let (access, secret) = (ephemeral_secret(), ephemeral_secret());
    let container = Container::start(
        "minio",
        9000,
        &[
            "-e",
            &format!("MINIO_ROOT_USER={access}"),
            "-e",
            &format!("MINIO_ROOT_PASSWORD={secret}"),
        ],
        &["cgr.dev/chainguard/minio:latest", "server", "/tmp/data"],
    );
    let endpoint = format!("http://127.0.0.1:{}", container.port);

    let bucket = rusty_s3::Bucket::new(
        endpoint.parse().unwrap(),
        rusty_s3::UrlStyle::Path,
        "rmx-test".to_owned(),
        "us-east-1".to_owned(),
    )
    .unwrap();
    let credentials = rusty_s3::Credentials::new(access.clone(), secret.clone());
    let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
    let create = CreateBucket::new(&bucket, &credentials).sign(Duration::from_secs(300));
    wait_until_ready(|| {
        agent.put(create.as_str()).send_empty().is_ok_and(|r| r.status().is_success())
    });

    let config = S3Config {
        endpoint,
        bucket: "rmx-test".to_owned(),
        region: "us-east-1".to_owned(),
        key: "dir/vault.rmxdb".to_owned(),
        access_key: access,
        secret_key: Secret::new(secret),
        path_style: true,
        allow_plain_http: false,
    };
    let backend = S3Backend::new(config.clone()).unwrap();
    let denied =
        S3Backend::new(S3Config { secret_key: Secret::new("wrong-secret"), ..config }).unwrap();
    assert!(matches!(denied.head(), Err(BackendError::Http(403))));

    let dir = tempfile::tempdir().unwrap();
    exercise(&backend, dir.path());
}
