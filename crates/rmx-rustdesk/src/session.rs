//! The tasks behind a [`DisplayHandle`](rmx_display::DisplayHandle).
//!
//! One task reads the peer and turns interface input into messages; another writes, so a slow
//! socket never holds up reading. Video goes to a decoding thread of its own (see `video`),
//! which also owns the event channel to the interface.

use std::io::Read;
use std::thread;

use bytes::Bytes;
use log::{debug, info, warn};
use rmx_display::{DisplayEvent, InputEvent};
use tokio::sync::mpsc;

use crate::input::{InputMapper, Platform};
use crate::proto::{
    self, BoolOption, Clipboard, ClipboardFormat, ImageQuality, Message, OptionMessage, PeerInfo,
    PreferCodec, SupportedDecoding, message, misc,
};
use crate::stream::{Connection, Reader, Writer};
use crate::video::{self, Codec, Feed, Request};
use crate::{ConnectError, Quality};

/// How many decoded-frame batches may wait for the video thread before reading stops.
const VIDEO_QUEUE: usize = 4;
/// How many messages may wait for the socket.
const SEND_QUEUE: usize = 64;
/// Clipboard text beyond this is not taken from the peer.
const MAX_CLIPBOARD: usize = 16 * 1024 * 1024;

/// The options a session asks the peer for.
pub(crate) fn options(quality: Quality) -> OptionMessage {
    let mut decoding = SupportedDecoding::default();
    for codec in video::supported() {
        match codec {
            Codec::Vp8 => decoding.ability_vp8 = 1,
            Codec::Vp9 => decoding.ability_vp9 = 1,
            Codec::Av1 => decoding.ability_av1 = 1,
            Codec::H264 => decoding.ability_h264 = 1,
            Codec::H265 => decoding.ability_h265 = 1,
        }
    }
    decoding.prefer = match video::supported().first() {
        Some(Codec::Vp8) => PreferCodec::Vp8,
        Some(Codec::Av1) => PreferCodec::Av1,
        Some(Codec::H264) => PreferCodec::H264,
        Some(Codec::H265) => PreferCodec::H265,
        Some(Codec::Vp9) | None => PreferCodec::Vp9,
    } as i32;
    let image_quality = match quality {
        Quality::Best => ImageQuality::Best,
        Quality::Balanced => ImageQuality::Balanced,
        Quality::Fast => ImageQuality::Low,
    };
    OptionMessage {
        image_quality: image_quality as i32,
        disable_audio: BoolOption::Yes as i32,
        supported_decoding: Some(decoding),
        ..Default::default()
    }
}

/// Starts the session's tasks on the current runtime.
pub(crate) fn start(
    connection: Connection,
    info: PeerInfo,
    early: Vec<Message>,
    input: mpsc::Receiver<InputEvent>,
    events: mpsc::Sender<DisplayEvent>,
) -> Result<(), ConnectError> {
    let (feed_tx, feed_rx) = mpsc::channel(VIDEO_QUEUE);
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let runtime = tokio::runtime::Handle::current();
    thread::Builder::new()
        .name("rmx-rustdesk-video".to_owned())
        .spawn(move || runtime.block_on(video::run(feed_rx, events, request_tx)))
        .map_err(|err| ConnectError::Failed(err.to_string()))?;

    let (reader, writer) = connection.split();
    let (send_tx, send_rx) = mpsc::channel(SEND_QUEUE);
    tokio::spawn(write(writer, send_rx));
    let session = Session {
        mapper: InputMapper::new(Platform::of(&info.platform)),
        feed: feed_tx,
        send: send_tx,
    };
    tokio::spawn(session.run(reader, info, early, input, request_rx));
    Ok(())
}

async fn write(mut writer: Writer, mut queue: mpsc::Receiver<Message>) {
    while let Some(message) = queue.recv().await {
        if let Err(err) = writer.send(&message).await {
            debug!("sending to the peer failed: {err}");
            break;
        }
    }
    writer.close().await;
}

struct Session {
    mapper: InputMapper,
    feed: mpsc::Sender<Feed>,
    send: mpsc::Sender<Message>,
}

/// Why the session should end, if it should.
type End = Option<Option<String>>;

impl Session {
    async fn run(
        mut self,
        mut reader: Reader,
        info: PeerInfo,
        early: Vec<Message>,
        mut input: mpsc::Receiver<InputEvent>,
        mut requests: mpsc::UnboundedReceiver<Request>,
    ) {
        info!(
            "RustDesk session with {} ({}, version {})",
            info.hostname, info.platform, info.version
        );
        let mut reason = self.peer_info(&info).await;
        for message in early {
            if reason.is_some() {
                break;
            }
            reason = self.handle(message).await;
        }
        let reason = match reason {
            Some(reason) => reason,
            None => loop {
                let end = tokio::select! {
                    frame = reader.receive() => match frame {
                        Ok(Some(frame)) => match proto::decode::<Message>(&frame) {
                            Some(message) => self.handle(message).await,
                            None => None,
                        },
                        Ok(None) => Some(Some("The peer closed the session".to_owned())),
                        Err(err) => Some(Some(format!("The connection failed: {err}"))),
                    },
                    event = input.recv() => match event {
                        Some(InputEvent::Disconnect) | None => Some(None),
                        Some(event) => self.input(event).await,
                    },
                    Some(Request::Refresh) = requests.recv() => {
                        self.queue(Message::misc(misc::Union::RefreshVideo(true))).await
                    },
                };
                if let Some(reason) = end {
                    break reason;
                }
            },
        };
        for message in self.mapper.release_all() {
            let _ = self.send.try_send(message);
        }
        let _ = self.feed.send(Feed::Close(reason)).await;
    }

    async fn peer_info(&mut self, info: &PeerInfo) -> End {
        let shown =
            usize::try_from(info.current_display).ok().and_then(|index| info.displays.get(index));
        let Some(display) = shown.or(info.displays.first()) else {
            return Some(Some("The peer has no display to show".to_owned()));
        };
        self.mapper.set_origin(display.x, display.y);
        self.resize(display.width, display.height).await
    }

    async fn resize(&mut self, width: i32, height: i32) -> End {
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return None;
        };
        if width == 0 || height == 0 {
            return None;
        }
        self.send_video(Feed::Resize { width, height }).await
    }

    async fn send_video(&mut self, feed: Feed) -> End {
        match self.feed.send(feed).await {
            Ok(()) => None,
            Err(_) => Some(None),
        }
    }

    async fn queue(&mut self, message: Message) -> End {
        match self.send.send(message).await {
            Ok(()) => None,
            Err(_) => Some(Some("The connection to the peer closed".to_owned())),
        }
    }

    async fn handle(&mut self, message: Message) -> End {
        match message.union? {
            message::Union::VideoFrame(frame) => {
                let union = frame.union?;
                let (codec, frames) = Codec::split(union);
                self.send_video(Feed::Frames(codec, frames)).await
            },
            message::Union::TestDelay(mut delay) if !delay.from_client => {
                // The peer measures the round trip by having it sent back.
                delay.from_client = false;
                self.queue(Message::of(message::Union::TestDelay(delay))).await
            },
            message::Union::PeerInfo(info) => self.peer_info(&info).await,
            message::Union::Misc(misc) => match misc.union? {
                misc::Union::SwitchDisplay(display) => {
                    self.mapper.set_origin(display.x, display.y);
                    self.resize(display.width, display.height).await
                },
                misc::Union::CloseReason(reason) => Some(Some(reason)),
                _ => None,
            },
            message::Union::Clipboard(clipboard) => self.clipboard(&clipboard).await,
            message::Union::MultiClipboards(all) => {
                let text =
                    all.clipboards.iter().find(|c| c.format == ClipboardFormat::Text as i32)?;
                self.clipboard(text).await
            },
            message::Union::MessageBox(text) => {
                warn!("peer says: {} {} {}", text.msgtype, text.title, text.text);
                None
            },
            _ => None,
        }
    }

    async fn clipboard(&mut self, clipboard: &Clipboard) -> End {
        if clipboard.format != ClipboardFormat::Text as i32 {
            return None;
        }
        let Some(bytes) = clipboard_bytes(clipboard) else {
            debug!("ignored a clipboard the peer sent: it did not decompress");
            return None;
        };
        let text = String::from_utf8(bytes).ok()?;
        self.send_video(Feed::Clipboard(text)).await
    }

    async fn input(&mut self, event: InputEvent) -> End {
        let messages = match event {
            InputEvent::Clipboard(text) => {
                vec![Message::of(message::Union::Clipboard(Clipboard {
                    compress: false,
                    content: Bytes::from(text.into_bytes()),
                    format: ClipboardFormat::Text as i32,
                    ..Default::default()
                }))]
            },
            // The peer's resolution is never changed to fit the pane; the picture is scaled
            // instead. RustDesk's own client only sets a resolution the user picks from the
            // peer's list, and a request hangs a Wayland peer: it enumerates displays inside the
            // connection's task while PipeWire captures, so video and input stop.
            InputEvent::Resize { .. } => Vec::new(),
            event => self.mapper.translate(&event),
        };
        for message in messages {
            if let Some(end) = self.queue(message).await {
                return Some(end);
            }
        }
        None
    }
}

/// The content of a clipboard message, decompressed if it was compressed.
fn clipboard_bytes(clipboard: &Clipboard) -> Option<Vec<u8>> {
    if !clipboard.compress {
        return Some(clipboard.content.to_vec());
    }
    let mut source: &[u8] = &clipboard.content;
    let decoder = ruzstd::decoding::StreamingDecoder::new(&mut source).ok()?;
    let mut out = Vec::new();
    decoder.take(MAX_CLIPBOARD as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() <= MAX_CLIPBOARD).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_advertise_what_this_build_decodes() {
        let options = options(Quality::Fast);
        assert_eq!(options.image_quality, ImageQuality::Low as i32);
        let decoding = options.supported_decoding.unwrap();
        assert_eq!(decoding.ability_vp9, i32::from(video::supported().contains(&Codec::Vp9)));
        assert_eq!(decoding.ability_av1, i32::from(video::supported().contains(&Codec::Av1)));
        assert_eq!(options.disable_audio, BoolOption::Yes as i32);
    }

    #[test]
    fn plain_clipboards_pass_and_bad_compressed_ones_do_not() {
        let plain = Clipboard { content: Bytes::from_static(b"hi"), ..Default::default() };
        assert_eq!(clipboard_bytes(&plain).unwrap(), b"hi");
        let broken = Clipboard {
            compress: true,
            content: Bytes::from_static(b"not zstd"),
            ..Default::default()
        };
        assert_eq!(clipboard_bytes(&broken), None);
    }

    #[test]
    fn compressed_clipboards_decompress() {
        // "hello" as a zstd frame with the raw block type, made by `zstd -c --no-check`.
        let frame =
            [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o'];
        let compressed = Clipboard {
            compress: true,
            content: Bytes::copy_from_slice(&frame),
            ..Default::default()
        };
        assert_eq!(clipboard_bytes(&compressed).as_deref(), Some(&b"hello"[..]));
    }
}
