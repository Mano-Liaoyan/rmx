//! GPU rendering of terminals, derived from Alacritty's renderer.
//!
//! Everything is drawn into a [`Frame`] of the window, so panes can animate without the terminal
//! under them changing size.

#[allow(clippy::all, unsafe_op_in_unsafe_fn, missing_docs)]
pub mod gl {
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

mod cell;
mod color;
pub mod font;
pub mod probe;
pub mod renderer;
mod size;

pub use cell::{RenderableCell, RenderableCellExtra};
pub use color::Rgb;
pub use renderer::remote::{
    RemoteLayout, RemotePointer, RemoteTexture, ScaleMode, UploadPath, UploadStatus,
};
pub use renderer::{Frame, GlyphCache, RasterizedGlyphs, Renderer};
pub use size::SizeInfo;

/// Which GL renderer to use instead of picking one from the GPU's capabilities.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RendererPreference {
    /// OpenGL 3.3 renderer.
    Glsl3,
    /// GLES 2 renderer, with optional extensions like dual source blending.
    Gles2,
    /// Pure GLES 2 renderer.
    Gles2Pure,
}
