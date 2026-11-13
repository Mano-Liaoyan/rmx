//! Securing the connection to the peer and logging in.
//!
//! The peer speaks first, with its ID and an ephemeral X25519 key signed by its identity key.
//! The client answers with a session key sealed to that ephemeral key, and from then on every
//! frame is encrypted. The peer then sends a salt and a challenge, and the client logs in with
//! a proof that it knows the password.
//!
//! A peer reached at its direct access port skips the first part: RustDesk serves those
//! connections unsecured, so its first message is already the salt and challenge.

use std::time::Duration;

use bytes::Bytes;
use log::{info, warn};

use crate::crypto::{
    Cipher, Key, Transcript, key_of, offer_key, open_signed, password_proof, pick_version,
    salted_password,
};
use crate::proto::{
    self, Auth2Fa, Hash, IdPk, LoginRequest, Message, OptionMessage, PeerInfo, PublicKey,
    login_response, message,
};
use crate::stream::Connection;
use crate::{ConnectError, Credentials};

/// How long the peer may take to answer during the handshake.
const STEP_TIMEOUT: Duration = Duration::from_secs(18);
/// How long the person at the peer may take to accept a session.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// What the peer's login error strings mean.
const PASSWORD_EMPTY: &str = "Empty Password";
const PASSWORD_WRONG: &str = "Wrong Password";
const TWO_FACTOR_REQUIRED: &str = "2FA Required";
const TWO_FACTOR_WRONG: &str = "Wrong 2FA Code";
const NO_PASSWORD_ACCESS: &str = "No Password Access";
/// Also what a peer says to a login that names neither its ID nor its address.
const OFFLINE: &str = "Offline";

/// How far the peer's identity was checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    /// The rendezvous server vouched for the peer's key and the peer proved it holds it.
    Verified,
    /// The connection is encrypted, but nothing proved who is at the other end: a rendezvous
    /// server that sent no key.
    Unverified,
    /// The peer did not offer to encrypt, as RustDesk does not on its direct access port. The
    /// password still only goes as a salted proof, but the session itself is in the clear.
    Unencrypted,
}

/// Secures `connection`. `peer_key` is the identity key the rendezvous server vouched for.
///
/// Also returns the peer's first message when it was not part of the handshake, which happens
/// only when the peer does not encrypt; the login is to handle it.
pub(crate) async fn secure(
    connection: &mut Connection,
    peer_id: Option<&str>,
    peer_key: Option<&Key>,
) -> Result<(Identity, Option<Message>), ConnectError> {
    let frame = connection.receive_within(STEP_TIMEOUT).await.map_err(peer_failure)?;
    let first = proto::decode::<Message>(&frame)
        .ok_or_else(|| ConnectError::Failed("the peer sent something unreadable".to_owned()))?;
    let signed = match first.union {
        Some(message::Union::SignedId(signed)) => signed,
        // A peer the server vouched for has to prove it; anything else could be someone
        // pretending to be it.
        _ if peer_key.is_some() => {
            return Err(ConnectError::Failed("the peer did not identify itself".to_owned()));
        },
        _ => {
            // As RustDesk's own client does: an empty message in case the peer is waiting for
            // one, then the session in the clear.
            connection.send(&Message::default()).await.map_err(peer_failure)?;
            warn!("the RustDesk peer does not encrypt this connection");
            return Ok((Identity::Unencrypted, Some(first)));
        },
    };

    let (id_pk, identity) = match peer_key {
        Some(key) => {
            let body = open_signed(&signed.id, key).map_err(|_| ConnectError::PeerKeyMismatch)?;
            let id_pk: IdPk = proto::decode(body).ok_or(ConnectError::PeerKeyMismatch)?;
            if peer_id.is_some_and(|id| id != id_pk.id) {
                return Err(ConnectError::PeerKeyMismatch);
            }
            (id_pk, Identity::Verified)
        },
        None => {
            // Nothing to check the signature against, but the ephemeral key still lets the
            // stream be encrypted, which keeps it from anyone only listening.
            let body = signed.id.get(64..).unwrap_or_default();
            let id_pk: IdPk = proto::decode(body).ok_or_else(|| {
                ConnectError::Failed("the peer's identity is malformed".to_owned())
            })?;
            (id_pk, Identity::Unverified)
        },
    };
    let their_pk = key_of(&id_pk.pk).map_err(|_| ConnectError::PeerKeyMismatch)?;

    let offer = offer_key(&their_pk);
    let picked = pick_version(id_pk.kx_version);
    let reply = Message::of(message::Union::PublicKey(PublicKey {
        asymmetric_value: Bytes::copy_from_slice(&offer.public_key),
        symmetric_value: offer.sealed_key,
        kx_version: picked,
    }));
    connection.send(&reply).await.map_err(peer_failure)?;
    let transcript = Transcript {
        initiator_pk: &offer.public_key,
        responder_pk: &their_pk,
        advertised: id_pk.kx_version,
        picked,
    };
    connection.set_cipher(Cipher::negotiated(&offer.session_key, &transcript));
    info!("connection to the peer secured, key exchange version {picked}, {identity:?}");
    Ok((identity, None))
}

/// What logging in needs besides the credentials.
pub(crate) struct Login<'a> {
    pub(crate) peer_id: &'a str,
    pub(crate) my_id: &'a str,
    pub(crate) my_name: &'a str,
    pub(crate) option: OptionMessage,
    pub(crate) session_id: u64,
}

/// Logs in, and returns what the peer says about itself.
///
/// `first` is a message the peer already sent, as [`secure`] returns it. Messages that arrive
/// before the answer and are not part of the login are returned too, so the session can handle
/// them.
pub(crate) async fn login(
    connection: &mut Connection,
    credentials: &Credentials,
    login: Login<'_>,
    mut first: Option<Message>,
) -> Result<(PeerInfo, Vec<Message>), ConnectError> {
    let hash = loop {
        let message = match first.take() {
            Some(message) => Some(message),
            None => {
                let frame = connection.receive_within(STEP_TIMEOUT).await.map_err(peer_failure)?;
                proto::decode::<Message>(&frame)
            },
        };
        match message.and_then(|message| message.union) {
            Some(message::Union::Hash(hash)) => break hash,
            Some(message::Union::MessageBox(text)) => return Err(message_box(text)),
            Some(message::Union::Misc(proto::Misc {
                union: Some(proto::misc::Union::CloseReason(reason)),
            })) => {
                return Err(ConnectError::Failed(reason));
            },
            _ => {},
        }
    };

    let proof = proof(credentials.password.as_deref(), &hash);
    let request = LoginRequest {
        username: login.peer_id.to_owned(),
        password: Bytes::from(proof),
        my_id: login.my_id.to_owned(),
        my_name: login.my_name.to_owned(),
        option: Some(login.option),
        session_id: login.session_id,
        version: crate::PEER_VERSION.to_owned(),
        my_platform: platform().to_owned(),
        ..Default::default()
    };
    connection
        .send(&Message::of(message::Union::LoginRequest(request)))
        .await
        .map_err(peer_failure)?;

    let mut early = Vec::new();
    let mut sent_code = false;
    loop {
        let frame = connection.receive_within(APPROVAL_TIMEOUT).await.map_err(peer_failure)?;
        let Some(message) = proto::decode::<Message>(&frame) else { continue };
        match message.union {
            Some(message::Union::LoginResponse(response)) => match response.union {
                Some(login_response::Union::PeerInfo(info)) => return Ok((info, early)),
                Some(login_response::Union::Error(error)) => match error.as_str() {
                    TWO_FACTOR_REQUIRED if !sent_code => {
                        let Some(code) = credentials.two_factor_code.clone() else {
                            return Err(ConnectError::TwoFactorRequired);
                        };
                        let auth = Message::of(message::Union::Auth2Fa(Auth2Fa { code }));
                        connection.send(&auth).await.map_err(peer_failure)?;
                        sent_code = true;
                    },
                    _ => return Err(login_error(&error)),
                },
                None => {},
            },
            Some(message::Union::MessageBox(text)) => {
                // Notices while waiting, such as the peer asking its user to accept.
                warn!("peer says: {} {}", text.title, text.text);
                if text.msgtype.contains("error") {
                    return Err(message_box(text));
                }
            },
            Some(message::Union::Misc(proto::Misc {
                union: Some(proto::misc::Union::CloseReason(reason)),
            })) => {
                return Err(ConnectError::Failed(reason));
            },
            Some(message::Union::TestDelay(_)) | Some(message::Union::Hash(_)) => {
                early.push(message)
            },
            Some(_) => early.push(message),
            None => {},
        }
    }
}

/// The password proof for `hash`, or nothing to ask the person at the peer to accept instead.
fn proof(password: Option<&str>, hash: &Hash) -> Vec<u8> {
    match password.filter(|password| !password.is_empty()) {
        Some(password) => password_proof(&salted_password(password, &hash.salt), &hash.challenge),
        None => Vec::new(),
    }
}

fn login_error(error: &str) -> ConnectError {
    match error {
        PASSWORD_EMPTY => ConnectError::PasswordRequired,
        PASSWORD_WRONG => ConnectError::WrongPassword,
        TWO_FACTOR_REQUIRED | TWO_FACTOR_WRONG => ConnectError::WrongTwoFactorCode,
        NO_PASSWORD_ACCESS => {
            ConnectError::Failed("the peer only accepts sessions its user approves".to_owned())
        },
        OFFLINE => ConnectError::Offline,
        other => ConnectError::Failed(other.to_owned()),
    }
}

fn message_box(text: proto::MessageBox) -> ConnectError {
    let reason =
        [text.title, text.text].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>();
    ConnectError::Failed(if reason.is_empty() {
        "the peer refused".to_owned()
    } else {
        reason.join(": ")
    })
}

fn peer_failure(err: std::io::Error) -> ConnectError {
    match err.kind() {
        std::io::ErrorKind::TimedOut => ConnectError::Failed("the peer did not answer".to_owned()),
        std::io::ErrorKind::UnexpectedEof => {
            ConnectError::Failed("the peer closed the connection".to_owned())
        },
        _ => ConnectError::Failed(err.to_string()),
    }
}

/// The platform name RustDesk peers expect.
fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "Mac OS"
    } else {
        "Linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{SignedId, login_response::Union as LoginUnion};
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// A peer's half of the handshake, as `rustdesk/src/server.rs` does it.
    struct FakePeer {
        connection: Connection,
        identity: Ed25519KeyPair,
        ephemeral: crypto_box::SecretKey,
    }

    impl FakePeer {
        fn new(connection: Connection) -> Self {
            Self {
                connection,
                identity: Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap(),
                ephemeral: crypto_box::SecretKey::from([8; 32]),
            }
        }

        fn identity_key(&self) -> Key {
            key_of(self.identity.public_key().as_ref()).unwrap()
        }

        async fn identify(&mut self, id: &str, kx_version: u32) {
            let id_pk = IdPk {
                id: id.into(),
                pk: Bytes::copy_from_slice(self.ephemeral.public_key().as_bytes()),
                kx_version,
            };
            let body = proto::encode(&id_pk);
            let mut signed = self.identity.sign(&body).as_ref().to_vec();
            signed.extend_from_slice(&body);
            let message = Message::of(message::Union::SignedId(SignedId { id: signed.into() }));
            self.connection.send(&message).await.unwrap();
        }

        async fn accept_key(&mut self, advertised: u32) {
            let frame = self.connection.receive().await.unwrap().unwrap();
            let Some(message::Union::PublicKey(pk)) =
                proto::decode::<Message>(&frame).unwrap().union
            else {
                panic!("expected a public key");
            };
            let client = key_of(&pk.asymmetric_value).unwrap();
            let key = crate::crypto::open_sealed_key(&pk.symmetric_value, &client, &self.ephemeral)
                .unwrap();
            let ours = self.ephemeral.public_key();
            let transcript = Transcript {
                initiator_pk: &pk.asymmetric_value,
                responder_pk: ours.as_bytes(),
                advertised,
                picked: pk.kx_version,
            };
            self.connection.set_cipher(Cipher::responder(&key, &transcript));
        }
    }

    fn pair() -> (Connection, Connection) {
        let (a, b) = tokio::io::duplex(1 << 16);
        (Connection::new(a), Connection::new(b))
    }

    fn login_for<'a>() -> Login<'a> {
        Login {
            peer_id: "123",
            my_id: "rmx",
            my_name: "rmx",
            option: OptionMessage::default(),
            session_id: 1,
        }
    }

    #[tokio::test]
    async fn a_verified_peer_completes_the_exchange_at_either_version() {
        for version in [0, 1] {
            let (mut client, peer) = pair();
            let mut peer = FakePeer::new(peer);
            let key = peer.identity_key();
            peer.identify("123", version).await;
            let secured = secure(&mut client, Some("123"), Some(&key)).await.unwrap();
            assert_eq!(secured, (Identity::Verified, None));
            peer.accept_key(version).await;

            let hash =
                Message::of(message::Union::Hash(Hash { salt: "s".into(), challenge: "c".into() }));
            peer.connection.send(&hash).await.unwrap();
            let frame = client.receive().await.unwrap().unwrap();
            assert_eq!(proto::decode::<Message>(&frame), Some(hash), "version {version}");
        }
    }

    #[tokio::test]
    async fn a_peer_signing_with_another_key_is_refused() {
        let (mut client, peer) = pair();
        let mut peer = FakePeer::new(peer);
        peer.identify("123", 1).await;
        let result = secure(&mut client, Some("123"), Some(&[1; 32])).await;
        assert_eq!(result, Err(ConnectError::PeerKeyMismatch));
    }

    #[tokio::test]
    async fn a_peer_claiming_another_id_is_refused() {
        let (mut client, peer) = pair();
        let mut peer = FakePeer::new(peer);
        let key = peer.identity_key();
        peer.identify("999", 1).await;
        assert_eq!(
            secure(&mut client, Some("123"), Some(&key)).await,
            Err(ConnectError::PeerKeyMismatch)
        );
    }

    #[tokio::test]
    async fn without_a_key_the_stream_is_still_encrypted() {
        let (mut client, peer) = pair();
        let mut peer = FakePeer::new(peer);
        peer.identify("123", 1).await;
        assert_eq!(secure(&mut client, None, None).await, Ok((Identity::Unverified, None)));
        assert!(client.is_encrypted());
        peer.accept_key(1).await;
    }

    /// RustDesk's direct access port serves connections unsecured: the peer's first message is
    /// its login challenge.
    #[tokio::test]
    async fn a_direct_peer_that_does_not_encrypt_goes_straight_to_the_login() {
        let (mut client, mut peer) = pair();
        let hash =
            Message::of(message::Union::Hash(Hash { salt: "s".into(), challenge: "c".into() }));
        peer.send(&hash).await.unwrap();
        assert_eq!(secure(&mut client, None, None).await, Ok((Identity::Unencrypted, Some(hash))));
        assert!(!client.is_encrypted());
        // The empty message RustDesk's client sends there too.
        let frame = peer.receive().await.unwrap().unwrap();
        assert_eq!(proto::decode::<Message>(&frame), Some(Message::default()));
    }

    #[tokio::test]
    async fn a_peer_the_server_vouched_for_must_identify_itself() {
        let (mut client, mut peer) = pair();
        let hash = Message::of(message::Union::Hash(Hash::default()));
        peer.send(&hash).await.unwrap();
        assert_eq!(
            secure(&mut client, Some("123"), Some(&[1; 32])).await,
            Err(ConnectError::Failed("the peer did not identify itself".to_owned()))
        );
    }

    #[tokio::test]
    async fn login_sends_the_proof_and_returns_the_peer_info() {
        let (mut client, mut peer) = pair();
        let credentials = Credentials { password: Some("pw".into()), two_factor_code: None };
        let peer_task = tokio::spawn(async move {
            let hash = Hash { salt: "salt".into(), challenge: "challenge".into() };
            peer.send(&Message::of(message::Union::Hash(hash))).await.unwrap();
            let frame = peer.receive().await.unwrap().unwrap();
            let Some(message::Union::LoginRequest(request)) =
                proto::decode::<Message>(&frame).unwrap().union
            else {
                panic!("expected a login");
            };
            let info = PeerInfo { platform: "Windows".into(), ..Default::default() };
            let response = proto::LoginResponse { union: Some(LoginUnion::PeerInfo(info)) };
            peer.send(&Message::of(message::Union::LoginResponse(response))).await.unwrap();
            request
        });
        let (info, _) = login(&mut client, &credentials, login_for(), None).await.unwrap();
        assert_eq!(info.platform, "Windows");
        let request = peer_task.await.unwrap();
        let expected = password_proof(&salted_password("pw", "salt"), "challenge");
        assert_eq!(request.password.as_ref(), expected.as_slice());
        assert_eq!(request.username, "123");
    }

    #[tokio::test]
    async fn login_errors_map_to_what_the_user_can_fix() {
        for (error, expected) in [
            (PASSWORD_WRONG, ConnectError::WrongPassword),
            (PASSWORD_EMPTY, ConnectError::PasswordRequired),
            (TWO_FACTOR_REQUIRED, ConnectError::TwoFactorRequired),
        ] {
            let (mut client, mut peer) = pair();
            let credentials = Credentials { password: Some("pw".into()), two_factor_code: None };
            tokio::spawn(async move {
                let hash = Hash { salt: "s".into(), challenge: "c".into() };
                peer.send(&Message::of(message::Union::Hash(hash))).await.unwrap();
                let _ = peer.receive().await;
                let response =
                    proto::LoginResponse { union: Some(LoginUnion::Error(error.into())) };
                peer.send(&Message::of(message::Union::LoginResponse(response))).await.unwrap();
                let _ = peer.receive().await;
            });
            assert_eq!(
                login(&mut client, &credentials, login_for(), None).await.err(),
                Some(expected)
            );
        }
    }

    #[test]
    fn an_empty_password_asks_for_approval() {
        let hash = Hash { salt: "s".into(), challenge: "c".into() };
        assert!(proof(None, &hash).is_empty());
        assert!(proof(Some(""), &hash).is_empty());
        assert_eq!(proof(Some("x"), &hash).len(), 32);
    }
}
