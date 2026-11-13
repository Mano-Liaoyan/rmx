use std::sync::{Arc, LazyLock};
use std::time::Duration;

use rmx_display::DisplayEvent;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, duplex};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use super::*;
use crate::{CertificatePolicy, connect};

/// A self-signed certificate for `localhost` and its PKCS #8 key, made once per test run.
static IDENTITY: LazyLock<(Vec<u8>, Vec<u8>)> = LazyLock::new(|| {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    (cert.der().to_vec(), key.serialize_der())
});

fn cert() -> &'static [u8] {
    &IDENTITY.0
}

const WAIT: Duration = Duration::from_secs(10);

fn pin() -> [u8; 32] {
    tls::fingerprint(cert())
}

fn acceptor() -> TlsAcceptor {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(IDENTITY.1.clone()));
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert().to_vec())], key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

fn config() -> Config {
    Config { certificate: CertificatePolicy::Pinned(pin()), ..Config::default() }
}

/// The server's side up to its list of security types, answered with the client's choice.
async fn offer(stream: &mut DuplexStream, version: &[u8; 12], types: &[u8]) -> u8 {
    stream.write_all(version).await.unwrap();
    let mut theirs = [0; 12];
    stream.read_exact(&mut theirs).await.unwrap();
    assert_eq!(&theirs, version, "the client answers with the server's version");
    stream.write_u8(types.len() as u8).await.unwrap();
    stream.write_all(types).await.unwrap();
    stream.read_u8().await.unwrap()
}

/// The server's side of VeNCrypt up to the start of TLS, answered with the chosen subtype if any.
async fn offer_vencrypt(stream: &mut DuplexStream, subtypes: &[u32]) -> Option<u32> {
    assert_eq!(offer(stream, b"RFB 003.008\n", &[VNC_AUTH, VENCRYPT]).await, VENCRYPT);
    stream.write_all(&[0, 2]).await.unwrap();
    let mut version = [0; 2];
    stream.read_exact(&mut version).await.unwrap();
    assert_eq!(version, [0, 2]);
    stream.write_u8(0).await.unwrap();
    stream.write_u8(subtypes.len() as u8).await.unwrap();
    for &subtype in subtypes {
        stream.write_u32(subtype).await.unwrap();
    }
    stream.read_u32().await.ok()
}

/// Reads what the handoff plays to `vnc-rs`, which must be `expected`.
async fn played(handoff: &mut (impl AsyncRead + Unpin), expected: &[u8]) {
    let mut script = vec![0; expected.len()];
    handoff.read_exact(&mut script).await.unwrap();
    assert_eq!(script, expected);
}

/// What the server receives through `stream` once `vnc-rs` sends ClientInit through `handoff`.
async fn client_init<A, B>(handoff: &mut A, stream: &mut B)
where
    A: AsyncWrite + Unpin,
    B: AsyncRead + Unpin,
{
    handoff.write_all(b"RFB 003.003\n").await.unwrap();
    handoff.write_all(&[1]).await.unwrap();
    handoff.flush().await.unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), 1, "ClientInit reaches the server");
}

#[test]
fn x509_subtypes_are_taken_in_the_servers_order() {
    let order = [TLS_VNC, X509_PLAIN, X509_VNC, X509_NONE];
    assert_eq!(choose_subtype(&order, true, true).unwrap(), (X509_PLAIN, Security::X509Plain));
    assert_eq!(choose_subtype(&order, false, true).unwrap(), (X509_VNC, Security::X509Vnc));
    assert_eq!(choose_subtype(&order, false, false).unwrap(), (X509_NONE, Security::X509None));
}

#[test]
fn a_subtype_lacking_credentials_says_what_is_missing() {
    assert_eq!(choose_subtype(&[X509_VNC], false, false), Err(ConnectError::PasswordRequired));
    assert_eq!(choose_subtype(&[X509_PLAIN], false, true), Err(ConnectError::LoginRequired));
    assert_eq!(
        choose_subtype(&[X509_PLAIN, X509_VNC], false, false),
        Err(ConnectError::LoginRequired),
    );
}

#[test]
fn anonymous_tls_and_plain_are_refused() {
    let Err(ConnectError::Failed(reason)) = choose_subtype(&[TLS_NONE, TLS_VNC], true, true) else {
        panic!("anonymous TLS was accepted");
    };
    assert!(reason.contains("anonymous TLS"), "{reason}");
    let Err(ConnectError::Failed(reason)) = choose_subtype(&[PLAIN], true, true) else {
        panic!("Plain was accepted");
    };
    assert!(reason.contains("unencrypted"), "{reason}");
}

#[test]
fn vencrypt_is_preferred_and_can_be_required() {
    assert_eq!(choose_type(&[NONE, VNC_AUTH, VENCRYPT], Encryption::Prefer), Ok(VENCRYPT));
    assert_eq!(choose_type(&[VNC_AUTH, NONE], Encryption::Prefer), Ok(NONE));
    assert_eq!(choose_type(&[VNC_AUTH], Encryption::Prefer), Ok(VNC_AUTH));
    assert_eq!(choose_type(&[VNC_AUTH], Encryption::Require), Err(ConnectError::Unencrypted));
    assert!(matches!(choose_type(&[16], Encryption::Prefer), Err(ConnectError::Failed(_))));
}

#[tokio::test]
async fn no_security_in_rfb_3_8_reads_the_security_result() {
    let (near, mut far) = duplex(4096);
    let server = async {
        assert_eq!(offer(&mut far, b"RFB 003.008\n", &[NONE]).await, NONE);
        far.write_u32(0).await.unwrap();
        far
    };
    let config = config();
    let (mut far, result) = tokio::join!(server, negotiate(near, &config));
    let (mut handoff, security) = result.unwrap();
    assert_eq!(security, Security::None);
    assert!(!security.encrypted());
    played(&mut handoff, b"RFB 003.003\n\0\0\0\x01").await;
    client_init(&mut handoff, &mut far).await;
}

#[tokio::test]
async fn vnc_authentication_is_left_to_vnc_rs_at_the_servers_version() {
    let (near, mut far) = duplex(4096);
    let server = async {
        assert_eq!(offer(&mut far, b"RFB 003.007\n", &[VNC_AUTH]).await, VNC_AUTH);
        far.write_all(&[7; 16]).await.unwrap();
        far
    };
    let config = config();
    let (_far, result) = tokio::join!(server, negotiate(near, &config));
    let (mut handoff, security) = result.unwrap();
    assert_eq!(security, Security::VncAuth);
    played(&mut handoff, b"RFB 003.007\n\x01\x02").await;
    played(&mut handoff, &[7; 16]).await;
}

#[tokio::test]
async fn rfb_3_3_takes_the_type_the_server_decides() {
    let (near, mut far) = duplex(4096);
    let server = async {
        far.write_all(b"RFB 003.003\n").await.unwrap();
        let mut theirs = [0; 12];
        far.read_exact(&mut theirs).await.unwrap();
        far.write_u32(2).await.unwrap();
        far
    };
    let config = config();
    let (_far, result) = tokio::join!(server, negotiate(near, &config));
    let (mut handoff, security) = result.unwrap();
    assert_eq!(security, Security::VncAuth);
    played(&mut handoff, b"RFB 003.003\n\0\0\0\x02").await;
}

#[tokio::test]
async fn a_server_without_vencrypt_is_refused_when_encryption_is_required() {
    let (near, mut far) = duplex(4096);
    let server = async {
        far.write_all(b"RFB 003.008\n").await.unwrap();
        let mut theirs = [0; 12];
        far.read_exact(&mut theirs).await.unwrap();
        far.write_all(&[2, NONE, VNC_AUTH]).await.unwrap();
        far
    };
    let config = Config { encryption: Encryption::Require, ..config() };
    let (_far, result) = tokio::join!(server, negotiate(near, &config));
    assert_eq!(result.err(), Some(ConnectError::Unencrypted));
}

#[tokio::test]
async fn a_server_refusing_the_connection_gives_its_reason() {
    let (near, mut far) = duplex(4096);
    let server = async {
        far.write_all(b"RFB 003.008\n").await.unwrap();
        let mut theirs = [0; 12];
        far.read_exact(&mut theirs).await.unwrap();
        far.write_u8(0).await.unwrap();
        far.write_u32(9).await.unwrap();
        far.write_all(b"too many!").await.unwrap();
        far
    };
    let config = config();
    let (_far, result) = tokio::join!(server, negotiate(near, &config));
    assert_eq!(result.err(), Some(ConnectError::Failed("too many!".to_owned())));
}

#[tokio::test]
async fn x509_plain_sends_the_login_only_inside_pinned_tls() {
    let (near, mut far) = duplex(1 << 16);
    let server = async {
        assert_eq!(offer_vencrypt(&mut far, &[X509_PLAIN, X509_VNC]).await, Some(X509_PLAIN));
        far.write_u8(1).await.unwrap();
        let mut tls = acceptor().accept(far).await.unwrap();
        let (user_length, password_length) =
            (tls.read_u32().await.unwrap(), tls.read_u32().await.unwrap());
        let mut login = vec![0; (user_length + password_length) as usize];
        tls.read_exact(&mut login).await.unwrap();
        assert_eq!((user_length, password_length), (5, 6));
        assert_eq!(login, b"aliceSecret");
        tls.write_u32(0).await.unwrap();
        tls.flush().await.unwrap();
        tls
    };
    let config = Config {
        username: Some("alice".to_owned()),
        password: Some("Secret".to_owned()),
        ..config()
    };
    let (mut tls, result) = tokio::join!(server, negotiate(near, &config));
    let (mut handoff, security) = result.unwrap();
    assert_eq!(security, Security::X509Plain);
    assert!(security.encrypted());
    played(&mut handoff, b"RFB 003.003\n\0\0\0\x01").await;
    client_init(&mut handoff, &mut tls).await;
}

#[tokio::test]
async fn a_rejected_login_gives_the_servers_reason() {
    let (near, mut far) = duplex(1 << 16);
    let server = async {
        offer_vencrypt(&mut far, &[X509_PLAIN]).await;
        far.write_u8(1).await.unwrap();
        let mut tls = acceptor().accept(far).await.unwrap();
        let mut login = [0; 8 + 2];
        tls.read_exact(&mut login).await.unwrap();
        tls.write_u32(1).await.unwrap();
        tls.write_u32(4).await.unwrap();
        tls.write_all(b"nope").await.unwrap();
        tls.flush().await.unwrap();
        tls
    };
    let config =
        Config { username: Some("a".to_owned()), password: Some("b".to_owned()), ..config() };
    let (_tls, result) = tokio::join!(server, negotiate(near, &config));
    assert_eq!(result.err(), Some(ConnectError::LoginRejected("nope".to_owned())));
}

#[tokio::test]
async fn a_certificate_that_is_not_pinned_gets_no_credentials() {
    let (near, mut far) = duplex(1 << 16);
    let server = async {
        offer_vencrypt(&mut far, &[X509_PLAIN]).await;
        far.write_u8(1).await.unwrap();
        let mut rest = Vec::new();
        // The client hangs up without a word once it has seen the certificate, which can be
        // before the server is done with the handshake.
        if let Ok(mut tls) = acceptor().accept(far).await {
            let _ = tls.read_to_end(&mut rest).await;
        }
        rest
    };
    let config = Config {
        username: Some("alice".to_owned()),
        password: Some("Secret".to_owned()),
        certificate: CertificatePolicy::Pinned([1; 32]),
        ..config()
    };
    // Failing drops the stream, which ends the connection for the server.
    let (rest, result) = tokio::join!(server, negotiate(near, &config));
    assert_eq!(result.err(), Some(ConnectError::CertificateRejected));
    assert!(rest.is_empty(), "the server received {rest:?}");
}

#[tokio::test]
async fn an_asked_certificate_comes_with_its_fingerprint() {
    let (near, mut far) = duplex(1 << 16);
    let server = async {
        assert_eq!(offer_vencrypt(&mut far, &[X509_NONE]).await, Some(X509_NONE));
        far.write_u8(1).await.unwrap();
        let mut tls = acceptor().accept(far).await.unwrap();
        tls.write_u32(0).await.unwrap();
        tls.flush().await.unwrap();
        tls
    };
    let (prompts, mut asked) = mpsc::channel::<crate::CertificatePrompt>(1);
    let judge = async move {
        let prompt = asked.recv().await.unwrap();
        assert_eq!(prompt.sha256, pin());
        assert_eq!(prompt.der, cert());
        assert_eq!(prompt.server_name, "vnc.example");
        prompt.answer.send(true).unwrap();
    };
    let config = Config {
        server_name: "vnc.example".to_owned(),
        certificate: CertificatePolicy::Ask(prompts),
        ..Config::default()
    };
    let (_tls, result, ()) = tokio::join!(server, negotiate(near, &config), judge);
    assert_eq!(result.unwrap().1, Security::X509None);
}

#[tokio::test]
async fn x509_vnc_without_a_password_asks_for_one_before_tls() {
    let (near, mut far) = duplex(1 << 16);
    let server = async { offer_vencrypt(&mut far, &[X509_VNC]).await };
    let config = config();
    let (chosen, result) = tokio::join!(server, negotiate(near, &config));
    assert_eq!(result.err(), Some(ConnectError::PasswordRequired));
    assert_eq!(chosen, None, "the client chose a subtype");
}

/// A whole session over X509Vnc: `vnc-rs` answers the challenge inside TLS and gets a picture.
#[tokio::test]
async fn a_session_runs_over_x509_vnc() {
    let (near, mut far) = duplex(1 << 16);
    let server = async {
        assert_eq!(offer_vencrypt(&mut far, &[X509_VNC]).await, Some(X509_VNC));
        far.write_u8(1).await.unwrap();
        let mut tls = acceptor().accept(far).await.unwrap();
        tls.write_all(&[3; 16]).await.unwrap();
        tls.flush().await.unwrap();
        let mut response = [0; 16];
        tls.read_exact(&mut response).await.unwrap();
        tls.write_u32(0).await.unwrap();
        assert_eq!(tls.read_u8().await.unwrap(), 1, "the session is shared");
        tls.write_u16(4).await.unwrap();
        tls.write_u16(2).await.unwrap();
        tls.write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]).await.unwrap();
        tls.write_u32(3).await.unwrap();
        tls.write_all(b"tls").await.unwrap();
        tls.flush().await.unwrap();
        tls
    };
    let config = Config { password: Some("secret".to_owned()), ..config() };
    let (_tls, session) = tokio::join!(server, connect(near, config));
    let mut session = session.unwrap();
    assert_eq!(session.security, Security::X509Vnc);
    let event = timeout(WAIT, session.display.events.recv()).await.unwrap();
    assert!(matches!(event, Some(DisplayEvent::Resized { width: 4, height: 2 })), "{event:?}");
}
