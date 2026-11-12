//! RustDesk's frame format: a little-endian length of one to four bytes, then the payload.
//!
//! The low two bits of the first byte say how many bytes the header has, less one; the rest of
//! the header, shifted right by two, is the payload length.

use std::io;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// The largest payload the four-byte header can describe.
pub(crate) const MAX_FRAME: usize = 0x3FFF_FFFF;

/// Bounds what an untrusted header can make the decoder reserve before the payload arrives.
const MAX_RESERVE: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct FrameCodec {
    /// The length of the payload being read, once its header has been.
    pending: Option<usize>,
    max_frame: usize,
}

impl FrameCodec {
    pub(crate) fn new(max_frame: usize) -> Self {
        Self { pending: None, max_frame: max_frame.min(MAX_FRAME) }
    }
}

impl Decoder for FrameCodec {
    type Error = io::Error;
    type Item = BytesMut;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        let length = match self.pending {
            Some(length) => length,
            None => {
                let Some(&first) = src.first() else { return Ok(None) };
                let header = usize::from(first & 0x3) + 1;
                if src.len() < header {
                    return Ok(None);
                }
                let length = src[..header]
                    .iter()
                    .rev()
                    .fold(0usize, |acc, &byte| acc << 8 | usize::from(byte))
                    >> 2;
                if length > self.max_frame {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
                }
                src.advance(header);
                src.reserve(length.saturating_sub(src.len()).min(MAX_RESERVE));
                self.pending = Some(length);
                length
            },
        };
        if src.len() < length {
            return Ok(None);
        }
        self.pending = None;
        Ok(Some(src.split_to(length)))
    }
}

impl Encoder<Bytes> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, data: Bytes, dst: &mut BytesMut) -> io::Result<()> {
        let length = data.len();
        dst.reserve(length + 4);
        if length <= 0x3F {
            dst.put_u8((length << 2) as u8);
        } else if length <= 0x3FFF {
            dst.put_u16_le((length << 2) as u16 | 0x1);
        } else if length <= 0x3F_FFFF {
            let header = (length << 2) as u32 | 0x2;
            dst.put_u16_le(header as u16);
            dst.put_u8((header >> 16) as u8);
        } else if length <= MAX_FRAME {
            dst.put_u32_le((length << 2) as u32 | 0x3);
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
        }
        dst.put(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(length: usize) -> BytesMut {
        let mut out = BytesMut::new();
        FrameCodec::new(MAX_FRAME).encode(Bytes::from(vec![7u8; length]), &mut out).unwrap();
        out
    }

    #[test]
    fn headers_grow_with_the_payload() {
        for (length, header) in [(0, 1), (0x3F, 1), (0x40, 2), (0x3FFF, 2), (0x4000, 3)] {
            assert_eq!(encoded(length).len(), length + header, "length {length}");
        }
        assert_eq!(&encoded(5)[..1], [5 << 2]);
        assert_eq!(&encoded(0x40)[..2], [0x01, 0x01]);
    }

    #[test]
    fn frames_round_trip_whatever_the_size() {
        for length in [0, 1, 63, 64, 300, 0x3FFF, 0x4000, 70_000] {
            let mut wire = encoded(length);
            let frame = FrameCodec::new(MAX_FRAME).decode(&mut wire).unwrap().unwrap();
            assert_eq!(frame.len(), length);
            assert!(wire.is_empty());
        }
    }

    #[test]
    fn a_frame_split_across_reads_waits_for_the_rest() {
        let whole = encoded(1000);
        let mut codec = FrameCodec::new(MAX_FRAME);
        let mut wire = BytesMut::from(&whole[..1]);
        assert!(codec.decode(&mut wire).unwrap().is_none());
        wire.extend_from_slice(&whole[1..500]);
        assert!(codec.decode(&mut wire).unwrap().is_none());
        wire.extend_from_slice(&whole[500..]);
        assert_eq!(codec.decode(&mut wire).unwrap().unwrap().len(), 1000);
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut wire = encoded(100);
        assert!(FrameCodec::new(99).decode(&mut wire).is_err());
    }
}
