//! The decoders this build has.
//!
//! VP9 is the codec every RustDesk peer can always send: its encoder falls back to VP9 whenever
//! another codec is not usable by every client, and always honours a client preferring it. The
//! decoder is `vp9-mt`, pure Rust, so the build needs no C toolchain or assembler.

use super::{Codec, Decoder, Picture, Planes};

pub(super) const SUPPORTED: &[Codec] = &[Codec::Vp9];

/// Worker threads for tile columns and the loop filter. More gains little at desktop sizes.
const VP9_THREADS: usize = 4;

pub(super) fn new(codec: Codec) -> Option<Box<dyn Decoder>> {
    match codec {
        Codec::Vp9 => Some(Box::new(Vp9::new())),
        _ => None,
    }
}

struct Vp9 {
    decoder: vp9_mt::Vp9Decoder,
    /// The picture last shown, kept so the caller can borrow its planes.
    shown: Option<vp9_mt::Picture>,
}

impl Vp9 {
    fn new() -> Self {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(VP9_THREADS);
        Self { decoder: vp9_mt::Vp9Decoder::with_threads(threads), shown: None }
    }
}

impl Decoder for Vp9 {
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture<'_>>, String> {
        // The decoder recycles a picture's buffer only once it is dropped.
        self.shown = None;
        self.decoder.push(data, None).map_err(|err| err.to_string())?;
        // A superframe holds hidden frames before the shown one; each takes a call.
        while self.decoder.pending() > 0 {
            match self.decoder.next_picture() {
                Ok(picture) => self.shown = Some(picture),
                Err(vp9_mt::Error::Again) => {},
                Err(err) => return Err(err.to_string()),
            }
        }
        let Some(picture) = self.shown.as_ref() else { return Ok(None) };
        if picture.bit_depth() != 8 || picture.subsampling() != (1, 1) {
            return Err(format!(
                "{}-bit {:?} subsampling is not supported",
                picture.bit_depth(),
                picture.subsampling()
            ));
        }
        let (y, y_stride) = picture.plane(0);
        let (u, uv_stride) = picture.plane(1);
        let (v, _) = picture.plane(2);
        Ok(Some(Picture::Words(Planes {
            width: picture.width(),
            height: picture.height(),
            y,
            u,
            v,
            y_stride,
            uv_stride,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::EncodedVideoFrame;
    use crate::video::Video;
    use rmx_display::DisplayEvent;

    /// The frames of an IVF file: a 32-byte header, then each frame after a 12-byte one.
    fn ivf_frames(file: &[u8]) -> Vec<EncodedVideoFrame> {
        assert_eq!(&file[..4], b"DKIF");
        let mut frames = Vec::new();
        let mut at = usize::from(u16::from_le_bytes([file[6], file[7]]));
        while at + 12 <= file.len() {
            let size = u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize;
            let data = bytes::Bytes::copy_from_slice(&file[at + 12..at + 12 + size]);
            frames.push(EncodedVideoFrame { data, key: frames.is_empty(), pts: 0 });
            at += 12 + size;
        }
        frames
    }

    /// Streams made by `ffmpeg -c:v libvpx-vp9 -deadline realtime -cpu-used 7 -lag-in-frames 0`,
    /// the realtime settings RustDesk's encoder uses: two frames of solid red at 64x32, and six
    /// of `testsrc2` at 96x64.
    const RED: &[u8] = include_bytes!("testdata/red.ivf");
    const MOTION: &[u8] = include_bytes!("testdata/motion.ivf");

    fn events(video: &mut Video) -> Vec<DisplayEvent> {
        std::iter::from_fn(|| video.outbox.next_event()).collect()
    }

    #[test]
    fn a_real_vp9_stream_decodes_to_its_colors() {
        let frames = ivf_frames(RED);
        assert_eq!(frames.len(), 2);
        let mut video = Video::default();
        assert!(video.frames(Codec::Vp9, &frames[..1]).is_ok());
        let first = events(&mut video);
        assert_eq!(first[0], DisplayEvent::Resized { width: 64, height: 32 });
        let DisplayEvent::Update(update) = &first[1] else { panic!("no update: {first:?}") };
        let [b, g, r, a] = update.pixels[..4] else { unreachable!() };
        assert!(r > 230 && g < 25 && b < 25 && a == 255, "red decoded as {r} {g} {b}");

        // The second frame shows the same picture, so nothing is owed.
        assert!(video.frames(Codec::Vp9, &frames[1..]).is_ok());
        assert_eq!(events(&mut video), []);
    }

    #[test]
    fn inter_frames_decode_and_only_changes_are_sent() {
        let frames = ivf_frames(MOTION);
        assert!(frames.len() >= 5);
        let mut video = Video::default();
        for (index, frame) in frames.iter().enumerate() {
            assert!(video.frames(Codec::Vp9, std::slice::from_ref(frame)).is_ok(), "frame {index}");
            let mut updated = false;
            for event in events(&mut video) {
                if let DisplayEvent::Update(update) = event {
                    assert!(update.x + update.width <= 96 && update.y + update.height <= 64);
                    updated = true;
                }
            }
            assert!(updated, "testsrc2 changes every frame, frame {index} sent nothing");
        }
        assert_eq!(video.size, (96, 64));
    }

    #[test]
    fn an_inter_frame_without_its_key_frame_asks_for_one() {
        let frames = ivf_frames(MOTION);
        let mut video = Video::default();
        assert!(video.frames(Codec::Vp9, &frames[2..3]).is_err());
        // After the error a key frame starts things again.
        assert!(video.frames(Codec::Vp9, &frames[..1]).is_ok());
    }

    #[test]
    fn vp9_is_advertised_and_others_are_not_made() {
        assert_eq!(SUPPORTED, [Codec::Vp9]);
        assert!(new(Codec::Vp9).is_some());
        assert!(new(Codec::Av1).is_none());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let mut decoder = Vp9::new();
        assert!(decoder.decode(&[0xFF; 64]).is_err());
        assert!(decoder.decode(&[]).is_err() || decoder.decode(&[]).unwrap().is_none());
    }
}
