//! Credentials, Argon2id key derivation and the salt-embedded file header.
//!
//! SQLCipher keeps a 16-byte salt in the clear at the start of the file. rmx chooses those bytes
//! itself, so the file alone says how to derive its key:
//!
//! | byte  | meaning                          |
//! |-------|----------------------------------|
//! | 0     | header version (1)               |
//! | 1     | log2 of the Argon2id memory, KiB |
//! | 2     | Argon2id iterations              |
//! | 3     | Argon2id parallelism             |
//! | 4..16 | random                           |
//!
//! All 16 bytes are the Argon2id salt. Tampering with them only yields a key that fails to decrypt.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use ring::digest::{self, SHA256};
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::secret::Secret;

const HEADER_VERSION: u8 = 1;
pub(crate) const SALT_LEN: usize = 16;
pub(crate) const KEY_LEN: usize = 32;

/// Argon2id cost. Stored in the file header, so each vault carries its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    memory_log2_kib: u8,
    iterations: u8,
    parallelism: u8,
}

impl KdfParams {
    /// The default for new vaults: 128 MiB, 3 passes, 1 lane.
    pub const RECOMMENDED: Self = Self { memory_log2_kib: 17, iterations: 3, parallelism: 1 };

    /// Validates a custom cost. Memory is `2^memory_log2_kib` KiB and at most 4 GiB.
    pub fn new(memory_log2_kib: u8, iterations: u8, parallelism: u8) -> Result<Self> {
        if !(3..=22).contains(&memory_log2_kib) {
            return Err(Error::InvalidKeyParams("Argon2id memory out of range"));
        }
        if iterations == 0 || !(1..=16).contains(&parallelism) {
            return Err(Error::InvalidKeyParams("Argon2id passes or lanes out of range"));
        }
        if (1u32 << memory_log2_kib) < 8 * u32::from(parallelism) {
            return Err(Error::InvalidKeyParams("Argon2id memory too small for its lanes"));
        }
        Ok(Self { memory_log2_kib, iterations, parallelism })
    }
}

impl Default for KdfParams {
    fn default() -> Self {
        Self::RECOMMENDED
    }
}

pub(crate) fn generate_salt(params: KdfParams) -> Result<[u8; SALT_LEN]> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt[4..]).map_err(|_| Error::Kdf)?;
    salt[0] = HEADER_VERSION;
    salt[1] = params.memory_log2_kib;
    salt[2] = params.iterations;
    salt[3] = params.parallelism;
    Ok(salt)
}

pub(crate) fn parse_salt(bytes: &[u8]) -> Result<([u8; SALT_LEN], KdfParams)> {
    let salt: [u8; SALT_LEN] =
        bytes.get(..SALT_LEN).and_then(|s| s.try_into().ok()).ok_or(Error::NotAVault)?;
    if salt[0] != HEADER_VERSION {
        return Err(Error::NotAVault);
    }
    let params = KdfParams::new(salt[1], salt[2], salt[3]).map_err(|_| Error::NotAVault)?;
    Ok((salt, params))
}

pub(crate) fn read_salt(path: &Path) -> Result<[u8; SALT_LEN]> {
    let mut head = [0u8; SALT_LEN];
    let mut file = File::open(path)?;
    match file.read_exact(&mut head) {
        Ok(()) => Ok(head),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Err(Error::NotAVault),
        Err(e) => Err(e.into()),
    }
}

/// What unlocks a vault: a master password and optionally a key file.
///
/// Holds only the password and the key file's *path*; the file is read during derivation.
#[derive(Clone, Debug)]
pub struct Credentials {
    password: Secret,
    key_file: Option<PathBuf>,
}

impl Credentials {
    /// Credentials made of a master password alone.
    pub fn new(password: impl Into<Secret>) -> Self {
        Self { password: password.into(), key_file: None }
    }

    /// Additionally requires the contents of the file at `path`.
    #[must_use]
    pub fn with_key_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.key_file = Some(path.into());
        self
    }

    fn composite(&self) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        if self.password.is_empty() && self.key_file.is_none() {
            return Err(Error::InvalidKeyParams("empty password without a key file"));
        }
        let mut ctx = digest::Context::new(&SHA256);
        ctx.update(b"rmx-vault-composite-v1");
        ctx.update(digest::digest(&SHA256, self.password.expose().as_bytes()).as_ref());
        match &self.key_file {
            Some(path) => {
                ctx.update(&[1]);
                ctx.update(hash_key_file(path)?.as_ref());
            },
            None => ctx.update(&[0]),
        }
        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        out.copy_from_slice(ctx.finish().as_ref());
        Ok(out)
    }

    pub(crate) fn derive(&self, salt: [u8; SALT_LEN], params: KdfParams) -> Result<VaultKey> {
        let composite = self.composite()?;
        let argon = Params::new(
            1 << params.memory_log2_kib,
            params.iterations.into(),
            params.parallelism.into(),
            Some(KEY_LEN),
        )
        .map_err(|_| Error::InvalidKeyParams("rejected by Argon2id"))?;
        let mut raw = Zeroizing::new([0u8; KEY_LEN]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, argon)
            .hash_password_into(composite.as_slice(), &salt, raw.as_mut_slice())
            .map_err(|_| Error::Kdf)?;
        Ok(VaultKey { raw, salt })
    }
}

fn hash_key_file(path: &Path) -> Result<digest::Digest> {
    let mut file = File::open(path)?;
    let mut ctx = digest::Context::new(&SHA256);
    let mut buf = Zeroizing::new(vec![0u8; 64 * 1024]);
    let mut total = 0usize;
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => {
                ctx.update(&buf[..n]);
                total += n;
            },
        }
    }
    if total == 0 {
        return Err(Error::InvalidKeyParams("key file is empty"));
    }
    Ok(ctx.finish())
}

/// Creates a new key file holding 64 random bytes, readable only by its owner.
pub fn generate_key_file(path: &Path) -> Result<()> {
    let mut bytes = Zeroizing::new([0u8; 64]);
    getrandom::fill(bytes.as_mut_slice()).map_err(|_| Error::Kdf)?;
    let mut file = create_private(path)?;
    file.write_all(bytes.as_slice())?;
    file.sync_all()?;
    Ok(())
}

/// The derived SQLCipher key together with the salt it is bound to. Wiped on drop.
#[derive(Clone)]
pub(crate) struct VaultKey {
    raw: Zeroizing<[u8; KEY_LEN]>,
    salt: [u8; SALT_LEN],
}

impl VaultKey {
    #[cfg(feature = "keyring")]
    pub(crate) fn from_raw(raw: &[u8], salt: [u8; SALT_LEN]) -> Option<Self> {
        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        (raw.len() == KEY_LEN).then(|| {
            out.copy_from_slice(raw);
            Self { raw: out, salt }
        })
    }

    #[cfg(feature = "keyring")]
    pub(crate) fn raw(&self) -> &[u8] {
        self.raw.as_slice()
    }

    pub(crate) fn salt(&self) -> [u8; SALT_LEN] {
        self.salt
    }

    /// Whether both are the same key, compared in constant time.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        let diff = self.raw.iter().zip(other.raw.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        diff == 0 && self.salt == other.salt
    }

    /// The SQLCipher raw-key literal: `x'<key hex><salt hex>'`.
    pub(crate) fn keyspec(&self) -> Zeroizing<String> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = Zeroizing::new(String::with_capacity(3 + 2 * (KEY_LEN + SALT_LEN)));
        out.push_str("x'");
        for byte in self.raw.iter().chain(self.salt.iter()) {
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 15)] as char);
        }
        out.push('\'');
        out
    }
}

impl fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VaultKey(***)")
    }
}

/// Creates a new file readable only by its owner; fails if it exists.
pub(crate) fn create_private(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salt_round_trips_params() {
        let params = KdfParams::new(10, 2, 1).unwrap();
        let salt = generate_salt(params).unwrap();
        assert_eq!(parse_salt(&salt).unwrap(), (salt, params));
    }

    #[test]
    fn foreign_headers_are_rejected() {
        assert!(matches!(parse_salt(b"SQLite format 3\0"), Err(Error::NotAVault)));
        assert!(matches!(parse_salt(&[1, 99, 1, 1, 0, 0]), Err(Error::NotAVault)));
    }

    #[test]
    fn key_debug_is_redacted() {
        let key = Credentials::new("hunter2")
            .derive(
                generate_salt(KdfParams::new(3, 1, 1).unwrap()).unwrap(),
                KdfParams::new(3, 1, 1).unwrap(),
            )
            .unwrap();
        assert_eq!(format!("{key:?}"), "VaultKey(***)");
        assert!(!format!("{:?}", Credentials::new("hunter2")).contains("hunter2"));
    }
}
