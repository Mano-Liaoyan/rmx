//! Synchronization: the backend contract and the fetch-merge-push loop.
//!
//! The file on the backend is only ever replaced by a whole `VACUUM INTO` snapshot, so WAL files
//! and half-written states never reach it. A lost compare-and-swap race is resolved by fetching
//! again, merging, and retrying. Everything here blocks: run it off the UI thread.

mod http;
mod local_path;
mod s3;
mod sftp;
mod webdav;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

pub use local_path::LocalPathBackend;
use log::{debug, warn};
use ring::digest::{self, SHA256};
pub use s3::{S3Backend, S3Config};
pub use sftp::{RemoteFiles, SftpBackend};
pub use webdav::{WebDavBackend, WebDavConfig};

use crate::error::{BackendError, Error, Result};
use crate::key::{self, Credentials, VaultKey};
use crate::local_state::{LocalState, SyncCursor};
use crate::vault::{TempDb, Vault};

const MAX_ATTEMPTS: u32 = 8;

/// An opaque token identifying one state of the remote file: an ETag or a content hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteVersion(pub String);

/// The remote file and the version it was read at.
#[derive(Clone, Debug)]
pub struct RemoteObject {
    /// Encrypted vault bytes.
    pub data: Vec<u8>,
    /// Version of those bytes.
    pub version: RemoteVersion,
}

/// A divergent copy of the vault left by a file synchronizer.
#[derive(Clone, Debug)]
pub struct ConflictCopy {
    /// Backend-specific name, passed back to [`SyncBackend::remove_conflict_copy`].
    pub name: String,
    /// Encrypted vault bytes.
    pub data: Vec<u8>,
}

/// Where the shared copy of a vault lives.
///
/// Implementations hold no plaintext: everything they move is already encrypted.
pub trait SyncBackend: Send + Sync {
    /// A stable identifier without secrets, used to key the local sync cursor.
    fn id(&self) -> String;

    /// The current remote version, or `None` if there is no remote file.
    fn head(&self) -> Result<Option<RemoteVersion>, BackendError>;

    /// The remote file, or `None` if there is none.
    fn get(&self) -> Result<Option<RemoteObject>, BackendError>;

    /// Replaces the remote file if it is still at `expected`; `None` requires that none exists.
    ///
    /// Fails with [`BackendError::Conflict`] if the remote moved. Returns the new version, or
    /// `Ok(None)` if the server did not report it.
    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError>;

    /// Divergent copies to merge. The default is none.
    fn conflict_copies(&self) -> Result<Vec<ConflictCopy>, BackendError> {
        Ok(Vec::new())
    }

    /// Discards a conflict copy that has been merged.
    fn remove_conflict_copy(&self, _name: &str) -> Result<(), BackendError> {
        Ok(())
    }
}

/// What one [`Vault::sync`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// A remote file was downloaded and merged.
    pub pulled: bool,
    /// A new snapshot was uploaded.
    pub pushed: bool,
    /// Compare-and-swap races lost and retried.
    pub conflicts_retried: u32,
    /// Conflict copies merged and removed.
    pub conflict_copies_merged: usize,
    /// The working copy gained rows from elsewhere.
    pub changed_locally: bool,
}

pub(crate) fn content_version(data: &[u8]) -> RemoteVersion {
    let hash = digest::digest(&SHA256, data);
    RemoteVersion(hash.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// How a sync treats the remote file's encryption.
#[derive(Clone, Copy)]
enum RemoteKey<'a> {
    /// Same credentials as the working copy.
    Same,
    /// Decrypt with these credentials, then replace the remote with the working copy's key.
    Replace(&'a Credentials),
}

impl Vault {
    /// Creates a working copy at `path` from the backend's remote file.
    ///
    /// This is how a second device joins a vault.
    pub fn join(
        path: &Path,
        credentials: &Credentials,
        backend: &dyn SyncBackend,
    ) -> Result<Vault> {
        if path.exists() {
            return Err(Error::AlreadyExists(path.to_owned()));
        }
        let remote = backend.get()?.ok_or(Error::RemoteMissing)?;
        key::parse_salt(&remote.data)?;
        let tmp = TempDb::new(path, "join");
        write_private(tmp.path(), &remote.data)?;
        fs::rename(tmp.path(), path)?;
        Vault::open(path, credentials).inspect_err(|_| {
            crate::vault::remove_db_files(path);
            let _ = fs::remove_file(LocalState::path_for(path));
        })
    }

    /// Fetches, merges and, if the remote lacks anything, pushes a new snapshot.
    ///
    /// Retries on a lost race; [`Error::RemoteKeyChanged`] means the master password changed.
    pub fn sync(&mut self, backend: &dyn SyncBackend) -> Result<SyncReport> {
        self.run_sync(backend, RemoteKey::Same)
    }

    /// Pushes the working copy's credentials after
    /// [`change_credentials`](Self::change_credentials), reading the remote with the `old` ones.
    pub fn push_new_credentials(
        &mut self,
        backend: &dyn SyncBackend,
        old: &Credentials,
    ) -> Result<SyncReport> {
        self.run_sync(backend, RemoteKey::Replace(old))
    }

    /// Follows a password change made on another device: `new` must open the remote copy, and
    /// the working copy is re-encrypted to match it before syncing.
    pub fn adopt_remote_credentials(
        &mut self,
        backend: &dyn SyncBackend,
        new: &Credentials,
    ) -> Result<SyncReport> {
        let remote = backend.get()?.ok_or(Error::RemoteMissing)?;
        let key = remote_key(&remote.data, new)?;
        // Prove `new` opens the file before touching the working copy.
        let tmp = TempDb::new(&self.path, "adopt");
        write_private(tmp.path(), &remote.data)?;
        self.merge_file(tmp.path(), &key).map_err(|e| match e {
            Error::RemoteCorrupt => Error::WrongKey,
            other => other,
        })?;
        self.reencrypt(key)?;
        self.local.clear_sync_cursors()?;
        self.sync(backend)
    }

    fn run_sync(&mut self, backend: &dyn SyncBackend, mode: RemoteKey<'_>) -> Result<SyncReport> {
        let id = backend.id();
        let mut report = SyncReport::default();
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
            }
            if matches!(mode, RemoteKey::Same) && self.is_up_to_date(backend, &id)? {
                debug!("sync: already up to date");
                return Ok(report);
            }
            match self.sync_once(backend, &id, mode, &mut report) {
                Err(Error::Backend(BackendError::Conflict)) => {
                    debug!("sync: lost a compare-and-swap race, retrying");
                    report.conflicts_retried += 1;
                },
                Ok(()) => return Ok(report),
                Err(e) => return Err(e),
            }
        }
        Err(Error::SyncContention(MAX_ATTEMPTS))
    }

    fn is_up_to_date(&mut self, backend: &dyn SyncBackend, id: &str) -> Result<bool> {
        let Some(cursor) = self.local.sync_cursor(id)? else { return Ok(false) };
        let Some(version) = cursor.version else { return Ok(false) };
        if self.digest()? != cursor.digest {
            return Ok(false);
        }
        let remote_unchanged = backend.head()?.is_some_and(|head| head == RemoteVersion(version));
        Ok(remote_unchanged && backend.conflict_copies()?.is_empty())
    }

    fn sync_once(
        &mut self,
        backend: &dyn SyncBackend,
        id: &str,
        mode: RemoteKey<'_>,
        report: &mut SyncReport,
    ) -> Result<()> {
        let session_key = self.session()?.key.clone();
        let remote = backend.get()?;
        let mut remote_digest = None;
        if let Some(object) = &remote {
            let key = match mode {
                RemoteKey::Same => session_key.clone(),
                // A repeated push finds the remote already re-encrypted.
                RemoteKey::Replace(_) if object.data.starts_with(&session_key.salt()) => {
                    session_key.clone()
                },
                RemoteKey::Replace(old) => remote_key(&object.data, old)?,
            };
            let (merged, digest) = self.merge_bytes(&object.data, &key)?;
            report.pulled = true;
            report.changed_locally |= merged;
            remote_digest = Some(digest);
        }

        let mut merged_copies = Vec::new();
        for copy in backend.conflict_copies()? {
            match self.merge_bytes(&copy.data, &session_key) {
                Ok((merged, _)) => {
                    report.changed_locally |= merged;
                    merged_copies.push(copy.name);
                },
                // Some other vault or password: leave it for the user.
                Err(
                    Error::RemoteKeyChanged
                    | Error::RemoteCorrupt
                    | Error::VaultMismatch
                    | Error::NotAVault,
                ) => {
                    warn!("sync: skipping a conflict copy that does not belong to this vault");
                },
                Err(e) => return Err(e),
            }
        }

        let digest = self.digest()?;
        let replace = matches!(mode, RemoteKey::Replace(_));
        let version = if remote_digest.as_deref() == Some(digest.as_str()) && !replace {
            remote.as_ref().map(|object| object.version.clone())
        } else {
            let tmp = TempDb::new(&self.path, "snapshot");
            self.snapshot(tmp.path())?;
            let bytes = fs::read(tmp.path())?;
            let expected = remote.as_ref().map(|object| &object.version);
            let version = backend.put_if_match(&bytes, expected)?;
            report.pushed = true;
            version
        };

        for name in merged_copies {
            backend.remove_conflict_copy(&name)?;
            report.conflict_copies_merged += 1;
        }
        let cursor = SyncCursor { version: version.map(|v| v.0), digest };
        self.local.set_sync_cursor(id, &cursor)
    }

    fn merge_bytes(&mut self, bytes: &[u8], key: &VaultKey) -> Result<(bool, String)> {
        let tmp = TempDb::new(&self.path, "remote");
        write_private(tmp.path(), bytes)?;
        let (report, digest) = self.merge_file(tmp.path(), key)?;
        Ok((report.changed, digest))
    }
}

fn remote_key(data: &[u8], credentials: &Credentials) -> Result<VaultKey> {
    let (salt, params) = key::parse_salt(data).map_err(|_| Error::RemoteCorrupt)?;
    credentials.derive(salt, params)
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = key::create_private(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}
