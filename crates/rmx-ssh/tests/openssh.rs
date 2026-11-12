//! Integration tests against a real OpenSSH server in a local container.
//!
//! Needs podman or docker, `ssh-keygen`, `ssh-agent` and `ssh-add`. The first
//! run builds a small Alpine image (`localhost/rmx-ssh-sshd-test:1`), which is
//! kept to speed up later runs. Each test starts its own uniquely named
//! container published on 127.0.0.1 only, with keys and a password generated
//! for that run, and removes it afterwards, even on panic.
//!
//! ```sh
//! CARGO_TARGET_DIR=target/agents/ssh cargo test -p rmx-ssh --test openssh -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rmx_ssh::{
    AuthMethod, ConnectConfig, ConnectOptions, DisconnectReason, Error, HostConfig, HostKeyCheck,
    HostKeyDecision, HostKeyRejection, HostKeyVerifier, KeyboardChallenge, KnownHostsVerifier,
    NoPrompter, PromptContext, Prompter, SecretString, ShellExit, ShellOptions, ShellReader,
    ShellWriter, SshClient, UnknownHostPolicy, connect,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const IMAGE: &str = "localhost/rmx-ssh-sshd-test:1";
const USER: &str = "alice";
const PASSPHRASE: &str = "correct horse battery staple";
/// The echo service inside the container.
const ECHO_PORT: u16 = 7777;
/// Reachable only from inside the container, so only through a jump.
const INNER_SSH_PORTS: [u16; 2] = [2222, 2223];
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

const CONTAINERFILE: &str = "\
FROM docker.io/library/alpine:3.22
RUN apk add --no-cache openssh-server-pam linux-pam openssh-keygen socat \
 && ln -sf /usr/sbin/sshd.pam /usr/sbin/sshd
RUN printf 'auth required pam_unix.so\\naccount required pam_unix.so\\nsession required pam_permit.so\\npassword required pam_deny.so\\n' > /etc/pam.d/sshd
";

const ENTRYPOINT: &str = r#"
set -e
adduser -D -s /bin/sh alice
echo "alice:$RMX_PASSWORD" | chpasswd
mkdir -p /home/alice/.ssh
printf '%s\n' "$RMX_AUTHORIZED_KEYS" > /home/alice/.ssh/authorized_keys
chown -R alice:alice /home/alice/.ssh
chmod 700 /home/alice/.ssh
chmod 600 /home/alice/.ssh/authorized_keys
cat > /etc/ssh/test_config <<CONFIG
PermitRootLogin no
MaxAuthTries 20
StrictModes no
UsePAM yes
PasswordAuthentication yes
KbdInteractiveAuthentication yes
PubkeyAuthentication yes
AllowTcpForwarding yes
GatewayPorts no
Subsystem sftp internal-sftp
LogLevel ERROR
CONFIG
for port in 22 2222 2223; do
  ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/host_key_$port
  /usr/sbin/sshd -f /etc/ssh/test_config -p $port -h /etc/ssh/host_key_$port
done
socat TCP-LISTEN:7777,bind=127.0.0.1,fork,reuseaddr EXEC:cat &
exec sleep 3600
"#;

fn runtime() -> &'static str {
    static RUNTIME: OnceLock<&'static str> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let works = |name: &str| {
            Command::new(name)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if works("podman") {
            "podman"
        } else if works("docker") {
            "docker"
        } else {
            panic!("these tests need podman or docker")
        }
    })
}

fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running {program}: {e}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Builds the image once per test process.
fn ensure_image() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let exists = Command::new(runtime())
            .args(["image", "inspect", IMAGE])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !exists {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("Containerfile"), CONTAINERFILE).unwrap();
            let context = dir.path().to_str().unwrap();
            let file = dir.path().join("Containerfile");
            run(runtime(), &["build", "-t", IMAGE, "-f", file.to_str().unwrap(), context]);
        }
    });
}

fn unique() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    format!("{}-{}-{nanos:x}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn keygen(path: &Path, kind: &str, passphrase: &str) -> String {
    run(
        "ssh-keygen",
        &["-q", "-t", kind, "-N", passphrase, "-C", "rmx-test", "-f", path.to_str().unwrap()],
    );
    std::fs::read_to_string(path.with_extension("pub")).unwrap()
}

struct Sshd {
    name: String,
    port: u16,
    password: String,
    keys: tempfile::TempDir,
}

impl Sshd {
    async fn start() -> Self {
        ensure_image();
        let keys = tempfile::tempdir().unwrap();
        let dir = keys.path();
        let mut authorized = String::new();
        authorized += &keygen(&dir.join("ed25519"), "ed25519", "");
        authorized += &keygen(&dir.join("rsa"), "rsa", "");
        authorized += &keygen(&dir.join("encrypted"), "ed25519", PASSPHRASE);
        authorized += &keygen(&dir.join("agent"), "ed25519", "");
        // Generated but not authorized.
        keygen(&dir.join("stranger"), "ed25519", "");

        let name = format!("rmx-ssh-test-{}", unique());
        let password = format!("pw-{}", unique());
        run(
            runtime(),
            &[
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-p",
                "127.0.0.1::22",
                "-e",
                &format!("RMX_PASSWORD={password}"),
                "-e",
                &format!("RMX_AUTHORIZED_KEYS={authorized}"),
                IMAGE,
                "sh",
                "-c",
                ENTRYPOINT,
            ],
        );
        // From here on, `Drop` removes the container.
        let mut server = Sshd { name, port: 0, password, keys };
        let mapping = run(runtime(), &["port", &server.name, "22/tcp"]);
        let port = mapping.trim().rsplit(':').next().and_then(|p| p.parse().ok());
        server.port = port.unwrap_or_else(|| panic!("no port in {mapping:?}"));
        server.wait_until_ready().await;
        server
    }

    async fn wait_until_ready(&self) {
        for _ in 0..100 {
            if self.inner_ports_ready() && banner(self.port).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("sshd did not start: {}", run(runtime(), &["logs", &self.name]));
    }

    /// Whether the unpublished sshds and the echo service are listening.
    fn inner_ports_ready(&self) -> bool {
        let script = "for p in 2222 2223 7777; do nc -z 127.0.0.1 $p || exit 1; done";
        Command::new(runtime())
            .args(["exec", &self.name, "sh", "-c", script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn exec(&self, script: &str) {
        run(runtime(), &["exec", &self.name, "sh", "-c", script]);
    }

    fn key(&self, name: &str) -> PathBuf {
        self.keys.path().join(name)
    }

    fn password_auth(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::Password(Some(self.password.as_str().into()))]
    }

    fn host(&self, auth: Vec<AuthMethod>) -> HostConfig {
        HostConfig::new("127.0.0.1", USER, auth).with_port(self.port)
    }

    fn config(&self, auth: Vec<AuthMethod>) -> ConnectConfig {
        ConnectConfig {
            target: self.host(auth),
            jumps: Vec::new(),
            verifier: Arc::new(RecordingVerifier::default()),
            prompter: Arc::new(NoPrompter),
            options: ConnectOptions::default(),
        }
    }

    async fn login(&self) -> SshClient {
        connect(self.config(self.password_auth())).await.expect("password login")
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = Command::new(runtime())
            .args(["rm", "-f", "-t", "0", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn banner(port: u16) -> bool {
    let Ok(mut tcp) = TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    let mut buf = [0u8; 7];
    matches!(
        tokio::time::timeout(Duration::from_secs(1), tcp.read_exact(&mut buf)).await,
        Ok(Ok(_)) if &buf == b"SSH-2.0"
    )
}

/// Runs a test body under a deadline so a hang fails instead of wedging CI.
async fn deadline<T>(body: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, body).await.expect("test timed out")
}

/// Accepts everything and remembers what it saw.
#[derive(Default)]
struct RecordingVerifier {
    seen: Mutex<Vec<(String, u16, String)>>,
}

#[async_trait]
impl HostKeyVerifier for RecordingVerifier {
    async fn verify(&self, check: &HostKeyCheck<'_>) -> HostKeyDecision {
        self.seen.lock().unwrap().push((check.host.to_owned(), check.port, check.fingerprint()));
        HostKeyDecision::Accept
    }
}

/// Answers from a list of canned passwords, counting how many were used.
#[derive(Default)]
struct ScriptedPrompter {
    answers: Mutex<Vec<String>>,
    asked: AtomicU32,
    challenges: Mutex<Vec<KeyboardChallenge>>,
}

impl ScriptedPrompter {
    fn new(answers: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().rev().map(|s| s.to_string()).collect()),
            ..Default::default()
        })
    }

    fn next(&self) -> Option<SecretString> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        self.answers.lock().unwrap().pop().map(SecretString::from)
    }
}

#[async_trait]
impl Prompter for ScriptedPrompter {
    async fn password(&self, _context: &PromptContext<'_>) -> Option<SecretString> {
        self.next()
    }

    async fn key_passphrase(
        &self,
        _context: &PromptContext<'_>,
        _path: &Path,
    ) -> Option<SecretString> {
        self.next()
    }

    async fn keyboard_interactive(
        &self,
        _context: &PromptContext<'_>,
        challenge: &KeyboardChallenge,
    ) -> Option<Vec<SecretString>> {
        self.challenges.lock().unwrap().push(challenge.clone());
        challenge.prompts.iter().map(|_| self.next()).collect()
    }
}

async fn read_until(reader: &mut ShellReader, needle: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(needle) {
        match tokio::time::timeout(Duration::from_secs(20), reader.recv()).await {
            Ok(Some(data)) => seen.push_str(&String::from_utf8_lossy(&data)),
            Ok(None) => panic!("shell ended before {needle:?}; saw {seen:?}"),
            Err(_) => panic!("timed out waiting for {needle:?}; saw {seen:?}"),
        }
    }
    seen
}

async fn shell_round_trip(client: &SshClient) -> (ShellReader, ShellWriter) {
    let (mut reader, writer) = client.open_shell(&ShellOptions::default()).await.unwrap();
    writer.write("echo READY_$((40+2))\n").await.unwrap();
    read_until(&mut reader, "READY_42").await;
    (reader, writer)
}

/// Writes `payload` and reads back the same bytes while still writing, which
/// deadlocks if flow control is broken.
async fn assert_echoes<S>(stream: &mut S, payload: &[u8])
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut read, mut write) = tokio::io::split(stream);
    let writing = async {
        write.write_all(payload).await.unwrap();
        write.flush().await.unwrap();
    };
    let mut received = vec![0u8; payload.len()];
    let reading = async {
        read.read_exact(&mut received).await.unwrap();
    };
    tokio::join!(writing, reading);
    assert!(received == payload, "echoed bytes differ");
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn spawn_local_echo() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = tcp.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    port
}

fn assert_auth_failed(result: Result<SshClient, Error>) -> rmx_ssh::AuthFailure {
    match result {
        Err(Error::AuthFailed(failure)) => failure,
        other => panic!("expected an auth failure, got {other:?}"),
    }
}

// ----- authentication -----

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn password_from_config() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = connect(sshd.config(sshd.password_auth())).await.unwrap();
        assert_eq!(client.user(), USER);
        assert!(client.disconnect_reason().is_none());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn wrong_password_reports_a_clear_failure() {
    deadline(async {
        let sshd = Sshd::start().await;
        let result = connect(sshd.config(vec![AuthMethod::Password(Some("wrong".into()))])).await;
        let failure = assert_auth_failed(result);
        assert_eq!(failure.user, USER);
        assert_eq!(failure.attempts.len(), 1);
        assert!(failure.server_methods.iter().any(|m| m == "publickey"), "{failure:?}");
        assert!(!failure.to_string().contains("wrong"), "the password leaked into the message");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn prompted_password_retries_after_a_wrong_answer() {
    deadline(async {
        let sshd = Sshd::start().await;
        let prompter = ScriptedPrompter::new(&["wrong", &sshd.password]);
        let mut config = sshd.config(vec![AuthMethod::Password(None)]);
        config.prompter = prompter.clone();
        connect(config).await.unwrap();
        assert_eq!(prompter.asked.load(Ordering::Relaxed), 2);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn prompter_can_cancel() {
    deadline(async {
        let sshd = Sshd::start().await;
        let result = connect(sshd.config(vec![AuthMethod::Password(None)])).await;
        assert!(matches!(result, Err(Error::AuthCancelled { .. })), "{result:?}");
    })
    .await;
}

fn key_auth(path: PathBuf, passphrase: Option<&str>) -> Vec<AuthMethod> {
    vec![AuthMethod::PrivateKeyFile { path, passphrase: passphrase.map(SecretString::from) }]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn private_key_ed25519() {
    deadline(async {
        let sshd = Sshd::start().await;
        connect(sshd.config(key_auth(sshd.key("ed25519"), None))).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn private_key_rsa() {
    deadline(async {
        let sshd = Sshd::start().await;
        connect(sshd.config(key_auth(sshd.key("rsa"), None))).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn encrypted_key_with_passphrase_from_config() {
    deadline(async {
        let sshd = Sshd::start().await;
        connect(sshd.config(key_auth(sshd.key("encrypted"), Some(PASSPHRASE)))).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn encrypted_key_prompts_and_recovers_from_a_wrong_passphrase() {
    deadline(async {
        let sshd = Sshd::start().await;
        let prompter = ScriptedPrompter::new(&[PASSPHRASE]);
        let mut config = sshd.config(key_auth(sshd.key("encrypted"), Some("wrong")));
        config.prompter = prompter.clone();
        connect(config).await.unwrap();
        assert_eq!(prompter.asked.load(Ordering::Relaxed), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn unauthorized_key_fails_then_next_method_succeeds() {
    deadline(async {
        let sshd = Sshd::start().await;
        let auth = vec![
            AuthMethod::PrivateKeyFile { path: sshd.key("stranger"), passphrase: None },
            AuthMethod::PrivateKeyFile { path: sshd.key("does-not-exist"), passphrase: None },
            AuthMethod::Password(Some(sshd.password.as_str().into())),
        ];
        connect(sshd.config(auth)).await.unwrap();
    })
    .await;
}

struct SshAgent {
    child: tokio::process::Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl SshAgent {
    async fn start(key: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent.sock");
        let child = tokio::process::Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let status = Command::new("ssh-add")
            .arg(key)
            .env("SSH_AUTH_SOCK", &socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "ssh-add failed");
        SshAgent { child, socket, _dir: dir }
    }
}

impl Drop for SshAgent {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn ssh_agent() {
    deadline(async {
        let sshd = Sshd::start().await;
        let agent = SshAgent::start(&sshd.key("agent")).await;
        let auth = vec![AuthMethod::Agent { socket: Some(agent.socket.clone()) }];
        connect(sshd.config(auth)).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn unreachable_agent_is_reported_and_skipped() {
    deadline(async {
        let sshd = Sshd::start().await;
        let auth = vec![AuthMethod::Agent { socket: Some(sshd.key("no-such-socket")) }];
        let failure = assert_auth_failed(connect(sshd.config(auth)).await);
        assert_eq!(failure.attempts[0].method, "agent");
        assert!(failure.attempts[0].detail.contains("agent"), "{failure:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn keyboard_interactive_through_pam() {
    deadline(async {
        let sshd = Sshd::start().await;
        let prompter = ScriptedPrompter::new(&[&sshd.password]);
        let mut config = sshd.config(vec![AuthMethod::KeyboardInteractive { submethods: None }]);
        config.prompter = prompter.clone();
        connect(config).await.unwrap();
        let challenges = prompter.challenges.lock().unwrap();
        assert_eq!(challenges.len(), 1);
        assert_eq!(challenges[0].prompts.len(), 1);
        assert!(!challenges[0].prompts[0].echo, "a password prompt must not echo");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn keyboard_interactive_wrong_answer_fails() {
    deadline(async {
        let sshd = Sshd::start().await;
        let mut config = sshd.config(vec![AuthMethod::KeyboardInteractive { submethods: None }]);
        config.prompter = ScriptedPrompter::new(&["wrong"]);
        assert_auth_failed(connect(config).await);
    })
    .await;
}

// ----- host keys -----

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn known_hosts_tofu_then_strict_then_changed() {
    deadline(async {
        let sshd = Sshd::start().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");

        let mut config = sshd.config(sshd.password_auth());
        config.verifier = Arc::new(KnownHostsVerifier::new(&path, UnknownHostPolicy::Reject));
        let refused = connect(config.clone()).await;
        match refused {
            Err(Error::HostKeyRejected { reason: HostKeyRejection::Unknown, port, .. }) => {
                assert_eq!(port, sshd.port);
            },
            other => panic!("expected unknown host, got {other:?}"),
        }
        assert!(!path.exists());

        config.verifier = Arc::new(KnownHostsVerifier::new(&path, UnknownHostPolicy::AcceptNew));
        connect(config.clone()).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&format!("[127.0.0.1]:{} ", sshd.port)), "{text}");

        // The recorded key now satisfies a strict verifier.
        config.verifier = Arc::new(KnownHostsVerifier::new(&path, UnknownHostPolicy::Reject));
        connect(config.clone()).await.unwrap();

        // A different ed25519 key on record means the server's key has changed.
        let other = std::fs::read_to_string(sshd.key("stranger").with_extension("pub")).unwrap();
        std::fs::write(&path, format!("[127.0.0.1]:{} {other}", sshd.port)).unwrap();
        match connect(config).await {
            Err(Error::HostKeyRejected { reason: HostKeyRejection::Changed { .. }, .. }) => {},
            other => panic!("expected a changed key, got {other:?}"),
        }
    })
    .await;
}

struct DecliningVerifier;

#[async_trait]
impl HostKeyVerifier for DecliningVerifier {
    async fn verify(&self, _check: &HostKeyCheck<'_>) -> HostKeyDecision {
        HostKeyDecision::Reject(HostKeyRejection::Declined)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn verifier_decline_is_surfaced() {
    deadline(async {
        let sshd = Sshd::start().await;
        let mut config = sshd.config(sshd.password_auth());
        config.verifier = Arc::new(DecliningVerifier);
        let result = connect(config).await;
        match result {
            Err(Error::HostKeyRejected {
                reason: HostKeyRejection::Declined, fingerprint, ..
            }) => assert!(fingerprint.starts_with("SHA256:")),
            other => panic!("expected a declined key, got {other:?}"),
        }
    })
    .await;
}

/// A verifier that takes longer than the handshake timeout, as a person would.
struct SlowVerifier;

#[async_trait]
impl HostKeyVerifier for SlowVerifier {
    async fn verify(&self, _check: &HostKeyCheck<'_>) -> HostKeyDecision {
        tokio::time::sleep(Duration::from_secs(3)).await;
        HostKeyDecision::Accept
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn handshake_timeout_excludes_time_in_the_verifier() {
    deadline(async {
        let sshd = Sshd::start().await;
        let mut config = sshd.config(sshd.password_auth());
        config.verifier = Arc::new(SlowVerifier);
        config.options.timeout = Duration::from_secs(1);
        connect(config).await.unwrap();
    })
    .await;
}

// ----- shell -----

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn shell_round_trip_with_resize_and_exit_status() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let options = ShellOptions {
            term: "xterm-256color".to_owned(),
            cols: 100,
            rows: 40,
            ..Default::default()
        };
        let (mut reader, writer) = client.open_shell(&options).await.unwrap();

        writer.write("echo TERM_IS_${TERM}_END\n").await.unwrap();
        read_until(&mut reader, "TERM_IS_xterm-256color_END").await;

        writer.write("echo SIZE_$(stty size)_END\n").await.unwrap();
        read_until(&mut reader, "SIZE_40 100_END").await;

        writer.resize(120, 50, 0, 0).await.unwrap();
        writer.write("echo SIZE_$(stty size)_END\n").await.unwrap();
        read_until(&mut reader, "SIZE_50 120_END").await;

        assert_eq!(reader.exit(), None);
        writer.write("exit 7\n").await.unwrap();
        while reader.recv().await.is_some() {}
        assert_eq!(reader.exit(), Some(&ShellExit::Status(7)));
        assert!(reader.recv().await.is_none(), "recv stays finished");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn shell_delivers_a_large_burst_to_a_slow_reader() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let (mut reader, writer) = client.open_shell(&ShellOptions::default()).await.unwrap();
        const SIZE: usize = 4_000_000;
        writer
            .write(format!(
                "stty -echo; echo START_$((1+1)); head -c {SIZE} /dev/zero | tr '\\0' Q; exit 0\n"
            ))
            .await
            .unwrap();
        // Let the server run far ahead of us, past every buffer in the path.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let seen = read_until(&mut reader, "START_2").await;
        let after_marker = seen.split_once("START_2").map_or("", |(_, rest)| rest);
        let mut count = after_marker.matches('Q').count();
        let mut chunks = 0u32;
        while let Some(data) = reader.recv().await {
            count += data.iter().filter(|b| **b == b'Q').count();
            chunks += 1;
            if chunks & 15 == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        assert_eq!(count, SIZE);
        assert_eq!(reader.exit(), Some(&ShellExit::Status(0)));
    })
    .await;
}

// ----- jump hosts -----

fn two_hop_config(sshd: &Sshd, verifier: Arc<RecordingVerifier>) -> ConnectConfig {
    let hop = |port| HostConfig::new("127.0.0.1", USER, sshd.password_auth()).with_port(port);
    ConnectConfig {
        target: hop(INNER_SSH_PORTS[1]),
        jumps: vec![hop(sshd.port), hop(INNER_SSH_PORTS[0])],
        verifier,
        prompter: Arc::new(NoPrompter),
        options: ConnectOptions::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn two_hop_jump() {
    deadline(async {
        let sshd = Sshd::start().await;
        let verifier = Arc::new(RecordingVerifier::default());
        let client = connect(two_hop_config(&sshd, verifier.clone())).await.unwrap();

        // Each hop's own key was checked, under its own port.
        let seen = verifier.seen.lock().unwrap().clone();
        let ports: Vec<u16> = seen.iter().map(|s| s.1).collect();
        assert_eq!(ports, [sshd.port, INNER_SSH_PORTS[0], INNER_SSH_PORTS[1]]);
        let mut fingerprints: Vec<&String> = seen.iter().map(|s| &s.2).collect();
        fingerprints.sort();
        fingerprints.dedup();
        assert_eq!(fingerprints.len(), 3);

        // The shell really is on the last sshd.
        let (mut reader, writer) = shell_round_trip(&client).await;
        writer.write("echo CONN_$(echo $SSH_CONNECTION | cut -d' ' -f4)_END\n").await.unwrap();
        read_until(&mut reader, "CONN_2223_END").await;

        // And it carries further tunnels.
        let mut stream = client.open_direct_tcpip("127.0.0.1", ECHO_PORT).await.unwrap();
        assert_echoes(&mut stream, &pattern(200_000)).await;

        assert_eq!(client.jump_host().unwrap().port(), INNER_SSH_PORTS[0]);
        assert_eq!(client.jump_host().unwrap().jump_host().unwrap().port(), sshd.port);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn failure_at_a_jump_host_names_the_hop() {
    deadline(async {
        let sshd = Sshd::start().await;
        let mut config = two_hop_config(&sshd, Arc::new(RecordingVerifier::default()));
        config.jumps[1].auth = vec![AuthMethod::Password(Some("wrong".into()))];
        match connect(config).await {
            Err(Error::Jump { port, source, .. }) => {
                assert_eq!(port, INNER_SSH_PORTS[0]);
                assert!(matches!(*source, Error::AuthFailed(_)), "{source:?}");
            },
            other => panic!("expected a jump failure, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn losing_a_jump_host_is_reported_on_the_target() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client =
            connect(two_hop_config(&sshd, Arc::new(RecordingVerifier::default()))).await.unwrap();
        let (mut reader, _writer) = shell_round_trip(&client).await;

        client.jump_host().unwrap().disconnect().await;

        let reason = client.closed().await;
        match &reason {
            DisconnectReason::JumpHostLost { port, reason, .. } => {
                assert_eq!(*port, INNER_SSH_PORTS[0]);
                assert_eq!(**reason, DisconnectReason::Requested);
            },
            other => panic!("expected a lost jump host, got {other:?}"),
        }
        while reader.recv().await.is_some() {}
        assert_eq!(reader.exit(), Some(&ShellExit::Disconnected(reason)));
    })
    .await;
}

// ----- tunnels -----

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn direct_tcpip_is_a_duplex_stream() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let mut stream = client.open_direct_tcpip("127.0.0.1", ECHO_PORT).await.unwrap();
        assert_echoes(&mut stream, &pattern(1_000_000)).await;

        // Half-close: EOF reaches the service, which then closes its side.
        stream.write_all(b"last").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut tail = Vec::new();
        stream.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, b"last");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn direct_tcpip_to_a_closed_port_says_why() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        match client.open_direct_tcpip("127.0.0.1", 1).await {
            Err(Error::ChannelOpen { destination, reason }) => {
                assert_eq!(destination, "127.0.0.1:1");
                assert!(matches!(reason, rmx_ssh::ChannelOpenFailure::ConnectFailed), "{reason:?}");
            },
            other => panic!("expected a refused channel, got {:?}", other.map(|_| ())),
        }
        // The connection survives a refused channel.
        let mut stream = client.open_direct_tcpip("127.0.0.1", ECHO_PORT).await.unwrap();
        assert_echoes(&mut stream, b"still here").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn sftp_subsystem_speaks_sftp() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let mut stream = client.open_sftp().await.unwrap();
        // SSH_FXP_INIT, version 3.
        stream.write_all(&[0, 0, 0, 5, 1, 0, 0, 0, 3]).await.unwrap();
        stream.flush().await.unwrap();
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header[4], 2, "expected SSH_FXP_VERSION, got {header:?}");
    })
    .await;
}

// ----- port forwards -----

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn local_forward() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let forward = client.forward_local("127.0.0.1:0", "127.0.0.1", ECHO_PORT).await.unwrap();
        let address = forward.local_addr().unwrap();

        let exchange = |payload: Vec<u8>| async move {
            let mut tcp = TcpStream::connect(address).await.unwrap();
            assert_echoes(&mut tcp, &payload).await;
        };
        tokio::join!(exchange(pattern(300_000)), exchange(pattern(100_000)));

        drop(forward);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(TcpStream::connect(address).await.is_err(), "the listener should be gone");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn local_forward_closes_the_socket_when_the_target_is_down() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let forward = client.forward_local("127.0.0.1:0", "127.0.0.1", 1).await.unwrap();
        let mut tcp = TcpStream::connect(forward.local_addr().unwrap()).await.unwrap();
        let mut buf = Vec::new();
        tcp.read_to_end(&mut buf).await.unwrap();
        assert!(buf.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn remote_forward_with_a_server_chosen_port() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let echo = spawn_local_echo().await;
        let forward = client.forward_remote("127.0.0.1", 0, "127.0.0.1", echo).await.unwrap();
        let port = forward.remote_port().unwrap();
        assert_ne!(port, 0);

        // The server dials its own forwarded port, reaching our local echo.
        let mut stream = client.open_direct_tcpip("127.0.0.1", port).await.unwrap();
        assert_echoes(&mut stream, &pattern(300_000)).await;
        drop(stream);

        forward.close().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            client.open_direct_tcpip("127.0.0.1", port).await.is_err(),
            "the server should have stopped listening"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn remote_forward_with_a_fixed_port_and_a_dead_target() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        // Bound then released, so nothing listens here.
        let dead = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let forward = client.forward_remote("127.0.0.1", 28_080, "127.0.0.1", dead).await.unwrap();
        assert_eq!(forward.remote_port(), Some(28_080));

        let mut stream = client.open_direct_tcpip("127.0.0.1", 28_080).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        assert!(buf.is_empty(), "the server should drop a connection it cannot deliver");

        // A second request for the same port is denied by the server.
        let again = client.forward_remote("127.0.0.1", 28_080, "127.0.0.1", dead).await;
        assert!(matches!(again, Err(Error::RequestDenied(_))), "{again:?}");
    })
    .await;
}

async fn socks_connect(proxy: std::net::SocketAddr, request: &[u8]) -> (TcpStream, u8) {
    let mut tcp = TcpStream::connect(proxy).await.unwrap();
    tcp.write_all(&[5, 1, 0]).await.unwrap();
    let mut method = [0u8; 2];
    tcp.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [5, 0]);
    tcp.write_all(request).await.unwrap();
    let mut reply = [0u8; 10];
    tcp.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0], 5);
    (tcp, reply[1])
}

fn socks_request(command: u8, address: &[u8], port: u16) -> Vec<u8> {
    let mut request = vec![5, command, 0];
    request.extend_from_slice(address);
    request.extend_from_slice(&port.to_be_bytes());
    request
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn dynamic_forward_socks5() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let forward = client.forward_dynamic("127.0.0.1:0").await.unwrap();
        let proxy = forward.local_addr().unwrap();

        let ipv4 = socks_request(1, &[1, 127, 0, 0, 1], ECHO_PORT);
        let (mut tcp, code) = socks_connect(proxy, &ipv4).await;
        assert_eq!(code, 0);
        assert_echoes(&mut tcp, &pattern(200_000)).await;

        let mut domain = vec![3, 9];
        domain.extend_from_slice(b"localhost");
        let (mut tcp, code) = socks_connect(proxy, &socks_request(1, &domain, ECHO_PORT)).await;
        assert_eq!(code, 0);
        assert_echoes(&mut tcp, b"by name").await;

        let (_, code) = socks_connect(proxy, &socks_request(1, &[1, 127, 0, 0, 1], 1)).await;
        assert_eq!(code, 5, "a refused target maps to connection refused");

        let (_, code) = socks_connect(proxy, &socks_request(2, &[1, 127, 0, 0, 1], 80)).await;
        assert_eq!(code, 7, "BIND is not supported");
    })
    .await;
}

// ----- disconnects -----

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn local_disconnect_is_reported_as_requested() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let (mut reader, _writer) = shell_round_trip(&client).await;
        client.disconnect().await;
        assert_eq!(client.closed().await, DisconnectReason::Requested);
        while reader.recv().await.is_some() {}
        assert_eq!(reader.exit(), Some(&ShellExit::Disconnected(DisconnectReason::Requested)));
        let again = client.open_direct_tcpip("127.0.0.1", ECHO_PORT).await;
        assert!(matches!(again, Err(Error::Disconnected(DisconnectReason::Requested))));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn dropping_the_last_handle_ends_the_session() {
    deadline(async {
        let sshd = Sshd::start().await;
        let sessions = || {
            let output = run(
                runtime(),
                &["exec", &sshd.name, "sh", "-c", "pgrep -f '[s]shd-session' | wc -l"],
            );
            output.trim().parse::<u32>().unwrap()
        };
        let client = sshd.login().await;
        let (reader, writer) = client.open_shell(&ShellOptions::default()).await.unwrap();
        assert!(sessions() > 0);

        drop((reader, writer, client));
        for _ in 0..50 {
            if sessions() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the server still has a session after every handle was dropped");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn killed_server_process_is_reported_as_lost() {
    deadline(async {
        let sshd = Sshd::start().await;
        let client = sshd.login().await;
        let (mut reader, _writer) = shell_round_trip(&client).await;
        sshd.exec("pkill -KILL -f '[s]shd-session'");
        let reason = client.closed().await;
        assert_eq!(reason, DisconnectReason::ConnectionClosed);
        while reader.recv().await.is_some() {}
        assert_eq!(reader.exit(), Some(&ShellExit::Disconnected(reason)));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker"]
async fn silent_server_trips_the_keepalive() {
    deadline(async {
        let sshd = Sshd::start().await;
        let mut config = sshd.config(sshd.password_auth());
        config.options.keepalive_interval = Some(Duration::from_secs(1));
        config.options.keepalive_max = 2;
        let client = connect(config).await.unwrap();
        run(runtime(), &["pause", &sshd.name]);
        let reason = client.closed().await;
        let _ = Command::new(runtime())
            .args(["unpause", &sshd.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert_eq!(reason, DisconnectReason::KeepaliveTimeout);
    })
    .await;
}
