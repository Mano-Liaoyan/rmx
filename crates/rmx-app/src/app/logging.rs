//! Logging a pane's terminal to a file: started from its menu or by its connection, carried over
//! a reconnect, and stopped from the menu or by an error.

use std::path::{Path, PathBuf};

use rmx_terminal::Streams;

use super::*;
use crate::session_log::{self, SessionLog};

impl At<'_> {
    /// Start logging the output of pane `id`, or stop if it is logging.
    pub(super) fn toggle_log(&mut self, id: PaneId) {
        let Some(pane) = self.pane_mut(id) else { return };
        if let Some(log) = pane.log.take() {
            pane.notice = Some(format!("Stopped logging to {}", log.path.display()));
        } else {
            // Typing holds passwords, so the menu only ever logs what is shown.
            self.start_log(id, Streams::OUTPUT, None);
        }
        self.mark_dirty(id);
    }

    /// Pane `id` has a new terminal: go on with its log, in the same file, or begin the one its
    /// connection asks for.
    pub(super) fn log_new_terminal(&mut self, id: PaneId) {
        let Some(pane) = self.pane_mut(id) else { return };
        let carried = pane.log.take().map(|log| (log.path.clone(), log.streams));
        let wanted = pane.remote.as_ref().and_then(|remote| session_log::streams(remote.log));
        match (carried, wanted) {
            (Some((path, streams)), _) => self.start_log(id, streams, Some(path)),
            (None, Some(streams)) => self.start_log(id, streams, None),
            (None, None) => return,
        }
        self.mark_dirty(id);
    }

    /// The log of pane `id` to `path` could not go on.
    pub(super) fn log_failed(&mut self, id: PaneId, path: &Path, why: String) {
        let Some(pane) = self.pane_mut(id) else { return };
        if pane.log.as_ref().is_some_and(|log| log.path == path) {
            pane.log = None;
        }
        pane.notice = Some(why);
        self.mark_dirty(id);
    }

    /// Log `streams` of the terminal of pane `id` to `path`, or to a new file, saying where and
    /// what that file may hold.
    fn start_log(&mut self, id: PaneId, streams: Streams, path: Option<PathBuf>) {
        let runtime = self.core.runtime.handle().clone();
        let proxy = self.core.proxy.clone();
        let Some(pane) = self.pane_mut(id) else { return };
        let Some(terminal) = pane.content.terminal() else { return };
        let fresh = path.is_none();
        let path = path.unwrap_or_else(|| {
            let label = pane.remote.as_ref().map_or("local", |remote| remote.name.as_str());
            session_log::new_path(&crate::background::logs_dir(), label, id)
        });
        let told = path.clone();
        let failed = move |why| {
            let _ = proxy.send_event(AppEvent::LogFailed(id, told, why));
        };
        match SessionLog::start(terminal, path, streams, &runtime, failed) {
            Ok(log) => {
                if fresh {
                    pane.notice = Some(session_log::warning(&log.path, streams));
                }
                pane.log = Some(log);
            },
            Err(err) => pane.notice = Some(format!("Could not start logging: {err}")),
        }
    }

    fn mark_dirty(&mut self, id: PaneId) {
        if let Some(pane) = self.pane_mut(id) {
            pane.dirty = true;
        }
        self.win.chrome.invalidate();
        self.request_frame();
    }
}
