//! Creating, opening, locking and re-keying vaults.

mod common;

use std::time::Duration;

use common::{creds, fast_kdf};
use rmx_vault::{
    Credential, Credentials, Error, Folder, Id, LocalState, Secret, Vault, generate_key_file,
};

const MARKER: &str = "plaintext-marker-5f3a";

#[test]
fn reopens_with_the_right_password() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let id = Id::new();
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    vault.put(&Folder { id, name: "kept".to_owned(), ..Default::default() }).unwrap();
    drop(vault);
    let mut vault = Vault::open(&path, &creds()).unwrap();
    assert_eq!(vault.get::<Folder>(id).unwrap().unwrap().name, "kept");
}

#[test]
fn wrong_password_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    drop(Vault::create(&path, &creds(), fast_kdf()).unwrap());
    let result = Vault::open(&path, &Credentials::new("not the password"));
    assert!(matches!(result, Err(Error::WrongKey)), "{result:?}");
}

#[test]
fn key_file_is_required_and_checked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let key_file = dir.path().join("key");
    generate_key_file(&key_file).unwrap();
    let with_key = creds().with_key_file(&key_file);
    let id = Id::new();
    let mut vault = Vault::create(&path, &with_key, fast_kdf()).unwrap();
    vault.put(&Folder { id, ..Default::default() }).unwrap();
    drop(vault);

    assert!(Vault::open(&path, &with_key).unwrap().get::<Folder>(id).unwrap().is_some());
    assert!(matches!(Vault::open(&path, &creds()), Err(Error::WrongKey)));
    let other_key = dir.path().join("other");
    generate_key_file(&other_key).unwrap();
    assert!(matches!(Vault::open(&path, &creds().with_key_file(&other_key)), Err(Error::WrongKey)));
    assert!(matches!(
        Vault::open(&path, &creds().with_key_file(dir.path().join("missing"))),
        Err(Error::Io(_))
    ));
}

#[test]
fn key_file_alone_is_enough_but_nothing_at_all_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let key_file = dir.path().join("key");
    generate_key_file(&key_file).unwrap();
    let only_key = Credentials::new("").with_key_file(&key_file);
    drop(Vault::create(&path, &only_key, fast_kdf()).unwrap());
    Vault::open(&path, &only_key).unwrap();
    let empty = dir.path().join("empty.rmxdb");
    assert!(matches!(
        Vault::create(&empty, &Credentials::new(""), fast_kdf()),
        Err(Error::InvalidKeyParams(_))
    ));
}

#[test]
fn key_file_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("key");
    generate_key_file(&key_file).unwrap();
    assert!(generate_key_file(&key_file).is_err());
}

#[test]
fn nothing_is_stored_in_plaintext_and_kdf_params_travel_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    vault
        .put(&Credential {
            name: MARKER.to_owned(),
            secret: Secret::new(MARKER),
            ..Default::default()
        })
        .unwrap();
    let snapshot = dir.path().join("snap.rmxdb");
    vault.snapshot(&snapshot).unwrap();
    drop(vault);
    for file in [&path, &snapshot] {
        let bytes = std::fs::read(file).unwrap();
        assert!(!bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()));
        assert!(!bytes.starts_with(b"SQLite format 3"));
        assert_eq!(&bytes[..4], &[1, 5, 1, 1], "header carries the Argon2id cost");
    }
    // The snapshot is a standalone vault with the same credentials and no WAL.
    assert!(!dir.path().join("snap.rmxdb-wal").exists());
    let mut opened = Vault::open(&snapshot, &creds()).unwrap();
    assert_eq!(opened.list::<Credential>().unwrap()[0].secret.expose(), MARKER);
}

#[cfg(unix)]
#[test]
fn working_copy_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    let snapshot = dir.path().join("snap");
    vault.snapshot(&snapshot).unwrap();
    for file in [path.clone(), snapshot, LocalState::path_for(&path)] {
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", file.display());
    }
}

#[test]
fn foreign_files_are_not_vaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x");
    std::fs::write(&path, b"short").unwrap();
    assert!(matches!(Vault::open(&path, &creds()), Err(Error::NotAVault)));
    std::fs::write(&path, b"SQLite format 3\0 and more bytes").unwrap();
    assert!(matches!(Vault::open(&path, &creds()), Err(Error::NotAVault)));
}

#[test]
fn creating_over_an_existing_vault_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    drop(Vault::create(&path, &creds(), fast_kdf()).unwrap());
    assert!(matches!(Vault::create(&path, &creds(), fast_kdf()), Err(Error::AlreadyExists(_))));
}

#[test]
fn lock_blocks_access_until_unlocked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    let row = Folder::default();
    vault.put(&row).unwrap();
    vault.lock();
    assert!(vault.is_locked());
    assert!(matches!(vault.get::<Folder>(row.id), Err(Error::Locked)));
    assert!(matches!(vault.unlock(&Credentials::new("nope")), Err(Error::WrongKey)));
    assert!(vault.is_locked());
    vault.unlock(&creds()).unwrap();
    assert!(vault.get::<Folder>(row.id).unwrap().is_some());
}

#[test]
fn idle_timeout_locks_without_being_polled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    vault.set_idle_timeout(Some(Duration::from_millis(20)));
    vault.list::<Folder>().unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert!(matches!(vault.list::<Folder>(), Err(Error::Locked)));
    vault.unlock(&creds()).unwrap();
    assert!(!vault.is_locked());
}

#[test]
fn changing_credentials_replaces_the_old_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let new = Credentials::new("a new password");
    let row = Folder { name: "survives".to_owned(), ..Default::default() };
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    vault.put(&row).unwrap();
    let digest = vault.digest().unwrap();
    vault.change_credentials(&new, fast_kdf()).unwrap();
    assert_eq!(vault.digest().unwrap(), digest);
    vault.put(&Folder { name: "after".to_owned(), ..Default::default() }).unwrap();
    drop(vault);
    assert!(matches!(Vault::open(&path, &creds()), Err(Error::WrongKey)));
    let mut opened = Vault::open(&path, &new).unwrap();
    assert_eq!(opened.get::<Folder>(row.id).unwrap().unwrap(), row);
    assert_eq!(opened.list::<Folder>().unwrap().len(), 2);
}

#[test]
fn local_state_is_separate_and_readable_while_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
    let digest = vault.digest().unwrap();
    vault.local_state().set("sidebar.width", "260").unwrap();
    assert_eq!(vault.digest().unwrap(), digest, "local state must not enter the vault");
    let node = vault.local_state().node_id().unwrap();
    drop(vault);

    let state = LocalState::open(&LocalState::path_for(&path)).unwrap();
    assert_eq!(state.get("sidebar.width").unwrap().as_deref(), Some("260"));
    assert_eq!(state.node_id().unwrap(), node);
    state.remove("sidebar.width").unwrap();
    assert_eq!(state.get("sidebar.width").unwrap(), None);
}

#[cfg(feature = "keyring")]
mod quick_unlock {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use rmx_vault::quick_unlock::KeyStore;
    use rmx_vault::{Error, Result, Vault};
    use zeroize::Zeroizing;

    use super::{creds, fast_kdf};

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

    impl KeyStore for MemoryStore {
        fn store(&self, account: &str, key: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(account.to_owned(), key.to_vec());
            Ok(())
        }

        fn load(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            Ok(self.0.lock().unwrap().get(account).cloned().map(Zeroizing::new))
        }

        fn clear(&self, account: &str) -> Result<()> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn stored_key_reopens_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.rmxdb");
        let store = MemoryStore::default();
        let mut vault = Vault::create(&path, &creds(), fast_kdf()).unwrap();
        assert!(matches!(Vault::open_quick(&path, &store), Err(Error::NoQuickUnlock)));
        vault.enable_quick_unlock(&store).unwrap();
        drop(vault);
        let mut vault = Vault::open_quick(&path, &store).unwrap();
        vault.digest().unwrap();
        vault.disable_quick_unlock(&store).unwrap();
        assert!(matches!(Vault::open_quick(&path, &store), Err(Error::NoQuickUnlock)));
    }
}
