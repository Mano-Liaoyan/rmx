//! The context menus of the tree, panes, tabs, file panes and the vault.

use super::*;

impl At<'_> {
    /// What can be done from the menu of a node of the tree.
    pub(super) fn tree_menu(&self, node: NodeId) -> Vec<MenuItem> {
        let Some(info) = self.core.ui.nodes.get(&node.0) else { return Vec::new() };
        if info.kind == NodeKind::Folder {
            return vec![
                menu_item(icons::PLUS, "New connection", UiAction::NewConnection, false),
                menu_item(icons::FOLDER, "New folder", UiAction::TreeNewFolder(Some(node)), false),
                menu_item(icons::PENCIL, "Rename", UiAction::TreeRenameFolder(node), false),
                MenuItem::Separator,
                menu_item(icons::TRASH_2, "Delete folder", UiAction::TreeDelete(node), true),
            ];
        }
        let mut items = vec![
            menu_item(icons::PLUG, "Connect", UiAction::TreeOpen(node), false),
            menu_item(
                icons::COLUMNS_2,
                "Connect in split right",
                UiAction::TreeOpenSplit { id: node, down: false },
                false,
            ),
            menu_item(
                icons::ROWS_2,
                "Connect in split down",
                UiAction::TreeOpenSplit { id: node, down: true },
                false,
            ),
        ];
        if info.kind == NodeKind::Connection(Protocol::Ssh) {
            items.push(menu_item(
                icons::FOLDER_SYNC,
                "Files",
                UiAction::TreeOpenFiles(node),
                false,
            ));
        }
        let has_session = self.sessions_of(node.0).next().is_some();
        let live = self.sessions_of(node.0).any(|pane| pane.link == Link::Live);
        let down = self.sessions_of(node.0).any(|pane| {
            matches!(pane.link, Link::Failed { .. } | Link::Closed { .. } | Link::Awaiting { .. })
        });
        if has_session {
            items.push(MenuItem::Separator);
            items.push(menu_item(icons::SEARCH, "Show session", UiAction::TreeShow(node), false));
            if down {
                items.push(menu_item(
                    icons::PLUG,
                    "Reconnect",
                    UiAction::TreeReconnect(node),
                    false,
                ));
            }
            if live {
                items.push(menu_item(
                    icons::X,
                    "Disconnect",
                    UiAction::TreeDisconnect(node),
                    false,
                ));
            }
        }
        items.push(MenuItem::Separator);
        items.push(menu_item(icons::PENCIL, "Edit", UiAction::TreeEdit(node), false));
        items.push(menu_item(icons::COPY, "Duplicate", UiAction::TreeDuplicate(node), false));
        if !info.host.is_empty() {
            items.push(menu_item(
                icons::COPY,
                "Copy host",
                UiAction::CopyText(info.host.clone()),
                false,
            ));
        }
        items.push(MenuItem::Separator);
        items.push(menu_item(icons::TRASH_2, "Delete", UiAction::TreeDelete(node), true));
        items
    }

    /// What can be done about the session of `pane`.
    pub(super) fn session_items(&self, pane: PaneId, items: &mut Vec<MenuItem>) {
        let id = rmx_ui::PaneId(pane);
        let Some(found) = self.pane(pane) else { return };
        match &found.link {
            Link::Live => {
                if found.content.display().is_some() {
                    items.push(menu_item(
                        icons::MONITOR,
                        "Send Ctrl+Alt+Del",
                        UiAction::PaneCtrlAltDel(id),
                        false,
                    ));
                    items.push(menu_item(
                        icons::MONITOR_COG,
                        "Toggle scale",
                        UiAction::PaneToggleScale(id),
                        false,
                    ));
                }
                if found.content.terminal().is_some() && self.pane_is_ssh(pane) {
                    items.push(menu_item(
                        icons::FOLDER_SYNC,
                        "Files here",
                        UiAction::PaneOpenFiles(id),
                        false,
                    ));
                }
                items.push(menu_item(
                    icons::PLUG,
                    "Disconnect",
                    UiAction::PaneDisconnect(id),
                    false,
                ));
            },
            Link::Closed { .. } | Link::Failed { .. } | Link::Awaiting { .. } => {
                items.push(menu_item(icons::PLUG, "Reconnect", UiAction::PaneReconnect(id), false));
            },
            Link::Connecting { .. } => {
                items.push(menu_item(icons::X, "Cancel", UiAction::PaneCancel(id), false));
            },
            Link::Local => (),
        }
        items.push(menu_item(icons::COPY, "Duplicate session", UiAction::PaneDuplicate(id), false));
    }

    /// Splitting and zooming `pane`.
    pub(super) fn layout_items(&self, pane: PaneId, items: &mut Vec<MenuItem>) {
        let id = rmx_ui::PaneId(pane);
        items.push(menu_keyed(
            icons::COLUMNS_2,
            "Split right",
            "Ctrl+Shift+E",
            UiAction::PaneSplitRight(id),
        ));
        items.push(menu_keyed(
            icons::ROWS_2,
            "Split down",
            "Ctrl+Shift+O",
            UiAction::PaneSplitDown(id),
        ));
        let tab = self.win.workspace.tabs().iter().find(|tab| tab.layout.panes().contains(&pane));
        if tab.is_some_and(|tab| tab.layout.panes().len() > 1) {
            let zoomed = tab.is_some_and(|tab| tab.zoomed);
            let (glyph, label) =
                if zoomed { (icons::COPY, "Unzoom") } else { (icons::SQUARE, "Zoom") };
            items.push(menu_keyed(glyph, label, "Ctrl+Shift+Z", UiAction::PaneZoom(id)));
        }
    }

    /// What can be done from the menu of a pane.
    pub(super) fn pane_menu(&self, pane: PaneId) -> Vec<MenuItem> {
        let id = rmx_ui::PaneId(pane);
        let mut items = Vec::new();
        if let Some(terminal) = self.pane(pane).and_then(|found| found.content.terminal()) {
            if terminal.selection().is_some() {
                items.push(menu_keyed(icons::COPY, "Copy", "Ctrl+Shift+C", UiAction::PaneCopy(id)));
            }
            items.push(menu_keyed(
                icons::CORNER_DOWN_LEFT,
                "Paste",
                "Ctrl+Shift+V",
                UiAction::PanePaste(id),
            ));
            let logging = self.pane(pane).is_some_and(|found| found.log.is_some());
            let label = if logging { "Stop logging" } else { "Log output to a file" };
            items.push(menu_item(icons::HARD_DRIVE_UPLOAD, label, UiAction::PaneLog(id), false));
            let joined = self.pane(pane).is_some_and(|found| found.broadcast);
            let label = if joined { "Stop broadcasting" } else { "Broadcast input" };
            items.push(menu_item(icons::CABLE, label, UiAction::PaneBroadcast(id), false));
            items.push(MenuItem::Separator);
        }
        let before = items.len();
        self.session_items(pane, &mut items);
        if items.len() > before {
            items.push(MenuItem::Separator);
        }
        self.layout_items(pane, &mut items);

        let tabs = self.win.workspace.tabs();
        let grouped = tabs
            .iter()
            .any(|tab| tab.layout.panes().len() > 1 && tab.layout.panes().contains(&pane));
        if grouped || tabs.len() > 1 {
            items.push(MenuItem::Separator);
        }
        if grouped {
            items.push(menu_item(
                icons::ARROW_RIGHT,
                "Move to new tab",
                UiAction::PaneToTab(id),
                false,
            ));
        }
        if grouped || tabs.len() > 1 {
            items.push(menu_item(
                icons::SQUARE,
                "Move to new window",
                UiAction::PaneToWindow(id),
                false,
            ));
        }
        items.push(MenuItem::Separator);
        items.push(menu_keyed(icons::X, "Close", "Ctrl+Shift+W", UiAction::PaneClose(id)));
        items
    }

    /// What can be done from the menu of the tab `id`.
    pub(super) fn tab_menu(&self, id: TabId) -> Vec<MenuItem> {
        let Some(index) = self.win.workspace.tab_index(id.0) else { return Vec::new() };
        let tab = &self.win.workspace.tabs()[index];
        let mut items =
            vec![menu_item(icons::PENCIL, "Rename", UiAction::TabStartRename(id), false)];
        if tab.name.is_some() {
            items.push(menu_item(
                icons::CORNER_DOWN_LEFT,
                "Reset name",
                UiAction::TabRename { id, name: None },
                false,
            ));
        }
        items.push(MenuItem::Separator);
        self.session_items(tab.focus, &mut items);
        items.push(MenuItem::Separator);
        self.layout_items(tab.focus, &mut items);
        items.push(MenuItem::Separator);
        if tab.layout.panes().len() > 1 {
            items.push(menu_item(icons::COLUMNS_2, "Ungroup", UiAction::TabUngroup(id), false));
        }
        if self.win.workspace.tabs().len() > 1 {
            items.push(menu_item(
                icons::SQUARE,
                "Move to new window",
                UiAction::TabDetach(id),
                false,
            ));
        }
        for (number, &window) in self.core.window_keys.iter().enumerate() {
            if window != self.win.key {
                let label = format!("Move to window {}", number + 1);
                items.push(menu_item(
                    icons::CORNER_DOWN_LEFT,
                    &label,
                    UiAction::TabToWindow(id, window),
                    false,
                ));
            }
        }
        items.push(menu_item(icons::X, "Close tab", UiAction::TabClose(id), true));
        items
    }

    /// What can be done from the menu of a column of a file pane.
    pub(super) fn files_menu(
        &self,
        pane: PaneId,
        side: rmx_ui::FileSide,
        name: Option<Arc<str>>,
    ) -> Vec<MenuItem> {
        let id = rmx_ui::PaneId(pane);
        let act = |action| UiAction::Files(id, action);
        let Some(files) = self.pane(pane).and_then(|found| found.content.files()) else {
            return Vec::new();
        };
        let Some(name) = name else {
            return vec![
                menu_item(
                    icons::FOLDER,
                    "New folder",
                    UiAction::FilesAskNewFolder(id, side),
                    false,
                ),
                menu_item(icons::FOLDER_SYNC, "Refresh", act(FilesAction::Refresh { side }), false),
            ];
        };
        let names = self.core.ui.files_selected_or(pane, side, &name);
        let row = files.rows(side).iter().find(|row| row.name == name);
        let mut items = Vec::new();
        if row.is_some_and(rmx_ui::widgets::FileRow::openable) {
            items.push(menu_item(
                icons::FOLDER_OPEN,
                "Open",
                act(FilesAction::Open { side, name: name.clone() }),
                false,
            ));
        }
        if let Some((local, remote)) =
            self.core.ui.files_compare_pair(pane, &files.view(id), side, &name)
        {
            items.push(menu_item(
                icons::COLUMNS_2,
                "Compare",
                act(FilesAction::Compare { local, remote }),
                false,
            ));
        }
        let (glyph, label) = match side {
            rmx_ui::FileSide::Local => (icons::ARROW_RIGHT, "Upload"),
            rmx_ui::FileSide::Remote => (icons::ARROW_LEFT, "Download"),
        };
        items.push(menu_item(
            glyph,
            label,
            act(FilesAction::Transfer { from: side, names: names.clone() }),
            false,
        ));
        let path = format!("{}/{name}", files.path(side).trim_end_matches('/'));
        items.push(menu_item(icons::COPY, "Copy path", UiAction::CopyText(path), false));
        items.push(MenuItem::Separator);
        if names.len() == 1 {
            items.push(menu_item(
                icons::PENCIL,
                "Rename",
                UiAction::FilesAskRename(id, side, name.clone()),
                false,
            ));
        }
        items.push(menu_item(
            icons::FOLDER,
            "New folder",
            UiAction::FilesAskNewFolder(id, side),
            false,
        ));
        items.push(MenuItem::Separator);
        items.push(menu_item(
            icons::TRASH_2,
            "Delete",
            UiAction::FilesAskDelete(id, side, names),
            true,
        ));
        items
    }

    /// What can be done from the vault switcher.
    pub(super) fn vault_menu(&self) -> Vec<MenuItem> {
        let ui = &self.core.ui;
        let row = |glyph, path: &str| {
            menu_item(
                glyph,
                &shell::short_path(path),
                UiAction::VaultSwitch(path.to_owned()),
                false,
            )
        };
        let mut items = vec![row(icons::CHECK, &ui.vault_path)];
        items.extend(ui.vault_others.iter().map(|path| row(icons::KEY_ROUND, path)));
        items.push(MenuItem::Separator);
        items.push(menu_item(
            icons::FOLDER_OPEN,
            "Open vault",
            UiAction::VaultAskPath { create: false },
            false,
        ));
        items.push(menu_item(
            icons::PLUS,
            "New vault",
            UiAction::VaultAskPath { create: true },
            false,
        ));
        items.push(menu_item(icons::FOLDER_SYNC, "Move vault", UiAction::VaultAskMove, false));
        items
    }
}
