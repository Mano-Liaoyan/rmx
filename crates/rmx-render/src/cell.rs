//! A terminal cell resolved to what is drawn for it.

use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::{Flags, Hyperlink};

use crate::Rgb;

/// Cell ready for rendering.
#[derive(Clone, Debug)]
pub struct RenderableCell {
    pub character: char,
    pub point: Point<usize>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bg_alpha: f32,
    pub underline: Rgb,
    pub flags: Flags,
    pub extra: Option<Box<RenderableCellExtra>>,
}

/// Rarely present fields of a [`RenderableCell`], kept out of it to keep the cell small.
#[derive(Clone, Debug)]
pub struct RenderableCellExtra {
    pub zerowidth: Option<Vec<char>>,
    pub hyperlink: Option<Hyperlink>,
}
