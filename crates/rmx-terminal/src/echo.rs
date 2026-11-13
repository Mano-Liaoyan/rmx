//! What a terminal shows of its own input when the program does not echo it.
//!
//! Only text is shown. Enter starts a new line and a backspace erases, while control keys,
//! escape sequences and anything that is not UTF-8 are left out: a key or a paste must not be
//! able to drive the terminal as output from the program could.

use bytes::Bytes;

/// What to show for `typed`, which is returned as it is when it is plain text.
pub(crate) fn local_echo(typed: Bytes) -> Bytes {
    if typed.iter().all(|byte| (0x20..0x7f).contains(byte)) {
        return typed;
    }

    let mut echo = Vec::with_capacity(typed.len() + 8);
    let mut rest = &typed[..];
    let mut after_cr = false;
    while let [byte, tail @ ..] = rest {
        let was_cr = std::mem::take(&mut after_cr);
        match *byte {
            0x20..=0x7e | b'\t' => echo.push(*byte),
            b'\r' => {
                echo.extend_from_slice(b"\r\n");
                after_cr = true;
            },
            b'\n' if was_cr => {},
            b'\n' => echo.extend_from_slice(b"\r\n"),
            0x08 | 0x7f => echo.extend_from_slice(b"\x08 \x08"),
            0x1b => {
                rest = skip_escape(tail);
                continue;
            },
            0x80.. => {
                let encoded = &rest[..utf8_len(*byte).min(rest.len())];
                let text = std::str::from_utf8(encoded).ok();
                if let Some(text) = text.filter(|text| !text.chars().any(char::is_control)) {
                    echo.extend_from_slice(text.as_bytes());
                    rest = &rest[encoded.len()..];
                    continue;
                }
            },
            _ => {},
        }
        rest = tail;
    }
    Bytes::from(echo)
}

/// What follows the escape sequence that `rest` comes after an ESC of, as keys and mouse reports
/// encode them.
fn skip_escape(rest: &[u8]) -> &[u8] {
    match rest {
        [b'[', tail @ ..] => {
            let params = tail.iter().take_while(|byte| (0x20..=0x3f).contains(*byte)).count();
            match &tail[params..] {
                // A mouse report of the oldest kind, with three bytes that would pass for text.
                [b'M', report @ ..] if params == 0 => &report[report.len().min(3)..],
                [_final, after @ ..] => after,
                [] => &[],
            }
        },
        [b'O', _key, after @ ..] => after,
        // Alt with a key.
        [_, after @ ..] => after,
        // Escape alone.
        [] => rest,
    }
}

/// How long the UTF-8 sequence starting with `lead` is, 1 for a byte that cannot start one.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo(typed: &[u8]) -> Vec<u8> {
        local_echo(Bytes::copy_from_slice(typed)).to_vec()
    }

    #[test]
    fn text_is_shown_as_typed() {
        assert_eq!(echo(b"ls -la"), b"ls -la");
        assert_eq!(echo("中文 é\t".as_bytes()), "中文 é\t".as_bytes());
    }

    #[test]
    fn enter_starts_a_new_line_once() {
        assert_eq!(echo(b"\r"), b"\r\n");
        assert_eq!(echo(b"\n"), b"\r\n");
        assert_eq!(echo(b"a\r\nb"), b"a\r\nb");
        assert_eq!(echo(b"\r\r"), b"\r\n\r\n");
    }

    #[test]
    fn a_backspace_erases_what_it_backs_over() {
        assert_eq!(echo(b"\x7f"), b"\x08 \x08");
        assert_eq!(echo(b"\x08"), b"\x08 \x08");
    }

    #[test]
    fn control_keys_and_escape_sequences_are_not_shown() {
        // Ctrl-C, arrows, an F key, Alt-x, a lone Escape and bracketed paste markers.
        assert_eq!(echo(b"\x03"), b"");
        assert_eq!(echo(b"\x1b[A\x1bOB\x1b[15~\x1bx\x1b"), b"");
        assert_eq!(echo(b"\x1b[200~pasted\x1b[201~"), b"pasted");
        // Mouse reports: the oldest kind, with its three raw bytes, and an SGR one.
        assert_eq!(echo(b"\x1b[M !!a\x1b[<0;3;4Mb"), b"ab");
    }

    #[test]
    fn nothing_typed_or_pasted_can_drive_the_terminal() {
        assert_eq!(echo(b"\x1b]0;title\x07after"), b"0;titleafter");
        // A C1 CSI, as a raw byte and encoded, and a broken sequence.
        assert_eq!(echo(&[0x9b, b'2', b'J']), b"2J");
        assert_eq!(echo("\u{9b}2J".as_bytes()), b"2J");
        assert_eq!(echo(&[b'a', 0xe4, 0xb8, b'b']), b"ab");
    }

    #[test]
    fn plain_text_is_not_copied() {
        let typed = Bytes::from_static(b"plain text");
        assert_eq!(local_echo(typed.clone()).as_ptr(), typed.as_ptr());
    }
}
