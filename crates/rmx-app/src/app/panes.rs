//! Pane and tab actions: restoring, splitting, showing, sending out and closing them.

use super::*;

impl At<'_> {
    /// Lay this window out as it was left, starting what was in it. What no longer exists is
    /// left out.
    pub(super) fn restore(&mut self, saved: SavedWindow<SavedPane>) {
        let SavedWindow { sidebar_open, maximized, active, tabs, .. } = saved;
        self.win.shell.sidebar_open = sidebar_open;
        self.win.maximized = maximized;
        let mut start = Vec::new();
        let active_id = active;
        for (index, tab) in tabs.into_iter().enumerate() {
            let SavedTab { layout, focus, zoomed, name } = tab;
            let mut gone = Vec::new();
            let order = layout.panes();
            let focused = order.get(focus).copied();
            let mut focus_id = None;
            let layout = layout.map(&mut |saved| {
                let id = self.new_id();
                if Some(saved) == focused && focus_id.is_none() {
                    focus_id = Some(id);
                }
                match saved {
                    // Never saved, so nothing to reopen: left out like a deleted one.
                    SavedPane::Transient => gone.push(id),
                    SavedPane::Local => {
                        self.add_pane(Pane::local(id));
                        start.push(id);
                    },
                    SavedPane::Node(node) | SavedPane::Files(node) => {
                        let goal = match saved {
                            SavedPane::Files(_) => Goal::Files { directory: None },
                            _ => Goal::Session,
                        };
                        match self.core.ui.nodes.get(&node).map(|info| info.name.clone()) {
                            Some(name) => {
                                self.add_pane(Pane::remote(id, Origin::Saved(node), name, goal));
                                start.push(id);
                            },
                            None => gone.push(id),
                        }
                    },
                }
                id
            });
            let mut remaining = Some(layout);
            for id in gone {
                remaining = remaining.and_then(|layout| layout.remove(id));
            }
            if let Some(layout) = remaining {
                let panes = layout.panes();
                let focus = focus_id.filter(|id| panes.contains(id)).unwrap_or(panes[0]);
                self.win.workspace.restore_tab(layout, focus, zoomed, name, index == active_id);
            }
        }
        for id in start {
            let is_remote = self.pane(id).is_some_and(|pane| pane.remote.is_some());
            if is_remote {
                if let Some(remote) = self.pane_mut(id).and_then(|pane| pane.remote.as_mut()) {
                    remote.quiet = true;
                }
                self.start_session(id);
            } else if !self.spawn(id) {
                self.close(id);
            }
        }
        self.relayout(false);
    }

    /// Count `pane` as this window's.
    pub(super) fn add_pane(&mut self, mut pane: Pane) {
        #[cfg(feature = "bench")]
        crate::bench::present::note("pane", || format!("added {}", pane.id));
        pane.window = self.win.key;
        self.core.panes.push(pane);
    }

    pub(super) fn new_id(&mut self) -> PaneId {
        self.core.next_id += 1;
        self.core.next_id
    }

    pub(super) fn focused_terminal(&self) -> Option<&Terminal> {
        let id = self.win.workspace.focused()?;
        self.pane(id)?.content.terminal()
    }

    pub(super) fn pane_is_ssh(&self, pane: PaneId) -> bool {
        self.pane(pane)
            .and_then(|found| found.remote.as_ref())
            .and_then(|remote| remote.protocol(&self.core.ui.nodes))
            == Some(Protocol::Ssh)
    }

    /// Open the session of `pane` again, to the same connection and for the same thing.
    pub(super) fn clone_of(&mut self, pane: PaneId) -> Option<(Origin, String, Goal)> {
        let remote = self.pane(pane)?.remote.as_ref()?;
        let name = match &remote.source {
            Origin::Saved(node) => self.core.ui.nodes.get(node)?.name.clone(),
            Origin::Transient(_) => remote.name.clone(),
        };
        Some((remote.source.clone(), name, remote.goal.clone()))
    }

    /// Put a new pane next to the focused one: the same session where another one makes sense,
    /// else a local shell.
    pub(super) fn split_focused(&mut self, axis: Axis) {
        let focused = self.win.workspace.focused();
        let protocol = focused
            .and_then(|pane| self.pane(pane))
            .and_then(|pane| pane.remote.as_ref())
            .and_then(|remote| remote.protocol(&self.core.ui.nodes));
        let same = focused.and_then(|pane| self.clone_of(pane)).filter(|(_, _, goal)| {
            *goal != Goal::Session
                || matches!(
                    protocol,
                    Some(Protocol::Ssh | Protocol::Telnet | Protocol::Sftp | Protocol::Ftp)
                )
        });
        let Some((source, name, goal)) = same else {
            self.split_local(axis);
            return;
        };
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::remote(id, source, name, goal));
            self.start_session(id);
            self.relayout(true);
        }
    }

    pub(super) fn split_local(&mut self, axis: Axis) {
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::local(id));
            if self.spawn(id) {
                self.relayout(true);
            } else {
                self.close(id);
            }
        }
    }

    /// Open the same session as `pane` again, in a tab of its own.
    pub(super) fn duplicate_session(&mut self, pane: PaneId) {
        match self.clone_of(pane) {
            Some((source, name, goal)) => self.open_source(source, name, goal),
            None => self.perform(Action::NewTab),
        }
    }

    /// Open the saved connection `node` beside the focused pane, or in a tab with none to go beside.
    pub(super) fn open_node_split(&mut self, node: u64, axis: Axis) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        if info.kind == NodeKind::Folder {
            return;
        }
        let name = info.name.clone();
        let id = self.new_id();
        if self.win.workspace.split(id, axis) {
            self.add_pane(Pane::remote(id, Origin::Saved(node), name, Goal::Session));
            self.start_session(id);
            self.relayout(true);
        } else {
            self.open_node(node, Goal::Session);
        }
    }

    /// The panes of this window connected to `node`, most recent last.
    pub(super) fn sessions_of(&self, node: u64) -> impl Iterator<Item = &Pane> {
        let key = self.win.key;
        self.core.panes.iter().filter(move |pane| {
            pane.window == key && pane.remote.as_ref().is_some_and(|r| r.node() == Some(node))
        })
    }

    /// Bring `pane` into view, with the keyboard.
    pub(super) fn show_pane(&mut self, pane: PaneId) {
        let tab =
            self.win.workspace.tabs().iter().position(|tab| tab.layout.panes().contains(&pane));
        if let Some(tab) = tab {
            self.win.workspace.select(tab);
            self.win.workspace.focus(pane);
            self.relayout(false);
        }
    }

    /// What `source` holds has been let go outside the window: see [`Landing`].
    pub(super) fn pointer_left_with(&mut self, source: Source, pane: PaneId) {
        let tab = match source {
            Source::Tab(tab) | Source::Pane(tab, _) => tab,
        };
        let what = match source {
            Source::Tab(_) => self.win.workspace.tabs().get(tab).map(|tab| Moved::Tab(tab.id)),
            Source::Pane(..) => Some(Moved::Pane(pane)),
        };
        if let Some(what) = what {
            self.send_out(what, true);
        }
    }

    /// Start moving `what` out of this window, to another one if `claimable` and one claims it,
    /// else to one of its own, if this window would still have something to show.
    pub(super) fn send_out(&mut self, what: Moved, claimable: bool) {
        let tabs = self.win.workspace.tabs();
        let can_have_own = match what {
            Moved::Tab(_) => tabs.len() > 1,
            Moved::Pane(pane) => {
                tabs.len() > 1
                    || tabs.first().is_some_and(|tab| {
                        tab.layout.panes().len() > 1 && tab.layout.panes().contains(&pane)
                    })
            },
        };
        let (scale, size) = (self.win.gfx.window.scale_factor(), self.win.logical_size());
        let early = can_have_own.then(|| Early::start(scale, size, self.core.theme));
        // A window ought to show at least the one tab: left alone it just stays, unless claimed.
        self.core.landing = Some(Landing {
            from: self.win.key,
            what,
            deadline: Instant::now() + claim_wait(),
            claimable,
            early,
        });
    }

    /// `panes` have left for another window, leaving this one maybe empty.
    pub(super) fn departed(&mut self, panes: &[PaneId]) {
        if self.win.captured.is_some_and(|id| panes.contains(&id)) {
            self.win.captured = None;
        }
        if self.win.workspace.tabs().is_empty() {
            self.win.exit = true;
        } else {
            self.relayout(false);
        }
    }

    /// Close everything in this window, which is ending.
    pub(super) fn close_all(&mut self) {
        let panes: Vec<PaneId> =
            self.win.workspace.tabs().iter().flat_map(|tab| tab.layout.panes()).collect();
        self.win.gfx.make_current();
        for pane in panes {
            self.remove_pane(pane);
        }
    }

    /// Land a dragged pane.
    pub(super) fn drop_pane(&mut self, drop: PaneDrop) {
        let PaneDrop { pane, source, target } = drop;
        let (area, spacing, min) = (self.area(), self.spacing(), self.min_pane());
        let animate = match target {
            Target::Dock { pane: at, axis, side } => {
                let onto =
                    self.win.workspace.active().and_then(|tab| tab.layout.panes().get(at).copied());
                onto.is_some_and(|onto| self.win.workspace.dock(pane, onto, axis, side))
            },
            Target::Merge(into) => {
                self.win.workspace.merge(pane, into, area, min, spacing);
                false
            },
            Target::Detach(slot) => {
                self.win.workspace.detach(pane, slot);
                false
            },
            Target::Reorder(slot) => {
                if let Source::Tab(from) = source {
                    self.win.workspace.reorder_tab(from, slot);
                }
                false
            },
            Target::Window => {
                self.pointer_left_with(source, pane);
                false
            },
        };
        self.relayout(animate);
    }

    /// Close every pane of the tab at `index`.
    pub(super) fn close_tab(&mut self, index: usize) {
        let closed = self.win.workspace.close_tab(index);
        for &id in &closed {
            self.remove_pane(id);
        }
        if self.win.workspace.tabs().is_empty() {
            self.win.vacated = Some(WhenLast::ShowEmpty);
        }
        self.relayout(false);
    }

    /// Remove a pane, leaving the window empty if it was the last.
    pub(super) fn close(&mut self, id: PaneId) {
        self.remove_pane(id);
        match self.win.workspace.close(id) {
            Closed::NotFound => (),
            Closed::Empty => {
                self.win.vacated = Some(WhenLast::ShowEmpty);
                self.relayout(true);
            },
            Closed::Remaining => self.relayout(true),
        }
    }

    /// Forget the pane `id`, ending whatever it was connected to.
    pub(super) fn remove_pane(&mut self, id: PaneId) {
        self.dismiss_prompts(id);
        let Some(index) = self.core.panes.iter().position(|pane| pane.id == id) else { return };
        let mut pane = self.core.panes.remove(index);
        let node = pane.remote.as_ref().and_then(crate::pane::Remote::node);
        pane.hang_up();
        self.retire(pane.content);
        if let Some(node) = node {
            let dot = shell::session_dot(&self.core.panes, node);
            self.core.ui.tree.set_state(NodeId(node), dot);
        }
    }
}
