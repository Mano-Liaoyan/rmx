use std::fmt;

use async_trait::async_trait;
use russh::keys::{HashAlg, PublicKey};

/// The server key to be judged by a [`HostKeyVerifier`].
#[derive(Debug)]
pub struct HostKeyCheck<'a> {
    /// The host name as given in the configuration, which for a jump chain is
    /// the name that hop's predecessor dialled.
    pub host: &'a str,
    /// The port.
    pub port: u16,
    /// The server's public key. The server has already proved it holds the
    /// private half.
    pub key: &'a PublicKey,
}

impl HostKeyCheck<'_> {
    /// The key's SHA-256 fingerprint, in the `SHA256:...` form OpenSSH prints.
    pub fn fingerprint(&self) -> String {
        self.key.fingerprint(HashAlg::Sha256).to_string()
    }
}

/// A [`HostKeyVerifier`]'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Trust the key and continue.
    Accept,
    /// Refuse the connection.
    Reject(HostKeyRejection),
}

/// Why a host key was not trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostKeyRejection {
    /// The host has no recorded key and nothing approved this one.
    Unknown,
    /// A different key is recorded for this host.
    Changed {
        /// Fingerprint of the recorded key.
        known_fingerprint: String,
        /// Where the recorded key came from, for example `/home/u/.ssh/known_hosts:12`.
        location: String,
    },
    /// The key is marked `@revoked`.
    Revoked {
        /// Where the revocation was recorded.
        location: String,
    },
    /// The user or application declined the key.
    Declined,
    /// The check could not be carried out, for example an unreadable file.
    Failed(String),
}

impl fmt::Display for HostKeyRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostKeyRejection::Unknown => f.write_str("unknown host"),
            HostKeyRejection::Changed { known_fingerprint, location } => {
                write!(f, "key has changed, expected {known_fingerprint} from {location}")
            },
            HostKeyRejection::Revoked { location } => write!(f, "key is revoked in {location}"),
            HostKeyRejection::Declined => f.write_str("declined"),
            HostKeyRejection::Failed(message) => write!(f, "could not verify: {message}"),
        }
    }
}

/// Decides whether to trust a server's host key.
///
/// Implement this to back host keys with the UI or a database. For a
/// `known_hosts` file use [`KnownHostsVerifier`](crate::KnownHostsVerifier).
///
/// `verify` may wait on a user: the handshake timeout does not run while it is
/// pending. It is called once per connection and once per hop of a jump chain.
#[async_trait]
pub trait HostKeyVerifier: Send + Sync {
    /// Judges one key.
    async fn verify(&self, check: &HostKeyCheck<'_>) -> HostKeyDecision;
}
