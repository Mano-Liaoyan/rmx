//! Unlocking from the system keyring instead of the master password (feature `keyring`).

use std::path::Path;

use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::key::{self, VaultKey};
use crate::local_state::LocalState;
use crate::vault::Vault;

const SERVICE: &str = "rmx-vault";

/// Somewhere to keep a vault key between sessions.
pub trait KeyStore {
    /// Stores `key` under `account`, replacing any previous value.
    fn store(&self, account: &str, key: &[u8]) -> Result<()>;
    /// Reads the key stored under `account`.
    fn load(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>>;
    /// Forgets the key stored under `account`.
    fn clear(&self, account: &str) -> Result<()>;
}

/// The operating system's keyring.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsKeyring;

fn keyring_error(e: keyring::Error) -> Error {
    Error::Keyring(e.to_string())
}

impl KeyStore for OsKeyring {
    fn store(&self, account: &str, key: &[u8]) -> Result<()> {
        keyring::Entry::new(SERVICE, account).and_then(|e| e.set_secret(key)).map_err(keyring_error)
    }

    fn load(&self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(keyring_error)?;
        match entry.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error(e)),
        }
    }

    fn clear(&self, account: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(keyring_error)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_error(e)),
        }
    }
}

fn account(local: &LocalState) -> Result<String> {
    Ok(format!("{:016x}", local.node_id()?))
}

impl Vault {
    /// Saves the unlocked vault's key to `store` for [`open_quick`](Self::open_quick).
    pub fn enable_quick_unlock(&mut self, store: &dyn KeyStore) -> Result<()> {
        self.require_encrypted()?;
        let account = account(&self.local)?;
        let key = self.session()?.key.clone().ok_or(Error::Unencrypted)?;
        store.store(&account, key.raw())
    }

    /// Forgets the stored key.
    pub fn disable_quick_unlock(&self, store: &dyn KeyStore) -> Result<()> {
        store.clear(&account(&self.local)?)
    }

    /// Opens the working copy at `path` with the key saved in `store`.
    ///
    /// Fails with [`Error::NoQuickUnlock`] if none is saved, and [`Error::WrongKey`] if it went
    /// stale because the master password changed.
    pub fn open_quick(path: &Path, store: &dyn KeyStore) -> Result<Vault> {
        let local = LocalState::open(&LocalState::path_for(path))?;
        let raw = store.load(&account(&local)?)?.ok_or(Error::NoQuickUnlock)?;
        let (salt, _) = key::parse_salt(&key::read_salt(path)?)?;
        let key = VaultKey::from_raw(&raw, salt).ok_or(Error::WrongKey)?;
        Vault::open_with_key(path, key)
    }
}
