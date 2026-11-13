//! Drives a real TigerVNC server in a throwaway container, plain and over VeNCrypt.
//!
//! Needs podman or docker and network access for `apk`. Run with:
//!
//! ```sh
//! CARGO_TARGET_DIR=target/agents/vnc cargo test -p rmx-vnc --test container -- --ignored --nocapture
//! ```
//!
//! The server publishes on 127.0.0.1 only, under a unique container name, with a password made
//! up for the run, and the container is removed when the test ends, even if it fails.

use std::fs::File;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use rmx_display::{DisplayEvent, DisplayHandle, FrameUpdate, InputEvent, Key, buttons};
use rmx_vnc::{CertificatePolicy, Config, ConnectError, Security, Session, connect};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const IMAGE: &str = "docker.io/library/alpine:3.22";
const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
const WAIT: Duration = Duration::from_secs(20);

const WIDTH: u32 = 800;
const HEIGHT: u32 = 600;

/// The X root window colours, in the BGRA8 byte order updates must use.
const BLUE_ROOT: [u8; 4] = [0x99, 0x66, 0x33, 0xff];
const RED_ROOT: [u8; 4] = [0x11, 0x22, 0xaa, 0xff];

/// A point on the root window, outside the `xev` window in the top left corner.
const ROOT_POINT: (u32, u32) = (700, 500);

/// The user X509Plain logs in as, with the same password as VNC authentication.
const PLAIN_USER: &str = "rmx";

const SCRIPT: &str = r##"
set -eu
apk add --no-cache --quiet tigervnc xev xsetroot xclip openssl linux-pam shadow
printf '%s' "$VNC_PW" | vncpasswd -f > /tmp/passwd
chmod 600 /tmp/passwd
# A self-signed certificate, made up for the run, and a system user for X509Plain.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 1 \
    -subj /CN=rmx-test -keyout /tmp/key.pem -out /tmp/cert.pem 2>/dev/null
adduser -D rmx
printf 'rmx:%s\n' "$VNC_PW" | chpasswd
printf 'auth required pam_unix.so\naccount required pam_unix.so\n' > /etc/pam.d/vnc
unset VNC_PW
Xvnc :1 -rfbport 5901 -SecurityTypes "$VNC_SECURITY" -PasswordFile /tmp/passwd \
    -X509Cert /tmp/cert.pem -X509Key /tmp/key.pem -PlainUsers rmx -PAMService vnc \
    -geometry 800x600 -depth 24 -AlwaysShared >/tmp/xvnc.log 2>&1 &
export DISPLAY=:1
until xsetroot -solid '#336699' 2>/dev/null; do sleep 0.1; done
xev -geometry 400x300+0+0 >/tmp/xev.log 2>&1 &
# Pressing "r" on the remote desktop turns the root window red.
(until grep -q 'keysym 0x72' /tmp/xev.log; do sleep 0.1; done; xsetroot -solid '#aa2211') &
touch /tmp/ready
wait
"##;

/// A running container, removed on drop.
struct Container {
    engine: &'static str,
    name: String,
}

impl Container {
    /// A server offering the TigerVNC `-SecurityTypes` in `security`, in that order.
    fn start(security: &[&str], password: &str) -> Self {
        let engine = container_engine();
        let name = format!("rmx-vnc-test-{}-{}", std::process::id(), random_hex());
        let status = Command::new(engine)
            // The password travels in the environment, so it stays out of process listings.
            .env("VNC_PW", password)
            .env("VNC_SECURITY", security.join(","))
            .args(["run", "--detach", "--rm", "--name", &name, "--env", "VNC_PW"])
            .args(["--env", "VNC_SECURITY"])
            .args(["--publish", "127.0.0.1::5901"])
            .args([IMAGE, "sh", "-c", SCRIPT])
            .stdout(Stdio::null())
            .status()
            .expect("the container engine could not be run");
        assert!(status.success(), "{engine} run failed");
        Self { engine, name }
    }

    fn exec(&self, command: &str) -> Output {
        Command::new(self.engine)
            .args(["exec", "--env", "DISPLAY=:1", &self.name, "sh", "-c", command])
            .output()
            .expect("the container engine could not be run")
    }

    fn succeeds(&self, command: &str) -> bool {
        self.exec(command).status.success()
    }

    /// Waits for the desktop to be up and returns its port on 127.0.0.1.
    fn wait_until_ready(&self) -> u16 {
        let start = Instant::now();
        while !self.succeeds("test -f /tmp/ready") {
            assert!(start.elapsed() < BOOT_TIMEOUT, "the desktop did not start in time");
            std::thread::sleep(Duration::from_millis(200));
        }
        let output = Command::new(self.engine)
            .args(["port", &self.name, "5901/tcp"])
            .output()
            .expect("the container engine could not be run");
        let mapping = String::from_utf8_lossy(&output.stdout);
        let line = mapping.lines().next().unwrap_or_default();
        assert!(line.starts_with("127.0.0.1:"), "published beyond loopback: {mapping}");
        line.rsplit(':').next().unwrap().trim().parse().expect("bad port mapping")
    }

    /// The SHA-256 of the server's certificate, as a pin.
    fn certificate_pin(&self) -> [u8; 32] {
        let output = self.exec("openssl x509 -in /tmp/cert.pem -outform DER | sha256sum");
        let text = String::from_utf8_lossy(&output.stdout);
        let hex = text.split_whitespace().next().expect("no digest");
        let bytes: Vec<u8> = (0..64)
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("not hex"))
            .collect();
        bytes.try_into().expect("not a SHA-256")
    }

    fn xev_log(&self) -> String {
        let output = self.exec("cat /tmp/xev.log");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn clipboard(&self) -> String {
        let output = self.exec("xclip -selection clipboard -o 2>/dev/null");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.engine)
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn container_engine() -> &'static str {
    let works = |engine: &str| {
        Command::new(engine)
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    ["podman", "docker"].into_iter().find(|engine| works(engine)).expect("no podman or docker")
}

/// Eight hex digits, which is also all of a VNC password that counts.
fn random_hex() -> String {
    let mut bytes = [0; 4];
    File::open("/dev/urandom").unwrap().read_exact(&mut bytes).unwrap();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The remote desktop as the interface would rebuild it.
struct Screen {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    covered: Vec<bool>,
}

impl Screen {
    fn new(width: u32, height: u32) -> Self {
        let area = (width * height) as usize;
        Self { width, height, pixels: vec![0; area * 4], covered: vec![false; area] }
    }

    fn apply(&mut self, update: &FrameUpdate) {
        assert!(update.x + update.width <= self.width, "update beyond the right edge");
        assert!(update.y + update.height <= self.height, "update beyond the bottom edge");
        assert_eq!(update.pixels.len() as u32, update.width * update.height * 4);
        for row in 0..update.height {
            let start = (row * update.width * 4) as usize;
            let line = &update.pixels[start..start + (update.width * 4) as usize];
            let target = ((update.y + row) * self.width + update.x) as usize;
            self.pixels[target * 4..target * 4 + line.len()].copy_from_slice(line);
            self.covered[target..target + update.width as usize].fill(true);
        }
    }

    fn fully_covered(&self) -> bool {
        self.covered.iter().all(|&covered| covered)
    }

    fn pixel(&self, (x, y): (u32, u32)) -> [u8; 4] {
        let start = ((y * self.width + x) * 4) as usize;
        self.pixels[start..start + 4].try_into().unwrap()
    }
}

/// The interface's side of a session under test.
struct Viewer {
    handle: DisplayHandle,
    screen: Screen,
    clipboard: Vec<String>,
    closed: Option<Option<String>>,
}

impl Viewer {
    fn new(handle: DisplayHandle) -> Self {
        Self { handle, screen: Screen::new(0, 0), clipboard: Vec::new(), closed: None }
    }

    /// Applies events until `done` holds, failing after a while.
    async fn until(&mut self, what: &str, mut done: impl FnMut(&Viewer) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            match timeout(left, self.handle.events.recv()).await {
                Ok(Some(event)) => self.apply(event),
                Ok(None) => panic!("the session ended while waiting for {what}"),
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    fn apply(&mut self, event: DisplayEvent) {
        match event {
            DisplayEvent::Resized { width, height } => self.screen = Screen::new(width, height),
            DisplayEvent::Update(update) => self.screen.apply(&update),
            DisplayEvent::Clipboard(text) => self.clipboard.push(text),
            DisplayEvent::Bell | DisplayEvent::Cursor(_) => {},
            DisplayEvent::Closed(reason) => self.closed = Some(reason),
        }
    }

    async fn send(&self, event: InputEvent) {
        self.handle.input.send(event).await.expect("the session stopped taking input");
    }

    /// Keeps taking events while `xev` logs something matching `check`.
    async fn wait_for_log(
        &mut self,
        container: &Container,
        what: &str,
        check: impl Fn(&str) -> bool,
    ) {
        let deadline = Instant::now() + WAIT;
        loop {
            if check(&container.xev_log()) {
                return;
            }
            assert!(Instant::now() < deadline, "xev never logged {what}:\n{}", container.xev_log());
            self.drain_for(Duration::from_millis(100)).await;
        }
    }

    async fn drain_for(&mut self, period: Duration) {
        let pause = sleep(period);
        tokio::pin!(pause);
        loop {
            tokio::select! {
                () = &mut pause => return,
                event = self.handle.events.recv() => match event {
                    Some(event) => self.apply(event),
                    None => return,
                },
            }
        }
    }
}

async fn open(port: u16, password: &str) -> Result<DisplayHandle, ConnectError> {
    let config = Config { password: Some(password.to_owned()), ..Config::default() };
    open_with(port, config).await.map(|session| session.display)
}

async fn open_with(port: u16, config: Config) -> Result<Session, ConnectError> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.expect("no route to the container");
    timeout(WAIT, connect(stream, config)).await.expect("the handshake hung")
}

/// Whether a `ButtonPress` for `button` appears in an `xev` log.
fn logged_button(log: &str, button: u32) -> bool {
    let mut lines = log.lines().peekable();
    let wanted = format!("button {button},");
    while let Some(line) = lines.next() {
        if line.starts_with("ButtonPress") {
            let details: Vec<_> = lines.by_ref().take(3).collect();
            if details.iter().any(|detail| detail.contains(&wanted)) {
                return true;
            }
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker and network access; see the module documentation"]
async fn a_real_server_shows_pixels_and_reacts_to_input() {
    let password = random_hex();
    let container = Container::start(&["VncAuth"], &password);
    let port = container.wait_until_ready();

    let wrong = open(port, "not-it").await;
    assert!(
        matches!(wrong, Err(ConnectError::WrongPassword | ConnectError::Failed(_))),
        "a wrong password was accepted: {:?}",
        wrong.map(|_| ())
    );

    let mut viewer = Viewer::new(open(port, &password).await.expect("the password was refused"));

    // A full frame with the colours the server was told to draw, in BGRA order.
    viewer
        .until("the first full frame", |v| {
            (v.screen.width, v.screen.height) == (WIDTH, HEIGHT) && v.screen.fully_covered()
        })
        .await;
    assert_eq!(viewer.screen.pixel(ROOT_POINT), BLUE_ROOT);
    assert_eq!(viewer.screen.pixel((0, HEIGHT - 1)), BLUE_ROOT);
    assert_ne!(viewer.screen.pixel((200, 150)), BLUE_ROOT, "the xev window is missing");
    assert!(viewer.screen.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 0xff), "not opaque");

    // Inside the xev window, which also gives it keyboard focus without a window manager.
    viewer.send(InputEvent::Pointer { x: 100, y: 120, buttons: 0 }).await;
    viewer.wait_for_log(&container, "the pointer move", |log| log.contains("root:(100,120)")).await;

    viewer.send(InputEvent::Pointer { x: 100, y: 120, buttons: buttons::LEFT }).await;
    viewer.send(InputEvent::Pointer { x: 100, y: 120, buttons: 0 }).await;
    viewer.wait_for_log(&container, "button 1", |log| logged_button(log, 1)).await;

    for (vertical, horizontal, button) in [(1, 0, 4), (-1, 0, 5), (0, -1, 6), (0, 1, 7)] {
        viewer.send(InputEvent::Wheel { x: 100, y: 120, vertical, horizontal }).await;
        let what = format!("button {button}");
        viewer.wait_for_log(&container, &what, |log| logged_button(log, button)).await;
    }

    // The key is seen by the client application, and its effect comes back as pixels.
    let r = Key { evdev: 19, keysym: Some(0x72) };
    viewer.send(InputEvent::Key { key: r, pressed: true }).await;
    viewer.send(InputEvent::Key { key: r, pressed: false }).await;
    viewer.wait_for_log(&container, "the key press", |log| log.contains("keysym 0x72, r")).await;
    viewer
        .until("the key to repaint the desktop", |v| v.screen.pixel(ROOT_POINT) == RED_ROOT)
        .await;

    // Clipboard, both ways.
    let local = format!("to-server-{}", random_hex());
    viewer.send(InputEvent::Clipboard(local.clone())).await;
    let deadline = Instant::now() + WAIT;
    while container.clipboard() != local {
        assert!(Instant::now() < deadline, "the server clipboard never changed");
        viewer.drain_for(Duration::from_millis(100)).await;
    }

    let remote = format!("from-server-{}", random_hex());
    let copy = format!("printf %s {remote} | xclip -selection clipboard -i >/dev/null 2>&1");
    assert!(container.succeeds(&copy), "xclip failed");
    viewer.until("the remote clipboard", |v| v.clipboard.contains(&remote)).await;

    // Resize, confirmed by a new surface that a full frame covers.
    let resize_started = Instant::now();
    viewer.send(InputEvent::Resize { width: 1024, height: 768 }).await;
    viewer
        .until("the resized desktop", |v| {
            (v.screen.width, v.screen.height) == (1024, 768) && v.screen.fully_covered()
        })
        .await;
    let resize_took = resize_started.elapsed();
    eprintln!("resize to full picture: {resize_took:?}");
    // TigerVNC answers the full request with the layout alone: the pixels need one more request,
    // which used to wait for the idle refresh, a second away.
    assert!(resize_took < Duration::from_millis(500), "the new picture took {resize_took:?}");
    assert_eq!(viewer.screen.pixel((1000, 700)), RED_ROOT);

    viewer.send(InputEvent::Disconnect).await;
    viewer.until("Closed", |v| v.closed.is_some()).await;
    assert_eq!(viewer.closed, Some(None));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs podman or docker and network access; see the module documentation"]
async fn vencrypt_connects_only_to_the_pinned_certificate() {
    let password = random_hex();
    let container = Container::start(&["X509Plain", "X509Vnc", "X509None"], &password);
    let port = container.wait_until_ready();
    let pin = container.certificate_pin();
    let pinned = Config { certificate: CertificatePolicy::Pinned(pin), ..Config::default() };

    // Another certificate is refused before any password is sent.
    let mut wrong_pin = pin;
    wrong_pin[0] ^= 1;
    let config = Config {
        password: Some(password.clone()),
        certificate: CertificatePolicy::Pinned(wrong_pin),
        ..Config::default()
    };
    let refused = open_with(port, config).await;
    assert!(
        matches!(refused, Err(ConnectError::CertificateRejected)),
        "another certificate was accepted: {:?}",
        refused.map(|session| session.security)
    );

    let plain = Config {
        username: Some(PLAIN_USER.to_owned()),
        password: Some(random_hex()),
        ..pinned.clone()
    };
    let rejected = open_with(port, plain).await;
    assert!(
        matches!(rejected, Err(ConnectError::LoginRejected(_))),
        "a wrong login was accepted: {:?}",
        rejected.map(|session| session.security)
    );

    // The server lists X509Plain first: with a user name it is taken, with only a password
    // X509Vnc, and with neither X509None.
    let logins = [
        (Some(PLAIN_USER), Some(password.as_str()), Security::X509Plain),
        (None, Some(password.as_str()), Security::X509Vnc),
        (None, None, Security::X509None),
    ];
    for (username, password, expected) in logins {
        let config = Config {
            username: username.map(str::to_owned),
            password: password.map(str::to_owned),
            ..pinned.clone()
        };
        let session = open_with(port, config).await.expect("the pinned server was refused");
        assert_eq!(session.security, expected);
        assert!(session.security.encrypted());

        let mut viewer = Viewer::new(session.display);
        viewer
            .until("the first full frame", |v| {
                (v.screen.width, v.screen.height) == (WIDTH, HEIGHT) && v.screen.fully_covered()
            })
            .await;
        assert_eq!(viewer.screen.pixel(ROOT_POINT), BLUE_ROOT);

        // Input crosses the TLS session too.
        viewer.send(InputEvent::Pointer { x: 123, y: 45, buttons: 0 }).await;
        viewer
            .wait_for_log(&container, "the pointer move", |log| log.contains("root:(123,45)"))
            .await;
        viewer.send(InputEvent::Pointer { x: 100, y: 120, buttons: 0 }).await;
        viewer
            .wait_for_log(&container, "the pointer back", |log| log.contains("root:(100,120)"))
            .await;

        viewer.send(InputEvent::Disconnect).await;
        viewer.until("Closed", |v| v.closed.is_some()).await;
        container.exec("truncate -s 0 /tmp/xev.log");
    }
}
