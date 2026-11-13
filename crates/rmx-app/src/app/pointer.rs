//! The pointer: window edges, dividers, presses, motion and the wheel.

use super::*;

/// Which edge of a window the point `(x, y)` is on, if any: the corners take twice the band.
pub(super) fn resize_edge(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    band: f32,
) -> Option<ResizeDirection> {
    if x < 0. || y < 0. || x >= width || y >= height {
        return None;
    }
    let corner = band * 2.;
    let (left, right) = (x < band, x >= width - band);
    let (top, bottom) = (y < band, y >= height - band);
    let (near_left, near_right) = (x < corner, x >= width - corner);
    let (near_top, near_bottom) = (y < corner, y >= height - corner);
    Some(match () {
        () if (top && near_left) || (near_top && left) => ResizeDirection::NorthWest,
        () if (top && near_right) || (near_top && right) => ResizeDirection::NorthEast,
        () if (bottom && near_left) || (near_bottom && left) => ResizeDirection::SouthWest,
        () if (bottom && near_right) || (near_bottom && right) => ResizeDirection::SouthEast,
        () if top => ResizeDirection::North,
        () if bottom => ResizeDirection::South,
        () if left => ResizeDirection::West,
        () if right => ResizeDirection::East,
        () => return None,
    })
}

pub(super) fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
    }
}

impl At<'_> {
    pub(super) fn holding_pointer(&self) -> bool {
        self.win.captured.is_some() || self.win.drag.is_some()
    }

    pub(super) fn divider_at(&self, point: (f32, f32)) -> Option<Divider> {
        let band = (DIVIDER_BAND * self.scale()).max(self.spacing());
        let tab = self.win.workspace.active().filter(|tab| !tab.zoomed)?;
        let dividers = tab.layout.dividers(self.area(), self.spacing());
        dividers.into_iter().find(|divider| {
            let area = divider.area;
            match divider.axis {
                Axis::Row => {
                    (point.0 - divider.at).abs() <= band / 2.
                        && point.1 >= area.y
                        && point.1 < area.bottom()
                },
                Axis::Column => {
                    (point.1 - divider.at).abs() <= band / 2.
                        && point.0 >= area.x
                        && point.0 < area.right()
                },
            }
        })
    }

    pub(super) fn drag_divider(&mut self, drag: DividerDrag, point: (f32, f32)) {
        let (area, spacing, min) = (self.area(), self.spacing(), self.min_pane());
        let snap = DIVIDER_SNAP * self.scale();
        let Some(tab) = self.win.workspace.active_mut() else { return };
        let dividers = tab.layout.dividers(area, spacing);
        let Some(divider) = dividers.get(drag.index).copied() else { return };
        let Some((low, high)) = tab.layout.ratio_range(drag.index, divider.area, min, spacing)
        else {
            return;
        };

        let (at, start, length) = match divider.axis {
            Axis::Row => (point.0, divider.area.x, divider.area.w),
            Axis::Column => (point.1, divider.area.y, divider.area.h),
        };
        let wanted = at - drag.grab;
        let ratio = if (wanted - (start + length / 2.)).abs() <= snap {
            0.5
        } else {
            (wanted - spacing / 2. - start) / (length - spacing)
        };
        tab.layout.set_ratio(drag.index, ratio.clamp(low, high));
        self.relayout(false);
        self.show_divider_pill(Some(divider), point, true);
    }

    /// The ratio of the divider `divider` of the active tab, as it is now.
    pub(super) fn divider_ratio(&self, divider: &Divider) -> f32 {
        let spacing = self.spacing();
        let (start, length) = match divider.axis {
            Axis::Row => (divider.area.x, divider.area.w),
            Axis::Column => (divider.area.y, divider.area.h),
        };
        (divider.at - spacing / 2. - start) / (length - spacing).max(1.)
    }

    /// Set the divider `index` of the active tab to `ratio` as far as its panes allow, sliding.
    pub(super) fn set_divider(&mut self, index: usize, ratio: f32) {
        let (area, spacing, min) = (self.area(), self.spacing(), self.min_pane());
        let Some(tab) = self.win.workspace.active_mut() else { return };
        let dividers = tab.layout.dividers(area, spacing);
        let Some(divider) = dividers.get(index).copied() else { return };
        let (low, high) =
            tab.layout.ratio_range(index, divider.area, min, spacing).unwrap_or((0.5, 0.5));
        tab.layout.set_ratio(index, ratio.clamp(low, high));
        self.relayout(true);
    }

    /// Show the pill on `divider`, or hide it, in the interface's next pass if that changed it.
    pub(super) fn show_divider_pill(
        &mut self,
        divider: Option<Divider>,
        point: (f32, f32),
        dragged: bool,
    ) {
        let scale = self.scale();
        let band = divider.map(|divider| {
            let area = divider.area;
            let (rect, upright, along) = match divider.axis {
                Axis::Row => {
                    (Rect::new(divider.at - scale * 2., area.y, scale * 4., area.h), true, point.1)
                },
                Axis::Column => {
                    (Rect::new(area.x, divider.at - scale * 2., area.w, scale * 4.), false, point.0)
                },
            };
            let rect = egui::Rect::from_min_size(
                egui::pos2(rect.x / scale, rect.y / scale),
                egui::vec2(rect.w / scale, rect.h / scale),
            );
            (rect, upright, along / scale)
        });
        if self.win.shell.hover_divider(band, dragged, Instant::now()) {
            self.win.chrome.invalidate();
            self.request_frame();
        }
    }

    /// Let go of the divider being dragged, as a press of Escape or a lost focus can do.
    pub(super) fn cancel_divider_drag(&mut self) {
        if let Some(drag) = self.win.drag.take() {
            self.win.last_divider = None;
            self.set_divider(drag.index, drag.ratio);
            self.show_divider_pill(None, (0., 0.), false);
        }
    }

    /// The pane under `point`, and the point relative to what it shows.
    pub(super) fn pane_at(&self, point: (f32, f32)) -> Option<(PaneId, f32, f32)> {
        let position = (f64::from(point.0), f64::from(point.1));
        let bar = self.bar();
        self.win.rects.iter().find_map(|&(id, rect)| {
            let content = shell::terminal_rect(rect, bar);
            content.contains(position).then_some((id, point.0 - content.x, point.1 - content.y))
        })
    }

    pub(super) fn pointer_in(&self, id: PaneId, point: (f32, f32)) -> Option<(f32, f32)> {
        let &(_, rect) = self.win.rects.iter().find(|&&(pane, _)| pane == id)?;
        let content = shell::terminal_rect(rect, self.bar());
        Some((point.0 - content.x, point.1 - content.y))
    }

    /// A right press in the pane `id`. Returns whether that was all it did: it gives an unfocused
    /// pane the keyboard, and opens the pane's menu unless what is inside takes the mouse and Shift
    /// is not held. Control is left to the terminal, for extending a selection.
    pub(super) fn right_press(&mut self, id: PaneId, mods: ModifiersState) -> bool {
        if mods.control_key() {
            return false;
        }
        let focused = self.win.workspace.focused() == Some(id);
        if !focused {
            self.focus_pane(id);
        }
        let takes_mouse = self.pane(id).is_some_and(|pane| {
            pane.content.display().is_some()
                || pane.content.terminal().is_some_and(|terminal| {
                    let mode = terminal.mode();
                    mode.intersects(rmx_terminal::TermMode::MOUSE_MODE)
                        && !mode.contains(rmx_terminal::TermMode::VI)
                })
        });
        if takes_mouse && !mods.shift_key() {
            return !focused;
        }
        let scale = self.scale();
        let pos = egui::pos2(self.win.pointer.x as f32 / scale, self.win.pointer.y as f32 / scale);
        let items = self.pane_menu(id);
        self.win.shell.open_menu(pos, items);
        self.win.chrome.invalidate();
        self.request_frame();
        true
    }

    pub(super) fn pointer_moved(&mut self, position: PhysicalPosition<f64>, mods: ModifiersState) {
        self.win.pointer = position;
        let point = (position.x as f32, position.y as f32);

        if let Some(drag) = self.win.drag {
            self.drag_divider(drag, point);
            return;
        }

        if let Some(id) = self.win.captured {
            if let Some(at) = self.display_point(id) {
                if let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut()) {
                    display.pointer_moved(at.0, at.1);
                }
                return;
            }
            let Some((x, y)) = self.pointer_in(id, point) else { return };
            let Some(pane) = self.pane_mut(id) else { return };
            let Some(terminal) = pane.content.terminal_mut() else { return };
            if terminal.mouse_moved(x, y, mods) {
                pane.dirty = true;
                self.request_frame();
            }
            return;
        }

        let hovered = self.divider_at(point);
        self.show_divider_pill(hovered, point, false);
        let pane = self.pane_at(point).map(|(id, ..)| id);
        let cursor = match hovered.map(|divider| divider.axis) {
            Some(Axis::Row) => WindowCursor::Icon(CursorIcon::EwResize),
            Some(Axis::Column) => WindowCursor::Icon(CursorIcon::NsResize),
            None => match pane {
                Some(id) if self.pane(id).is_some_and(|pane| pane.content.display().is_some()) => {
                    self.display_cursor_at(id)
                },
                Some(_) => WindowCursor::Icon(CursorIcon::Text),
                None => WindowCursor::Icon(CursorIcon::Default),
            },
        };
        self.show_cursor(cursor);

        if let Some((id, x, y)) = self.pane_at(point) {
            if let Some(at) = self.display_point(id) {
                if at.2
                    && let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut())
                {
                    display.pointer_moved(at.0, at.1);
                }
                return;
            }
            if let Some(terminal) = self.pane_mut(id).and_then(|p| p.content.terminal_mut()) {
                terminal.mouse_moved(x, y, mods);
            }
        }
    }

    pub(super) fn mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        mods: ModifiersState,
    ) {
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        let pressed = state == ElementState::Pressed;

        if !pressed && self.win.swallow_release == Some(button) {
            self.win.swallow_release = None;
            return;
        }

        if button == MouseButton::Left && !pressed && self.win.drag.take().is_some() {
            let hovered = self.divider_at(point);
            self.show_divider_pill(hovered, point, false);
            return;
        }

        if button == MouseButton::Left && pressed {
            if let Some(divider) = self.divider_at(point) {
                let now = Instant::now();
                let index = divider.index;
                let last = self.win.last_divider.replace((index, now));
                if divider_double_click(last, index, now) {
                    self.win.last_divider = None;
                    self.win.swallow_release = Some(button);
                    self.set_divider(index, 0.5);
                    return;
                }
                let at = match divider.axis {
                    Axis::Row => point.0,
                    Axis::Column => point.1,
                };
                let ratio = self.divider_ratio(&divider);
                self.win.drag = Some(DividerDrag { index, grab: at - divider.at, ratio });
                self.show_divider_pill(Some(divider), point, true);
                return;
            }

            // Pressing another pane only gives it the keyboard.
            if let Some((id, ..)) = self.pane_at(point)
                && self.win.workspace.focused() != Some(id)
            {
                self.win.workspace.focus(id);
                self.sync_focus(Instant::now());
                self.redraw_all();
                return;
            }
        }

        // A right click is the pane's, not its program's, unless the program asks for the mouse.
        if button == MouseButton::Right
            && pressed
            && self.win.captured.is_none()
            && let Some((id, ..)) = self.pane_at(point)
            && self.right_press(id, mods)
        {
            self.win.swallow_release = Some(button);
            return;
        }

        let target = self.win.captured.or_else(|| self.pane_at(point).map(|(id, ..)| id));
        let Some(id) = target else { return };
        self.win.captured = if pressed { Some(id) } else { None };

        if self.pane(id).is_some_and(|pane| pane.content.display().is_some()) {
            if let (Some(at), Some(bit)) = (self.display_point(id), remote::button_bit(button))
                && (at.2 || !pressed)
                && let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut())
            {
                display.button(at.0, at.1, bit, pressed);
            }
            if !pressed
                && self.pane(id).and_then(|p| p.content.display()).is_some_and(Display::any_button)
            {
                self.win.captured = Some(id);
            }
            return;
        }

        let Some((x, y)) = self.pointer_in(id, point) else { return };
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(terminal) = pane.content.terminal_mut() else { return };
        let selected = terminal.mouse_button(x, y, button, state, mods);
        pane.dirty = true;
        if let Some(text) = selected {
            self.core.clipboard.store(ClipboardType::Selection, text);
        }
        if button == MouseButton::Middle && pressed {
            let text = self.core.clipboard.load(ClipboardType::Selection);
            if let Some(terminal) = self.pane(id).and_then(|pane| pane.content.terminal()) {
                terminal.paste(&text);
            }
        }
        self.request_frame();
    }

    pub(super) fn wheel(&mut self, delta: MouseScrollDelta, mods: ModifiersState) {
        let wheel = match delta {
            MouseScrollDelta::LineDelta(_, y) => Wheel::Lines(f64::from(y)),
            MouseScrollDelta::PixelDelta(position) => Wheel::Pixels(position.y),
        };
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        let Some((id, x, y)) = self.pane_at(point) else { return };
        if self.pane(id).is_some_and(|pane| pane.content.display().is_some()) {
            let delta = match delta {
                MouseScrollDelta::LineDelta(x, y) => {
                    WheelDelta::Lines([f64::from(x), f64::from(y)])
                },
                MouseScrollDelta::PixelDelta(position) => {
                    WheelDelta::Pixels([position.x, position.y])
                },
            };
            let Some((layout, _)) = self.display_layout(id) else { return };
            let Some(at) = self.display_point(id) else { return };
            let Some(pane) = self.pane_mut(id) else { return };
            let Some(display) = pane.content.display_mut() else { return };
            // Alt turns the wheel into panning a 1:1 view.
            if mods.alt_key() {
                if display.pan(delta, &layout) {
                    pane.dirty = true;
                    self.request_frame();
                }
            } else if at.2 {
                display.wheel(delta, at.0, at.1);
            }
            return;
        }
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(terminal) = pane.content.terminal_mut() else { return };
        if terminal.mouse_wheel(wheel, x, y, mods) {
            pane.dirty = true;
            self.request_frame();
        }
    }
}
