use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use russh::ChannelStream;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::client::SshClient;

/// A tunnelled byte stream over an SSH channel, from
/// [`SshClient::open_direct_tcpip`] or [`SshClient::open_sftp`].
///
/// Writes wait for the remote window, so a slow peer slows the writer.
/// `shutdown` sends EOF on the channel; dropping the stream closes the channel.
/// The stream keeps its connection (and any jump hosts) alive. Drop it inside a
/// Tokio runtime, as closing the channel spawns a task.
pub struct SshStream {
    inner: ChannelStream<Msg>,
    // Declared after `inner` so the channel closes before the connection may.
    _client: SshClient,
}

impl SshStream {
    pub(crate) fn new(inner: ChannelStream<Msg>, client: SshClient) -> Self {
        Self { inner, _client: client }
    }
}

impl AsyncRead for SshStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
