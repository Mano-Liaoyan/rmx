//! An RDP client producing display updates.
//!
//! [`connect`] runs the RDP handshake over a stream the caller supplies, so a session can go
//! straight to a socket or through an SSH tunnel, and returns the [`DisplayHandle`] the
//! interface drives. Nothing here touches the GPU or any window.
//!
//! # Tasks
//!
//! The session lives on a dedicated OS thread, which runs the handshake and then decodes every
//! bitmap and RemoteFX update into a BGRA8 framebuffer. Socket reads and writes are separate
//! Tokio tasks, so a slow decode delays neither input nor acknowledgements.
//!
//! # Backpressure
//!
//! Only dirty rectangles queue, never pixels. When the event channel has room, the pixels of a
//! rectangle are copied from the framebuffer as they are at that moment, so a backed-up
//! interface receives the newest picture rather than a history of old ones. Rectangles merge
//! once too many are waiting, and whatever is dirty is delivered as soon as the channel frees
//! up, even if the server has gone quiet, so the session always ends on the correct picture.

mod clipboard;
mod input;
mod outbox;
mod session;
mod tls;

use std::fmt;
use std::thread;
use std::time::Duration;

use rmx_display::DisplayHandle;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

/// How the server may authenticate the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// Require Network Level Authentication: TLS followed by CredSSP.
    Nla,
    /// Require plain TLS, with the password sent in the session after the handshake and the
    /// logon screen on the server.
    Tls,
    /// Offer both and let the server choose.
    NlaOrTls,
}

/// How much of the picture a session trades for bandwidth.
///
/// Each step asks the server for less: fewer colors, fewer desktop effects, and a slower
/// connection type, which servers use to pick their codecs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// 32-bit color with the wallpaper, themes, animations, font smoothing, desktop composition
    /// and window contents while dragging, for a fast network.
    Best,
    /// 32-bit color and font smoothing, without menu animations or window contents while
    /// dragging.
    #[default]
    Balanced,
    /// 16-bit color with no wallpaper, themes, animations, cursor shadow or font smoothing, for
    /// a slow link. RemoteFX needs 32-bit color, so servers send plain bitmaps instead.
    Fast,
}

/// How to decide whether the server's TLS certificate is acceptable.
///
/// RDP servers almost always present a self-signed certificate, so there is no chain to check.
/// The certificate's key is still proved by the handshake, and the decision is made before any
/// credential is sent.
pub enum CertificatePolicy {
    /// Accept only a certificate with this SHA-256 fingerprint of its DER encoding.
    Pinned([u8; 32]),
    /// Send a [`CertificatePrompt`] and wait for the answer, for example to ask the user.
    Ask(mpsc::Sender<CertificatePrompt>),
    /// Accept any certificate. An attacker on the path can then read the session.
    AcceptAny,
}

impl fmt::Debug for CertificatePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pinned(_) => "Pinned",
            Self::Ask(_) => "Ask",
            Self::AcceptAny => "AcceptAny",
        })
    }
}

/// A server certificate waiting for a decision under [`CertificatePolicy::Ask`].
#[derive(Debug)]
pub struct CertificatePrompt {
    /// The name the session was configured to reach.
    pub server_name: String,
    /// The leaf certificate, DER encoded.
    pub der: Vec<u8>,
    /// SHA-256 of `der`.
    pub sha256: [u8; 32],
    /// Send `true` to continue the connection. Dropping this rejects the certificate.
    pub answer: oneshot::Sender<bool>,
}

/// What a session connects to and how.
///
/// The `Debug` output never includes the password.
pub struct RdpConfig {
    /// The name of the server, used for TLS and for the CredSSP target. It need not resolve,
    /// since the stream is already connected.
    pub server_name: String,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// The desktop size to request, in pixels.
    pub width: u16,
    pub height: u16,
    pub security: Security,
    pub certificate: CertificatePolicy,
    /// The Windows keyboard layout identifier the remote should assume, such as `0x0409` for US.
    pub keyboard_layout: u32,
    /// Offer RemoteFX. Without it the server falls back to plain bitmap updates, which are
    /// lossless but need far more bandwidth.
    pub remotefx: bool,
    /// How much of the picture to trade for bandwidth.
    pub quality: Quality,
    /// The name this client gives the server.
    pub client_name: String,
    /// The bound of the [`DisplayHandle::events`] channel. A smaller value coalesces harder.
    pub event_capacity: usize,
    /// How long the whole handshake may take.
    pub connect_timeout: Duration,
}

impl RdpConfig {
    /// A configuration requiring NLA, with a pinned certificate and defaults for the rest.
    pub fn new(
        server_name: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        (width, height): (u16, u16),
        sha256: [u8; 32],
    ) -> Self {
        Self {
            server_name: server_name.into(),
            username: username.into(),
            password: password.into(),
            domain: None,
            width,
            height,
            security: Security::Nla,
            certificate: CertificatePolicy::Pinned(sha256),
            keyboard_layout: 0x0409,
            remotefx: true,
            quality: Quality::default(),
            client_name: "rmx".to_owned(),
            event_capacity: 8,
            connect_timeout: Duration::from_secs(30),
        }
    }
}

impl fmt::Debug for RdpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpConfig")
            .field("server_name", &self.server_name)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("security", &self.security)
            .field("certificate", &self.certificate)
            .field("keyboard_layout", &self.keyboard_layout)
            .field("remotefx", &self.remotefx)
            .field("quality", &self.quality)
            .field("client_name", &self.client_name)
            .field("event_capacity", &self.event_capacity)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

/// Why a session could not be established.
///
/// Once [`connect`] has returned, failures arrive as `DisplayEvent::Closed` instead.
#[derive(Debug, thiserror::Error)]
pub enum RdpError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS failed: {0}")]
    Tls(String),
    #[error("the server certificate was rejected")]
    CertificateRejected,
    #[error("the RDP handshake failed: {0}")]
    Handshake(String),
    #[error("the server redirected the connection, which is not supported")]
    Redirected,
    #[error("the RDP handshake timed out")]
    Timeout,
    #[error("the session ended during the handshake")]
    Aborted,
}

/// Establishes a session over `stream` and starts it.
///
/// Returns once the session is up, with `DisplayEvent::Resized` as the first event. Must be
/// called inside a Tokio runtime with timers enabled. Dropping the returned future cancels the
/// handshake.
pub async fn connect<S>(stream: S, config: RdpConfig) -> Result<DisplayHandle, RdpError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let runtime = tokio::runtime::Handle::current();
    let (ready_tx, ready_rx) = oneshot::channel();
    thread::Builder::new()
        .name("rmx-rdp-decode".to_owned())
        .spawn(move || runtime.block_on(session::run(stream, config, ready_tx)))?;
    ready_rx.await.map_err(|_| RdpError::Aborted)?
}
