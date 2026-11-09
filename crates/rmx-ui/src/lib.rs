//! The interface: design tokens on egui, and the widgets of the window around the panes.
//!
//! Widgets take plain view-models, draw immediately, and return a [`UiAction`] for the app to
//! carry out. Nothing here knows about databases or protocols.
//!
//! [`Chrome`] owns the egui context. It runs egui only when input, an animation or the app's own
//! state asks for it, and otherwise replays the last frame's triangles, so terminal output never
//! costs an egui pass. See [`chrome`] for the frame sequence and what it does to GL state.

pub mod action;
pub mod chrome;
mod draw;
mod float;
pub mod fonts;
pub mod icons;
pub mod input;
mod local_time;
pub mod snippet;
pub mod theme;
pub mod virtual_list;
pub mod widgets;

pub use egui;

pub use action::{FileSide, FilesAction, NodeId, PaneId, Protocol, Sensitive, TabId, UiAction};
pub use chrome::{Chrome, ChromeCore, ChromeOutput, FrameStats, PixelRect};
pub use local_time::{LocalTime, local_time};
pub use theme::{Palette, ThemeMode};
