//! Whole sessions against a scripted peer that plays RustDesk's side of the protocol, as
//! `rustdesk/src/server.rs` and `server/connection.rs` do.

use std::io;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use ring::signature::Ed25519KeyPair;
use rmx_display::{DisplayEvent, InputEvent, Key};
use tokio::io::DuplexStream;

use crate::crypto::{Cipher, Transcript, key_of, open_sealed_key, password_proof, salted_password};
use crate::proto::{
    self, Clipboard, DisplayInfo, Hash, IdPk, LoginResponse, Message, PeerInfo, SignedId,
    TestDelay, key_event, login_response, message,
};
use crate::stream::Connection;
use crate::{BoxFuture, Config, ConnectError, Credentials, Dial, Identity, Target, Transport};

/// Hands out one end of a pipe, once.
struct PipeDialer(Mutex<Option<DuplexStream>>);

impl Dial for PipeDialer {
    fn dial<'a>(
        &'a self,
        _host: &'a str,
        _port: u16,
    ) -> BoxFuture<'a, io::Result<Box<dyn Transport>>> {
        let stream = self.0.lock().unwrap().take();
        Box::pin(async move {
            stream
                .map(|s| Box::new(s) as Box<dyn Transport>)
                .ok_or_else(|| io::ErrorKind::ConnectionRefused.into())
        })
    }
}

/// A peer with a password, a 64 by 32 Linux desktop, and the habit of putting what it was sent
/// on its clipboard.
struct Peer {
    connection: Connection,
    password: &'static str,
    /// Whether it secures the connection, as RustDesk does for connections through its
    /// rendezvous server but not at its direct access port.
    secure: bool,
}

impl Peer {
    async fn send(&mut self, union: message::Union) {
        self.connection.send(&Message::of(union)).await.unwrap();
    }

    /// The next message with something in it, as RustDesk ignores empty ones.
    async fn receive(&mut self) -> Option<message::Union> {
        loop {
            let frame = self.connection.receive().await.ok()??;
            if let Some(union) = proto::decode::<Message>(&frame)?.union {
                return Some(union);
            }
        }
    }

    async fn serve(mut self) {
        if self.secure {
            self.identify().await;
        }
        self.serve_login().await;
    }

    /// The identity handshake.
    async fn identify(&mut self) {
        let identity = Ed25519KeyPair::from_seed_unchecked(&[1; 32]).unwrap();
        let ephemeral = crypto_box::SecretKey::from([2; 32]);
        let ours = ephemeral.public_key();
        let body = proto::encode(&IdPk {
            id: "peer".into(),
            pk: Bytes::copy_from_slice(ours.as_bytes()),
            kx_version: 1,
        });
        let mut signed = identity.sign(&body).as_ref().to_vec();
        signed.extend_from_slice(&body);
        self.send(message::Union::SignedId(SignedId { id: signed.into() })).await;
        let Some(message::Union::PublicKey(pk)) = self.receive().await else {
            panic!("no public key")
        };
        let client = key_of(&pk.asymmetric_value).unwrap();
        let key = open_sealed_key(&pk.symmetric_value, &client, &ephemeral).unwrap();
        let transcript = Transcript {
            initiator_pk: &pk.asymmetric_value,
            responder_pk: ours.as_bytes(),
            advertised: 1,
            picked: pk.kx_version,
        };
        self.connection.set_cipher(Cipher::responder(&key, &transcript));
    }

    async fn serve_login(mut self) {
        let hash = Hash { salt: "pepper".into(), challenge: "riddle".into() };
        self.send(message::Union::Hash(hash.clone())).await;
        let Some(message::Union::LoginRequest(request)) = self.receive().await else {
            panic!("no login")
        };
        let expected = password_proof(&salted_password(self.password, &hash.salt), &hash.challenge);
        let error = if request.password.is_empty() {
            Some("Empty Password")
        } else if request.password.as_ref() != expected.as_slice() {
            Some("Wrong Password")
        } else {
            None
        };
        if let Some(error) = error {
            let response =
                LoginResponse { union: Some(login_response::Union::Error(error.into())) };
            self.send(message::Union::LoginResponse(response)).await;
            return;
        }
        let display = DisplayInfo { width: 64, height: 32, online: true, ..Default::default() };
        let info =
            PeerInfo { platform: "Linux".into(), displays: vec![display], ..Default::default() };
        let response = LoginResponse { union: Some(login_response::Union::PeerInfo(info)) };
        self.send(message::Union::LoginResponse(response)).await;
        self.send(message::Union::TestDelay(TestDelay { time: 7, ..Default::default() })).await;

        // The session.
        while let Some(union) = self.receive().await {
            let text = match union {
                message::Union::TestDelay(delay) => format!("delay {}", delay.time),
                message::Union::KeyEvent(event) => match event.union {
                    Some(key_event::Union::Chr(code)) => {
                        format!("key {code} {}", if event.down { "down" } else { "up" })
                    },
                    _ => continue,
                },
                // Such as a resolution change, which would hang a Wayland peer.
                message::Union::Misc(_) => "misc".to_owned(),
                _ => continue,
            };
            let clipboard =
                Clipboard { content: Bytes::from(text.into_bytes()), ..Default::default() };
            self.send(message::Union::Clipboard(clipboard)).await;
        }
    }
}

fn start(password: &'static str, secure: bool) -> PipeDialer {
    let (client, server) = tokio::io::duplex(1 << 20);
    tokio::spawn(Peer { connection: Connection::new(server), password, secure }.serve());
    PipeDialer(Mutex::new(Some(client)))
}

fn config(password: Option<&str>) -> Config {
    let mut config = Config::new(Target::Direct { host: "peer".into(), port: crate::DIRECT_PORT });
    config.credentials =
        Credentials { password: password.map(str::to_owned), two_factor_code: None };
    config
}

async fn next_event(events: &mut tokio::sync::mpsc::Receiver<DisplayEvent>) -> DisplayEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event in time")
        .expect("open")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_logs_in_shows_the_desktop_and_carries_input() {
    for (secure, identity) in [(false, Identity::Unencrypted), (true, Identity::Unverified)] {
        let dialer = start("secret", secure);
        let session = crate::connect(&dialer, config(Some("secret"))).await.unwrap();
        assert_eq!(session.identity, identity);
        carries_input(session).await;
    }
}

async fn carries_input(session: crate::Session) {
    assert_eq!(session.platform, "Linux");
    let mut display = session.display;
    assert_eq!(
        next_event(&mut display.events).await,
        DisplayEvent::Resized { width: 64, height: 32 }
    );

    // The test delay that came in with the login was sent back.
    assert_eq!(next_event(&mut display.events).await, DisplayEvent::Clipboard("delay 7".into()));

    // The pane changing size asks nothing of the peer: the key is the next thing it hears of.
    display.input.send(InputEvent::Resize { width: 1280, height: 720 }).await.unwrap();
    // A Linux peer gets X key codes: A is evdev 30, X keycode 38.
    let key = Key { evdev: 30, keysym: None };
    display.input.send(InputEvent::Key { key, pressed: true }).await.unwrap();
    assert_eq!(
        next_event(&mut display.events).await,
        DisplayEvent::Clipboard("key 38 down".into())
    );

    display.input.send(InputEvent::Disconnect).await.unwrap();
    loop {
        if let DisplayEvent::Closed(reason) = next_event(&mut display.events).await {
            assert_eq!(reason, None);
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_is_reported() {
    let dialer = start("secret", false);
    assert_eq!(
        crate::connect(&dialer, config(Some("guess"))).await.err(),
        Some(ConnectError::WrongPassword)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_password_is_reported_as_needing_one() {
    let dialer = start("secret", false);
    assert_eq!(
        crate::connect(&dialer, config(None)).await.err(),
        Some(ConnectError::PasswordRequired)
    );
}
