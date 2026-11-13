//! The connection database and its synchronization.
//!
//! A vault is one SQLCipher file (`.rmxdb`), keyed by Argon2id over a master password and an
//! optional key file; or, when the user chose to have no master password, a plain SQLite file
//! ([`VaultKind`]), which says so in its header and is never dressed up as encrypted. The
//! application only ever opens a local working copy through [`Vault`]; other devices are reached
//! with [`Vault::sync`] and a [`SyncBackend`]:
//!
//! - every row carries a UUID and a [hybrid logical clock](Hlc) value;
//! - [`Vault::put`] and [`Vault::delete`] keep superseded versions in `history` and deletions in
//!   `tombstones`, from which [`Vault::history`] and [`Vault::restore`] read;
//! - a sync downloads the remote file, merges it row by row in one transaction, and uploads a
//!   `VACUUM INTO` snapshot with compare-and-swap, retrying when it loses a race. The merge is
//!   commutative and idempotent, so devices converge whatever order they sync in.
//!
//! Folders can hand a login down to the connections inside them: see [`inherit`].
//!
//! Device-local data (window layout, sync cursors) lives in [`LocalState`], outside the vault.
//!
//! Everything here blocks on disk, Argon2id or the network: keep it off the UI thread.
//!
//! # Features
//!
//! - `bundled-sqlcipher` (default): compile SQLCipher, link the system libcrypto.
//! - `vendored-openssl`: compile and statically link OpenSSL too.
//! - `keyring`: quick unlock through the desktop's Secret Service ([`quick_unlock`]).

#![warn(missing_docs)]

mod error;
mod hlc;
pub mod inherit;
mod key;
mod kind;
mod local_state;
mod merge;
mod records;
mod schema;
mod secret;
mod sync;
mod vault;

#[cfg(feature = "keyring")]
pub mod quick_unlock;

pub use error::{BackendError, Error, Result};
pub use hlc::{Hlc, HlcClock};
pub use key::{Credentials, KdfParams, generate_key_file};
pub use kind::VaultKind;
pub use local_state::{LocalState, SyncCursor};
pub use merge::HISTORY_LIMIT;
pub use records::{
    Connection, Credential, Folder, FolderLogin, Id, KnownHost, Record, SharedSetting, Snippet,
    SshKey, Tunnel,
};
pub use schema::SCHEMA_VERSION;
pub use secret::Secret;
pub use sync::{
    ConflictCopy, LocalPathBackend, RemoteFiles, RemoteObject, RemoteVersion, S3Backend, S3Config,
    SftpBackend, SyncBackend, SyncReport, WebDavBackend, WebDavConfig,
};
pub use vault::{MergeReport, Revision, Vault};
