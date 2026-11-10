//! The local-path and SFTP backends, which need no server.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::{creds, fast_kdf};
use rmx_vault::{
    BackendError, Folder, LocalPathBackend, RemoteFiles, RemoteVersion, SftpBackend, SyncBackend,
    Vault,
};

fn folder(name: &str) -> Folder {
    Folder { name: name.to_owned(), ..Default::default() }
}

#[test]
fn local_path_compare_and_swap() {
    let dir = tempfile::tempdir().unwrap();
    let backend = LocalPathBackend::new(dir.path().join("vault.rmxdb"));
    assert!(backend.get().unwrap().is_none());
    let v1 = backend.put_if_match(b"one", None).unwrap().unwrap();
    assert_eq!(backend.head().unwrap().unwrap(), v1);
    assert!(matches!(backend.put_if_match(b"x", None), Err(BackendError::Conflict)));
    let stale = RemoteVersion("stale".to_owned());
    assert!(matches!(backend.put_if_match(b"x", Some(&stale)), Err(BackendError::Conflict)));
    let v2 = backend.put_if_match(b"two", Some(&v1)).unwrap().unwrap();
    assert_ne!(v1, v2);
    assert_eq!(backend.get().unwrap().unwrap().data, b"two");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "no temporary files stay behind");
}

#[test]
fn local_path_put_detects_an_external_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.rmxdb");
    let backend = LocalPathBackend::new(&path);
    let v1 = backend.put_if_match(b"one", None).unwrap().unwrap();
    std::fs::write(&path, b"changed by syncthing").unwrap();
    assert!(matches!(backend.put_if_match(b"two", Some(&v1)), Err(BackendError::Conflict)));
}

#[test]
fn local_path_end_to_end_merges_syncthing_conflict_files() {
    let dir = tempfile::tempdir().unwrap();
    let sync_dir = dir.path().join("synced");
    std::fs::create_dir(&sync_dir).unwrap();
    let remote_path = sync_dir.join("vault.rmxdb");
    let backend = LocalPathBackend::new(&remote_path);
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(&backend).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), &backend).unwrap();

    // Syncthing saw both devices write, kept one and renamed the other.
    let only_b = folder("only in the conflict copy");
    b.put(&only_b).unwrap();
    b.snapshot(&sync_dir.join("vault.sync-conflict-20260101-120000-ABCDEFG.rmxdb")).unwrap();
    std::fs::write(sync_dir.join("other.sync-conflict-20260101-120000-ABCDEFG.rmxdb"), b"keep")
        .unwrap();

    // A vault under a different password must be skipped, not deleted.
    let stranger_path = dir.path().join("stranger.rmxdb");
    let mut stranger =
        Vault::create(&stranger_path, &rmx_vault::Credentials::new("other"), fast_kdf()).unwrap();
    let foreign = sync_dir.join("vault.sync-conflict-20260202-120000-HIJKLMN.rmxdb");
    stranger.snapshot(&foreign).unwrap();

    let report = a.sync(&backend).unwrap();
    assert_eq!(report.conflict_copies_merged, 1);
    assert!(report.pushed);
    assert!(a.get::<Folder>(only_b.id).unwrap().is_some());
    assert!(!sync_dir.join("vault.sync-conflict-20260101-120000-ABCDEFG.rmxdb").exists());
    assert!(sync_dir.join("other.sync-conflict-20260101-120000-ABCDEFG.rmxdb").exists());
    assert!(foreign.exists());

    b.sync(&backend).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert!(
        !std::fs::read_dir(&sync_dir).unwrap().any(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.ends_with("-wal") || name.ends_with("-shm")
        }),
        "WAL files must never reach the sync directory"
    );
}

#[derive(Clone, Default)]
struct MemoryFiles(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl RemoteFiles for MemoryFiles {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, BackendError> {
        Ok(self.0.lock().unwrap().get(path).cloned())
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), BackendError> {
        self.0.lock().unwrap().insert(path.to_owned(), data.to_vec());
        Ok(())
    }

    fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, BackendError> {
        let mut files = self.0.lock().unwrap();
        if files.contains_key(path) {
            return Ok(false);
        }
        files.insert(path.to_owned(), data.to_vec());
        Ok(true)
    }

    fn rename_replace(&self, from: &str, to: &str) -> Result<(), BackendError> {
        let mut files = self.0.lock().unwrap();
        let data = files.remove(from).ok_or_else(|| BackendError::Protocol("gone".to_owned()))?;
        files.insert(to.to_owned(), data);
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), BackendError> {
        self.0.lock().unwrap().remove(path);
        Ok(())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[test]
fn sftp_rechecks_the_hash_under_the_lock() {
    let files = MemoryFiles::default();
    let backend = SftpBackend::new(files.clone(), "test", "/v.rmxdb");
    let v1 = backend.put_if_match(b"one", None).unwrap().unwrap();
    files.write("/v.rmxdb", b"sneaky").unwrap();
    assert!(matches!(backend.put_if_match(b"two", Some(&v1)), Err(BackendError::Conflict)));
    assert!(files.read("/v.rmxdb.rmxlock").unwrap().is_none(), "the lock must be released");
    assert_eq!(files.0.lock().unwrap().len(), 1, "no temporary files stay behind");
}

#[test]
fn sftp_live_lock_means_conflict() {
    let files = MemoryFiles::default();
    let backend = SftpBackend::new(files.clone(), "test", "/v.rmxdb");
    let body = format!("someone {}", now_ms() + 30_000);
    files.write("/v.rmxdb.rmxlock", body.as_bytes()).unwrap();
    assert!(matches!(backend.put_if_match(b"x", None), Err(BackendError::Conflict)));
    assert_eq!(files.read("/v.rmxdb.rmxlock").unwrap().unwrap(), body.as_bytes());
}

#[test]
fn sftp_stale_lock_is_broken() {
    let files = MemoryFiles::default();
    let backend = SftpBackend::new(files.clone(), "test", "/v.rmxdb");
    files.write("/v.rmxdb.rmxlock", format!("dead {}", now_ms() - 1).as_bytes()).unwrap();
    backend.put_if_match(b"x", None).unwrap();
    assert_eq!(files.read("/v.rmxdb").unwrap().unwrap(), b"x");
    assert!(files.read("/v.rmxdb.rmxlock").unwrap().is_none());
}

#[test]
fn sftp_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let backend = SftpBackend::new(MemoryFiles::default(), "user@host:22", "/srv/v.rmxdb");
    assert_eq!(backend.id(), "sftp:user@host:22:/srv/v.rmxdb");
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.put(&folder("a")).unwrap();
    a.sync(&backend).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), &backend).unwrap();
    b.put(&folder("b")).unwrap();
    b.sync(&backend).unwrap();
    a.sync(&backend).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert_eq!(a.list::<Folder>().unwrap().len(), 2);
}

#[test]
fn a_password_change_goes_through_a_local_folder() {
    let dir = tempfile::tempdir().unwrap();
    let backend = LocalPathBackend::new(dir.path().join("vault.rmxdb"));
    let new = rmx_vault::Credentials::new("rotated password");
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(&backend).unwrap();
    a.change_credentials(&new, fast_kdf()).unwrap();
    a.push_new_credentials(&backend, &creds()).unwrap();
    a.sync(&backend).unwrap();
    drop(a);
    Vault::join(&dir.path().join("b.rmxdb"), &new, &backend).unwrap();
}
