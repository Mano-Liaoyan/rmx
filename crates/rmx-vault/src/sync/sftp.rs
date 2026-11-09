//! SFTP, which has no compare-and-swap: a lock file plus a hash re-check under the lock.

use std::fmt;

use super::{RemoteObject, RemoteVersion, SyncBackend, content_version};
use crate::error::BackendError;
use crate::hlc::system_time_ms;
use crate::records::Id;

const LOCK_TTL_MS: u64 = 60_000;

/// The remote file operations [`SftpBackend`] needs, so this crate need not depend on `rmx-ssh`.
pub trait RemoteFiles: Send + Sync {
    /// Reads a whole file, or `None` if it does not exist.
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, BackendError>;

    /// Creates or truncates a file and writes `data`.
    fn write(&self, path: &str, data: &[u8]) -> Result<(), BackendError>;

    /// Creates a file only if absent (`SSH_FXF_CREAT | SSH_FXF_EXCL`); `false` if it existed.
    fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, BackendError>;

    /// Renames, atomically replacing `to` (`posix-rename@openssh.com`).
    fn rename_replace(&self, from: &str, to: &str) -> Result<(), BackendError>;

    /// Deletes a file; absence is not an error.
    fn remove(&self, path: &str) -> Result<(), BackendError>;
}

/// Syncs through one file over SFTP.
///
/// Writers take `<path>.rmxlock`, re-check the content hash, upload to a temporary name and
/// rename into place. A lock older than a minute is treated as abandoned, using this device's
/// clock, so devices with wildly wrong clocks or uploads longer than that can still race.
pub struct SftpBackend<F> {
    files: F,
    label: String,
    path: String,
}

impl<F> fmt::Debug for SftpBackend<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpBackend").field("label", &self.label).finish_non_exhaustive()
    }
}

impl<F: RemoteFiles> SftpBackend<F> {
    /// A backend for `path` on `files`. `label` names the server in the sync cursor, for example
    /// `user@host:22`.
    pub fn new(files: F, label: impl Into<String>, path: impl Into<String>) -> Self {
        Self { files, label: label.into(), path: path.into() }
    }

    fn lock_path(&self) -> String {
        format!("{}.rmxlock", self.path)
    }

    fn acquire(&self) -> Result<Lock<'_, F>, BackendError> {
        let token = format!("{} {}", Id::new(), system_time_ms() + LOCK_TTL_MS);
        let path = self.lock_path();
        let acquired = if self.files.create_exclusive(&path, token.as_bytes())? {
            true
        } else if self.files.read(&path)?.as_deref().is_some_and(lock_is_live) {
            false
        } else {
            self.files.remove(&path)?;
            self.files.create_exclusive(&path, token.as_bytes())?
        };
        if !acquired {
            return Err(BackendError::Conflict);
        }
        let lock = Lock { files: &self.files, path, token };
        lock.ensure_held()?;
        Ok(lock)
    }
}

fn lock_is_live(body: &[u8]) -> bool {
    let expiry = std::str::from_utf8(body)
        .ok()
        .and_then(|text| text.split_once(' '))
        .and_then(|(_, expiry)| expiry.parse::<u64>().ok());
    expiry.is_some_and(|expiry| expiry > system_time_ms())
}

struct Lock<'a, F: RemoteFiles> {
    files: &'a F,
    path: String,
    token: String,
}

impl<F: RemoteFiles> Lock<'_, F> {
    fn is_held(&self) -> Result<bool, BackendError> {
        Ok(self.files.read(&self.path)?.as_deref() == Some(self.token.as_bytes()))
    }

    fn ensure_held(&self) -> Result<(), BackendError> {
        if self.is_held()? { Ok(()) } else { Err(BackendError::Conflict) }
    }
}

impl<F: RemoteFiles> Drop for Lock<'_, F> {
    fn drop(&mut self) {
        if matches!(self.is_held(), Ok(true)) {
            let _ = self.files.remove(&self.path);
        }
    }
}

impl<F: RemoteFiles> SyncBackend for SftpBackend<F> {
    fn id(&self) -> String {
        format!("sftp:{}:{}", self.label, self.path)
    }

    fn head(&self) -> Result<Option<RemoteVersion>, BackendError> {
        Ok(self.files.read(&self.path)?.map(|data| content_version(&data)))
    }

    fn get(&self) -> Result<Option<RemoteObject>, BackendError> {
        Ok(self
            .files
            .read(&self.path)?
            .map(|data| RemoteObject { version: content_version(&data), data }))
    }

    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError> {
        let lock = self.acquire()?;
        if self.head()?.as_ref() != expected {
            return Err(BackendError::Conflict);
        }
        let tmp = format!("{}.{}.tmp", self.path, Id::new());
        self.files.write(&tmp, data)?;
        let result = lock.ensure_held().and_then(|()| self.files.rename_replace(&tmp, &self.path));
        if result.is_err() {
            let _ = self.files.remove(&tmp);
        }
        result.map(|()| Some(content_version(data)))
    }
}
