//! End-to-end runs of the real application against a real OpenSSH server, driven by bench scripts.
//!
//! Every server is a throwaway container on 127.0.0.1, with credentials made up for the run, and
//! is removed afterwards, also on failure. Every run has its own `HOME` and data directory, so the
//! user's own `~/.ssh` and configuration are never looked at.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{Report, measure};

const IMAGE: &str = "localhost/rmx-ssh-sshd-test:1";
/// The same image the `rmx-ssh` OpenSSH tests build, so either makes it for the other.
const IMAGE_CONTAINERFILE: &str = "\
FROM docker.io/library/alpine:3.22
RUN apk add --no-cache openssh-server-pam linux-pam openssh-keygen socat \
 && ln -sf /usr/sbin/sshd.pam /usr/sbin/sshd
RUN printf 'auth required pam_unix.so\\naccount required pam_unix.so\\nsession required pam_permit.so\\npassword required pam_deny.so\\n' > /etc/pam.d/sshd
";
/// Disposable servers built from packages, made once and reused between runs.
/// The buttons of a file open in a pane of a 1280 by 800 window with a tab strip and sidebar.
const TEXT_DIFF: &str = "move:1149:90";
const TEXT_SAVE: &str = "move:1201:90";
const VNC_IMAGE: &str = "localhost/rmx-e2e-vnc:1";
const FTP_IMAGE: &str = "localhost/rmx-e2e-ftp:1";
const TELNET_IMAGE: &str = "localhost/rmx-e2e-telnet:1";
const RDP_IMAGE: &str = "localhost/rmx-rdp-xrdp-test:latest";
/// Gaps between script steps that are not held for something, in milliseconds.
const STEP_MS: &str = "120";

/// A shell tab under the sessions, whose content does not matter.
const LOCAL_SHELL: &str = "exec sleep 100000";

const ENTRYPOINT: &str = r#"
set -e
adduser -D -s /bin/sh alice
echo "alice:$RMX_PASSWORD" | chpasswd
mkdir -p /home/alice/.ssh
printf '%s\n' "$RMX_AUTHORIZED_KEYS" > /home/alice/.ssh/authorized_keys
chmod 600 /home/alice/.ssh/authorized_keys
chown -R alice:alice /home/alice/.ssh
chmod 700 /home/alice/.ssh
cat > /etc/ssh/test_config <<CONFIG
PermitRootLogin no
MaxAuthTries 20
StrictModes no
UsePAM yes
PasswordAuthentication yes
KbdInteractiveAuthentication yes
PubkeyAuthentication yes
AllowTcpForwarding yes
Subsystem sftp internal-sftp
LogLevel ERROR
CONFIG
for port in 22 2222; do
  ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/host_key_$port
  /usr/sbin/sshd -f /etc/ssh/test_config -p $port -h /etc/ssh/host_key_$port
done
socat TCP-LISTEN:7777,bind=127.0.0.1,fork,reuseaddr EXEC:cat &
exec sleep 3600
"#;

/// Random hex from the system, for credentials and names that nothing else has seen.
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes)).unwrap();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What the scenario running now left out, and why. Taken by [`take_skipped`] after each one.
static SKIPPED: Mutex<Vec<Skipped>> = Mutex::new(Vec::new());

/// One thing a scenario could not do here, for want of something the machine does not have.
pub struct Skipped {
    /// The part of the scenario left out, or `None` when it is the whole of it.
    pub part: Option<String>,
    pub reason: String,
}

/// Note that `part` of the running scenario is left out, and go on with the rest.
fn skip_part(part: &str, reason: &str) {
    println!("skip {part}: {reason}");
    let skipped = Skipped { part: Some(part.to_owned()), reason: reason.to_owned() };
    SKIPPED.lock().unwrap().push(skipped);
}

/// The error that ends the running scenario as skipped rather than failed.
fn skip(reason: String) -> String {
    let skipped = Skipped { part: None, reason: reason.clone() };
    SKIPPED.lock().unwrap().push(skipped);
    reason
}

/// `result`, or `None` if it was skipped: then only `part` of the scenario is left out.
fn optional<T>(part: &str, result: Result<T, String>) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) => {
            let mut skipped = SKIPPED.lock().unwrap();
            if !skipped.last().is_some_and(|last| last.part.is_none() && last.reason == err) {
                return Err(err);
            }
            skipped.pop();
            drop(skipped);
            skip_part(part, &err);
            Ok(None)
        },
    }
}

/// What the scenario just run left out.
pub fn take_skipped() -> Vec<Skipped> {
    std::mem::take(&mut *SKIPPED.lock().unwrap())
}

/// Whether `program` can be started at all.
fn installed(program: &str) -> bool {
    Command::new(program).arg("-V").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

fn podman(args: &[&str]) -> Result<String, String> {
    let output = Command::new("podman").args(args).output().map_err(podman_missing)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!("podman {}: {}", args[0], String::from_utf8_lossy(&output.stderr)))
    }
}

/// A throwaway OpenSSH server. Dropping it removes the container.
pub struct Sshd {
    name: String,
    pub port: u16,
    pub password: String,
}

impl Sshd {
    pub fn start() -> Result<Self, String> {
        Self::with_keys("")
    }

    /// A server that also takes the public keys in `authorized_keys`.
    pub fn with_keys(authorized_keys: &str) -> Result<Self, String> {
        ensure_image(IMAGE, IMAGE_CONTAINERFILE)?;
        let name = format!("rmx-e2e-{}", random_hex());
        let password = random_hex();
        let env = format!("RMX_PASSWORD={password}");
        let keys = format!("RMX_AUTHORIZED_KEYS={authorized_keys}");
        let args = [
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-p",
            "127.0.0.1::22",
            "-e",
            &env,
            "-e",
            &keys,
            IMAGE,
            "sh",
            "-c",
            ENTRYPOINT,
        ];
        podman(&args)?;
        // From here on, dropping it removes the container.
        let mut server = Self { name, port: 0, password };
        let mapping = podman(&["port", &server.name, "22/tcp"])?;
        let port = mapping.lines().next().unwrap_or("");
        if !port.starts_with("127.0.0.1:") {
            return Err(format!("not published on loopback only: {port}"));
        }
        server.port = port.rsplit(':').next().and_then(|p| p.parse().ok()).ok_or("no port")?;
        server.wait_until_ready()?;
        Ok(server)
    }

    /// A server sharing the network of `owner`, whose loopback it can reach: a way in through a
    /// tunnel to something nothing else reaches. `owner` has to publish port 22.
    fn beside(owner: &Server) -> Result<Self, String> {
        ensure_image(IMAGE, IMAGE_CONTAINERFILE)?;
        let name = format!("rmx-e2e-{}", random_hex());
        let password = random_hex();
        let env = format!("RMX_PASSWORD={password}");
        let network = format!("--network=container:{}", owner.name);
        podman(&[
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            &network,
            "-e",
            &env,
            "-e",
            "RMX_AUTHORIZED_KEYS=",
            IMAGE,
            "sh",
            "-c",
            ENTRYPOINT,
        ])?;
        let server = Self { name, port: owner.port(22)?, password };
        server.wait_until_ready()?;
        Ok(server)
    }

    fn wait_until_ready(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let ready = "for p in 22 2222 7777; do nc -z 127.0.0.1 $p || exit 1; done";
        while Instant::now() < deadline {
            let up = Command::new("podman")
                .args(["exec", &self.name, "sh", "-c", ready])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if up && self.answers() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err("sshd did not start".to_owned())
    }

    fn answers(&self) -> bool {
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) else { return false };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut banner = [0u8; 4];
        stream.read_exact(&mut banner).is_ok() && banner == *b"SSH-"
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", "-t", "0", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A directory of the run's own, removed afterwards.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let base =
            std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_nanos());
        let path = base.join(format!("rmx-e2e-{stamp:x}"));
        fs::create_dir_all(path.join("home/.ssh")).unwrap();
        Self(path)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One run of the application on its own home and data directory.
pub fn app_run(
    bench_target: &Path,
    scratch: &Scratch,
    device: &str,
    script: &str,
) -> Result<Report, String> {
    app_run_with(bench_target, scratch, device, script, &[])
}

fn app_run_with(
    bench_target: &Path,
    scratch: &Scratch,
    device: &str,
    script: &str,
    more: &[(&str, &str)],
) -> Result<Report, String> {
    app_run_shell(bench_target, scratch, device, script, more, LOCAL_SHELL)
}

fn app_run_shell(
    bench_target: &Path,
    scratch: &Scratch,
    device: &str,
    script: &str,
    more: &[(&str, &str)],
    shell: &str,
) -> Result<Report, String> {
    let home = scratch.path("home");
    let data = scratch.path(device);
    let script = format!("{script},quit");
    let mut env = vec![
        ("HOME", home.to_str().unwrap()),
        ("RMX_DATA_DIR", data.to_str().unwrap()),
        ("RMX_BENCH_SCRIPT", script.as_str()),
        ("RMX_BENCH_STEP_MS", STEP_MS),
        ("RMX_BENCH_WARMUP", "0"),
    ];
    env.extend_from_slice(more);
    let report = measure(bench_target, Some((1280, 800)), &env, shell)
        .ok_or_else(|| "the application did not exit cleanly".to_owned())?;
    if report.value("script_ok") != 1 {
        let failed = report.text.lines().find_map(|line| line.strip_prefix("script_failed="));
        return Err(format!("the script gave up waiting for {}", failed.unwrap_or("?")));
    }
    Ok(report)
}

/// A local port nothing listens on just now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Keep trying the port forward at `port`, whose far end echoes. Sets the flag once it has.
fn watch_forward(port: u16) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let worked = flag.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut echoed = [0u8; 4];
                if stream.write_all(b"ping").is_ok()
                    && stream.read_exact(&mut echoed).is_ok()
                    && echoed == *b"ping"
                {
                    worked.store(true, Ordering::SeqCst);
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    flag
}

/// The session logs written under the data directory `data`.
fn session_logs(data: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = data.join("logs");
    let entries = fs::read_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(entries.filter_map(Result::ok).map(|entry| entry.path()).collect())
}

/// Whether `needle` occurs in the file at `path`.
fn file_contains(path: &Path, needle: &str) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

/// Create a vault, add a connection, trust its key, use the shell, resize, disconnect, reconnect,
/// relock and unlock; then a jump host, a forward, the tree, and a second start of the app.
pub fn ssh(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let scratch = Scratch::new();
    let key = scratch.path("home/id_test");
    let passphrase = random_hex();
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", &passphrase, "-f"])
        .arg(&key)
        .status()
        .map_err(|err| err.to_string())?;
    if !status.success() {
        return Err("ssh-keygen failed".to_owned());
    }
    let public = fs::read_to_string(key.with_extension("pub")).map_err(|err| err.to_string())?;
    let server = Sshd::with_keys(&public)?;
    fs::write(
        scratch.path("home/.ssh/config"),
        "Host fixture-host\n  HostName 192.0.2.1\n  User someone\n",
    )
    .map_err(|err| err.to_string())?;
    let vault_password = random_hex();
    let forward_port = free_port();
    let forward = watch_forward(forward_port);
    let (port, password) = (server.port, &server.password);
    let shot = |name: &str| format!("shot:{}", shots.join(name).display());

    let script = [
        "await:setup".to_owned(),
        "wait".to_owned(),
        shot("01-setup.ppm"),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn:box|127.0.0.1|{port}|alice|{password}"),
        "await:host-key".to_owned(),
        // Giving up takes the question away with it.
        "cancel".to_owned(),
        "await:no-question".to_owned(),
        "await:failed".to_owned(),
        "wait,wait".to_owned(),
        shot("02-cancelled.ppm"),
        "reconnect".to_owned(),
        "await:host-key".to_owned(),
        "wait,wait".to_owned(),
        shot("02-host-key.ppm"),
        "hostkey:always".to_owned(),
        "await:live".to_owned(),
        "await:text:$ ".to_owned(),
        "run:echo marker-$((6*7))".to_owned(),
        "await:text:marker-42".to_owned(),
        "wait".to_owned(),
        shot("03-shell.ppm"),
        "resize:1000x640".to_owned(),
        "wait,wait,wait,wait".to_owned(),
        "run:clear; stty size | sed s/^/size-/".to_owned(),
        "await:remote-size".to_owned(),
        "wait".to_owned(),
        shot("04-resized.ppm"),
        "disconnect".to_owned(),
        "await:closed".to_owned(),
        "wait,wait".to_owned(),
        shot("05-disconnected.ppm"),
        // Through the interface: the button on the pane, as the shot shows it.
        "move:932:605,wait,press,release".to_owned(),
        "await:live".to_owned(),
        "await:text:$ ".to_owned(),
        "vault-lock".to_owned(),
        "await:locked".to_owned(),
        "wait,wait".to_owned(),
        shot("06-locked.ppm"),
        "vault-unlock:wrong-password".to_owned(),
        "await:vault-error".to_owned(),
        format!("vault-unlock:{vault_password}"),
        "await:unlocked".to_owned(),
        "await:text:$ ".to_owned(),
        "wait,wait".to_owned(),
        shot("07-unlocked.ppm"),
        "theme:light,wait,wait".to_owned(),
        shot("08-light.ppm"),
        "theme:dark,wait".to_owned(),
        format!("conn:inner|127.0.0.1|2222|alice|{password}|box|L127.0.0.1:{forward_port}:127.0.0.1:7777"),
        "await:host-key".to_owned(),
        "hostkey:always".to_owned(),
        "await:live".to_owned(),
        "await:text:$ ".to_owned(),
        "run:echo inner-${SSH_CONNECTION##* }".to_owned(),
        "await:text:inner-2222".to_owned(),
        "wait,wait,wait,wait,wait,wait".to_owned(),
        shot("09-jump.ppm"),
        "discard,wait,wait".to_owned(),
        shot("09-output-lost.ppm"),
        format!("conn:kbd|127.0.0.1|{port}|alice|"),
        "await:prompt".to_owned(),
        "wait,wait".to_owned(),
        shot("09-keyboard-interactive.ppm"),
        format!("answer:{password}"),
        "await:live".to_owned(),
        format!("conn-key:keyed|127.0.0.1|{port}|alice|{}", key.display()),
        "await:prompt".to_owned(),
        "wait,wait".to_owned(),
        shot("09-passphrase.ppm"),
        format!("answer:{passphrase}"),
        "await:live".to_owned(),
        "await:text:$ ".to_owned(),
        "menu:box,wait,wait".to_owned(),
        shot("09-tree-menu.ppm"),
        "escape,wait".to_owned(),
        "edit:box".to_owned(),
        "await:dialog".to_owned(),
        "wait,wait".to_owned(),
        shot("09-edit.ppm"),
        "rename:box-renamed".to_owned(),
        "await:tree:box-renamed".to_owned(),
        "dup:box-renamed".to_owned(),
        "await:tree:box-renamed (copy)".to_owned(),
        "del:box-renamed (copy)".to_owned(),
        "await:prompt".to_owned(),
        "wait,wait".to_owned(),
        shot("10-confirm.ppm"),
        "confirm".to_owned(),
        "await:no-tree:box-renamed (copy)".to_owned(),
        "import-ssh".to_owned(),
        "await:tree:fixture-host".to_owned(),
        "settings,wait,wait,wait".to_owned(),
        shot("11-settings.ppm"),
        "escape,wait,wait".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("ssh run", &report);
    if !forward.load(Ordering::SeqCst) {
        return Err("the port forward never echoed".to_owned());
    }

    // A later start shows the unlock screen, and needs no second word about the host key.
    let script = [
        "await:locked".to_owned(),
        "wait,wait".to_owned(),
        // Typed into the screen, and a double click on the row, as a person would.
        format!("text:{vault_password}"),
        "wait,enter".to_owned(),
        "await:unlocked".to_owned(),
        "await:tree:box-renamed".to_owned(),
        "wait,wait,wait".to_owned(),
        "move:54:194,wait,press,release,press,release".to_owned(),
        "await:live".to_owned(),
        "await:text:$ ".to_owned(),
        "wait,wait".to_owned(),
        shot("12-second-start.ppm"),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("second start", &report);

    // What was typed for the server and the vault is nowhere in the clear on disk.
    let state = scratch.path("device/app.local");
    let vault = scratch.path("device/vault.rmxdb");
    for file in [&state, &vault] {
        if !file.exists() {
            return Err(format!("{} was not written", file.display()));
        }
        if file_contains(file, password)
            || file_contains(file, &vault_password)
            || file_contains(file, "OPENSSH PRIVATE KEY")
        {
            return Err(format!("a password is readable in {}", file.display()));
        }
    }
    println!("ok   nothing secret in the state file or readable in the vault file");

    // Left alone, the vault locks.
    let scratch = Scratch::new();
    let script =
        format!("await:setup,vault-create:{vault_password},await:unlocked,await:locked,wait");
    let report = app_run_with(
        bench_target,
        &scratch,
        "device",
        &script,
        &[("RMX_BENCH_AUTO_LOCK_MS", "1500")],
    )?;
    report_paths("auto-lock", &report);
    Ok(())
}

fn report_paths(name: &str, report: &Report) {
    println!(
        "ok   {name}: action max {} us ({}), frame max {} us, argon2 {} and connect {} off main, {} on it",
        report.value("action_max_us"),
        report.text.lines().find_map(|l| l.strip_prefix("action_slowest=")).unwrap_or("?"),
        report.value("frame_max_us"),
        report.value("argon2_runs"),
        report.value("connect_runs"),
        report.value("on_main_violations"),
    );
}

/// Two devices, each its own run, converging through `kind`.
fn two_devices(
    bench_target: &Path,
    name: &str,
    connect: &str,
    first_sync_held: bool,
) -> Result<(), String> {
    let scratch = Scratch::new();
    let password = random_hex();
    let hold = if first_sync_held { "await:host-key,hostkey:always," } else { "" };

    let a = format!(
        "await:setup,vault-create:{password},await:unlocked,sync-set:{connect},wait,wait,\
         conn:from-a|127.0.0.1|1|u|p,await:failed,snippet-new:snip-a|echo a,await:snippet:snip-a,\
         sync-now,{hold}await:synced,wait"
    );
    let report = app_run(bench_target, &scratch, "a", &a)?;
    report_paths(&format!("{name}: first device"), &report);

    let b = format!(
        "await:setup,join:{password}|{connect},{hold}await:unlocked,await:tree:from-a,await:snippet:snip-a,\
         conn:from-b|127.0.0.1|1|u|p,await:failed,sync-now,await:synced,wait"
    );
    let report = app_run(bench_target, &scratch, "b", &b)?;
    report_paths(&format!("{name}: second device"), &report);

    let again = format!(
        "await:locked,vault-unlock:{password},await:unlocked,sync-now,await:synced,\
         await:tree:from-b,await:tree:from-a,wait"
    );
    let report = app_run(bench_target, &scratch, "a", &again)?;
    report_paths(&format!("{name}: first device again"), &report);
    println!("ok   {name}: both devices have both connections and the snippet");
    Ok(())
}

/// Sync between two instances through a local path and through SFTP.
pub fn sync(bench_target: &Path) -> Result<(), String> {
    let scratch = Scratch::new();
    let folder = scratch.path("synced");
    fs::create_dir_all(&folder).map_err(|err| err.to_string())?;
    let local = format!("local|||||{}", folder.join("vault.rmxdb").display());
    two_devices(bench_target, "local path", &local, false)?;

    let server = Sshd::start()?;
    let sftp =
        format!("sftp|127.0.0.1|{}|alice|{}|/home/alice/vault.rmxdb", server.port, server.password);
    two_devices(bench_target, "sftp", &sftp, true)
}

/// How long a script gets to say where a let-go tab landed, in milliseconds.
const CLAIM_MS: &str = "1500";

/// Drag the tab at `index` of the window acted on off the top of its window and let go.
fn tear_tab(index: usize) -> String {
    format!(
        "tab-point:{index},wait,press,wait,move:700:20,wait,move:700:-80,wait,move:700:-200,wait,release"
    )
}

/// Drag the only tab of window 1 out, and let it come down on window 0's strip.
fn merge_tab(index: usize) -> String {
    format!("{},wait,win:0,move:700:20", tear_tab(index))
}

/// Working the bar of the pane at index 0 of a tab: a local shell beside it, the buttons, a drag
/// onto its neighbour, and its menu, which `menu_row` must be in. Leaves the tab as it found it.
fn pane_chrome(panes: usize, menu_row: &str, name: &str, shots: &Path) -> String {
    let more = panes + 1;
    [
        format!("split-right-local,await:panes:{more},wait,wait,wait"),
        "bar:0,wait,wait,wait".to_owned(),
        shot_step(shots, &format!("ux-{name}-bar.ppm")),
        "bar-button:0:1,wait,press,wait,release,await:zoomed,wait,wait,wait".to_owned(),
        "bar-button:0:1,wait,press,wait,release,await:not-zoomed,wait,wait,wait".to_owned(),
        "bar:0,wait,press-right,wait,release-right,await:menu".to_owned(),
        format!("await:menu-offers:{menu_row},wait,wait,wait"),
        shot_step(shots, &format!("ux-{name}-menu.ppm")),
        "escape,await:no-menu,wait".to_owned(),
        // The shell beside it leaves the mouse alone, so a right click there is the menu.
        "pane-at:1:0.5:0.5,wait,press-right,wait,release-right,await:menu,await:menu-offers:Paste,escape,await:no-menu,wait".to_owned(),
        // Held by its bar, the pane docks under its neighbour, and back.
        "bar:0,wait,press,wait,pointer-by:40:30,wait,pane-at:1:0.5:0.9,wait,wait,wait,wait".to_owned(),
        shot_step(shots, &format!("ux-{name}-drag.ppm")),
        "release,await:split-column,wait,wait,wait".to_owned(),
        "bar:0,wait,press,wait,pointer-by:40:30,wait,pane-at:1:0.9:0.5,wait,wait,release,await:split-row,wait,wait,wait".to_owned(),
        // The neighbour goes by its own close button.
        format!("bar:1,wait,bar-button:1:0,wait,press,wait,release,await:panes:{panes},wait,wait,wait"),
    ]
    .join(",")
}

/// Resizing every panel by dragging, with the sizes coming back after a restart; the bar on panes;
/// the menus; and how a right click and a program that takes the mouse agree.
pub fn panes(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let scratch = Scratch::new();
    let shot = |name: &str| shot_step(shots, name);
    let reports = scratch.path("mouse-reports");
    // The first tab's shell asks for the mouse, and writes down what it gets.
    let shell =
        format!("printf '\\033[?1000h'; stty raw -echo; exec cat >> {}", reports.display(),);
    let script = [
        "await:setup,vault-create-plain,await:unlocked,await:panes:1,wait,wait,wait,wait".to_owned(),
        "pane-at:0:0.5:0.5,wait,press-right,wait,release-right,wait,wait".to_owned(),
        "await:no-menu".to_owned(),
        "shift-down,wait,press-right,wait,release-right,await:menu,shift-up,wait,wait".to_owned(),
        "await:menu-offers:Paste,await:menu-offers:Split right,await:menu-offers:Close".to_owned(),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-terminal-menu-light.ppm"),
        "theme:dark,wait,wait,wait,wait".to_owned(),
        shot("ux-terminal-menu-dark.ppm"),
        "shift-down,wait,pane-at:0:0.2:0.3,wait,press-right,wait,release-right,await:menu,shift-up,wait,wait".to_owned(),
        "escape,await:no-menu,wait".to_owned(),
        "tab-point:0,wait,press-right,wait,release-right,await:menu,await:menu-offers:Rename,wait,wait,wait".to_owned(),
        shot("ux-tab-menu-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-tab-menu-light.ppm"),
        "theme:dark,wait,wait,wait".to_owned(),
        "menu-row:Rename,wait,press,wait,release,await:no-menu,wait,wait,text:prod,wait,enter,await:tab-name:prod,wait".to_owned(),
        "new-tab,await:tabs:2,await:panes:2,wait,wait,wait,split-right-local,await:panes:3,wait,wait,wait,wait".to_owned(),
        "bar:0,wait,wait,wait".to_owned(),
        shot("ux-pane-bar-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-pane-bar-light.ppm"),
        "theme:dark,wait,wait,wait".to_owned(),
        "bar-button:0:1,wait,press,wait,release,await:zoomed,wait,wait,wait".to_owned(),
        "bar-button:0:1,wait,press,wait,release,await:not-zoomed,wait,wait,wait".to_owned(),
        // The divider: hovered it shows its pill, dragged it moves, and a double click evens it.
        "divider:0,wait,wait,wait".to_owned(),
        shot("ux-resize-divider-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-resize-divider-light.ppm"),
        "theme:dark,wait,wait,wait".to_owned(),
        "press,wait,pointer-by:-150:0,wait,pointer-by:-50:0,wait,release,await:ratio:0.2:0.42,wait".to_owned(),
        "divider:0,wait,press,release,wait,press,release,await:ratio:0.49:0.51,wait,wait,wait,wait".to_owned(),
        // The sidebar: by its edge, then double clicked back, then to where it is to stay.
        "sidebar-edge,wait,wait,wait".to_owned(),
        shot("ux-resize-sidebar-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-resize-sidebar-light.ppm"),
        "theme:dark,wait,wait,wait".to_owned(),
        "press,wait,pointer-by:40:0,wait,pointer-by:60:0,wait,release,await:sidebar:340:380,wait".to_owned(),
        "sidebar-edge,wait,press,release,press,release,await:sidebar:259:261,wait,wait,wait,wait".to_owned(),
        "sidebar-edge,wait,press,wait,pointer-by:-60:0,wait,release,await:sidebar:190:210,wait".to_owned(),
        // The smallest it goes.
        "sidebar-edge,wait,press,wait,pointer-by:-200:0,wait,release,await:sidebar:180:181,wait".to_owned(),
        "settings,wait,wait,wait".to_owned(),
        shot("ux-settings-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-settings-light.ppm"),
        "theme:dark,wait,wait,wait,escape,wait,wait".to_owned(),
        "dialog,wait,wait,wait".to_owned(),
        shot("ux-new-connection-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-new-connection-light.ppm"),
        "theme:dark,wait,wait,wait,escape,wait,wait,form:SSH,wait,wait,wait".to_owned(),
        shot("ux-new-connection-form-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-new-connection-form-light.ppm"),
        "theme:dark,wait,wait,wait,escape,wait,wait".to_owned(),
        // Hover and drag chrome leave nothing running: not a frame nor a wakeup once at rest.
        "pane-at:1:0.5:0.5,wait,wait,wait,wait,wait,wait,wait,wait,idle:1500".to_owned(),
        // Saved by the time the next steps run.
        "wait,wait,wait".to_owned(),
    ]
    .join(",");
    let more: [(&str, &str); 0] = [];
    let report = app_run_shell(bench_target, &scratch, "device", &script, &more, &shell)?;
    report_paths("panes", &report);
    if report.value("idle_measured") != 1
        || report.value("idle_frames") != 0
        || report.value("idle_wakeups") != 0
    {
        return Err(format!(
            "after hovering and dragging: {} frames and {} wakeups idle",
            report.value("idle_frames"),
            report.value("idle_wakeups"),
        ));
    }
    // X10 reporting: ESC [ M, then the button, 2 being the right one: it reached the program once
    // pressed without Shift, and never with it.
    let seen = fs::read(&reports).map_err(|err| format!("{}: {err}", reports.display()))?;
    let rights = seen
        .windows(4)
        .filter(|window| window.starts_with(b"\x1b[M") && window[3] == 32 + 2)
        .count();
    if rights != 1 {
        return Err(format!("the program saw {rights} right presses, not one: {seen:?}"));
    }

    // Sizes come back, and the tab keeps its name.
    let script = "await:panes:3,await:sidebar:180:181,wait,wait";
    app_run_shell(bench_target, &scratch, "device", script, &more, &shell)?;
    println!("ok   panes: resizing, the bar, zoom, menus and mouse reporting, with sizes kept");
    Ok(())
}

/// Split a terminal's answers to the program (`ESC [ ? ... c`) out of what it was sent: how many
/// there were, and the rest.
fn split_answers(seen: &[u8]) -> (usize, Vec<u8>) {
    let (mut answers, mut rest, mut at) = (0, Vec::new(), 0);
    while at < seen.len() {
        if seen[at..].starts_with(b"\x1b[?")
            && let Some(end) = seen[at..].iter().position(|&byte| byte == b'c')
        {
            answers += 1;
            at += end + 1;
        } else {
            rest.push(seen[at]);
            at += 1;
        }
    }
    (answers, rest)
}

/// Typing into a broadcast group: three local shells, two of them joined from the bar's button
/// and the menu. What is typed in the group reaches both of them once, and the third not at all;
/// what is typed in the third stays there. The terminals' own answers are not passed around.
pub fn broadcast(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let scratch = Scratch::new();
    let received = scratch.path("received");
    fs::create_dir_all(&received).map_err(|err| format!("{}: {err}", received.display()))?;
    // Each shell asks its terminal what it is, then writes down everything it is sent.
    let shell = format!(
        "f=$(mktemp -p {} in.XXXXXX); stty raw -echo; printf '\\033[c'; exec cat >> \"$f\"",
        received.display(),
    );
    // The bar's buttons from the right, in a tab of several panes: close, zoom, split down, split
    // right, broadcast.
    let script = [
        "await:setup,vault-create-plain,await:unlocked,await:panes:1,wait,wait".to_owned(),
        "split-right-local,await:panes:2,wait,wait,split-right-local,await:panes:3".to_owned(),
        "await:live-shown:3,wait,wait,wait".to_owned(),
        "bar:0,wait,wait,wait,bar-button:0:4,wait,press,wait,release,await:broadcast:1".to_owned(),
        "bar:1,wait,press-right,wait,release-right,await:menu".to_owned(),
        "await:menu-offers:Broadcast input,menu-row:Broadcast input,wait,press,wait,release"
            .to_owned(),
        "await:no-menu,await:broadcast:2,wait,wait".to_owned(),
        "bar:0,wait,press,wait,release,wait,commit:one,wait,wait,wait".to_owned(),
        shot_step(shots, "ux-broadcast.ppm"),
        "bar:2,wait,press,wait,release,wait,commit:solo,wait,wait".to_owned(),
        "bar:1,wait,wait,wait,bar-button:1:4,wait,press,wait,release,await:broadcast:1".to_owned(),
        "bar:0,wait,press,wait,release,wait,commit:last,wait,wait,wait,wait".to_owned(),
    ]
    .join(",");
    let more: [(&str, &str); 0] = [];
    app_run_shell(bench_target, &scratch, "device", &script, &more, &shell)?;

    let mut typed = Vec::new();
    for entry in fs::read_dir(&received).map_err(|err| format!("{}: {err}", received.display()))? {
        let path = entry.map_err(|err| err.to_string())?.path();
        let seen = fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let (answers, rest) = split_answers(&seen);
        if answers != 1 {
            return Err(format!("a shell got {answers} answers, not its own one: {seen:?}"));
        }
        typed.push(String::from_utf8_lossy(&rest).into_owned());
    }
    typed.sort();
    if typed != ["one", "onelast", "solo"] {
        return Err(format!("the shells were sent {typed:?}, not one, onelast and solo"));
    }
    println!("ok   broadcast: typing reaches the group once each, and nothing else");
    Ok(())
}

/// `.rdp` files as mstsc saves them, imported from the sidebar's add menu and by dropping one
/// on the window, each shown in the preview first. Needs no server.
pub fn rdp_import(bench_target: &Path, shots: &Path) -> Result<(), String> {
    const GATEWAY: &[u8] =
        include_bytes!("../../crates/rmx-app/tests/rdp/gateway-custom-effects.rdp");
    const UTF16: &[u8] = include_bytes!("../../crates/rmx-app/tests/rdp/utf16-bom.rdp");
    let scratch = Scratch::new();
    let picked = scratch.path("home/jump.rdp");
    let dropped = scratch.path("home/office.rdp");
    fs::write(&picked, GATEWAY).map_err(|err| format!("{}: {err}", picked.display()))?;
    fs::write(&dropped, UTF16).map_err(|err| format!("{}: {err}", dropped.display()))?;
    let row = "Import .rdp\u{2026}";
    let script = [
        "await:setup,vault-create-plain,await:unlocked,wait,wait".to_owned(),
        format!("add-menu,await:menu-offers:{row},menu-row:{row},wait,press,wait,release"),
        format!("await:prompt,answer:{},await:import:1,wait,wait,wait,wait", picked.display()),
        shot_step(shots, "rdp-import-preview.ppm"),
        "import-confirm:RDP files,await:no-question,await:tree:jump".to_owned(),
        "await:host:jump|10.20.30.40,await:tree:RDP files".to_owned(),
        format!("drop:{},await:import:1,wait,wait", dropped.display()),
        "import-confirm:Office,await:no-question,await:tree:office,await:tree:Office".to_owned(),
        "await:host:office|fd00::17,wait,wait".to_owned(),
        shot_step(shots, "rdp-imported.ppm"),
    ]
    .join(",");
    app_run(bench_target, &scratch, "device", &script)?;
    println!("ok   rdp-import: a picked and a dropped .rdp file, previewed, then saved");
    Ok(())
}

/// Tearing tabs off with a live SSH session, merging them back, the session reopened after a
/// restart behind the lock, and snippets sent from the palette.
pub fn windows(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let server = Sshd::start()?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let (port, password) = (server.port, server.password.clone());
    let shot = |name: &str| shot_step(shots, name);
    let more = [("RMX_BENCH_CLAIM_MS", CLAIM_MS)];

    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn:box|127.0.0.1|{port}|alice|{password}"),
        "await:host-key,hostkey:always,await:live,await:text:$ ".to_owned(),
        "run:echo before-tear,await:text:before-tear,wait,wait".to_owned(),
        shot("m8-tear-before.ppm"),
        tear_tab(1),
        "await:windows:2,win:1,await:tabs:1,await:live,wait,wait,wait,wait".to_owned(),
        shot("m8-tear-new-window.ppm"),
        // The same terminal, still connected: what it showed is there, and it still answers.
        "await:text:before-tear".to_owned(),
        "run:echo after-tear,await:text:after-tear,wait,wait".to_owned(),
        // Two windows, both at rest, and not a wakeup between them.
        "idle:1500".to_owned(),
        shot("m8-tear-window-1.ppm"),
        "win:0,wait,wait".to_owned(),
        shot("m8-tear-window-0.ppm"),
        // Locked, neither window shows what it holds; unlocked, both do, still connected.
        "vault-lock,await:locked,wait,wait,win:1,wait,wait".to_owned(),
        shot("m8-locked-window-1.ppm"),
        format!("vault-unlock:{vault_password}"),
        "await:unlocked,await:live,await:text:after-tear,win:1".to_owned(),
        merge_tab(0),
        "await:windows:1,await:tabs:2,await:live,await:text:after-tear".to_owned(),
        "run:echo after-merge,await:text:after-merge,wait,wait".to_owned(),
        shot("m8-merged.ppm"),
        // Snippets, from the palette, one of them asking for what it leaves open.
        "snippet-new:greet|echo snip-{{who:world}}-{{n}}~,await:snippet:greet".to_owned(),
        "snippet-new:plain|echo plain-snippet~,await:snippet:plain".to_owned(),
        "snippets,wait,wait,wait".to_owned(),
        shot("m8-snippets.ppm"),
        "escape,wait,wait".to_owned(),
        "palette,wait,text:plain,wait,enter,await:text:plain-snippet,wait".to_owned(),
        "palette,wait,text:greet,wait,enter,await:prompt,wait,wait".to_owned(),
        shot("m8-snippet-prompt.ppm"),
        "answer:rmx|7,await:no-question,await:text:snip-rmx-7,wait,wait".to_owned(),
        shot("m8-snippet-sent.ppm"),
        // Something that takes a password each time, split with a shell, and a window more.
        format!("conn:asks|127.0.0.1|{port}|alice|"),
        "await:prompt".to_owned(),
        format!("answer:{password}"),
        "await:live,await:text:$ ".to_owned(),
        "split-right-local,wait,wait,wait,wait".to_owned(),
        tear_tab(0),
        "await:windows:2,wait,wait,wait,wait".to_owned(),
    ]
    .join(",");
    let report = app_run_with(bench_target, &scratch, "device", &script, &more)?;
    report_paths("windows", &report);
    if report.value("idle_measured") != 1
        || report.value("idle_frames") != 0
        || report.value("idle_wakeups") != 0
    {
        return Err(format!(
            "two idle windows: {} frames and {} wakeups",
            report.value("idle_frames"),
            report.value("idle_wakeups"),
        ));
    }
    if report.value("connect_runs") != 2 {
        return Err(format!(
            "moving between windows reconnected: {} connects",
            report.value("connect_runs")
        ));
    }
    println!(
        "ok   windows: a new window took {} us on the main thread",
        report.value("window_open_us")
    );

    // Started and left locked: nothing connects, and the session stays saved.
    let script = "await:locked,wait,wait,wait,wait";
    let report = app_run(bench_target, &scratch, "device", script)?;
    if report.value("connect_runs") != 0 {
        return Err("something connected behind the lock".to_owned());
    }
    if report.value("first_frame_us") > crate::MAX_FIRST_FRAME {
        return Err(format!(
            "first frame with a session saved: {} us",
            report.value("first_frame_us")
        ));
    }
    println!(
        "ok   restore: first frame {} us, nothing connected while locked",
        report.value("first_frame_us")
    );

    let script = [
        "await:locked,wait,wait".to_owned(),
        format!("vault-unlock:{vault_password}"),
        "await:unlocked".to_owned(),
        "await:windows:2,await:panes:4,await:tabs:2,wait,wait".to_owned(),
        // No dialog for the one that takes a password: it waits.
        "await:no-question".to_owned(),
        "left,wait,await:awaiting,wait,wait".to_owned(),
        shot("m8-restored-awaiting.ppm"),
        "await:no-question".to_owned(),
        // The one with its password in the vault just comes back.
        "next-tab,await:live,await:text:$ ,wait,wait".to_owned(),
        shot("m8-restored-ssh.ppm"),
        "win:1,await:live-shown:1,wait,wait".to_owned(),
        shot("m8-restored-window-1.ppm"),
        // Asked for, the waiting one asks.
        "win:0,next-tab,await:awaiting,reconnect,await:prompt,wait".to_owned(),
        shot("m8-restored-asked.ppm"),
        format!("answer:{password}"),
        "await:live,await:text:$ ,wait".to_owned(),
    ]
    .join(",");
    let report = app_run_with(bench_target, &scratch, "device", &script, &more)?;
    report_paths("restored session", &report);
    println!("ok   restore: layout back, a pane awaiting without a dialog, the others reconnected");
    Ok(())
}

/// Where things are in the first-run form, at the 1280x800 every scripted run has.
const SETUP_AGREE: &str = "move:600:457";
const SETUP_CREATE: &str = "move:640:502";
/// Where "New local shell" is on the empty page, with the sidebar open.
const EMPTY_SHELL: &str = "move:770:400";

/// The first bytes of a database with no password.
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

fn is_plain(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes.starts_with(SQLITE_HEADER))
}

fn must_be(path: &Path, plain: bool) -> Result<(), String> {
    if is_plain(path) == plain {
        return Ok(());
    }
    let wanted = if plain { "plain SQLite" } else { "encrypted" };
    Err(format!("{} should be {wanted}", path.display()))
}

/// A database with no password: asked for in the form, used for a real session, opened again with
/// no prompt, kept in step with another copy, and kept from mixing with an encrypted one.
pub fn no_passphrase(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let shot = |name: &str| shot_step(shots, name);

    // Neither Enter nor a request without the box ticked gets a database made.
    let scratch = Scratch::new();
    let script = "await:setup,wait,wait,enter,wait,wait,wait,await:setup,vault-create:,await:vault-error,wait";
    let report = app_run(bench_target, &scratch, "refused", script)?;
    report_paths("no passphrase refused", &report);
    if fs::metadata(scratch.path("refused/vault.rmxdb")).is_ok_and(|meta| meta.len() > 0) {
        return Err("a database was created without the warning being agreed to".to_owned());
    }
    println!("ok   no passphrase: nothing is created before the box is ticked");

    let server = Sshd::start()?;
    let scratch = Scratch::new();
    let (port, password) = (server.port, server.password.clone());
    let script = [
        "await:setup,wait,wait".to_owned(),
        shot("fix-create-warning-dark.ppm"),
        "theme:light,wait,wait".to_owned(),
        shot("fix-create-warning-light.ppm"),
        "theme:dark,wait".to_owned(),
        // Pressing Create with the box unticked does nothing either.
        format!("{SETUP_CREATE},wait,press,release,wait,wait,await:setup"),
        format!("{SETUP_AGREE},wait,press,release,wait,wait"),
        format!("{SETUP_CREATE},wait,press,release"),
        "await:unlocked,await:unencrypted".to_owned(),
        format!("conn:box|127.0.0.1|{port}|alice|{password}"),
        "await:host-key,hostkey:always,await:live,await:text:$ ".to_owned(),
        "run:echo plain-$((6*7)),await:text:plain-42,wait,wait".to_owned(),
        shot("fix-unencrypted-indicator-dark.ppm"),
        "theme:light,wait,wait".to_owned(),
        shot("fix-unencrypted-indicator-light.ppm"),
        "theme:dark,wait".to_owned(),
        // There is nothing to lock it with.
        "vault-lock,wait,wait,wait,wait,await:unlocked".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("no passphrase", &report);
    let vault = scratch.path("device/vault.rmxdb");
    must_be(&vault, true)?;
    if report.value("argon2_runs") != 0 {
        return Err("a key was derived for a database with no password".to_owned());
    }
    // No prompt: only `await:unlocked`, which an unlock screen would never let through.
    let script = "await:unlocked,await:unencrypted,await:tree:box,await:live,await:text:$ ,wait";
    let report = app_run(bench_target, &scratch, "device", script)?;
    report_paths("no passphrase, second start", &report);
    if report.value("argon2_runs") != 0 {
        return Err("the second start derived a key".to_owned());
    }
    println!(
        "ok   no passphrase: created through the form, a real session, restarted with no prompt"
    );

    let script =
        "await:unlocked,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,await:unlocked";
    app_run_with(bench_target, &scratch, "device", script, &[("RMX_BENCH_AUTO_LOCK_MS", "400")])?;
    println!("ok   no passphrase: auto-lock does not apply");

    plain_sync(bench_target)?;
    drop(server);
    Ok(())
}

/// Two copies with no password in step; and refusal, both ways round, against the other kind.
fn plain_sync(bench_target: &Path) -> Result<(), String> {
    let scratch = Scratch::new();
    let password = random_hex();
    let folder = |name: &str| -> Result<String, String> {
        let path = scratch.path(name);
        fs::create_dir_all(&path).map_err(|err| err.to_string())?;
        Ok(format!("local|||||{}", path.join("vault.rmxdb").display()))
    };

    let plain_target = folder("synced-plain")?;
    let plain_file = scratch.path("synced-plain/vault.rmxdb");
    let a = format!(
        "await:setup,vault-create-plain,await:unlocked,sync-set:{plain_target},wait,wait,\
         conn:from-a|127.0.0.1|1|u|p,await:failed,sync-now,await:synced,wait"
    );
    report_paths("plain sync: first device", &app_run(bench_target, &scratch, "a", &a)?);
    must_be(&plain_file, true)?;
    let b = format!(
        "await:setup,join-plain:{plain_target},await:unlocked,await:unencrypted,await:tree:from-a,\
         conn:from-b|127.0.0.1|1|u|p,await:failed,sync-now,await:synced,wait"
    );
    report_paths("plain sync: second device", &app_run(bench_target, &scratch, "b", &b)?);
    must_be(&scratch.path("b/vault.rmxdb"), true)?;
    let again = "await:unlocked,sync-now,await:synced,await:tree:from-b,await:tree:from-a,wait";
    report_paths("plain sync: first device again", &app_run(bench_target, &scratch, "a", again)?);
    must_be(&plain_file, true)?;
    println!("ok   no passphrase: two plain copies converged");

    // Against a plain remote, an encrypted copy stops with neither side touched.
    let before = fs::read(&plain_file).map_err(|err| err.to_string())?;
    let c = format!(
        "await:setup,vault-create:{password},await:unlocked,sync-set:{plain_target},wait,wait,\
         conn:from-c|127.0.0.1|1|u|p,await:failed,sync-now,await:sync-failed:unencrypted,wait"
    );
    report_paths("mismatch: encrypted against plain", &app_run(bench_target, &scratch, "c", &c)?);
    if fs::read(&plain_file).map_err(|err| err.to_string())? != before {
        return Err("the plain remote was changed by an encrypted copy".to_owned());
    }
    must_be(&scratch.path("c/vault.rmxdb"), false)?;
    let reopened =
        format!("await:locked,vault-unlock:{password},await:unlocked,await:tree:from-c,wait");
    app_run(bench_target, &scratch, "c", &reopened)?;

    // Against an encrypted remote, a plain copy stops too.
    let encrypted_target = folder("synced-encrypted")?;
    let encrypted_file = scratch.path("synced-encrypted/vault.rmxdb");
    let e = format!(
        "await:setup,vault-create:{password},await:unlocked,sync-set:{encrypted_target},wait,wait,\
         conn:from-e|127.0.0.1|1|u|p,await:failed,sync-now,await:synced,wait"
    );
    app_run(bench_target, &scratch, "e", &e)?;
    must_be(&encrypted_file, false)?;
    let before = fs::read(&encrypted_file).map_err(|err| err.to_string())?;
    let d = format!(
        "await:setup,vault-create-plain,await:unlocked,sync-set:{encrypted_target},wait,wait,\
         conn:from-d|127.0.0.1|1|u|p,await:failed,sync-now,await:sync-failed:encrypted,wait"
    );
    report_paths("mismatch: plain against encrypted", &app_run(bench_target, &scratch, "d", &d)?);
    if fs::read(&encrypted_file).map_err(|err| err.to_string())? != before {
        return Err("the encrypted remote was changed by a plain copy".to_owned());
    }
    must_be(&scratch.path("d/vault.rmxdb"), true)?;
    let reopened = "await:unlocked,await:tree:from-d,wait";
    app_run(bench_target, &scratch, "d", reopened)?;

    // Nor does joining mix them, whichever way it is asked for.
    let wrong = format!("await:setup,join:{password}|{plain_target},await:vault-error,wait");
    app_run(bench_target, &scratch, "f", &wrong)?;
    let wrong = format!("await:setup,join-plain:{encrypted_target},await:vault-error,wait");
    app_run(bench_target, &scratch, "g", &wrong)?;
    for device in ["f", "g"] {
        if fs::metadata(scratch.path(&format!("{device}/vault.rmxdb")))
            .is_ok_and(|meta| meta.len() > 0)
        {
            return Err("joining the other kind of copy left a database behind".to_owned());
        }
    }
    println!("ok   no passphrase: a mismatch stops the sync and leaves both sides as they were");
    Ok(())
}

/// Quick connect without saving: `user@host:port` connects and puts nothing in the vault, its
/// host key is still remembered, it reconnects, a later start does not reopen it, and saving it
/// from the pane's menu keeps the session it has.
pub fn quick(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let server = Sshd::start()?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let (port, password) = (server.port, server.password.clone());
    let target = format!("alice@127.0.0.1:{port}");
    let shot = |name: &str| shot_step(shots, name);

    let script = [
        format!("await:setup,vault-create:{vault_password},await:unlocked,await:panes:1"),
        format!("quick:{target},await:host-key,hostkey:always"),
        // Nothing saved, so nothing to log in with: the password is asked for.
        format!("await:prompt,answer:{password},await:live,await:text:$ "),
        format!("await:panes:2,await:no-tree:{target}"),
        "run:echo quick-$((6*7)),await:text:quick-42,wait,wait".to_owned(),
        shot("quick-connected.ppm"),
        // Again from the same draft, and its key is known now: only the password is asked.
        "disconnect,await:closed,reconnect".to_owned(),
        format!("await:prompt,answer:{password},await:live,await:text:$ "),
        format!("await:no-tree:{target}"),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("quick connect", &report);

    // The quick tab is not reopened: only the local shell is. Then saved from the pane's menu, it
    // is in the tree, and still the one pane, connected.
    let script = [
        format!("await:locked,vault-unlock:{vault_password},await:unlocked,await:panes:1"),
        "wait,wait,wait,wait,await:panes:1".to_owned(),
        format!("quick:{target},await:prompt,answer:{password},await:live,await:text:$ "),
        "await:panes:2,bar:0,wait,press-right,wait,release-right,await:menu".to_owned(),
        "await:menu-offers:Save connection,menu-row:Save connection,wait,press,wait,release"
            .to_owned(),
        "await:dialog,wait,wait".to_owned(),
        shot("quick-save.ppm"),
        "rename:quick-box,await:tree:quick-box,wait,wait,await:panes:2,await:live".to_owned(),
        "run:echo still-$((6*7)),await:text:still-42".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("quick connect saved", &report);

    // Saved, it is reopened like any other.
    let script = [
        format!("await:locked,vault-unlock:{vault_password},await:unlocked"),
        "await:tree:quick-box,await:panes:2".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("quick connect reopened", &report);
    Ok(())
}

/// Closing the last tab, however it happens: the application stays, offering a new one.
pub fn last_tab(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let shot = |name: &str| shot_step(shots, name);
    let scratch = Scratch::new();
    let more = [("RMX_BENCH_CLAIM_MS", CLAIM_MS)];
    let settle = "wait,".repeat(12);
    let script = [
        "await:setup,vault-create-plain,await:unlocked,await:tabs:1,await:live-shown:1,wait,wait"
            .to_owned(),
        // By the shortcut's action.
        "close,await:tabs:0,await:panes:0".to_owned(),
        format!("{settle}wait"),
        shot("fix-empty-dark.ppm"),
        "theme:light,wait,wait".to_owned(),
        shot("fix-empty-light.ppm"),
        format!("theme:dark,{settle}wait"),
        // Nothing moves on the empty page: no frames, no wakeups.
        "idle:1500,wait".to_owned(),
        // Its own button, pressed with the pointer.
        format!("{EMPTY_SHELL},wait,press,release,await:tabs:1,await:live-shown:1,wait"),
        // By the tab's button.
        "tab-close:0,await:tabs:0,await:panes:0,wait,wait".to_owned(),
        // The keyboard shortcuts work with nothing open.
        "shortcut:t,await:tabs:1,await:live-shown:1,wait".to_owned(),
        "close,await:tabs:0,wait,wait".to_owned(),
        "shortcut:n,await:dialog,escape,wait,wait,await:tabs:0".to_owned(),
        // A torn-off window with nothing left in it goes by itself.
        "new-tab,await:tabs:1,new-tab,await:tabs:2,wait,wait".to_owned(),
        tear_tab(1),
        "await:windows:2,win:1,await:tabs:1,wait,wait,wait".to_owned(),
        "close,await:windows:1,win:0,await:tabs:1,await:panes:1,wait,wait".to_owned(),
        // Back to the only window, whose last tab goes the same way as the first.
        "close,await:tabs:0,wait,wait,wait,await:windows:1".to_owned(),
        "wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait".to_owned(),
    ]
    .join(",");
    let report = app_run_with(bench_target, &scratch, "device", &script, &more)?;
    report_paths("last tab", &report);
    if report.value("idle_measured") != 1
        || report.value("idle_frames") != 0
        || report.value("idle_wakeups") != 0
    {
        return Err(format!(
            "the empty page: {} frames and {} wakeups",
            report.value("idle_frames"),
            report.value("idle_wakeups"),
        ));
    }
    println!(
        "ok   last tab: closed three ways and stayed alive, idle at 0 frames and 0 wakeups, torn-off window closed alone"
    );

    // Left with nothing open, it comes back with nothing open, and carries on.
    let script = format!(
        "await:unlocked,{settle}await:tabs:0,await:panes:0,new-tab,await:tabs:1,await:live-shown:1,wait"
    );
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("last tab, restart", &report);
    println!("ok   last tab: restarted to the empty state");

    // The window's own close button still ends it, here as anywhere. The hold never releases, so
    // only the application ending lets the run finish clean.
    let script = "await:unlocked,await:live-shown:1,wait,close,await:tabs:0,wait,wait,wait,\
                  move:1260:20,wait,press,release,await:tabs:99";
    app_run(bench_target, &scratch, "device", script)?;
    println!("ok   last tab: the window's close button quits from the empty state");

    // Giving up at the first-run form is not leaving the window empty: the first tab still comes.
    let scratch = Scratch::new();
    let script = format!("await:setup,{settle}wait");
    app_run(bench_target, &scratch, "device", &script)?;
    let script =
        "await:setup,vault-create-plain,await:unlocked,await:tabs:1,await:live-shown:1,wait";
    let report = app_run(bench_target, &scratch, "device", script)?;
    report_paths("last tab, first run after giving up", &report);
    println!("ok   last tab: no tabs yet is not the same as all tabs closed");

    // The shell ending by itself is the same as closing the tab. Without `-e`, as a person runs it.
    let scratch = Scratch::new();
    let script = "await:setup,vault-create-plain,await:unlocked,await:tabs:1,await:live-shown:1,\
                  await:text:$ ,wait,run:exit,await:tabs:0,await:panes:0,wait,wait,wait,await:windows:1,\
                  new-tab,await:tabs:1,await:live-shown:1";
    let report =
        app_run_shell(bench_target, &scratch, "device", script, &[("SHELL", "/bin/sh")], "")?;
    report_paths("last tab, shell exit", &report);
    println!("ok   last tab: a shell exiting by itself leaves the empty state");

    // `-e` is a terminal for one program: when that ends with nothing else open, so does the app.
    // The hold never releases, so reaching it would take the script's full 30 s and fail.
    let scratch = Scratch::new();
    let script = "await:setup,vault-create-plain,await:unlocked,await:tabs:99";
    let started = Instant::now();
    app_run_shell(bench_target, &scratch, "device", script, &[], "exit 0")?;
    if started.elapsed() > Duration::from_secs(20) {
        return Err("rmx -e did not end with its program".to_owned());
    }
    println!("ok   last tab: rmx -e ends with its program");
    Ok(())
}

/// Every way of closing asks before it ends a live SSH session, and none asks for a local shell, a
/// disconnected session, or a tab torn off and merged back. "Don't ask again" holds across a
/// restart, and the setting brings the question back.
pub fn closing(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let server = Sshd::start()?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let (port, password) = (server.port, server.password.clone());
    let shot = |name: &str| shot_step(shots, name);
    let more = [("RMX_BENCH_CLAIM_MS", CLAIM_MS)];
    // Asked, and cancelled: the session stays.
    let kept = |tabs: usize| {
        format!("escape,await:no-question,await:windows:1,await:tabs:{tabs},await:live,wait")
    };
    let one = "await:asking:Disconnect 1 live session?,await:asking:box";

    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked,await:tabs:1".to_owned(),
        format!("conn:box|127.0.0.1|{port}|alice|{password}"),
        "await:host-key,hostkey:always,await:live,await:text:$ ,await:tabs:2".to_owned(),
        // The shortcut, the tab's button, the window's button and the system closing the window.
        format!("close,{one},wait,wait"),
        shot("close-confirm.ppm"),
        kept(2),
        format!("tab-close:1,{one},{}", kept(2)),
        format!("window-close,{one},{}", kept(2)),
        format!("os-close,{one},{}", kept(2)),
        // A local shell beside it goes without a word.
        "split-right-local,await:panes:3,wait,wait,wait".to_owned(),
        "close,await:panes:2,await:no-question,await:live,wait".to_owned(),
        // Torn off into a window of its own and merged back, it neither ends nor asks.
        tear_tab(1),
        "await:windows:2,win:1,await:tabs:1,await:live,await:no-question,wait,wait".to_owned(),
        merge_tab(0),
        "await:windows:1,await:tabs:2,await:live,await:no-question".to_owned(),
        "run:echo still-here,await:text:still-here,wait".to_owned(),
        // Two sessions: the window names how many.
        "open:box,await:tabs:3,await:live,await:text:$ ".to_owned(),
        "os-close,await:asking:Disconnect 2 live sessions?,wait,wait".to_owned(),
        shot("close-confirm-two.ppm"),
        kept(3),
        // A disconnected session is not asked about.
        "disconnect,await:closed,close,await:tabs:2,await:no-question,wait".to_owned(),
        // Confirmed, it closes.
        format!("tab-close:1,{one},confirm,await:tabs:1,await:panes:1,await:no-question"),
        // Told not to ask again, it no longer does.
        "open:box,await:tabs:2,await:live".to_owned(),
        format!("close,{one},dont-ask,wait,wait"),
        shot("close-dont-ask.ppm"),
        "confirm,await:tabs:1,await:no-question".to_owned(),
        "open:box,await:tabs:2,await:live,close,await:tabs:1,await:no-question,wait".to_owned(),
    ]
    .join(",");
    let report = app_run_with(bench_target, &scratch, "device", &script, &more)?;
    report_paths("closing", &report);
    println!("ok   closing: every close asks for a live session, and only for one");

    // Still not asked after a restart: the window closes at once, and with it the application.
    // The hold never releases, so only the application ending lets the run finish clean.
    let script = [
        "await:locked".to_owned(),
        format!("vault-unlock:{vault_password}"),
        "await:unlocked,await:tabs:1".to_owned(),
        "open:box,await:tabs:2,await:live,wait,window-close,await:tabs:99".to_owned(),
    ]
    .join(",");
    app_run(bench_target, &scratch, "device", &script)?;
    println!("ok   closing: \"Don't ask again\" holds after a restart");

    // Turned back on in the settings, the system closing the window asks again, and quits once
    // confirmed.
    let script = [
        "await:locked".to_owned(),
        format!("vault-unlock:{vault_password}"),
        "await:unlocked".to_owned(),
        "settings,await:settings,wait,wait".to_owned(),
        shot("close-setting.ppm"),
        "confirm-close:on,wait,wait".to_owned(),
        "open:box,await:live,wait,os-close,await:asking:box,confirm,await:tabs:99".to_owned(),
    ]
    .join(",");
    app_run(bench_target, &scratch, "device", &script)?;
    println!("ok   closing: the setting brings the question back, and quitting waits for it");
    Ok(())
}

/// A busybox telnetd giving a shell straight away.
fn telnet_server() -> Result<Server, String> {
    ensure_image(
        TELNET_IMAGE,
        "FROM docker.io/library/alpine:3.22\nRUN apk add --no-cache --quiet busybox-extras\n",
    )?;
    let publish = ["127.0.0.1::23".to_owned(), "127.0.0.1::22".to_owned()];
    let script = "busybox-extras telnetd -F -p 23 -l /bin/sh & touch /tmp/ready; wait";
    let server = Server::start(TELNET_IMAGE, &publish, &[], script)?;
    server.wait("test -f /tmp/ready && nc -z 127.0.0.1 23", Duration::from_secs(30))?;
    Ok(server)
}

/// A shell at the far end of a pty, standing in for a device on a serial line. Dropping it takes
/// the port away.
struct SerialLine {
    process: std::process::Child,
    path: PathBuf,
}

impl SerialLine {
    fn start(path: PathBuf) -> Result<Self, String> {
        let port = format!("pty,raw,echo=0,link={}", path.display());
        let process = Command::new("socat")
            .args([&port, "EXEC:/bin/sh,pty,stderr,setsid,sane"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("socat: {err}"))?;
        let line = Self { process, path };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !line.path.exists() {
            if Instant::now() > deadline {
                return Err("socat did not make the pty".to_owned());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(line)
    }
}

impl Drop for SerialLine {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = fs::remove_file(&self.path);
    }
}

/// Telnet, directly and through an SSH tunnel; a serial line, and that line missing on restart.
pub fn terminals(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let server = telnet_server()?;
    let gate = optional("telnet through an ssh tunnel", Sshd::beside(&server))?;
    let port = server.port(23)?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let shot = |name: &str| shot_step(shots, name);
    let mut script = vec![
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        "dialog,wait,wait,wait".to_owned(),
        shot("m8-new-connection-tiles.ppm"),
        "escape,wait,form:Telnet,wait,wait,wait".to_owned(),
        shot("m8-telnet-form.ppm"),
        "escape,wait,form:Serial,wait,wait,wait,wait,wait".to_owned(),
        shot("m8-serial-form.ppm"),
        "escape,wait".to_owned(),
        format!("conn-as:Telnet|telly|127.0.0.1|{port}||||"),
        "await:live,wait,wait,wait".to_owned(),
        // Logged from the pane's menu, on through a reconnect, into the one file.
        "log-toggle,await:notice:Logging output to".to_owned(),
        "run:echo tel-$((6*7)),await:text:tel-42,wait,wait".to_owned(),
        shot("m8-telnet.ppm"),
        "disconnect,await:closed,reconnect,await:live,wait,wait".to_owned(),
        "run:echo back-$((6*7)),await:text:back-42".to_owned(),
        "log-toggle,await:notice:Stopped logging,wait".to_owned(),
    ];
    if let Some(gate) = &gate {
        script.extend([
            format!("conn:gate|127.0.0.1|{}|alice|{}", gate.port, gate.password),
            "await:host-key,hostkey:always,await:live".to_owned(),
            "conn-as:Telnet|telly-tunnelled|127.0.0.1|23|||gate|".to_owned(),
            "await:live,wait,wait,wait".to_owned(),
            "run:echo via-$((6*7)),await:text:via-42,wait".to_owned(),
            shot("m8-telnet-tunnelled.ppm"),
        ]);
    }
    let report = app_run(bench_target, &scratch, "telnet", &script.join(","))?;
    report_paths("telnet", &report);
    let logs = session_logs(&scratch.path("telnet"))?;
    let [log] = logs.as_slice() else { return Err(format!("telnet: logs {logs:?}")) };
    if !file_contains(log, "tel-42") || !file_contains(log, "back-42") {
        return Err(format!("telnet: {} has not both sessions in it", log.display()));
    }
    let tunnel = if gate.is_some() { ", and through an ssh tunnel" } else { "" };
    println!("ok   telnet: a shell, reconnected, logged{tunnel}");

    if !installed("socat") {
        skip_part("serial", "socat, which stands in for the device, is not installed");
        return Ok(());
    }
    let scratch = Scratch::new();
    let path = scratch.path("ttyTEST0");
    let line = SerialLine::start(path.clone())?;
    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn-as:Serial|console|{}|115200|||log", path.display()),
        "await:live,wait,wait".to_owned(),
        "run:echo ser-$((6*7)),await:text:ser-42,wait,wait".to_owned(),
        shot("m8-serial.ppm"),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "serial", &script)?;
    report_paths("serial", &report);
    let logs = session_logs(&scratch.path("serial"))?;
    if !logs.iter().any(|log| file_contains(log, "ser-42")) {
        return Err(format!("serial: the connection's log is not in {logs:?}"));
    }
    drop(line);

    // The port has gone: reopened, the pane waits. Asked for, it says why.
    let script = [
        "await:locked,wait".to_owned(),
        format!("vault-unlock:{vault_password}"),
        "await:unlocked,await:awaiting,await:no-question,wait,wait".to_owned(),
        shot("m8-serial-missing.ppm"),
        "reconnect,await:failed,wait".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "serial", &script)?;
    report_paths("serial, port gone", &report);
    println!(
        "ok   serial: a shell over a pty, and the port gone on restart leaves the pane waiting"
    );
    Ok(())
}

/// One idle SSH pane behind a real unlock, for the bench.
pub fn idle_ssh_pane(bench_target: &Path) -> Result<Report, String> {
    let server = Sshd::start()?;
    let scratch = Scratch::new();
    let password = random_hex();
    let (port, secret) = (server.port, &server.password);
    let script = format!(
        "await:setup,vault-create:{password},await:unlocked,vault-lock,await:locked,\
         vault-unlock:{password},await:unlocked,conn:idle|127.0.0.1|{port}|alice|{secret},\
         await:host-key,hostkey:always,await:live,await:text:$ ,wait,wait,wait,wait,wait,wait,\
         wait,wait,wait,wait,idle:3000,wait"
    );
    app_run(bench_target, &scratch, "device", &script)
}

/// A throwaway container with a command of its own. Dropping it removes the container.
struct Server {
    name: String,
}

impl Server {
    fn start(
        image: &str,
        publish: &[String],
        secrets: &[(&str, &str)],
        script: &str,
    ) -> Result<Self, String> {
        let name = format!("rmx-e2e-{}", random_hex());
        let mut command = Command::new("podman");
        command.args(["run", "-d", "--rm", "--name", &name]);
        for spec in publish {
            command.arg("-p").arg(spec);
        }
        // Named here and passed through the environment, so that they stay out of `ps`.
        for (key, value) in secrets {
            command.arg("--env").arg(key).env(key, value);
        }
        command.arg(image).args(["sh", "-c", script]);
        let output = command.output().map_err(podman_missing)?;
        if !output.status.success() {
            return Err(format!("podman run: {}", String::from_utf8_lossy(&output.stderr)));
        }
        Ok(Self { name })
    }

    /// The port on 127.0.0.1 that `port` is published on. Nothing but loopback will do.
    fn port(&self, port: u16) -> Result<u16, String> {
        let mapping = podman(&["port", &self.name, &format!("{port}/tcp")])?;
        let line = mapping.lines().next().unwrap_or("");
        if !line.starts_with("127.0.0.1:") {
            return Err(format!("not published on loopback only: {line}"));
        }
        line.rsplit(':').next().and_then(|p| p.parse().ok()).ok_or_else(|| "no port".to_owned())
    }

    fn exec(&self, command: &str) -> Result<String, String> {
        podman(&["exec", "--env", "DISPLAY=:1", &self.name, "sh", "-c", command])
    }

    fn exec_ok(&self, command: &str) -> bool {
        self.exec(command).is_ok()
    }

    fn wait(&self, ready: &str, limit: Duration) -> Result<(), String> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.exec_ok(ready) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!("the server in {} did not become ready", self.name))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", "-t", "0", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Podman could not be started: everything needing a server is skipped, not failed.
fn podman_missing(err: std::io::Error) -> String {
    skip(format!("podman: {err}"))
}

/// Build `tag` from `containerfile` unless it is already here.
fn ensure_image(tag: &str, containerfile: &str) -> Result<(), String> {
    let scratch = Scratch::new();
    let directory = scratch.path("build");
    fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
    fs::write(directory.join("Containerfile"), containerfile).map_err(|err| err.to_string())?;
    ensure_image_from(tag, &directory)
}

/// Build `tag` from the directory `context` unless it is already here. An image that cannot be
/// built, as with no network, skips the scenario.
fn ensure_image_from(tag: &str, context: &Path) -> Result<(), String> {
    let exists = Command::new("podman").args(["image", "exists", tag]).status();
    if exists.map_err(podman_missing)?.success() {
        return Ok(());
    }
    // Tried once a run: with no network, every try would wait as long for the same answer.
    static UNBUILDABLE: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let tried =
        UNBUILDABLE.lock().unwrap().iter().find(|(t, _)| t == tag).map(|(_, why)| why.clone());
    if let Some(why) = tried {
        return Err(skip(why));
    }
    println!("building {tag}");
    let built = Command::new("podman")
        .args(["build", "-q", "-t", tag])
        .arg(context)
        .stdout(Stdio::null())
        .output()
        .map_err(podman_missing)?;
    if built.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&built.stderr);
    let why = why.lines().rfind(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let why = format!("no image {tag}, and building it failed: {why}");
    UNBUILDABLE.lock().unwrap().push((tag.to_owned(), why.clone()));
    Err(skip(why))
}

/// A port nothing listens on just now, and the `count` after it.
fn free_port_range(count: u16) -> u16 {
    loop {
        let base = free_port();
        if base < 60_000
            && (0..count).all(|offset| TcpListener::bind(("127.0.0.1", base + offset)).is_ok())
        {
            return base;
        }
    }
}

fn shot_step(shots: &Path, name: &str) -> String {
    format!("shot:{}", shots.join(name).display())
}

/// A vsftpd with a user to log in as, offering TLS to those who ask for it.
fn ftp_server(password: &str, pasv: u16) -> Result<Server, String> {
    ensure_image(
        FTP_IMAGE,
        "FROM docker.io/library/alpine:3.22\nRUN apk add --no-cache --quiet vsftpd openssl\n",
    )?;
    let script = format!(
        r#"set -e
adduser -D -s /bin/sh alice
printf 'alice:%s\n' "$RMX_PASSWORD" | chpasswd
unset RMX_PASSWORD
openssl req -x509 -newkey rsa:2048 -nodes -keyout /etc/vsftpd/key.pem -out /etc/vsftpd/cert.pem -days 1 -subj /CN=rmx-ftps-test 2>/dev/null
cat > /etc/vsftpd/vsftpd.conf <<CONFIG
listen=YES
listen_ipv6=NO
background=NO
anonymous_enable=NO
local_enable=YES
write_enable=YES
pasv_enable=YES
pasv_min_port={pasv}
pasv_max_port={last}
pasv_address=127.0.0.1
pasv_promiscuous=YES
check_shell=NO
seccomp_sandbox=NO
local_umask=022
ssl_enable=YES
rsa_cert_file=/etc/vsftpd/cert.pem
rsa_private_key_file=/etc/vsftpd/key.pem
force_local_logins_ssl=NO
force_local_data_ssl=NO
CONFIG
touch /tmp/ready
# Not `exec`: vsftpd's parent process does not survive running as PID 1.
/usr/sbin/vsftpd /etc/vsftpd/vsftpd.conf 0>&2
echo vsftpd exited with $?
sleep 100000
"#,
        pasv = pasv,
        last = pasv + 4,
    );
    let publish = [
        "127.0.0.1::21".to_owned(),
        "127.0.0.1::22".to_owned(),
        format!("127.0.0.1:{pasv}-{}:{pasv}-{}", pasv + 4, pasv + 4),
    ];
    let server = Server::start(FTP_IMAGE, &publish, &[("RMX_PASSWORD", password)], &script)?;
    server.wait("test -f /tmp/ready && nc -z 127.0.0.1 21", Duration::from_secs(60))?;
    Ok(server)
}

/// Keep reading a server's clipboard while a session runs, which is the only time there is one to
/// read: `xclip` asks the connected client. The flag says `wanted` was seen.
fn watch_clipboard(
    name: String,
    user: Option<(String, String)>,
    wanted: &'static str,
) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let seen = flag.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            let mut command = Command::new("podman");
            command.arg("exec");
            match &user {
                Some((user, xauthority)) => {
                    command.args(["-u", user, "--env", "DISPLAY=:10", "--env", xauthority]);
                },
                None => {
                    command.args(["--env", "DISPLAY=:1"]);
                },
            }
            command.args([
                &name,
                "sh",
                "-c",
                "timeout 2 xclip -selection clipboard -o 2>/dev/null",
            ]);
            let Ok(output) = command.output() else { return };
            if String::from_utf8_lossy(&output.stdout) == wanted {
                seen.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    flag
}

/// Fill in the same content on the server: a small file to download and a big one to cancel.
fn seed_remote(exec: impl Fn(&str) -> Result<String, String>, home: &str) -> Result<(), String> {
    exec(&format!(
        "printf 'from-the-server' > {home}/down.txt && truncate -s 2147483648 {home}/big.bin && \
         chown alice:alice {home}/down.txt {home}/big.bin"
    ))?;
    Ok(())
}

/// A file browsing session: list a column, drag a file down, upload one, then cancel and retry.
fn browse_and_copy(
    bench_target: &Path,
    scratch: &Scratch,
    device: &str,
    shots: &Path,
    name: &str,
    opening: &[String],
    remote_home: &str,
) -> Result<(), String> {
    let home = scratch.path("home");
    let local = home.join("ftp-side");
    fs::create_dir_all(&local).map_err(|err| err.to_string())?;
    fs::write(local.join("up.txt"), b"from-the-client").map_err(|err| err.to_string())?;
    let sample = "#!/usr/bin/env python3\n# Say hello.\nimport sys\n\nclass Greeter:\n    def greet(self, name: str) -> str:\n        return f\"hello {name}\" * 2\n\nGreeter().greet(sys.argv[1])\n";
    fs::write(local.join("sample.py"), sample).map_err(|err| err.to_string())?;
    let local = local.display().to_string();
    let shot = |file: &str| shot_step(shots, &format!("{name}-{file}"));

    let mut script: Vec<String> = opening.to_vec();
    script.extend([
        format!("files-goto:L|{local}"),
        format!("await:at-path:L|{local}"),
        "await:listed:L|up.txt".to_owned(),
        format!("files-goto:R|{remote_home}"),
        "await:listed:R|down.txt".to_owned(),
        "wait,wait".to_owned(),
        shot("listing.ppm"),
        pane_chrome(2, "Disconnect", "files", shots),
        // Its two columns and its queue, resized by their edges, and a double click evens them.
        "files-split,wait,wait,wait".to_owned(),
        shot("ux-resize-files-split-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-resize-files-split-light.ppm"),
        "theme:dark,wait,wait,wait".to_owned(),
        "press,wait,pointer-by:-120:0,wait,release,await:files-split:0.3:0.45,wait,wait".to_owned(),
        "files-split,wait,press,release,press,release,await:files-split:0.49:0.51,wait,wait,wait,wait".to_owned(),
        "files-queue,wait,press,wait,pointer-by:0:-60,wait,release,await:files-queue:200:215,wait,wait".to_owned(),
        "files-queue,wait,press,release,press,release,await:files-queue:147:149,wait,wait,wait,wait".to_owned(),
        // The menu of a row, and of the empty space under the rows.
        "files-point:R|down.txt,wait,press-right,wait,release-right,await:menu,await:menu-offers:Download,wait,wait,wait".to_owned(),
        shot("ux-files-row-menu-dark.ppm"),
        "theme:light,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-files-row-menu-light.ppm"),
        "theme:dark,wait,wait,wait,escape,await:no-menu,wait".to_owned(),
        // Dragged across with the pointer, as a person would.
        "files-point:R|down.txt,wait,press,wait".to_owned(),
        "files-column:L,wait,wait,release".to_owned(),
        "await:transfer:down.txt|done".to_owned(),
        "await:listed:L|down.txt".to_owned(),
        // Copied with the button.
        "files-select:L|up.txt,wait,files-send:L".to_owned(),
        "await:transfer:up.txt|done".to_owned(),
        "await:listed:R|up.txt".to_owned(),
        // Cancelled partway, then retried to the end.
        "files-select:R|big.bin,wait,files-send:R".to_owned(),
        "await:transfer:big.bin|running".to_owned(),
        "files-cancel:big.bin".to_owned(),
        "await:transfer:big.bin|cancelled".to_owned(),
        "wait,wait".to_owned(),
        shot("cancelled.ppm"),
        "files-retry:big.bin".to_owned(),
        "await:transfer:big.bin|running".to_owned(),
        shot("copying.ppm"),
        "await:transfer:big.bin|done".to_owned(),
        "await:listed:L|big.bin".to_owned(),
        "wait,wait".to_owned(),
        shot("done.ppm"),
        // The queue folds away and back.
        "files-fold,wait,press,release,await:files-folded:1,wait,wait".to_owned(),
        shot("ux-files-queue-folded.ppm"),
        "files-fold,wait,press,release,await:files-folded:0,wait,wait".to_owned(),
        // What to do about a file in the way is a menu now.
        "move:300:666,wait,press,release,await:menu,await:menu-offers:Overwrite,wait,wait".to_owned(),
        shot("ux-files-policy-menu.ppm"),
        "escape,await:no-menu,wait".to_owned(),
        // A file opened in the pane, written back, and compared with its local copy.
        "files-open:R|down.txt,await:files-text:from-the-server,wait,wait,wait".to_owned(),
        shot("ux-files-text.ppm"),
        "files-close,await:files-columns,wait".to_owned(),
        // Source text is colored by what it is.
        "files-open:L|sample.py,await:files-text:Greeter,wait,wait,wait,wait,wait".to_owned(),
        shot("ux-files-code-dark.ppm"),
        "theme:light,wait,wait,wait,wait".to_owned(),
        shot("ux-files-code-light.ppm"),
        "theme:dark,wait,wait,files-close,await:files-columns,wait".to_owned(),
        "files-open:R|down.txt,await:files-text:from-the-server,wait,wait,wait".to_owned(),
        // Typed into the field: its button shows the edits, and the other one saves them.
        "text:second-line,enter,wait,wait,wait".to_owned(),
        shot("ux-files-text-edited.ppm"),
        format!("{TEXT_DIFF},wait,press,release,await:files-diff:2,wait,wait,wait"),
        shot("ux-files-edits-diff.ppm"),
        "files-close,await:no-diff,wait,wait".to_owned(),
        format!("{TEXT_SAVE},wait,press,release,await:files-text:second-line,wait,wait"),
        "files-close,await:files-columns,wait,wait".to_owned(),
        // Leaving with unsaved edits asks first.
        "files-open:R|down.txt,await:files-text:second-line,wait,wait,wait".to_owned(),
        "text:lost,wait,wait,files-close,await:prompt,wait,wait".to_owned(),
        shot("ux-files-discard.ppm"),
        "confirm,await:files-columns,wait,wait".to_owned(),
        "files-compare:down.txt|down.txt,await:files-diff:2,wait,wait,wait".to_owned(),
        shot("ux-files-diff.ppm"),
        "files-close,await:files-columns,wait".to_owned(),
    ]);
    let report = app_run(bench_target, scratch, device, &script.join(","))?;
    report_paths(name, &report);
    Ok(())
}

/// Going from one vault to another and back: each shows its own connections and wants its own
/// password.
pub fn vaults(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let scratch = Scratch::new();
    let second = scratch.path("second.rmxdb").display().to_string();
    let shot = |name: &str| shot_step(shots, name);
    let first = scratch.path("switch/vault.rmxdb").display().to_string();
    let script = [
        "await:setup".to_owned(),
        "vault-create:first-password".to_owned(),
        "await:unlocked".to_owned(),
        "await:vault-named:vault.rmxdb".to_owned(),
        "conn-as:SFTP|in-first|127.0.0.1|1|alice|pw||".to_owned(),
        "await:tree:in-first".to_owned(),
        "wait,wait,wait".to_owned(),
        shot("ux-vault-switcher.ppm"),
        format!("vault-switch:{second}"),
        "await:setup,await:vault-named:second.rmxdb,wait,wait,wait".to_owned(),
        shot("ux-vault-setup-other.ppm"),
        "vault-create:second-password".to_owned(),
        "await:unlocked".to_owned(),
        "await:no-tree:in-first".to_owned(),
        format!("vault-switch:{first}"),
        "await:locked,await:vault-named:vault.rmxdb,wait,wait".to_owned(),
        shot("ux-vault-unlock-other.ppm"),
        "vault-unlock:first-password".to_owned(),
        "await:unlocked".to_owned(),
        "await:tree:in-first".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "switch", &script)?;
    report_paths("vaults", &report);
    println!("ok   vaults: switched to another vault and back");
    Ok(())
}

/// SFTP and FTP browsing, and copies both ways with cancel and retry, against real servers.
pub fn files(bench_target: &Path, shots: &Path) -> Result<(), String> {
    // SFTP, on its own connection; and from a shell, opening at its working directory.
    let server = Sshd::start()?;
    let exec = |command: &str| podman(&["exec", &server.name, "sh", "-c", command]);
    seed_remote(exec, "/home/alice")?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let (port, password) = (server.port, server.password.clone());
    let opening = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn-as:SFTP|files|127.0.0.1|{port}|alice|{password}||"),
        "await:host-key".to_owned(),
        "hostkey:always".to_owned(),
        "await:live".to_owned(),
    ];
    browse_and_copy(bench_target, &scratch, "sftp", shots, "sftp", &opening, "/home/alice")?;
    let path = scratch.path("home/ftp-side/down.txt");
    let got = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    if got != "from-the-server" {
        return Err(format!("down.txt came through wrong: {got:?}"));
    }
    let edited = podman(&["exec", &server.name, "cat", "/home/alice/down.txt"])?;
    if edited != "second-line\nfrom-the-server" {
        return Err(format!("the edited file came through wrong: {edited:?}"));
    }
    exec("printf 'from-the-server' > /home/alice/down.txt")?;
    let up = podman(&["exec", &server.name, "cat", "/home/alice/up.txt"])?;
    if up != "from-the-client" {
        return Err(format!("the upload came through wrong: {up:?}"));
    }
    let big = fs::metadata(scratch.path("home/ftp-side/big.bin")).map_err(|err| err.to_string())?;
    if big.len() != 2_147_483_648 {
        return Err(format!("the retried copy has {} bytes", big.len()));
    }
    println!("ok   sftp: browsed, copied both ways, cancelled and retried");

    // "Open files here": a new connection, opened where the shell said it is.
    exec_shell(
        &server,
        "mkdir -p /home/alice/work/inner && touch /home/alice/work/inner/here.txt",
    )?;
    let scratch = Scratch::new();
    let shot_here = shot_step(shots, "sftp-open-here.ppm");
    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn:shell|127.0.0.1|{port}|alice|{password}"),
        "await:host-key".to_owned(),
        "hostkey:always".to_owned(),
        "await:live".to_owned(),
        "await:text:$ ".to_owned(),
        "run:cd work/inner".to_owned(),
        r"run:printf '\033]7;file://%s%s\033\\' host $PWD".to_owned(),
        "wait,wait,wait,wait".to_owned(),
        "open-files".to_owned(),
        "await:live".to_owned(),
        "await:at-path:R|/home/alice/work/inner".to_owned(),
        "await:listed:R|here.txt".to_owned(),
        "wait,wait".to_owned(),
        shot_here,
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "open-here", &script)?;
    report_paths("sftp open here", &report);
    println!("ok   sftp: opened at the shell's working directory");

    // With the keyboard: into a directory, a copy across, and back up.
    let scratch = Scratch::new();
    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn-as:SFTP|keys|127.0.0.1|{port}|alice|{password}||"),
        "await:host-key,hostkey:always,await:live".to_owned(),
        "files-goto:R|/home/alice/work,await:listed:R|inner".to_owned(),
        "files-key:tab,files-key:down,wait".to_owned(),
        "files-key:enter".to_owned(),
        "await:at-path:R|/home/alice/work/inner,await:listed:R|here.txt".to_owned(),
        "files-key:down,wait,wait".to_owned(),
        shot_step(shots, "m8-files-keyboard.ppm"),
        "files-key:copy".to_owned(),
        "await:transfer:here.txt|done,await:listed:L|here.txt".to_owned(),
        "files-key:back".to_owned(),
        "await:at-path:R|/home/alice/work".to_owned(),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "keys", &script)?;
    report_paths("files with the keyboard", &report);
    println!("ok   files: arrows, Enter, Backspace, Tab and the copy key");
    drop(server);

    // FTP, and then FTPS against the certificate pinned the first time.
    let password = random_hex();
    let pasv = free_port_range(5);
    let server = ftp_server(&password, pasv)?;
    seed_remote(|command| podman(&["exec", &server.name, "sh", "-c", command]), "/home/alice")?;
    let port = server.port(21)?;
    let scratch = Scratch::new();
    let opening = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn-as:FTP|ftp-files|127.0.0.1|{port}|alice|{password}||"),
        "await:live".to_owned(),
        // Not the root: where the server said the login is.
        "await:at-path:R|/home/alice".to_owned(),
        "await:listed:R|down.txt".to_owned(),
    ];
    browse_and_copy(bench_target, &scratch, "ftp", shots, "ftp", &opening, "/home/alice")?;
    let up = podman(&["exec", &server.name, "cat", "/home/alice/up.txt"])?;
    if up != "from-the-client" {
        return Err(format!("the FTP upload came through wrong: {up:?}"));
    }
    let edited = podman(&["exec", &server.name, "cat", "/home/alice/down.txt"])?;
    if edited != "second-line\nfrom-the-server" {
        return Err(format!("the file edited over FTP came through wrong: {edited:?}"));
    }
    seed_remote(|command| podman(&["exec", &server.name, "sh", "-c", command]), "/home/alice")?;
    println!("ok   ftp: browsed, copied both ways, cancelled and retried");

    let scratch = Scratch::new();
    let script = format!(
        "await:setup,vault-create:{vault_password},await:unlocked,\
         conn-as:FTP|ftps-files|127.0.0.1|{port}|alice|{password}||ftps,\
         await:host-key,hostkey:always,await:live,files-goto:R|/home/alice,\
         await:listed:R|down.txt,wait,{shot},disconnect,await:closed,reconnect,await:live,\
         files-goto:R|/home/alice,await:listed:R|down.txt,wait",
        shot = shot_step(shots, "ftps-listing.ppm"),
    );
    let report = app_run(bench_target, &scratch, "ftps", &script)?;
    report_paths("ftps", &report);
    println!("ok   ftps: the pinned certificate needs no second word");

    // FTP through a tunnel to the server's own loopback, its data connections included.
    let gate = Sshd::beside(&server)?;
    let scratch = Scratch::new();
    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn:gate|127.0.0.1|{}|alice|{}", gate.port, gate.password),
        "await:host-key,hostkey:always,await:live".to_owned(),
        format!("conn-as:FTP|tunnelled|127.0.0.1|21|alice|{password}|gate|"),
        "await:live".to_owned(),
        "await:at-path:R|/home/alice".to_owned(),
        "await:listed:R|down.txt".to_owned(),
        "files-select:R|down.txt,wait,files-send:R".to_owned(),
        "await:transfer:down.txt|done".to_owned(),
        "await:listed:L|down.txt".to_owned(),
        "wait,wait".to_owned(),
        shot_step(shots, "m8-ftp-tunnelled.ppm"),
    ]
    .join(",");
    let report = app_run(bench_target, &scratch, "ftp-tunnel", &script)?;
    report_paths("ftp through ssh", &report);
    let got = scratch.path("home/down.txt");
    let got = fs::read_to_string(&got).map_err(|err| format!("{}: {err}", got.display()))?;
    if got != "from-the-server" {
        return Err(format!("down.txt came through the tunnel wrong: {got:?}"));
    }
    println!("ok   ftp: through an ssh tunnel, listing and data connections");
    Ok(())
}

fn exec_shell(server: &Sshd, command: &str) -> Result<String, String> {
    podman(&["exec", &server.name, "sh", "-c", command])
}

/// A TigerVNC desktop, an xterm the typing lands in, and an sshd to tunnel through.
fn vnc_server(
    password: &str,
    geometry: &str,
    desktop: &str,
    ssh_password: &str,
) -> Result<Server, String> {
    ensure_image(
        VNC_IMAGE,
        "FROM docker.io/library/alpine:3.22\nRUN apk add --no-cache --quiet tigervnc xterm xsetroot xclip \
         openssh font-misc-misc mesa-dri-gallium\nRUN ssh-keygen -A\n",
    )?;
    let script = format!(
        r#"set -eu
printf '%s' "$RMX_VNC_PW" | vncpasswd -f > /tmp/passwd
chmod 600 /tmp/passwd
adduser -D -s /bin/sh alice
printf 'alice:%s\n' "$RMX_SSH_PW" | chpasswd
unset RMX_VNC_PW RMX_SSH_PW
/usr/sbin/sshd -o AllowTcpForwarding=yes -o PasswordAuthentication=yes
Xvnc :1 -rfbport 5901 -SecurityTypes VncAuth -PasswordFile /tmp/passwd -geometry {geometry} -depth 24 -AlwaysShared >/tmp/xvnc.log 2>&1 &
export DISPLAY=:1
until xsetroot -solid '#336699' 2>/dev/null; do sleep 0.1; done
{desktop}
touch /tmp/ready
wait
"#
    );
    let publish = ["127.0.0.1::5901".to_owned(), "127.0.0.1::22".to_owned()];
    let server = Server::start(
        VNC_IMAGE,
        &publish,
        &[("RMX_VNC_PW", password), ("RMX_SSH_PW", ssh_password)],
        &script,
    )?;
    server.wait("test -f /tmp/ready", Duration::from_secs(120))?;
    Ok(server)
}

/// What the desktop runs: the terminal that receives what is typed, and the clipboard answer.
const VNC_DESKTOP: &str = r#"
xterm -geometry 200x70+0+0 -bg '#102030' -fg '#e0e0e0' -e sh -c 'stty echo; tee /tmp/typed.txt' &
(until grep -q hello /tmp/typed.txt 2>/dev/null; do sleep 0.1; done; printf 'from-vnc-server' | xclip -selection clipboard -i) &
"#;

/// A session through a real TigerVNC server: pixels, typing, resize, clipboard both ways.
pub fn vnc(bench_target: &Path, shots: &Path) -> Result<(), String> {
    let password = random_hex()[..8].to_owned();
    let ssh_password = random_hex();
    let server = vnc_server(&password, "1280x800", VNC_DESKTOP, &ssh_password)?;
    let port = server.port(5901)?;
    let ssh_port = server.port(22)?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let shot = |file: &str| shot_step(shots, file);
    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        // A tunnel through SSH to the server's own loopback, which is otherwise unreachable.
        format!("conn:gate|127.0.0.1|{ssh_port}|alice|{ssh_password}"),
        "await:host-key,hostkey:always,await:live".to_owned(),
        format!("conn-as:VNC|tunnelled|127.0.0.1|5901||{password}|gate|"),
        "await:live,await:display-fits,await:display".to_owned(),
        "wait,wait".to_owned(),
        shot("vnc-tunnelled.ppm"),
        "disconnect,await:closed".to_owned(),
        format!("conn-as:VNC|desktop|127.0.0.1|{port}||{password}||"),
        "await:live,await:display-fits,await:display".to_owned(),
        "center,wait,wait".to_owned(),
        "keys:hello-123~".to_owned(),
        "await:clipboard:from-vnc-server".to_owned(),
        "wait,wait".to_owned(),
        shot("vnc-desktop.ppm"),
        pane_chrome(4, "Send Ctrl+Alt+Del", "vnc", shots),
        // Inside a desktop, the right button is the desktop's; with Shift it is ours.
        "pane-at:0:0.5:0.5,wait,press-right,wait,release-right,wait,await:no-menu".to_owned(),
        "shift-down,wait,press-right,wait,release-right,await:menu,shift-up,await:menu-offers:Toggle scale,escape,await:no-menu,wait".to_owned(),
        format!("clip-set:from-rmx-client,clip-offer,{}", "wait,".repeat(30).trim_end_matches(',')),
        // Past ASCII, nothing crosses and the user is told.
        "clip-set:中文内容,clip-offer".to_owned(),
        "await:notice:ASCII".to_owned(),
        "wait,wait".to_owned(),
        shot("vnc-cjk-withheld.ppm"),
        "resize:1000x640,wait,wait,wait,wait,wait,wait,wait".to_owned(),
        "await:display-fits,await:display".to_owned(),
        "wait,wait,wait".to_owned(),
        shot("vnc-resized.ppm"),
        "scale-toggle,wait,wait".to_owned(),
        shot("vnc-native.ppm"),
        // A desktop keeps its picture and its session in another window, and coming back.
        "scale-toggle,wait".to_owned(),
        tear_tab(3),
        "await:windows:2,win:1,await:display,wait,wait,wait,wait".to_owned(),
        shot("m8-vnc-torn-off.ppm"),
        "center,wait,keys:torn-off-456~,wait,wait,wait,wait".to_owned(),
        merge_tab(0),
        "await:windows:1,await:display,wait,wait,wait".to_owned(),
        shot("m8-vnc-merged.ppm"),
    ]
    .join(",");
    let clipboard = watch_clipboard(server.name.clone(), None, "from-rmx-client");
    let more = [("RMX_BENCH_CLAIM_MS", CLAIM_MS)];
    let report = app_run_with(bench_target, &scratch, "device", &script, &more)?;
    report_paths("vnc", &report);
    if report.value("connect_runs") != 3 {
        return Err(format!(
            "moving between windows reconnected: {} connects",
            report.value("connect_runs")
        ));
    }

    let typed = server.exec("cat /tmp/typed.txt")?;
    if !typed.contains("torn-off-456") {
        return Err(format!("typing in the torn-off window did not arrive: {typed:?}"));
    }
    if !typed.contains("hello-123") {
        return Err(format!("the typing did not arrive: {typed:?}"));
    }
    if !clipboard.load(Ordering::SeqCst) {
        return Err("the server never saw the client's clipboard".to_owned());
    }
    println!("ok   vnc: typing on the server, clipboard both ways, and CJK held back");
    Ok(())
}

/// A session through a real xrdp server: pixels, typing, resize, clipboard both ways, and a
/// certificate pinned on the first connection.
pub fn rdp(bench_target: &Path, shots: &Path) -> Result<(), String> {
    ensure_image_from(RDP_IMAGE, Path::new("crates/rmx-rdp/tests/xrdp"))?;
    let scratch = Scratch::new();
    let suffix = random_hex();
    let user = format!("u{}", &suffix[..8]);
    let password = format!("p{}", random_hex());
    let session = scratch.path("session.sh");
    let script = r#"#!/bin/sh
xsetroot -solid '#225577'
(until grep -q hello "$HOME/typed.txt" 2>/dev/null; do sleep 0.1; done; printf 'from-rdp-server' | xclip -selection clipboard -i) &
exec xterm -geometry 160x48+0+0 -bg '#102030' -fg '#e0e0e0' -e sh -c 'stty echo; tee -a "$HOME/typed.txt"'
"#;
    fs::write(&session, script).map_err(|err| err.to_string())?;
    // sesman runs it directly, and falls back to its own session if it cannot.
    fs::set_permissions(&session, fs::Permissions::from_mode(0o755))
        .map_err(|err| err.to_string())?;

    let name = format!("rmx-e2e-{}", random_hex());
    let mut run = Command::new("podman");
    run.args([
        "run",
        "-d",
        "--rm",
        "--name",
        &name,
        "-p",
        "127.0.0.1::3389",
        "-p",
        "127.0.0.1::22",
    ])
    .arg("-v")
    .arg(format!("{}:/etc/xrdp/startwm.sh:ro", session.display()))
    .args(["--env", "RDP_USER", "--env", "RDP_PASS"])
    .env("RDP_USER", &user)
    .env("RDP_PASS", &password)
    .arg(RDP_IMAGE);
    let output = run.output().map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(format!("podman run: {}", String::from_utf8_lossy(&output.stderr)));
    }
    let server = Server { name };
    let port = server.port(3389)?;
    server.wait("grep -q ':0D3D ' /proc/net/tcp /proc/net/tcp6", Duration::from_secs(60))?;
    let gate = Sshd::beside(&server)?;
    let exec_user = |command: &str| {
        podman(&[
            "exec",
            "-u",
            &user,
            "--env",
            "DISPLAY=:10",
            "--env",
            &format!("XAUTHORITY=/home/{user}/.Xauthority"),
            &server.name,
            "sh",
            "-c",
            command,
        ])
    };

    let vault_password = random_hex();
    let shot = |file: &str| shot_step(shots, file);
    let script = [
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn-as:RDP|windows|127.0.0.1|{port}|{user}|{password}||tls"),
        "await:host-key".to_owned(),
        "wait,wait".to_owned(),
        shot("rdp-certificate.ppm"),
        "hostkey:always".to_owned(),
        "await:live,await:display-fits,await:display".to_owned(),
        "wait,wait,wait,wait,wait".to_owned(),
        // A second connection needs no second word about the certificate.
        "disconnect,await:closed,wait".to_owned(),
        "reconnect,await:live,await:display-fits,await:display".to_owned(),
        "wait,wait,wait,wait,wait,wait,center,wait,wait".to_owned(),
        "keys:hello-456~".to_owned(),
        "wait,wait,wait,wait".to_owned(),
        shot("rdp-desktop.ppm"),
        pane_chrome(2, "Toggle scale", "rdp", shots),
        "await:clipboard:from-rdp-server".to_owned(),
        format!("clip-set:from-rmx-中文,clip-offer,{}", "wait,".repeat(30).trim_end_matches(',')),
        "resize:1000x640,wait,wait,wait,wait,wait,wait,wait,wait".to_owned(),
        "await:display-fits,await:display".to_owned(),
        "wait,wait,wait,wait".to_owned(),
        shot("rdp-resized.ppm"),
        "salute,wait".to_owned(),
        // Through a tunnel to the server's own loopback, where it answers to another address and
        // so with a certificate to be trusted again.
        "disconnect,await:closed,wait".to_owned(),
        format!("conn:gate|127.0.0.1|{}|alice|{}", gate.port, gate.password),
        "await:host-key,hostkey:always,await:live".to_owned(),
        format!("conn-as:RDP|tunnelled|127.0.0.1|3389|{user}|{password}|gate|tls"),
        "await:host-key,hostkey:always".to_owned(),
        "await:live,await:display-fits,await:display".to_owned(),
        "wait,wait,wait,wait,wait,wait,center,wait,wait".to_owned(),
        "keys:tunnel-789~".to_owned(),
        "wait,wait,wait,wait".to_owned(),
        shot("m8-rdp-tunnelled.ppm"),
    ]
    .join(",");
    let xauthority = format!("XAUTHORITY=/home/{user}/.Xauthority");
    let clipboard =
        watch_clipboard(server.name.clone(), Some((user.clone(), xauthority)), "from-rmx-中文");
    let report = app_run(bench_target, &scratch, "device", &script)?;
    report_paths("rdp", &report);

    let typed = exec_user("cat \"$HOME/typed.txt\"")?;
    if !typed.contains("hello-456") {
        return Err(format!("the typing did not arrive: {typed:?}"));
    }
    if !clipboard.load(Ordering::SeqCst) {
        return Err("the server never saw the client's clipboard".to_owned());
    }
    if !typed.contains("tunnel-789") {
        return Err(format!("the typing through the tunnel did not arrive: {typed:?}"));
    }
    println!(
        "ok   rdp: typing on the server, clipboard both ways, resize, pinned certificate, ssh tunnel"
    );
    Ok(())
}

/// What repaints the whole desktop, again and again: a full-screen update of 4K pixels each time.
const FULL_SCREEN_DESKTOP: &str = r#"
(while :; do xsetroot -solid '#aa4411'; xsetroot -solid '#1144aa'; done) &
"#;

/// Output that keeps a terminal busy, written by the shell at the far end.
const BUSY_OUTPUT: &str =
    "yes $(printf '\\033[3%dm%08d\\033[0m drwxr-xr-x root 远程连接 /usr/lib/x86_64/lib.so ' 2 7)";

/// The plan's standard scene: three SSH terminals in constant output and a VNC desktop updating
/// the whole of a 4K screen, with split and zoom animations cycling over them.
pub fn standard_scene(bench_target: &Path, cycles: usize) -> Result<Report, String> {
    let ssh = Sshd::start()?;
    let vnc_password = random_hex()[..8].to_owned();
    let ssh_password = random_hex();
    let vnc = vnc_server(&vnc_password, "3840x2160", FULL_SCREEN_DESKTOP, &ssh_password)?;
    let vnc_port = vnc.port(5901)?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let (port, password) = (ssh.port, ssh.password.clone());

    let busy = format!("run:{BUSY_OUTPUT}");
    let cycle =
        "zoom,zoom,split-right-local,close,move:120:150,move:600:20,palette,wait,palette,wait";
    let mut script = vec![
        "await:setup".to_owned(),
        format!("vault-create:{vault_password}"),
        "await:unlocked".to_owned(),
        format!("conn:busy|127.0.0.1|{port}|alice|{password}"),
        "await:host-key,hostkey:always,await:live,await:text:$ ".to_owned(),
        busy.clone(),
        "open-split:right:busy,await:live,await:text:$ ".to_owned(),
        busy.clone(),
        "open-split:down:busy,await:live,await:text:$ ".to_owned(),
        busy,
        // Saving connects in a tab of its own, so that one goes and comes back beside the others.
        format!("conn-as:VNC|screen|127.0.0.1|{vnc_port}||{vnc_password}||keep"),
        "await:live,await:display,close,await:asking:screen,confirm,await:panes:4,left".to_owned(),
        "open-split:down:screen,await:live,await:display".to_owned(),
        "wait,wait,wait,wait".to_owned(),
    ];
    script.extend(std::iter::repeat_n(cycle.to_owned(), cycles));

    let env = [("RMX_BENCH_STEP_MS", "300"), ("RMX_BENCH_WARMUP", "240")];
    let joined = format!("{},quit", script.join(","));
    let env: Vec<(&str, &str)> =
        env.iter().copied().chain([("RMX_BENCH_SCRIPT", joined.as_str())]).collect();
    let home = scratch.path("home");
    let data = scratch.path("device");
    let env: Vec<(&str, &str)> = env
        .into_iter()
        .chain([("HOME", home.to_str().unwrap()), ("RMX_DATA_DIR", data.to_str().unwrap())])
        .collect();
    let report = measure(bench_target, Some((3840, 2160)), &env, LOCAL_SHELL)
        .ok_or_else(|| "the application did not exit cleanly".to_owned())?;
    if report.value("script_ok") != 1 {
        let failed = report.text.lines().find_map(|line| line.strip_prefix("script_failed="));
        return Err(format!("the script gave up waiting for {}", failed.unwrap_or("?")));
    }
    Ok(report)
}

/// One idle VNC pane against a real server, for the bench.
pub fn idle_remote_pane(bench_target: &Path) -> Result<Report, String> {
    let vnc_password = random_hex()[..8].to_owned();
    let ssh_password = random_hex();
    let server = vnc_server(&vnc_password, "1280x800", "", &ssh_password)?;
    let port = server.port(5901)?;
    let scratch = Scratch::new();
    let vault_password = random_hex();
    let script = format!(
        "await:setup,vault-create:{vault_password},await:unlocked,\
         conn-as:VNC|idle|127.0.0.1|{port}||{vnc_password}||,await:live,await:display,\
         wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,wait,idle:3000,wait"
    );
    app_run(bench_target, &scratch, "device", &script)
}
