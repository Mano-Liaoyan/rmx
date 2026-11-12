//! The fetch-merge-push loop against an in-memory compare-and-swap store.

mod common;

use std::sync::Arc;

use common::{MemoryBackend, creds, fast_kdf, replicas};
use rmx_vault::{
    BackendError, Connection, Credentials, Error, Folder, Id, RemoteObject, RemoteVersion,
    SyncBackend, Vault,
};

fn folder(name: &str) -> Folder {
    Folder { name: name.to_owned(), ..Default::default() }
}

#[test]
fn etag_conflict_is_resolved_by_refetch_merge_retry() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(backend.as_ref()).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), backend.as_ref()).unwrap();

    let (from_a, from_b) = (folder("from a"), folder("from b"));
    a.put(&from_a).unwrap();
    b.put(&from_b).unwrap();

    // B's upload lands between A's download and A's conditional put.
    let racer = std::sync::Mutex::new(b);
    let racer = Arc::new(racer);
    let other = racer.clone();
    backend.before_next_put(move |store| {
        let report = other.lock().unwrap().sync(store).unwrap();
        assert!(report.pushed);
    });

    let report = a.sync(backend.as_ref()).unwrap();
    assert_eq!(report.conflicts_retried, 1, "A must lose exactly one race");
    assert!(report.pushed && report.changed_locally);
    let names: Vec<String> = a.list::<Folder>().unwrap().into_iter().map(|f| f.name).collect();
    assert_eq!(names.len(), 2, "A retried with B's row merged in: {names:?}");

    let mut b = Arc::into_inner(racer).unwrap().into_inner().unwrap();
    b.sync(backend.as_ref()).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert!(a.get::<Folder>(from_b.id).unwrap().is_some());
    assert!(b.get::<Folder>(from_a.id).unwrap().is_some());
}

#[test]
fn gives_up_when_every_attempt_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    backend.always_conflict();
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    assert!(matches!(a.sync(&backend), Err(Error::SyncContention(8))));
}

#[test]
fn two_offline_working_copies_converge() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    let shared = Connection {
        name: "shared".to_owned(),
        protocol: "ssh".to_owned(),
        host: "h0".to_owned(),
        ..Default::default()
    };
    let doomed = folder("doomed");
    a.put(&shared).unwrap();
    a.put(&doomed).unwrap();
    a.sync(&backend).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), &backend).unwrap();

    // Offline: A edits, deletes and adds; B edits the same row later and adds its own.
    let (only_a, only_b) = (folder("only a"), folder("only b"));
    a.put(&Connection { host: "edited by a".to_owned(), ..shared.clone() }).unwrap();
    a.delete::<Folder>(doomed.id).unwrap();
    a.put(&only_a).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    b.put(&Connection { host: "edited by b".to_owned(), ..shared.clone() }).unwrap();
    b.put(&only_b).unwrap();

    // Back online, in the "wrong" order, then once more each to settle.
    b.sync(&backend).unwrap();
    a.sync(&backend).unwrap();
    b.sync(&backend).unwrap();

    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    for vault in [&mut a, &mut b] {
        assert_eq!(vault.get::<Connection>(shared.id).unwrap().unwrap().host, "edited by b");
        assert_eq!(vault.get::<Folder>(doomed.id).unwrap(), None);
        assert!(vault.get::<Folder>(only_a.id).unwrap().is_some());
        assert!(vault.get::<Folder>(only_b.id).unwrap().is_some());
        let history = vault.history::<Connection>(shared.id).unwrap();
        assert_eq!(history[0].record.host, "edited by a", "the loser is kept in history");
    }
    // The working copies are still independent, encrypted files.
    Vault::open(&dir.path().join("a.rmxdb"), &creds()).unwrap();
}

#[test]
fn nothing_to_do_means_no_download_and_no_upload() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.put(&folder("x")).unwrap();
    let first = a.sync(&backend).unwrap();
    assert!(first.pushed && !first.pulled);

    let counts = backend.counts();
    let report = a.sync(&backend).unwrap();
    assert_eq!(report, Default::default());
    assert_eq!(backend.counts(), counts, "unchanged state must use head only");

    a.put(&folder("y")).unwrap();
    let report = a.sync(&backend).unwrap();
    assert!(report.pushed && report.pulled);
}

#[test]
fn pull_without_local_changes_does_not_push() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(&backend).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), &backend).unwrap();
    a.put(&folder("x")).unwrap();
    a.sync(&backend).unwrap();
    let report = b.sync(&backend).unwrap();
    assert!(report.pulled && report.changed_locally && !report.pushed);
}

#[test]
fn clock_skew_between_syncing_devices_cannot_lose_a_reply() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.set_time_source(|| 9_000_000_000_000).unwrap();
    let row = folder("first");
    a.put(&row).unwrap();
    a.sync(&backend).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), &backend).unwrap();
    b.set_time_source(|| 1_000).unwrap();
    b.sync(&backend).unwrap();
    b.put(&Folder { name: "reply".to_owned(), ..row.clone() }).unwrap();
    b.sync(&backend).unwrap();
    a.sync(&backend).unwrap();
    assert_eq!(a.get::<Folder>(row.id).unwrap().unwrap().name, "reply");
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
}

#[test]
fn concurrent_threads_all_land() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let mut seed = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    seed.sync(backend.as_ref()).unwrap();
    let mut handles = Vec::new();
    for t in 0..3 {
        let (path, store) = (dir.path().join(format!("t{t}.rmxdb")), backend.clone());
        handles.push(std::thread::spawn(move || {
            let mut vault = Vault::join(&path, &creds(), store.as_ref()).unwrap();
            for i in 0..4 {
                vault.put(&folder(&format!("t{t}-{i}"))).unwrap();
                vault.sync(store.as_ref()).unwrap();
            }
            vault
        }));
    }
    let mut vaults: Vec<Vault> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    vaults.push(seed);
    for vault in &mut vaults {
        vault.sync(backend.as_ref()).unwrap();
    }
    for vault in &mut vaults {
        vault.sync(backend.as_ref()).unwrap();
        assert_eq!(vault.list::<Folder>().unwrap().len(), 12);
    }
    let digest = vaults[0].digest().unwrap();
    assert!(vaults.iter_mut().all(|v| v.digest().unwrap() == digest));
}

#[test]
fn join_requires_a_remote_and_the_right_password() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let path = dir.path().join("b.rmxdb");
    assert!(matches!(Vault::join(&path, &creds(), &backend), Err(Error::RemoteMissing)));
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(&backend).unwrap();
    assert!(matches!(
        Vault::join(&path, &Credentials::new("nope"), &backend),
        Err(Error::WrongKey)
    ));
    assert!(!path.exists(), "a failed join must leave nothing behind");
    Vault::join(&path, &creds(), &backend).unwrap();
    assert!(matches!(Vault::join(&path, &creds(), &backend), Err(Error::AlreadyExists(_))));
}

#[test]
fn password_change_on_one_device_is_followed_by_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let new = Credentials::new("rotated password");
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(&backend).unwrap();
    let mut b = Vault::join(&dir.path().join("b.rmxdb"), &creds(), &backend).unwrap();

    let (from_a, from_b) = (folder("from a"), folder("from b"));
    a.put(&from_a).unwrap();
    b.put(&from_b).unwrap();

    // Until the new key is pushed, an ordinary sync must refuse, not overwrite.
    a.change_credentials(&new, fast_kdf()).unwrap();
    assert!(matches!(a.sync(&backend), Err(Error::RemoteKeyChanged)));
    let pushed = a.push_new_credentials(&backend, &creds()).unwrap();
    assert!(pushed.pushed);
    a.push_new_credentials(&backend, &creds()).unwrap();
    a.sync(&backend).unwrap();

    assert!(matches!(b.sync(&backend), Err(Error::RemoteKeyChanged)));
    assert!(matches!(
        b.adopt_remote_credentials(&backend, &Credentials::new("wrong")),
        Err(Error::WrongKey)
    ));
    b.adopt_remote_credentials(&backend, &new).unwrap();
    a.sync(&backend).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert!(b.get::<Folder>(from_a.id).unwrap().is_some());
    assert!(a.get::<Folder>(from_b.id).unwrap().is_some());
    drop(b);
    Vault::open(&dir.path().join("b.rmxdb"), &new).unwrap();
}

#[test]
fn corrupt_remote_is_reported_and_leaves_the_working_copy_alone() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.put(&folder("x")).unwrap();
    a.sync(&backend).unwrap();
    let mut remote = backend.get().unwrap().unwrap();
    let digest = a.digest().unwrap();
    for byte in &mut remote.data[16..] {
        *byte ^= 0x5a;
    }
    let version = remote.version.clone();
    backend.put_if_match(&remote.data, Some(&version)).unwrap();
    let id = Id::new();
    assert!(matches!(a.sync(&backend), Err(Error::RemoteCorrupt)));
    assert_eq!(a.digest().unwrap(), digest);
    assert_eq!(a.get::<Folder>(id).unwrap(), None);
}

#[test]
fn snapshots_are_taken_from_the_committed_state_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut vaults = replicas(dir.path(), 2);
    vaults[0].put(&folder("in the wal")).unwrap();
    let snapshot = dir.path().join("snap");
    vaults[0].snapshot(&snapshot).unwrap();
    let report = vaults[1].merge_snapshot(&snapshot).unwrap();
    assert!(report.changed, "a snapshot must include rows still in the WAL");
}

/// Stores the upload, then reports an error, as a backend that fails after its write does.
struct LandsThenFails(MemoryBackend);

impl SyncBackend for LandsThenFails {
    fn id(&self) -> String {
        self.0.id()
    }

    fn head(&self) -> Result<Option<RemoteVersion>, BackendError> {
        self.0.head()
    }

    fn get(&self) -> Result<Option<RemoteObject>, BackendError> {
        self.0.get()
    }

    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError> {
        self.0.put_if_match(data, expected)?;
        Err(BackendError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)))
    }
}

#[test]
fn a_push_that_failed_after_landing_is_told_from_one_that_did_not() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let new = Credentials::new("rotated password");
    let mut a = Vault::create(&dir.path().join("a.rmxdb"), &creds(), fast_kdf()).unwrap();
    a.sync(&backend).unwrap();
    assert!(a.remote_has_current_key(&backend).unwrap());

    a.change_credentials(&new, fast_kdf()).unwrap();
    assert!(!a.remote_has_current_key(&backend).unwrap(), "not pushed yet");
    let flaky = LandsThenFails(backend);
    assert!(a.push_new_credentials(&flaky, &creds()).is_err());
    assert!(a.remote_has_current_key(&flaky).unwrap(), "the upload got there");
    a.sync(&flaky.0).unwrap();
}
