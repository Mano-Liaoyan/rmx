//! What a remote desktop session and the interface showing it exchange.
//!
//! Protocol crates decode on their own tasks and never touch the GPU. The interface uploads
//! updates to a texture, so the channels are bounded: a slow reader applies backpressure, and
//! a session that falls behind should coalesce updates rather than queue them.

pub mod keymap;
pub mod outbox;

use tokio::sync::mpsc;

/// A rectangle of new pixels, as tightly packed BGRA8 rows, top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameUpdate {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// A pointer shape the remote desktop asks for, to show as the local cursor over its picture.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// The point of the image that is the pointer's position.
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// Tightly packed RGBA8 rows, top to bottom, alpha not premultiplied.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for CursorImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hotspot", &(self.hotspot_x, self.hotspot_y))
            .finish_non_exhaustive()
    }
}

/// How the local cursor should look over the remote picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteCursor {
    /// The system's ordinary arrow.
    Default,
    /// No cursor at all, as when the remote hides it while typing.
    Hidden,
    Image(std::sync::Arc<CursorImage>),
}

/// Something a session reports to the interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayEvent {
    /// The session is up, or the remote desktop changed size. All pixels are unknown until
    /// updates cover them.
    Resized {
        width: u32,
        height: u32,
    },
    Update(FrameUpdate),
    /// The remote clipboard now holds this text.
    Clipboard(String),
    /// The remote pointer changed shape. Sessions that leave the pointer in the picture never
    /// send this.
    Cursor(RemoteCursor),
    Bell,
    /// The session ended, with the reason if it failed.
    Closed(Option<String>),
}

/// A key, identified both ways since protocols differ in which they need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// Linux evdev code of the physical key.
    pub evdev: u32,
    /// X11 keysym of what the key produces, when known.
    pub keysym: Option<u32>,
}

/// Mouse buttons held, as a bit mask.
pub mod buttons {
    pub const LEFT: u8 = 1 << 0;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
}

/// Something the interface sends to a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key {
        key: Key,
        pressed: bool,
    },
    /// The pointer is at `x`, `y` in remote pixels with these `buttons` held.
    Pointer {
        x: u32,
        y: u32,
        buttons: u8,
    },
    /// Wheel notches at `x`, `y`; positive scrolls up or right.
    Wheel {
        x: u32,
        y: u32,
        vertical: i16,
        horizontal: i16,
    },
    /// The local clipboard now holds this text.
    Clipboard(String),
    /// Ask for the remote desktop to take this size, where the protocol can.
    Resize {
        width: u32,
        height: u32,
    },
    /// End the session.
    Disconnect,
}

/// The interface's end of a running session.
#[derive(Debug)]
pub struct DisplayHandle {
    pub events: mpsc::Receiver<DisplayEvent>,
    pub input: mpsc::Sender<InputEvent>,
}
