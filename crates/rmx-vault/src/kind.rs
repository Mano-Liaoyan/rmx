//! Whether a vault file is encrypted, which its first bytes say.
//!
//! A plain vault starts with SQLite's own header. An encrypted one starts with the salt rmx wrote
//! (see `key.rs`), whose first byte is a small version number, so the two cannot be confused.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;

use crate::error::Result;

const PLAIN_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// What kind of file a vault is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultKind {
    /// SQLCipher, opened with the master password and key file.
    Encrypted,
    /// Plain SQLite: everything in it can be read by anyone who can read the file.
    Plain,
}

impl VaultKind {
    /// The kind of a file starting with `bytes`. Anything without SQLite's header counts as
    /// encrypted, and opening it says whether it is one of ours.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        if bytes.starts_with(PLAIN_HEADER) { Self::Plain } else { Self::Encrypted }
    }

    /// The kind of the file at `path`; `None` when there is no such file or it is empty, which
    /// is what a vault not yet created looks like.
    pub fn of_file(path: &Path) -> Result<Option<Self>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut head = Vec::with_capacity(PLAIN_HEADER.len());
        file.take(PLAIN_HEADER.len() as u64).read_to_end(&mut head)?;
        Ok((!head.is_empty()).then(|| Self::of_bytes(&head)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_decides() {
        assert_eq!(VaultKind::of_bytes(b"SQLite format 3\0 and the rest"), VaultKind::Plain);
        assert_eq!(VaultKind::of_bytes(b"SQLite format 3\0"), VaultKind::Plain);
        // The salt of an encrypted vault, and things that only look like the header.
        assert_eq!(VaultKind::of_bytes(&[1, 17, 3, 1, 9, 9, 9, 9]), VaultKind::Encrypted);
        assert_eq!(VaultKind::of_bytes(b"SQLite format 3"), VaultKind::Encrypted);
        assert_eq!(VaultKind::of_bytes(b"SQLite format 4\0"), VaultKind::Encrypted);
        assert_eq!(VaultKind::of_bytes(b""), VaultKind::Encrypted);
    }

    #[test]
    fn a_missing_or_empty_file_is_no_vault_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v");
        assert_eq!(VaultKind::of_file(&path).unwrap(), None);
        std::fs::write(&path, b"").unwrap();
        assert_eq!(VaultKind::of_file(&path).unwrap(), None);
        std::fs::write(&path, b"short").unwrap();
        assert_eq!(VaultKind::of_file(&path).unwrap(), Some(VaultKind::Encrypted));
        std::fs::write(&path, b"SQLite format 3\0more").unwrap();
        assert_eq!(VaultKind::of_file(&path).unwrap(), Some(VaultKind::Plain));
    }
}
