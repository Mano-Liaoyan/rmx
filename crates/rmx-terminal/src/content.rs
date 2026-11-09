//! Resolving the cells of a terminal to what is drawn for them.

use std::mem;
use std::num::NonZeroU32;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Indexed;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{self, RenderableContent as TerminalContent, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use rmx_render::{RenderableCell, RenderableCellExtra, Rgb};

use crate::theme::{DIM_FACTOR, Theme};

/// Minimum contrast between a fixed cursor color and the cell's background.
const MIN_CURSOR_CONTRAST: f64 = 1.5;

/// An iterator over the non-empty cells of a terminal.
pub struct RenderableContent<'a> {
    terminal: TerminalContent<'a>,
    theme: &'a Theme,
    cursor: RenderableCursor,
    cursor_shape: CursorShape,
    cursor_point: Point<usize>,
}

impl<'a> RenderableContent<'a> {
    pub fn new<T: EventListener>(term: &'a Term<T>, theme: &'a Theme) -> Self {
        let terminal = term.renderable_content();
        let cursor_shape = match terminal.cursor.shape {
            CursorShape::Hidden => CursorShape::Hidden,
            _ if !term.is_focused => CursorShape::HollowBlock,
            shape => shape,
        };
        let cursor_point = term::point_to_viewport(terminal.display_offset, terminal.cursor.point)
            .unwrap_or_default();

        Self { terminal, theme, cursor: RenderableCursor::hidden(), cursor_shape, cursor_point }
    }

    /// The cursor, known once all cells have been iterated over.
    pub fn cursor(&self) -> RenderableCursor {
        self.cursor
    }

    fn color(&self, index: usize) -> Rgb {
        self.terminal.colors[index].map(Rgb).unwrap_or(self.theme[index])
    }

    fn fg(&self, fg: Color, flags: Flags) -> Rgb {
        match fg {
            Color::Spec(rgb) if flags.contains(Flags::DIM) => Rgb(rgb) * DIM_FACTOR,
            Color::Spec(rgb) => Rgb(rgb),
            Color::Named(named) => match flags & Flags::DIM_BOLD {
                Flags::BOLD => self.color(named.to_bright() as usize),
                Flags::DIM => self.color(named.to_dim() as usize),
                _ => self.color(named as usize),
            },
            Color::Indexed(index) => self.color(match (flags & Flags::DIM_BOLD, index) {
                (Flags::BOLD, 0..=7) => index as usize + 8,
                _ => index as usize,
            }),
        }
    }

    fn bg(&self, bg: Color) -> Rgb {
        match bg {
            Color::Spec(rgb) => Rgb(rgb),
            Color::Named(named) => self.color(named as usize),
            Color::Indexed(index) => self.color(index as usize),
        }
    }

    fn cell(&self, cell: Indexed<&Cell>) -> RenderableCell {
        let mut fg = self.fg(cell.fg, cell.flags);
        let mut bg = self.bg(cell.bg);

        let mut bg_alpha = if cell.flags.contains(Flags::INVERSE) {
            mem::swap(&mut fg, &mut bg);
            1.
        } else if cell.bg == Color::Named(NamedColor::Background) {
            0.
        } else {
            1.
        };

        let selected = self.terminal.selection.is_some_and(|selection| {
            selection.contains_cell(&cell, self.terminal.cursor.point, self.cursor_shape)
        });
        if selected {
            fg = self.theme.selection_fg;
            bg = self.theme.selection_bg;
            bg_alpha = 1.;
        }

        let point = term::point_to_viewport(self.terminal.display_offset, cell.point).unwrap();
        let underline = cell.underline_color().map_or(fg, |color| self.fg(color, cell.flags));
        let zerowidth = cell.zerowidth();
        let hyperlink = cell.hyperlink();
        let extra = (zerowidth.is_some() || hyperlink.is_some()).then(|| {
            Box::new(RenderableCellExtra {
                zerowidth: zerowidth.map(|zerowidth| zerowidth.to_vec()),
                hyperlink,
            })
        });

        RenderableCell {
            character: cell.c,
            point,
            fg,
            bg,
            bg_alpha,
            underline,
            flags: cell.flags,
            extra,
        }
    }

    fn cursor_for(&self, cell: &RenderableCell) -> RenderableCursor {
        let cursor = self.terminal.colors[NamedColor::Cursor];
        let (mut color, mut text) = match cursor {
            Some(rgb) => (Rgb(rgb), cell.bg),
            // Without a set color the cursor inverts the cell under it.
            None => (cell.fg, cell.bg),
        };

        if color.contrast(*cell.bg) < MIN_CURSOR_CONTRAST {
            color = self.theme.foreground();
            text = self.theme.background();
        }

        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        RenderableCursor {
            shape: self.cursor_shape,
            cursor_color: color,
            text_color: text,
            width: NonZeroU32::new(if wide { 2 } else { 1 }).unwrap(),
            point: self.cursor_point,
        }
    }
}

impl Iterator for RenderableContent<'_> {
    type Item = RenderableCell;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let cell = self.terminal.display_iter.next()?;
            let mut cell = self.cell(cell);

            if cell.point == self.cursor_point {
                self.cursor = self.cursor_for(&cell);
                if self.cursor.shape == CursorShape::Block {
                    cell.fg = self.cursor.text_color;
                    cell.bg = self.cursor.cursor_color;
                    cell.bg_alpha = 1.;
                }
                return Some(cell);
            }

            let empty = cell.bg_alpha == 0.
                && cell.character == ' '
                && cell.extra.is_none()
                && !cell.flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT);
            if !empty && !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                return Some(cell);
            }
        }
    }
}

/// How the terminal cursor is drawn.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct RenderableCursor {
    shape: CursorShape,
    cursor_color: Rgb,
    text_color: Rgb,
    width: NonZeroU32,
    point: Point<usize>,
}

impl Default for RenderableCursor {
    fn default() -> Self {
        Self::hidden()
    }
}

impl RenderableCursor {
    fn hidden() -> Self {
        Self {
            shape: CursorShape::Hidden,
            cursor_color: Rgb::default(),
            text_color: Rgb::default(),
            width: NonZeroU32::new(1).unwrap(),
            point: Point::default(),
        }
    }

    pub fn color(&self) -> Rgb {
        self.cursor_color
    }

    pub fn shape(&self) -> CursorShape {
        self.shape
    }

    pub fn width(&self) -> NonZeroU32 {
        self.width
    }

    pub fn point(&self) -> Point<usize> {
        self.point
    }
}
