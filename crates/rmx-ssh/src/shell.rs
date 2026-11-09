use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use russh::client::Msg;
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Pty, Sig};

use crate::client::SshClient;
use crate::error::{DisconnectReason, Error};

/// PTY settings for [`SshClient::open_shell`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellOptions {
    /// The `TERM` value for the remote shell.
    pub term: String,
    /// Columns.
    pub cols: u32,
    /// Rows.
    pub rows: u32,
    /// Width in pixels, or 0 if unknown.
    pub pixel_width: u32,
    /// Height in pixels, or 0 if unknown.
    pub pixel_height: u32,
    /// Terminal modes.
    pub modes: Vec<(Pty, u32)>,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            term: String::from("xterm-256color"),
            cols: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
            modes: Vec::new(),
        }
    }
}

/// How a shell ended, from [`ShellReader::exit`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShellExit {
    /// The shell exited with this status.
    Status(u32),
    /// The shell was killed by a signal.
    Signal {
        /// The signal name, for example `TERM`.
        name: String,
        /// Whether it dumped core.
        core_dumped: bool,
        /// The server's message, if any.
        message: String,
    },
    /// The connection ended first.
    Disconnected(DisconnectReason),
    /// The channel closed with no status.
    Closed,
}

/// The output half of a shell.
///
/// Holds at most [`ConnectOptions::channel_buffer`] undelivered packets.
/// Calling [`recv`](Self::recv) is what makes room, so a terminal driver that
/// stops calling it applies backpressure (see the crate docs).
///
/// [`ConnectOptions::channel_buffer`]: crate::ConnectOptions::channel_buffer
#[derive(Debug)]
pub struct ShellReader {
    channel: ChannelReadHalf,
    pending: VecDeque<Bytes>,
    exit: Option<ShellExit>,
    done: bool,
    client: SshClient,
}

impl ShellReader {
    /// Receives the next chunk of output, which on a PTY merges stdout and
    /// stderr. Returns `None` when the shell has ended; then see
    /// [`exit`](Self::exit). Cancel-safe.
    pub async fn recv(&mut self) -> Option<Bytes> {
        if let Some(data) = self.pending.pop_front() {
            return Some(data);
        }
        if self.done {
            return None;
        }
        loop {
            match self.channel.wait().await {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    return Some(data);
                },
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    self.exit = Some(ShellExit::Status(exit_status));
                },
                Some(ChannelMsg::ExitSignal {
                    signal_name, core_dumped, error_message, ..
                }) => {
                    self.exit = Some(ShellExit::Signal {
                        name: signal_name_text(signal_name),
                        core_dumped,
                        message: error_message,
                    });
                },
                Some(ChannelMsg::Close) | None => {
                    self.done = true;
                    if self.exit.is_none() {
                        self.exit = Some(match self.client.disconnect_reason() {
                            Some(reason) => ShellExit::Disconnected(reason),
                            None => ShellExit::Closed,
                        });
                    }
                    return None;
                },
                Some(_) => {},
            }
        }
    }

    /// How the shell ended. `None` until [`recv`](Self::recv) has returned `None`.
    pub fn exit(&self) -> Option<&ShellExit> {
        if self.done { self.exit.as_ref() } else { None }
    }
}

fn signal_name_text(signal: Sig) -> String {
    match signal {
        Sig::Custom(name) => name,
        other => format!("{other:?}"),
    }
}

/// The input half of a shell. Cheap to clone; resizing does not wait for a
/// write in progress on another clone.
#[derive(Clone, Debug)]
pub struct ShellWriter {
    inner: Arc<WriterInner>,
}

#[derive(Debug)]
struct WriterInner {
    channel: Arc<ChannelWriteHalf<Msg>>,
    client: SshClient,
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        // Without this the remote shell would run on after the last handle goes.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let channel = Arc::clone(&self.channel);
            runtime.spawn(async move {
                let _ = channel.close().await;
            });
        }
    }
}

impl ShellWriter {
    /// Sends bytes to the shell, waiting while the server's window is full.
    /// Not cancel-safe: if the future is dropped, part of `data` may have been
    /// sent.
    pub async fn write(&self, data: impl Into<Bytes>) -> Result<(), Error> {
        self.inner.channel.data_bytes(data).await.map_err(|e| self.inner.client.classify(e))
    }

    /// Changes the terminal size.
    pub async fn resize(
        &self,
        cols: u32,
        rows: u32,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Result<(), Error> {
        self.inner
            .channel
            .window_change(cols, rows, pixel_width, pixel_height)
            .await
            .map_err(|e| self.inner.client.classify(e))
    }

    /// Sends EOF, the same as closing the shell's input.
    pub async fn send_eof(&self) -> Result<(), Error> {
        self.inner.channel.eof().await.map_err(|e| self.inner.client.classify(e))
    }

    /// Closes the channel, ending the session on the server.
    pub async fn close(&self) -> Result<(), Error> {
        self.inner.channel.close().await.map_err(|e| self.inner.client.classify(e))
    }
}

/// Opens a PTY shell on `client`.
pub(crate) async fn open(
    client: &SshClient,
    options: &ShellOptions,
) -> Result<(ShellReader, ShellWriter), Error> {
    let mut channel = client.open_session_channel().await?;
    channel
        .request_pty(
            true,
            &options.term,
            options.cols,
            options.rows,
            options.pixel_width,
            options.pixel_height,
            &options.modes,
        )
        .await
        .map_err(|e| client.classify(e))?;
    let mut pending = VecDeque::new();
    client.wait_channel_reply(&mut channel, &mut pending, "PTY allocation").await?;
    channel.request_shell(true).await.map_err(|e| client.classify(e))?;
    client.wait_channel_reply(&mut channel, &mut pending, "shell").await?;

    let (read, write) = channel.split();
    let reader =
        ShellReader { channel: read, pending, exit: None, done: false, client: client.clone() };
    let writer = ShellWriter {
        inner: Arc::new(WriterInner { channel: Arc::new(write), client: client.clone() }),
    };
    Ok((reader, writer))
}
