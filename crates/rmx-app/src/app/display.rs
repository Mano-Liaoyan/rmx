//! Remote displays: their frames, clipboard and scaling.

use super::*;

impl At<'_> {
    /// Tell the desktop of `id` what the local clipboard holds, if that is something it can carry.
    pub(super) fn offer_clipboard(&mut self, id: PaneId) {
        let text = self.core.clipboard.load(ClipboardType::Clipboard);
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(display) = pane.content.display_mut() else { return };
        if let ClipboardOutcome::Withheld { first_time: true } = display.clipboard_out(&text) {
            pane.notice = Some(VNC_CLIPBOARD_NOTICE.to_owned());
            self.link_changed(id);
        }
    }

    /// Take what the session of pane `id` has sent, in order, and show it soon.
    pub(super) fn display_news(&mut self, id: PaneId, generation: u64) {
        #[cfg(feature = "bench")]
        let started = Instant::now();
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let proxy = self.core.proxy.clone();
        let Some(index) = self.core.panes.iter().position(|pane| pane.id == id) else { return };
        let (panes, gfx) = (&mut self.core.panes, &self.win.gfx);
        gfx.make_current();
        let pane = &mut panes[index];
        let Some(display) = pane.content.display_mut() else { return };
        let ready = display.ready.clone();
        let mut size_changed = false;
        let mut reallocated = false;
        let mut failed = None;
        display.take_frames(|display, item| match item {
            Incoming::Resized { width, height } => {
                display.set_desktop(width, height);
                display.scroll = [0, 0];
                size_changed = true;
            },
            Incoming::Update(update) => {
                // Only with pixels to show does the texture take the new size: until then the
                // old picture stays, stretched, rather than a black pane.
                if display.texture_stale() {
                    let [width, height] =
                        display.desktop().unwrap_or([update.width, update.height]);
                    let flag = ready.clone();
                    let result = match &mut display.texture {
                        Some(texture) => texture.resize(width, height),
                        None => {
                            let (proxy, flag) = (proxy.clone(), flag);
                            gfx.create_remote_texture(width, height, move || {
                                if !flag.swap(true, std::sync::atomic::Ordering::AcqRel) {
                                    let _ =
                                        proxy.send_event(AppEvent::DisplayReady(id, generation));
                                }
                            })
                            .map(|texture| display.texture = Some(texture))
                        },
                    };
                    match result {
                        Ok(()) => reallocated = true,
                        Err(err) => failed = Some(err.to_string()),
                    }
                }
                display.submit(update);
            },
        });
        if let Some(reason) = failed {
            pane.link = Link::Failed { reason };
            pane.hang_up();
        }
        pane.dirty |= size_changed;
        let _ = reallocated;
        #[cfg(feature = "bench")]
        if !reallocated {
            crate::bench::note_remote(started.elapsed());
        }
        if size_changed {
            self.link_changed(id);
        }
        self.request_frame();
    }

    /// The pane's connection attempt that is current, if any.
    pub(super) fn pane_generation(&self, id: PaneId) -> Option<u64> {
        self.pane(id).and_then(|pane| pane.remote.as_ref()).map(|remote| remote.generation)
    }

    /// An upload has finished: pick it up on the next frame.
    pub(super) fn display_ready(&mut self, id: PaneId, generation: u64) {
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let shown = self.win.rects.iter().any(|&(pane, _)| pane == id);
        if let Some(display) = self.pane_mut(id).and_then(|pane| pane.content.display_mut()) {
            display.ready.store(false, std::sync::atomic::Ordering::Release);
            display.upload_due = true;
        }
        if shown {
            self.request_frame();
        }
    }

    /// The remote clipboard changed: believe it if it can be believed.
    pub(super) fn display_clipboard(&mut self, id: PaneId, generation: u64, text: String) {
        if self.pane_generation(id) != Some(generation) {
            return;
        }
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(display) = pane.content.display_mut() else { return };
        match display.clipboard_in(&text) {
            ClipboardOutcome::Pass => self.core.clipboard.store(ClipboardType::Clipboard, text),
            ClipboardOutcome::Withheld { first_time } => {
                if first_time {
                    pane.notice = Some(VNC_CLIPBOARD_NOTICE.to_owned());
                    self.link_changed(id);
                }
            },
        }
    }

    /// Upload what is due for the panes on screen, marking those whose picture changed.
    pub(super) fn upload_displays(&mut self, now: Instant) {
        #[cfg(feature = "bench")]
        let started = Instant::now();
        let mut uploaded = false;
        for index in 0..self.win.rects.len() {
            let id = self.win.rects[index].0;
            let Some(pane) = self.pane_mut(id) else { continue };
            let Some(display) = pane.content.display_mut() else { continue };
            if display.upload_due {
                uploaded = true;
                pane.dirty |= display.upload();
                display.next_poll = display.upload_due.then_some(now + remote::UPLOAD_POLL);
            }
        }
        if uploaded {
            self.win.gfx.flush();
            #[cfg(feature = "bench")]
            crate::bench::note_remote(started.elapsed());
        }
    }

    /// Ask for frames still owed to desktops on screen, and for the sizes of panes that have
    /// settled. Returns when next to look; nothing at all when no desktop has anything pending,
    /// so an idle pane costs no wakeups.
    pub(super) fn tick_displays(&mut self, now: Instant) -> Option<Instant> {
        let animating = self.win.motion.running(now);
        let mut wake: Option<Instant> = None;
        let mut frame = false;
        for index in 0..self.win.rects.len() {
            let id = self.win.rects[index].0;
            let Some(display) = self.pane_mut(id).and_then(|p| p.content.display_mut()) else {
                continue;
            };
            if let Some(at) = display.next_poll {
                if at <= now {
                    frame = true;
                    display.next_poll = None;
                } else {
                    wake = Some(wake.map_or(at, |wake| wake.min(at)));
                }
            }
            if let Some(due) = display.resize_deadline() {
                if due > now {
                    wake = Some(wake.map_or(due, |wake| wake.min(due)));
                } else if animating {
                    // Never during an animation: frames are running, so this is looked at again.
                    wake = Some(now + Duration::from_millis(16));
                } else {
                    display.settle(now);
                }
            }
        }
        if frame {
            self.request_frame();
        }
        wake
    }

    /// Where the pointer at window position `point` lands on the desktop of `id`.
    pub(super) fn display_layout(&self, id: PaneId) -> Option<(rmx_render::RemoteLayout, Rect)> {
        let pane = self.pane(id)?;
        let display = pane.content.display()?;
        let desktop = display.desktop()?;
        let &(_, rest) = self.win.rects.iter().find(|&&(pane, _)| pane == id)?;
        let now = Instant::now();
        let rect = self.win.motion.frame(id, now).unwrap_or(rest);
        let (rect, rest) = (self.terminal_rect(rect), self.terminal_rect(rest));
        Some((self.win.gfx.remote_layout(display.mode(), desktop, rect, rest), rest))
    }

    pub(super) fn display_point(&self, id: PaneId) -> Option<(u32, u32, bool)> {
        let (layout, _) = self.display_layout(id)?;
        let (_, height) = self.win.gfx.window_size();
        let at = layout.pointer(height, self.win.pointer.x, self.win.pointer.y);
        Some((at.x, at.y, at.inside))
    }

    /// Send Ctrl+Alt+Del to the desktop of `id`.
    pub(super) fn salute(&mut self, id: PaneId) {
        if let Some(display) = self.pane_mut(id).and_then(|pane| pane.content.display_mut()) {
            display.ctrl_alt_del();
        }
    }

    /// Show the desktop of `id` the other way.
    pub(super) fn toggle_scale(&mut self, id: PaneId) {
        use rmx_ui::widgets::ScaleChoice;
        if let Some(pane) = self.pane_mut(id)
            && let Some(display) = pane.content.display_mut()
        {
            display.scale = match display.scale {
                ScaleChoice::Fit => ScaleChoice::Native,
                ScaleChoice::Native => ScaleChoice::Fit,
            };
            display.scroll = [0, 0];
            pane.dirty = true;
        }
        self.redraw_all();
    }

    /// Browse the files of the SSH pane `id`, at its working directory if it has said what that
    /// is, in a tab of its own on a connection of its own.
    pub(super) fn open_files_here(&mut self, id: PaneId) {
        let Some(pane) = self.pane(id) else { return };
        let Some(remote) = &pane.remote else { return };
        let directory =
            pane.content.terminal().and_then(Terminal::working_directory).map(|cwd| cwd.path);
        let node = remote.node;
        self.open_node(node, Goal::Files { directory });
    }

    pub(super) fn set_title(&mut self, id: PaneId, title: Option<String>) {
        if let Some(pane) = self.pane_mut(id) {
            pane.title = title;
        }
        self.sync_focus(Instant::now());
    }
}
