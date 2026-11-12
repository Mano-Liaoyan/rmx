use std::{fmt, io};

use russh::ChannelOpenFailure;

use crate::host_key::HostKeyRejection;

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The TCP connection to the host could not be made.
    #[error("could not connect to {host}:{port}: {source}")]
    Connect {
        /// The host that was dialled.
        host: String,
        /// The port that was dialled.
        port: u16,
        /// The underlying error.
        source: io::Error,
    },

    /// A connection step did not finish within
    /// [`ConnectOptions::timeout`](crate::ConnectOptions::timeout).
    #[error("timed out during {stage} with {host}:{port}")]
    Timeout {
        /// The host being contacted.
        host: String,
        /// The port being contacted.
        port: u16,
        /// Which step timed out.
        stage: &'static str,
    },

    /// The [`HostKeyVerifier`](crate::HostKeyVerifier) refused the server's key.
    #[error("host key for {host}:{port} ({fingerprint}) rejected: {reason}")]
    HostKeyRejected {
        /// The host that presented the key.
        host: String,
        /// The port that presented the key.
        port: u16,
        /// The SHA-256 fingerprint of the presented key.
        fingerprint: String,
        /// Why it was rejected.
        reason: HostKeyRejection,
    },

    /// No configured authentication method succeeded.
    #[error(transparent)]
    AuthFailed(#[from] AuthFailure),

    /// The [`Prompter`](crate::Prompter) cancelled authentication.
    #[error("authentication to {host} as {user} was cancelled")]
    AuthCancelled {
        /// The host being authenticated to.
        host: String,
        /// The user being authenticated.
        user: String,
    },

    /// A jump host in the chain failed; `source` says how.
    #[error("jump host {host}:{port}: {source}")]
    Jump {
        /// The jump host that failed.
        host: String,
        /// The jump host's port.
        port: u16,
        /// What went wrong there.
        #[source]
        source: Box<Error>,
    },

    /// The server refused to open a channel.
    #[error("server refused channel to {destination}: {reason:?}")]
    ChannelOpen {
        /// What the channel was for, for example `db.internal:5432` or `session`.
        destination: String,
        /// The server's stated reason.
        reason: ChannelOpenFailure,
    },

    /// The server denied a request on an open channel or connection, such as a
    /// PTY, a subsystem or a remote forward.
    #[error("server denied request: {0}")]
    RequestDenied(String),

    /// The connection ended.
    #[error("disconnected: {0}")]
    Disconnected(DisconnectReason),

    /// The caller's configuration cannot be used.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// A local I/O error, such as failing to bind a listening socket.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// An SSH protocol error.
    #[error("ssh protocol error: {0}")]
    Protocol(String),
}

impl From<russh::Error> for Error {
    fn from(error: russh::Error) -> Self {
        match error {
            russh::Error::ChannelOpenFailure(reason) => {
                Error::ChannelOpen { destination: String::from("channel"), reason }
            },
            russh::Error::IO(source) => Error::Io(source),
            russh::Error::RequestDenied => Error::RequestDenied(String::from("request denied")),
            russh::Error::Disconnect
            | russh::Error::HUP
            | russh::Error::SendError
            | russh::Error::RecvError => Error::Disconnected(DisconnectReason::ConnectionClosed),
            russh::Error::KeepaliveTimeout => {
                Error::Disconnected(DisconnectReason::KeepaliveTimeout)
            },
            russh::Error::InactivityTimeout => {
                Error::Disconnected(DisconnectReason::InactivityTimeout)
            },
            other => Error::Protocol(other.to_string()),
        }
    }
}

/// Why an established connection ended, as reported by
/// [`SshClient::closed`](crate::SshClient::closed).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// This side called [`SshClient::disconnect`](crate::SshClient::disconnect).
    Requested,
    /// The server sent an SSH disconnect message.
    Server {
        /// The RFC 4253 disconnect reason code.
        code: u32,
        /// The server's message.
        message: String,
    },
    /// The server stopped answering keepalives.
    KeepaliveTimeout,
    /// Nothing was exchanged for the configured inactivity timeout.
    InactivityTimeout,
    /// The transport closed without an SSH disconnect message.
    ConnectionClosed,
    /// The transport failed with an I/O or protocol error.
    Transport(String),
    /// A jump host this connection runs through went away.
    JumpHostLost {
        /// The jump host that was lost.
        host: String,
        /// The jump host's port.
        port: u16,
        /// Why the jump host's connection ended.
        reason: Box<DisconnectReason>,
    },
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisconnectReason::Requested => f.write_str("closed by this client"),
            DisconnectReason::Server { code, message } if message.is_empty() => {
                write!(f, "closed by the server (reason code {code})")
            },
            DisconnectReason::Server { code, message } => {
                write!(f, "closed by the server: {message} (reason code {code})")
            },
            DisconnectReason::KeepaliveTimeout => {
                f.write_str("server stopped answering keepalives")
            },
            DisconnectReason::InactivityTimeout => f.write_str("idle for too long"),
            DisconnectReason::ConnectionClosed => f.write_str("connection closed unexpectedly"),
            DisconnectReason::Transport(message) => write!(f, "connection failed: {message}"),
            DisconnectReason::JumpHostLost { host, port, reason } => {
                write!(f, "lost jump host {host}:{port}: {reason}")
            },
        }
    }
}

/// One authentication method that was tried and did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthAttempt {
    /// Which method: `password`, `publickey`, `agent`, or `keyboard-interactive`.
    pub method: &'static str,
    /// What happened, without any secret material.
    pub detail: String,
}

/// Every authentication method failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "authentication to {host} as {user} failed (server accepts: {}; tried: {})",
    server_methods.join(", "),
    describe_attempts(attempts)
)]
pub struct AuthFailure {
    /// The host authenticated to.
    pub host: String,
    /// The user authenticated as.
    pub user: String,
    /// What was tried, in order.
    pub attempts: Vec<AuthAttempt>,
    /// The methods the server last said it would accept.
    pub server_methods: Vec<String>,
}

fn describe_attempts(attempts: &[AuthAttempt]) -> String {
    if attempts.is_empty() {
        return String::from("nothing");
    }
    let parts: Vec<String> =
        attempts.iter().map(|a| format!("{}: {}", a.method, a.detail)).collect();
    parts.join("; ")
}
