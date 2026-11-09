//! A pane showing a remote desktop: frames coming in, and the keyboard and pointer going out.
//!
//! The [`DisplayHandle`](rmx_display::DisplayHandle) is read on the runtime. Pixels reach the GPU
//! through [`RemoteTexture`] without the main thread copying them: it only hands each update over,
//! so it never waits on a frame, and an [`Inbox`] bounds how many it may be behind by. The picture
//! is redrawn only when an upload actually changed the texture.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rmx_display::{FrameUpdate, InputEvent, Key as RemoteKey, buttons};
use rmx_render::{RemoteLayout, RemoteTexture, ScaleMode, UploadStatus};
use rmx_ui::widgets::ScaleChoice;
use tokio::sync::{Notify, mpsc};
use winit::event::MouseButton;
use winit::keyboard::{Key, KeyCode, KeyLocation, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;

/// Frames that may wait for the main thread, past which the session starts coalescing.
const INBOX_LIMIT: usize = 8;
/// How long a pane's size has to hold still before the desktop is asked to take it.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);
/// How soon to look again when an upload is still on its way.
pub const UPLOAD_POLL: Duration = Duration::from_millis(2);
/// Smallest desktop worth asking for: RDP refuses anything below it.
const MIN_DESKTOP: u32 = 200;
/// A wheel notch, in window pixels of smooth scrolling.
const WHEEL_NOTCH: f64 = 40.;

/// `evdev` codes of the keys Ctrl+Alt+Del is made of.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_DELETE: u32 = 111;
/// Keysyms of the same keys.
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_DELETE: u32 = 0xffff;

/// What the main thread still has to take from the session.
pub enum Incoming {
    Resized { width: u32, height: u32 },
    Update(FrameUpdate),
}

/// Frames between the task reading the session and the main thread.
#[derive(Default)]
pub struct Inbox {
    queue: Mutex<VecDeque<Incoming>>,
    room: Notify,
    /// Whether the main thread has been told there is something to take.
    told: AtomicBool,
}

impl Inbox {
    /// Add a frame, waiting while the main thread is behind. Returns whether it needs telling.
    pub async fn push(&self, item: Incoming) -> bool {
        loop {
            {
                let mut queue = self.queue.lock().unwrap_or_else(|poison| poison.into_inner());
                if queue.len() < INBOX_LIMIT {
                    queue.push_back(item);
                    break;
                }
                // Over the limit: this frame waits, as the session behind it does.
                drop(queue);
            }
            self.room.notified().await;
        }
        !self.told.swap(true, Ordering::AcqRel)
    }

    /// Hand over everything waiting. `spare` is what the last call left empty.
    fn take(&self, spare: &mut VecDeque<Incoming>) {
        self.told.store(false, Ordering::Release);
        let mut queue = self.queue.lock().unwrap_or_else(|poison| poison.into_inner());
        std::mem::swap(&mut *queue, spare);
        drop(queue);
        self.room.notify_one();
    }
}

/// Which protocol a display speaks, for what it cannot carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    Vnc,
    Rdp,
}

/// What a session gives the pane once it is up.
pub struct DisplayLink {
    pub kind: DisplayKind,
    pub input: mpsc::Sender<InputEvent>,
    pub inbox: Arc<Inbox>,
    pub scale: ScaleChoice,
    pub follow_size: bool,
}

/// What came of a clipboard text that cannot cross VNC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardOutcome {
    /// It is fine as it is.
    Pass,
    /// It stays where it is, and the user has been told why, once.
    Withheld { first_time: bool },
}

/// A remote desktop pane.
pub struct Display {
    pub kind: DisplayKind,
    /// Only ever made and dropped on the main thread, as it owns GL objects.
    pub texture: Option<RemoteTexture>,
    /// What the session said the desktop measures, as a texture would hold it.
    desktop: Option<[u32; 2]>,
    pub scale: ScaleChoice,
    pub scroll: [u32; 2],
    input: mpsc::Sender<InputEvent>,
    inbox: Arc<Inbox>,
    /// Kept between drains so that taking frames does not allocate.
    spare: VecDeque<Incoming>,
    /// Set by the upload worker's notification, cleared when the main thread takes it.
    pub ready: Arc<AtomicBool>,
    /// An upload is due on the next frame.
    pub upload_due: bool,
    pub next_poll: Option<Instant>,
    /// Times the texture has changed since the desktop last changed size.
    pub pictures: u64,
    buttons: u8,
    pointer: [u32; 2],
    held: Vec<(u32, Option<u32>)>,
    wheel: [f64; 2],
    follow_size: bool,
    wanted: Option<[u32; 2]>,
    resize_due: Option<Instant>,
    asked: Option<[u32; 2]>,
    /// What last crossed the clipboard, so that it does not bounce back.
    clipboard: Option<u64>,
    told_clipboard: bool,
}

/// One button's bit in the mask, for those a desktop has.
pub fn button_bit(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(buttons::LEFT),
        MouseButton::Middle => Some(buttons::MIDDLE),
        MouseButton::Right => Some(buttons::RIGHT),
        _ => None,
    }
}

impl Display {
    pub fn new(link: DisplayLink) -> Self {
        let DisplayLink { kind, input, inbox, scale, follow_size } = link;
        Self {
            kind,
            texture: None,
            desktop: None,
            scale,
            scroll: [0, 0],
            input,
            inbox,
            spare: VecDeque::with_capacity(INBOX_LIMIT),
            ready: Arc::new(AtomicBool::new(false)),
            upload_due: false,
            next_poll: None,
            pictures: 0,
            buttons: 0,
            pointer: [0, 0],
            held: Vec::new(),
            wheel: [0., 0.],
            follow_size,
            wanted: None,
            resize_due: None,
            asked: None,
            clipboard: None,
            told_clipboard: false,
        }
    }

    /// How large the desktop is, if the session has said.
    pub fn desktop(&self) -> Option<[u32; 2]> {
        self.desktop
    }

    /// What `RemoteLayout` needs to place the picture.
    pub fn mode(&self) -> ScaleMode {
        match self.scale {
            ScaleChoice::Fit => ScaleMode::Fit,
            ScaleChoice::Native => ScaleMode::Native { scroll: self.scroll },
        }
    }

    /// Take the frames that have come in, in order, with `apply` doing what each one calls for.
    pub fn take_frames(&mut self, mut apply: impl FnMut(&mut Self, Incoming)) {
        let mut queue = std::mem::take(&mut self.spare);
        self.inbox.take(&mut queue);
        while let Some(item) = queue.pop_front() {
            apply(self, item);
        }
        self.spare = queue;
    }

    /// The session said the desktop is this size.
    pub fn set_desktop(&mut self, width: u32, height: u32) {
        self.desktop = Some([width, height]);
        self.asked = None;
        self.pictures = 0;
    }

    /// Whether the texture is not of the size the desktop has now said it is.
    pub fn texture_stale(&self) -> bool {
        match (&self.texture, self.desktop) {
            (Some(texture), Some(desktop)) => texture.size() != desktop,
            (None, Some(_)) => true,
            _ => false,
        }
    }

    /// Give new pixels to the texture, if it is ready for them.
    pub fn submit(&mut self, update: FrameUpdate) {
        if let Some(texture) = &mut self.texture {
            texture.submit(update);
        }
    }

    /// Apply what has been uploaded. Returns whether the picture changed.
    pub fn upload(&mut self) -> bool {
        let Some(texture) = &mut self.texture else {
            self.upload_due = false;
            return false;
        };
        let UploadStatus { changed, pending } = texture.upload();
        self.upload_due = pending;
        self.pictures += u64::from(changed);
        changed
    }

    fn send(&self, event: InputEvent) {
        if self.input.try_send(event).is_err() {
            log::warn!("A remote input event was dropped: the session is not keeping up");
        }
    }

    /// Let go of everything held down: the pane lost the keyboard.
    pub fn release_all(&mut self) {
        for (evdev, keysym) in std::mem::take(&mut self.held) {
            let key = RemoteKey { evdev, keysym };
            self.send(InputEvent::Key { key, pressed: false });
        }
        if self.buttons != 0 {
            self.buttons = 0;
            let [x, y] = self.pointer;
            self.send(InputEvent::Pointer { x, y, buttons: 0 });
        }
    }

    /// A key went down or came up.
    pub fn key(&mut self, key: RemoteKey, pressed: bool, repeat: bool) {
        if pressed {
            // X servers repeat a held key themselves. RDP servers rely on the client.
            if repeat && self.kind == DisplayKind::Vnc {
                return;
            }
            if !repeat {
                self.held.retain(|&(evdev, _)| evdev != key.evdev);
                self.held.push((key.evdev, key.keysym));
            }
            self.send(InputEvent::Key { key, pressed: true });
        } else {
            // A release uses what the press sent: the shift may have gone up since.
            let Some(index) = self.held.iter().position(|&(evdev, _)| evdev == key.evdev) else {
                return;
            };
            let (evdev, keysym) = self.held.remove(index);
            self.send(InputEvent::Key { key: RemoteKey { evdev, keysym }, pressed: false });
        }
    }

    /// Send the salute, whatever is held.
    pub fn ctrl_alt_del(&mut self) {
        for (evdev, keysym) in std::mem::take(&mut self.held) {
            self.send(InputEvent::Key { key: RemoteKey { evdev, keysym }, pressed: false });
        }
        for key in [(KEY_LEFTCTRL, XK_CONTROL_L), (KEY_LEFTALT, XK_ALT_L), (KEY_DELETE, XK_DELETE)] {
            self.send(InputEvent::Key {
                key: RemoteKey { evdev: key.0, keysym: Some(key.1) },
                pressed: true,
            });
        }
        for key in [(KEY_DELETE, XK_DELETE), (KEY_LEFTALT, XK_ALT_L), (KEY_LEFTCTRL, XK_CONTROL_L)] {
            self.send(InputEvent::Key {
                key: RemoteKey { evdev: key.0, keysym: Some(key.1) },
                pressed: false,
            });
        }
    }

    /// The pointer moved over the picture. Moves give way when the session is behind, so that
    /// keys and clicks always find room.
    pub fn pointer_moved(&mut self, x: u32, y: u32) {
        self.pointer = [x, y];
        if self.input.capacity() * 2 < self.input.max_capacity() {
            return;
        }
        self.send(InputEvent::Pointer { x, y, buttons: self.buttons });
    }

    /// A button changed, with the pointer at `x`, `y`.
    pub fn button(&mut self, x: u32, y: u32, bit: u8, pressed: bool) {
        self.pointer = [x, y];
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.send(InputEvent::Pointer { x, y, buttons: self.buttons });
    }

    /// Whether any button is held down.
    pub fn any_button(&self) -> bool {
        self.buttons != 0
    }

    /// The wheel turned by `lines`, or by `pixels` on a touchpad. Sends whole notches.
    pub fn wheel(&mut self, delta: WheelDelta, x: u32, y: u32) {
        let pixels = match delta {
            WheelDelta::Lines(lines) => lines.map(|line| line * WHEEL_NOTCH),
            WheelDelta::Pixels(pixels) => pixels,
        };
        self.wheel[0] += pixels[0];
        self.wheel[1] += pixels[1];
        let whole = |rest: &mut f64| {
            let notches = (*rest / WHEEL_NOTCH).trunc();
            *rest -= notches * WHEEL_NOTCH;
            notches as i16
        };
        let horizontal = whole(&mut self.wheel[0]);
        let vertical = whole(&mut self.wheel[1]);
        if horizontal != 0 || vertical != 0 {
            self.send(InputEvent::Wheel { x, y, vertical, horizontal });
        }
    }

    /// Pan a 1:1 view by whole lines. Returns whether anything moved.
    pub fn pan(&mut self, delta: WheelDelta, layout: &RemoteLayout) -> bool {
        const LINE: f64 = 48.;
        let pixels = match delta {
            WheelDelta::Lines(lines) => lines.map(|line| line * LINE),
            WheelDelta::Pixels(pixels) => pixels,
        };
        let shift = |value: f64, was: u32, max: u32| {
            (f64::from(was) - value).clamp(0., f64::from(max)).round() as u32
        };
        let scroll = [
            shift(pixels[0], self.scroll[0], layout.max_scroll[0]),
            shift(pixels[1], self.scroll[1], layout.max_scroll[1]),
        ];
        let moved = scroll != self.scroll;
        self.scroll = scroll;
        moved
    }

    /// The pane settled at this size, in pixels, as far as it is known yet.
    pub fn pane_resized(&mut self, width: u32, height: u32, now: Instant) {
        if !self.follow_size || width < MIN_DESKTOP || height < MIN_DESKTOP {
            return;
        }
        let size = [width, height];
        if self.wanted != Some(size) {
            self.wanted = Some(size);
            self.resize_due = Some(now + RESIZE_DEBOUNCE);
        }
    }

    /// When the desktop should next hear about the pane's size, if it should.
    pub fn resize_deadline(&self) -> Option<Instant> {
        self.resize_due
    }

    /// The pane has stopped moving at `now`: ask for its size, once, if the debounce has passed.
    pub fn settle(&mut self, now: Instant) {
        let Some(due) = self.resize_due else { return };
        if now < due {
            return;
        }
        self.resize_due = None;
        let Some(wanted) = self.wanted else { return };
        if self.desktop == Some(wanted) || self.asked == Some(wanted) {
            return;
        }
        self.asked = Some(wanted);
        let [width, height] = wanted;
        self.send(InputEvent::Resize { width, height });
    }

    /// Whether the desktop has taken the size of the pane.
    #[cfg(any(test, feature = "bench"))]
    pub fn fits_pane(&self) -> bool {
        self.desktop.is_some() && self.desktop == self.wanted
    }

    /// Whether `text`, read from the other end, may be believed.
    pub fn clipboard_in(&mut self, text: &str) -> ClipboardOutcome {
        match self.check_clipboard(text) {
            ClipboardOutcome::Pass => {
                self.clipboard = Some(hash_text(text));
                ClipboardOutcome::Pass
            },
            withheld => withheld,
        }
    }

    /// Offer the local clipboard to the session, if it has changed. Returns what became of it.
    pub fn clipboard_out(&mut self, text: &str) -> ClipboardOutcome {
        if text.is_empty() || self.clipboard == Some(hash_text(text)) {
            return ClipboardOutcome::Pass;
        }
        let outcome = self.check_clipboard(text);
        if outcome == ClipboardOutcome::Pass {
            self.clipboard = Some(hash_text(text));
            self.send(InputEvent::Clipboard(text.to_owned()));
        }
        outcome
    }

    /// `vnc-rs` carries text as Latin-1 going one way and lossy UTF-8 the other, and has no
    /// extended clipboard: anything past ASCII arrives as garbage. The whole text is held back
    /// rather than sending what is left of it.
    fn check_clipboard(&mut self, text: &str) -> ClipboardOutcome {
        if self.kind == DisplayKind::Rdp || text.is_ascii() {
            return ClipboardOutcome::Pass;
        }
        let first_time = !self.told_clipboard;
        self.told_clipboard = true;
        ClipboardOutcome::Withheld { first_time }
    }

    /// End the session.
    pub fn hang_up(&mut self) {
        self.release_all();
        let _ = self.input.try_send(InputEvent::Disconnect);
    }
}

/// What a wheel event said.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WheelDelta {
    Lines([f64; 2]),
    Pixels([f64; 2]),
}

/// A fingerprint of clipboard text, to tell whether it has changed.
fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Latin-1 keysyms are the code point, and everything else in Unicode has its own range.
fn char_keysym(character: char) -> u32 {
    let code = character as u32;
    match code {
        0x20..=0x7e | 0xa0..=0xff => code,
        _ => 0x0100_0000 | code,
    }
}

/// A digit or operator of the numeric keypad.
fn keypad_keysym(character: char) -> Option<u32> {
    match character {
        '0'..='9' => Some(0xffb0 + (character as u32 - '0' as u32)),
        '.' | ',' => Some(0xffae),
        '+' => Some(0xffab),
        '-' => Some(0xffad),
        '*' => Some(0xffaa),
        '/' => Some(0xffaf),
        _ => None,
    }
}

fn named_keysym(key: NamedKey, location: KeyLocation) -> Option<u32> {
    let right = location == KeyLocation::Right;
    let numpad = location == KeyLocation::Numpad;
    Some(match key {
        NamedKey::Backspace => 0xff08,
        NamedKey::Tab => 0xff09,
        NamedKey::Enter if numpad => 0xff8d,
        NamedKey::Enter => 0xff0d,
        NamedKey::Escape => 0xff1b,
        NamedKey::Space => 0x20,
        NamedKey::Delete => 0xffff,
        NamedKey::Home => 0xff50,
        NamedKey::ArrowLeft => 0xff51,
        NamedKey::ArrowUp => 0xff52,
        NamedKey::ArrowRight => 0xff53,
        NamedKey::ArrowDown => 0xff54,
        NamedKey::PageUp => 0xff55,
        NamedKey::PageDown => 0xff56,
        NamedKey::End => 0xff57,
        NamedKey::Insert => 0xff63,
        NamedKey::Pause => 0xff13,
        NamedKey::ScrollLock => 0xff14,
        NamedKey::PrintScreen => 0xff61,
        NamedKey::ContextMenu => 0xff67,
        NamedKey::NumLock => 0xff7f,
        NamedKey::CapsLock => 0xffe5,
        NamedKey::Shift if right => 0xffe2,
        NamedKey::Shift => 0xffe1,
        NamedKey::Control if right => 0xffe4,
        NamedKey::Control => 0xffe3,
        NamedKey::Alt if right => 0xffea,
        NamedKey::Alt => 0xffe9,
        NamedKey::AltGraph => 0xfe03,
        NamedKey::Super if right => 0xffec,
        NamedKey::Super => 0xffeb,
        NamedKey::Meta if right => 0xffec,
        NamedKey::Meta => 0xffeb,
        NamedKey::F1 => 0xffbe,
        NamedKey::F2 => 0xffbf,
        NamedKey::F3 => 0xffc0,
        NamedKey::F4 => 0xffc1,
        NamedKey::F5 => 0xffc2,
        NamedKey::F6 => 0xffc3,
        NamedKey::F7 => 0xffc4,
        NamedKey::F8 => 0xffc5,
        NamedKey::F9 => 0xffc6,
        NamedKey::F10 => 0xffc7,
        NamedKey::F11 => 0xffc8,
        NamedKey::F12 => 0xffc9,
        _ => return None,
    })
}

/// The keysym of what a key produces, when it is known.
fn keysym(logical: &Key, location: KeyLocation) -> Option<u32> {
    match logical {
        Key::Named(key) => named_keysym(*key, location),
        Key::Character(text) => {
            let mut characters = text.chars();
            let (character, rest) = (characters.next()?, characters.next());
            if rest.is_some() {
                return None;
            }
            if location == KeyLocation::Numpad {
                keypad_keysym(character)
            } else {
                Some(char_keysym(character))
            }
        },
        _ => None,
    }
}

/// A key as a session wants it, or none for a key with no physical code.
pub fn remote_key(physical: PhysicalKey, logical: &Key, location: KeyLocation) -> Option<RemoteKey> {
    Some(RemoteKey { evdev: physical.to_scancode()?, keysym: keysym(logical, location) })
}

/// Whether this is the key that, with Ctrl and Alt, stands for Ctrl+Alt+Del.
pub fn is_salute_key(physical: PhysicalKey) -> bool {
    physical == PhysicalKey::Code(KeyCode::End)
}

/// The physical key and character typing `character` takes on a US layout, for scripted input.
#[cfg(feature = "bench")]
pub fn typed_key(character: char) -> Option<(KeyCode, Key)> {
    let code = match character {
        'a'..='z' => {
            let letters = [
                KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC, KeyCode::KeyD, KeyCode::KeyE, KeyCode::KeyF,
                KeyCode::KeyG, KeyCode::KeyH, KeyCode::KeyI, KeyCode::KeyJ, KeyCode::KeyK, KeyCode::KeyL,
                KeyCode::KeyM, KeyCode::KeyN, KeyCode::KeyO, KeyCode::KeyP, KeyCode::KeyQ, KeyCode::KeyR,
                KeyCode::KeyS, KeyCode::KeyT, KeyCode::KeyU, KeyCode::KeyV, KeyCode::KeyW, KeyCode::KeyX,
                KeyCode::KeyY, KeyCode::KeyZ,
            ];
            letters[(character as u8 - b'a') as usize]
        },
        '0' => KeyCode::Digit0,
        '1' => KeyCode::Digit1,
        '2' => KeyCode::Digit2,
        '3' => KeyCode::Digit3,
        '4' => KeyCode::Digit4,
        '5' => KeyCode::Digit5,
        '6' => KeyCode::Digit6,
        '7' => KeyCode::Digit7,
        '8' => KeyCode::Digit8,
        '9' => KeyCode::Digit9,
        '-' => KeyCode::Minus,
        '.' => KeyCode::Period,
        ' ' => KeyCode::Space,
        '\n' => {
            return Some((KeyCode::Enter, Key::Named(NamedKey::Enter)));
        },
        _ => return None,
    };
    Some((code, Key::Character(character.to_string().into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: DisplayKind) -> (Display, mpsc::Receiver<InputEvent>) {
        let (input, events) = mpsc::channel(16);
        let inbox = Arc::new(Inbox::default());
        let link = DisplayLink { kind, input, inbox, scale: ScaleChoice::Fit, follow_size: true };
        (Display::new(link), events)
    }

    fn drain(events: &mut mpsc::Receiver<InputEvent>) -> Vec<InputEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        seen
    }

    #[test]
    fn a_key_carries_its_physical_code_and_what_it_produces() {
        let physical = PhysicalKey::Code(KeyCode::KeyA);
        let key = remote_key(physical, &Key::Character("a".into()), KeyLocation::Standard).unwrap();
        assert_eq!((key.evdev, key.keysym), (30, Some(0x61)));
        let key = remote_key(physical, &Key::Character("A".into()), KeyLocation::Standard).unwrap();
        assert_eq!((key.evdev, key.keysym), (30, Some(0x41)));
    }

    #[test]
    fn modifiers_are_told_apart_by_side() {
        let shift = |code, location| {
            let key = remote_key(PhysicalKey::Code(code), &Key::Named(NamedKey::Shift), location);
            key.unwrap().keysym
        };
        assert_eq!(shift(KeyCode::ShiftLeft, KeyLocation::Left), Some(0xffe1));
        assert_eq!(shift(KeyCode::ShiftRight, KeyLocation::Right), Some(0xffe2));
    }

    #[test]
    fn keysyms_cover_latin_one_and_unicode() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('你'), 0x0100_4f60);
    }

    #[test]
    fn the_keypad_sends_keypad_symbols() {
        let digit = Key::Character("5".into());
        assert_eq!(keysym(&digit, KeyLocation::Numpad), Some(0xffb5));
        assert_eq!(keysym(&digit, KeyLocation::Standard), Some(0x35));
        assert_eq!(keysym(&Key::Named(NamedKey::Enter), KeyLocation::Numpad), Some(0xff8d));
    }

    #[test]
    fn a_release_says_what_the_press_said() {
        let (mut display, mut events) = link(DisplayKind::Vnc);
        let upper = RemoteKey { evdev: 30, keysym: Some(0x41) };
        let lower = RemoteKey { evdev: 30, keysym: Some(0x61) };
        display.key(upper, true, false);
        display.key(lower, false, false);
        let seen = drain(&mut events);
        assert_eq!(seen, [
            InputEvent::Key { key: upper, pressed: true },
            InputEvent::Key { key: upper, pressed: false },
        ]);
    }

    #[test]
    fn losing_the_keyboard_releases_what_is_held() {
        let (mut display, mut events) = link(DisplayKind::Rdp);
        let key = RemoteKey { evdev: 30, keysym: None };
        display.key(key, true, false);
        display.button(5, 6, buttons::LEFT, true);
        let _ = drain(&mut events);
        display.release_all();
        assert_eq!(drain(&mut events), [
            InputEvent::Key { key, pressed: false },
            InputEvent::Pointer { x: 5, y: 6, buttons: 0 },
        ]);
    }

    #[test]
    fn a_held_key_is_not_repeated_on_vnc() {
        let (mut display, mut events) = link(DisplayKind::Vnc);
        let key = RemoteKey { evdev: 30, keysym: Some(0x61) };
        display.key(key, true, false);
        display.key(key, true, true);
        assert_eq!(drain(&mut events).len(), 1);
        let (mut display, mut events) = link(DisplayKind::Rdp);
        display.key(key, true, false);
        display.key(key, true, true);
        assert_eq!(drain(&mut events).len(), 2);
    }

    #[test]
    fn ctrl_alt_del_goes_down_and_comes_back_up_in_order() {
        let (mut display, mut events) = link(DisplayKind::Rdp);
        display.ctrl_alt_del();
        let codes: Vec<(u32, bool)> = drain(&mut events)
            .into_iter()
            .map(|event| match event {
                InputEvent::Key { key, pressed } => (key.evdev, pressed),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(codes, [(29, true), (56, true), (111, true), (111, false), (56, false), (29, false)]);
    }

    #[test]
    fn a_resize_waits_for_the_pane_to_settle_and_is_asked_once() {
        let (mut display, mut events) = link(DisplayKind::Rdp);
        let now = Instant::now();
        display.pane_resized(1000, 800, now);
        display.pane_resized(1010, 800, now + Duration::from_millis(100));
        display.settle(now + Duration::from_millis(200));
        assert!(drain(&mut events).is_empty(), "still within the debounce");
        display.settle(now + Duration::from_millis(400));
        assert_eq!(drain(&mut events), [InputEvent::Resize { width: 1010, height: 800 }]);
        display.settle(now + Duration::from_secs(5));
        assert!(drain(&mut events).is_empty(), "asked once");
    }

    #[test]
    fn nothing_is_asked_while_the_desktop_already_fits() {
        let (mut display, mut events) = link(DisplayKind::Rdp);
        let now = Instant::now();
        display.set_desktop(1000, 800);
        display.pane_resized(1000, 800, now);
        display.settle(now + Duration::from_secs(1));
        assert!(drain(&mut events).is_empty());
        assert!(display.fits_pane());
    }

    #[test]
    fn a_keep_size_display_never_asks() {
        let (input, mut events) = mpsc::channel(4);
        let inbox = Arc::new(Inbox::default());
        let link = DisplayLink { kind: DisplayKind::Vnc, input, inbox, scale: ScaleChoice::Fit, follow_size: false };
        let mut display = Display::new(link);
        let now = Instant::now();
        display.pane_resized(1000, 800, now);
        display.settle(now + Duration::from_secs(1));
        assert!(drain(&mut events).is_empty());
    }

    #[test]
    fn text_past_ascii_does_not_cross_vnc() {
        let (mut display, mut events) = link(DisplayKind::Vnc);
        let outcome = display.clipboard_out("中文 text");
        assert_eq!(outcome, ClipboardOutcome::Withheld { first_time: true });
        let outcome = display.clipboard_in("caf\u{fffd}");
        assert_eq!(outcome, ClipboardOutcome::Withheld { first_time: false });
        assert!(drain(&mut events).is_empty(), "no garbage crosses");
        assert_eq!(display.clipboard_out("plain"), ClipboardOutcome::Pass);
        assert_eq!(drain(&mut events), [InputEvent::Clipboard("plain".to_owned())]);
    }

    #[test]
    fn rdp_carries_everything_and_nothing_bounces_back() {
        let (mut display, mut events) = link(DisplayKind::Rdp);
        assert_eq!(display.clipboard_in("中文"), ClipboardOutcome::Pass);
        assert_eq!(display.clipboard_out("中文"), ClipboardOutcome::Pass);
        assert!(drain(&mut events).is_empty(), "what came from there does not go back");
        assert_eq!(display.clipboard_out("新的"), ClipboardOutcome::Pass);
        assert_eq!(drain(&mut events), [InputEvent::Clipboard("新的".to_owned())]);
    }

    #[test]
    fn whole_notches_of_the_wheel_are_sent_and_the_rest_kept() {
        let (mut display, mut events) = link(DisplayKind::Rdp);
        display.wheel(WheelDelta::Pixels([0., 30.]), 1, 2);
        assert!(drain(&mut events).is_empty());
        display.wheel(WheelDelta::Pixels([0., 30.]), 1, 2);
        assert_eq!(drain(&mut events), [InputEvent::Wheel { x: 1, y: 2, vertical: 1, horizontal: 0 }]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn frames_wait_rather_than_pile_up() {
        let inbox = Arc::new(Inbox::default());
        let update = || Incoming::Resized { width: 1, height: 1 };
        // The first frame tells the main thread; the ones after it are coalesced.
        assert!(inbox.push(update()).await);
        for _ in 1..INBOX_LIMIT {
            assert!(!inbox.push(update()).await);
        }

        let waiting = inbox.clone();
        let task = tokio::spawn(async move { waiting.push(update()).await });
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "over the limit, the session has to wait");

        let mut taken = VecDeque::new();
        inbox.take(&mut taken);
        assert_eq!(taken.len(), INBOX_LIMIT);
        assert!(task.await.unwrap(), "after a drain, the main thread is told again");
    }
}
