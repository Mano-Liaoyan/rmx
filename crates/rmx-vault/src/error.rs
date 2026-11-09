//! Error types shared across the crate.

use std::path::PathBuf;

use crate::kind::VaultKind;

/// Everything a vault operation can fail with.
///
/// No variant ever carries key material, passwords or signed URLs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The vault was locked (explicitly or by the idle timeout); unlock it first.
    #[error("the vault is locked")]
    Locked,
    /// The master password or key file does not decrypt this file.
    #[error("wrong master password or key file")]
    WrongKey,
    /// The file does not start with an rmx vault header.
    #[error("not an rmx vault file")]
    NotAVault,
    /// The file uses a schema version this build cannot handle.
    #[error("unsupported vault schema version {found} (this build supports {supported})")]
    UnsupportedFormat {
        /// Version found in the file.
        found: i64,
        /// Version this build reads and writes.
        supported: i64,
    },
    /// A vault already exists where a new one was to be created.
    #[error("a vault already exists at {}", .0.display())]
    AlreadyExists(PathBuf),
    /// A path is not valid UTF-8, which SQLite requires.
    #[error("path is not valid UTF-8: {}", .0.display())]
    InvalidPath(PathBuf),
    /// Credentials were empty, or KDF parameters are out of range.
    #[error("invalid key parameters: {0}")]
    InvalidKeyParams(&'static str),
    /// Argon2id failed.
    #[error("key derivation failed")]
    Kdf,
    /// A stored hybrid logical clock value could not be parsed.
    #[error("malformed hybrid logical clock value")]
    InvalidHlc,
    /// The requested row or revision does not exist.
    #[error("row or revision not found")]
    NotFound,
    /// The remote copy has a different vault identity.
    #[error("the remote copy belongs to a different vault")]
    VaultMismatch,
    /// One of two copies is encrypted and the other is not. Neither was changed.
    #[error("{}", kind_mismatch(*.local))]
    KindMismatch {
        /// What the working copy is.
        local: VaultKind,
        /// What the other copy is.
        remote: VaultKind,
    },
    /// That only makes sense for an encrypted vault, and this one has no master password.
    #[error("this database has no master password")]
    Unencrypted,
    /// The remote copy is encrypted under other credentials; the master password was changed.
    #[error("the remote copy is encrypted with different credentials")]
    RemoteKeyChanged,
    /// The remote copy has this vault's header but does not decrypt.
    #[error("the remote copy is corrupt")]
    RemoteCorrupt,
    /// The backend holds no remote copy.
    #[error("the backend holds no remote copy")]
    RemoteMissing,
    /// Every sync attempt lost its compare-and-swap race.
    #[error("sync gave up after {0} conflicting attempts")]
    SyncContention(u32),
    /// A sync backend failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// SQLite or SQLCipher failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The system keyring failed.
    #[cfg(feature = "keyring")]
    #[error("keyring: {0}")]
    Keyring(String),
    /// No quick-unlock key is stored for this working copy.
    #[cfg(feature = "keyring")]
    #[error("no quick-unlock key is stored for this vault")]
    NoQuickUnlock,
}

/// A short sentence, since it ends up at the foot of the sidebar.
fn kind_mismatch(local: VaultKind) -> &'static str {
    match local {
        VaultKind::Plain => "Not synced: the remote copy is encrypted, this database is not",
        VaultKind::Encrypted => "Not synced: the remote copy is unencrypted, this database is encrypted",
    }
}

/// Failures of a [`SyncBackend`](crate::SyncBackend).
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The remote changed since the expected version: fetch, merge and retry.
    #[error("the remote changed concurrently")]
    Conflict,
    /// The server answered with an unexpected status.
    #[error("server answered with HTTP status {0}")]
    Http(u16),
    /// The request could not be carried out.
    #[error("transport error: {0}")]
    Transport(String),
    /// The configuration or the server's behaviour rules out safe sync.
    #[error("{0}")]
    Protocol(String),
    /// A local or remote file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
