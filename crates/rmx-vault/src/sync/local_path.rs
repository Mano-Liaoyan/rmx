//! A file in a local directory, typically one watched by Syncthing or similar.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use super::{ConflictCopy, RemoteObject, RemoteVersion, SyncBackend, content_version};
use crate::error::BackendError;
use crate::records::Id;

/// Syncs through a file at a local path.
///
/// Versions are content hashes and writes are atomic renames, so a directory synchronizer never
/// sees a partial file. Syncthing's `*.sync-conflict-*` siblings are reported as conflict copies.
/// Hash comparison narrows but cannot close the race against an external synchronizer; whatever
/// slips through surfaces as a conflict copy and is merged on the next sync.
#[derive(Clone, Debug)]
pub struct LocalPathBackend {
    path: PathBuf,
}

impl LocalPathBackend {
    /// A backend for the file at `path`. Its directory must exist.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> Result<Option<Vec<u8>>, BackendError> {
        match fs::read(&self.path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn check(&self, expected: Option<&RemoteVersion>) -> Result<(), BackendError> {
        if self.read()?.map(|data| content_version(&data)).as_ref() == expected {
            Ok(())
        } else {
            Err(BackendError::Conflict)
        }
    }

    fn parent(&self) -> &Path {
        match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    fn file_name(&self) -> String {
        self.path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    fn is_conflict_copy(&self, candidate: &str) -> bool {
        let name = self.file_name();
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name.as_str(), ""));
        candidate.starts_with(&format!("{stem}.sync-conflict-"))
            && (ext.is_empty() || candidate.ends_with(&format!(".{ext}")))
    }
}

impl SyncBackend for LocalPathBackend {
    fn id(&self) -> String {
        format!("file:{}", self.path.display())
    }

    fn head(&self) -> Result<Option<RemoteVersion>, BackendError> {
        Ok(self.read()?.map(|data| content_version(&data)))
    }

    fn get(&self) -> Result<Option<RemoteObject>, BackendError> {
        Ok(self.read()?.map(|data| RemoteObject { version: content_version(&data), data }))
    }

    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError> {
        self.check(expected)?;
        // Syncthing ignores this name, so it never transfers the partial file.
        let tmp = self.parent().join(format!(".syncthing.{}.{}.tmp", self.file_name(), Id::new()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            self.check(expected)?;
            fs::rename(&tmp, &self.path)?;
            File::open(self.parent())?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map(|()| Some(content_version(data)))
    }

    fn conflict_copies(&self) -> Result<Vec<ConflictCopy>, BackendError> {
        let mut copies = Vec::new();
        for entry in fs::read_dir(self.parent())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.is_conflict_copy(&name) {
                copies.push(ConflictCopy { data: fs::read(entry.path())?, name });
            }
        }
        copies.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(copies)
    }

    fn remove_conflict_copy(&self, name: &str) -> Result<(), BackendError> {
        if !self.is_conflict_copy(name) {
            return Err(BackendError::Protocol("not a conflict copy of this vault".to_owned()));
        }
        fs::remove_file(self.parent().join(name))?;
        Ok(())
    }
}
