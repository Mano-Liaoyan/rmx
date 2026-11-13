//! What the chrome asks for: its actions, snippets, typed text and file actions.

use super::*;

impl At<'_> {
    /// Give the keyboard to `pane`, as a click on it would.
    pub(super) fn focus_pane(&mut self, pane: PaneId) {
        if self.win.workspace.focus(pane) {
            self.sync_focus(Instant::now());
            self.redraw_all();
        }
    }

    /// Do what the interface asked. Returns false once the application should end.
    pub(super) fn ui_action(&mut self, action: UiAction) -> bool {
        // Only what the lock screen itself offers does anything while it is up.
        if self.core.ui.locked()
            && !matches!(
                action,
                UiAction::WindowMinimize
                    | UiAction::WindowToggleMaximize
                    | UiAction::WindowClose
                    | UiAction::WindowDrag
                    | UiAction::VaultUnlock { .. }
                    | UiAction::VaultCreate { .. }
                    | UiAction::VaultJoin { .. }
                    | UiAction::VaultMenu(_)
                    | UiAction::VaultSwitch(_)
                    | UiAction::VaultAskPath { .. }
                    | UiAction::VaultAskMove
                    | UiAction::HostKeyAcceptAlways
                    | UiAction::HostKeyAcceptOnce
                    | UiAction::HostKeyReject
                    | UiAction::PromptSubmit(_)
                    | UiAction::PromptCancel
            )
        {
            return true;
        }
        match action {
            UiAction::WindowMinimize => self.win.gfx.window.set_minimized(true),
            UiAction::WindowToggleMaximize => {
                let maximized = !self.win.gfx.window.is_maximized();
                self.win.gfx.window.set_maximized(maximized);
            },
            UiAction::WindowClose => {
                let key = self.win.key;
                let here = self
                    .core
                    .panes
                    .iter()
                    .filter(|pane| pane.window == key)
                    .map(|pane| pane.id)
                    .collect();
                if !self.ask_before_losing_edits(here, UiAction::WindowClose) {
                    return false;
                }
            },
            UiAction::WindowDrag => {
                if let Err(err) = self.win.gfx.window.drag_window() {
                    log::warn!("Could not move the window: {err}");
                }
                self.win.chrome.cancel_pointer();
            },
            UiAction::ToggleSidebar => self.perform(Action::ToggleSidebar),
            UiAction::TabActivate(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.perform(Action::SelectTab(index));
                }
            },
            UiAction::TabClose(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    let panes = self.win.workspace.tabs()[index].layout.panes().to_vec();
                    if !self.ask_before_losing_edits(panes, UiAction::TabClose(TabId(id))) {
                        self.close_tab(index);
                    }
                }
            },
            UiAction::TabNew => self.perform(Action::NewTab),
            UiAction::TabReorder { id: TabId(id), to } => {
                if let Some(from) = self.win.workspace.tab_index(id) {
                    self.win.workspace.reorder_tab(from, to);
                    self.relayout(false);
                }
            },
            UiAction::TabContextMenu { id, pos } => {
                let items = self.tab_menu(id);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::TabStartRename(id) => {
                if let Some(tab) = self
                    .win
                    .workspace
                    .tab_index(id.0)
                    .map(|index| &self.win.workspace.tabs()[index])
                {
                    let name = tab
                        .name
                        .as_deref()
                        .unwrap_or(pane_label(&self.core.panes, tab.focus))
                        .to_owned();
                    self.win.shell.start_tab_rename(id, &name);
                }
            },
            UiAction::TabRename { id: TabId(id), name } => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.win.workspace.set_tab_name(index, name);
                    self.core.session_changed();
                }
            },
            UiAction::TabUngroup(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id)
                    && self.win.workspace.ungroup(index)
                {
                    self.relayout(false);
                }
            },
            UiAction::TabDetach(TabId(id)) => {
                self.core.ops.push(Op::Tear { from: self.win.key, what: Moved::Tab(id) });
            },
            UiAction::TabToWindow(TabId(id), to) => {
                let (from, what) = (self.win.key, Moved::Tab(id));
                self.core.ops.push(Op::Move { from, what, to, at: usize::MAX });
            },
            UiAction::TabDragOut(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.win.shell.start_tab_drag(&self.win.workspace, index);
                }
            },
            UiAction::PaneSplitRight(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Split(Axis::Row));
            },
            UiAction::PaneSplitDown(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Split(Axis::Column));
            },
            UiAction::PaneMenu { id: rmx_ui::PaneId(pane), pos } => {
                self.focus_pane(pane);
                let items = self.pane_menu(pane);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::PaneZoom(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Zoom);
            },
            UiAction::PaneDuplicate(rmx_ui::PaneId(pane)) => self.duplicate_session(pane),
            UiAction::PaneSaveConnection(rmx_ui::PaneId(pane)) => self.ask_to_save(pane),
            UiAction::PaneToTab(rmx_ui::PaneId(pane)) => {
                let at = self.win.workspace.active_index() + 1;
                if self.win.workspace.detach(pane, at) {
                    self.relayout(false);
                }
            },
            UiAction::PaneToWindow(rmx_ui::PaneId(pane)) => {
                self.core.ops.push(Op::Tear { from: self.win.key, what: Moved::Pane(pane) });
            },
            UiAction::PaneCopy(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Copy);
            },
            UiAction::PanePaste(rmx_ui::PaneId(pane)) => {
                self.focus_pane(pane);
                self.perform(Action::Paste);
            },
            UiAction::PaneLog(rmx_ui::PaneId(pane)) => self.toggle_log(pane),
            UiAction::PaneDragStart(rmx_ui::PaneId(pane)) => {
                self.win.shell.start_pane_drag(&self.win.workspace, pane);
            },
            UiAction::QuickOpenPick(id) => match self.win.shell.picked(id) {
                Some(Picked::Run(action)) => self.perform(action),
                Some(Picked::Connection(id)) => self.open_connection(id),
                Some(Picked::Snippet(id)) => self.run_snippet(id),
                None => (),
            },
            UiAction::NewConnection => self.perform(Action::NewConnection),
            UiAction::ConnectionSubmit(draft) => match self.win.adopting.take() {
                Some(origin) if draft.editing.is_none() => {
                    self.core.vault.send(Request::SaveTransient { draft, origin });
                },
                _ => self.core.vault.send(Request::SaveConnection(draft)),
            },
            UiAction::ConnectionCancel => self.win.adopting = None,
            UiAction::QuickOpenDismiss | UiAction::TreeSelect(_) => (),
            UiAction::TreeOpen(NodeId(node)) => self.open_connection(node),
            UiAction::TreeOpenSplit { id: NodeId(node), down } => {
                self.open_node_split(node, if down { Axis::Column } else { Axis::Row });
            },
            UiAction::TreeOpenFiles(NodeId(node)) => {
                self.open_node(node, Goal::Files { directory: None })
            },
            UiAction::TreeShow(NodeId(node)) => {
                if let Some(pane) = self.sessions_of(node).last().map(|pane| pane.id) {
                    self.show_pane(pane);
                }
            },
            UiAction::TreeReconnect(NodeId(node)) => {
                let down: Vec<PaneId> = self
                    .sessions_of(node)
                    .filter(|pane| {
                        matches!(
                            pane.link,
                            Link::Failed { .. } | Link::Closed { .. } | Link::Awaiting { .. }
                        )
                    })
                    .map(|pane| pane.id)
                    .collect();
                for pane in down {
                    self.ui_action(UiAction::PaneReconnect(rmx_ui::PaneId(pane)));
                }
            },
            UiAction::TreeDisconnect(NodeId(node)) => {
                let live: Vec<PaneId> = self
                    .sessions_of(node)
                    .filter(|pane| pane.link == Link::Live)
                    .map(|pane| pane.id)
                    .collect();
                for pane in live {
                    self.hang_up(pane, "Disconnected");
                }
            },
            UiAction::CopyText(text) => self.core.clipboard.store(ClipboardType::Clipboard, text),
            UiAction::QuickConnect(text) => {
                if let Some(draft) = quick_draft(&text) {
                    self.open_transient(draft);
                }
            },
            UiAction::SidebarResize(width) => {
                self.win.shell.sidebar_width = width;
                self.core.ui.panels.sidebar = width;
                self.relayout(false);
            },
            UiAction::PanelsChanged => self.core.vault.send(Request::SaveLayout(self.layout())),
            UiAction::FilesMenu { pane: rmx_ui::PaneId(pane), side, name, pos } => {
                self.focus_pane(pane);
                let items = self.files_menu(pane, side, name);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::FilesPolicyMenu { pane: rmx_ui::PaneId(pane), pos } => {
                let current = self
                    .pane(pane)
                    .and_then(|found| found.content.files())
                    .map_or(0, FilesPane::policy);
                let act = |index| UiAction::Files(rmx_ui::PaneId(pane), FilesAction::Policy(index));
                let row = |(index, label): (usize, &&str)| MenuItem::Action {
                    glyph: (index == current).then_some(icons::CHECK),
                    label: (*label).to_owned(),
                    shortcut: None,
                    action: act(index),
                    danger: false,
                };
                let items =
                    rmx_ui::widgets::CONFLICT_POLICIES.iter().enumerate().map(row).collect();
                self.win.shell.open_menu(pos, items);
            },
            UiAction::FilesAskNewFolder(rmx_ui::PaneId(pane), side) => {
                self.ask_folder_name(
                    "New folder",
                    String::new(),
                    Reply::FilesNewFolder(pane, side),
                );
            },
            UiAction::FilesAskRename(rmx_ui::PaneId(pane), side, name) => {
                self.ask_folder_name(
                    "Rename",
                    name.to_string(),
                    Reply::FilesRename(pane, side, name),
                );
            },
            UiAction::FilesAskDelete(rmx_ui::PaneId(pane), side, names) => {
                let title = match names.as_slice() {
                    [one] => format!("Delete {one}?"),
                    many => format!("Delete {} items?", many.len()),
                };
                let mut state = PromptState::new(title, "This cannot be undone.", Vec::new());
                state.submit = "Delete".to_owned();
                self.ask_here(state, Reply::FilesDelete(pane, side, names));
            },
            UiAction::TreeFolderToggled { id, expanded } => {
                self.core.vault.send(Request::FolderToggled { node: id.0, expanded });
            },
            UiAction::TreeContextMenu { id, pos } => {
                let items = self.tree_menu(id);
                self.win.shell.open_menu(pos, items);
            },
            UiAction::TreeEdit(NodeId(node)) => self.core.vault.send(Request::EditConnection(node)),
            UiAction::TreeDuplicate(NodeId(node)) => self.core.vault.send(Request::Duplicate(node)),
            UiAction::TreeDelete(NodeId(node)) => self.ask_delete(node),
            UiAction::TreeNewFolder(parent) => {
                let parent = parent.map(|id| id.0);
                self.ask_folder_name("New folder", String::new(), Reply::NewFolder(parent));
            },
            UiAction::TreeRenameFolder(NodeId(node)) => {
                let name = self.core.ui.nodes.get(&node).map(|info| info.name.clone());
                self.ask_folder_name(
                    "Rename folder",
                    name.unwrap_or_default(),
                    Reply::RenameFolder(node),
                );
            },
            UiAction::TreeFolderLogin(NodeId(node)) => {
                self.core.vault.send(Request::EditFolderLogin(node));
            },
            UiAction::TreeClearFolderLogin(NodeId(node)) => {
                self.core.vault.send(Request::ClearFolderLogin(node));
            },
            UiAction::VaultLock => self.core.vault.send(Request::Lock),
            UiAction::VaultMenu(pos) => {
                let items = self.vault_menu();
                self.win.shell.open_menu(pos, items);
            },
            UiAction::VaultSwitch(path) => {
                self.core.ui.vault_error = None;
                self.core.vault.send(Request::Switch(path));
            },
            UiAction::VaultAskPath { create } => {
                let directory = std::path::Path::new(&self.core.ui.vault_path)
                    .parent()
                    .map_or(String::new(), |dir| dir.display().to_string());
                let (title, value) = if create {
                    ("New vault", format!("{directory}/new.rmxdb"))
                } else {
                    ("Open vault", format!("{directory}/"))
                };
                let field = PromptField { label: String::new(), secret: false, value };
                self.ask_here(PromptState::new(title, "", vec![field]), Reply::VaultPath);
            },
            UiAction::VaultAskMove => {
                let field = PromptField {
                    label: String::new(),
                    secret: false,
                    value: self.core.ui.vault_path.clone(),
                };
                self.ask_here(PromptState::new("Move vault", "", vec![field]), Reply::VaultMove);
            },
            UiAction::VaultAskPassword => {
                let secret = |label: &str| PromptField {
                    label: label.to_owned(),
                    secret: true,
                    value: String::new(),
                };
                let key_file = PromptField {
                    label: "Key file (optional)".to_owned(),
                    secret: false,
                    value: self.core.ui.vault_key_file.clone(),
                };
                let fields = vec![
                    secret("Current master password"),
                    secret("New master password"),
                    secret("Repeat the new master password"),
                    key_file,
                ];
                let detail = if matches!(self.core.ui.sync, SyncState::Off) {
                    ""
                } else {
                    "The synced copy changes too. Other devices will ask for the new password."
                };
                let mut state = PromptState::new("Change master password", detail, fields);
                state.submit = "Change".to_owned();
                self.ask_here(state, Reply::VaultPassword);
            },
            UiAction::VaultUnlock { mut password, key_file } => {
                let password = Secret::new(password.take());
                self.core.vault.send(Request::Unlock { password, key_file: key_path(&key_file) });
                self.start_vault_work();
            },
            UiAction::VaultCreate { mut password, key_file, plain_ok } => {
                let password = Secret::new(password.take());
                self.core.vault.send(Request::Create {
                    password,
                    key_file: key_path(&key_file),
                    plain_ok,
                });
                self.start_vault_work();
            },
            UiAction::VaultJoin { mut password, key_file, sync, plain_ok } => {
                let password = Secret::new(password.take());
                self.core.vault.send(Request::Join {
                    password,
                    key_file: key_path(&key_file),
                    sync,
                    plain_ok,
                });
                self.start_vault_work();
            },
            UiAction::HostKeyReject => self.answered(Some(HostKeyAnswer::Reject), None),
            UiAction::HostKeyAcceptOnce => self.answered(Some(HostKeyAnswer::Once), None),
            UiAction::HostKeyAcceptAlways => self.answered(Some(HostKeyAnswer::Always), None),
            UiAction::PromptSubmit(fields) => self.answered(None, Some(fields)),
            UiAction::PromptCancel => self.answered(None, None),
            UiAction::PaneCancel(rmx_ui::PaneId(pane)) => self.hang_up(pane, "Cancelled"),
            UiAction::PaneDisconnect(rmx_ui::PaneId(pane)) => self.hang_up(pane, "Disconnected"),
            UiAction::PaneReconnect(rmx_ui::PaneId(pane)) => {
                if let Some(remote) = self.pane_mut(pane).and_then(|found| found.remote.as_mut()) {
                    remote.quiet = false;
                }
                self.start_session(pane);
            },
            UiAction::PaneClose(rmx_ui::PaneId(pane)) => {
                if !self
                    .ask_before_losing_edits(vec![pane], UiAction::PaneClose(rmx_ui::PaneId(pane)))
                {
                    self.close(pane);
                }
            },
            UiAction::PaneFocus(rmx_ui::PaneId(pane)) => self.focus_pane(pane),
            UiAction::PaneCtrlAltDel(rmx_ui::PaneId(pane)) => self.salute(pane),
            UiAction::PaneToggleScale(rmx_ui::PaneId(pane)) => self.toggle_scale(pane),
            UiAction::PaneOpenFiles(rmx_ui::PaneId(pane)) => self.open_files_here(pane),
            UiAction::PaneBroadcast(rmx_ui::PaneId(pane)) => self.toggle_broadcast(pane),
            UiAction::Files(rmx_ui::PaneId(pane), action) => self.files_action(pane, action),
            UiAction::PaneDismissNotice(rmx_ui::PaneId(pane)) => {
                if let Some(found) = self.pane_mut(pane) {
                    found.notice = None;
                }
                self.link_changed(pane);
            },
            UiAction::OpenSettings => self.perform(Action::OpenSettings),
            UiAction::ToggleTheme => self.perform(Action::ToggleTheme),
            UiAction::SettingsSave(draft) => {
                self.core.ops.push(Op::SetTheme(draft.theme));
                self.core.vault.send(Request::SaveSettings(draft));
            },
            UiAction::SettingsCancel => (),
            UiAction::SyncNow => self.perform(Action::SyncNow),
            UiAction::ImportSshConfig => self.core.vault.send(Request::ImportSshConfig),
            UiAction::SnippetSave { id, name, body } => {
                self.core.vault.send(Request::SaveSnippet { id, name, body });
            },
            UiAction::SnippetDelete(id) => self.core.vault.send(Request::DeleteSnippet(id)),
            UiAction::ListSerialPorts => {
                let proxy = self.core.proxy.clone();
                self.core.runtime.spawn(async move {
                    let ports = rmx_serial::serial::list_ports().await;
                    let _ = proxy
                        .send_event(AppEvent::SerialPorts(ports.iter().map(port_choice).collect()));
                });
            },
            UiAction::SnippetSend(id) => self.run_snippet(id),
        }
        true
    }

    /// Send a saved snippet to the focused terminal, first asking for what it leaves open.
    pub(super) fn run_snippet(&mut self, id: u64) {
        let Some(row) = self.core.ui.snippets.iter().find(|row| row.id == id).cloned() else {
            return;
        };
        let fields: Vec<PromptField> = snippet::variables(&row.body)
            .into_iter()
            .map(|variable| PromptField {
                label: variable.name.to_owned(),
                secret: false,
                value: variable.default.unwrap_or_default().to_owned(),
            })
            .collect();
        if fields.is_empty() {
            self.send_text(&row.body);
            return;
        }
        let mut state = PromptState::new(row.name.clone(), "", fields);
        state.submit = "Send".to_owned();
        self.ask_here(state, Reply::Snippet(row.body));
    }

    /// Type `text` into the focused terminal, a line break being the Enter key.
    pub(super) fn send_text(&mut self, text: &str) {
        let bytes = text.replace('\n', "\r").into_bytes();
        let terminal = self.focused_mut().and_then(|pane| {
            pane.dirty = true;
            pane.content.terminal_mut()
        });
        match terminal {
            Some(terminal) => {
                terminal.scroll_to_bottom();
                terminal.write(bytes);
                self.request_frame();
            },
            None => {
                self.core.ui.status_note =
                    Some(("Snippets go to a terminal: focus one first".to_owned(), true));
                self.core.broadcast = true;
            },
        }
    }

    /// Do what a file pane asked.
    pub(super) fn files_action(&mut self, id: PaneId, action: FilesAction) {
        let editing =
            self.pane(id).and_then(|pane| pane.content.files()).is_some_and(FilesPane::editing);
        if action == FilesAction::CloseBody && editing && self.core.ui.files_dirty(id) {
            let then = UiAction::Files(rmx_ui::PaneId(id), FilesAction::CloseBody);
            self.ask_before_losing_edits(vec![id], then);
            return;
        }
        let Some(files) = self.pane_mut(id).and_then(|pane| pane.content.files_mut()) else {
            return;
        };
        if let Some(side) = files.action(action) {
            self.core.ui.files_reset(id, side);
        }
        self.win.chrome.invalidate();
        self.request_frame();
    }
}
