//! Serial ports as terminal transports.
//!
//! Reading and writing go straight to the device and wait for it, so the kernel's buffers and
//! the configured flow control are the only queues. [`open`] and [`list_ports`] make their
//! blocking system calls on the runtime's blocking pool.
//!
//! Only Linux has a backend for now. Others go in [`sys`] behind the same four items, and until
//! then [`open`] fails with [`SerialError::Unsupported`] and [`list_ports`] returns nothing.

#[cfg(target_os = "linux")]
#[path = "serial/linux.rs"]
mod sys;
#[cfg(not(target_os = "linux"))]
#[path = "serial/unsupported.rs"]
mod sys;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, io};

use bytes::{Buf, Bytes};
use rmx_terminal::{CloseReason, TransportReader, TransportWriter, WindowSize};
use tokio::sync::Mutex;

const READ_SIZE: usize = 4096;

/// Bits in a character.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    #[default]
    Eight,
}

/// Parity bit. Characters received with the wrong parity are delivered as they are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

/// Stop bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StopBits {
    #[default]
    One,
    Two,
}

/// How the two ends pause each other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FlowControl {
    #[default]
    None,
    /// XON/XOFF in the data stream.
    Software,
    /// The RTS and CTS lines.
    Hardware,
}

/// Line settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SerialConfig {
    /// Bits per second. Any rate the device takes, not only the standard ones.
    pub baud_rate: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

impl Default for SerialConfig {
    /// 115200 8N1 with no flow control.
    fn default() -> Self {
        Self {
            baud_rate: 115_200,
            data_bits: DataBits::default(),
            parity: Parity::default(),
            stop_bits: StopBits::default(),
            flow_control: FlowControl::default(),
        }
    }
}

/// Why a port could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SerialError {
    /// There is no such device.
    NotFound,
    /// The device exists but this user may not open it, usually for want of the `dialout` or
    /// `uucp` group.
    PermissionDenied,
    /// Another program has it open.
    Busy,
    /// The path is not a terminal device.
    NotASerialPort,
    /// The device did not accept these settings, for this reason.
    InvalidConfig(String),
    /// This platform has no serial backend yet.
    Unsupported,
    /// Anything else the system reported.
    Io(String),
}

impl fmt::Display for SerialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such serial port"),
            Self::PermissionDenied => f.write_str("no permission to open the serial port"),
            Self::Busy => f.write_str("the serial port is in use"),
            Self::NotASerialPort => f.write_str("not a serial port"),
            Self::InvalidConfig(reason) => write!(f, "unusable serial settings: {reason}"),
            Self::Unsupported => f.write_str("serial ports are not supported on this platform"),
            Self::Io(reason) => write!(f, "serial port error: {reason}"),
        }
    }
}

impl std::error::Error for SerialError {}

/// What is known about the USB adapter behind a port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
}

/// A port found by [`list_ports`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    /// What to give [`open`], for example `/dev/ttyUSB0`.
    pub path: PathBuf,
    /// The kernel driver's name, if known.
    pub driver: Option<String>,
    /// Set for USB adapters.
    pub usb: Option<UsbInfo>,
}

/// The ports on this machine, sorted by path. Empty where there is no backend.
pub async fn list_ports() -> Vec<PortInfo> {
    blocking(sys::list_ports).await.unwrap_or_default()
}

/// Opens `path` for exclusive use and returns the pair for
/// [`Terminal::connect`](rmx_terminal::Terminal::connect).
///
/// Must be called within a Tokio runtime with I/O enabled.
pub async fn open(
    path: impl AsRef<Path>,
    config: SerialConfig,
) -> Result<(SerialReader, SerialWriter), SerialError> {
    let path = path.as_ref().to_path_buf();
    let device = blocking(move || sys::Device::open(&path, config))
        .await
        .map_err(|error| SerialError::Io(error.to_string()))??;
    let device = Arc::new(device.register().map_err(|error| SerialError::Io(error.to_string()))?);
    let reader =
        SerialReader { device: Arc::clone(&device), buffer: vec![0; READ_SIZE].into_boxed_slice() };
    Ok((reader, SerialWriter { device, write: Mutex::new(()) }))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(work).await
}

fn lost(error: io::Error) -> CloseReason {
    CloseReason::Disconnected(error.to_string())
}

/// The output half of an open port.
pub struct SerialReader {
    device: Arc<sys::Registered>,
    buffer: Box<[u8]>,
}

impl TransportReader for SerialReader {
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        match self.device.read(&mut self.buffer).await {
            Ok(0) => Err(CloseReason::Disconnected(String::from("the serial port went away"))),
            Ok(read) => Ok(Bytes::copy_from_slice(&self.buffer[..read])),
            Err(error) => Err(lost(error)),
        }
    }
}

/// The input half of an open port. Resizing has no meaning on a serial line and does nothing.
pub struct SerialWriter {
    device: Arc<sys::Registered>,
    write: Mutex<()>,
}

impl TransportWriter for SerialWriter {
    async fn write(&self, mut bytes: Bytes) -> Result<(), CloseReason> {
        let _order = self.write.lock().await;
        while bytes.has_remaining() {
            let written = self.device.write(&bytes).await.map_err(lost)?;
            bytes.advance(written);
        }
        Ok(())
    }

    async fn resize(&self, _size: WindowSize) -> Result<(), CloseReason> {
        Ok(())
    }
}
