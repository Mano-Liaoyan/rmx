//! Serial ports through the kernel's `termios2`, which unlike `termios` takes any baud rate.

use std::ffi::CString;
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use super::{
    DataBits, FlowControl, Parity, PortInfo, SerialConfig, SerialError, StopBits, UsbInfo,
};

const SYS_TTY: &str = "/sys/class/tty";

/// A device whose rate came back further off than this is not usable as asked: past a few
/// percent the two ends lose each other within a character.
const MAX_BAUD_ERROR_PERCENT: u64 = 3;

pub(super) fn list_ports() -> Vec<PortInfo> {
    scan(Path::new(SYS_TTY), Path::new("/dev"))
}

fn scan(sys_tty: &Path, dev: &Path) -> Vec<PortInfo> {
    let Ok(entries) = fs::read_dir(sys_tty) else { return Vec::new() };
    let mut ports: Vec<PortInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let tty = entry.path();
            // Virtual consoles and ptys have no device behind them.
            let device = fs::canonicalize(tty.join("device")).ok()?;
            // The kernel keeps a ttyS for every possible 8250 port, most with nothing there.
            if read_text(&tty.join("type")).as_deref() == Some("0") {
                return None;
            }
            let driver = fs::read_link(device.join("driver"))
                .ok()
                .and_then(|link| link.file_name().map(|name| name.to_string_lossy().into_owned()));
            Some(PortInfo { path: dev.join(entry.file_name()), driver, usb: usb_info(&device) })
        })
        .collect();
    ports.sort_by(|a, b| a.path.cmp(&b.path));
    ports
}

/// Looks up the tree from a port's device for the USB device it belongs to.
fn usb_info(device: &Path) -> Option<UsbInfo> {
    let usb: PathBuf =
        device.ancestors().take(4).find(|dir| dir.join("idVendor").exists())?.to_path_buf();
    Some(UsbInfo {
        vendor_id: read_hex(&usb.join("idVendor"))?,
        product_id: read_hex(&usb.join("idProduct"))?,
        manufacturer: read_text(&usb.join("manufacturer")),
        product: read_text(&usb.join("product")),
        serial_number: read_text(&usb.join("serial")),
    })
}

fn read_text(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn read_hex(path: &Path) -> Option<u16> {
    u16::from_str_radix(&read_text(path)?, 16).ok()
}

fn open_error(error: io::Error) -> SerialError {
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENODEV | libc::ENXIO) => SerialError::NotFound,
        Some(libc::EACCES | libc::EPERM) => SerialError::PermissionDenied,
        Some(libc::EBUSY) => SerialError::Busy,
        Some(libc::ENOTTY) => SerialError::NotASerialPort,
        _ => SerialError::Io(error.to_string()),
    }
}

/// A port that is open and configured, not yet tied to a runtime.
pub(super) struct Device(OwnedFd);

impl Device {
    pub(super) fn open(path: &Path, config: SerialConfig) -> Result<Self, SerialError> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| SerialError::NotFound)?;
        // SAFETY: `path` is a valid C string that outlives the call.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(open_error(io::Error::last_os_error()));
        }
        // SAFETY: `open` just returned this descriptor and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // SAFETY: only reads a descriptor that is open.
        if unsafe { libc::isatty(fd.as_raw_fd()) } != 1 {
            return Err(SerialError::NotASerialPort);
        }
        lock(fd.as_raw_fd())?;
        configure(fd.as_raw_fd(), &config)?;
        Ok(Self(fd))
    }

    pub(super) fn register(self) -> io::Result<Registered> {
        let interest = Interest::READABLE | Interest::WRITABLE;
        // SAFETY: the descriptor is ours alone and lives exactly as long as the `AsyncFd`.
        Ok(Registered(unsafe { AsyncFd::register_with_interest(self.0, interest) }?))
    }
}

/// Keeps out programs honoring either convention, as a second writer corrupts both streams.
fn lock(fd: RawFd) -> Result<(), SerialError> {
    // SAFETY: `fd` is open; neither call takes a pointer.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::EWOULDBLOCK) => SerialError::Busy,
            _ => SerialError::Io(error.to_string()),
        });
    }
    // SAFETY: as above.
    if unsafe { libc::ioctl(fd, libc::TIOCEXCL) } != 0 {
        return Err(open_error(io::Error::last_os_error()));
    }
    Ok(())
}

fn get_attributes(fd: RawFd) -> Result<libc::termios2, SerialError> {
    let mut attributes = MaybeUninit::<libc::termios2>::uninit();
    // SAFETY: the kernel fills the struct, of the size this request expects, on success.
    if unsafe { libc::ioctl(fd, libc::TCGETS2 as _, attributes.as_mut_ptr()) } != 0 {
        return Err(open_error(io::Error::last_os_error()));
    }
    // SAFETY: the call succeeded.
    Ok(unsafe { attributes.assume_init() })
}

fn line_bits(config: &SerialConfig) -> libc::tcflag_t {
    let size = match config.data_bits {
        DataBits::Five => libc::CS5,
        DataBits::Six => libc::CS6,
        DataBits::Seven => libc::CS7,
        DataBits::Eight => libc::CS8,
    };
    let parity = match config.parity {
        Parity::None => 0,
        Parity::Odd => libc::PARENB | libc::PARODD,
        Parity::Even => libc::PARENB,
    };
    let stop = match config.stop_bits {
        StopBits::One => 0,
        StopBits::Two => libc::CSTOPB,
    };
    let flow = match config.flow_control {
        FlowControl::Hardware => libc::CRTSCTS,
        FlowControl::None | FlowControl::Software => 0,
    };
    size | parity | stop | flow
}

const LINE_MASK: libc::tcflag_t =
    libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS;

fn configure(fd: RawFd, config: &SerialConfig) -> Result<(), SerialError> {
    if config.baud_rate == 0 {
        return Err(SerialError::InvalidConfig(String::from("the baud rate is zero")));
    }

    let mut attributes = get_attributes(fd)?;
    // No translation, line editing or signals in either direction: the terminal handles those.
    attributes.c_iflag = match config.flow_control {
        FlowControl::Software => libc::IXON | libc::IXOFF,
        FlowControl::None | FlowControl::Hardware => 0,
    };
    attributes.c_oflag = 0;
    attributes.c_lflag = 0;
    attributes.c_cflag &= !(libc::CBAUD | libc::CIBAUD | LINE_MASK);
    attributes.c_cflag |= libc::BOTHER | libc::CREAD | libc::CLOCAL | line_bits(config);
    attributes.c_ispeed = config.baud_rate;
    attributes.c_ospeed = config.baud_rate;
    // A read has data, or waits: 0 would make "nothing yet" look like end of file.
    attributes.c_cc[libc::VMIN] = 1;
    attributes.c_cc[libc::VTIME] = 0;

    // SAFETY: `attributes` is a valid struct of the size this request expects.
    if unsafe { libc::ioctl(fd, libc::TCSETS2 as _, &attributes) } != 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::EINVAL) => SerialError::InvalidConfig(error.to_string()),
            _ => SerialError::Io(error.to_string()),
        });
    }

    // Drivers change what they cannot do without saying so, and report it only here.
    let applied = get_attributes(fd)?;
    if applied.c_cflag & LINE_MASK != attributes.c_cflag & LINE_MASK {
        return Err(SerialError::InvalidConfig(String::from(
            "the device does not support this character format or flow control",
        )));
    }
    let baud = u64::from(config.baud_rate);
    if u64::from(applied.c_ospeed).abs_diff(baud) * 100 > baud * MAX_BAUD_ERROR_PERCENT {
        return Err(SerialError::InvalidConfig(format!(
            "the device set {} baud for {baud}",
            applied.c_ospeed
        )));
    }
    Ok(())
}

/// An open port tied to the runtime's reactor.
pub(super) struct Registered(AsyncFd<OwnedFd>);

impl Drop for Registered {
    fn drop(&mut self) {
        let fd = self.0.as_raw_fd();
        // Closing waits for queued output, up to 30 seconds on a port held up by flow control,
        // and would stall whichever thread this is. Neither call waits for the device. The
        // exclusive flag would otherwise outlast this close if anyone else has the port open.
        // SAFETY: `fd` is open, and neither request reads memory through its argument.
        unsafe {
            libc::ioctl(fd, libc::TCFLSH as _, libc::TCOFLUSH);
            libc::ioctl(fd, libc::TIOCNXCL as _);
        }
    }
}

impl Registered {
    pub(super) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.0.readable().await?;
            match ready.try_io(|fd| read(fd.as_raw_fd(), buffer)) {
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EINTR) => {},
                Ok(result) => return result,
                Err(_would_block) => {},
            }
        }
    }

    pub(super) async fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.0.writable().await?;
            match ready.try_io(|fd| write(fd.as_raw_fd(), bytes)) {
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EINTR) => {},
                Ok(result) => return result,
                Err(_would_block) => {},
            }
        }
    }
}

fn read(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buffer` is valid for writes of its length.
    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    match usize::try_from(read) {
        Ok(read) => Ok(read),
        Err(_) => {
            let error = io::Error::last_os_error();
            // What a pty gives once its other end is gone, and a USB adapter once unplugged.
            if error.raw_os_error() == Some(libc::EIO) { Ok(0) } else { Err(error) }
        },
    }
}

fn write(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: `bytes` is valid for reads of its length.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    usize::try_from(written).map_err(|_| io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn line_bits_follow_the_settings() {
        let config = SerialConfig {
            baud_rate: 9600,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::Hardware,
        };
        assert_eq!(line_bits(&config), libc::CS7 | libc::PARENB | libc::CSTOPB | libc::CRTSCTS);

        let odd = SerialConfig { parity: Parity::Odd, ..SerialConfig::default() };
        assert_eq!(line_bits(&odd), libc::CS8 | libc::PARENB | libc::PARODD);
        assert_eq!(line_bits(&SerialConfig::default()), libc::CS8);
    }

    fn write_file(path: PathBuf, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn scan_finds_real_ports_with_their_usb_details() {
        let root = tempfile::tempdir().unwrap();
        let (sys, dev) = (root.path().join("tty"), root.path().join("dev"));
        fs::create_dir_all(&sys).unwrap();

        // A USB adapter, with its interface and tty nested under the USB device.
        let usb = root.path().join("devices/usb1/1-1");
        write_file(usb.join("idVendor"), "0403\n");
        write_file(usb.join("idProduct"), "6001\n");
        write_file(usb.join("product"), "FT232R\n");
        let usb_port = usb.join("1-1:1.0/ttyUSB0");
        fs::create_dir_all(&usb_port).unwrap();
        fs::create_dir_all(sys.join("ttyUSB0")).unwrap();
        symlink(&usb_port, sys.join("ttyUSB0/device")).unwrap();
        fs::create_dir_all(root.path().join("drivers/ftdi_sio")).unwrap();
        symlink(root.path().join("drivers/ftdi_sio"), usb_port.join("driver")).unwrap();

        // An 8250 slot with no hardware, a real one, and a virtual console.
        for (name, kind) in [("ttyS0", "4"), ("ttyS1", "0")] {
            let device = root.path().join("devices/platform").join(name);
            fs::create_dir_all(&device).unwrap();
            write_file(sys.join(name).join("type"), kind);
            symlink(&device, sys.join(name).join("device")).unwrap();
        }
        fs::create_dir_all(sys.join("tty0")).unwrap();

        let ports = scan(&sys, &dev);

        let paths: Vec<_> = ports.iter().map(|port| port.path.clone()).collect();
        assert_eq!(paths, [dev.join("ttyS0"), dev.join("ttyUSB0")]);
        assert_eq!(ports[0].usb, None);
        assert_eq!(ports[1].driver.as_deref(), Some("ftdi_sio"));
        let usb = ports[1].usb.as_ref().unwrap();
        assert_eq!((usb.vendor_id, usb.product_id), (0x0403, 0x6001));
        assert_eq!(usb.product.as_deref(), Some("FT232R"));
        assert_eq!(usb.manufacturer, None);
    }

    #[test]
    fn scan_of_a_missing_directory_is_empty() {
        assert!(scan(Path::new("/nonexistent"), Path::new("/dev")).is_empty());
    }
}
