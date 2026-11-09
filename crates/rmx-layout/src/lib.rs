//! Pane layout and motion: pure logic, no window or GPU.

pub mod anim;
pub mod dnd;
pub mod motion;
pub mod region;
pub mod session;
pub mod split;
pub mod workspace;

mod rect;

pub use rect::Rect;

/// Identifies a pane for as long as the application runs.
pub type PaneId = u64;
