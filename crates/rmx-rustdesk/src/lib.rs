//! A RustDesk client producing display updates, with no GPU or UI code.
//!
//! [`connect`] reaches a RustDesk peer, by its ID through a rendezvous server or straight at its
//! address, logs in and returns the [`DisplayHandle`] the interface drives. Connections are made
//! by a [`Dial`] the caller supplies, so a session can be tunnelled over SSH.
//!
//! This is an independent implementation of the protocol: the messages in `proto` carry the
//! field numbers of RustDesk's schema, and nothing of RustDesk's code is used.
//!
//! # What is supported
//!
//! - Reaching a peer by ID, through the relay or straight on the same network, and by address
//!   at its direct access port. UDP and TCP hole punching and WebRTC are not attempted.
//! - Encryption at both key exchange versions, with the peer's identity checked against the key
//!   of the rendezvous server. RustDesk serves a direct connection unencrypted and with no
//!   identity, and so does this client; only the password is protected, as a salted proof.
//! - Password login, two-factor codes, and sessions the person at the peer approves.
//! - Video, keyboard (by key position), pointer, wheel and text clipboard. No audio, file
//!   transfer or remote cursor shape: the cursor is drawn locally.
//! - The peer keeps its own resolution: [`InputEvent::Resize`](rmx_display::InputEvent::Resize)
//!   is ignored, and the interface scales the picture.

mod codec;
mod crypto;
mod handshake;
mod input;
mod proto;
mod rendezvous;
mod session;
mod stream;
mod video;

use std::fmt;
use std::time::Duration;

use rmx_display::DisplayHandle;
use tokio::sync::mpsc;

pub use handshake::Identity;
pub use rendezvous::{BoxFuture, Dial, PUBLIC_SERVER_KEY, RELAY_PORT, RENDEZVOUS_PORT, TcpDialer};
pub use stream::Transport;

/// The port a peer listens on for direct connections, when its user has turned them on.
pub const DIRECT_PORT: u16 = 21118;

/// The version this client reports. Peers turn features off for clients older than they know.
pub(crate) const PEER_VERSION: &str = "1.4.0";

/// How long each connection to a server or peer may take.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(18);

/// A rendezvous server, which knows where peers are and can relay to them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// `host` or `host:port`; the port defaults to [`RENDEZVOUS_PORT`].
    pub host: String,
    /// The server's public key, base64 as RustDesk shows it. `None` means RustDesk's own public
    /// servers, whose key is [`PUBLIC_SERVER_KEY`].
    pub key: Option<String>,
}

/// Where the peer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The peer with this ID, found through the server.
    Id { id: String, server: Server },
    /// The peer listening at `host:port`, normally [`DIRECT_PORT`].
    Direct { host: String, port: u16 },
}

/// How to log in.
#[derive(Clone, Default)]
pub struct Credentials {
    /// The peer's permanent or one-time password. Without one, the person at the peer is asked
    /// to accept the session.
    pub password: Option<String>,
    /// The current code, for a peer with two-factor authentication.
    pub two_factor_code: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("two_factor_code", &self.two_factor_code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// How much of the picture a session trades for bandwidth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// The peer's best quality.
    Best,
    /// The peer's balanced setting, its default.
    #[default]
    Balanced,
    /// Lower quality for a slow link.
    Fast,
}

/// What a session needs besides where to go.
#[derive(Clone, Debug)]
pub struct Config {
    pub target: Target,
    pub credentials: Credentials,
    pub quality: Quality,
    /// The name the peer shows its user for this client.
    pub client_name: String,
    /// How many events may wait for the interface before updates start to coalesce.
    pub event_capacity: usize,
    /// How many input events may wait for the session.
    pub input_capacity: usize,
}

impl Config {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            credentials: Credentials::default(),
            quality: Quality::default(),
            client_name: "rmx".to_owned(),
            event_capacity: 8,
            input_capacity: 256,
        }
    }
}

/// Why a session could not be established.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("the peer requires a password")]
    PasswordRequired,
    #[error("the peer rejected the password")]
    WrongPassword,
    #[error("the peer requires a two-factor code")]
    TwoFactorRequired,
    #[error("the peer rejected the two-factor code")]
    WrongTwoFactorCode,
    #[error("the rendezvous server does not know this ID")]
    UnknownId,
    #[error("the peer is offline")]
    Offline,
    #[error("the server key is not valid base64 of 32 bytes")]
    BadServerKey,
    #[error("the rendezvous server's signature does not match its key")]
    ServerKeyMismatch,
    #[error("the peer could not prove it is who the rendezvous server says")]
    PeerKeyMismatch,
    #[error("RustDesk connection failed: {0}")]
    Failed(String),
}

/// A session that is up.
#[derive(Debug)]
pub struct Session {
    pub display: DisplayHandle,
    /// How far the peer's identity was checked.
    pub identity: Identity,
    /// Whether the session goes through a relay server rather than straight to the peer.
    pub relayed: bool,
    /// The peer's operating system, as it reports it.
    pub platform: String,
}

/// Starts a session.
///
/// Returns once the peer has accepted the login; the first events are
/// [`Resized`](rmx_display::DisplayEvent::Resized) and then the first frame. Without a password
/// this waits up to two minutes for the person at the peer. The session ends with
/// [`Closed`](rmx_display::DisplayEvent::Closed), which follows an
/// [`InputEvent::Disconnect`](rmx_display::InputEvent::Disconnect), the loss of the connection,
/// or the interface dropping its input sender.
///
/// Must be called within a Tokio runtime with the time driver enabled.
pub async fn connect(dialer: &dyn Dial, config: Config) -> Result<Session, ConnectError> {
    let (mut connection, peer_id, peer_key, relayed) = match &config.target {
        Target::Id { id, server } => {
            let id = id.trim().replace(' ', "");
            let reached = rendezvous::reach(dialer, server, &id).await?;
            (reached.connection, Some(id), reached.peer_key, reached.relayed)
        },
        Target::Direct { host, port } => {
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, dialer.dial(host, *port))
                .await
                .map_err(|_| ConnectError::Failed(format!("{host}:{port} did not answer")))?
                .map_err(|err| {
                    ConnectError::Failed(format!("could not reach {host}:{port}: {err}"))
                })?;
            (stream::Connection::new(stream), None, None, false)
        },
    };
    let (identity, first) =
        handshake::secure(&mut connection, peer_id.as_deref(), peer_key.as_ref()).await?;

    let option = session::options(config.quality);
    let session_id = u64::from_le_bytes(crypto::random_key()[..8].try_into().expect("8 bytes"));
    let login = handshake::Login {
        peer_id: peer_id.as_deref().unwrap_or_default(),
        my_id: &config.client_name,
        my_name: &config.client_name,
        option,
        session_id,
    };
    let (info, early) =
        handshake::login(&mut connection, &config.credentials, login, first).await?;
    connection.set_max_frame(stream::MAX_SESSION_FRAME);

    let (events_tx, events) = mpsc::channel(config.event_capacity.max(1));
    let (input, input_rx) = mpsc::channel(config.input_capacity.max(1));
    let platform = info.platform.clone();
    session::start(connection, info, early, input_rx, events_tx)?;
    Ok(Session { display: DisplayHandle { events, input }, identity, relayed, platform })
}

#[cfg(test)]
mod tests;
