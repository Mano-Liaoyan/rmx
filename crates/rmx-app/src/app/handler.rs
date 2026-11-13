//! The event loop's entry points: resuming, user and window events, waiting and exiting.

use super::*;

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.running.is_some() {
            return;
        }
        if let Err(err) = self.start(event_loop) {
            eprintln!("rmx: {err}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        #[cfg(feature = "bench")]
        let _slow = crate::bench::present::Slow::new("user_event", app_event_name(&event));
        let Some(running) = &mut self.running else { return };
        match event {
            AppEvent::Wakeup(id) => running.wakeup(id),
            AppEvent::Spawned(id, terminal) => {
                let Some(mut at) = running.at_pane(id) else { return };
                at.spawned(id, *terminal);
            },
            AppEvent::Session(id, event) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.session_event(id, event);
                }
            },
            AppEvent::Link(id, generation, event) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.link_event(id, generation, event);
                } else if let LinkEvent::Ready(Ready::Terminal(terminal)) = event {
                    running.core.retire(PaneContent::Terminal(terminal));
                }
            },
            AppEvent::DisplayNews(id, generation) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.display_news(id, generation);
                }
            },
            AppEvent::DisplayReady(id, generation) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.display_ready(id, generation);
                }
            },
            AppEvent::DisplayClipboard(id, generation, text) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.display_clipboard(id, generation, text);
                }
            },
            AppEvent::DisplayCursor(id, generation, cursor) => {
                let cursor = crate::remote::display_cursor(event_loop, &cursor);
                if let Some(mut at) = running.at_pane(id) {
                    at.display_cursor(id, generation, cursor);
                }
            },
            AppEvent::Files(id, generation, event) => {
                if let Some(mut at) = running.at_pane(id) {
                    at.files_event(id, generation, event);
                }
            },
            AppEvent::Prompt(prompt) => {
                let at = match prompt.pane {
                    Some(id) => running.at_pane(id),
                    None => running.at_active(),
                };
                if let Some(mut at) = at {
                    at.ask(prompt);
                }
            },
            AppEvent::Vault(event) => running.vault_event(event),
            AppEvent::SerialPorts(ports) => {
                for win in &mut running.windows {
                    win.shell.dialog.set_ports(ports.clone());
                }
                running.core.broadcast = true;
            },
            AppEvent::Term(id, event) => {
                let Some(mut at) = running.at_pane(id) else {
                    if matches!(event, TermEvent::Exit | TermEvent::ChildExit(_)) {
                        running.core.ended_early.push(id);
                    }
                    return;
                };
                let theme = *at.win.gfx.theme();
                let Some(terminal) = at.pane(id).and_then(|pane| pane.content.terminal()) else {
                    if matches!(event, TermEvent::Exit | TermEvent::ChildExit(_)) {
                        at.core.ended_early.push(id);
                    }
                    return;
                };
                match terminal.handle(event, &theme) {
                    Output::Title(title) => at.set_title(id, Some(title)),
                    Output::ResetTitle => at.set_title(id, None),
                    Output::Exit => at.program_ended(id),
                    Output::ClipboardStore(ty, text) => at.core.clipboard.store(ty, text),
                    Output::ClipboardLoad(ty, format) => {
                        let text = format(&at.core.clipboard.load(ty));
                        if let Some(terminal) = at.pane(id).and_then(|p| p.content.terminal()) {
                            terminal.write(text.into_bytes());
                        }
                    },
                    Output::Bell | Output::None => (),
                }
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        #[cfg(feature = "bench")]
        let _slow = {
            crate::bench::present::note_window_event(&event);
            crate::bench::present::Slow::new(
                "window_event",
                crate::bench::present::window_event_name(&event),
            )
        };
        let _ = event_loop;
        let Some(running) = &mut self.running else { return };
        running.note_pointer_arrival(id, &event);
        let Some(mut at) = running.at_window(id) else { return };
        // Focus goes with the window the user is in.
        if matches!(
            event,
            WindowEvent::Focused(true)
                | WindowEvent::MouseInput { .. }
                | WindowEvent::KeyboardInput { .. }
        ) {
            at.core.active = at.win.key;
        }

        // The edges of the window are the first to get a press, even over the title bar.
        let point = (at.win.pointer.x as f32, at.win.pointer.y as f32);
        if let WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } = event
            && !at.holding_pointer()
            && !at.win.shell.dragging_pane()
            && let Some(direction) = at.resize_edge_at(point)
        {
            if let Err(err) = at.win.gfx.window.drag_resize_window(direction) {
                log::warn!("Could not resize the window: {err}");
            }
            return;
        }

        // Over anything but the menu, a right press closes it and counts for what is there.
        if let WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Right,
            ..
        } = event
            && at.win.shell.menu_open()
        {
            let scale = at.scale();
            let at_point =
                egui::pos2(at.win.pointer.x as f32 / scale, at.win.pointer.y as f32 / scale);
            if !at.win.shell.menu_contains(at_point) {
                at.win.shell.close_menu();
                at.win.chrome.float_closed();
                at.request_frame();
            }
        }

        if let WindowEvent::KeyboardInput { event: key, .. } = &event
            && at.win.drag.is_some()
            && key.state == ElementState::Pressed
            && key.logical_key == Key::Named(NamedKey::Escape)
        {
            at.cancel_divider_drag();
            return;
        }

        if let WindowEvent::KeyboardInput { event: key, .. } = &event
            && at.win.shell.dragging_pane()
            && key.state == ElementState::Pressed
            && key.logical_key == Key::Named(NamedKey::Escape)
        {
            at.win.shell.cancel_pane_drag(Instant::now());
            at.win.chrome.invalidate();
            return;
        }

        // Ahead of the interface, which would take Tab to mean its next field.
        if let WindowEvent::KeyboardInput { event: key, is_synthetic: false, .. } = &event
            && at.files_key(key, self.modifiers)
        {
            return;
        }

        // Pointer events belong to the app while it follows a held button, and to the interface
        // otherwise, wherever it wants them.
        let held = at.holding_pointer();
        let pointer = matches!(
            event,
            WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
        );
        // A release still reaches the chrome, in case it heard the press.
        let release =
            matches!(event, WindowEvent::MouseInput { state: ElementState::Released, .. });
        let consumed = !(held && pointer && !release) && at.win.chrome.window_event(&event);

        if let (true, WindowEvent::KeyboardInput { event: key, .. }) = (consumed, &event)
            && key.state == ElementState::Pressed
            && self.modifiers.control_key()
            && matches!(key.logical_key.as_ref(), Key::Character(text) if text.eq_ignore_ascii_case("v"))
        {
            let text = at.core.clipboard.load(ClipboardType::Clipboard);
            at.win.chrome.paste(text);
        }

        match event {
            WindowEvent::CloseRequested => {
                let key = at.win.key;
                let here = at
                    .core
                    .panes
                    .iter()
                    .filter(|pane| pane.window == key)
                    .map(|pane| pane.id)
                    .collect();
                if !at.ask_before_losing_edits(here, UiAction::WindowClose) {
                    at.win.exit = true;
                }
            },
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                if at.win.motion.running(now) || at.win.chrome.needs_update(now) {
                    at.win.pacer.mark_dirty();
                }
                if !at.win.pacer.on_redraw() {
                    return;
                }

                #[cfg(feature = "bench")]
                let (frame_start, frame_start_ns, allocs_before, cpu_before, sched_before) = (
                    Instant::now(),
                    crate::bench::present::now_ns(),
                    crate::bench::allocations(),
                    crate::bench::thread_cpu_us(),
                    crate::bench::sched(),
                );

                let presented = at.draw();
                if at.win.exit {
                    return;
                }

                if !presented {
                    at.win.pacer.on_nothing_to_draw();
                    // An upload picked up with nothing else to show still has to be shown.
                    if at.any_dirty() {
                        at.request_frame();
                    }
                    return;
                }

                #[cfg(feature = "bench")]
                if let Some(presentation) = &mut at.win.gfx.presentation {
                    presentation.note_frame(at.win.pacer.pending_since(), frame_start_ns);
                }

                #[cfg(feature = "bench")]
                self.bench.frame(
                    frame_start.elapsed(),
                    crate::bench::thread_cpu_us() - cpu_before,
                    self.started,
                    at.win.gfx.phases,
                    at.win.gfx.stats,
                    (
                        crate::bench::allocations() - allocs_before,
                        at.win.gfx.winit_allocations,
                        at.win.ran_ui.then(|| at.win.chrome.stats()),
                    ),
                    sched_before,
                    rmx_render::probe::take(),
                );

                if !at.win.started {
                    at.win.started = true;
                    at.win.gfx.finish_startup();
                }

                let now = Instant::now();
                if at.win.pacer.on_presented(now) {
                    at.win.gfx.window.request_redraw();
                }
                // Keep frames coming until the animation has come to rest.
                if at.win.motion.running(now) || at.win.was_animating || at.any_dirty() {
                    at.win.pacer.mark_dirty();
                }
                #[cfg(feature = "bench")]
                if at.win.gfx.control.is_some() {
                    at.win.pacer.mark_dirty();
                }
            },
            WindowEvent::Resized(_) => {
                at.win.gfx.resize();
                at.win.maximized = at.win.gfx.window.is_maximized();
                at.relayout(false);
            },
            WindowEvent::ScaleFactorChanged { .. } => {
                at.win.pacer.set_refresh(refresh_interval(&at.win.gfx.window));
            },
            WindowEvent::Focused(focused) => {
                at.win.has_focus = focused;
                // The release that would end a drag may never come now.
                if !focused {
                    at.win.drag = None;
                    at.show_divider_pill(None, (0., 0.), false);
                    at.win.shell.cancel_pane_drag(Instant::now());
                }
                if let Some(pane) = at.focused_mut() {
                    pane.content.set_focused(focused);
                    pane.dirty = true;
                }
                if focused && let Some(id) = at.win.display_focus {
                    at.offer_clipboard(id);
                }
                at.request_frame();
            },
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                at.touch();
                // Nothing typed at the lock screen may reach a session behind it.
                if consumed || at.core.ui.locked() {
                    return;
                }
                let mods = self.modifiers;
                if let Some(action) = action_for(event.logical_key.as_ref(), mods) {
                    if event.state == ElementState::Pressed {
                        at.perform(action);
                    }
                    return;
                }

                let Some(pane) = at.focused_mut() else { return };
                if let Some(display) = pane.content.display_mut() {
                    let pressed = event.state == ElementState::Pressed;
                    if pressed
                        && mods.control_key()
                        && mods.alt_key()
                        && remote::is_salute_key(event.physical_key)
                    {
                        display.ctrl_alt_del();
                        return;
                    }
                    if let Some(key) =
                        remote::remote_key(event.physical_key, &event.logical_key, event.location)
                    {
                        display.key(key, pressed, event.repeat);
                    }
                    return;
                }
                let Some(terminal) = pane.content.terminal_mut() else { return };
                let pressed = event.state == ElementState::Pressed;
                let bytes = encode_key(event, mods, terminal.mode());
                if !bytes.is_empty() {
                    if pressed {
                        terminal.scroll_to_bottom();
                        terminal.write(bytes);
                        pane.dirty = true;
                    } else {
                        terminal.write(bytes);
                    }
                }
            },
            WindowEvent::Ime(ime) if !consumed && !at.core.ui.locked() => at.ime(ime),
            WindowEvent::CursorMoved { position, .. } => {
                let point = (position.x as f32, position.y as f32);
                if !held && let Some(direction) = at.resize_edge_at(point) {
                    at.win.pointer = position;
                    at.set_cursor(resize_cursor(direction));
                } else if consumed && at.win.chrome.pointer_on_interface() {
                    at.win.pointer = position;
                    at.show_divider_pill(None, (0., 0.), false);
                } else {
                    at.pointer_moved(position, self.modifiers);
                }
            },
            WindowEvent::MouseInput { button, state, .. } => {
                at.touch();
                if !consumed && !at.core.ui.locked() {
                    at.mouse_input(button, state, self.modifiers);
                }
            },
            WindowEvent::MouseWheel { delta, .. } if !consumed => {
                at.wheel(delta, self.modifiers);
            },
            _ => (),
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        #[cfg(feature = "bench")]
        if let Some(running) = &mut self.running {
            for win in &mut running.windows {
                if win.gfx.presentation.is_some() {
                    win.gfx.make_current();
                }
                if let Some(presentation) = &mut win.gfx.presentation {
                    presentation.finish(&win.gfx.window);
                }
            }
        }
        if let Some(running) = &mut self.running
            && let Some(at) = running.at_index(0)
        {
            let (done, wait) = mpsc::channel();
            let layout = at.layout();
            let session = running.session_text();
            running.core.vault.send(Request::Shutdown(layout, session, done));
            let _ = wait.recv_timeout(SAVE_ON_EXIT);
        }
        // The window and its GL objects must go while the display connection still exists.
        self.running = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(feature = "bench")]
        let _slow = crate::bench::present::Slow::new("about_to_wait", "loop");
        #[cfg(feature = "bench")]
        let script_wake = self.run_script(event_loop);
        #[cfg(not(feature = "bench"))]
        let script_wake: Option<Instant> = None;

        let Some(running) = &mut self.running else { return };
        running.settle(event_loop);
        if running.windows.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut wakes: Option<Instant> = script_wake;
        for index in 0..running.windows.len() {
            let Some(mut at) = running.at_index(index) else { continue };
            let (redraw, wake) = at.win.pacer.before_wait(now);
            if redraw {
                at.win.gfx.window.request_redraw();
            }

            // When egui is due it asks for a frame like everything else; only a later wish needs a
            // timer, since waiting for a frame that is on its way must not spin.
            let due = at.win.chrome.next_deadline();
            if due.is_some_and(|due| due <= now) {
                at.request_frame();
            }
            let chrome_wake = due.filter(|&due| due > now);
            let display_wake = at.tick_displays(now);
            wakes = [wakes, wake, chrome_wake, display_wake].into_iter().flatten().min();
        }
        let wake = [wakes, running.core.next_op_wake()].into_iter().flatten().min();
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

#[cfg(feature = "bench")]
fn app_event_name(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::Wakeup(_) => "wakeup",
        AppEvent::Term(..) => "term",
        AppEvent::Spawned(..) => "spawned",
        AppEvent::Session(..) => "session",
        AppEvent::Link(..) => "link",
        AppEvent::DisplayNews(..) => "display-news",
        AppEvent::DisplayReady(..) => "display-ready",
        AppEvent::DisplayClipboard(..) => "display-clipboard",
        AppEvent::DisplayCursor(..) => "display-cursor",
        AppEvent::Files(..) => "files",
        AppEvent::Prompt(_) => "prompt",
        AppEvent::SerialPorts(_) => "serial-ports",
        AppEvent::Vault(_) => "vault",
    }
}
