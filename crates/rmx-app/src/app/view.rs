//! A window's geometry, focus and cursor, its chrome and IME, and drawing it.

use super::*;

impl At<'_> {
    pub(super) fn scale(&self) -> f32 {
        self.win.gfx.scale()
    }

    pub(super) fn spacing(&self) -> f32 {
        (SPACING * self.scale()).round()
    }

    /// The width of the sidebar, in pixels, none while it is closed.
    pub(super) fn sidebar_width(&self) -> f32 {
        if self.win.shell.sidebar_open {
            (self.win.shell.sidebar_width * self.scale()).round()
        } else {
            0.
        }
    }

    /// The height of the strip of tabs, in pixels.
    pub(super) fn strip_height(&self) -> f32 {
        (STRIP_HEIGHT * self.scale()).round()
    }

    /// The height of a pane's bar, in pixels.
    pub(super) fn bar(&self) -> f32 {
        (PANE_BAR_HEIGHT * self.scale()).round()
    }

    /// The space the panes tile, in the window, right of the sidebar and below the strip.
    pub(super) fn area(&self) -> Rect {
        let (width, height) = self.win.gfx.window_size();
        let (margin, left, top) = (self.spacing(), self.sidebar_width(), self.strip_height());
        Rect::new(
            left + margin,
            top + margin,
            (width - left - 2. * margin).max(1.),
            (height - top - 2. * margin).max(1.),
        )
    }

    pub(super) fn min_pane(&self) -> (f32, f32) {
        let (cell_w, cell_h) = self.win.gfx.cell();
        (MIN_CELLS.0 * cell_w, MIN_CELLS.1 * cell_h + self.bar())
    }

    /// The part of a pane the terminal draws in.
    pub(super) fn terminal_rect(&self, panel: Rect) -> Rect {
        shell::terminal_rect(panel, self.bar())
    }

    pub(super) fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.core.panes.iter().find(|pane| pane.id == id)
    }

    pub(super) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.core.panes.iter_mut().find(|pane| pane.id == id)
    }

    pub(super) fn focused_mut(&mut self) -> Option<&mut Pane> {
        let id = self.win.workspace.focused()?;
        self.pane_mut(id)
    }

    /// Ask for a frame, when one can be drawn.
    pub(super) fn request_frame(&mut self) {
        if self.win.pacer.mark_dirty() {
            self.win.gfx.window.request_redraw();
        }
    }

    /// Everything has to be redrawn.
    pub(super) fn redraw_all(&mut self) {
        self.win.full = true;
        self.request_frame();
    }

    /// Show this icon, which winit passes to the compositor even when it is the same.
    pub(super) fn set_cursor(&mut self, icon: CursorIcon) {
        self.show_cursor(WindowCursor::Icon(icon));
    }

    /// Show this cursor, telling the window only if it is not showing already.
    pub(super) fn show_cursor(&mut self, cursor: WindowCursor) {
        if self.win.cursor == cursor {
            return;
        }
        let window = &self.win.gfx.window;
        match &cursor {
            WindowCursor::Icon(icon) => window.set_cursor(*icon),
            WindowCursor::Custom(custom) => window.set_cursor(custom.clone()),
            WindowCursor::Hidden => {},
        }
        let hidden = cursor == WindowCursor::Hidden;
        if hidden != (self.win.cursor == WindowCursor::Hidden) {
            window.set_cursor_visible(!hidden);
        }
        self.win.cursor = cursor;
    }

    /// The remote pointer of `id` took a new shape.
    pub(super) fn display_cursor(
        &mut self,
        id: PaneId,
        generation: u64,
        cursor: crate::remote::DisplayCursor,
    ) {
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let Some(display) = self.pane_mut(id).and_then(|pane| pane.content.display_mut()) else {
            return;
        };
        display.cursor = cursor;
        // Shown at once if the pointer is over the picture, even holding still.
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        let over = self.win.captured.or_else(|| self.pane_at(point).map(|(pane, ..)| pane));
        if over == Some(id) && !self.win.chrome.pointer_on_interface() {
            let cursor = self.display_cursor_at(id);
            self.show_cursor(cursor);
        }
    }

    /// The cursor to show over the remote desktop of `id`: its own pointer over the picture,
    /// the arrow over the bars around it.
    pub(super) fn display_cursor_at(&self, id: PaneId) -> WindowCursor {
        let inside = self.display_point(id).is_some_and(|at| at.2);
        match self.pane(id).and_then(|pane| pane.content.display()) {
            Some(display) if inside => WindowCursor::from(&display.cursor),
            _ => WindowCursor::Icon(CursorIcon::Default),
        }
    }

    /// What goes in the state file about the window.
    pub(super) fn layout(&self) -> Layout {
        let size =
            self.win.gfx.window.inner_size().to_logical::<f64>(self.win.gfx.window.scale_factor());
        Layout {
            width: size.width,
            height: size.height,
            maximized: self.win.maximized,
            sidebar_open: self.win.shell.sidebar_open,
            theme: self.core.theme,
            panels: self.core.ui.panels,
        }
    }

    /// Tell the vault's thread the user is still there. Costs nothing per event.
    pub(super) fn touch(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.core.touched) >= TOUCH_EVERY {
            self.core.touched = now;
            self.core.vault.send(Request::Touch);
        }
    }

    /// Something about a pane's connection changed: its overlay and footer, the tree's dot.
    pub(super) fn link_changed(&mut self, id: PaneId) {
        let Some(pane) = self.pane_mut(id) else { return };
        pane.dirty = true;
        let node = pane.remote.as_ref().map(|remote| remote.node);
        if let Some(node) = node {
            let dot = shell::session_dot(&self.core.panes, node);
            self.core.ui.tree.set_state(NodeId(node), dot);
            self.core.broadcast = true;
        }
        self.sync_chrome_area();
        self.win.chrome.invalidate();
        self.request_frame();
    }

    /// Which edge of the window `point` resizes, if it can be resized.
    pub(super) fn resize_edge_at(&self, point: (f32, f32)) -> Option<ResizeDirection> {
        if self.win.maximized {
            return None;
        }
        let (width, height) = self.win.gfx.window_size();
        resize_edge(point.0, point.1, width, height, RESIZE_BAND * self.scale())
    }

    /// Tell the chrome where the panes are, and which parts of them are its own.
    pub(super) fn sync_chrome_area(&mut self) {
        let scale = self.scale();
        if self.core.ui.locked() || self.win.rects.is_empty() {
            // Everything is the interface's: the panes are out of sight, or there are none.
            self.win.chrome.set_pane_area(None);
            self.win.chrome.set_pane_holes([]);
            return;
        }
        let area = self.area();
        self.win.chrome.set_pane_area(Some(egui::Rect::from_min_size(
            egui::pos2(area.x / scale, area.y / scale),
            egui::vec2(area.w / scale, area.h / scale),
        )));

        let bar = self.bar();
        let panes = &self.core.panes;
        let holes = self.win.rects.iter().flat_map(|&(id, rect)| {
            let content = shell::content_points(rect, bar, scale);
            let pane = panes.iter().find(|pane| pane.id == id);
            std::iter::once(shell::bar_rect(rect, scale))
                .chain(pane.into_iter().flat_map(move |pane| pane.interface_rects(content)))
        });
        self.win.chrome.set_pane_holes(holes);
    }

    /// The tree or the window changed: put every shown pane where it now goes.
    pub(super) fn relayout(&mut self, animate: bool) {
        let now = Instant::now();
        let (area, spacing) = (self.area(), self.spacing());
        let shown = std::mem::take(&mut self.win.rects);
        if !self.core.ui.locked() {
            self.win.rects = self.win.workspace.rects(area, spacing);
        }

        for index in 0..self.win.rects.len() {
            let (id, rect) = self.win.rects[index];
            let size = self.win.gfx.size_for(self.terminal_rect(rect));
            // A pane out of sight has not been keeping its snapshot up to date.
            let appeared = !shown.iter().any(|&(pane, _)| pane == id);
            let settled = self.terminal_rect(rect);
            if let Some(pane) = self.pane_mut(id) {
                pane.content.resize(size);
                pane.dirty |= appeared;
                if let Some(display) = pane.content.display_mut() {
                    display.pane_resized(settled.w as u32, settled.h as u32, now);
                }
            }
        }

        self.win.motion.place(&self.win.rects, now, animate);
        self.core.session_changed();
        self.sync_focus(now);
        self.sync_chrome_area();
        self.win.chrome.invalidate();
        self.redraw_all();
    }

    /// Show who has the keyboard, on the borders and in the terminals.
    pub(super) fn sync_focus(&mut self, now: Instant) {
        let focused = self.win.workspace.focused();
        if let Some(focused) = focused {
            self.win.motion.focus(focused, now);
        }
        let (window, has_focus) = (self.win.key, self.win.has_focus);
        for pane in self.core.panes.iter_mut().filter(|pane| pane.window == window) {
            if pane.content.set_focused(has_focus && Some(pane.id) == focused) {
                pane.dirty = true;
            }
        }

        let title = focused.and_then(|id| self.pane(id)).and_then(|pane| pane.title.as_deref());
        self.win.gfx.window.set_title(title.unwrap_or("rmx"));
        self.win.chrome.invalidate();

        let display =
            focused.filter(|&id| self.pane(id).is_some_and(|p| p.content.display().is_some()));
        if display != self.win.display_focus {
            self.win.display_focus = display;
            if let Some(id) = display {
                self.offer_clipboard(id);
            }
        }
        self.sync_ime_allowed();
    }

    /// Give the keyboard to the remote desktop or take it away: while it has it, the input method
    /// on this side must not be in the way of the one on the remote desktop.
    pub(super) fn sync_ime_allowed(&mut self) {
        let keys_to_interface = self.win.chrome.ctx().egui_wants_keyboard_input();
        let allowed = self.win.display_focus.is_none() || keys_to_interface;
        if allowed != self.win.ime_allowed {
            self.win.ime_allowed = allowed;
            self.win.gfx.window.set_ime_allowed(allowed);
        }
    }

    /// Run egui if it needs to, and do what comes of it. Returns whether what it draws changed.
    pub(super) fn update_chrome(&mut self, now: Instant) -> bool {
        let (width, height) = self.win.gfx.window_size();
        let size = [width as u32, height as u32];
        let ppp = self.scale();

        let mut changed = false;
        let mut ran = false;
        // Twice at most: what the first pass made happen is shown by the second, in this frame.
        for _ in 0..2 {
            let view = ShellView {
                window: self.win.key,
                workspace: &self.win.workspace,
                panes: &self.core.panes,
                rects: &self.win.rects,
                motion: &self.win.motion,
                now,
                area: self.area(),
                spacing: self.spacing(),
                bar: self.bar(),
                min_pane: self.min_pane(),
                maximized: self.win.maximized,
            };
            let (shell, common) = (&mut self.win.shell, &mut self.core.ui);
            if !self.win.chrome.update(now, size, ppp, |ui| shell.show(ui, &view, common)) {
                break;
            }
            ran = true;
            changed |= self.win.chrome.last_pass_changed();
            self.apply_chrome_output();
            self.sync_ime_allowed();

            let acted = !self.win.shell.actions.is_empty() || self.win.shell.dropped.is_some();
            for action in std::mem::take(&mut self.win.shell.actions) {
                self.win.exit |= !self.ui_action(action);
            }
            if let Some(drop) = self.win.shell.dropped.take() {
                self.drop_pane(drop);
            }
            if !acted {
                break;
            }
            self.win.chrome.invalidate();
        }

        #[cfg(feature = "bench")]
        {
            self.win.ran_ui = ran;
        }
        let _ = ran;
        changed
    }

    /// What egui asked of the platform: a cursor, the clipboard, where the input method goes.
    pub(super) fn apply_chrome_output(&mut self) {
        let output = self.win.chrome.take_output();
        if let Some(text) = output.copied {
            self.core.clipboard.store(ClipboardType::Clipboard, text);
        }
        let point = (self.win.pointer.x as f32, self.win.pointer.y as f32);
        if self.win.chrome.pointer_on_interface() && self.resize_edge_at(point).is_none() {
            self.set_cursor(rmx_ui::input::cursor(output.cursor).unwrap_or(CursorIcon::Default));
        }

        if let Some(rect) = output.ime_rect {
            let scale = self.scale();
            self.set_ime_area((
                (rect.left() * scale) as i32,
                (rect.top() * scale) as i32,
                (rect.width() * scale) as u32,
                (rect.height() * scale) as u32,
            ));
        }
    }

    /// Tell the input method where text goes. Only when that changes: on Wayland each call comes
    /// back as an input method event, and so as another egui pass.
    pub(super) fn set_ime_area(&mut self, area: (i32, i32, u32, u32)) {
        if self.win.ime_area != Some(area) {
            self.win.ime_area = Some(area);
            let (x, y, width, height) = area;
            self.win
                .gfx
                .window
                .set_ime_cursor_area(PhysicalPosition::new(x, y), PhysicalSize::new(width, height));
        }
    }

    /// Tell the input method where the terminal cursor is, when it has moved.
    pub(super) fn sync_ime_area(&mut self) {
        if self.win.chrome.ctx().egui_wants_keyboard_input() {
            return;
        }
        let Some(id) = self.win.workspace.focused() else { return };
        let Some(&(_, panel)) = self.win.rects.iter().find(|&&(pane, _)| pane == id) else {
            return;
        };
        let rect = self.terminal_rect(panel);
        let Some(terminal) = self.pane(id).and_then(|pane| pane.content.terminal()) else { return };
        let point = terminal.last_snapshot().cursor.point();
        let area = self.win.gfx.cell_rect(rect, point.line, point.column.0);
        self.set_ime_area(area);
    }

    /// The input method has new unfinished text, or has finished.
    pub(super) fn ime(&mut self, ime: Ime) {
        let preedit = match ime {
            Ime::Preedit(text, cursor) => {
                (!text.is_empty()).then(|| Preedit { text, cursor: cursor.map(|at| at.0) })
            },
            Ime::Commit(text) => {
                self.type_keys(true, |_| text.as_bytes().to_vec());
                None
            },
            Ime::Enabled | Ime::Disabled => None,
        };
        if self.win.preedit != preedit {
            self.win.preedit = preedit;
            if let Some(pane) = self.focused_mut() {
                pane.dirty = true;
            }
            self.request_frame();
        }
    }

    /// Whether the app itself is following a held button.
    pub(super) fn any_dirty(&self) -> bool {
        self.core.panes.iter().any(|pane| pane.window == self.win.key && pane.dirty)
    }

    /// Draw a frame. Returns whether one was presented.
    pub(super) fn draw(&mut self) -> bool {
        self.win.gfx.make_current();
        #[cfg(feature = "bench")]
        if let Some(control) = self.win.gfx.control {
            self.win.gfx.draw_control(control);
            return true;
        }
        let now = Instant::now();
        let animating = self.win.motion.running(now);
        // The outlines and footers move with the panes.
        if animating || self.win.was_animating {
            self.win.chrome.invalidate();
        }
        let changed = {
            let _probe = probe::enter(Phase::Chrome);
            self.update_chrome(now)
        };
        let full =
            std::mem::take(&mut self.win.full) || animating || self.win.was_animating || changed;
        self.win.was_animating = animating;

        let bar = self.bar();
        let focused = self.win.workspace.focused();
        let motion = &self.win.motion;
        let panes = &self.core.panes;
        let scene = &mut self.win.scene;
        let rests = &self.win.rects;
        let mut add = |id: PaneId, rect: Rect| {
            let index = panes.iter().position(|pane| pane.id == id);
            let dirty = index.is_some_and(|index| panes[index].dirty);
            // A pane without a terminal yet is drawn blank, as is one the interface draws.
            let terminal = index.filter(|&index| panes[index].content.terminal().is_some());
            let display = index.filter(|&index| panes[index].content.display().is_some());
            let rest = rests.iter().find(|&&(pane, _)| pane == id).map_or(rect, |&(_, rest)| rest);
            scene.push(PaneDraw {
                terminal,
                display,
                rect: shell::terminal_rect(rect, bar),
                rest: shell::terminal_rect(rest, bar),
                focused: focused == Some(id),
                dirty,
                stale: false,
            });
        };

        // Panes on their way out go under the others.
        if !self.core.ui.locked() {
            for (id, rect) in motion.leaving(now) {
                add(id, rect);
            }
        }
        for &(id, rest) in &self.win.rects {
            add(id, motion.frame(id, now).unwrap_or(rest));
        }

        let preedit = self.win.preedit.as_ref();
        let chrome = &mut self.win.chrome;
        let _ = panes;
        let presented = self.win.gfx.draw(scene, &mut self.core.panes, preedit, full, chrome);

        let window = self.win.key;
        for pane in self.core.panes.iter_mut().filter(|pane| pane.window == window) {
            pane.dirty = false;
        }
        // Panes that could not be read keep asking for frames, drawn as they last were.
        for pane in scene.drain(..).filter(|pane| pane.stale) {
            if let Some(index) = pane.terminal {
                self.core.panes[index].dirty = true;
            }
        }
        let _probe = probe::enter(Phase::Post);
        self.sync_ime_area();
        // Picked up after presenting, not before: the GPU then has a whole frame to take in a
        // full-screen update, instead of holding this frame's swap until it has. It shows on the
        // next frame, which is the one frame of latency the budget allows.
        self.upload_displays(Instant::now());
        presented
    }
}
