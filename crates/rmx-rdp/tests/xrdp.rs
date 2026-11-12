//! A real session against xrdp in a disposable container.
//!
//! Requires podman or docker and network access to pull `debian:trixie-slim`. The server is
//! published on 127.0.0.1 only, under a unique name, with credentials generated for this run,
//! and is removed afterwards. Run with:
//!
//! ```sh
//! CARGO_TARGET_DIR=target/agents/rdp cargo test -p rmx-rdp --test xrdp -- --ignored --nocapture
//! ```
//!
//! xrdp speaks TLS only, so NLA is exercised against FreeRDP's shadow server instead, an
//! independent CredSSP and NTLM implementation. A real Windows host has not been tested.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rmx_display::{DisplayEvent, DisplayHandle, InputEvent, Key, buttons};
use rmx_rdp::{CertificatePolicy, RdpConfig, RdpError, Security};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::sleep;

const IMAGE: &str = "rmx-rdp-xrdp-test:latest";

/// A running test container, removed on drop.
struct Server {
    runtime: &'static str,
    name: String,
    port: u16,
    user: String,
    password: String,
}

impl Server {
    fn start() -> Self {
        Self::start_as("xrdp")
    }

    /// Starts the server `kind`: `xrdp`, or `freerdp` for the shadow server with NLA.
    fn start_as(kind: &str) -> Self {
        let runtime = detect_runtime();
        let context = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/xrdp");
        let status = Command::new(runtime)
            .args(["build", "-q", "-t", IMAGE, context])
            .stdout(Stdio::null())
            .status()
            .expect("run the container build");
        assert!(status.success(), "building the xrdp image failed");

        let suffix = unique_suffix();
        let name = format!("rmx-rdp-xrdp-{suffix}");
        let user = format!("u{}", &suffix[..8]);
        let password = format!("p{suffix}");
        // The password goes through the child's environment, so it never appears in an argv.
        let status = Command::new(runtime)
            .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::3389"])
            .args(["--env", "RDP_USER", "--env", "RDP_PASS", "--env", "RDP_SERVER", IMAGE])
            .env("RDP_SERVER", kind)
            .env("RDP_USER", &user)
            .env("RDP_PASS", &password)
            .stdout(Stdio::null())
            .status()
            .expect("run the container");
        assert!(status.success(), "starting the xrdp container failed");

        let mut server = Self { runtime, name, port: 0, user, password };
        server.port = server.published_port();
        server
    }

    /// Waits until the server inside the container is listening, which the published port,
    /// being a forwarder, cannot tell us.
    async fn wait_listening(&self) {
        wait_until("the server to listen", || {
            self.exec("grep -q ':0D3D ' /proc/net/tcp /proc/net/tcp6").0
        })
        .await;
    }

    fn published_port(&self) -> u16 {
        let output = Command::new(self.runtime)
            .args(["port", &self.name, "3389/tcp"])
            .output()
            .expect("query the published port");
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().next().unwrap_or_default();
        assert!(line.starts_with("127.0.0.1:"), "published on loopback only: {line:?}");
        line.rsplit(':').next().and_then(|port| port.parse().ok()).expect("a port number")
    }

    /// Runs a command in the container as the session user.
    fn exec(&self, script: &str) -> (bool, String) {
        let output = Command::new(self.runtime)
            .args(["exec", "-u", &self.user, "-e", "DISPLAY=:10"])
            .args(["-e", &format!("XAUTHORITY=/home/{}/.Xauthority", self.user)])
            .args([&self.name, "sh", "-c", script])
            .output()
            .expect("exec in the container");
        (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The SHA-256 fingerprint of the certificate xrdp presents.
    fn certificate_fingerprint(&self) -> [u8; 32] {
        let output = Command::new(self.runtime)
            .args(["exec", &self.name, "openssl", "x509", "-in", "/etc/xrdp/cert.pem"])
            .args(["-noout", "-fingerprint", "-sha256"])
            .output()
            .expect("read the server certificate");
        let text = String::from_utf8_lossy(&output.stdout);
        let hex = text.rsplit('=').next().unwrap_or_default().trim().replace(':', "");
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .expect("a hex fingerprint");
        bytes.try_into().expect("32 bytes")
    }

    fn config(&self, fingerprint: [u8; 32]) -> RdpConfig {
        let mut config =
            RdpConfig::new("localhost", &self.user, &self.password, (1024, 768), fingerprint);
        config.security = Security::Tls;
        config.connect_timeout = Duration::from_secs(20);
        config
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let output = Command::new(self.runtime)
                .args(["exec", &self.name, "sh", "-c"])
                .arg("tail -n 40 /var/log/xrdp.log /var/log/xrdp-sesman.log; ps aux | head -30")
                .output();
            if let Ok(output) = output {
                eprintln!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        let _ = Command::new(self.runtime)
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn detect_runtime() -> &'static str {
    ["podman", "docker"]
        .into_iter()
        .find(|runtime| {
            Command::new(runtime)
                .arg("--version")
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("podman or docker is required")
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
    format!("{:08x}{:08x}", std::process::id(), nanos as u32)
}

#[derive(Debug, Default)]
struct Seen {
    resized: Vec<(u32, u32)>,
    updates: usize,
    /// Whether any update carried a pixel that was not black.
    lit: bool,
    clipboard: Vec<String>,
    closed: Option<Option<String>>,
}

/// Drains the events on a task so the channel keeps moving while the test waits on the server.
fn watch(mut events: mpsc::Receiver<DisplayEvent>) -> Arc<Mutex<Seen>> {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let mut seen = shared.lock().unwrap();
            match event {
                DisplayEvent::Resized { width, height } => seen.resized.push((width, height)),
                DisplayEvent::Update(update) => {
                    assert_eq!(
                        update.pixels.len(),
                        (update.width * update.height) as usize * 4,
                        "tightly packed BGRA8"
                    );
                    assert!(update.width > 0 && update.height > 0);
                    seen.lit |= update.pixels.as_chunks::<4>().0.iter().any(|px| px[..3] != [0, 0, 0]);
                    seen.updates += 1;
                },
                DisplayEvent::Clipboard(text) => seen.clipboard.push(text),
                DisplayEvent::Bell | DisplayEvent::Cursor(_) => {},
                DisplayEvent::Closed(reason) => {
                    seen.closed = Some(reason);
                    break;
                },
            }
        }
    });
    seen
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(100)).await;
    }
}

async fn tap(input: &mpsc::Sender<InputEvent>, evdev: u32) {
    let key = Key { evdev, keysym: None };
    input.send(InputEvent::Key { key, pressed: true }).await.unwrap();
    input.send(InputEvent::Key { key, pressed: false }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker and pulls an image"]
async fn session_against_xrdp() {
    let server = Server::start();
    server.wait_listening().await;
    let stream = TcpStream::connect(("127.0.0.1", server.port)).await.expect("connect");
    let handle = rmx_rdp::connect(stream, server.config(server.certificate_fingerprint()))
        .await
        .expect("connecting to xrdp");
    let DisplayHandle { events, input } = handle;
    let seen = watch(events);

    wait_until("the first Resized", || !seen.lock().unwrap().resized.is_empty()).await;
    assert_eq!(seen.lock().unwrap().resized[0], (1024, 768));
    wait_until("a first frame", || seen.lock().unwrap().updates > 0).await;
    wait_until("visible pixels", || seen.lock().unwrap().lit).await;
    println!("first frame received; {:?}", seen.lock().unwrap().updates);

    // Put the pointer over the terminal (no window manager, so it decides focus), then type.
    wait_until("the session", || server.exec("pgrep -x xterm").0).await;
    sleep(Duration::from_millis(500)).await;
    input.send(InputEvent::Pointer { x: 300, y: 300, buttons: 0 }).await.unwrap();
    sleep(Duration::from_millis(200)).await;
    for evdev in [35, 18, 38, 38, 24, 28] {
        tap(&input, evdev).await;
    }
    wait_until("the typed line to arrive on the server", || {
        let (ok, out) = server.exec("cat \"$HOME/typed.txt\"");
        ok && out == "hello\n"
    })
    .await;
    println!("keyboard input reached the server");

    // Clicking and scrolling use the same path. They must at least not disturb the session.
    input.send(InputEvent::Pointer { x: 310, y: 310, buttons: buttons::LEFT }).await.unwrap();
    input.send(InputEvent::Pointer { x: 310, y: 310, buttons: 0 }).await.unwrap();
    input.send(InputEvent::Wheel { x: 310, y: 310, vertical: 1, horizontal: 0 }).await.unwrap();

    // Local clipboard to remote, then remote to local.
    input.send(InputEvent::Clipboard("rmx-from-client".to_owned())).await.unwrap();
    wait_until("the local clipboard on the server", || {
        let (ok, out) = server.exec("xclip -selection clipboard -o 2>/dev/null");
        ok && out == "rmx-from-client"
    })
    .await;
    println!("clipboard: client to server ok");

    let (ok, _) =
        server.exec("printf rmx-from-server | xclip -selection clipboard -i >/dev/null 2>&1");
    assert!(ok, "setting the server clipboard");
    wait_until("the server clipboard on the client", || {
        seen.lock().unwrap().clipboard.iter().any(|text| text == "rmx-from-server")
    })
    .await;
    println!("clipboard: server to client ok");

    // Resize through display control, where the server has it.
    let resized_before = seen.lock().unwrap().resized.len();
    input.send(InputEvent::Resize { width: 800, height: 600 }).await.unwrap();
    let resized = async {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if seen.lock().unwrap().resized.len() > resized_before {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
    .await;
    if resized {
        let last = *seen.lock().unwrap().resized.last().unwrap();
        println!("display control resized the session to {last:?}");
        assert_eq!(last, (800, 600));
    } else {
        println!("display control did not resize the session");
    }

    input.send(InputEvent::Disconnect).await.unwrap();
    wait_until("the session to close", || seen.lock().unwrap().closed.is_some()).await;
    let seen = seen.lock().unwrap();
    println!("session closed: {:?}, {} updates", seen.closed, seen.updates);
    assert_eq!(seen.closed, Some(None), "a clean close");
    assert!(resized, "xrdp 0.10 with Xorg supports display control");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker and pulls an image"]
async fn a_wrong_certificate_pin_is_rejected_before_credentials() {
    let server = Server::start();
    server.wait_listening().await;
    let stream = TcpStream::connect(("127.0.0.1", server.port)).await.expect("connect");
    let mut config = server.config([0; 32]);
    config.certificate = CertificatePolicy::Pinned([0; 32]);
    match rmx_rdp::connect(stream, config).await {
        Err(RdpError::CertificateRejected) => {},
        Ok(_) => panic!("a wrong pin must not connect"),
        Err(other) => panic!("expected a certificate rejection, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker and pulls an image"]
async fn bitmap_updates_without_remotefx() {
    let server = Server::start();
    server.wait_listening().await;
    let stream = TcpStream::connect(("127.0.0.1", server.port)).await.expect("connect");
    let mut config = server.config(server.certificate_fingerprint());
    config.remotefx = false;
    let handle = rmx_rdp::connect(stream, config).await.expect("connecting to xrdp");
    let DisplayHandle { events, input } = handle;
    let seen = watch(events);
    wait_until("visible pixels", || seen.lock().unwrap().lit).await;
    println!("bitmap updates decoded; {} updates", seen.lock().unwrap().updates);
    input.send(InputEvent::Disconnect).await.unwrap();
    wait_until("the session to close", || seen.lock().unwrap().closed.is_some()).await;
}

async fn nla_connect(server: &Server, password: &str) -> Result<DisplayHandle, RdpError> {
    let stream = TcpStream::connect(("127.0.0.1", server.port)).await.expect("connect");
    let mut config = RdpConfig::new("localhost", &server.user, password, (1024, 768), [0; 32]);
    config.security = Security::Nla;
    config.certificate = CertificatePolicy::AcceptAny;
    config.connect_timeout = Duration::from_secs(20);
    rmx_rdp::connect(stream, config).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker and pulls an image"]
async fn nla_against_freerdp() {
    let server = Server::start_as("freerdp");
    server.wait_listening().await;

    let handle = nla_connect(&server, &server.password).await.expect("NLA connection");
    let DisplayHandle { events, input } = handle;
    let seen = watch(events);
    wait_until("the first Resized", || !seen.lock().unwrap().resized.is_empty()).await;
    wait_until("a first frame", || seen.lock().unwrap().updates > 0).await;
    println!("NLA session established; {} updates", seen.lock().unwrap().updates);
    input.send(InputEvent::Disconnect).await.unwrap();
    wait_until("the session to close", || seen.lock().unwrap().closed.is_some()).await;

    // The wrong password must not get a session.
    match nla_connect(&server, "not-the-password").await {
        Err(RdpError::Handshake(message)) => println!("wrong password refused: {message}"),
        Err(other) => panic!("expected an authentication failure, got {other:?}"),
        Ok(_) => panic!("a wrong password must not connect"),
    }
}
