//! Session lifecycle: starting a pane's program or connection, its events, and its end.

use super::*;

impl At<'_> {
    /// Start a terminal for the pane `id`, which is already in the workspace and stays blank
    /// until it arrives. Starting a process takes too long for the main thread.
    pub(super) fn spawn(&mut self, id: PaneId) -> bool {
        let (area, spacing) = (self.area(), self.spacing());
        let rect =
            self.win.workspace.rects(area, spacing).into_iter().find(|&(pane, _)| pane == id);
        let size = self.win.gfx.size_for(self.terminal_rect(rect.map_or(area, |(_, rect)| rect)));

        let sink: Arc<dyn Sink> = self.core.sink.clone();
        let proxy = self.core.sink.0.clone();
        let theme = *self.win.gfx.theme();
        let spawn = move || {
            let shell = startup_shell();
            #[cfg(feature = "bench")]
            let started = Instant::now();
            let terminal = Terminal::spawn(id, size, shell, theme, sink);
            #[cfg(feature = "bench")]
            crate::bench::present::note("pane", || {
                format!("terminal {id} spawned in {}us", started.elapsed().as_micros())
            });
            let _ = proxy.send_event(AppEvent::Spawned(id, Box::new(terminal)));
        };
        match std::thread::Builder::new().spawn(spawn) {
            Ok(_) => true,
            Err(err) => {
                log::error!("Could not start a terminal: {err}");
                false
            },
        }
    }

    /// A terminal has arrived from the thread that started it.
    pub(super) fn spawned(&mut self, id: PaneId, terminal: io::Result<Terminal>) {
        let mut terminal = match terminal {
            Ok(terminal) => terminal,
            Err(err) => {
                log::error!("Could not start a terminal: {err}");
                self.program_ended(id);
                return;
            },
        };

        // Closed while it started.
        if !self.win.workspace.contains(id) {
            self.retire(PaneContent::Terminal(Box::new(terminal)));
            return;
        }
        if let Some(index) = self.core.ended_early.iter().position(|&pane| pane == id) {
            self.core.ended_early.remove(index);
            self.program_ended(id);
            return;
        }

        // The layout may have changed while it started.
        let (area, spacing) = (self.area(), self.spacing());
        if let Some((_, rect)) =
            self.win.workspace.rects(area, spacing).into_iter().find(|&(pane, _)| pane == id)
        {
            terminal.resize(self.win.gfx.size_for(self.terminal_rect(rect)));
        }
        if let Some(pane) = self.pane_mut(id) {
            pane.content = PaneContent::Terminal(Box::new(terminal));
            pane.dirty = true;
        }
        self.sync_focus(Instant::now());
        self.request_frame();
    }

    /// The program of pane `id` ended by itself, or could not start.
    ///
    /// `rmx -e` is a terminal for that one program, as `scripts/headless.sh` and the throughput
    /// bench rely on: once it is over and nothing else is showing, so is the application.
    pub(super) fn program_ended(&mut self, id: PaneId) {
        let local = self.pane(id).is_some_and(|pane| pane.remote.is_none());
        self.close(id);
        if local && self.win.workspace.tabs().is_empty() && startup_shell().is_some() {
            self.win.vacated = Some(WhenLast::Quit);
        }
    }

    pub(super) fn retire(&self, content: PaneContent) {
        self.core.retire(content);
    }

    /// Rows a file pane no longer shows, which are too many to free on this thread.
    pub(super) fn retire_rows(&self, rows: Option<Arc<Vec<rmx_ui::widgets::FileRow>>>) {
        if let Some(rows) = rows {
            self.core.runtime.spawn_blocking(move || drop(rows));
        }
    }

    /// News from the tasks behind file pane `id`.
    pub(super) fn files_event(&mut self, id: PaneId, generation: u64, event: FilesEvent) {
        let Some(files) = self.pane_mut(id).and_then(|pane| pane.content.files_mut()) else {
            return;
        };
        if files.generation() != generation {
            return;
        }
        let applied = files.apply(event);
        if let Some(side) = applied.moved {
            self.core.ui.files_reset(id, side);
        }
        if let Some(path) = &applied.conflict {
            let mut state =
                PromptState::new(format!("{path} has changed since you opened it"), "", Vec::new());
            state.submit = "Overwrite".to_owned();
            self.ask_here(state, Reply::FilesOverwrite(id));
        }
        if let Some(files) = self.pane_mut(id).and_then(|pane| pane.content.files_mut()) {
            for (side, directory) in &applied.refresh {
                files.refresh_if_showing(*side, directory);
            }
        }
        self.retire_rows(applied.retired);
        self.win.chrome.invalidate();
        self.request_frame();
    }

    /// Open the saved connection `node` in a new tab, which shows at once that it is connecting.
    pub(super) fn open_connection(&mut self, node: u64) {
        self.open_node(node, Goal::Session);
    }

    pub(super) fn open_node(&mut self, node: u64, goal: Goal) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        if info.kind == NodeKind::Folder {
            return;
        }
        let name = info.name.clone();
        let id = self.new_id();
        self.add_pane(Pane::remote(id, node, name, goal));
        self.win.workspace.add_tab(id);
        self.start_session(id);
        self.relayout(false);
    }

    /// Start connecting the remote pane `id`, whose earlier screen stays until there is a new one.
    pub(super) fn start_session(&mut self, id: PaneId) {
        #[cfg(feature = "bench")]
        crate::bench::present::note("pane", || format!("session {id}"));
        let (area, spacing) = (self.area(), self.spacing());
        let rect =
            self.win.workspace.rects(area, spacing).into_iter().find(|&(pane, _)| pane == id);
        let content = self.terminal_rect(rect.map_or(area, |(_, rect)| rect));
        let size = self.win.gfx.size_for(content);
        let pixels = (content.w as u32, content.h as u32);
        let theme = *self.win.gfx.theme();
        let vault = self.core.vault.clone();
        let proxy = self.core.proxy.clone();
        let sink: Arc<dyn Sink> = self.core.sink.clone();
        let runtime = self.core.runtime.handle().clone();

        let Some(pane) = self.pane_mut(id) else { return };
        let Some(remote) = pane.remote.as_mut() else { return };
        let (cancel_sender, cancel) = oneshot::channel();
        remote.generation += 1;
        remote.cancel = Some(cancel_sender);
        remote.forwards = 0;
        let request = ConnectRequest {
            pane: id,
            generation: remote.generation,
            node: remote.node,
            size,
            pixels,
            goal: remote.goal.clone(),
            theme,
            vault,
            proxy,
            sink,
            runtime: runtime.clone(),
            cancel,
        };
        pane.link = Link::Connecting { stage: "Reading the vault".to_owned() };
        pane.notice = None;
        pane.dirty = true;
        runtime.spawn(connect::run(request));
        self.link_changed(id);
    }

    /// Give up on what the pane `id` is doing, leaving the pane.
    pub(super) fn hang_up(&mut self, id: PaneId, reason: &str) {
        self.dismiss_prompts(id);
        let Some(pane) = self.pane_mut(id) else { return };
        pane.hang_up();
        if matches!(pane.link, Link::Connecting { .. }) {
            pane.link = Link::Failed { reason: reason.to_owned() };
        }
        self.link_changed(id);
    }

    /// News of a connection attempt.
    pub(super) fn link_event(&mut self, id: PaneId, generation: u64, event: LinkEvent) {
        let current = self
            .pane(id)
            .and_then(|pane| pane.remote.as_ref())
            .is_some_and(|remote| remote.generation == generation);
        if !current {
            match event {
                LinkEvent::Ready(Ready::Terminal(terminal)) => {
                    self.retire(PaneContent::Terminal(terminal));
                },
                // Nothing of these is on the GPU yet, and dropping them ends the session.
                LinkEvent::Ready(Ready::Display(_) | Ready::Files(_)) => (),
                _ => (),
            }
            return;
        }
        match event {
            LinkEvent::Stage(stage) => {
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Connecting { stage };
                }
            },
            LinkEvent::Target(target) => {
                if let Some(remote) = self.pane_mut(id).and_then(|pane| pane.remote.as_mut()) {
                    remote.target = target;
                }
            },
            LinkEvent::Ready(ready) => {
                // The window may have changed while it connected.
                let (area, spacing) = (self.area(), self.spacing());
                let rect = self.win.workspace.rects(area, spacing).into_iter().find(|r| r.0 == id);
                let content = match ready {
                    Ready::Terminal(mut terminal) => {
                        if let Some((_, rect)) = rect {
                            terminal.resize(self.win.gfx.size_for(self.terminal_rect(rect)));
                        }
                        PaneContent::Terminal(terminal)
                    },
                    Ready::Display(link) => {
                        let mut display = Display::new(*link);
                        if let Some((_, rect)) = rect {
                            let settled = self.terminal_rect(rect);
                            display.pane_resized(
                                settled.w as u32,
                                settled.h as u32,
                                Instant::now(),
                            );
                        }
                        PaneContent::Display(Box::new(display))
                    },
                    Ready::Files(link) => {
                        let (runtime, proxy) =
                            (self.core.runtime.handle().clone(), self.core.proxy.clone());
                        PaneContent::Files(Box::new(FilesPane::new(
                            id,
                            generation,
                            *link,
                            files::local_home(),
                            runtime,
                            proxy,
                        )))
                    },
                };
                let Some(pane) = self.pane_mut(id) else { return };
                let old = std::mem::replace(&mut pane.content, content);
                pane.link = Link::Live;
                pane.dirty = true;
                self.retire(old);
                self.log_new_terminal(id);
                self.sync_focus(Instant::now());
            },
            LinkEvent::Missing(why) => {
                self.dismiss_prompts(id);
                if let Some(pane) = self.pane_mut(id) {
                    let quiet = pane.remote.as_ref().is_some_and(|remote| remote.quiet);
                    pane.link =
                        if quiet { Link::Awaiting { why } } else { Link::Failed { reason: why } };
                    pane.hang_up();
                }
            },
            LinkEvent::Closed(reason) => {
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Closed { reason };
                    pane.hang_up();
                }
            },
            LinkEvent::Failed(reason) => {
                self.dismiss_prompts(id);
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Failed { reason };
                    pane.hang_up();
                }
            },
            LinkEvent::Logging(choice) => {
                if let Some(remote) = self.pane_mut(id).and_then(|pane| pane.remote.as_mut()) {
                    remote.log = choice;
                }
            },
            LinkEvent::Forwards(running, errors) => {
                if let Some(pane) = self.pane_mut(id) {
                    if let Some(remote) = pane.remote.as_mut() {
                        remote.forwards = running;
                    }
                    if !errors.is_empty() {
                        pane.notice = Some(format!("Port forward failed: {}", errors.join("; ")));
                    }
                }
            },
        }
        self.link_changed(id);
    }

    /// What happened to the session of a terminal.
    pub(super) fn session_event(&mut self, id: PaneId, event: SessionEvent) {
        match event {
            SessionEvent::OutputDiscarded(bytes) => {
                if let Some(pane) = self.pane_mut(id) {
                    let amount = if bytes >= MIB {
                        format!("{} MiB", bytes / MIB)
                    } else {
                        format!("{} KiB", bytes.div_ceil(1024))
                    };
                    pane.notice =
                        Some(format!("Output could not keep up: {amount} of it was lost"));
                }
                self.link_changed(id);
            },
            SessionEvent::Closed(reason) => {
                if let Some(pane) = self.pane_mut(id) {
                    pane.link = Link::Closed { reason: reason.to_string() };
                    pane.hang_up();
                }
                self.link_changed(id);
            },
            _ => (),
        }
    }
}
