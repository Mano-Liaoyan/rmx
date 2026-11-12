//! Linux evdev key codes to the PC set-1 scancodes Windows and RDP use.

/// `KEY_PAUSE`, which has no scancode of its own and is sent as its own sequence.
pub const EVDEV_PAUSE: u32 = 119;

/// Returns the set-1 scancode of the physical key, or `None` if it has no scancode.
///
/// Extended keys carry `0xE0` in the high byte, as `0xE048` for the up arrow. Evdev numbers for
/// the main block are set-1 scancodes by construction; everything else comes from the extended
/// range or the Japanese and Korean keys.
pub fn evdev_to_scancode(evdev: u32) -> Option<u16> {
    const E: u16 = 0xE000;
    let code = match evdev {
        1..=83 => evdev as u16,
        85 => 0x76,      // KEY_ZENKAKUHANKAKU
        86 => 0x56,      // KEY_102ND
        87 => 0x57,      // KEY_F11
        88 => 0x58,      // KEY_F12
        89 => 0x73,      // KEY_RO
        90 => 0x78,      // KEY_KATAKANA
        91 => 0x77,      // KEY_HIRAGANA
        92 => 0x79,      // KEY_HENKAN
        93 => 0x70,      // KEY_KATAKANAHIRAGANA
        94 => 0x7B,      // KEY_MUHENKAN
        95 => 0x5C,      // KEY_KPJPCOMMA
        96 => E | 0x1C,  // KEY_KPENTER
        97 => E | 0x1D,  // KEY_RIGHTCTRL
        98 => E | 0x35,  // KEY_KPSLASH
        99 => E | 0x37,  // KEY_SYSRQ
        100 => E | 0x38, // KEY_RIGHTALT
        102 => E | 0x47, // KEY_HOME
        103 => E | 0x48, // KEY_UP
        104 => E | 0x49, // KEY_PAGEUP
        105 => E | 0x4B, // KEY_LEFT
        106 => E | 0x4D, // KEY_RIGHT
        107 => E | 0x4F, // KEY_END
        108 => E | 0x50, // KEY_DOWN
        109 => E | 0x51, // KEY_PAGEDOWN
        110 => E | 0x52, // KEY_INSERT
        111 => E | 0x53, // KEY_DELETE
        113 => E | 0x20, // KEY_MUTE
        114 => E | 0x2E, // KEY_VOLUMEDOWN
        115 => E | 0x30, // KEY_VOLUMEUP
        116 => E | 0x5E, // KEY_POWER
        117 => 0x59,     // KEY_KPEQUAL
        121 => 0x7E,     // KEY_KPCOMMA
        122 => 0x72,     // KEY_HANGEUL
        123 => 0x71,     // KEY_HANJA
        124 => 0x7D,     // KEY_YEN
        125 => E | 0x5B, // KEY_LEFTMETA
        126 => E | 0x5C, // KEY_RIGHTMETA
        127 => E | 0x5D, // KEY_COMPOSE
        140 => E | 0x21, // KEY_CALC
        142 => E | 0x5F, // KEY_SLEEP
        143 => E | 0x63, // KEY_WAKEUP
        155 => E | 0x6C, // KEY_MAIL
        156 => E | 0x66, // KEY_BOOKMARKS
        157 => E | 0x6B, // KEY_COMPUTER
        158 => E | 0x6A, // KEY_BACK
        159 => E | 0x69, // KEY_FORWARD
        163 => E | 0x19, // KEY_NEXTSONG
        164 => E | 0x22, // KEY_PLAYPAUSE
        165 => E | 0x10, // KEY_PREVIOUSSONG
        166 => E | 0x24, // KEY_STOPCD
        172 => E | 0x32, // KEY_HOMEPAGE
        173 => E | 0x67, // KEY_REFRESH
        217 => E | 0x65, // KEY_SEARCH
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(evdev: u32) -> (bool, u8) {
        let code = evdev_to_scancode(evdev).expect("mapped");
        (code >> 8 == 0xE0, code as u8)
    }

    #[test]
    fn main_block_is_identity() {
        assert_eq!(parts(1), (false, 0x01)); // ESC
        assert_eq!(parts(16), (false, 0x10)); // Q
        assert_eq!(parts(30), (false, 0x1E)); // A
        assert_eq!(parts(57), (false, 0x39)); // SPACE
        assert_eq!(parts(59), (false, 0x3B)); // F1
        assert_eq!(parts(83), (false, 0x53)); // KPDOT
    }

    #[test]
    fn extended_keys_carry_the_e0_prefix() {
        assert_eq!(parts(103), (true, 0x48)); // UP
        assert_eq!(parts(108), (true, 0x50)); // DOWN
        assert_eq!(parts(105), (true, 0x4B)); // LEFT
        assert_eq!(parts(106), (true, 0x4D)); // RIGHT
        assert_eq!(parts(97), (true, 0x1D)); // RIGHTCTRL
        assert_eq!(parts(100), (true, 0x38)); // RIGHTALT
        assert_eq!(parts(96), (true, 0x1C)); // KPENTER
        assert_eq!(parts(98), (true, 0x35)); // KPSLASH
        assert_eq!(parts(125), (true, 0x5B)); // LEFTMETA
        assert_eq!(parts(111), (true, 0x53)); // DELETE
    }

    #[test]
    fn function_keys_past_ten_are_not_contiguous_with_f1() {
        assert_eq!(parts(68), (false, 0x44)); // F10
        assert_eq!(parts(87), (false, 0x57)); // F11
        assert_eq!(parts(88), (false, 0x58)); // F12
    }

    #[test]
    fn right_and_left_keys_do_not_collide() {
        for (right, left) in [(97, 29), (100, 56)] {
            assert_ne!(parts(right), parts(left));
        }
    }

    #[test]
    fn unmapped_codes_are_none() {
        assert!(evdev_to_scancode(0).is_none());
        assert!(evdev_to_scancode(84).is_none());
        assert!(evdev_to_scancode(EVDEV_PAUSE).is_none());
        assert!(evdev_to_scancode(0x110).is_none()); // BTN_LEFT
    }

    #[test]
    fn every_mapped_scancode_is_unique() {
        let mut seen = std::collections::HashMap::new();
        for evdev in 0..512 {
            if let Some(code) = evdev_to_scancode(evdev)
                && let Some(earlier) = seen.insert(code, evdev)
            {
                panic!("{earlier} and {evdev} share {code:#x}");
            }
        }
    }
}
