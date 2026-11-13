//! The streams a session runs over once the security handshake is done.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::client::TlsStream;

use crate::handshake::Version;

/// The connection, encrypted or not.
pub(crate) enum Transport<S> {
    Plain(S),
    Tls(Box<TlsStream<S>>),
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Transport<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Transport<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => {
                // `vnc-rs` never flushes, and TLS can hold written records back when the
                // connection is busy: they go out before anything new is taken, and are pushed
                // after it, so a message waits at most for the next one.
                ready!(Pin::new(&mut *stream).poll_flush(cx))?;
                let written = ready!(Pin::new(&mut *stream).poll_write(cx, buf))?;
                let _ = Pin::new(stream).poll_flush(cx)?;
                Poll::Ready(Ok(written))
            },
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// A stream that first plays `vnc-rs` the start of a handshake already done, and swallows what
/// it answers, then passes everything through.
pub(crate) struct Handoff<S> {
    inner: S,
    script: [u8; 16],
    /// How much of `script` there is, and how much of it has been read.
    length: usize,
    read: usize,
    /// How many bytes `vnc-rs` writes that the server must not see.
    swallow: usize,
}

impl<S> Handoff<S> {
    /// For a handshake done up to ClientInit: RFB 3.3 with no security, which has no
    /// SecurityResult. Only the 12 bytes of the client's version are answered.
    pub(crate) fn authenticated(inner: S) -> Self {
        Self::new(inner, Version::V33, &[0, 0, 0, 1], 12)
    }

    /// For a handshake that has chosen VNC authentication, which `vnc-rs` then runs against the
    /// server: the challenge and the SecurityResult come from the server itself.
    pub(crate) fn vnc_auth(inner: S, version: Version) -> Self {
        match version {
            Version::V33 => Self::new(inner, version, &[0, 0, 0, 2], 12),
            Version::V37 | Version::V38 => Self::new(inner, version, &[1, 2], 13),
        }
    }

    fn new(inner: S, version: Version, security: &[u8], swallow: usize) -> Self {
        let mut script = [0; 16];
        script[..12].copy_from_slice(version.bytes());
        script[12..12 + security.len()].copy_from_slice(security);
        Self { inner, script, length: 12 + security.len(), read: 0, swallow }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Handoff<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.read < this.length {
            let left = &this.script[this.read..this.length];
            let taken = left.len().min(buf.remaining());
            buf.put_slice(&left[..taken]);
            this.read += taken;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Handoff<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.swallow > 0 {
            let taken = this.swallow.min(buf.len());
            this.swallow -= taken;
            return Poll::Ready(Ok(taken));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, duplex};

    use super::*;

    #[tokio::test]
    async fn a_handoff_replays_its_script_and_swallows_the_answer() {
        let (near, mut far) = duplex(64);
        let mut handoff = Handoff::vnc_auth(near, Version::V38);
        let mut script = [0; 14];
        handoff.read_exact(&mut script).await.unwrap();
        assert_eq!(&script, b"RFB 003.008\n\x01\x02");

        handoff.write_all(b"RFB 003.008\n").await.unwrap();
        handoff.write_all(&[2, 0xaa, 0xbb]).await.unwrap();
        handoff.flush().await.unwrap();
        let mut passed = [0; 2];
        far.read_exact(&mut passed).await.unwrap();
        assert_eq!(passed, [0xaa, 0xbb]);

        far.write_all(b"challenge").await.unwrap();
        let mut challenge = [0; 9];
        handoff.read_exact(&mut challenge).await.unwrap();
        assert_eq!(&challenge, b"challenge");
    }

    #[tokio::test]
    async fn an_authenticated_handoff_claims_rfb_3_3_without_security() {
        let (near, mut far) = duplex(64);
        let mut handoff = Handoff::authenticated(near);
        let mut script = [0; 16];
        handoff.read_exact(&mut script).await.unwrap();
        assert_eq!(&script, b"RFB 003.003\n\0\0\0\x01");
        handoff.write_all(b"RFB 003.003\n\x01").await.unwrap();
        let mut passed = [0; 1];
        far.read_exact(&mut passed).await.unwrap();
        assert_eq!(passed, [1], "ClientInit reaches the server");
    }
}
