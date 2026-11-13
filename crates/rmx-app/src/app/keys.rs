//! Input routing: the keys bound to actions, what they do, and the keys of a file pane.

use rmx_terminal::TermMode;

use super::*;

/// The action bound to a key, if any.
pub(super) fn action_for(key: Key<&str>, mods: ModifiersState) -> Option<Action> {
    let ctrl_shift = mods.control_key() && mods.shift_key() && !mods.alt_key();

    if mods.control_key() && key == Key::Named(NamedKey::Tab) {
        return Some(Action::CycleTab(if mods.shift_key() { -1 } else { 1 }));
    }
    if !ctrl_shift {
        return None;
    }

    match key {
        Key::Named(NamedKey::ArrowLeft) => Some(Action::Focus(Direction::Left)),
        Key::Named(NamedKey::ArrowRight) => Some(Action::Focus(Direction::Right)),
        Key::Named(NamedKey::ArrowUp) => Some(Action::Focus(Direction::Up)),
        Key::Named(NamedKey::ArrowDown) => Some(Action::Focus(Direction::Down)),
        Key::Character(text) => match text.to_lowercase().as_str() {
            "t" => Some(Action::NewTab),
            "w" => Some(Action::ClosePane),
            "e" => Some(Action::Split(Axis::Row)),
            "o" => Some(Action::Split(Axis::Column)),
            "z" => Some(Action::Zoom),
            "c" => Some(Action::Copy),
            "v" => Some(Action::Paste),
            "p" => Some(Action::QuickOpen),
            "n" => Some(Action::NewConnection),
            "s" => Some(Action::Snippets),
            "b" => Some(Action::ToggleSidebar),
            digit => match digit.parse::<usize>() {
                Ok(n @ 1..=9) => Some(Action::SelectTab(n - 1)),
                _ => None,
            },
        },
        _ => None,
    }
}

/// What `key` does in a file pane, and whether Shift is adding to the selection.
pub(super) fn files_key_for(key: Key<&str>, mods: ModifiersState) -> Option<(FilesKey, bool)> {
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return None;
    }
    let Key::Named(name) = key else { return None };
    let found = match name {
        NamedKey::ArrowUp => FilesKey::Up,
        NamedKey::ArrowDown => FilesKey::Down,
        NamedKey::PageUp => FilesKey::PageUp,
        NamedKey::PageDown => FilesKey::PageDown,
        NamedKey::Home => FilesKey::Home,
        NamedKey::End => FilesKey::End,
        NamedKey::Enter => FilesKey::Open,
        NamedKey::Backspace => FilesKey::Back,
        NamedKey::Tab => FilesKey::Switch,
        NamedKey::F5 => FilesKey::Copy,
        NamedKey::Escape => FilesKey::Escape,
        _ => return None,
    };
    Some((found, mods.shift_key()))
}

impl At<'_> {
    pub(super) fn perform(&mut self, action: Action) {
        match action {
            Action::NewTab => {
                let id = self.new_id();
                self.add_pane(Pane::local(id));
                self.win.workspace.add_tab(id);
                if self.spawn(id) {
                    self.relayout(false);
                } else {
                    self.close(id);
                }
            },
            Action::Split(axis) => self.split_focused(axis),
            Action::SplitLocal(axis) => self.split_local(axis),
            Action::ClosePane => {
                if let Some(id) = self.win.workspace.focused() {
                    self.ui_action(UiAction::PaneClose(rmx_ui::PaneId(id)));
                }
            },
            Action::Focus(direction) => {
                let (area, spacing) = (self.area(), self.spacing());
                let zoomed = self.win.workspace.active().is_some_and(|tab| tab.zoomed);
                if self.win.workspace.focus_direction(direction, area, spacing) {
                    if zoomed {
                        self.relayout(true);
                    } else {
                        self.sync_focus(Instant::now());
                        self.redraw_all();
                    }
                }
            },
            Action::Zoom => {
                self.win.workspace.toggle_zoom();
                self.relayout(true);
            },
            Action::CycleTab(delta) => {
                if self.win.workspace.cycle(delta) {
                    self.relayout(false);
                }
            },
            Action::SelectTab(index) => {
                if self.win.workspace.select(index) {
                    self.relayout(false);
                }
            },
            Action::Copy => {
                let text = self.focused_terminal().and_then(Terminal::selection);
                if let Some(text) = text {
                    self.core.clipboard.store(ClipboardType::Clipboard, text);
                }
            },
            Action::Paste => {
                let text = self.core.clipboard.load(ClipboardType::Clipboard);
                if let Some(terminal) = self.focused_terminal() {
                    terminal.paste(&text);
                } else if let Some(id) = self.win.display_focus {
                    self.offer_clipboard(id);
                }
            },
            Action::SendCtrlAltDel => {
                if let Some(id) = self.win.workspace.focused() {
                    self.salute(id);
                }
            },
            Action::ToggleScale => {
                if let Some(id) = self.win.workspace.focused() {
                    self.toggle_scale(id);
                }
            },
            Action::QuickOpen => {
                if self.win.shell.quick.is_open() {
                    self.win.shell.quick.close();
                } else {
                    self.win.shell.open_palette(
                        &self.win.workspace,
                        &self.core.panes,
                        &self.core.ui,
                    );
                }
                self.win.chrome.invalidate();
            },
            Action::NewConnection => {
                self.win.shell.dialog.open();
                self.win.chrome.invalidate();
            },
            Action::Snippets => {
                self.win.shell.snippets.open();
                self.win.chrome.invalidate();
            },
            Action::ToggleSidebar => {
                self.win.shell.sidebar_open = !self.win.shell.sidebar_open;
                self.core.vault.send(Request::SaveLayout(self.layout()));
                self.relayout(false);
            },
            Action::OpenSettings => {
                self.core.vault.send(Request::LoadSettings { theme: self.core.theme })
            },
            Action::ToggleTheme => {
                let mode = if self.core.theme == ThemeMode::Dark {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                self.core.ops.push(Op::SetTheme(mode));
            },
            Action::Lock => self.core.vault.send(Request::Lock),
            Action::SyncNow => {
                if self.core.ui.sync != SyncState::Off {
                    self.core.ui.sync = SyncState::Running;
                    self.win.chrome.invalidate();
                }
                self.core.vault.send(Request::SyncNow);
            },
        }
    }

    /// Put `pane` in the broadcast group, or take it out. Only a terminal can join.
    pub(super) fn toggle_broadcast(&mut self, pane: PaneId) {
        let Some(found) = self.pane_mut(pane) else { return };
        if !found.broadcast && found.content.terminal().is_none() {
            return;
        }
        found.broadcast = !found.broadcast;
        self.win.chrome.invalidate();
        self.redraw_all();
    }

    /// Type into the focused terminal, and into the rest of its broadcast group when it is in one.
    /// `encode` makes the bytes for a terminal's modes, so that each target gets the key as it
    /// reads keys; each is written to once. A press scrolls each target to the bottom.
    pub(super) fn type_keys(&mut self, pressed: bool, encode: impl Fn(TermMode) -> Vec<u8>) {
        let Some(focused) = self.win.workspace.focused() else { return };
        crate::broadcast::each_target(&mut self.core.panes, focused, |pane| {
            let Some(terminal) = pane.content.terminal_mut() else { return };
            let bytes = encode(terminal.mode());
            if bytes.is_empty() {
                return;
            }
            if pressed {
                terminal.scroll_to_bottom();
                pane.dirty = true;
            }
            terminal.write(bytes);
        });
    }

    /// A key meant for the focused pane, if it is a file pane's. Returns whether it was used.
    pub(super) fn files_key(&mut self, key: &KeyEvent, mods: ModifiersState) -> bool {
        if key.state != ElementState::Pressed {
            return false;
        }
        match files_key_for(key.logical_key.as_ref(), mods) {
            Some((key, extend)) => self.press_files_key(key, extend),
            None => false,
        }
    }

    /// Have the focused pane take `key`, if it is a file pane. Returns whether it was.
    pub(super) fn press_files_key(&mut self, key: FilesKey, extend: bool) -> bool {
        if self.core.ui.locked() || self.core.ui.asking.is_some() || self.win.chrome.has_keyboard()
        {
            return false;
        }
        let Some(id) = self.win.workspace.focused() else { return false };
        let Core { panes, ui, .. } = &mut *self.core;
        let Some(files) =
            panes.iter().find(|pane| pane.id == id).and_then(|pane| pane.content.files())
        else {
            return false;
        };
        let view = files.view(rmx_ui::PaneId(id));
        let action = ui.files_key(id, key, extend, &view);
        if let Some(action) = action {
            self.files_action(id, action);
        }
        self.win.chrome.invalidate();
        self.request_frame();
        true
    }
}
