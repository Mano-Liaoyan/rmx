//! Vaults with no master password: plain SQLite files, kept apart from encrypted ones.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{MemoryBackend, creds, fast_kdf};
use rmx_vault::{
    Credential, Credentials, Error, Folder, Id, Secret, SyncBackend, Vault, VaultKind,
};

const MARKER: &str = "plaintext-marker-b2c9";

fn folder(name: &str) -> Folder {
    Folder { name: name.to_owned(), ..Default::default() }
}

fn names(vault: &mut Vault) -> Vec<String> {
    let mut names: Vec<String> =
        vault.list::<Folder>().unwrap().into_iter().map(|f| f.name).collect();
    names.sort();
    names
}

fn plain(path: &Path) -> Vault {
    Vault::create_plain(path).unwrap()
}

#[test]
fn a_plain_vault_is_an_ordinary_sqlite_file_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = plain(&path);
    assert_eq!(vault.kind(), VaultKind::Plain);
    vault.put(&Credential { secret: Secret::new(MARKER), ..Default::default() }).unwrap();
    let snapshot = dir.path().join("snap");
    vault.snapshot(&snapshot).unwrap();
    drop(vault);

    assert_eq!(VaultKind::of_file(&path).unwrap(), Some(VaultKind::Plain));
    let bytes = std::fs::read(&snapshot).unwrap();
    assert_eq!(&bytes[..16], b"SQLite format 3\0");
    assert!(
        bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
        "it must not look encrypted"
    );
}

#[test]
fn it_opens_without_credentials_and_keeps_what_was_put_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let id = Id::new();
    let mut vault = plain(&path);
    vault.put(&Folder { id, name: "kept".to_owned(), ..Default::default() }).unwrap();
    drop(vault);
    let mut vault = Vault::open_plain(&path).unwrap();
    assert_eq!(vault.kind(), VaultKind::Plain);
    assert_eq!(vault.get::<Folder>(id).unwrap().unwrap().name, "kept");
}

#[test]
fn an_encrypted_vault_is_not_taken_for_a_plain_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    assert_eq!(vault.kind(), VaultKind::Encrypted);
    vault.put(&folder("x")).unwrap();
    drop(vault);
    assert_eq!(VaultKind::of_file(&path).unwrap(), Some(VaultKind::Encrypted));
    assert!(matches!(Vault::open_plain(&path), Err(Error::NotAVault)));
    assert_eq!(names(&mut Vault::open(&path, &creds()).unwrap()), ["x"]);
}

#[test]
fn another_programs_sqlite_file_is_not_a_vault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('hello');")
        .unwrap();
    drop(conn);
    assert!(matches!(Vault::open_plain(&path), Err(Error::NotAVault)));
}

#[test]
fn empty_credentials_never_make_a_plain_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let result = Vault::create(&path, &Credentials::new(""), fast_kdf());
    assert!(matches!(result, Err(Error::InvalidKeyParams(_))), "{result:?}");
    assert_ne!(VaultKind::of_file(&path).unwrap(), Some(VaultKind::Plain));
}

#[test]
fn it_cannot_be_locked_or_time_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut vault = plain(&dir.path().join("v.rmxdb"));
    vault.put(&folder("x")).unwrap();
    vault.lock();
    assert!(!vault.is_locked());
    vault.set_idle_timeout(Some(Duration::ZERO));
    assert!(!vault.is_locked());
    assert_eq!(names(&mut vault), ["x"]);
    vault.unlock(&creds()).unwrap();
}

#[test]
fn what_needs_a_password_says_there_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut vault = plain(&dir.path().join("v.rmxdb"));
    assert!(matches!(vault.change_credentials(&creds(), fast_kdf()), Err(Error::Unencrypted)));
    let backend = MemoryBackend::default();
    assert!(matches!(vault.push_new_credentials(&backend, &creds()), Err(Error::Unencrypted)));
    assert!(matches!(vault.adopt_remote_credentials(&backend, &creds()), Err(Error::Unencrypted)));
}

#[test]
fn two_plain_copies_sync_and_converge() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let mut a = plain(&dir.path().join("a.rmxdb"));
    a.put(&folder("from-a")).unwrap();
    a.sync(backend.as_ref()).unwrap();
    assert_eq!(VaultKind::of_bytes(&backend.get().unwrap().unwrap().data), VaultKind::Plain);

    let mut b = Vault::join_plain(&dir.path().join("b.rmxdb"), backend.as_ref()).unwrap();
    assert_eq!(b.kind(), VaultKind::Plain);
    assert_eq!(names(&mut b), ["from-a"]);
    b.put(&folder("from-b")).unwrap();
    b.sync(backend.as_ref()).unwrap();

    a.put(&folder("from-a-again")).unwrap();
    a.sync(backend.as_ref()).unwrap();
    b.sync(backend.as_ref()).unwrap();
    assert_eq!(names(&mut a), ["from-a", "from-a-again", "from-b"]);
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
}

/// A sync that has to stop: neither side may have changed.
fn assert_refused(vault: &mut Vault, backend: &MemoryBackend, local: VaultKind, remote: VaultKind) {
    let before = (vault.digest().unwrap(), backend.get().unwrap().unwrap().data);
    let puts = backend.counts().1;
    let result = vault.sync(backend);
    let Err(Error::KindMismatch { local: found_local, remote: found_remote }) = result else {
        panic!("{result:?}");
    };
    assert_eq!((found_local, found_remote), (local, remote));
    assert_eq!(backend.counts().1, puts, "the remote was overwritten");
    assert_eq!(backend.get().unwrap().unwrap().data, before.1, "the remote changed");
    assert_eq!(vault.digest().unwrap(), before.0, "the working copy changed");
}

#[test]
fn a_plain_copy_refuses_an_encrypted_remote() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut other = Vault::create(&dir.path().join("e.rmxdb"), &creds(), fast_kdf()).unwrap();
    other.put(&folder("secret")).unwrap();
    other.sync(&backend).unwrap();

    let mut vault = plain(&dir.path().join("p.rmxdb"));
    vault.put(&folder("mine")).unwrap();
    assert_refused(&mut vault, &backend, VaultKind::Plain, VaultKind::Encrypted);
    assert_eq!(names(&mut vault), ["mine"]);
}

#[test]
fn an_encrypted_copy_refuses_a_plain_remote() {
    let dir = tempfile::tempdir().unwrap();
    let backend = MemoryBackend::default();
    let mut other = plain(&dir.path().join("p.rmxdb"));
    other.put(&folder("open")).unwrap();
    other.sync(&backend).unwrap();

    let mut vault = Vault::create(&dir.path().join("e.rmxdb"), &creds(), fast_kdf()).unwrap();
    vault.put(&folder("mine")).unwrap();
    assert_refused(&mut vault, &backend, VaultKind::Encrypted, VaultKind::Plain);
}

#[test]
fn the_message_says_which_side_is_which() {
    let plain_local = Error::KindMismatch { local: VaultKind::Plain, remote: VaultKind::Encrypted };
    let encrypted_local =
        Error::KindMismatch { local: VaultKind::Encrypted, remote: VaultKind::Plain };
    assert!(plain_local.to_string().contains("remote copy is encrypted"));
    assert!(encrypted_local.to_string().contains("remote copy is unencrypted"));
}

#[test]
fn snapshots_of_the_other_kind_do_not_merge() {
    let dir = tempfile::tempdir().unwrap();
    let mut encrypted = Vault::create(&dir.path().join("e.rmxdb"), &creds(), fast_kdf()).unwrap();
    let mut open = plain(&dir.path().join("p.rmxdb"));
    let (to_plain, to_encrypted) = (dir.path().join("to-plain"), dir.path().join("to-encrypted"));
    encrypted.snapshot(&to_plain).unwrap();
    open.snapshot(&to_encrypted).unwrap();
    assert!(matches!(open.merge_snapshot(&to_plain), Err(Error::KindMismatch { .. })));
    assert!(matches!(encrypted.merge_snapshot(&to_encrypted), Err(Error::KindMismatch { .. })));
}

#[test]
fn joining_needs_the_right_call_for_the_kind_of_remote() {
    let dir = tempfile::tempdir().unwrap();
    let plain_backend = MemoryBackend::default();
    plain(&dir.path().join("p.rmxdb")).sync(&plain_backend).unwrap();
    let encrypted_backend = MemoryBackend::default();
    Vault::create(&dir.path().join("e.rmxdb"), &creds(), fast_kdf())
        .unwrap()
        .sync(&encrypted_backend)
        .unwrap();

    let path = dir.path().join("joined.rmxdb");
    assert!(matches!(
        Vault::join(&path, &creds(), &plain_backend),
        Err(Error::KindMismatch { local: VaultKind::Encrypted, remote: VaultKind::Plain })
    ));
    assert!(!path.exists());
    assert!(matches!(
        Vault::join_plain(&path, &encrypted_backend),
        Err(Error::KindMismatch { local: VaultKind::Plain, remote: VaultKind::Encrypted })
    ));
    assert!(!path.exists());
}

#[test]
fn a_conflict_copy_of_the_other_kind_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let synced = dir.path().join("synced");
    std::fs::create_dir(&synced).unwrap();
    let backend = rmx_vault::LocalPathBackend::new(synced.join("v.rmxdb"));

    let mut vault = plain(&dir.path().join("p.rmxdb"));
    vault.put(&folder("mine")).unwrap();
    vault.sync(&backend).unwrap();

    let copy = synced.join("v.sync-conflict-20260101-000000-ABCDEFG.rmxdb");
    let mut other = Vault::create(&dir.path().join("e.rmxdb"), &creds(), fast_kdf()).unwrap();
    other.snapshot(&copy).unwrap();
    let bytes = std::fs::read(&copy).unwrap();

    vault.put(&folder("more")).unwrap();
    let report = vault.sync(&backend).unwrap();
    assert_eq!(report.conflict_copies_merged, 0);
    assert_eq!(std::fs::read(&copy).unwrap(), bytes);
    assert_eq!(names(&mut vault), ["mine", "more"]);
}
