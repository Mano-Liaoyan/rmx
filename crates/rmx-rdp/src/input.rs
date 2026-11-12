//! Interface input to RDP fast-path events.

use ironrdp_input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use rmx_display::keymap::{EVDEV_PAUSE, evdev_to_scancode};
use rmx_display::{InputEvent, Key, buttons};

/// Rotation units of one wheel notch.
const WHEEL_DELTA: i16 = 120;

/// Remembers held keys and buttons so changes can be expressed as RDP's press and release events.
pub(crate) struct InputMapper {
    state: Database,
    buttons: u8,
}

impl InputMapper {
    pub fn new() -> Self {
        Self { state: Database::new(), buttons: 0 }
    }

    /// The fast-path events for `event`, or nothing if it is not input.
    pub fn translate(&mut self, event: &InputEvent) -> Vec<FastPathInputEvent> {
        match *event {
            InputEvent::Key { key, pressed } => self.key(key, pressed),
            InputEvent::Pointer { x, y, buttons } => self.pointer(x, y, buttons),
            InputEvent::Wheel { x, y, vertical, horizontal } => {
                self.wheel(x, y, vertical, horizontal)
            },
            _ => Vec::new(),
        }
    }

    /// Releases everything still held, so the remote is not left with stuck modifiers.
    pub fn release_all(&mut self) -> Vec<FastPathInputEvent> {
        self.buttons = 0;
        self.state.release_all().into_iter().collect()
    }

    fn key(&mut self, key: Key, pressed: bool) -> Vec<FastPathInputEvent> {
        if key.evdev == EVDEV_PAUSE {
            return if pressed { pause_sequence() } else { Vec::new() };
        }
        let Some(scancode) = evdev_to_scancode(key.evdev).map(Scancode::from_u16) else {
            return Vec::new();
        };
        if pressed && self.state.is_key_pressed(scancode) {
            // The database drops repeats, but a repeat is how RDP auto-repeats a held key.
            let (extended, code) = scancode.as_u8();
            let flags = if extended { KeyboardFlags::EXTENDED } else { KeyboardFlags::empty() };
            return vec![FastPathInputEvent::KeyboardEvent(flags, code)];
        }
        let operation = if pressed {
            Operation::KeyPressed(scancode)
        } else {
            Operation::KeyReleased(scancode)
        };
        self.state.apply([operation]).into_iter().collect()
    }

    fn pointer(&mut self, x: u32, y: u32, held: u8) -> Vec<FastPathInputEvent> {
        let mut operations = vec![Operation::MouseMove(position(x, y))];
        for (bit, button) in [
            (buttons::LEFT, MouseButton::Left),
            (buttons::MIDDLE, MouseButton::Middle),
            (buttons::RIGHT, MouseButton::Right),
        ] {
            match (self.buttons & bit != 0, held & bit != 0) {
                (false, true) => operations.push(Operation::MouseButtonPressed(button)),
                (true, false) => operations.push(Operation::MouseButtonReleased(button)),
                _ => {},
            }
        }
        self.buttons = held;
        self.state.apply(operations).into_iter().collect()
    }

    fn wheel(&mut self, x: u32, y: u32, vertical: i16, horizontal: i16) -> Vec<FastPathInputEvent> {
        let mut operations = vec![Operation::MouseMove(position(x, y))];
        for (is_vertical, notches) in [(true, vertical), (false, horizontal)] {
            if notches != 0 {
                operations.push(Operation::WheelRotations(WheelRotations {
                    is_vertical,
                    rotation_units: notches.saturating_mul(WHEEL_DELTA),
                }));
            }
        }
        self.state.apply(operations).into_iter().collect()
    }
}

fn position(x: u32, y: u32) -> MousePosition {
    MousePosition { x: x.try_into().unwrap_or(u16::MAX), y: y.try_into().unwrap_or(u16::MAX) }
}

/// Pause is a make and break of Ctrl+NumLock with the `E1` prefix on Ctrl, sent all at once.
fn pause_sequence() -> Vec<FastPathInputEvent> {
    const CTRL: u8 = 0x1D;
    const NUMLOCK: u8 = 0x45;
    vec![
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1, CTRL),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), NUMLOCK),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1 | KeyboardFlags::RELEASE, CTRL),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, NUMLOCK),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 30;
    const UP: u32 = 103;

    fn key(evdev: u32, pressed: bool) -> InputEvent {
        InputEvent::Key { key: Key { evdev, keysym: None }, pressed }
    }

    fn keyboard(event: &FastPathInputEvent) -> (KeyboardFlags, u8) {
        match event {
            FastPathInputEvent::KeyboardEvent(flags, code) => (*flags, *code),
            other => panic!("expected a keyboard event, got {other:?}"),
        }
    }

    #[test]
    fn press_then_release() {
        let mut mapper = InputMapper::new();
        let press = mapper.translate(&key(A, true));
        assert_eq!(
            press.iter().map(keyboard).collect::<Vec<_>>(),
            [(KeyboardFlags::empty(), 0x1E)]
        );
        let release = mapper.translate(&key(A, false));
        assert_eq!(
            release.iter().map(keyboard).collect::<Vec<_>>(),
            [(KeyboardFlags::RELEASE, 0x1E)]
        );
    }

    #[test]
    fn extended_keys_set_the_extended_flag() {
        let mut mapper = InputMapper::new();
        let press = mapper.translate(&key(UP, true));
        assert_eq!(
            press.iter().map(keyboard).collect::<Vec<_>>(),
            [(KeyboardFlags::EXTENDED, 0x48)]
        );
    }

    #[test]
    fn a_held_key_repeats() {
        let mut mapper = InputMapper::new();
        mapper.translate(&key(A, true));
        let repeat = mapper.translate(&key(A, true));
        assert_eq!(
            repeat.iter().map(keyboard).collect::<Vec<_>>(),
            [(KeyboardFlags::empty(), 0x1E)]
        );
    }

    #[test]
    fn pause_is_a_complete_sequence_on_press_only() {
        let mut mapper = InputMapper::new();
        let events = mapper.translate(&key(EVDEV_PAUSE, true));
        assert_eq!(events.len(), 4);
        assert_eq!(keyboard(&events[0]), (KeyboardFlags::EXTENDED1, 0x1D));
        assert!(mapper.translate(&key(EVDEV_PAUSE, false)).is_empty());
    }

    #[test]
    fn unmapped_keys_send_nothing() {
        let mut mapper = InputMapper::new();
        assert!(mapper.translate(&key(0x110, true)).is_empty());
    }

    #[test]
    fn button_changes_become_events() {
        let mut mapper = InputMapper::new();
        let down = mapper.translate(&InputEvent::Pointer { x: 5, y: 6, buttons: buttons::LEFT });
        assert_eq!(down.len(), 2, "a move, then a press: {down:?}");
        let up = mapper.translate(&InputEvent::Pointer { x: 5, y: 6, buttons: 0 });
        assert_eq!(up.len(), 1, "only a release: {up:?}");
    }

    #[test]
    fn release_all_releases_held_keys() {
        let mut mapper = InputMapper::new();
        mapper.translate(&key(A, true));
        let events = mapper.release_all();
        assert_eq!(
            events.iter().map(keyboard).collect::<Vec<_>>(),
            [(KeyboardFlags::RELEASE, 0x1E)]
        );
    }

    #[test]
    fn wheel_moves_then_rotates() {
        let mut mapper = InputMapper::new();
        let events =
            mapper.translate(&InputEvent::Wheel { x: 1, y: 2, vertical: 1, horizontal: 0 });
        assert_eq!(events.len(), 2, "{events:?}");
    }
}
