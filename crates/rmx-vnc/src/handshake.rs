//! The security handshake, done here rather than by `vnc-rs` so that VeNCrypt can be offered.
//!
//! [`negotiate`] reads the server's version and security types and runs the chosen security up
//! to where `vnc-rs` can take over: right before the challenge for VNC authentication, right
//! before ClientInit for everything else. [`Handoff`] then plays `vnc-rs` a short handshake of
//! its own that it can only answer with that choice, and swallows the answers, so the bytes
//! after them go to the real server.
//!
//! VeNCrypt 0.2 is spoken with these subtypes, in the order the server lists them:
//!
//! - X509None, X509Vnc and X509Plain: TLS, then no authentication, a VNC password, or a user name
//!   and password. The certificate is judged by the [`CertificatePolicy`].
//! - TLSNone, TLSVnc and TLSPlain are refused: they need anonymous Diffie-Hellman, which `rustls`
//!   does not implement, and which no certificate could protect from a machine in the middle.
//! - Plain is refused, as it sends the password unencrypted.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::stream::{Handoff, Transport};
use crate::{Config, ConnectError, Encryption, Security, tls};

/// The security types of RFB.
const NONE: u8 = 1;
const VNC_AUTH: u8 = 2;
const VENCRYPT: u8 = 19;

/// The VeNCrypt subtypes.
const PLAIN: u32 = 256;
const TLS_NONE: u32 = 257;
const TLS_VNC: u32 = 258;
const TLS_PLAIN: u32 = 259;
const X509_NONE: u32 = 260;
const X509_VNC: u32 = 261;
const X509_PLAIN: u32 = 262;

/// The most of a failure reason that is kept.
const MAX_REASON: u32 = 4096;

/// The RFB versions there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    V33,
    V37,
    V38,
}

impl Version {
    /// Reads a server's version as `vnc-rs` does: anything unknown speaks 3.3.
    fn parse(bytes: &[u8; 12]) -> Self {
        match bytes {
            b"RFB 003.007\n" => Self::V37,
            b"RFB 003.008\n" => Self::V38,
            _ => Self::V33,
        }
    }

    pub(crate) fn bytes(self) -> &'static [u8; 12] {
        match self {
            Self::V33 => b"RFB 003.003\n",
            Self::V37 => b"RFB 003.007\n",
            Self::V38 => b"RFB 003.008\n",
        }
    }
}

/// Runs the security handshake on `stream`, ready for `vnc-rs` to finish.
pub(crate) async fn negotiate<S>(
    mut stream: S,
    config: &Config,
) -> Result<(Handoff<Transport<S>>, Security), ConnectError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin,
{
    let mut theirs = [0; 12];
    stream.read_exact(&mut theirs).await?;
    let version = Version::parse(&theirs);
    stream.write_all(version.bytes()).await?;

    let offered = read_security_types(&mut stream, version).await?;
    let choice = choose_type(&offered, config.encryption)?;
    if version != Version::V33 {
        stream.write_all(&[choice]).await?;
    }
    match choice {
        NONE => {
            if version == Version::V38 {
                expect_success(&mut stream, version).await.map_err(ConnectError::Failed)?;
            }
            Ok((Handoff::authenticated(Transport::Plain(stream)), Security::None))
        },
        VNC_AUTH => Ok((Handoff::vnc_auth(Transport::Plain(stream), version), Security::VncAuth)),
        _ => vencrypt(stream, version, config).await,
    }
}

async fn read_security_types<S>(stream: &mut S, version: Version) -> Result<Vec<u8>, ConnectError>
where
    S: AsyncRead + Unpin,
{
    if version == Version::V33 {
        return match stream.read_u32().await? {
            0 => Err(ConnectError::Failed(read_reason(stream).await?)),
            kind @ (1 | 2) => Ok(vec![kind as u8]),
            kind => Err(ConnectError::Failed(format!("unknown security type {kind}"))),
        };
    }
    let count = stream.read_u8().await?;
    if count == 0 {
        return Err(ConnectError::Failed(read_reason(stream).await?));
    }
    let mut offered = vec![0; usize::from(count)];
    stream.read_exact(&mut offered).await?;
    Ok(offered)
}

/// The security type to use of those `offered`.
fn choose_type(offered: &[u8], encryption: Encryption) -> Result<u8, ConnectError> {
    if offered.contains(&VENCRYPT) {
        return Ok(VENCRYPT);
    }
    if encryption == Encryption::Require {
        return Err(ConnectError::Unencrypted);
    }
    [NONE, VNC_AUTH].into_iter().find(|kind| offered.contains(kind)).ok_or_else(|| {
        ConnectError::Failed(format!("the server offers no supported security type: {offered:?}"))
    })
}

/// The VeNCrypt subtype to use of those `offered`, given what credentials there are.
///
/// The server's order is kept, but a subtype is passed over while a later one needs only what
/// is at hand. If none does, the first supported one says what is missing.
fn choose_subtype(
    offered: &[u32],
    username: bool,
    password: bool,
) -> Result<(u32, Security), ConnectError> {
    let mut missing = None;
    for &subtype in offered {
        let (security, lacking) = match subtype {
            X509_NONE => (Security::X509None, None),
            X509_VNC => (Security::X509Vnc, (!password).then_some(ConnectError::PasswordRequired)),
            X509_PLAIN => (
                Security::X509Plain,
                (!(username && password)).then_some(ConnectError::LoginRequired),
            ),
            _ => continue,
        };
        match lacking {
            None => return Ok((subtype, security)),
            Some(error) => {
                missing.get_or_insert(error);
            },
        }
    }
    if let Some(error) = missing {
        return Err(error);
    }
    let reason = if offered.iter().any(|subtype| [TLS_NONE, TLS_VNC, TLS_PLAIN].contains(subtype)) {
        "the server offers only anonymous TLS (TLSNone, TLSVnc or TLSPlain), which cannot be \
         verified; give it a certificate and the X509None, X509Vnc or X509Plain security types"
            .to_owned()
    } else if offered.contains(&PLAIN) {
        "the server offers only VeNCrypt Plain, which sends the password unencrypted".to_owned()
    } else {
        format!("the server offers no supported VeNCrypt subtype: {offered:?}")
    };
    Err(ConnectError::Failed(reason))
}

async fn vencrypt<S>(
    mut stream: S,
    version: Version,
    config: &Config,
) -> Result<(Handoff<Transport<S>>, Security), ConnectError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin,
{
    let (major, minor) = (stream.read_u8().await?, stream.read_u8().await?);
    if (major, minor) < (0, 2) {
        let reason = format!("the server speaks VeNCrypt {major}.{minor}, and 0.2 is needed");
        return Err(ConnectError::Failed(reason));
    }
    stream.write_all(&[0, 2]).await?;
    if stream.read_u8().await? != 0 {
        return Err(ConnectError::Failed("the server refused VeNCrypt 0.2".to_owned()));
    }
    let count = stream.read_u8().await?;
    let mut offered = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        offered.push(stream.read_u32().await?);
    }
    let (username, password) = (config.username.as_deref(), config.password.as_deref());
    let (subtype, security) = choose_subtype(&offered, username.is_some(), password.is_some())?;
    stream.write_u32(subtype).await?;
    if stream.read_u8().await? == 0 {
        return Err(ConnectError::Failed("the server could not start TLS".to_owned()));
    }

    let mut stream = tls::upgrade(stream, &config.server_name, &config.certificate).await?;
    match subtype {
        X509_VNC => return Ok((Handoff::vnc_auth(Transport::Tls(stream), version), security)),
        X509_PLAIN => {
            let (username, password) = (username.unwrap_or_default(), password.unwrap_or_default());
            let mut login = Vec::with_capacity(8 + username.len() + password.len());
            login.extend_from_slice(&(username.len() as u32).to_be_bytes());
            login.extend_from_slice(&(password.len() as u32).to_be_bytes());
            login.extend_from_slice(username.as_bytes());
            login.extend_from_slice(password.as_bytes());
            stream.write_all(&login).await?;
            stream.flush().await?;
            expect_success(&mut stream, version).await.map_err(ConnectError::LoginRejected)?;
        },
        _ => expect_success(&mut stream, version).await.map_err(ConnectError::Failed)?,
    }
    Ok((Handoff::authenticated(Transport::Tls(stream)), security))
}

/// Reads a SecurityResult, giving the server's reason for a failure.
async fn expect_success<S>(stream: &mut S, version: Version) -> Result<(), String>
where
    S: AsyncRead + Unpin,
{
    let failed = |error: io::Error| error.to_string();
    match stream.read_u32().await.map_err(failed)? {
        0 => Ok(()),
        _ if version == Version::V38 => {
            Err(read_reason(stream).await.map_err(|error| error.to_string())?)
        },
        _ => Err("the server refused the authentication".to_owned()),
    }
}

/// Reads a reason string, keeping at most [`MAX_REASON`] bytes of it.
async fn read_reason<S>(stream: &mut S) -> Result<String, ConnectError>
where
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await?;
    let mut reason = vec![0; length.min(MAX_REASON) as usize];
    stream.read_exact(&mut reason).await?;
    Ok(String::from_utf8_lossy(&reason).into_owned())
}

#[cfg(test)]
mod tests;
