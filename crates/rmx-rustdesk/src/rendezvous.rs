//! Reaching a peer by its ID: through a rendezvous server, and a relay when there is no direct
//! way.
//!
//! The client asks the rendezvous server (`hbbs`) to connect it to the peer. The peer either
//! answers with an address the client can reach, which only works on the same network without
//! hole punching, or opens a relay (`hbbr`) session that the client then joins with the same
//! UUID. Both answers carry the peer's identity key, signed by the rendezvous server.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use log::{debug, info};
use ring::rand::{SecureRandom, SystemRandom};

use crate::crypto::{Key, key_of, open_signed};
use crate::proto::{
    self, ConnType, IdPk, NatType, PunchFailure, PunchHoleRequest, RelayResponse,
    RendezvousMessage, RequestRelay, rendezvous_message,
};
use crate::stream::{Connection, Transport};
use crate::{ConnectError, Server};

/// The port of the rendezvous server.
pub const RENDEZVOUS_PORT: u16 = 21116;
/// The port of the relay server.
pub const RELAY_PORT: u16 = 21117;

/// The public key of RustDesk's own public servers.
pub const PUBLIC_SERVER_KEY: &str = "OeVuKk5nlHiXp+APNn0Y3pC1Iwpwn44JGqrQCsWqmBw=";

/// How long each attempt at the rendezvous server waits, times its number.
const ATTEMPT_STEP: Duration = Duration::from_secs(3);
const ATTEMPTS: u32 = 3;
/// How long a direct connection to a peer on the same network may take before the relay is used.
const LOCAL_CONNECT: Duration = Duration::from_secs(3);
const RELAY_ANSWER: Duration = Duration::from_secs(18);

/// A boxed future, as the dialer returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Opens connections to the hosts a session needs.
///
/// Reaching a peer by ID takes up to three connections: the rendezvous server, the relay and
/// the peer itself. A dialer can route them all through a tunnel.
pub trait Dial: Send + Sync {
    fn dial<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> BoxFuture<'a, io::Result<Box<dyn Transport>>>;
}

/// A dialer of plain TCP connections.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpDialer;

impl Dial for TcpDialer {
    fn dial<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> BoxFuture<'a, io::Result<Box<dyn Transport>>> {
        Box::pin(async move {
            let stream = tokio::net::TcpStream::connect((host, port)).await?;
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream) as Box<dyn Transport>)
        })
    }
}

/// A connection to the peer and, if the rendezvous server vouched for it, the peer's identity key.
pub(crate) struct Reached {
    pub(crate) connection: Connection,
    pub(crate) peer_key: Option<Key>,
    pub(crate) relayed: bool,
}

/// Splits `host:port`, or takes `default` when there is no port. IPv6 addresses go in brackets.
pub(crate) fn host_port(address: &str, default: u16) -> (String, u16) {
    let address = address.trim();
    if let Some(rest) = address.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail.strip_prefix(':').and_then(|port| port.parse().ok());
        return (host.to_owned(), port.unwrap_or(default));
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host.to_owned(), port),
            Err(_) => (address.to_owned(), default),
        },
        _ => (address.to_owned(), default),
    }
}

/// The rendezvous server's public key, as configured or RustDesk's own.
fn server_key(server: &Server) -> Result<Key, ConnectError> {
    use base64::Engine;
    let text = server.key.as_deref().map(str::trim).filter(|key| !key.is_empty());
    let text = text.unwrap_or(PUBLIC_SERVER_KEY);
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()
        .and_then(|bytes| key_of(&bytes).ok())
        .ok_or(ConnectError::BadServerKey)
}

/// The peer's identity key out of what the rendezvous server signed, if it is for `peer_id`.
pub(crate) fn peer_key(
    signed: &[u8],
    server_key: &Key,
    peer_id: &str,
) -> Result<Key, ConnectError> {
    let message = open_signed(signed, server_key).map_err(|_| ConnectError::ServerKeyMismatch)?;
    let id_pk: IdPk = proto::decode(message).ok_or(ConnectError::ServerKeyMismatch)?;
    if id_pk.id != peer_id {
        return Err(ConnectError::ServerKeyMismatch);
    }
    key_of(&id_pk.pk).map_err(|_| ConnectError::ServerKeyMismatch)
}

/// What the rendezvous server answered.
enum Answer {
    /// The peer opened a relay session.
    Relay(RelayResponse),
    /// The peer can be reached at `address` if it is on the same network.
    Punched { address: SocketAddr, pk: Bytes, relay_server: String, local: bool },
}

/// Reaches `peer_id` through `server`.
pub(crate) async fn reach(
    dialer: &dyn Dial,
    server: &Server,
    peer_id: &str,
) -> Result<Reached, ConnectError> {
    let key = server_key(server)?;
    let licence = server.key.clone().unwrap_or_default();
    let (host, port) = host_port(&server.host, RENDEZVOUS_PORT);
    let mut rendezvous = Connection::new(dial(dialer, &host, port, "the rendezvous server").await?);
    info!("asking {host}:{port} for RustDesk peer {peer_id}");

    let request =
        RendezvousMessage::of(rendezvous_message::Union::PunchHoleRequest(PunchHoleRequest {
            id: peer_id.to_owned(),
            // Claiming a symmetric NAT and asking for the relay keeps the peer from waiting for a
            // UDP or TCP punch this client does not attempt.
            nat_type: NatType::Symmetric as i32,
            licence_key: licence.clone(),
            conn_type: ConnType::DefaultConn as i32,
            version: crate::PEER_VERSION.to_owned(),
            force_relay: true,
            ..Default::default()
        }));
    let answer = ask(&mut rendezvous, &request).await?;
    drop(rendezvous);

    match answer {
        Answer::Relay(response) => {
            let peer_key = verify(&response.signed_pk(), &key, peer_id)?;
            let relay = relay_address(&response.relay_server, &host);
            let connection = join_relay(dialer, &relay, &response.uuid, &licence, peer_id).await?;
            Ok(Reached { connection, peer_key, relayed: true })
        },
        Answer::Punched { address, pk, relay_server, local } => {
            let peer_key = verify(&pk, &key, peer_id)?;
            if local {
                let ip = address.ip().to_string();
                match tokio::time::timeout(LOCAL_CONNECT, dialer.dial(&ip, address.port())).await {
                    Ok(Ok(stream)) => {
                        info!("reached {peer_id} directly at {address}");
                        let connection = Connection::new(stream);
                        return Ok(Reached { connection, peer_key, relayed: false });
                    },
                    Ok(Err(err)) => debug!("direct connection to {address} failed: {err}"),
                    Err(_) => debug!("direct connection to {address} timed out"),
                }
            }
            if relay_server.is_empty() {
                return Err(ConnectError::Failed("the peer offered no relay".to_owned()));
            }
            let relay = relay_address(&relay_server, &host);
            let uuid =
                request_relay(dialer, (&host, port), &relay, peer_id, peer_key.is_some()).await?;
            let connection = join_relay(dialer, &relay, &uuid, &licence, peer_id).await?;
            Ok(Reached { connection, peer_key, relayed: true })
        },
    }
}

/// The peer's key from what the server signed: absent if the server sent nothing, an error if
/// what it sent does not verify.
fn verify(signed: &[u8], server_key: &Key, peer_id: &str) -> Result<Option<Key>, ConnectError> {
    if signed.is_empty() {
        return Ok(None);
    }
    peer_key(signed, server_key, peer_id).map(Some)
}

fn relay_address(relay_server: &str, rendezvous_host: &str) -> (String, u16) {
    if relay_server.trim().is_empty() {
        (rendezvous_host.to_owned(), RELAY_PORT)
    } else {
        host_port(relay_server, RELAY_PORT)
    }
}

async fn dial(
    dialer: &dyn Dial,
    host: &str,
    port: u16,
    what: &str,
) -> Result<Box<dyn Transport>, ConnectError> {
    match tokio::time::timeout(crate::CONNECT_TIMEOUT, dialer.dial(host, port)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => {
            Err(ConnectError::Failed(format!("could not reach {what} at {host}:{port}: {err}")))
        },
        Err(_) => Err(ConnectError::Failed(format!("{what} at {host}:{port} did not answer"))),
    }
}

/// Sends the punch request until the server answers, at most [`ATTEMPTS`] times.
async fn ask(
    rendezvous: &mut Connection,
    request: &RendezvousMessage,
) -> Result<Answer, ConnectError> {
    for attempt in 1..=ATTEMPTS {
        rendezvous.send(request).await.map_err(io_failure)?;
        let deadline = tokio::time::Instant::now() + ATTEMPT_STEP * attempt;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let frame = match rendezvous.receive_within(left).await {
                Ok(frame) => frame,
                Err(err) if err.kind() == io::ErrorKind::TimedOut => break,
                Err(err) => return Err(io_failure(err)),
            };
            let Some(message) = proto::decode::<RendezvousMessage>(&frame) else { continue };
            match message.union {
                Some(rendezvous_message::Union::RelayResponse(response)) => {
                    if !response.refuse_reason.is_empty() {
                        return Err(ConnectError::Failed(response.refuse_reason));
                    }
                    return Ok(Answer::Relay(response));
                },
                Some(rendezvous_message::Union::PunchHoleResponse(response)) => {
                    if response.socket_addr.is_empty() {
                        return Err(punch_failure(&response));
                    }
                    let local = matches!(
                        response.union,
                        Some(proto::punch_hole_response::Union::IsLocal(true))
                    );
                    return Ok(Answer::Punched {
                        address: decode_address(&response.socket_addr),
                        pk: response.pk,
                        relay_server: response.relay_server,
                        local,
                    });
                },
                // A server offering to encrypt this connection; nothing here is secret.
                Some(rendezvous_message::Union::KeyExchange(_)) => {},
                _ => debug!("ignored a rendezvous message"),
            }
        }
    }
    Err(ConnectError::Failed("the rendezvous server did not answer".to_owned()))
}

fn punch_failure(response: &proto::PunchHoleResponse) -> ConnectError {
    if !response.other_failure.is_empty() {
        return ConnectError::Failed(response.other_failure.clone());
    }
    match PunchFailure::try_from(response.failure) {
        Ok(PunchFailure::IdNotExist) => ConnectError::UnknownId,
        Ok(PunchFailure::Offline) => ConnectError::Offline,
        Ok(PunchFailure::LicenseMismatch) => {
            ConnectError::Failed("the server key does not match".to_owned())
        },
        Ok(PunchFailure::LicenseOveruse) => {
            ConnectError::Failed("the server key is over its limit".to_owned())
        },
        Err(_) => ConnectError::Failed("the rendezvous server could not reach the peer".to_owned()),
    }
}

/// Asks the rendezvous server to have the peer meet this client at `relay`, and returns the UUID
/// the two will use there.
async fn request_relay(
    dialer: &dyn Dial,
    (host, port): (&str, u16),
    relay: &(String, u16),
    peer_id: &str,
    secure: bool,
) -> Result<String, ConnectError> {
    let mut rendezvous = Connection::new(dial(dialer, host, port, "the rendezvous server").await?);
    let uuid = new_uuid();
    let relay_server =
        if relay.1 == RELAY_PORT { relay.0.clone() } else { format!("{}:{}", relay.0, relay.1) };
    let request = RendezvousMessage::of(rendezvous_message::Union::RequestRelay(RequestRelay {
        id: peer_id.to_owned(),
        uuid: uuid.clone(),
        relay_server,
        secure,
        ..Default::default()
    }));
    rendezvous.send(&request).await.map_err(io_failure)?;
    loop {
        let frame = rendezvous.receive_within(RELAY_ANSWER).await.map_err(io_failure)?;
        match proto::decode::<RendezvousMessage>(&frame).and_then(|message| message.union) {
            Some(rendezvous_message::Union::RelayResponse(response)) => {
                if !response.refuse_reason.is_empty() {
                    return Err(ConnectError::Failed(response.refuse_reason));
                }
                return Ok(uuid);
            },
            Some(rendezvous_message::Union::KeyExchange(_)) => {},
            _ => {
                return Err(ConnectError::Failed(
                    "the rendezvous server refused the relay".to_owned(),
                ));
            },
        }
    }
}

/// Joins the relay session `uuid`, after which the stream leads to the peer.
async fn join_relay(
    dialer: &dyn Dial,
    (host, port): &(String, u16),
    uuid: &str,
    licence: &str,
    peer_id: &str,
) -> Result<Connection, ConnectError> {
    check_uuid(uuid)?;
    let mut relay = Connection::new(dial(dialer, host, *port, "the relay server").await?);
    let request = RendezvousMessage::of(rendezvous_message::Union::RequestRelay(RequestRelay {
        id: peer_id.to_owned(),
        uuid: uuid.to_owned(),
        licence_key: licence.to_owned(),
        conn_type: ConnType::DefaultConn as i32,
        ..Default::default()
    }));
    relay.send(&request).await.map_err(io_failure)?;
    info!("joined relay {host}:{port} for {peer_id}");
    Ok(relay)
}

/// A relay UUID comes from the network and goes to the relay, so it is held to what one looks like.
fn check_uuid(uuid: &str) -> Result<(), ConnectError> {
    let valid = !uuid.is_empty()
        && uuid.len() <= 128
        && uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid
        .then_some(())
        .ok_or_else(|| ConnectError::Failed("the relay session is not valid".to_owned()))
}

/// A random version 4 UUID.
fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).expect("the system has a random number generator");
    bytes[6] = bytes[6] & 0x0F | 0x40;
    bytes[8] = bytes[8] & 0x3F | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// RustDesk's obfuscated socket address: IPv4 mixed with a timestamp, or IPv6 bytes and a port.
pub(crate) fn decode_address(bytes: &[u8]) -> SocketAddr {
    if bytes.len() == 18 {
        let ip: [u8; 16] = bytes[..16].try_into().expect("16 bytes");
        let port = u16::from_le_bytes([bytes[16], bytes[17]]);
        return SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port);
    }
    if bytes.len() > 16 {
        return SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    }
    let mut padded = [0u8; 16];
    padded[..bytes.len()].copy_from_slice(bytes);
    let number = u128::from_le_bytes(padded);
    let tm = (number >> 17) & u128::from(u32::MAX);
    let ip = ((number >> 49).wrapping_sub(tm) as u32).to_le_bytes();
    let port = (number & 0xFF_FFFF).wrapping_sub(tm & 0xFFFF) as u16;
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port))
}

fn io_failure(err: io::Error) -> ConnectError {
    ConnectError::Failed(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RustDesk's encoder, for checking the decoder against.
    fn encode_address(addr: SocketAddrV4, tm: u32) -> Vec<u8> {
        let tm = u128::from(tm);
        let ip = u128::from(u32::from_le_bytes(addr.ip().octets()));
        let port = u128::from(addr.port());
        let v = ((ip + tm) << 49) | (tm << 17) | (port + (tm & 0xFFFF));
        let bytes = v.to_le_bytes();
        let used = 16 - bytes.iter().rev().take_while(|&&b| b == 0).count();
        bytes[..used].to_vec()
    }

    #[test]
    fn mangled_ipv4_addresses_decode() {
        for (ip, port, tm) in [
            ([192, 168, 1, 20], 21118, 123_456_789),
            ([10, 0, 0, 1], 1, 0),
            ([8, 8, 8, 8], 65535, u32::MAX),
        ] {
            let addr = SocketAddrV4::new(Ipv4Addr::from(ip), port);
            assert_eq!(
                decode_address(&encode_address(addr, tm)),
                SocketAddr::V4(addr),
                "{addr} at {tm}"
            );
        }
    }

    #[test]
    fn ipv6_addresses_decode() {
        let mut bytes = Ipv6Addr::LOCALHOST.octets().to_vec();
        bytes.extend_from_slice(&21118u16.to_le_bytes());
        assert_eq!(decode_address(&bytes), "[::1]:21118".parse().unwrap());
    }

    #[test]
    fn host_and_port_split() {
        assert_eq!(host_port("rs.example.com", 21116), ("rs.example.com".into(), 21116));
        assert_eq!(host_port("rs.example.com:9000", 21116), ("rs.example.com".into(), 9000));
        assert_eq!(host_port("10.0.0.2:21117", 1), ("10.0.0.2".into(), 21117));
        assert_eq!(host_port("[::1]:5", 1), ("::1".into(), 5));
        assert_eq!(host_port("[::1]", 7), ("::1".into(), 7));
        assert_eq!(host_port("fe80::1", 7), ("fe80::1".into(), 7));
    }

    #[test]
    fn uuids_are_version_4_and_valid_for_the_relay() {
        let uuid = new_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert!(check_uuid(&uuid).is_ok());
        assert!(check_uuid("").is_err());
        assert!(check_uuid("a b").is_err());
    }

    #[test]
    fn the_public_key_decodes() {
        let server = Server { host: "rs-ny.rustdesk.com".into(), key: None };
        assert!(server_key(&server).is_ok());
        let bad = Server { host: String::new(), key: Some("not base64!".into()) };
        assert_eq!(server_key(&bad).err(), Some(ConnectError::BadServerKey));
    }

    #[test]
    fn a_peer_key_needs_the_server_signature_and_the_right_id() {
        use ring::signature::{Ed25519KeyPair, KeyPair};
        let server = Ed25519KeyPair::from_seed_unchecked(&[1; 32]).unwrap();
        let server_public = key_of(server.public_key().as_ref()).unwrap();
        let id_pk = IdPk { id: "123".into(), pk: Bytes::from(vec![9; 32]), kx_version: 1 };
        let body = proto::encode(&id_pk);
        let mut signed = server.sign(&body).as_ref().to_vec();
        signed.extend_from_slice(&body);

        assert_eq!(peer_key(&signed, &server_public, "123"), Ok([9; 32]));
        assert_eq!(peer_key(&signed, &server_public, "456"), Err(ConnectError::ServerKeyMismatch));
        assert_eq!(peer_key(&signed, &[0; 32], "123"), Err(ConnectError::ServerKeyMismatch));
        assert_eq!(verify(&[], &server_public, "123"), Ok(None));
    }
}
