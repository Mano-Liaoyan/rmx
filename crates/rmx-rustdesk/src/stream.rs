//! A framed connection that carries protobuf messages, encrypted once a key is set.

use std::io;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::FrameCodec;
use crate::crypto::Cipher;

/// Anything a connection can run over: a socket, or a channel through an SSH tunnel.
pub trait Transport: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Transport for T {}

type Boxed = Box<dyn Transport>;

/// Before a session logs in a frame stays small; RustDesk's own limit is similar.
pub(crate) const MAX_HANDSHAKE_FRAME: usize = 64 * 1024;
/// Video key frames of a large desktop are a few megabytes.
pub(crate) const MAX_SESSION_FRAME: usize = 64 * 1024 * 1024;

/// One side of an encrypted stream: the frames and the counter for that direction.
pub(crate) struct Connection {
    reader: FramedRead<ReadHalf<Boxed>, FrameCodec>,
    writer: FramedWrite<WriteHalf<Boxed>, FrameCodec>,
    cipher: Option<Cipher>,
}

impl Connection {
    pub(crate) fn new(transport: impl Transport) -> Self {
        let (read, write) = tokio::io::split(Box::new(transport) as Boxed);
        Self {
            reader: FramedRead::new(read, FrameCodec::new(MAX_HANDSHAKE_FRAME)),
            writer: FramedWrite::new(write, FrameCodec::new(MAX_HANDSHAKE_FRAME)),
            cipher: None,
        }
    }

    /// Encrypts everything from here on.
    pub(crate) fn set_cipher(&mut self, cipher: Cipher) {
        self.cipher = Some(cipher);
    }

    #[cfg(test)]
    pub(crate) fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Accepts frames up to `max` bytes from now on.
    pub(crate) fn set_max_frame(&mut self, max: usize) {
        *self.reader.decoder_mut() = FrameCodec::new(max);
    }

    pub(crate) async fn send(&mut self, message: &impl prost::Message) -> io::Result<()> {
        let plain = message.encode_to_vec();
        let frame = match self.cipher.as_mut() {
            Some(cipher) => cipher.seal(&plain),
            None => plain,
        };
        self.writer.send(Bytes::from(frame)).await
    }

    /// The next frame, decrypted, or `None` once the stream has ended.
    pub(crate) async fn receive(&mut self) -> io::Result<Option<Vec<u8>>> {
        let Some(frame) = self.reader.next().await.transpose()? else {
            return Ok(None);
        };
        match self.cipher.as_mut() {
            Some(cipher) => cipher
                .open(&frame)
                .map(Some)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            None => Ok(Some(frame.to_vec())),
        }
    }

    /// The next frame, or a timeout error if none arrives within `limit`.
    pub(crate) async fn receive_within(&mut self, limit: Duration) -> io::Result<Vec<u8>> {
        match tokio::time::timeout(limit, self.receive()).await {
            Ok(Ok(Some(frame))) => Ok(frame),
            Ok(Ok(None)) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(io::ErrorKind::TimedOut.into()),
        }
    }

    /// Splits the connection so reading and writing can run on separate tasks.
    pub(crate) fn split(self) -> (Reader, Writer) {
        let (receive, send) = match self.cipher {
            Some(cipher) => {
                let (send, receive) = cipher.split();
                (Some(receive), Some(send))
            },
            None => (None, None),
        };
        (
            Reader { frames: self.reader, cipher: receive },
            Writer { frames: self.writer, cipher: send },
        )
    }
}

/// The receiving half of a split [`Connection`].
pub(crate) struct Reader {
    frames: FramedRead<ReadHalf<Boxed>, FrameCodec>,
    cipher: Option<crate::crypto::Opener>,
}

impl Reader {
    pub(crate) async fn receive(&mut self) -> io::Result<Option<Vec<u8>>> {
        let Some(frame) = self.frames.next().await.transpose()? else {
            return Ok(None);
        };
        match self.cipher.as_mut() {
            Some(cipher) => cipher
                .open(&frame)
                .map(Some)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            None => Ok(Some(frame.to_vec())),
        }
    }
}

/// The sending half of a split [`Connection`].
pub(crate) struct Writer {
    frames: FramedWrite<WriteHalf<Boxed>, FrameCodec>,
    cipher: Option<crate::crypto::Sealer>,
}

impl Writer {
    pub(crate) async fn send(&mut self, message: &impl prost::Message) -> io::Result<()> {
        let plain = message.encode_to_vec();
        let frame = match self.cipher.as_mut() {
            Some(cipher) => cipher.seal(&plain),
            None => plain,
        };
        self.frames.send(Bytes::from(frame)).await
    }

    pub(crate) async fn close(&mut self) {
        let _ = self.frames.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Hash, Message, message};

    fn hash(salt: &str) -> Message {
        Message::of(message::Union::Hash(Hash { salt: salt.into(), challenge: "c".into() }))
    }

    #[tokio::test]
    async fn messages_cross_in_the_clear_and_encrypted() {
        let (a, b) = tokio::io::duplex(4096);
        let (mut a, mut b) = (Connection::new(a), Connection::new(b));
        a.send(&hash("plain")).await.unwrap();
        let frame = b.receive().await.unwrap().unwrap();
        assert_eq!(crate::proto::decode::<Message>(&frame), Some(hash("plain")));

        a.set_cipher(Cipher::shared(&[1; 32]));
        b.set_cipher(Cipher::shared(&[1; 32]));
        for round in 0..3 {
            a.send(&hash(&format!("secret {round}"))).await.unwrap();
            let frame = b.receive().await.unwrap().unwrap();
            assert_eq!(
                crate::proto::decode::<Message>(&frame),
                Some(hash(&format!("secret {round}")))
            );
        }
    }

    #[tokio::test]
    async fn split_halves_keep_their_counters() {
        let (a, b) = tokio::io::duplex(4096);
        let (mut a, mut b) = (Connection::new(a), Connection::new(b));
        a.set_cipher(Cipher::shared(&[2; 32]));
        b.set_cipher(Cipher::shared(&[2; 32]));
        a.send(&hash("before")).await.unwrap();
        assert!(b.receive().await.unwrap().is_some());
        b.send(&hash("reply")).await.unwrap();
        let (mut reader, mut writer) = a.split();
        assert!(reader.receive().await.unwrap().is_some());
        writer.send(&hash("after")).await.unwrap();
        let frame = b.receive().await.unwrap().unwrap();
        assert_eq!(crate::proto::decode::<Message>(&frame), Some(hash("after")));
    }

    #[tokio::test]
    async fn a_wrong_key_is_an_error() {
        let (a, b) = tokio::io::duplex(4096);
        let (mut a, mut b) = (Connection::new(a), Connection::new(b));
        a.set_cipher(Cipher::shared(&[1; 32]));
        b.set_cipher(Cipher::shared(&[2; 32]));
        a.send(&hash("x")).await.unwrap();
        assert!(b.receive().await.is_err());
    }
}
