//! The cryptography of a RustDesk connection.
//!
//! The peer proves its identity with Ed25519. The client then picks a random XSalsa20-Poly1305
//! key and seals it to the peer's ephemeral X25519 key with NaCl's `crypto_box`; every frame
//! after that is a NaCl `secretbox` under a nonce made of a per-direction counter. Key exchange
//! version 1 derives a separate key for each direction with keyed BLAKE2b, binding in what both
//! sides saw of the exchange.

use blake2::Blake2bMac;
use blake2::digest::consts::U32;
use blake2::digest::{FixedOutput, Update};
use bytes::Bytes;
use crypto_box::aead::AeadInPlace;
use crypto_box::aead::generic_array::GenericArray;
use crypto_secretbox::{KeyInit, XSalsa20Poly1305};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, UnparsedPublicKey};

/// The newest key exchange version this client speaks.
pub(crate) const KX_VERSION: u32 = 1;

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const SIGNATURE_LEN: usize = 64;
const SPLIT_CONTEXT: &[u8] = b"rdkx-spl";
const INITIATOR: u8 = 1;
const RESPONDER: u8 = 2;

/// A 32-byte key or public key.
pub(crate) type Key = [u8; 32];

/// Something the peer or the server sent that does not hold up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CryptoError {
    #[error("a signature did not verify")]
    BadSignature,
    #[error("a key has the wrong length")]
    BadKey,
    #[error("a frame failed to decrypt")]
    Decrypt,
}

/// The secure random numbers keys are made of.
pub(crate) fn random_key() -> Key {
    let mut key = [0; 32];
    SystemRandom::new().fill(&mut key).expect("the system has a random number generator");
    key
}

/// Splits `signed`, a libsodium "combined" signature, and returns the message if `public_key`
/// signed it.
pub(crate) fn open_signed<'a>(
    signed: &'a [u8],
    public_key: &[u8],
) -> Result<&'a [u8], CryptoError> {
    if signed.len() < SIGNATURE_LEN {
        return Err(CryptoError::BadSignature);
    }
    let (signature, message) = signed.split_at(SIGNATURE_LEN);
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message, signature)
        .map_err(|_| CryptoError::BadSignature)?;
    Ok(message)
}

/// Turns 32 bytes into a key, or fails if there are not 32.
pub(crate) fn key_of(bytes: &[u8]) -> Result<Key, CryptoError> {
    bytes.try_into().map_err(|_| CryptoError::BadKey)
}

/// What the client sends to set up encryption: its ephemeral public key and the session key
/// sealed to the peer's, plus the session key itself.
pub(crate) struct KeyOffer {
    pub(crate) public_key: Key,
    pub(crate) sealed_key: Bytes,
    pub(crate) session_key: Key,
}

/// Picks a session key and seals it to `their_public`, as `crypto_box` with a zero nonce.
pub(crate) fn offer_key(their_public: &Key) -> KeyOffer {
    let secret = crypto_box::SecretKey::from(random_key());
    let public_key = *secret.public_key().as_bytes();
    let session_key = random_key();
    let sealer = crypto_box::SalsaBox::new(&crypto_box::PublicKey::from(*their_public), &secret);
    let sealed_key = seal_detached(&sealer, &[0; NONCE_LEN], &session_key);
    KeyOffer { public_key, sealed_key: Bytes::from(sealed_key), session_key }
}

/// What the peer does with a [`KeyOffer`], for tests that play the peer.
#[cfg(test)]
pub(crate) fn open_sealed_key(
    sealed: &[u8],
    client_public: &Key,
    peer_secret: &crypto_box::SecretKey,
) -> Result<Key, CryptoError> {
    let opener =
        crypto_box::SalsaBox::new(&crypto_box::PublicKey::from(*client_public), peer_secret);
    key_of(&open_detached(&opener, &[0; NONCE_LEN], sealed)?)
}

/// NaCl's layout: the tag, then the ciphertext.
fn seal_detached<A: AeadInPlace<NonceSize = crypto_box::aead::consts::U24>>(
    cipher: &A,
    nonce: &[u8; NONCE_LEN],
    plain: &[u8],
) -> Vec<u8> {
    let mut out = vec![0; TAG_LEN + plain.len()];
    out[TAG_LEN..].copy_from_slice(plain);
    let tag = cipher
        .encrypt_in_place_detached(GenericArray::from_slice(nonce), &[], &mut out[TAG_LEN..])
        .expect("a frame within the size limit encrypts");
    out[..TAG_LEN].copy_from_slice(&tag);
    out
}

fn open_detached<A: AeadInPlace<NonceSize = crypto_box::aead::consts::U24>>(
    cipher: &A,
    nonce: &[u8; NONCE_LEN],
    sealed: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() < TAG_LEN {
        return Err(CryptoError::Decrypt);
    }
    let (tag, body) = sealed.split_at(TAG_LEN);
    let mut plain = body.to_vec();
    cipher
        .decrypt_in_place_detached(
            GenericArray::from_slice(nonce),
            &[],
            &mut plain,
            GenericArray::from_slice(tag),
        )
        .map_err(|_| CryptoError::Decrypt)?;
    Ok(plain)
}

/// What both sides saw of the key exchange, mixed into the version 1 keys.
pub(crate) struct Transcript<'a> {
    /// The ephemeral key of the side that sent the sealed key: this client.
    pub(crate) initiator_pk: &'a [u8],
    /// The ephemeral key the session key was sealed to: the peer's.
    pub(crate) responder_pk: &'a [u8],
    /// The version the peer advertised.
    pub(crate) advertised: u32,
    /// The version this client picked.
    pub(crate) picked: u32,
}

/// The version to run with a peer that advertised `advertised`.
pub(crate) fn pick_version(advertised: u32) -> u32 {
    advertised.min(KX_VERSION)
}

/// The keys and counters of an encrypted stream.
pub(crate) struct Cipher {
    sealer: Sealer,
    opener: Opener,
}

/// Encrypts what this side sends.
pub(crate) struct Sealer {
    key: XSalsa20Poly1305,
    count: u64,
}

/// Decrypts what this side receives.
pub(crate) struct Opener {
    key: XSalsa20Poly1305,
    count: u64,
}

impl Cipher {
    /// Version 0: one key both ways.
    pub(crate) fn shared(key: &Key) -> Self {
        Self::with_keys(key, key)
    }

    /// The keys for `transcript.picked`, as the initiator of the exchange.
    pub(crate) fn negotiated(key: &Key, transcript: &Transcript<'_>) -> Self {
        if transcript.picked == 0 {
            return Self::shared(key);
        }
        let send = subkey(key, INITIATOR, transcript);
        let receive = subkey(key, RESPONDER, transcript);
        Self::with_keys(&send, &receive)
    }

    fn with_keys(send: &Key, receive: &Key) -> Self {
        Self {
            sealer: Sealer { key: XSalsa20Poly1305::new(send.into()), count: 0 },
            opener: Opener { key: XSalsa20Poly1305::new(receive.into()), count: 0 },
        }
    }

    pub(crate) fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        self.sealer.seal(plain)
    }

    pub(crate) fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.opener.open(sealed)
    }

    /// The peer's side of [`negotiated`](Self::negotiated), for tests that play the peer.
    #[cfg(test)]
    pub(crate) fn responder(key: &Key, transcript: &Transcript<'_>) -> Self {
        if transcript.picked == 0 {
            return Self::shared(key);
        }
        Self::with_keys(&subkey(key, RESPONDER, transcript), &subkey(key, INITIATOR, transcript))
    }

    /// The two directions apart, keeping their counters.
    pub(crate) fn split(self) -> (Sealer, Opener) {
        (self.sealer, self.opener)
    }
}

impl Sealer {
    pub(crate) fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        self.count += 1;
        seal_detached(&self.key, &nonce(self.count), plain)
    }
}

impl Opener {
    /// Decrypts a frame. Frames of one byte or less are passed through as they are, as RustDesk
    /// does.
    pub(crate) fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() <= 1 {
            return Ok(sealed.to_vec());
        }
        self.count += 1;
        open_detached(&self.key, &nonce(self.count), sealed)
    }
}

fn nonce(counter: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0; NONCE_LEN];
    nonce[..8].copy_from_slice(&counter.to_le_bytes());
    nonce
}

fn subkey(key: &Key, direction: u8, t: &Transcript<'_>) -> Key {
    let mut mac = Blake2bMac::<U32>::new_with_salt_and_personal(Some(key), &[], &[])
        .expect("a 32-byte BLAKE2b key");
    for part in [
        SPLIT_CONTEXT,
        &[direction],
        &t.advertised.to_le_bytes(),
        &t.picked.to_le_bytes(),
        t.initiator_pk,
        t.responder_pk,
    ] {
        mac.update(part);
    }
    mac.finalize_fixed().into()
}

/// What a peer's password check wants: SHA-256 of the password and the peer's salt.
pub(crate) fn salted_password(password: &str, salt: &str) -> Vec<u8> {
    sha256(&[password.as_bytes(), salt.as_bytes()])
}

/// The login proof: SHA-256 of the salted password and the peer's challenge.
pub(crate) fn password_proof(salted: &[u8], challenge: &str) -> Vec<u8> {
    sha256(&[salted, challenge.as_bytes()])
}

fn sha256(parts: &[&[u8]]) -> Vec<u8> {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for part in parts {
        context.update(part);
    }
    context.finish().as_ref().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The frames and keys RustDesk pins in `hbb_common`'s tests: a change here breaks the wire.
    #[test]
    fn version_0_frames_match_rustdesk() {
        let mut cipher = Cipher::shared(&[0x11; 32]);
        assert_eq!(hex(&cipher.seal(b"hello")), "3b768f827fdcc4af555b9e42533be6611e2c333481");
    }

    #[test]
    fn version_1_subkeys_match_rustdesk() {
        let mut t = Transcript {
            initiator_pk: &[0x22; 32],
            responder_pk: &[0x33; 32],
            advertised: 1,
            picked: 1,
        };
        let key = [0x11; 32];
        assert_eq!(
            hex(&subkey(&key, INITIATOR, &t)),
            "c189ec6e1935c0751cfc85a0f075405cae507d7925bb19687caf56cbb54e0ecf"
        );
        assert_eq!(
            hex(&subkey(&key, RESPONDER, &t)),
            "8a62648e9195cb10ea900c0a24d2a166c1e982347a2dc6833c3e756bf5715d36"
        );
        t.advertised = 3;
        assert_eq!(
            hex(&subkey(&key, INITIATOR, &t)),
            "25b0778ae600f9731c7fa457d1cc5d40071b0aa3d34c90b53e1d5344b0e7123a"
        );
        assert_eq!(
            hex(&subkey(&key, RESPONDER, &t)),
            "5bbde0ac6a708d4cd1ff807125d79cb6030bde62f7a66b6cc1a79e31bd657756"
        );
    }

    #[test]
    fn version_1_sends_and_receives_under_different_keys() {
        let t =
            Transcript { initiator_pk: &[1; 32], responder_pk: &[2; 32], advertised: 1, picked: 1 };
        let key = [9; 32];
        let mut client = Cipher::negotiated(&key, &t);
        // The peer is the responder: it sends under the responder key.
        let mut peer =
            Cipher::with_keys(&subkey(&key, RESPONDER, &t), &subkey(&key, INITIATOR, &t));
        let sealed = client.seal(b"to the peer");
        assert_eq!(peer.open(&sealed).unwrap(), b"to the peer");
        let back = peer.seal(b"to the client");
        assert_eq!(client.open(&back).unwrap(), b"to the client");
        // The exchanged key itself opens neither direction.
        assert!(Cipher::shared(&key).open(&sealed).is_err());
    }

    #[test]
    fn counters_advance_per_frame() {
        let key = [5; 32];
        let (mut a, mut b) = (Cipher::shared(&key), Cipher::shared(&key));
        let first = a.seal(b"x1");
        let second = a.seal(b"x2");
        assert!(b.open(&second).is_err(), "frames out of order do not open");
        let mut b = Cipher::shared(&key);
        assert_eq!(b.open(&first).unwrap(), b"x1");
        assert_eq!(b.open(&second).unwrap(), b"x2");
        let _ = &mut a;
    }

    #[test]
    fn tiny_frames_pass_through() {
        assert_eq!(Cipher::shared(&[0; 32]).open(&[7]).unwrap(), [7]);
        assert!(Cipher::shared(&[0; 32]).open(&[]).unwrap().is_empty());
    }

    #[test]
    fn the_sealed_key_opens_with_the_peer_secret() {
        let peer_secret = crypto_box::SecretKey::from([3; 32]);
        let peer_public = *peer_secret.public_key().as_bytes();
        let offer = offer_key(&peer_public);
        let opened = open_sealed_key(&offer.sealed_key, &offer.public_key, &peer_secret).unwrap();
        assert_eq!(opened, offer.session_key);
    }

    #[test]
    fn signatures_are_checked() {
        use ring::signature::{Ed25519KeyPair, KeyPair};
        let pair = Ed25519KeyPair::from_seed_unchecked(&[4; 32]).unwrap();
        let mut signed = pair.sign(b"id and key").as_ref().to_vec();
        signed.extend_from_slice(b"id and key");
        let public = pair.public_key().as_ref();
        assert_eq!(open_signed(&signed, public).unwrap(), b"id and key");
        signed[70] ^= 1;
        assert_eq!(open_signed(&signed, public), Err(CryptoError::BadSignature));
        assert_eq!(open_signed(&signed[..10], public), Err(CryptoError::BadSignature));
    }

    #[test]
    fn the_password_proof_is_two_sha256_rounds() {
        let salted = salted_password("secret", "salt");
        assert_eq!(salted, sha256(&[b"secretsalt"]));
        assert_eq!(password_proof(&salted, "challenge"), sha256(&[&salted, b"challenge"]));
    }
}
