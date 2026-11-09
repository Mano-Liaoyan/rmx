//! Maps interface input to RFB key and pointer events.

use rmx_display::{Key, buttons};
use vnc::{ClientKeyEvent, ClientMouseEvent, X11Event};

/// RFB button-mask bits for the wheel, which X11 models as buttons 4 to 7.
const WHEEL_UP: u8 = 1 << 3;
const WHEEL_DOWN: u8 = 1 << 4;
const WHEEL_LEFT: u8 = 1 << 5;
const WHEEL_RIGHT: u8 = 1 << 6;

/// Keeps one wheel event from flooding the connection with press and release pairs.
const MAX_NOTCHES: u16 = 64;

/// One RFB input message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RfbInput {
    Key { keysym: u32, down: bool },
    Pointer { x: u16, y: u16, mask: u8 },
}

impl From<RfbInput> for X11Event {
    fn from(input: RfbInput) -> Self {
        match input {
            RfbInput::Key { keysym, down } => {
                X11Event::KeyEvent(ClientKeyEvent::from((keysym, down)))
            },
            RfbInput::Pointer { x, y, mask } => {
                X11Event::PointerEvent(ClientMouseEvent::from((x, y, mask)))
            },
        }
    }
}

/// The RFB message for a key, or `None` if the interface does not know its keysym.
pub(crate) fn key(key: Key, pressed: bool) -> Option<RfbInput> {
    key.keysym.map(|keysym| RfbInput::Key { keysym, down: pressed })
}

/// The RFB message for a pointer position. Bits of `held` beyond the three buttons are dropped.
pub(crate) fn pointer(x: u32, y: u32, held: u8) -> RfbInput {
    RfbInput::Pointer { x: coordinate(x), y: coordinate(y), mask: held_mask(held) }
}

/// The press and release of a wheel button for each notch, keeping `held` buttons down.
pub(crate) fn wheel(x: u32, y: u32, held: u8, vertical: i16, horizontal: i16) -> Vec<RfbInput> {
    let (x, y, held) = (coordinate(x), coordinate(y), held_mask(held));
    let vertical_button = if vertical > 0 { WHEEL_UP } else { WHEEL_DOWN };
    let horizontal_button = if horizontal > 0 { WHEEL_RIGHT } else { WHEEL_LEFT };

    [(vertical_button, vertical), (horizontal_button, horizontal)]
        .into_iter()
        .flat_map(|(button, notches)| {
            let press = RfbInput::Pointer { x, y, mask: held | button };
            let release = RfbInput::Pointer { x, y, mask: held };
            (0..notches.unsigned_abs().min(MAX_NOTCHES)).flat_map(move |_| [press, release])
        })
        .collect()
}

fn coordinate(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn held_mask(held: u8) -> u8 {
    held & (buttons::LEFT | buttons::MIDDLE | buttons::RIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer_event(x: u16, y: u16, mask: u8) -> RfbInput {
        RfbInput::Pointer { x, y, mask }
    }

    #[test]
    fn keys_use_the_keysym_in_both_directions() {
        let a = Key { evdev: 30, keysym: Some(0x61) };
        assert_eq!(key(a, true), Some(RfbInput::Key { keysym: 0x61, down: true }));
        assert_eq!(key(a, false), Some(RfbInput::Key { keysym: 0x61, down: false }));
    }

    #[test]
    fn keys_without_a_keysym_are_not_sent() {
        assert_eq!(key(Key { evdev: 30, keysym: None }, true), None);
    }

    #[test]
    fn button_bits_match_rfb() {
        assert_eq!(pointer(1, 2, buttons::LEFT), pointer_event(1, 2, 1));
        assert_eq!(pointer(1, 2, buttons::MIDDLE), pointer_event(1, 2, 2));
        assert_eq!(pointer(1, 2, buttons::RIGHT), pointer_event(1, 2, 4));
        assert_eq!(pointer(1, 2, buttons::LEFT | buttons::RIGHT), pointer_event(1, 2, 5));
    }

    #[test]
    fn pointer_ignores_bits_that_are_not_buttons() {
        assert_eq!(pointer(0, 0, 0xff), pointer_event(0, 0, 7));
    }

    #[test]
    fn coordinates_saturate() {
        assert_eq!(pointer(70_000, u32::MAX, 0), pointer_event(u16::MAX, u16::MAX, 0));
    }

    #[test]
    fn wheel_directions_are_buttons_four_to_seven() {
        assert_eq!(wheel(3, 4, 0, 1, 0), [pointer_event(3, 4, 1 << 3), pointer_event(3, 4, 0)]);
        assert_eq!(wheel(3, 4, 0, -1, 0), [pointer_event(3, 4, 1 << 4), pointer_event(3, 4, 0)]);
        assert_eq!(wheel(3, 4, 0, 0, -1), [pointer_event(3, 4, 1 << 5), pointer_event(3, 4, 0)]);
        assert_eq!(wheel(3, 4, 0, 0, 1), [pointer_event(3, 4, 1 << 6), pointer_event(3, 4, 0)]);
    }

    #[test]
    fn wheel_repeats_per_notch_and_keeps_buttons_held() {
        let events = wheel(0, 0, buttons::LEFT, 2, 0);
        let (press, release) = (pointer_event(0, 0, 1 | 1 << 3), pointer_event(0, 0, 1));
        assert_eq!(events, [press, release, press, release]);
    }

    #[test]
    fn wheel_sends_vertical_then_horizontal() {
        let events = wheel(0, 0, 0, 1, 1);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], pointer_event(0, 0, 1 << 3));
        assert_eq!(events[2], pointer_event(0, 0, 1 << 6));
    }

    #[test]
    fn no_notches_send_nothing() {
        assert!(wheel(0, 0, 0, 0, 0).is_empty());
    }

    #[test]
    fn extreme_wheel_values_are_capped() {
        let events = wheel(0, 0, 0, i16::MIN, 0);
        assert_eq!(events.len(), usize::from(MAX_NOTCHES) * 2);
        assert_eq!(events[0], pointer_event(0, 0, 1 << 4));
    }
}
