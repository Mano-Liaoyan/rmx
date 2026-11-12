//! Interface input to RustDesk key and mouse events.
//!
//! Keys go in RustDesk's map mode: the position of the key in the code space of the peer's
//! platform, so the peer's own layout decides what it types.

use rmx_display::keymap::{EVDEV_PAUSE, evdev_to_scancode};
use rmx_display::{InputEvent, Key, buttons};

use crate::proto::{ControlKey, KeyEvent, KeyboardMode, Message, MouseEvent, key_event, message};

/// Mouse event types, in the low three bits of the mask.
const MOVE: i32 = 0;
const DOWN: i32 = 1;
const UP: i32 = 2;
const WHEEL: i32 = 3;

/// RustDesk's button bits, above the type.
const LEFT: i32 = 0x01;
const RIGHT: i32 = 0x02;
const MIDDLE: i32 = 0x04;

/// Keeps one wheel event from flooding the connection.
const MAX_NOTCHES: i32 = 16;

/// The code space of a peer's key positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    Linux,
    Mac,
}

impl Platform {
    /// From what the peer reports in its `PeerInfo`.
    pub(crate) fn of(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        if name.contains("windows") {
            Self::Windows
        } else if name.contains("mac") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

/// Remembers what is held so changes become RustDesk's down and up events.
pub(crate) struct InputMapper {
    platform: Platform,
    /// Where the shown display starts on the peer's desktop.
    origin: (i32, i32),
    buttons: u8,
    position: Option<(u32, u32)>,
}

impl InputMapper {
    pub(crate) fn new(platform: Platform) -> Self {
        Self { platform, origin: (0, 0), buttons: 0, position: None }
    }

    pub(crate) fn set_origin(&mut self, x: i32, y: i32) {
        self.origin = (x, y);
    }

    /// The messages for `event`, or none if it is not input or cannot be sent.
    pub(crate) fn translate(&mut self, event: &InputEvent) -> Vec<Message> {
        match *event {
            InputEvent::Key { key, pressed } => self.key(key, pressed).into_iter().collect(),
            InputEvent::Pointer { x, y, buttons } => self.pointer(x, y, buttons),
            InputEvent::Wheel { x, y, vertical, horizontal } => {
                self.wheel(x, y, vertical, horizontal)
            },
            _ => Vec::new(),
        }
    }

    /// Releases every button still held.
    pub(crate) fn release_all(&mut self) -> Vec<Message> {
        let (x, y) = self.position.unwrap_or_default();
        self.pointer(x, y, 0)
    }

    fn key(&self, key: Key, pressed: bool) -> Option<Message> {
        if key.evdev == EVDEV_PAUSE && self.platform == Platform::Windows {
            // Windows has no scancode for Pause, so it goes by name.
            return Some(key_message(KeyEvent {
                down: pressed,
                union: Some(key_event::Union::ControlKey(ControlKey::Pause as i32)),
                mode: KeyboardMode::Legacy as i32,
                ..Default::default()
            }));
        }
        let code = match self.platform {
            Platform::Windows => evdev_to_scancode(key.evdev).map(u32::from),
            Platform::Linux => (key.evdev > 0 && key.evdev < 248).then_some(key.evdev + 8),
            Platform::Mac => evdev_to_mac(key.evdev).map(u32::from),
        }?;
        Some(key_message(KeyEvent {
            down: pressed,
            union: Some(key_event::Union::Chr(code)),
            mode: KeyboardMode::Map as i32,
            ..Default::default()
        }))
    }

    fn pointer(&mut self, x: u32, y: u32, held: u8) -> Vec<Message> {
        let held = held & (buttons::LEFT | buttons::MIDDLE | buttons::RIGHT);
        let mut out = Vec::new();
        if self.position != Some((x, y)) {
            out.push(self.mouse(MOVE, x, y));
            self.position = Some((x, y));
        }
        for (ours, theirs) in
            [(buttons::LEFT, LEFT), (buttons::MIDDLE, MIDDLE), (buttons::RIGHT, RIGHT)]
        {
            let (was, is) = (self.buttons & ours != 0, held & ours != 0);
            if was != is {
                out.push(self.mouse(theirs << 3 | if is { DOWN } else { UP }, x, y));
            }
        }
        self.buttons = held;
        out
    }

    /// RustDesk takes a wheel event as notches: positive `y` scrolls up and positive `x` left.
    fn wheel(&mut self, x: u32, y: u32, vertical: i16, horizontal: i16) -> Vec<Message> {
        let mut out = Vec::new();
        if self.position != Some((x, y)) {
            out.push(self.mouse(MOVE, x, y));
            self.position = Some((x, y));
        }
        let vertical = i32::from(vertical).clamp(-MAX_NOTCHES, MAX_NOTCHES);
        let horizontal = i32::from(horizontal).clamp(-MAX_NOTCHES, MAX_NOTCHES);
        if vertical != 0 || horizontal != 0 {
            let event =
                MouseEvent { mask: WHEEL, x: -horizontal, y: vertical, modifiers: Vec::new() };
            out.push(Message::of(message::Union::MouseEvent(event)));
        }
        out
    }

    fn mouse(&self, mask: i32, x: u32, y: u32) -> Message {
        let x = self.origin.0.saturating_add(i32::try_from(x).unwrap_or(i32::MAX));
        let y = self.origin.1.saturating_add(i32::try_from(y).unwrap_or(i32::MAX));
        Message::of(message::Union::MouseEvent(MouseEvent { mask, x, y, modifiers: Vec::new() }))
    }
}

fn key_message(event: KeyEvent) -> Message {
    Message::of(message::Union::KeyEvent(event))
}

/// The macOS virtual key code at the position of an evdev key, from `HIToolbox/Events.h`.
fn evdev_to_mac(evdev: u32) -> Option<u16> {
    const MAIN: [u16; 59] = [
        0xFFFF, 0x35, // ESC
        0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D, // 1 to 0
        0x1B, 0x18, 0x33, 0x30, // - = BACKSPACE TAB
        0x0C, 0x0D, 0x0E, 0x0F, 0x11, 0x10, 0x20, 0x22, 0x1F, 0x23, // Q to P
        0x21, 0x1E, 0x24, 0x3B, // [ ] ENTER LEFTCTRL
        0x00, 0x01, 0x02, 0x03, 0x05, 0x04, 0x26, 0x28, 0x25, // A to L
        0x29, 0x27, 0x32, 0x38, 0x2A, // ; ' ` LEFTSHIFT \
        0x06, 0x07, 0x08, 0x09, 0x0B, 0x2D, 0x2E, // Z to M
        0x2B, 0x2F, 0x2C, 0x3C, 0x43, 0x3A, 0x31,
        0x39, // , . / RIGHTSHIFT KP* LEFTALT SPACE CAPS
    ];
    let code = match evdev {
        0..=58 => MAIN[evdev as usize],
        59 => 0x7A,  // F1
        60 => 0x78,  // F2
        61 => 0x63,  // F3
        62 => 0x76,  // F4
        63 => 0x60,  // F5
        64 => 0x61,  // F6
        65 => 0x62,  // F7
        66 => 0x64,  // F8
        67 => 0x65,  // F9
        68 => 0x6D,  // F10
        69 => 0x47,  // NUMLOCK, the keypad's Clear
        71 => 0x59,  // KP7
        72 => 0x5B,  // KP8
        73 => 0x5C,  // KP9
        74 => 0x4E,  // KPMINUS
        75 => 0x56,  // KP4
        76 => 0x57,  // KP5
        77 => 0x58,  // KP6
        78 => 0x45,  // KPPLUS
        79 => 0x53,  // KP1
        80 => 0x54,  // KP2
        81 => 0x55,  // KP3
        82 => 0x52,  // KP0
        83 => 0x41,  // KPDOT
        86 => 0x0A,  // 102ND, the ISO section key
        87 => 0x67,  // F11
        88 => 0x6F,  // F12
        89 => 0x5E,  // RO, JIS underscore
        92 => 0x68,  // HENKAN, JIS kana
        94 => 0x66,  // MUHENKAN, JIS eisu
        95 => 0x5F,  // KPJPCOMMA
        96 => 0x4C,  // KPENTER
        97 => 0x3E,  // RIGHTCTRL
        98 => 0x4B,  // KPSLASH
        100 => 0x3D, // RIGHTALT
        102 => 0x73, // HOME
        103 => 0x7E, // UP
        104 => 0x74, // PAGEUP
        105 => 0x7B, // LEFT
        106 => 0x7C, // RIGHT
        107 => 0x77, // END
        108 => 0x7D, // DOWN
        109 => 0x79, // PAGEDOWN
        110 => 0x72, // INSERT, which Mac keyboards call Help
        111 => 0x75, // DELETE, forward delete
        113 => 0x4A, // MUTE
        114 => 0x49, // VOLUMEDOWN
        115 => 0x48, // VOLUMEUP
        117 => 0x51, // KPEQUAL
        124 => 0x5D, // YEN
        125 => 0x37, // LEFTMETA, Command
        126 => 0x36, // RIGHTMETA
        127 => 0x6E, // COMPOSE, the contextual menu key
        183 => 0x69, // F13
        184 => 0x6B, // F14
        185 => 0x71, // F15
        186 => 0x6A, // F16
        187 => 0x40, // F17
        188 => 0x4F, // F18
        189 => 0x50, // F19
        190 => 0x5A, // F20
        _ => return None,
    };
    (code != 0xFFFF).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(evdev: u32) -> Key {
        Key { evdev, keysym: None }
    }

    fn key_code(messages: Vec<Message>) -> Option<(bool, u32)> {
        match messages.into_iter().next()?.union? {
            message::Union::KeyEvent(KeyEvent {
                down,
                union: Some(key_event::Union::Chr(code)),
                mode,
                ..
            }) => {
                assert_eq!(mode, KeyboardMode::Map as i32);
                Some((down, code))
            },
            _ => None,
        }
    }

    fn masks(messages: &[Message]) -> Vec<(i32, i32, i32)> {
        messages
            .iter()
            .map(|message| match &message.union {
                Some(message::Union::MouseEvent(event)) => (event.mask, event.x, event.y),
                _ => panic!("not a mouse event"),
            })
            .collect()
    }

    #[test]
    fn keys_use_the_code_space_of_the_peer() {
        let press = |platform, evdev| {
            key_code(
                InputMapper::new(platform)
                    .translate(&InputEvent::Key { key: key(evdev), pressed: true }),
            )
        };
        // A, then the up arrow, which Windows marks as extended.
        assert_eq!(press(Platform::Windows, 30), Some((true, 0x1E)));
        assert_eq!(press(Platform::Windows, 103), Some((true, 0xE048)));
        assert_eq!(press(Platform::Linux, 30), Some((true, 38)));
        assert_eq!(press(Platform::Mac, 30), Some((true, 0x00)));
        assert_eq!(press(Platform::Mac, 125), Some((true, 0x37)));
    }

    #[test]
    fn releases_are_sent_too() {
        let mut mapper = InputMapper::new(Platform::Linux);
        let release = mapper.translate(&InputEvent::Key { key: key(30), pressed: false });
        assert_eq!(key_code(release), Some((false, 38)));
    }

    #[test]
    fn pause_goes_by_name_to_windows() {
        let mut mapper = InputMapper::new(Platform::Windows);
        let messages = mapper.translate(&InputEvent::Key { key: key(EVDEV_PAUSE), pressed: true });
        let Some(message::Union::KeyEvent(event)) = &messages[0].union else { panic!() };
        assert_eq!(event.union, Some(key_event::Union::ControlKey(ControlKey::Pause as i32)));
    }

    #[test]
    fn unmapped_keys_send_nothing() {
        let mut mapper = InputMapper::new(Platform::Mac);
        assert!(mapper.translate(&InputEvent::Key { key: key(0), pressed: true }).is_empty());
        assert!(mapper.translate(&InputEvent::Key { key: key(400), pressed: true }).is_empty());
    }

    #[test]
    fn button_changes_become_down_and_up() {
        let mut mapper = InputMapper::new(Platform::Windows);
        let down = mapper.translate(&InputEvent::Pointer { x: 5, y: 6, buttons: buttons::LEFT });
        assert_eq!(masks(&down), [(MOVE, 5, 6), (LEFT << 3 | DOWN, 5, 6)]);
        let drag = mapper.translate(&InputEvent::Pointer { x: 7, y: 6, buttons: buttons::LEFT });
        assert_eq!(masks(&drag), [(MOVE, 7, 6)]);
        let up = mapper.translate(&InputEvent::Pointer { x: 7, y: 6, buttons: buttons::RIGHT });
        assert_eq!(masks(&up), [(LEFT << 3 | UP, 7, 6), (RIGHT << 3 | DOWN, 7, 6)]);
        let middle =
            mapper.translate(&InputEvent::Pointer { x: 7, y: 6, buttons: buttons::MIDDLE });
        assert_eq!(masks(&middle), [(MIDDLE << 3 | DOWN, 7, 6), (RIGHT << 3 | UP, 7, 6)]);
    }

    #[test]
    fn coordinates_start_at_the_display_origin() {
        let mut mapper = InputMapper::new(Platform::Windows);
        mapper.set_origin(-1920, 100);
        let moved = mapper.translate(&InputEvent::Pointer { x: 10, y: 20, buttons: 0 });
        assert_eq!(masks(&moved), [(MOVE, -1910, 120)]);
    }

    #[test]
    fn the_wheel_sends_notches() {
        let mut mapper = InputMapper::new(Platform::Windows);
        let up = mapper.translate(&InputEvent::Wheel { x: 1, y: 1, vertical: 1, horizontal: 0 });
        assert_eq!(masks(&up), [(MOVE, 1, 1), (WHEEL, 0, 1)]);
        let right = mapper.translate(&InputEvent::Wheel { x: 1, y: 1, vertical: 0, horizontal: 2 });
        assert_eq!(masks(&right), [(WHEEL, -2, 0)]);
        let huge =
            mapper.translate(&InputEvent::Wheel { x: 1, y: 1, vertical: i16::MIN, horizontal: 0 });
        assert_eq!(masks(&huge), [(WHEEL, 0, -MAX_NOTCHES)]);
    }

    #[test]
    fn release_all_lets_go_of_buttons() {
        let mut mapper = InputMapper::new(Platform::Linux);
        mapper.translate(&InputEvent::Pointer {
            x: 3,
            y: 3,
            buttons: buttons::LEFT | buttons::RIGHT,
        });
        let released = mapper.release_all();
        assert_eq!(masks(&released), [(LEFT << 3 | UP, 3, 3), (RIGHT << 3 | UP, 3, 3)]);
    }

    #[test]
    fn mac_codes_are_unique() {
        let mut seen = std::collections::HashMap::new();
        for evdev in 0..256 {
            if let Some(code) = evdev_to_mac(evdev) {
                assert!(seen.insert(code, evdev).is_none(), "{evdev} repeats {code:#x}");
            }
        }
    }

    #[test]
    fn platforms_are_recognised() {
        assert_eq!(Platform::of("Windows"), Platform::Windows);
        assert_eq!(Platform::of("Mac OS"), Platform::Mac);
        assert_eq!(Platform::of("Linux"), Platform::Linux);
        assert_eq!(Platform::of("Android"), Platform::Linux);
    }
}
