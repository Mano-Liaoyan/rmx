//! Pointer input of a terminal: selection, scrolling, and reporting to the program in it.

use std::time::{Duration, Instant};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{self, TermMode};
use winit::event::{ElementState, MouseButton};
use winit::keyboard::ModifiersState;

use crate::terminal::Terminal;

/// Longest time between the presses of a double or triple click.
const CLICK_THRESHOLD: Duration = Duration::from_millis(400);
/// Lines scrolled per wheel notch.
const LINES_PER_NOTCH: f64 = 3.;

/// How much the wheel moved.
#[derive(Clone, Copy, Debug)]
pub enum Wheel {
    Lines(f64),
    Pixels(f64),
}

/// What a terminal remembers of the pointer between events.
#[derive(Debug, Default)]
pub struct MouseState {
    last_click: Option<(Instant, Point<usize>)>,
    clicks: u8,
    selecting: bool,
    pressed: Option<u8>,
    last_cell: Option<Point<usize>>,
    /// Pixels scrolled that did not add up to a line yet.
    scroll_rest: f64,
}

fn button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

fn mods_code(mods: ModifiersState) -> u8 {
    let mut code = 0;
    if mods.shift_key() {
        code += 4;
    }
    if mods.alt_key() {
        code += 8;
    }
    if mods.control_key() {
        code += 16;
    }
    code
}

/// The bytes reporting a mouse event at `point`, or nothing where the encoding cannot express it.
fn encode_report(mode: TermMode, point: Point<usize>, code: u8, pressed: bool) -> Vec<u8> {
    let (column, line) = (point.column.0, point.line);

    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        return format!("\x1b[<{code};{};{}{end}", column + 1, line + 1).into_bytes();
    }

    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let limit = if utf8 { 2015 } else { 223 };
    if line >= limit || column >= limit {
        return Vec::new();
    }

    // Plain encodings cannot tell which button was released.
    let code = if pressed { code } else { 3 + (code & 0b1_1100) };
    let mut bytes = vec![b'\x1b', b'[', b'M', 32 + code];
    for position in [column, line] {
        let value = 33 + position;
        if utf8 && position >= 95 {
            bytes.push(0xc0 + (value / 64) as u8);
            bytes.push(0x80 + (value & 63) as u8);
        } else {
            bytes.push(value as u8);
        }
    }
    bytes
}

impl Terminal {
    fn cell_at(&self, x: f32, y: f32) -> (Point<usize>, Side) {
        let size = self.size();
        let (columns, lines) = (size.columns_count(), size.lines_count());
        let (cx, cy) = (x - size.padding_x(), y - size.padding_y());

        let column = ((cx / size.cell_width()).max(0.) as usize).min(columns - 1);
        let line = ((cy / size.cell_height()).max(0.) as usize).min(lines - 1);
        let in_cell = cx.max(0.) % size.cell_width();
        let side = if in_cell > size.cell_width() / 2. { Side::Right } else { Side::Left };

        (Point::new(line, Column(column)), side)
    }

    fn grid_point(&self, point: Point<usize>) -> Point<Line> {
        term::viewport_to_point(self.term().grid().display_offset(), point)
    }

    fn reports_mouse(&self, mods: ModifiersState) -> bool {
        self.mode().intersects(TermMode::MOUSE_MODE) && !mods.shift_key()
    }

    /// A button changed state with the pointer at `x`, `y` in the terminal.
    ///
    /// Returns the text of a selection that was just finished.
    pub fn mouse_button(
        &mut self,
        x: f32,
        y: f32,
        button: MouseButton,
        state: ElementState,
        mods: ModifiersState,
    ) -> Option<String> {
        let (point, side) = self.cell_at(x, y);
        let pressed = state == ElementState::Pressed;

        if self.reports_mouse(mods) {
            let code = button_code(button)?;
            self.mouse.pressed = pressed.then_some(code);
            let report = encode_report(self.mode(), point, code + mods_code(mods), pressed);
            self.write(report);
            return None;
        }

        if button != MouseButton::Left {
            return None;
        }

        if pressed {
            self.start_selection(point, side, mods);
            None
        } else if std::mem::take(&mut self.mouse.selecting) {
            self.term().selection_to_string()
        } else {
            None
        }
    }

    fn start_selection(&mut self, point: Point<usize>, side: Side, mods: ModifiersState) {
        let now = Instant::now();
        let again = self.mouse.last_click.is_some_and(|(at, last)| {
            last == point && now.saturating_duration_since(at) < CLICK_THRESHOLD
        });
        self.mouse.clicks = if again { self.mouse.clicks % 3 + 1 } else { 1 };
        self.mouse.last_click = Some((now, point));
        self.mouse.selecting = true;

        let ty = match self.mouse.clicks {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ if mods.control_key() => SelectionType::Block,
            _ => SelectionType::Simple,
        };

        let grid = self.grid_point(point);
        let mut term = self.term();
        // A single click only places the anchor; nothing is selected until the pointer moves.
        term.selection = (ty != SelectionType::Simple && ty != SelectionType::Block)
            .then(|| Selection::new(ty, grid, side));
        drop(term);

        self.mouse.last_cell = Some(point);
        self.anchor = Some((ty, grid, side));
    }

    /// The pointer moved to `x`, `y` in the terminal.
    pub fn mouse_moved(&mut self, x: f32, y: f32, mods: ModifiersState) -> bool {
        let (point, side) = self.cell_at(x, y);
        let changed = self.mouse.last_cell != Some(point);

        if self.reports_mouse(mods) {
            if !changed {
                return false;
            }
            self.mouse.last_cell = Some(point);

            let mode = self.mode();
            let report = match self.mouse.pressed {
                Some(code) if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) => {
                    Some(code + 32)
                },
                None if mode.contains(TermMode::MOUSE_MOTION) => Some(35),
                _ => None,
            };
            if let Some(code) = report {
                self.write(encode_report(mode, point, code + mods_code(mods), true));
            }
            return false;
        }

        if !self.mouse.selecting {
            return false;
        }

        self.mouse.last_cell = Some(point);
        let grid = self.grid_point(point);
        let anchor = self.anchor;
        let mut term = self.term();
        match (&mut term.selection, anchor) {
            (Some(selection), _) => selection.update(grid, side),
            (selection @ None, Some((ty, start, start_side))) if changed => {
                let mut new = Selection::new(ty, start, start_side);
                new.update(grid, side);
                *selection = Some(new);
            },
            _ => return false,
        }
        true
    }

    /// The wheel turned over the terminal. Returns whether what is shown changed.
    pub fn mouse_wheel(&mut self, wheel: Wheel, x: f32, y: f32, mods: ModifiersState) -> bool {
        let cell_height = f64::from(self.size().cell_height());
        let pixels = match wheel {
            Wheel::Lines(lines) => lines * LINES_PER_NOTCH * cell_height,
            Wheel::Pixels(pixels) => pixels,
        };

        self.mouse.scroll_rest += pixels;
        let lines = (self.mouse.scroll_rest / cell_height).trunc();
        self.mouse.scroll_rest -= lines * cell_height;
        let lines = lines as i32;
        if lines == 0 {
            return false;
        }

        let mode = self.mode();
        if self.reports_mouse(mods) {
            let (point, _) = self.cell_at(x, y);
            let code = if lines > 0 { 64 } else { 65 } + mods_code(mods);
            for _ in 0..lines.unsigned_abs() {
                self.write(encode_report(mode, point, code, true));
            }
            return false;
        }

        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && !mods.shift_key() {
            let cursor = if mode.contains(TermMode::APP_CURSOR) { b'O' } else { b'[' };
            let key = if lines > 0 { b'A' } else { b'B' };
            let mut bytes = Vec::with_capacity(lines.unsigned_abs() as usize * 3);
            for _ in 0..lines.unsigned_abs() {
                bytes.extend_from_slice(&[0x1b, cursor, key]);
            }
            self.write(bytes);
            return false;
        }

        self.term().scroll_display(Scroll::Delta(lines));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: usize, column: usize) -> Point<usize> {
        Point::new(line, Column(column))
    }

    #[test]
    fn sgr_reports_press_and_release_one_based() {
        let mode = TermMode::SGR_MOUSE;
        assert_eq!(encode_report(mode, at(4, 9), 0, true), b"\x1b[<0;10;5M");
        assert_eq!(encode_report(mode, at(4, 9), 0, false), b"\x1b[<0;10;5m");
    }

    #[test]
    fn plain_reports_offset_by_32_and_release_as_button_3() {
        let mode = TermMode::empty();
        assert_eq!(encode_report(mode, at(0, 0), 0, true), b"\x1b[M\x20\x21\x21");
        assert_eq!(encode_report(mode, at(0, 0), 0, false), b"\x1b[M\x23\x21\x21");
    }

    #[test]
    fn plain_reports_cannot_reach_far_cells() {
        assert!(encode_report(TermMode::empty(), at(0, 223), 0, true).is_empty());
    }

    #[test]
    fn utf8_reports_widen_large_positions() {
        let bytes = encode_report(TermMode::UTF8_MOUSE, at(0, 100), 0, true);
        assert_eq!(&bytes[4..6], &[0xc2, 0x85]);
    }

    #[test]
    fn modifiers_add_to_the_button() {
        let code = mods_code(ModifiersState::CONTROL | ModifiersState::ALT);
        assert_eq!(code, 24);
    }
}
