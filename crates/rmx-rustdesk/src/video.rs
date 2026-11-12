//! Decoding the peer's video into the picture the interface is sent.
//!
//! Decoding runs on a thread of its own, so a slow frame delays neither input nor the
//! connection. Each decoded frame is compared with the last in tiles, and only tiles that
//! changed are marked as owed. The [`Outbox`] then delivers the newest pixels of those tiles as
//! the interface has room, so a backed-up interface skips frames instead of queueing them.

use log::{debug, warn};
use rmx_display::outbox::{Outbox, Rect};
use rmx_display::{DisplayEvent, FrameUpdate};
use tokio::sync::mpsc;

use crate::proto::{EncodedVideoFrame, video_frame};

mod codecs;

/// The side of the square tiles frames are compared in.
const TILE: u32 = 64;
const BYTES_PER_PIXEL: usize = 4;

/// The video formats a peer can send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    Vp8,
    Vp9,
    Av1,
    H264,
    H265,
}

impl Codec {
    /// The codec and frames of a video message.
    pub(crate) fn split(union: video_frame::Union) -> (Self, Vec<EncodedVideoFrame>) {
        match union {
            video_frame::Union::Vp8s(f) => (Self::Vp8, f.frames),
            video_frame::Union::Vp9s(f) => (Self::Vp9, f.frames),
            video_frame::Union::Av1s(f) => (Self::Av1, f.frames),
            video_frame::Union::H264s(f) => (Self::H264, f.frames),
            video_frame::Union::H265s(f) => (Self::H265, f.frames),
        }
    }
}

/// The codecs this build can decode, best first.
pub(crate) fn supported() -> &'static [Codec] {
    codecs::SUPPORTED
}

/// One decoded frame, in 8-bit 4:2:0 planes of samples of type `S`.
pub(crate) struct Planes<'a, S> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) y: &'a [S],
    pub(crate) u: &'a [S],
    pub(crate) v: &'a [S],
    /// Samples per row.
    pub(crate) y_stride: usize,
    pub(crate) uv_stride: usize,
}

/// A decoded frame, in whichever sample type its decoder keeps.
pub(crate) enum Picture<'a> {
    /// What a decoder keeping bytes would give; none does yet.
    #[allow(dead_code)]
    Bytes(Planes<'a, u8>),
    Words(Planes<'a, u16>),
}

impl Picture<'_> {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Bytes(p) => (p.width, p.height),
            Self::Words(p) => (p.width, p.height),
        }
    }

    fn convert(&self, out: &mut Vec<u8>) {
        match self {
            Self::Bytes(p) => convert(p, out),
            Self::Words(p) => convert(p, out),
        }
    }
}

/// A decoder of one codec.
pub(crate) trait Decoder: Send {
    /// Decodes one frame. `None` means the decoder has no picture to show yet.
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture<'_>>, String>;
}

/// What the session hands the video thread.
pub(crate) enum Feed {
    Frames(Codec, Vec<EncodedVideoFrame>),
    /// The peer switched displays or changed resolution.
    Resize {
        width: u32,
        height: u32,
    },
    Clipboard(String),
    Close(Option<String>),
}

/// What the video thread asks of the session.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// Send a key frame: the decoder lost track.
    Refresh,
}

/// Decodes until the session closes, delivering through `events`.
pub(crate) async fn run(
    mut feed: mpsc::Receiver<Feed>,
    events: mpsc::Sender<DisplayEvent>,
    requests: mpsc::UnboundedSender<Request>,
) {
    let mut video = Video::default();
    let mut closing = false;
    loop {
        tokio::select! {
            biased;
            permit = events.reserve(), if video.outbox.has_pending() => {
                let Ok(permit) = permit else { break };
                if let Some(event) = video.outbox.next_event() {
                    let closed = matches!(event, DisplayEvent::Closed(_));
                    permit.send(event);
                    if closed {
                        break;
                    }
                }
            },
            next = feed.recv(), if !closing => match next {
                Some(Feed::Frames(codec, frames)) => {
                    if video.frames(codec, &frames).is_err() {
                        let _ = requests.send(Request::Refresh);
                    }
                },
                Some(Feed::Resize { width, height }) => video.resize(width, height),
                Some(Feed::Clipboard(text)) => video.outbox.set_clipboard(text),
                Some(Feed::Close(reason)) => {
                    closing = true;
                    video.outbox.close(reason);
                },
                None => {
                    closing = true;
                    video.outbox.close(Some("The session ended".to_owned()));
                },
            },
            else => break,
        }
    }
}

#[derive(Default)]
struct Video {
    outbox: Outbox,
    decoder: Option<(Codec, Box<dyn Decoder>)>,
    /// The last decoded frame, as BGRA, to find what the next one changed.
    last: Vec<u8>,
    /// The frame being converted into.
    next: Vec<u8>,
    size: (u32, u32),
    /// Whether `last` holds a frame of `size` at all.
    has_last: bool,
}

/// A frame did not decode; a key frame is needed.
struct Lost;

impl Video {
    fn resize(&mut self, width: u32, height: u32) {
        if self.outbox.resize(width, height) {
            self.size = (width, height);
            self.has_last = false;
        }
    }

    fn frames(&mut self, codec: Codec, frames: &[EncodedVideoFrame]) -> Result<(), Lost> {
        if self.decoder.as_ref().is_none_or(|(current, _)| *current != codec) {
            match codecs::new(codec) {
                Some(decoder) => self.decoder = Some((codec, decoder)),
                None => {
                    warn!("the peer sent {codec:?}, which this build cannot decode");
                    self.decoder = None;
                    return Ok(());
                },
            }
        }
        let Some((_, decoder)) = self.decoder.as_mut() else { return Ok(()) };
        let mut shown = false;
        for frame in frames {
            match decoder.decode(&frame.data) {
                Ok(Some(picture)) => {
                    picture.convert(&mut self.next);
                    shown = true;
                    let (width, height) = picture.size();
                    if (width, height) != self.size {
                        // The stream is the truth about the size, whatever the peer said.
                        self.size = (width, height);
                        self.outbox.resize(width, height);
                        self.has_last = false;
                    }
                },
                Ok(None) => {},
                Err(err) => {
                    debug!("{codec:?} frame failed to decode: {err}");
                    // Whatever followed this frame depends on it.
                    self.decoder = None;
                    return Err(Lost);
                },
            }
        }
        if shown {
            self.show();
        }
        Ok(())
    }

    /// Paints `next` and marks the tiles where it differs from `last`.
    fn show(&mut self) {
        let (width, height) = self.size;
        let update =
            FrameUpdate { x: 0, y: 0, width, height, pixels: std::mem::take(&mut self.next) };
        if self.outbox.paint(&update).is_err() {
            self.next = update.pixels;
            return;
        }
        let next = update.pixels;
        for rect in
            changed_tiles(self.has_last.then_some(self.last.as_slice()), &next, width, height)
        {
            self.outbox.add_damage(rect);
        }
        self.next = std::mem::replace(&mut self.last, next);
        self.has_last = true;
    }
}

/// The tiles of `next` that differ from `last`, every tile when there is no `last`.
fn changed_tiles(last: Option<&[u8]>, next: &[u8], width: u32, height: u32) -> Vec<Rect> {
    let mut changed = Vec::new();
    let stride = width as usize * BYTES_PER_PIXEL;
    for top in (0..height).step_by(TILE as usize) {
        let rows = TILE.min(height - top);
        for left in (0..width).step_by(TILE as usize) {
            let columns = TILE.min(width - left);
            let differs = last.is_none_or(|last| {
                (top..top + rows).any(|row| {
                    let start = row as usize * stride + left as usize * BYTES_PER_PIXEL;
                    let end = start + columns as usize * BYTES_PER_PIXEL;
                    last[start..end] != next[start..end]
                })
            });
            if differs {
                changed.push(Rect { x: left, y: top, width: columns, height: rows });
            }
        }
    }
    changed
}

/// Converts BT.601 limited-range 4:2:0, what RustDesk's encoders produce, into opaque BGRA.
fn convert<S: Copy + Into<i32>>(picture: &Planes<'_, S>, out: &mut Vec<u8>) {
    let (width, height) = (picture.width as usize, picture.height as usize);
    out.clear();
    out.resize(width * height * BYTES_PER_PIXEL, 0);
    if width == 0 {
        return;
    }
    for (row, line) in out.chunks_exact_mut(width * BYTES_PER_PIXEL).enumerate() {
        let y_row = &picture.y[row * picture.y_stride..][..width];
        let u_row = &picture.u[(row / 2) * picture.uv_stride..][..width.div_ceil(2)];
        let v_row = &picture.v[(row / 2) * picture.uv_stride..][..width.div_ceil(2)];
        for (column, pixel) in line.as_chunks_mut::<BYTES_PER_PIXEL>().0.iter_mut().enumerate() {
            let c = (y_row[column].into() - 16) * 298;
            let d = u_row[column / 2].into() - 128;
            let e = v_row[column / 2].into() - 128;
            let clamp = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
            pixel[0] = clamp(c + 516 * d);
            pixel[1] = clamp(c - 100 * d - 208 * e);
            pixel[2] = clamp(c + 409 * e);
            pixel[3] = 0xFF;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture<'a>(
        y: &'a [u8],
        u: &'a [u8],
        v: &'a [u8],
        width: u32,
        height: u32,
    ) -> Planes<'a, u8> {
        Planes {
            width,
            height,
            y,
            u,
            v,
            y_stride: width as usize,
            uv_stride: width.div_ceil(2) as usize,
        }
    }

    #[test]
    fn studio_range_black_and_white_become_full_range() {
        let mut out = Vec::new();
        convert(&picture(&[16, 235], &[128], &[128], 2, 1), &mut out);
        assert_eq!(out, [0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn primary_colors_land_in_the_right_channels() {
        let mut out = Vec::new();
        // BT.601 studio-range red, green and blue.
        for (yuv, bgra) in [
            ((81, 90, 240), [0, 0, 255]),
            ((145, 54, 34), [0, 255, 0]),
            ((41, 240, 110), [255, 0, 0]),
        ] {
            let (y, u, v) = yuv;
            convert(&picture(&[y], &[u], &[v], 1, 1), &mut out);
            for (got, want) in out[..3].iter().zip(bgra) {
                assert!(got.abs_diff(want) <= 3, "{yuv:?} gave {:?}, not {bgra:?}", &out[..3]);
            }
        }
    }

    #[test]
    fn odd_sizes_share_the_last_chroma_sample() {
        let mut out = Vec::new();
        let y = [16; 9];
        convert(&picture(&y, &[128; 4], &[128; 4], 3, 3), &mut out);
        assert_eq!(out.len(), 3 * 3 * 4);
    }

    #[test]
    fn only_changed_tiles_are_owed() {
        let (width, height) = (130, 70);
        let last = vec![0u8; width as usize * height as usize * 4];
        let mut next = last.clone();
        assert_eq!(changed_tiles(Some(&last), &next, width, height), []);
        // A pixel in the last column of the second tile row.
        let offset = (65 * width as usize + 129) * 4;
        next[offset] = 1;
        assert_eq!(
            changed_tiles(Some(&last), &next, width, height),
            [Rect { x: 128, y: 64, width: 2, height: 6 }]
        );
        assert_eq!(changed_tiles(None, &next, width, height).len(), 3 * 2);
    }
}
