//! Helpers shared by the container integration tests.
#![allow(dead_code)]

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Returns the container runtime to use, preferring rootless podman.
pub fn runtime() -> &'static str {
    let works = |name: &str| {
        Command::new(name).arg("--version").output().is_ok_and(|output| output.status.success())
    };
    if works("podman") {
        "podman"
    } else if works("docker") {
        "docker"
    } else {
        panic!("neither podman nor docker is available");
    }
}

/// A short name unique to this process and moment.
pub fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}-{}-{}", std::process::id(), nanos & 0xffff_ffff)
}

/// Ephemeral password: 128 random bits as hex, read from the kernel.
pub fn random_secret() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut bytes).unwrap();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Finds a free loopback port with the next `extra` ports also free.
pub fn free_port_range(extra: u16) -> u16 {
    for _ in 0..200 {
        let port =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        if port > 60000 {
            continue;
        }
        let all_free =
            (0..=extra).all(|i| TcpListener::bind((Ipv4Addr::LOCALHOST, port + i)).is_ok());
        if all_free {
            return port;
        }
    }
    panic!("no free port range");
}

/// Waits until a loopback TCP port accepts connections.
pub fn wait_for_port(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err() {
        assert!(Instant::now() < deadline, "port {port} did not open in time");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A running container, removed on drop.
pub struct Container {
    pub name: String,
    runtime: &'static str,
}

impl Container {
    /// Starts an Alpine container on the host network, so that the service inside can bind to
    /// 127.0.0.1 and nothing else, and runs `script` as its main process.
    ///
    /// `bind_dir` is mounted read-only at `/ephemeral`.
    pub fn start(prefix: &str, bind_dir: &Path, script: &str) -> Self {
        let runtime = runtime();
        let name = unique(prefix);
        let status = Command::new(runtime)
            .args(["run", "-d", "--rm", "--name", &name, "--network=host"])
            .args(["--tmpfs", "/srv/data:rw,size=4g,mode=0777"])
            .arg("-v")
            .arg(format!("{}:/ephemeral:ro", bind_dir.display()))
            .args(["docker.io/library/alpine:3.22", "sh", "-c", script])
            .output()
            .expect("start container");
        assert!(
            status.status.success(),
            "container start failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        Self { name, runtime }
    }

    /// Runs a shell command inside the container and returns its stdout.
    pub fn exec(&self, command: &str) -> String {
        let output = Command::new(self.runtime)
            .args(["exec", &self.name, "sh", "-c", command])
            .output()
            .expect("exec in container");
        assert!(
            output.status.success(),
            "`{command}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    pub fn logs(&self) -> String {
        let output = Command::new(self.runtime)
            .args(["logs", &self.name])
            .output()
            .expect("read container logs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime).args(["rm", "-f", &self.name]).output();
    }
}

/// Removes the listed files when the test ends, even on panic.
pub struct RemoveOnDrop(pub Vec<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(2654435761).to_le_bytes()[3]).collect()
}
