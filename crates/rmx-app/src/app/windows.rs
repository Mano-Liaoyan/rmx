//! What takes more than one window: moving tabs between them, opening and closing them, and
//! the session they make up.

use super::*;

impl Core {
    /// Free content off this thread: its scrollback can take long to give back.
    pub(super) fn retire(&self, content: PaneContent) {
        match content {
            // Its GL objects must go on this thread, where a context of the group is current.
            PaneContent::Display(display) => drop(display),
            PaneContent::Terminal(terminal) => {
                self.runtime.spawn_blocking(move || drop(terminal));
            },
            PaneContent::Files(mut files) => {
                files.stop();
                self.runtime.spawn_blocking(move || drop(files));
            },
            PaneContent::Starting => (),
        }
    }

    /// When anything that is not a frame next needs doing.
    pub(super) fn next_op_wake(&self) -> Option<Instant> {
        let reopen = (!self.reopening.is_empty()).then(|| Instant::now() + PREPARE_POLL);
        [self.landing.as_ref().map(|landing| landing.deadline), self.session_due, reopen]
            .into_iter()
            .flatten()
            .min()
    }

    /// The windows changed: write them down before long, unless there is still a session waiting
    /// to be reopened, which is what the state file has to go on saying.
    pub(super) fn session_changed(&mut self) {
        let waiting = self.restoring.is_some() || !self.reopening.is_empty();
        if !waiting && self.session_due.is_none() {
            self.session_due = Some(Instant::now() + SESSION_SAVE_AFTER);
        }
    }

    /// How a pane goes in the state file.
    pub(super) fn saved_pane(&self, id: PaneId) -> SavedPane {
        let remote = self.panes.iter().find(|pane| pane.id == id).and_then(|p| p.remote.as_ref());
        match remote.map(|remote| (remote.node(), &remote.goal)) {
            None => SavedPane::Local,
            Some((None, _)) => SavedPane::Transient,
            Some((Some(node), Goal::Files { .. })) => SavedPane::Files(node),
            Some((Some(node), Goal::Session)) => SavedPane::Node(node),
        }
    }

    /// The unsaved connection `origin` was saved as `node`: its panes, in any window, are that
    /// connection's from now on. Their sessions carry on as they are.
    pub(super) fn adopt(&mut self, node: u64, origin: &Arc<ConnectionDraft>) {
        let name = self.ui.nodes.get(&node).map(|info| info.name.clone());
        let mut any = false;
        for pane in &mut self.panes {
            let Some(remote) = pane.remote.as_mut() else { continue };
            if matches!(&remote.source, Origin::Transient(draft) if Arc::ptr_eq(draft, origin)) {
                remote.source = Origin::Saved(node);
                if let Some(name) = &name {
                    remote.name.clone_from(name);
                }
                pane.dirty = true;
                any = true;
            }
        }
        if any {
            self.ui.tree.set_state(NodeId(node), shell::session_dot(&self.panes, node));
            self.broadcast = true;
            self.session_changed();
        }
    }
}

impl Running {
    /// A terminal has new output.
    pub(super) fn wakeup(&mut self, id: PaneId) {
        let Self { core, windows } = self;
        let Some(win) =
            windows.iter_mut().find(|win| win.rects.iter().any(|&(pane, _)| pane == id))
        else {
            return;
        };
        if let Some(pane) = core.panes.iter_mut().find(|pane| pane.id == id) {
            pane.dirty = true;
            if win.pacer.mark_dirty() {
                win.gfx.window.request_redraw();
            }
        }
    }

    /// Do what took more than one window, once the event that asked for it is over.
    pub(super) fn settle(&mut self, event_loop: &ActiveEventLoop) {
        for op in std::mem::take(&mut self.core.ops) {
            match op {
                Op::SetTheme(mode) => self.set_theme(mode),
                Op::Tear { from, what } => {
                    if let Some(mut at) = self.index_of(from).and_then(|index| self.at_index(index))
                    {
                        at.send_out(what, false);
                    }
                },
                Op::Move { from, what, to, at } => self.move_between(from, what, to, at),
            }
        }
        self.land(event_loop);
        self.reopen_next(event_loop);
        if self.core.session_due.is_some_and(|due| due <= Instant::now()) {
            self.core.session_due = None;
            if let Some(text) = self.session_text() {
                self.core.vault.send(Request::SaveSession(text));
            }
        }
        if std::mem::take(&mut self.core.broadcast) {
            for win in &mut self.windows {
                win.chrome.invalidate();
                if win.pacer.mark_dirty() {
                    win.gfx.window.request_redraw();
                }
            }
        }
        self.close_finished(event_loop);
    }

    /// What to write down about the windows, unless a session is still waiting to be reopened.
    pub(super) fn session_text(&self) -> Option<String> {
        #[cfg(feature = "bench")]
        if self.core.vaultless {
            return None;
        }
        if self.core.restoring.is_some() || !self.core.reopening.is_empty() {
            return None;
        }
        let core = &self.core;
        let windows: Vec<SavedWindow<SavedPane>> = self
            .windows
            .iter()
            // The last window stays with nothing in it, which is how it comes back. But only
            // once there has been something in it: before that, nothing is what it starts with.
            .filter(|win| {
                !win.workspace.tabs().is_empty() || (self.windows.len() == 1 && core.opened_first_tab)
            })
            .map(|win| {
                let (width, height) = win.logical_size();
                let tabs = win.workspace.tabs().iter().map(|tab| {
                    let order = tab.layout.panes();
                    let focus = order.iter().position(|&id| id == tab.focus).unwrap_or(0);
                    let layout = tab.layout.clone().map(&mut |id| core.saved_pane(id));
                    SavedTab { layout, focus, zoomed: tab.zoomed, name: tab.name.clone() }
                });
                SavedWindow {
                    width,
                    height,
                    maximized: win.maximized,
                    sidebar_open: win.shell.sidebar_open,
                    active: win.workspace.active_index(),
                    tabs: tabs.collect(),
                }
            })
            .collect();
        Some(session::encode(&windows, |pane| pane.spell()))
    }

    /// The vault is open and what it holds is known: reopen the windows of the last session.
    pub(super) fn restore_session(&mut self) {
        let Some(session) = self.core.restoring.take() else { return };
        let scale = self.windows[0].gfx.window.scale_factor();
        let theme = self.core.theme;
        let left_empty = session.first().is_some_and(|window| window.tabs.is_empty());
        let mut windows = session.into_iter();
        if let Some(first) = windows.next()
            && let Some(mut at) = self.at_index(0)
        {
            at.restore(first);
        }
        self.core.reopening.extend(
            windows
                .map(|window| (Early::start(scale, (window.width, window.height), theme), window)),
        );
        // Nothing left of what it held: as if there had been nothing saved. Unless that is how
        // it was left.
        if !left_empty
            && self.windows[0].workspace.tabs().is_empty()
            && !self.core.opened_first_tab
            && let Some(mut at) = self.at_index(0)
        {
            at.perform(Action::NewTab);
        }
        self.core.opened_first_tab = true;
        self.core.session_changed();
    }

    /// Open the next window of the last session, if what it takes is ready. Only one at a time:
    /// making a window keeps the others waiting.
    pub(super) fn reopen_next(&mut self, event_loop: &ActiveEventLoop) {
        if !self.core.reopening.front().is_some_and(|(early, _)| early.finished()) {
            return;
        }
        let Some((early, saved)) = self.core.reopening.pop_front() else { return };
        match self.open_window(event_loop, early, (saved.width, saved.height), saved.maximized) {
            Ok(index) => {
                if let Some(mut at) = self.at_index(index) {
                    at.restore(saved);
                    if at.win.workspace.tabs().is_empty() {
                        at.win.exit = true;
                    }
                }
            },
            Err(err) => log::error!("Could not reopen a window: {err}"),
        }
        self.core.session_changed();
    }

    /// Change the theme of everything drawn.
    pub(super) fn set_theme(&mut self, mode: ThemeMode) {
        if self.core.theme == mode {
            return;
        }
        self.core.theme = mode;
        let Self { core, windows } = self;
        for win in windows.iter_mut() {
            win.chrome.set_theme(mode);
            win.gfx.set_theme(mode);
            win.chrome.invalidate();
            win.full = true;
            if win.pacer.mark_dirty() {
                win.gfx.window.request_redraw();
            }
        }
        let theme = *windows[0].gfx.theme();
        for pane in &mut core.panes {
            pane.content.set_theme(theme);
            pane.dirty = true;
        }
        if let Some(at) = self.at_index(0) {
            let layout = at.layout();
            at.core.vault.send(Request::SaveLayout(layout));
        }
    }

    /// Decide on the windows whose last tab went. One at a time, so that two emptied by the same
    /// batch of events do not each count on the other to stay.
    fn settle_vacated(&mut self) {
        while let Some(index) = self.windows.iter().position(|win| win.vacated.is_some()) {
            let when_last = self.windows[index].vacated.take();
            if !self.windows[index].workspace.tabs().is_empty() {
                continue;
            }
            let others = self.windows.iter().enumerate().any(|(at, win)| at != index && !win.exit);
            if others || when_last == Some(WhenLast::Quit) {
                self.windows[index].exit = true;
            }
        }
    }

    /// Windows that have ended, and with them the application once none are left.
    pub(super) fn close_finished(&mut self, event_loop: &ActiveEventLoop) {
        self.settle_vacated();
        while let Some(index) = self.windows.iter().position(|win| win.exit) {
            if self.windows.len() == 1 {
                // Stays for `exiting`, which saves what it shows.
                event_loop.exit();
                return;
            }
            if let Some(mut at) = self.at_index(index) {
                at.close_all();
            }
            let window = self.windows.remove(index);
            self.core.window_keys.retain(|&key| key != window.key);
            self.core.ui.asking_window = match self.core.ui.asking_window {
                key if key == window.key => self.windows[0].key,
                key => key,
            };
            drop(window);
            // Whichever context was current may just have gone.
            self.windows[0].gfx.make_current();
            let key = self.windows[0].key;
            if !self.windows.iter().any(|win| win.key == self.core.active) {
                self.core.active = key;
            }
        }
    }

    /// The vault's thread has something to say.
    pub(super) fn vault_event(&mut self, event: VaultEvent) {
        #[cfg(feature = "bench")]
        if self.core.vaultless {
            return;
        }
        self.core.broadcast = true;
        match event {
            VaultEvent::Phase(phase) => {
                let was_locked = self.core.ui.locked();
                self.core.ui.gate = Gate::from(&phase);
                if let store::Phase::Unlocked { unencrypted, key_file } = &phase {
                    self.core.ui.unencrypted = *unencrypted;
                    self.core.ui.vault_key_file = key_file
                        .as_ref()
                        .map_or_else(String::new, |path| path.display().to_string());
                }
                // An import cannot be saved behind the lock: it is given up on.
                if self.core.ui.locked() && matches!(self.core.answer, Some((Reply::Import(_), _)))
                {
                    self.core.answer = None;
                    self.core.ui.asking = None;
                }
                self.core.ui.asking_hidden =
                    self.core.ui.locked() && matches!(self.core.answer, Some((_, Some(_))));
                self.core.ui.vault_busy = false;
                self.core.ui.vault_error = None;
                if self.core.ui.locked() {
                    for pane in &mut self.core.panes {
                        pane.content.set_focused(false);
                    }
                    for index in 0..self.windows.len() {
                        let Some(mut at) = self.at_index(index) else { continue };
                        // The dialogs say what is connected to what.
                        at.win.shell.quick.close();
                        at.win.shell.settings.close();
                        at.win.shell.dialog.close();
                        at.win.shell.snippets.close();
                        at.relayout(false);
                    }
                } else if was_locked {
                    if !self.core.opened_first_tab
                        && self.core.restoring.is_none()
                        && self.windows[0].workspace.tabs().is_empty()
                    {
                        self.core.opened_first_tab = true;
                        if let Some(mut at) = self.at_index(0) {
                            at.perform(Action::NewTab);
                        }
                    }
                    for index in 0..self.windows.len() {
                        if let Some(mut at) = self.at_index(index) {
                            at.relayout(false);
                        }
                    }
                    if let Some(mut at) = self.at_active() {
                        at.next_prompt();
                    }
                }
            },
            VaultEvent::OpenFailed(reason) => {
                self.core.ui.vault_busy = false;
                self.core.ui.vault_error = Some(reason);
            },
            VaultEvent::Tree(update) => {
                let Core { ui, panes, .. } = &mut self.core;
                ui.set_tree(*update, panes);
                self.restore_session();
            },
            VaultEvent::Sync(state) => {
                if matches!(state, SyncState::Running) {
                    self.core.ui.status_note = None;
                }
                self.core.ui.sync = state;
            },
            VaultEvent::Vaults { current, others } => {
                self.core.ui.vault_path = current.display().to_string();
                self.core.ui.vault_others =
                    others.iter().map(|path| path.display().to_string()).collect();
            },
            VaultEvent::Notice(text, bad) => self.core.ui.status_note = Some((text, bad)),
            VaultEvent::PasswordOutcome(title, detail, bad) => {
                self.core.ui.status_note = None;
                if let Some(mut at) = self.at_active() {
                    at.tell(&title, &detail, bad);
                }
            },
            VaultEvent::RemoteKeyChanged => {
                if let Some(mut at) = self.at_active() {
                    at.ask_remote_password();
                }
            },
            VaultEvent::Settings(mut draft) => {
                draft.confirm_close = self.core.confirm_close;
                if let Some(at) = self.at_active() {
                    at.win.shell.settings.open(*draft);
                }
            },
            VaultEvent::FolderLogin { node, folder, username, password, key } => {
                if let Some(mut at) = self.at_active() {
                    at.ask_folder_login(node, &folder, username, password, key);
                }
            },
            VaultEvent::Draft(draft) => {
                if let Some(at) = self.at_active() {
                    at.win.adopting = None;
                    at.win.shell.dialog.open_draft(*draft);
                }
            },
            VaultEvent::Saved { node, connect } => {
                if connect && let Some(mut at) = self.at_active() {
                    at.open_connection(node);
                }
            },
            VaultEvent::Adopted { node, origin } => self.core.adopt(node, &origin),
        }
    }

    /// The pointer showed up in window `id`. If a tab is waiting to land, it has landed there.
    pub(super) fn note_pointer_arrival(&mut self, id: WindowId, event: &WindowEvent) {
        let WindowEvent::CursorMoved { position, .. } = event else { return };
        let Some(landing) = self.core.landing.as_ref().filter(|landing| landing.claimable) else {
            return;
        };
        let from = landing.from;
        let Some(win) = self.windows.iter().find(|win| win.gfx.window.id() == id) else { return };
        if win.key == from {
            return;
        }
        let scale = win.gfx.window.scale_factor();
        let (to, at) = (win.key, win.shell.tab_slot((position.x / scale) as f32));
        if let Some(landing) = self.core.landing.take() {
            self.core.ops.push(Op::Move { from, what: landing.what, to, at });
        }
    }

    /// Give what nothing claimed in time a window of its own.
    pub(super) fn land(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let Some(landing) = self.core.landing.as_mut().filter(|landing| landing.deadline <= now)
        else {
            return;
        };
        if landing.early.as_ref().is_some_and(|early| !early.finished()) {
            landing.deadline = now + PREPARE_POLL;
            return;
        }
        let Some(landing) = self.core.landing.take() else { return };
        let Landing { from, what, early, .. } = landing;
        let (Some(early), Some(index)) = (early, self.index_of(from)) else { return };
        let (size, maximized) = (self.windows[index].logical_size(), false);
        let start = Instant::now();
        match self.open_window(event_loop, early, size, maximized) {
            Ok(new) => {
                let to = self.windows[new].key;
                self.move_between(from, what, to, 0);
                log::info!("A new window took {:?} on the main thread", start.elapsed());
                #[cfg(feature = "bench")]
                crate::bench::note_window_open(start.elapsed());
            },
            Err(err) => {
                // It never left: nothing has happened to it.
                log::error!("Could not open a window for the tab: {err}");
                self.core.ui.status_note = Some((format!("Could not open a window: {err}"), true));
                self.core.broadcast = true;
            },
        }
    }

    /// Make a window, on the main thread like all EGL work, from what a thread prepared.
    pub(super) fn open_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        early: Early,
        size: (f64, f64),
        maximized: bool,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let window = event_loop.create_window(window_attributes(size, maximized))?;
        window.set_ime_allowed(true);
        let key = self.core.next_window;
        let (gfx, chrome, mut shell) =
            Gfx::new(window, early, self.core.theme, key, Some(&self.windows[0].gfx))?;
        self.core.next_window += 1;
        // The tree is the first window's; a torn-off tab gets all of its own.
        shell.sidebar_open = false;
        shell.sidebar_width = self.core.ui.panels.sidebar;
        self.windows.push(Win::new(key, gfx, chrome, shell, self.core.wayland));
        self.core.window_keys.push(key);
        Ok(self.windows.len() - 1)
    }

    /// Move a tab or pane to another window, before its tab at index `at`.
    pub(super) fn move_between(&mut self, from: WinKey, what: Moved, to: WinKey, at: usize) {
        let (Some(source), Some(target)) = (self.index_of(from), self.index_of(to)) else { return };
        if source == target {
            return;
        }
        let Some(tab) = self.windows[source].take(what) else { return };
        let panes = tab.layout.panes();
        for pane in self.core.panes.iter_mut().filter(|pane| panes.contains(&pane.id)) {
            pane.window = to;
            pane.dirty = true;
        }
        if let Some(mut at_source) = self.at_index(source) {
            at_source.departed(&panes);
        }
        self.core.active = to;
        if let Some(mut at_target) = self.at_index(target) {
            at_target.win.workspace.insert_tab(tab, at);
            at_target.relayout(false);
            at_target.win.gfx.window.focus_window();
        }
        self.core.broadcast = true;
    }
}
