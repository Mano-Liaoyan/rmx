//! Serial ports against pseudo-terminals, which take the same `termios` calls as real ones.

#![cfg(target_os = "linux")]

use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rmx_serial::serial::{
    DataBits, FlowControl, Parity, SerialConfig, SerialError, StopBits, list_ports, open,
};
use rmx_terminal::{CloseReason, TransportReader, TransportWriter, WindowSize};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// The master side of a pseudo-terminal, and the path of its slave.
fn pty() -> (File, PathBuf, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: the out parameters are valid; the rest may be null.
    let status = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(status, 0, "openpty failed");
    // SAFETY: `openpty` just returned these descriptors and nothing else owns them.
    let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: `name` is valid for writes of its length.
    let status = unsafe { libc::ttyname_r(slave.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    assert_eq!(status, 0, "ttyname_r failed");
    // SAFETY: the call succeeded, so `name` is a C string.
    let path = unsafe { CStr::from_ptr(name.as_ptr()) }.to_str().unwrap().into();
    (master, path, slave)
}

fn attributes(fd: &OwnedFd) -> libc::termios2 {
    let mut attributes = MaybeUninit::<libc::termios2>::uninit();
    // SAFETY: the kernel fills the struct, of the size this request expects, on success.
    let status =
        unsafe { libc::ioctl(fd.as_raw_fd(), libc::TCGETS2 as _, attributes.as_mut_ptr()) };
    assert_eq!(status, 0, "TCGETS2 failed");
    // SAFETY: the call succeeded.
    unsafe { attributes.assume_init() }
}

#[tokio::test]
async fn line_settings_reach_the_device() {
    let (_master, path, slave) = pty();
    let config = SerialConfig {
        baud_rate: 250_000,
        data_bits: DataBits::Eight,
        parity: Parity::None,
        stop_bits: StopBits::Two,
        flow_control: FlowControl::Hardware,
    };

    let _port = open(&path, config).await.unwrap();

    let applied = attributes(&slave);
    assert_eq!(applied.c_cflag & libc::CSIZE, libc::CS8);
    assert_eq!(applied.c_cflag & libc::PARENB, 0);
    assert_ne!(applied.c_cflag & libc::CSTOPB, 0);
    assert_ne!(applied.c_cflag & libc::CRTSCTS, 0);
    assert_ne!(applied.c_cflag & libc::CLOCAL, 0);
    assert_eq!((applied.c_ispeed, applied.c_ospeed), (250_000, 250_000));
    assert_eq!(applied.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
    assert_eq!(applied.c_oflag & libc::OPOST, 0);
    assert_eq!((applied.c_cc[libc::VMIN], applied.c_cc[libc::VTIME]), (1, 0));
}

#[tokio::test]
async fn a_format_the_device_changes_is_refused() {
    // A pty forces 8 bits and no parity, as a limited driver would without saying so.
    let (_master, path, _slave) = pty();
    let config = SerialConfig {
        data_bits: DataBits::Seven,
        parity: Parity::Even,
        ..SerialConfig::default()
    };

    assert!(matches!(open(&path, config).await.err(), Some(SerialError::InvalidConfig(_))));
}

#[tokio::test]
async fn software_flow_control_sets_only_that() {
    let (_master, path, slave) = pty();
    let config = SerialConfig { flow_control: FlowControl::Software, ..SerialConfig::default() };

    let _port = open(&path, config).await.unwrap();

    let applied = attributes(&slave);
    assert_eq!(applied.c_iflag, libc::IXON | libc::IXOFF);
    assert_eq!(applied.c_cflag & libc::CRTSCTS, 0);
    assert_eq!(applied.c_cflag & libc::CSIZE, libc::CS8);
}

#[tokio::test]
async fn bytes_travel_both_ways_unchanged() {
    let (mut master, path, _slave) = pty();
    let (mut reader, writer) = open(&path, SerialConfig::default()).await.unwrap();

    // CR and 0xFF would not survive line discipline or a text-mode layer.
    let sent: Vec<u8> = (0..=255).collect();
    writer.write(Bytes::from(sent.clone())).await.unwrap();
    let mut received = vec![0; sent.len()];
    master.read_exact(&mut received).unwrap();
    assert_eq!(received, sent);

    master.write_all(&sent).unwrap();
    let mut received = Vec::new();
    while received.len() < sent.len() {
        received.extend_from_slice(&timeout(WAIT, reader.read()).await.unwrap().unwrap());
    }
    assert_eq!(received, sent);
}

#[tokio::test]
async fn a_full_output_waits_for_the_other_end() {
    let (mut master, path, _slave) = pty();
    let (_reader, writer) = open(&path, SerialConfig::default()).await.unwrap();

    // More than the pty keeps, so this can only finish as the master reads.
    let sent = vec![b'x'; 1 << 20];
    let write = tokio::spawn({
        let sent = Bytes::from(sent.clone());
        async move { writer.write(sent).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!write.is_finished(), "the write did not wait");

    let len = sent.len();
    let read = tokio::task::spawn_blocking(move || {
        let mut received = vec![0; len];
        master.read_exact(&mut received).unwrap();
        received
    });
    assert_eq!(timeout(WAIT, read).await.unwrap().unwrap(), sent);
    timeout(WAIT, write).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn resizing_is_accepted_and_ignored() {
    let (_master, path, _slave) = pty();
    let (_reader, writer) = open(&path, SerialConfig::default()).await.unwrap();

    let size = WindowSize { num_cols: 10, num_lines: 5, cell_width: 1, cell_height: 1 };
    writer.resize(size).await.unwrap();
}

#[tokio::test]
async fn losing_the_device_ends_the_session() {
    let (master, path, slave) = pty();
    let (mut reader, _writer) = open(&path, SerialConfig::default()).await.unwrap();

    drop(master);
    drop(slave);

    let reason = timeout(WAIT, reader.read()).await.unwrap().unwrap_err();
    assert!(matches!(reason, CloseReason::Disconnected(_)), "{reason:?}");
}

#[tokio::test]
async fn a_port_in_use_is_refused() {
    let (_master, path, _slave) = pty();
    let _first = open(&path, SerialConfig::default()).await.unwrap();

    assert_eq!(open(&path, SerialConfig::default()).await.err(), Some(SerialError::Busy));
}

#[tokio::test]
async fn closing_makes_the_port_available_again() {
    let (_master, path, _slave) = pty();
    drop(open(&path, SerialConfig::default()).await.unwrap());

    // A process another test is spawning holds a copy of the descriptor, and so the lock, until
    // it execs: the closed port can stay locked for a moment.
    let deadline = Instant::now() + WAIT;
    loop {
        match open(&path, SerialConfig::default()).await {
            Ok(_) => return,
            Err(SerialError::Busy) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            },
            Err(error) => panic!("{error}"),
        }
    }
}

#[tokio::test]
async fn bad_paths_and_settings_are_reported() {
    assert_eq!(
        open("/dev/rmx-no-such-port", SerialConfig::default()).await.err(),
        Some(SerialError::NotFound)
    );
    assert_eq!(
        open("/dev/null", SerialConfig::default()).await.err(),
        Some(SerialError::NotASerialPort)
    );

    let (_master, path, _slave) = pty();
    let zero = SerialConfig { baud_rate: 0, ..SerialConfig::default() };
    assert!(matches!(open(&path, zero).await.err(), Some(SerialError::InvalidConfig(_))));
}

#[tokio::test]
async fn listing_does_not_fail_on_this_machine() {
    let ports = list_ports().await;
    assert!(ports.iter().all(|port| port.path.starts_with("/dev/")));
}

/// A pair of connected ports from `socat`, removed on drop.
struct Socat {
    child: Child,
    a: PathBuf,
    b: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Socat {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Socat {
    fn start() -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let endpoint = |path: &Path| format!("pty,raw,echo=0,link={}", path.display());
        let child = Command::new("socat")
            .args([endpoint(&a), endpoint(&b)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let socat = Self { child, a, b, _dir: dir };
        let deadline = Instant::now() + WAIT;
        while !(socat.a.exists() && socat.b.exists()) {
            assert!(Instant::now() < deadline, "socat did not create its ptys");
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(socat)
    }
}

#[tokio::test]
async fn two_ports_talk_through_socat() {
    let Some(socat) = Socat::start() else {
        eprintln!("skipping: socat is not installed");
        return;
    };
    let (mut reader_a, writer_a) = open(&socat.a, SerialConfig::default()).await.unwrap();
    let (mut reader_b, writer_b) = open(&socat.b, SerialConfig::default()).await.unwrap();

    let sent: Vec<u8> = (0..=255).cycle().take(64 * 1024).collect();
    let send = async { writer_a.write(Bytes::from(sent.clone())).await.unwrap() };
    let receive = async {
        let mut received = Vec::new();
        while received.len() < sent.len() {
            received.extend_from_slice(&reader_b.read().await.unwrap());
        }
        received
    };
    let ((), received) = timeout(WAIT, async { tokio::join!(send, receive) }).await.unwrap();
    assert_eq!(received, sent);

    writer_b.write(Bytes::from_static(b"back\r\n")).await.unwrap();
    let mut echoed = Vec::new();
    while echoed.len() < 6 {
        echoed.extend_from_slice(&timeout(WAIT, reader_a.read()).await.unwrap().unwrap());
    }
    assert_eq!(echoed, b"back\r\n");
}
