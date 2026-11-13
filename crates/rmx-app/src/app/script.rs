//! The bench script runner: finding what a step points at, checking conditions, and
//! performing each step when it is due.

use super::*;

/// How often a held script looks again.
const SCRIPT_POLL: Duration = Duration::from_millis(20);

impl At<'_> {
    fn node_labelled(&self, label: &str) -> Option<u64> {
        self.core.ui.tree.nodes().iter().find(|node| node.label == label).map(|node| node.id.0)
    }

    /// The text of the focused terminal, laid out as it shows.
    fn screen_text(&self) -> String {
        let Some(terminal) = self.focused_terminal() else { return String::new() };
        let snapshot = terminal.last_snapshot();
        let (mut text, mut line, mut column) = (String::new(), 0, 0);
        for cell in &snapshot.cells {
            while line < cell.point.line {
                text.push('\n');
                line += 1;
                column = 0;
            }
            while column < cell.point.column.0 {
                text.push(' ');
                column += 1;
            }
            text.push(cell.character);
            column += 1;
        }
        text
    }

    fn focused_display(&self) -> Option<&Display> {
        self.pane(self.win.workspace.focused()?)?.content.display()
    }

    fn focused_files(&self) -> Option<&FilesPane> {
        self.pane(self.win.workspace.focused()?)?.content.files()
    }

    /// What the local clipboard holds, read through a copy so that a held `&self` can ask.
    fn bench_clipboard(&self) -> String {
        self.core.clipboard_seen.borrow().clone()
    }

    /// Act on the transfer with `name` in its label.
    fn files_row_action(&mut self, name: &str, action: fn(u64) -> FilesAction) {
        let Some(id) = self.win.workspace.focused() else { return };
        let row = self.focused_files().and_then(|files| {
            files.transfers().iter().find(|row| row.label.contains(name)).map(|row| row.id)
        });
        if let Some(row) = row {
            self.ui_action(UiAction::Files(rmx_ui::PaneId(id), action(row)));
        }
    }

    /// Open a connection in a new pane beside the focused one, as dragging it there would.
    fn open_split(&mut self, node: u64, axis: Axis) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        let name = info.name.clone();
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::remote(id, node, name, Goal::Session));
            self.start_session(id);
            self.relayout(true);
        }
    }

    /// The center of a column of the focused file pane, in window pixels.
    fn files_area(&self) -> Option<egui::Rect> {
        let id = self.win.workspace.focused()?;
        let &(_, rect) = self.win.rects.iter().find(|&&(pane, _)| pane == id)?;
        Some(shell::content_points(rect, self.bar(), self.scale()))
    }

    /// Where a script means by `target`, in window pixels.
    fn script_point(&self, target: &crate::bench::Point) -> Option<(f32, f32)> {
        use crate::bench::Point as P;
        let scale = self.scale();
        let panel = |index: usize| self.win.rects.get(index).map(|&(_, rect)| rect);
        match *target {
            P::By(dx, dy) => Some((self.win.pointer.x as f32 + dx, self.win.pointer.y as f32 + dy)),
            P::Pane(index, fx, fy) => {
                let rect = panel(index)?;
                Some((rect.x + rect.w * fx, rect.y + rect.h * fy))
            },
            P::Bar(index) => {
                let rect = panel(index)?;
                Some((rect.x + rect.w * 0.4, rect.y + PANE_BAR_HEIGHT * scale / 2.))
            },
            P::BarButton(index, from_right) => {
                let rect = panel(index)?;
                let (button, step) = (PANE_BAR_HEIGHT - 4., PANE_BAR_HEIGHT - 2.);
                let x = rect.right() - (2. + button / 2. + from_right as f32 * step) * scale;
                Some((x, rect.y + PANE_BAR_HEIGHT * scale / 2.))
            },
            P::Divider(index) => {
                let tab = self.win.workspace.active()?;
                let dividers = tab.layout.dividers(self.area(), self.spacing());
                let divider = dividers.get(index)?;
                Some(match divider.axis {
                    Axis::Row => (divider.at, divider.area.y + divider.area.h / 2.),
                    Axis::Column => (divider.area.x + divider.area.w / 2., divider.at),
                })
            },
            P::SidebarEdge => {
                let (_, height) = self.win.gfx.window_size();
                Some((self.sidebar_width(), height / 2.))
            },
            P::FilesFold => {
                let area = self.files_area()?;
                let at = rmx_ui::widgets::files_fold_point(area, &self.core.ui.panels);
                Some((at.x * scale, at.y * scale))
            },
            P::FilesSplit | P::FilesQueue => {
                let area = self.files_area()?;
                let (x, y) = rmx_ui::widgets::files_edges(area, &self.core.ui.panels);
                Some(match target {
                    P::FilesSplit => (x * scale, (area.top() + 100.) * scale),
                    _ => (area.center().x * scale, y * scale),
                })
            },
            P::MenuRow(ref label) => {
                let at = self.win.shell.menu_offers(label)?;
                Some((at.x * scale, at.y * scale))
            },
        }
    }

    fn condition_holds(&self, condition: &crate::bench::Condition, windows: usize) -> bool {
        use crate::bench::Condition as C;
        let link =
            || self.win.workspace.focused().and_then(|id| self.pane(id)).map(|pane| &pane.link);
        match condition {
            C::Setup => matches!(self.core.ui.gate, Gate::Setup(_)),
            C::Locked => matches!(self.core.ui.gate, Gate::Locked(_)),
            C::Unlocked => self.core.ui.gate == Gate::Open,
            C::HostKey => matches!(self.core.ui.asking, Some(Asking::HostKey(_))),
            C::Prompt => matches!(self.core.ui.asking, Some(Asking::Fields(_))),
            C::Live => matches!(link(), Some(Link::Live)),
            C::Closed => matches!(link(), Some(Link::Closed { .. })),
            C::Failed => matches!(link(), Some(Link::Failed { .. })),
            C::Synced => matches!(self.core.ui.sync, SyncState::Done(_)),
            C::NoQuestion => self.core.ui.asking.is_none(),
            C::Dialog => self.win.shell.dialog.is_open(),
            C::VaultError => self.core.ui.vault_error.is_some(),
            C::RemoteSize => self.focused_terminal().is_some_and(|terminal| {
                let size = terminal.size();
                let expected = format!("size-{} {}", size.lines_count(), size.columns_count());
                self.win.columns_before_resize != Some(size.columns_count())
                    && self.screen_text().contains(&expected)
            }),
            C::Tree(label) => self.node_labelled(label).is_some(),
            C::NoTree(label) => self.node_labelled(label).is_none(),
            C::Text(found) => self.screen_text().contains(found),
            C::Panes(count) => self.core.panes.len() == *count,
            C::Windows(count) => windows == *count,
            C::Snippet(name) => self.core.ui.snippets.iter().any(|row| row.name == *name),
            C::Awaiting => matches!(link(), Some(Link::Awaiting { .. })),
            C::LiveShown(count) => {
                let live = |&&(id, _): &&(PaneId, Rect)| {
                    self.pane(id).is_some_and(|pane| {
                        matches!(pane.link, Link::Live | Link::Local) && pane.content.has_screen()
                    })
                };
                self.win.rects.iter().filter(live).count() == *count
            },
            C::Tabs(count) => self.win.workspace.tabs().len() == *count,
            C::Display => self.focused_display().is_some_and(|d| d.pictures > 0),
            C::DisplayFits => self.focused_display().is_some_and(Display::fits_pane),
            C::Listed(side, name) => self
                .focused_files()
                .is_some_and(|f| f.rows(*side).iter().any(|r| r.name.as_ref() == name)),
            C::AtPath(side, path) => self.focused_files().is_some_and(|f| f.path(*side) == path),
            C::Transfer(name, state) => self.focused_files().is_some_and(|f| {
                use rmx_ui::widgets::TransferRowState as S;
                f.transfers().iter().any(|row| {
                    row.label.contains(name.as_str())
                        && match row.state {
                            S::Queued => state == "queued",
                            S::Running => state == "running",
                            S::Done => state == "done",
                            S::Failed => state == "failed",
                            S::Cancelled => state == "cancelled",
                        }
                })
            }),
            C::Unencrypted => self.core.ui.unencrypted,
            C::SyncFailed(found) => {
                matches!(&self.core.ui.sync, SyncState::Failed(reason) if reason.contains(found.as_str()))
            },
            C::Clipboard(found) => self.bench_clipboard().contains(found.as_str()),
            C::Zoomed(zoomed) => {
                self.win.workspace.active().is_some_and(|tab| tab.zoomed == *zoomed)
            },
            C::Menu(open) => self.win.shell.menu_open() == *open,
            C::MenuOffers(label) => self.win.shell.menu_offers(label).is_some(),
            C::Sidebar(low, high) => (*low..=*high).contains(&self.win.shell.sidebar_width),
            C::FilesSplit(low, high) => (*low..=*high).contains(&self.core.ui.panels.files_split),
            C::FilesQueue(low, high) => (*low..=*high).contains(&self.core.ui.panels.files_queue),
            C::FilesFolded(folded) => self.core.ui.panels.files_queue_folded == *folded,
            C::FilesText(found) => self
                .focused_files()
                .and_then(FilesPane::document_text)
                .is_some_and(|text| text.contains(found.as_str())),
            C::FilesDiff(rows) => self
                .focused_files()
                .and_then(FilesPane::diff_rows)
                .is_some_and(|found| found.len() == *rows),
            C::NoDiff => self.focused_files().is_some_and(|files| files.diff_rows().is_none()),
            C::FilesColumns => self.focused_files().is_some_and(|files| {
                files.document_text().is_none() && files.diff_rows().is_none()
            }),
            C::VaultNamed(name) => self.core.ui.vault_path.ends_with(name.as_str()),
            C::Ratio(low, high) => {
                let tab = self.win.workspace.active();
                let dividers = tab.map(|tab| tab.layout.dividers(self.area(), self.spacing()));
                dividers
                    .as_ref()
                    .and_then(|dividers| dividers.first())
                    .is_some_and(|divider| (*low..=*high).contains(&self.divider_ratio(divider)))
            },
            C::FirstSplit(axis) => {
                let tab = self.win.workspace.active();
                let dividers = tab.map(|tab| tab.layout.dividers(self.area(), self.spacing()));
                dividers
                    .as_ref()
                    .and_then(|dividers| dividers.first())
                    .is_some_and(|d| d.axis == *axis)
            },
            C::TabName(name) => {
                self.win.workspace.active().is_some_and(|tab| tab.name.as_deref() == Some(name))
            },
            C::Notice(found) => self
                .win
                .workspace
                .focused()
                .and_then(|id| self.pane(id))
                .and_then(|pane| pane.notice.as_deref())
                .is_some_and(|notice| notice.contains(found.as_str())),
        }
    }
}

impl App {
    /// Perform the next step of the bench script when it is due. Returns when the next one is.
    pub(super) fn run_script(&mut self, event_loop: &ActiveEventLoop) -> Option<Instant> {
        use crate::bench::Step;

        use crate::gfx::Shot;

        let running = self.running.as_mut()?;
        // A window that has gone falls back on the first.
        self.script_window = self.script_window.min(running.windows.len().saturating_sub(1));
        let window_count = running.windows.len();
        let script_window = self.script_window;
        let mut at = running.at_index(script_window)?;
        // Numbers for the wrong output are worse than none.
        if at.win.gfx.presentation.as_ref().is_some_and(|p| p.wrong_refresh()) {
            event_loop.exit();
            return None;
        }
        let now = Instant::now();
        self.bench.wakeup(now);
        if matches!(self.bench.waiting(), Some(crate::bench::Condition::Clipboard(_))) {
            let text = at.core.clipboard.load(ClipboardType::Clipboard);
            *at.core.clipboard_seen.borrow_mut() = text;
        }
        if let Some(condition) = self.bench.waiting() {
            if at.condition_holds(condition, window_count) {
                crate::bench::present::note("step", || "await released".to_owned());
                self.bench.release(now);
            } else if self.bench.hold_expired(now) {
                event_loop.exit();
                return None;
            } else {
                return Some(now + SCRIPT_POLL);
            }
        }
        let step = self.bench.due(now);
        if let Some(step) = step.as_ref().filter(|step| !matches!(step, Step::Wait)) {
            crate::bench::present::note("step", || step.name());
        }
        match step {
            None | Some(Step::Wait) => (),
            Some(Step::Quit) => event_loop.exit(),
            Some(Step::Action(action)) => {
                let what = action.clone();
                let start = Instant::now();
                at.perform(action);
                self.bench.action(start.elapsed(), &format!("{what:?}"));
            },
            Some(Step::Ui(what, action)) => {
                let start = Instant::now();
                if !at.ui_action(action) {
                    event_loop.exit();
                }
                self.bench.action(start.elapsed(), what);
                at.win.chrome.invalidate();
                at.request_frame();
            },
            Some(Step::Await(condition)) => {
                self.bench.hold(condition, now);
                return Some(now + SCRIPT_POLL);
            },
            Some(Step::Idle(length)) => self.bench.start_idle(now, length),
            Some(Step::Open(label)) => {
                let start = Instant::now();
                if let Some(node) = at.node_labelled(&label) {
                    at.open_connection(node);
                }
                self.bench.action(start.elapsed(), "open-connection");
            },
            Some(Step::Duplicate(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    at.core.vault.send(Request::Duplicate(node));
                }
            },
            Some(Step::Delete(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    at.ask_delete(node);
                }
            },
            Some(Step::Run(line)) => {
                if let Some(terminal) = at.focused_terminal() {
                    terminal.write(format!("{line}\r").into_bytes());
                }
            },
            Some(Step::Resize(width, height)) => {
                at.win.columns_before_resize =
                    at.focused_terminal().map(|terminal| terminal.size().columns_count());
                // On Wayland a size applied at once comes with no event to say so.
                if at.win.gfx.window.request_inner_size(LogicalSize::new(width, height)).is_some() {
                    at.win.gfx.resize();
                    at.relayout(false);
                }
            },
            Some(Step::Theme(mode)) => at.core.ops.push(Op::SetTheme(mode)),
            Some(Step::Edit(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    at.core.vault.send(Request::EditConnection(node));
                }
            },
            Some(Step::Rename(name)) => {
                if let Some(draft) = at.win.shell.dialog.draft_mut() {
                    draft.name = name;
                    let draft = Box::new(draft.clone());
                    at.win.shell.dialog.close();
                    at.ui_action(UiAction::ConnectionSubmit(draft));
                }
            },
            Some(Step::Menu(label)) => {
                if let Some(node) = at.node_labelled(&label) {
                    let pos = egui::pos2(120., 140.);
                    at.ui_action(UiAction::TreeContextMenu { id: NodeId(node), pos });
                    at.win.chrome.invalidate();
                    at.request_frame();
                }
            },
            Some(Step::OpenSplit(axis, label)) => {
                let start = Instant::now();
                if let Some(node) = at.node_labelled(&label) {
                    at.open_split(node, axis);
                }
                self.bench.action(start.elapsed(), "open-split");
            },
            Some(Step::Keys(text)) => {
                if let Some(pane) = at.focused_mut()
                    && let Some(display) = pane.content.display_mut()
                {
                    for character in text.chars() {
                        let Some((code, key)) = remote::typed_key(character) else { continue };
                        let physical = winit::keyboard::PhysicalKey::Code(code);
                        let location = winit::keyboard::KeyLocation::Standard;
                        if let Some(remote_key) = remote::remote_key(physical, &key, location) {
                            display.key(remote_key, true, false);
                            display.key(remote_key, false, false);
                        }
                    }
                }
            },
            Some(Step::Center) => {
                let center = at
                    .win
                    .workspace
                    .focused()
                    .and_then(|id| at.win.rects.iter().find(|&&(pane, _)| pane == id))
                    .map(|&(_, rect)| {
                        let rect = at.terminal_rect(rect);
                        (rect.x + rect.w / 2., rect.y + rect.h / 2.)
                    });
                if let Some((x, y)) = center {
                    let position = PhysicalPosition::new(f64::from(x), f64::from(y));
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(
                        event_loop,
                        id,
                        WindowEvent::CursorMoved { device_id, position },
                    );
                }
            },
            Some(Step::SetClipboard(text)) => {
                at.core.clipboard.store(ClipboardType::Clipboard, text)
            },
            Some(Step::OfferClipboard) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.offer_clipboard(id);
                }
            },
            Some(Step::Salute) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.salute(id);
                }
            },
            Some(Step::ToggleScale) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.toggle_scale(id);
                }
            },
            Some(Step::OpenFiles) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::PaneOpenFiles(rmx_ui::PaneId(id)));
                }
            },
            Some(Step::FilesGoto(side, path)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::Files(
                        rmx_ui::PaneId(id),
                        FilesAction::Goto { side, path },
                    ));
                }
            },
            Some(Step::FilesSelect(side, name)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.core.ui.files_select(id, side, &name);
                    at.win.chrome.invalidate();
                    at.request_frame();
                }
            },
            Some(Step::FilesSend(side)) => {
                if let Some(id) = at.win.workspace.focused() {
                    let names = at.core.ui.files_selected(id, side);
                    let action = FilesAction::Transfer { from: side, names };
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), action));
                }
            },
            Some(Step::FilesPoint(side, name)) => {
                let area = at.files_area();
                let files = at.focused_files();
                if let (Some(area), Some(files)) = (area, files)
                    && let Some(point) = files.row_point(area, side, &name, &at.core.ui.panels)
                {
                    let scale = at.scale();
                    let position = PhysicalPosition::new(
                        f64::from(point.x * scale),
                        f64::from(point.y * scale),
                    );
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(
                        event_loop,
                        id,
                        WindowEvent::CursorMoved { device_id, position },
                    );
                }
            },
            Some(Step::FilesPointColumn(side)) => {
                if let Some(area) = at.files_area() {
                    let scale = at.scale();
                    let x = if side == rmx_ui::FileSide::Local {
                        area.left() + area.width() / 4.
                    } else {
                        area.left() + area.width() * 3. / 4.
                    };
                    let position = PhysicalPosition::new(
                        f64::from(x * scale),
                        f64::from(area.center().y * scale * 0.7),
                    );
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(
                        event_loop,
                        id,
                        WindowEvent::CursorMoved { device_id, position },
                    );
                }
            },
            Some(Step::FilesCancel(name)) => at.files_row_action(&name, FilesAction::Cancel),
            Some(Step::FilesRetry(name)) => at.files_row_action(&name, FilesAction::Retry),
            Some(Step::FilesBody(action)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), action));
                }
            },
            Some(Step::FilesPolicy(index)) => {
                if let Some(id) = at.win.workspace.focused() {
                    at.ui_action(UiAction::Files(rmx_ui::PaneId(id), FilesAction::Policy(index)));
                }
            },
            Some(Step::Discard) => {
                if let Some(pane) = at.win.workspace.focused() {
                    at.session_event(pane, SessionEvent::OutputDiscarded(5 * MIB));
                }
            },
            Some(Step::Link(step)) => {
                use crate::bench::LinkStep;
                let start = Instant::now();
                if let Some(pane) = at.win.workspace.focused() {
                    let id = rmx_ui::PaneId(pane);
                    let action = match step {
                        LinkStep::Disconnect => UiAction::PaneDisconnect(id),
                        LinkStep::Reconnect => UiAction::PaneReconnect(id),
                        LinkStep::Cancel => UiAction::PaneCancel(id),
                    };
                    at.ui_action(action);
                }
                self.bench.action(start.elapsed(), "link");
            },
            Some(Step::Shortcut(letter)) => {
                // What the keyboard's way through `window_event` does, short of the key itself,
                // which cannot be made up: a script has no way to build a winit `KeyEvent`.
                let mods = ModifiersState::CONTROL | ModifiersState::SHIFT;
                if !at.core.ui.locked()
                    && !at.win.chrome.has_keyboard()
                    && let Some(action) = action_for(Key::Character(letter.as_str()), mods)
                {
                    at.perform(action);
                }
            },
            Some(Step::CloseTab(index)) => {
                if let Some(id) = at.win.workspace.tabs().get(index).map(|tab| tab.id) {
                    let start = Instant::now();
                    at.ui_action(UiAction::TabClose(TabId(id)));
                    self.bench.action(start.elapsed(), "close-tab");
                }
            },
            Some(Step::Window(index)) => self.script_window = index,
            Some(Step::Form(protocol)) => {
                at.win.shell.dialog.open_form(protocol);
                at.win.chrome.invalidate();
                at.request_frame();
            },
            Some(Step::FilesKey(key, extend)) => {
                at.press_files_key(key, extend);
            },
            Some(Step::TabPoint(index)) => {
                let point = at.win.shell.tab_center(index);
                if let Some(point) = point {
                    let scale = f64::from(at.scale());
                    let position = PhysicalPosition::new(
                        f64::from(point.x) * scale,
                        f64::from(point.y) * scale,
                    );
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(
                        event_loop,
                        id,
                        WindowEvent::CursorMoved { device_id, position },
                    );
                }
            },
            Some(Step::Point(target)) => {
                if let Some((x, y)) = at.script_point(&target) {
                    let position = PhysicalPosition::new(f64::from(x), f64::from(y));
                    let device_id = winit::event::DeviceId::dummy();
                    let id = at.win.gfx.window.id();
                    self.window_event(
                        event_loop,
                        id,
                        WindowEvent::CursorMoved { device_id, position },
                    );
                }
            },
            Some(Step::Shot(path)) => {
                at.win.gfx.shot = Some(Shot { path, full: true });
                at.redraw_all();
            },
            Some(Step::Peek(path)) => {
                at.win.gfx.shot = Some(Shot { path, full: false });
                if let Some(pane) = at.focused_mut() {
                    pane.dirty = true;
                }
                at.request_frame();
            },
            Some(Step::Input(input)) => {
                let id = at.win.gfx.window.id();
                self.window_event(event_loop, id, input);
            },
            Some(Step::Text(text)) => {
                at.win.chrome.push_event(egui::Event::Text(text));
            },
            Some(Step::Key(key)) => {
                for pressed in [true, false] {
                    at.win.chrome.push_event(egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
            },
        }
        self.bench.next_due()
    }
}
