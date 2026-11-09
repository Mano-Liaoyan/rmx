//! Translating winit's events into egui's.

use egui::{
    Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2, TouchPhase, pos2,
    vec2,
};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::CursorIcon;

pub(crate) fn modifiers(state: ModifiersState) -> Modifiers {
    Modifiers {
        alt: state.alt_key(),
        ctrl: state.control_key(),
        shift: state.shift_key(),
        mac_cmd: false,
        command: state.control_key(),
    }
}

pub(crate) fn button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Primary),
        MouseButton::Right => Some(PointerButton::Secondary),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::Back => Some(PointerButton::Extra1),
        MouseButton::Forward => Some(PointerButton::Extra2),
        MouseButton::Other(_) => None,
    }
}

pub(crate) fn wheel(delta: MouseScrollDelta, ppp: f32, modifiers: Modifiers) -> Event {
    let (unit, delta) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (MouseWheelUnit::Line, vec2(x, y)),
        MouseScrollDelta::PixelDelta(p) => {
            (MouseWheelUnit::Point, vec2(p.x as f32, p.y as f32) / ppp)
        },
    };
    Event::MouseWheel {
        unit,
        delta,
        phase: TouchPhase::Move,
        source: MouseWheelSource::Unknown,
        modifiers,
    }
}

pub(crate) fn point(x: f64, y: f64, ppp: f32) -> Pos2 {
    pos2(x as f32 / ppp, y as f32 / ppp)
}

fn key(key: &WinitKey) -> Option<Key> {
    match key {
        WinitKey::Named(named) => Some(match named {
            NamedKey::ArrowDown => Key::ArrowDown,
            NamedKey::ArrowLeft => Key::ArrowLeft,
            NamedKey::ArrowRight => Key::ArrowRight,
            NamedKey::ArrowUp => Key::ArrowUp,
            NamedKey::Escape => Key::Escape,
            NamedKey::Tab => Key::Tab,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Enter => Key::Enter,
            NamedKey::Space => Key::Space,
            NamedKey::Insert => Key::Insert,
            NamedKey::Delete => Key::Delete,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            _ => return None,
        }),
        WinitKey::Character(text) => Key::from_name(&text.to_uppercase()),
        _ => None,
    }
}

/// The egui events of a key press or release, appended to `out`.
pub(crate) fn key_events(event: &KeyEvent, mods: Modifiers, out: &mut Vec<Event>) {
    let pressed = event.state == ElementState::Pressed;
    if let Some(k) = key(&event.logical_key) {
        if pressed && mods.ctrl && !mods.alt {
            match k {
                Key::C => out.push(Event::Copy),
                Key::X => out.push(Event::Cut),
                _ => {},
            }
        }
        out.push(Event::Key {
            key: k,
            physical_key: None,
            pressed,
            repeat: event.repeat,
            modifiers: mods,
        });
    }
    if pressed
        && !mods.ctrl
        && let Some(text) = &event.text
        && text.chars().all(|c| !c.is_control())
    {
        out.push(Event::Text(text.to_string()));
    }
}

/// The egui event for an input-method update.
pub(crate) fn ime_event(ime: &Ime) -> Option<Event> {
    match ime {
        Ime::Preedit(text, cursor) => Some(Event::Ime(egui::ImeEvent::Preedit {
            text: text.clone(),
            active_range_chars: cursor.map(|(a, b)| {
                let at = |byte: usize| text[..byte.min(text.len())].chars().count();
                at(a)..at(b)
            }),
        })),
        Ime::Commit(text) => Some(Event::Ime(egui::ImeEvent::Commit(text.clone()))),
        Ime::Enabled | Ime::Disabled => None,
    }
}

/// The winit cursor for an egui cursor.
pub fn cursor(icon: egui::CursorIcon) -> Option<CursorIcon> {
    use egui::CursorIcon as C;
    Some(match icon {
        C::None => return None,
        C::Default => CursorIcon::Default,
        C::ContextMenu => CursorIcon::ContextMenu,
        C::Help => CursorIcon::Help,
        C::PointingHand => CursorIcon::Pointer,
        C::Progress => CursorIcon::Progress,
        C::Wait => CursorIcon::Wait,
        C::Cell => CursorIcon::Cell,
        C::Crosshair => CursorIcon::Crosshair,
        C::Text => CursorIcon::Text,
        C::VerticalText => CursorIcon::VerticalText,
        C::Alias => CursorIcon::Alias,
        C::Copy => CursorIcon::Copy,
        C::Move => CursorIcon::Move,
        C::NoDrop => CursorIcon::NoDrop,
        C::NotAllowed => CursorIcon::NotAllowed,
        C::Grab => CursorIcon::Grab,
        C::Grabbing => CursorIcon::Grabbing,
        C::AllScroll => CursorIcon::AllScroll,
        C::ResizeHorizontal => CursorIcon::EwResize,
        C::ResizeNeSw => CursorIcon::NeswResize,
        C::ResizeNwSe => CursorIcon::NwseResize,
        C::ResizeVertical => CursorIcon::NsResize,
        C::ResizeEast => CursorIcon::EResize,
        C::ResizeSouthEast => CursorIcon::SeResize,
        C::ResizeSouth => CursorIcon::SResize,
        C::ResizeSouthWest => CursorIcon::SwResize,
        C::ResizeWest => CursorIcon::WResize,
        C::ResizeNorthWest => CursorIcon::NwResize,
        C::ResizeNorth => CursorIcon::NResize,
        C::ResizeNorthEast => CursorIcon::NeResize,
        C::ResizeColumn => CursorIcon::ColResize,
        C::ResizeRow => CursorIcon::RowResize,
        C::ZoomIn => CursorIcon::ZoomIn,
        C::ZoomOut => CursorIcon::ZoomOut,
    })
}
