//! What the byte stream of a remote terminal says that `alacritty_terminal` keeps to itself.

use memchr::memchr;

use crate::transport::WorkingDirectory;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// `CSI` parameters of a synchronized update, before the final `h` or `l`.
const SYNC_PARAMS: &[u8] = b"?2026";
/// The start of an OSC that sets the working directory.
const OSC7_PREFIX: &[u8] = b"7;";
/// The longest OSC 7 payload kept, so that a sequence that never ends cannot grow without bound.
const MAX_PAYLOAD: usize = 8192;

/// What [`Scanner::feed`] came across.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Found {
    SyncBegin,
    SyncEnd,
    WorkingDirectory(WorkingDirectory),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// After `ESC`.
    Escape,
    /// In `ESC [`, having matched this much of [`SYNC_PARAMS`].
    Csi(usize),
    /// In `ESC ]`, having matched this much of [`OSC7_PREFIX`].
    OscPrefix(usize),
    /// In the payload of an OSC 7.
    Payload,
    /// After `ESC` in the payload of an OSC 7.
    PayloadEscape,
    /// In a string that is not looked at, an other OSC, DCS, SOS, PM or APC.
    Skip,
    /// After `ESC` in a string that is not looked at.
    SkipEscape,
}

/// Follows the stream closely enough to find synchronized updates and OSC 7, across any split.
pub(crate) struct Scanner {
    state: State,
    payload: Vec<u8>,
}

impl Scanner {
    pub(crate) fn new() -> Self {
        Self { state: State::Ground, payload: Vec::new() }
    }

    /// Forget a sequence in progress, as bytes are missing after this point.
    pub(crate) fn reset(&mut self) {
        self.state = State::Ground;
        self.payload.clear();
    }

    /// Look at the next bytes of the stream.
    pub(crate) fn feed(&mut self, mut bytes: &[u8], found: &mut impl FnMut(Found)) {
        loop {
            if self.state == State::Ground {
                match memchr(ESC, bytes) {
                    Some(index) => {
                        bytes = &bytes[index + 1..];
                        self.state = State::Escape;
                    },
                    None => return,
                }
            }
            let Some((&byte, rest)) = bytes.split_first() else { return };
            bytes = rest;
            self.advance(byte, found);
        }
    }

    fn advance(&mut self, byte: u8, found: &mut impl FnMut(Found)) {
        self.state = match self.state {
            State::Ground => State::Ground,
            State::PayloadEscape if byte == b'\\' => {
                self.finish_payload(found);
                State::Ground
            },
            State::SkipEscape if byte == b'\\' => State::Ground,
            State::Escape | State::PayloadEscape | State::SkipEscape => {
                self.payload.clear();
                match byte {
                    b'[' => State::Csi(0),
                    b']' => State::OscPrefix(0),
                    b'P' | b'X' | b'^' | b'_' => State::Skip,
                    ESC => State::Escape,
                    _ => State::Ground,
                }
            },
            State::Csi(matched) if matched < SYNC_PARAMS.len() => match byte {
                ESC => State::Escape,
                _ if byte == SYNC_PARAMS[matched] => State::Csi(matched + 1),
                _ => State::Ground,
            },
            State::Csi(_) => {
                match byte {
                    b'h' => found(Found::SyncBegin),
                    b'l' => found(Found::SyncEnd),
                    _ => (),
                }
                if byte == ESC { State::Escape } else { State::Ground }
            },
            State::OscPrefix(matched) if byte == OSC7_PREFIX[matched] => {
                if matched + 1 == OSC7_PREFIX.len() {
                    State::Payload
                } else {
                    State::OscPrefix(matched + 1)
                }
            },
            State::OscPrefix(_) => Self::skip(byte),
            State::Payload => match byte {
                BEL => {
                    self.finish_payload(found);
                    State::Ground
                },
                ESC => State::PayloadEscape,
                CAN | SUB => {
                    self.payload.clear();
                    State::Ground
                },
                _ if self.payload.len() == MAX_PAYLOAD => {
                    self.payload.clear();
                    State::Skip
                },
                _ => {
                    self.payload.push(byte);
                    State::Payload
                },
            },
            State::Skip => Self::skip(byte),
        };
    }

    /// Where `byte` leaves a string that is not looked at.
    fn skip(byte: u8) -> State {
        match byte {
            BEL | CAN | SUB => State::Ground,
            ESC => State::SkipEscape,
            _ => State::Skip,
        }
    }

    fn finish_payload(&mut self, found: &mut impl FnMut(Found)) {
        if let Some(directory) = parse_file_url(&self.payload) {
            found(Found::WorkingDirectory(directory));
        }
        self.payload.clear();
    }
}

/// `file://host/path`, percent-encoded. Paths that are not UTF-8 are not reported.
fn parse_file_url(url: &[u8]) -> Option<WorkingDirectory> {
    let rest = url.strip_prefix(b"file://")?;
    let slash = memchr(b'/', rest)?;
    let host = String::from_utf8(rest[..slash].to_vec()).ok()?;
    let path = String::from_utf8(percent_decode(&rest[slash..])).ok()?;
    Some(WorkingDirectory { host: (!host.is_empty()).then_some(host), path })
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = (bytes[index] == b'%')
            .then(|| Some((hex(*bytes.get(index + 1)?)?, hex(*bytes.get(index + 2)?)?)))
            .flatten();
        match escape {
            Some((high, low)) => {
                decoded.push(high << 4 | low);
                index += 3;
            },
            None => {
                decoded.push(bytes[index]);
                index += 1;
            },
        }
    }
    decoded
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Found> {
        let mut scanner = Scanner::new();
        let mut all = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, &mut |found| all.push(found));
        }
        all
    }

    fn directory(host: Option<&str>, path: &str) -> Found {
        Found::WorkingDirectory(WorkingDirectory {
            host: host.map(str::to_owned),
            path: path.to_owned(),
        })
    }

    #[test]
    fn finds_osc7_with_either_terminator() {
        let expected = [directory(Some("box"), "/home/me")];
        assert_eq!(scan(&[b"\x1b]7;file://box/home/me\x07"]), expected);
        assert_eq!(scan(&[b"text\x1b]7;file://box/home/me\x1b\\more"]), expected);
    }

    #[test]
    fn decodes_percent_escapes_and_empty_host() {
        assert_eq!(
            scan(&["\x1b]7;file:///tmp/a%20b/%E4%B8%AD%zz\x07".as_bytes()]),
            [directory(None, "/tmp/a b/中%zz")]
        );
    }

    #[test]
    fn survives_every_split() {
        let stream = b"ab\x1b[?2026h\x1b]7;file://h/x%2Fy\x1b\\\x1b[?2026l\x1b]7;file://h/z\x07";
        let expected = scan(&[stream]);
        assert_eq!(expected.len(), 4);
        for split in 0..stream.len() {
            let (head, tail) = stream.split_at(split);
            assert_eq!(scan(&[head, tail]), expected, "split at {split}");
        }
        let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(scan(&bytewise), expected);
    }

    #[test]
    fn ignores_lookalikes() {
        // Another OSC, a DCS, a bad scheme, an abandoned sequence and CSIs that are not 2026. An
        // ESC ends an OSC or DCS in `vte`, so a real OSC 7 cannot hide inside one.
        let stream: &[u8] = b"\x1b]0;7;file://h/title\x07\x1bP7;file://h/dcs\x1b\\\
            \x1b]7;http://h/x\x07\x1b]7;file://h/gone\x18\x1b[?2027h\x1b[?20261h\x1b[2026h";
        assert_eq!(scan(&[stream]), []);
    }

    #[test]
    fn a_runaway_payload_is_dropped() {
        let mut stream = b"\x1b]7;file://h/".to_vec();
        stream.extend(std::iter::repeat_n(b'a', MAX_PAYLOAD * 2));
        stream.extend(b"\x07\x1b[?2026h");
        assert_eq!(scan(&[&stream]), [Found::SyncBegin]);
    }
}
