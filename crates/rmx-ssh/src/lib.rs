//! SSH client library for rmx: authentication, shells, jump hosts, tunnels and
//! port forwarding, with no UI and no database.
//!
//! The entry point is [`connect`], which takes a [`ConnectConfig`] and returns
//! an [`SshClient`]. Everything the caller has to decide interactively goes
//! through two async traits, so the UI and the database can supply the answers
//! without this crate depending on them:
//!
//! * [`Prompter`] asks for passwords, key passphrases and keyboard-interactive
//!   responses.
//! * [`HostKeyVerifier`] decides whether a server's host key is trusted.
//!   [`KnownHostsVerifier`] implements it on an OpenSSH `known_hosts` file.
//!
//! From an [`SshClient`]:
//!
//! * [`SshClient::open_shell`] returns a [`ShellReader`] and [`ShellWriter`],
//!   bounded byte streams for an async terminal driver.
//! * [`SshClient::open_direct_tcpip`] and [`SshClient::open_sftp`] return an
//!   [`SshStream`] implementing `AsyncRead + AsyncWrite`, which is how other
//!   protocols are tunnelled.
//! * [`SshClient::forward_local`], [`SshClient::forward_remote`] and
//!   [`SshClient::forward_dynamic`] start port forwards.
//! * [`SshClient::closed`] reports why the connection ended.
//!
//! [`ssh_config`] parses `~/.ssh/config` for import.
//!
//! All of this runs on a Tokio runtime. Secrets are held in [`SecretString`],
//! which is zeroed on drop and redacted from `Debug` output; this crate never
//! logs them.
//!
//! # Backpressure
//!
//! A channel's incoming data is delivered through a bounded queue of
//! [`ConnectOptions::channel_buffer`] packets. `russh` reads the socket and
//! waits for room in that queue, so a consumer that stops reading eventually
//! pauses the whole connection, including its other channels and keepalive
//! replies. Keep reading every open channel, or close the ones you are done
//! with, and read a shell concurrently with writing to it: a write waits for
//! the server, and the server may be waiting for its output to be read.

#![warn(missing_docs)]

mod auth;
mod client;
mod error;
mod forward;
mod host_key;
mod known_hosts;
mod pattern;
mod secret;
mod shell;
pub mod ssh_config;
mod stream;

pub use auth::{
    AuthMethod, KeyboardChallenge, KeyboardPrompt, NoPrompter, PromptContext, Prompter,
};
pub use client::{ConnectConfig, ConnectOptions, HostConfig, SshClient, connect};
pub use error::{AuthAttempt, AuthFailure, DisconnectReason, Error};
pub use forward::PortForward;
pub use host_key::{HostKeyCheck, HostKeyDecision, HostKeyRejection, HostKeyVerifier};
pub use known_hosts::{KnownHosts, KnownHostsStatus, KnownHostsVerifier, UnknownHostPolicy};
pub use russh::keys::PublicKey;
pub use russh::{ChannelOpenFailure, Pty};
pub use secret::SecretString;
pub use shell::{ShellExit, ShellOptions, ShellReader, ShellWriter};
pub use stream::SshStream;
