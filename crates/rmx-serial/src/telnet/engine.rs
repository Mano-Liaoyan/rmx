//! The Telnet protocol, without any I/O: bytes in, data and replies out.

use bytes::{BufMut, Bytes, BytesMut};
use log::warn;
use memchr::{memchr, memchr2};

pub(crate) const IAC: u8 = 255;
pub(crate) const DONT: u8 = 254;
pub(crate) const DO: u8 = 253;
pub(crate) const WONT: u8 = 252;
pub(crate) const WILL: u8 = 251;
pub(crate) const SB: u8 = 250;
pub(crate) const SE: u8 = 240;

pub(crate) const BINARY: u8 = 0;
pub(crate) const ECHO: u8 = 1;
pub(crate) const SGA: u8 = 3;
pub(crate) const TTYPE: u8 = 24;
pub(crate) const NAWS: u8 = 31;

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

/// The longest subnegotiation kept. The only one understood, TTYPE SEND, is two bytes; what
/// exceeds this is discarded, however long it goes on.
pub(crate) const MAX_SUBNEGOTIATION: usize = 64;

/// An option's state in one direction. The client never withdraws an option it asked for, so
/// RFC 1143's `WANTNO` states are not needed to keep negotiation from looping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Off,
    Asked,
    On,
}

impl Side {
    /// Applies a received request. Returns the answer to send, `true` to agree and `false` to
    /// refuse or to acknowledge; none when the request only confirms what is already so.
    fn receive(&mut self, supported: bool, enable: bool) -> Option<bool> {
        match (*self, enable) {
            (Self::Off, true) if supported => {
                *self = Self::On;
                Some(true)
            },
            (Self::Off, true) => Some(false),
            (Self::Asked, true) => {
                *self = Self::On;
                None
            },
            (Self::On, true) | (Self::Off, false) => None,
            (Self::Asked, false) => {
                *self = Self::Off;
                None
            },
            (Self::On, false) => {
                *self = Self::Off;
                Some(false)
            },
        }
    }
}

struct Negotiated {
    option: u8,
    /// Whether the client performs this option, and whether the server may.
    ours: bool,
    theirs: bool,
    us: Side,
    him: Side,
}

impl Negotiated {
    const fn new(option: u8, ours: bool, theirs: bool) -> Self {
        Self { option, ours, theirs, us: Side::Off, him: Side::Off }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parse {
    Data,
    Iac,
    Verb(u8),
    Sub,
    SubIac,
}

pub(crate) struct Engine {
    parse: Parse,
    /// The last data byte was a CR, in a direction where a NUL after it is padding.
    after_cr: bool,
    sub: [u8; MAX_SUBNEGOTIATION],
    sub_len: usize,
    sub_overflowed: bool,
    options: [Negotiated; 5],
    replies: BytesMut,
    term: String,
    cols: u16,
    rows: u16,
}

impl Engine {
    /// A session with its opening offers queued.
    pub(crate) fn new(term: String, cols: u16, rows: u16) -> Self {
        let mut engine = Self {
            parse: Parse::Data,
            after_cr: false,
            sub: [0; MAX_SUBNEGOTIATION],
            sub_len: 0,
            sub_overflowed: false,
            options: [
                Negotiated::new(BINARY, true, true),
                Negotiated::new(ECHO, false, true),
                Negotiated::new(SGA, true, true),
                Negotiated::new(TTYPE, true, false),
                Negotiated::new(NAWS, true, false),
            ],
            replies: BytesMut::new(),
            term,
            cols,
            rows,
        };
        for index in 0..engine.options.len() {
            let option = engine.options[index].option;
            if engine.options[index].ours {
                engine.options[index].us = Side::Asked;
                engine.reply(WILL, option);
            }
            if engine.options[index].theirs {
                engine.options[index].him = Side::Asked;
                engine.reply(DO, option);
            }
        }
        engine
    }

    /// Takes in what arrived, appending its data to `data`, which grows by at most `input.len()`.
    pub(crate) fn feed(&mut self, mut input: &[u8], data: &mut BytesMut) {
        while let [byte, rest @ ..] = input {
            match self.parse {
                Parse::Data => {
                    let run = memchr(IAC, input).unwrap_or(input.len());
                    self.push_data(&input[..run], data);
                    input = &input[run..];
                    if let [_iac, rest @ ..] = input {
                        self.parse = Parse::Iac;
                        self.after_cr = false;
                        input = rest;
                    }
                },
                parse => {
                    input = rest;
                    self.step(parse, *byte, data);
                },
            }
        }
    }

    fn step(&mut self, parse: Parse, byte: u8, data: &mut BytesMut) {
        self.parse = match parse {
            Parse::Data | Parse::Iac => match byte {
                IAC => {
                    data.put_u8(IAC);
                    Parse::Data
                },
                WILL | WONT | DO | DONT => Parse::Verb(byte),
                SB => {
                    self.sub_len = 0;
                    self.sub_overflowed = false;
                    Parse::Sub
                },
                // NOP, GA, DM and the like change nothing for a stream of terminal output.
                _ => Parse::Data,
            },
            Parse::Verb(verb) => {
                self.negotiate(verb, byte);
                Parse::Data
            },
            Parse::Sub if byte == IAC => Parse::SubIac,
            Parse::Sub => {
                self.push_sub(byte);
                Parse::Sub
            },
            Parse::SubIac => match byte {
                IAC => {
                    self.push_sub(IAC);
                    Parse::Sub
                },
                SE => {
                    self.finish_sub();
                    Parse::Data
                },
                // The subnegotiation was cut off: drop it and honor this command.
                other => {
                    self.step(Parse::Iac, other, data);
                    return;
                },
            },
        };
    }

    /// Hands data to the terminal, removing the NUL that NVT puts after a bare CR.
    fn push_data(&mut self, mut run: &[u8], data: &mut BytesMut) {
        if self.is_on(BINARY, false) {
            self.after_cr = false;
            data.extend_from_slice(run);
            return;
        }
        if std::mem::take(&mut self.after_cr) {
            match run {
                [0, rest @ ..] => run = rest,
                [] => {
                    self.after_cr = true;
                    return;
                },
                _ => {},
            }
        }
        while let Some(at) = memchr(b'\r', run) {
            let (kept, rest) = run.split_at(at + 1);
            data.extend_from_slice(kept);
            match rest {
                [0, rest @ ..] => run = rest,
                [] => {
                    self.after_cr = true;
                    return;
                },
                rest => run = rest,
            }
        }
        data.extend_from_slice(run);
    }

    fn push_sub(&mut self, byte: u8) {
        match self.sub.get_mut(self.sub_len) {
            Some(slot) => {
                *slot = byte;
                self.sub_len += 1;
            },
            None if !self.sub_overflowed => {
                self.sub_overflowed = true;
                warn!("discarding an oversized telnet subnegotiation");
            },
            None => {},
        }
    }

    fn finish_sub(&mut self) {
        if !self.sub_overflowed && self.sub[..self.sub_len] == [TTYPE, TTYPE_SEND] {
            self.send_terminal_type();
        }
    }

    fn negotiate(&mut self, verb: u8, option: u8) {
        let enable = matches!(verb, WILL | DO);
        let found = self.options.iter().position(|known| known.option == option);
        if matches!(verb, WILL | WONT) {
            let answer = match found {
                Some(index) => {
                    let known = &mut self.options[index];
                    known.him.receive(known.theirs, enable)
                },
                None => Side::Off.receive(false, enable),
            };
            if let Some(agree) = answer {
                self.reply(if agree { DO } else { DONT }, option);
            }
            return;
        }

        let Some(index) = found else {
            if let Some(agree) = Side::Off.receive(false, enable) {
                self.reply(if agree { WILL } else { WONT }, option);
            }
            return;
        };
        let known = &mut self.options[index];
        let was = known.us;
        let answer = known.us.receive(known.ours, enable);
        let now = known.us;
        if let Some(agree) = answer {
            self.reply(if agree { WILL } else { WONT }, option);
        }
        if option == NAWS && was != Side::On && now == Side::On {
            self.send_size();
        }
    }

    /// The window has this size now, which the server hears of if it asked to.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        if self.is_on(NAWS, true) {
            self.send_size();
        }
    }

    /// What to send for `input`: doubled IACs and, unless in binary mode, NVT's CR rule.
    pub(crate) fn encode_input(&self, input: Bytes) -> Bytes {
        let binary = self.is_on(BINARY, true);
        let needs_work = if binary {
            memchr(IAC, &input).is_some()
        } else {
            memchr2(IAC, b'\r', &input).is_some()
        };
        if !needs_work {
            return input;
        }

        let mut encoded = BytesMut::with_capacity(input.len() + 16);
        for (index, &byte) in input.iter().enumerate() {
            encoded.put_u8(byte);
            match byte {
                IAC => encoded.put_u8(IAC),
                b'\r' if !binary && input.get(index + 1) != Some(&b'\n') => encoded.put_u8(0),
                _ => {},
            }
        }
        encoded.freeze()
    }

    /// Whether the server refused to echo, or stopped, which leaves echoing to the client. Not
    /// while the offer to have it echo is unanswered: most servers agree.
    pub(crate) fn echoes_locally(&self) -> bool {
        self.options.iter().any(|known| known.option == ECHO && known.him == Side::Off)
    }

    pub(crate) fn queued_replies(&self) -> usize {
        self.replies.len()
    }

    /// Takes everything waiting to be sent.
    pub(crate) fn take_replies(&mut self) -> Bytes {
        self.replies.split().freeze()
    }

    fn is_on(&self, option: u8, ours: bool) -> bool {
        self.options
            .iter()
            .find(|known| known.option == option)
            .is_some_and(|known| (if ours { known.us } else { known.him }) == Side::On)
    }

    fn reply(&mut self, verb: u8, option: u8) {
        self.replies.extend_from_slice(&[IAC, verb, option]);
    }

    fn send_size(&mut self) {
        let [cols_high, cols_low] = self.cols.to_be_bytes();
        let [rows_high, rows_low] = self.rows.to_be_bytes();
        self.send_sub(NAWS, &[cols_high, cols_low, rows_high, rows_low]);
    }

    fn send_terminal_type(&mut self) {
        if self.is_on(TTYPE, true) {
            let mut payload = Vec::with_capacity(self.term.len() + 1);
            payload.push(TTYPE_IS);
            payload.extend_from_slice(self.term.as_bytes());
            self.send_sub(TTYPE, &payload);
        }
    }

    fn send_sub(&mut self, option: u8, payload: &[u8]) {
        self.replies.extend_from_slice(&[IAC, SB, option]);
        for &byte in payload {
            self.replies.put_u8(byte);
            if byte == IAC {
                self.replies.put_u8(IAC);
            }
        }
        self.replies.extend_from_slice(&[IAC, SE]);
    }

    #[cfg(test)]
    fn sub_len(&self) -> usize {
        self.sub_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut engine = Engine::new("xterm".into(), 80, 24);
        engine.take_replies();
        engine
    }

    fn feed(engine: &mut Engine, chunks: &[&[u8]]) -> Vec<u8> {
        let mut data = BytesMut::new();
        for chunk in chunks {
            engine.feed(chunk, &mut data);
        }
        data.to_vec()
    }

    /// A small deterministic generator, so a failure names the seed that caused it.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, limit: usize) -> usize {
            (self.next() % limit as u64) as usize
        }
    }

    /// Cuts `bytes` at random places.
    fn chunks<'a>(bytes: &'a [u8], random: &mut Random) -> Vec<&'a [u8]> {
        let mut rest = bytes;
        let mut chunks = Vec::new();
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at((random.below(5) + 1).min(rest.len()));
            chunks.push(chunk);
            rest = tail;
        }
        chunks
    }

    /// Every kind of thing the parser handles, one after another.
    const SESSION: &[u8] = &[
        b'a', IAC, IAC, b'b', b'\r', 0, b'c', b'\r', b'\n', IAC, WILL, ECHO, IAC, DO, TTYPE, IAC,
        SB, TTYPE, TTYPE_SEND, IAC, SE, IAC, DO, NAWS, IAC, WILL, 99, IAC, DO, 99, IAC, SB, 42,
        IAC, IAC, 1, IAC, SE, IAC, 241, b'd',
    ];

    #[test]
    fn iac_split_across_reads_is_one_data_byte() {
        assert_eq!(feed(&mut engine(), &[&[b'a', IAC], &[IAC, b'b']]), [b'a', IAC, b'b']);
    }

    #[test]
    fn iac_iac_yields_a_single_byte() {
        assert_eq!(feed(&mut engine(), &[&[IAC, IAC, IAC, IAC]]), [IAC, IAC]);
    }

    #[test]
    fn a_command_split_across_reads_produces_no_data() {
        let mut engine = engine();
        assert!(feed(&mut engine, &[&[IAC], &[WILL], &[99]]).is_empty());
        assert_eq!(&engine.take_replies()[..], [IAC, DONT, 99]);
    }

    #[test]
    fn subnegotiation_split_across_reads_is_answered_once_whole() {
        let mut engine = engine();
        feed(&mut engine, &[&[IAC, DO, TTYPE]]);
        engine.take_replies();

        for byte in [IAC, SB, TTYPE, TTYPE_SEND, IAC] {
            feed(&mut engine, &[&[byte]]);
            assert!(engine.take_replies().is_empty(), "answered at {byte}");
        }
        feed(&mut engine, &[&[SE]]);

        let mut expected = vec![IAC, SB, TTYPE, TTYPE_IS];
        expected.extend_from_slice(b"xterm");
        expected.extend_from_slice(&[IAC, SE]);
        assert_eq!(&engine.take_replies()[..], expected);
    }

    #[test]
    fn iac_doubled_in_a_subnegotiation_is_payload() {
        let mut engine = engine();
        assert_eq!(
            feed(&mut engine, &[&[IAC, SB, 42, IAC, IAC, 7, IAC, SE, b'x']]),
            [b'x'],
            "the payload stayed inside"
        );
        assert_eq!(engine.sub_len(), 3);
    }

    #[test]
    fn an_oversized_subnegotiation_is_bounded_and_ends_cleanly() {
        let mut engine = engine();
        feed(&mut engine, &[&[IAC, DO, TTYPE]]);
        engine.take_replies();

        let mut noise = vec![IAC, SB, TTYPE, TTYPE_SEND];
        noise.extend(std::iter::repeat_n(b'x', 1 << 20));
        assert!(feed(&mut engine, &[&noise]).is_empty());
        assert!(engine.sub_len() <= MAX_SUBNEGOTIATION);

        assert_eq!(feed(&mut engine, &[&[IAC, SE, b'o', b'k']]), b"ok");
        assert!(engine.take_replies().is_empty(), "an oversized TTYPE SEND is not a request");
    }

    #[test]
    fn a_cut_off_subnegotiation_gives_way_to_the_next_command() {
        let mut engine = engine();
        assert_eq!(feed(&mut engine, &[&[IAC, SB, 42, 1, IAC, WILL, 99, b'x']]), [b'x']);
        assert_eq!(&engine.take_replies()[..], [IAC, DONT, 99]);
    }

    #[test]
    fn the_rest_is_refused_once_and_refusals_are_not_answered() {
        let mut engine = engine();
        feed(&mut engine, &[&[IAC, WILL, 99, IAC, DO, 98, IAC, WONT, 97, IAC, DONT, 96]]);
        assert_eq!(&engine.take_replies()[..], [IAC, DONT, 99, IAC, WONT, 98]);
    }

    #[test]
    fn options_only_go_in_their_direction() {
        let mut engine = engine();
        // Asked to echo, and offered a terminal type and window size of the server's.
        feed(&mut engine, &[&[IAC, DO, ECHO, IAC, WILL, TTYPE, IAC, WILL, NAWS]]);
        assert_eq!(
            &engine.take_replies()[..],
            [IAC, WONT, ECHO, IAC, DONT, TTYPE, IAC, DONT, NAWS]
        );
    }

    #[test]
    fn answers_to_our_offers_are_not_answered_back() {
        let mut engine = engine();
        feed(
            &mut engine,
            &[&[IAC, WILL, ECHO, IAC, DO, SGA, IAC, WONT, BINARY, IAC, DONT, BINARY]],
        );
        assert!(engine.take_replies().is_empty());
        assert!(engine.is_on(ECHO, false));
        assert!(!engine.is_on(BINARY, true));
    }

    #[test]
    fn echo_is_left_to_the_client_only_once_the_server_refuses_it() {
        let mut engine = engine();
        assert!(!engine.echoes_locally(), "the server may still agree");

        feed(&mut engine, &[&[IAC, WONT, ECHO]]);
        assert!(engine.echoes_locally());

        feed(&mut engine, &[&[IAC, WILL, ECHO]]);
        assert!(!engine.echoes_locally(), "the server took echoing back");
        assert_eq!(&engine.take_replies()[..], [IAC, DO, ECHO]);

        feed(&mut engine, &[&[IAC, WONT, ECHO]]);
        assert!(engine.echoes_locally(), "the server gave echoing up again");
    }

    #[test]
    fn the_session_opens_with_every_offer() {
        let mut engine = Engine::new("xterm".into(), 80, 24);
        let offers = engine.take_replies();
        for wanted in [
            [IAC, WILL, BINARY],
            [IAC, DO, BINARY],
            [IAC, DO, ECHO],
            [IAC, WILL, SGA],
            [IAC, DO, SGA],
            [IAC, WILL, TTYPE],
            [IAC, WILL, NAWS],
        ] {
            assert!(offers.windows(3).any(|seen| seen == wanted), "{wanted:?} not in {offers:?}");
        }
    }

    #[test]
    fn naws_waits_for_agreement_and_escapes_iac() {
        let mut engine = engine();
        engine.resize(255, 24);
        assert!(engine.take_replies().is_empty());

        feed(&mut engine, &[&[IAC, DO, NAWS]]);
        assert_eq!(
            &engine.take_replies()[..],
            [IAC, SB, NAWS, 0, IAC, IAC, 0, 24, IAC, SE],
            "no WILL after our own offer, and a doubled 255"
        );

        engine.resize(80, 0xFFFF);
        assert_eq!(&engine.take_replies()[..], [IAC, SB, NAWS, 0, 80, IAC, IAC, IAC, IAC, IAC, SE]);
    }

    #[test]
    fn a_terminal_type_is_only_sent_once_it_was_agreed() {
        let mut engine = engine();
        feed(&mut engine, &[&[IAC, SB, TTYPE, TTYPE_SEND, IAC, SE]]);
        assert!(engine.take_replies().is_empty());
    }

    #[test]
    fn nvt_padding_is_removed_even_across_reads() {
        assert_eq!(feed(&mut engine(), &[b"a\r\0b"]), b"a\rb");
        assert_eq!(feed(&mut engine(), &[b"a\r", b"\0b"]), b"a\rb");
        assert_eq!(feed(&mut engine(), &[b"a\r\n\0"]), b"a\r\n\0");
    }

    #[test]
    fn binary_mode_keeps_nuls_after_cr() {
        let mut engine = engine();
        feed(&mut engine, &[&[IAC, WILL, BINARY]]);
        assert_eq!(feed(&mut engine, &[b"a\r\0b"]), b"a\r\0b");
    }

    #[test]
    fn input_escapes_iac_and_pads_a_lone_cr() {
        let mut engine = engine();
        assert_eq!(&engine.encode_input(Bytes::from_static(b"\r"))[..], b"\r\0");
        assert_eq!(&engine.encode_input(Bytes::from_static(b"a\r\nb"))[..], b"a\r\nb");
        assert_eq!(&engine.encode_input(Bytes::from_static(&[1, IAC]))[..], [1, IAC, IAC]);

        feed(&mut engine, &[&[IAC, DO, BINARY]]);
        assert_eq!(&engine.encode_input(Bytes::from_static(b"\r"))[..], b"\r");
        assert_eq!(&engine.encode_input(Bytes::from_static(&[IAC]))[..], [IAC, IAC]);
    }

    #[test]
    fn plain_input_is_not_copied() {
        let input = Bytes::from_static(b"plain");
        let pointer = input.as_ptr();
        assert_eq!(engine().encode_input(input).as_ptr(), pointer);
    }

    #[test]
    fn however_a_session_is_cut_the_result_is_the_same() {
        let run = |chunks: &[&[u8]]| {
            let mut engine = engine();
            let data = feed(&mut engine, chunks);
            (data, engine.take_replies())
        };
        let whole = run(&[SESSION]);
        assert_eq!(whole.0, [b'a', IAC, b'b', b'\r', b'c', b'\r', b'\n', b'd']);

        for cut in 0..=SESSION.len() {
            let (head, tail) = SESSION.split_at(cut);
            assert_eq!(run(&[head, tail]), whole, "cut at {cut}");
        }
        for seed in 1..=2_000u64 {
            let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            assert_eq!(run(&chunks(SESSION, &mut random)), whole, "seed {seed}");
        }
    }

    #[test]
    fn random_bytes_stay_within_the_bounds() {
        for seed in 1..=500u64 {
            let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let len = random.below(4096);
            let input: Vec<u8> = (0..len)
                .map(|_| match random.below(4) {
                    // Keep it dense in protocol bytes, as uniform noise rarely is.
                    0 => IAC,
                    1 => [WILL, WONT, DO, DONT, SB, SE][random.below(6)],
                    2 => [BINARY, ECHO, SGA, TTYPE, NAWS, TTYPE_SEND][random.below(6)],
                    _ => random.next() as u8,
                })
                .collect();

            let mut engine = engine();
            let data = feed(&mut engine, &chunks(&input, &mut random));

            assert!(data.len() <= input.len(), "seed {seed}");
            assert!(engine.sub_len() <= MAX_SUBNEGOTIATION, "seed {seed}");
            // A reply is three bytes for a three-byte command, plus at most the terminal type
            // and window size for a subnegotiation or agreement of at least three bytes.
            assert!(engine.take_replies().len() <= input.len() * 8, "seed {seed}");
        }
    }
}
