//! A VNC client producing display updates, with no GPU or UI code.
//!
//! [`connect`] runs the RFB handshake over a stream the caller supplies, so a session can be
//! tunnelled over SSH, and returns the [`DisplayHandle`] the interface drives. Updates are
//! BGRA8 and always opaque.
//!
//! # Backpressure
//!
//! Decoding runs on background tasks and never waits for the interface. While the bounded
//! event channel is full the session paints into a private copy of the remote desktop and
//! only remembers which regions changed. When the channel has room those regions are sent
//! with their latest pixels, so memory stays bounded and the interface still ends up with
//! the correct final picture.
//!
//! # What is negotiated
//!
//! Pixel encodings are CopyRect, ZRLE, Tight (lossless only, and only at full color), TRLE and
//! Raw, at the [`ColorDepth`] the session asks for. Security is VNC
//! password or none; a server offering only VeNCrypt or TLS is refused by `vnc-rs`. The cursor
//! stays part of the framebuffer, since [`DisplayEvent`](rmx_display::DisplayEvent) cannot
//! carry a cursor shape.
//!
//! # Limits inherited from `vnc-rs` 0.6
//!
//! - Desktops above 8294400 pixels (3840x2160) or 8192 on a side are refused.
//! - Clipboard text does not survive outside ASCII. The library decodes server text as lossy UTF-8
//!   where RFB specifies Latin-1, sends local text as UTF-8 bytes, and has no extended clipboard.
//! - Resizing needs a server that announced ExtendedDesktopSize and a single screen covering the
//!   desktop. A refused [`InputEvent::Resize`](rmx_display::InputEvent::Resize) is logged and
//!   otherwise leaves the session as it was.

mod coalesce;
mod input;
mod session;

use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll};
use std::{fmt, io};

use rmx_display::DisplayHandle;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use vnc::{PixelFormat, VncConnector, VncEncoding, VncError};

/// How many colors a session asks the server for. Fewer colors need less bandwidth.
///
/// `vnc-rs` cannot offer the JPEG quality or compression level pseudo-encodings, so the depth is
/// the only quality a session can choose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit true color.
    #[default]
    Full,
    /// 16 bits a pixel, 5-6-5: half the data of full color, with slight banding in gradients.
    Bits16,
    /// 8 bits a pixel, 3-3-2: a quarter of the data, with plainly visible banding.
    Bits8,
}

impl ColorDepth {
    /// The format the server is asked to send pixels in, always little-endian true color.
    fn pixel_format(self) -> PixelFormat {
        let mut format = PixelFormat::bgra();
        let (bits, [red, green, blue]) = match self {
            Self::Full => return format,
            Self::Bits16 => (16, [(31, 11), (63, 5), (31, 0)]),
            Self::Bits8 => (8, [(7, 5), (7, 2), (3, 0)]),
        };
        format.bits_per_pixel = bits;
        format.depth = bits;
        (format.red_max, format.red_shift) = red;
        (format.green_max, format.green_shift) = green;
        (format.blue_max, format.blue_shift) = blue;
        format
    }
}

/// What a session needs besides its stream.
#[derive(Clone)]
pub struct Config {
    /// The VNC password. Only its first 8 bytes matter to the protocol, and it is not used if
    /// the server asks for none.
    pub password: Option<String>,
    /// Leave other clients of the server connected.
    pub shared: bool,
    /// How many colors to ask the server for.
    pub color_depth: ColorDepth,
    /// How many events may wait for the interface before updates start to coalesce.
    pub event_capacity: usize,
    /// How many input events may wait for the session.
    pub input_capacity: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            password: None,
            shared: true,
            color_depth: ColorDepth::default(),
            event_capacity: 8,
            input_capacity: 256,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("shared", &self.shared)
            .field("color_depth", &self.color_depth)
            .field("event_capacity", &self.event_capacity)
            .field("input_capacity", &self.input_capacity)
            .finish()
    }
}

/// Why a session could not be established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// The server wants a password and [`Config::password`] is `None`.
    PasswordRequired,
    /// The server rejected the password. Servers speaking RFB 3.8 give a reason instead, which
    /// arrives as [`ConnectError::Failed`].
    WrongPassword,
    /// The handshake failed, with the reason from the server or the transport.
    Failed(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PasswordRequired => f.write_str("the VNC server requires a password"),
            Self::WrongPassword => f.write_str("the VNC server rejected the password"),
            Self::Failed(reason) => write!(f, "VNC handshake failed: {reason}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<VncError> for ConnectError {
    fn from(error: VncError) -> Self {
        match error {
            VncError::NoPassword => Self::PasswordRequired,
            VncError::WrongPassword => Self::WrongPassword,
            other => Self::Failed(other.to_string()),
        }
    }
}

/// Starts a VNC session over `stream`.
///
/// Returns once the handshake is done; the first events are
/// [`Resized`](rmx_display::DisplayEvent::Resized) and the updates of the initial full frame.
/// The session ends with [`Closed`](rmx_display::DisplayEvent::Closed), which follows an
/// [`InputEvent::Disconnect`](rmx_display::InputEvent::Disconnect), the loss of the stream, or
/// the interface dropping its input sender.
///
/// Must be called within a Tokio runtime with the time driver enabled. Wrap the call in a
/// timeout if the far end might never answer.
pub async fn connect<S>(stream: S, config: Config) -> Result<DisplayHandle, ConnectError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let Config { password, shared, color_depth, event_capacity, input_capacity } = config;
    let format = color_depth.pixel_format();
    let mut connector = VncConnector::new(SyncStream(Mutex::new(stream)))
        .set_auth_method(async move { password.ok_or(VncError::NoPassword) })
        .allow_shared(shared)
        .set_pixel_format(format)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Zrle);
    // The Tight decoder of `vnc-rs` takes only 32-bit pixels.
    if color_depth == ColorDepth::Full {
        connector = connector.add_encoding(VncEncoding::Tight);
    }
    let client = connector
        .add_encoding(VncEncoding::Trle)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .add_encoding(VncEncoding::ExtendedDesktopSizePseudo)
        .add_encoding(VncEncoding::LastRectPseudo)
        .build()?
        .try_start()
        .await?
        .finish()?;

    let (events_tx, events) = mpsc::channel(event_capacity.max(1));
    let (input, input_rx) = mpsc::channel(input_capacity.max(1));
    tokio::spawn(session::run(client, format, input_rx, events_tx));
    Ok(DisplayHandle { events, input })
}

/// Lets a `Send`-only stream satisfy the `Sync` bound `VncConnector` puts on its stream.
///
/// `Mutex::get_mut` needs no locking, so this costs nothing at run time.
struct SyncStream<S>(Mutex<S>);

impl<S: Unpin> SyncStream<S> {
    fn stream(self: Pin<&mut Self>) -> Pin<&mut S> {
        Pin::new(self.get_mut().0.get_mut().unwrap_or_else(PoisonError::into_inner))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for SyncStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.stream().poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SyncStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.stream().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream().poll_shutdown(cx)
    }
}
