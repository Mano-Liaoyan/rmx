//! Terminals built on `alacritty_terminal`, and what drawing and typing into them takes.

mod content;
mod cursor;
mod driver;
mod keyboard;
mod mouse;
#[cfg(target_os = "linux")]
mod pty;
mod scan;
mod terminal;
mod theme;
mod transport;

pub use content::RenderableCursor;
pub use cursor::{CursorRects, IntoRects};
pub use keyboard::encode_key;
pub use mouse::Wheel;
pub use terminal::{Output, Sink, Snapshot, Terminal};
pub use theme::Theme;
pub use transport::{
    CloseReason, SessionEvent, TransportReader, TransportWriter, WorkingDirectory,
};

pub use alacritty_terminal::event::{Event as TermEvent, WindowSize};
pub use alacritty_terminal::term::{ClipboardType, TermMode};
