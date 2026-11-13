//! Asking before a close ends live sessions.
//!
//! What counts as a live session, and so what is asked about:
//!
//! - A pane connected to a saved connection whose session is up: SSH, Telnet, serial, a remote
//!   desktop or a file browser. Its link is [`Link::Live`].
//! - Not a local shell. Telling whether a shell is running something worth keeping means looking
//!   at its foreground process, which differs on every platform and would ask before closing
//!   every idle prompt; a local shell also leaves nothing behind on another machine.
//! - Not a pane still connecting, nor one whose session has closed, failed or is waiting to be
//!   asked for: closing those ends nothing.
//!
//! Every way of closing asks: a pane (its button, its menu, the shortcut, the palette), a tab (its
//! button, its menu), a window (its button, or the system closing it, as Alt+F4 or the taskbar
//! do), and so the application, which quits with its last window. Tearing a tab or pane off into
//! a window of its own, or merging it into another, moves the session without ending it, and
//! never asks. Neither does a session ending by itself. What the application is not told about in
//! time, such as the system shutting down or the process being killed, cannot be asked about.
//!
//! "Don't ask again" in the question, or the setting, turns the question off on this device.

use rmx_layout::PaneId;
use rmx_layout::workspace::Workspace;
use rmx_ui::widgets::PromptState;
use rmx_ui::{TabId, UiAction};

use crate::pane::{Link, Pane};

/// How many sessions the question names before it only counts the rest.
const NAMED: usize = 4;

/// What the question's tick box says.
pub const DONT_ASK: &str = "Don't ask again";

/// The panes `action` would close in a window showing `workspace`. None for anything but closing.
pub fn closed_by(workspace: &Workspace, action: &UiAction) -> Vec<PaneId> {
    match action {
        UiAction::WindowClose => {
            workspace.tabs().iter().flat_map(|tab| tab.layout.panes()).collect()
        },
        UiAction::TabClose(TabId(id)) => workspace
            .tab_index(*id)
            .map(|index| workspace.tabs()[index].layout.panes())
            .unwrap_or_default(),
        UiAction::PaneClose(rmx_ui::PaneId(pane)) if workspace.contains(*pane) => vec![*pane],
        _ => Vec::new(),
    }
}

/// Whether closing `pane` ends a session worth asking about.
pub fn is_live_session(pane: &Pane) -> bool {
    pane.remote.is_some() && pane.link == Link::Live
}

/// The names of the live sessions among `closing`, in that order.
pub fn live_sessions<'a>(panes: &'a [Pane], closing: &[PaneId]) -> Vec<&'a str> {
    closing
        .iter()
        .filter_map(|&id| panes.iter().find(|pane| pane.id == id))
        .filter(|pane| is_live_session(pane))
        .filter_map(|pane| pane.remote.as_ref().map(|remote| remote.name.as_str()))
        .collect()
}

/// The question before ending the sessions `names`, none when there are none. With `hidden`, as
/// behind the lock screen, it does not say which they are.
pub fn question(names: &[&str], hidden: bool) -> Option<PromptState> {
    let count = names.len();
    let title = match count {
        0 => return None,
        1 => "Disconnect 1 live session?".to_owned(),
        _ => format!("Disconnect {count} live sessions?"),
    };
    let detail = if hidden {
        "Closing this ends them.".to_owned()
    } else {
        let mut listed: Vec<&str> = names.iter().copied().take(NAMED).collect();
        let more = count.saturating_sub(NAMED);
        let rest = match more {
            0 => None,
            1 => Some("1 more".to_owned()),
            _ => Some(format!("{more} more")),
        };
        let last = match &rest {
            Some(rest) => rest.as_str(),
            None => listed.pop().unwrap_or_default(),
        };
        let them = if listed.is_empty() {
            last.to_owned()
        } else {
            format!("{} and {last}", listed.join(", "))
        };
        let verb = if count == 1 { "is" } else { "are" };
        format!("Closing this disconnects {them}, which {verb} still connected.")
    };
    let mut state = PromptState::new(title, detail, Vec::new());
    state.submit = "Disconnect".to_owned();
    state.check = Some(DONT_ASK.to_owned());
    Some(state)
}

#[cfg(test)]
mod tests {
    use rmx_layout::split::Axis;

    use super::*;
    use crate::connect::{Goal, Origin};

    fn remote(id: PaneId, name: &str, link: Link) -> Pane {
        let mut pane = Pane::remote(id, Origin::Saved(id), name.to_owned(), Goal::Session);
        pane.link = link;
        pane
    }

    fn failed() -> Link {
        Link::Failed { reason: "refused".to_owned() }
    }

    /// Two tabs: panes 1 and 2 split in the first, 3 alone in the second.
    fn workspace() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.split(2, Axis::Row);
        workspace.add_tab(3);
        workspace
    }

    #[test]
    fn closing_a_window_closes_every_pane_of_every_tab() {
        let mut closed = closed_by(&workspace(), &UiAction::WindowClose);
        closed.sort_unstable();
        assert_eq!(closed, vec![1, 2, 3]);
    }

    #[test]
    fn closing_a_tab_closes_its_panes_only() {
        let workspace = workspace();
        let first = workspace.tabs()[0].id;
        let mut closed = closed_by(&workspace, &UiAction::TabClose(TabId(first)));
        closed.sort_unstable();
        assert_eq!(closed, vec![1, 2]);
        assert!(closed_by(&workspace, &UiAction::TabClose(TabId(999))).is_empty());
    }

    #[test]
    fn closing_a_pane_closes_that_pane() {
        let workspace = workspace();
        assert_eq!(closed_by(&workspace, &UiAction::PaneClose(rmx_ui::PaneId(2))), vec![2]);
        assert!(closed_by(&workspace, &UiAction::PaneClose(rmx_ui::PaneId(9))).is_empty());
    }

    #[test]
    fn moving_and_other_actions_close_nothing() {
        let workspace = workspace();
        let tab = TabId(workspace.tabs()[0].id);
        for action in [
            UiAction::TabNew,
            UiAction::TabDetach(tab),
            UiAction::TabDragOut(tab),
            UiAction::TabToWindow(tab, 2),
            UiAction::TabUngroup(tab),
            UiAction::PaneToWindow(rmx_ui::PaneId(1)),
            UiAction::PaneToTab(rmx_ui::PaneId(1)),
            UiAction::TabReorder { id: tab, to: 1 },
            UiAction::PaneDisconnect(rmx_ui::PaneId(1)),
        ] {
            assert!(closed_by(&workspace, &action).is_empty(), "{action:?}");
        }
    }

    #[test]
    fn only_live_remote_sessions_count() {
        let panes = vec![
            Pane::local(1),
            remote(2, "web", Link::Live),
            remote(3, "db", Link::Connecting { stage: "Resolving".to_owned() }),
            remote(4, "old", Link::Closed { reason: "exit".to_owned() }),
            remote(5, "down", failed()),
            remote(6, "later", Link::Awaiting { why: "password".to_owned() }),
            remote(7, "files", Link::Live),
        ];
        assert_eq!(live_sessions(&panes, &[1, 2, 3, 4, 5, 6, 7]), vec!["web", "files"]);
        assert_eq!(live_sessions(&panes, &[1, 3, 4, 5, 6]), Vec::<&str>::new());
        assert_eq!(live_sessions(&panes, &[7, 2]), vec!["files", "web"]);
        assert_eq!(live_sessions(&panes, &[42]), Vec::<&str>::new());
    }

    #[test]
    fn a_quick_connection_counts_like_a_saved_one() {
        let draft =
            std::sync::Arc::new(rmx_ui::widgets::ConnectionDraft::new(rmx_ui::Protocol::Ssh));
        let mut quick =
            Pane::remote(1, Origin::Transient(draft), "alice@db".to_owned(), Goal::Session);
        quick.link = Link::Live;
        assert_eq!(live_sessions(&[quick], &[1]), vec!["alice@db"]);
    }

    #[test]
    fn a_local_shell_is_never_asked_about() {
        let mut shell = Pane::local(1);
        shell.link = Link::Live;
        assert!(!is_live_session(&shell));
    }

    #[test]
    fn nothing_live_asks_nothing() {
        assert!(question(&[], false).is_none());
        assert!(question(&[], true).is_none());
    }

    #[test]
    fn the_question_says_how_many_and_which() {
        let one = question(&["web"], false).unwrap();
        assert_eq!(one.title, "Disconnect 1 live session?");
        assert_eq!(one.detail, "Closing this disconnects web, which is still connected.");
        assert_eq!(one.submit, "Disconnect");
        assert_eq!(one.check.as_deref(), Some(DONT_ASK));
        assert!(!one.checked);
        assert!(one.fields.is_empty());

        let two = question(&["web", "db"], false).unwrap();
        assert_eq!(two.title, "Disconnect 2 live sessions?");
        assert_eq!(two.detail, "Closing this disconnects web and db, which are still connected.");

        let three = question(&["web", "db", "cache"], false).unwrap();
        assert_eq!(
            three.detail,
            "Closing this disconnects web, db and cache, which are still connected."
        );
    }

    #[test]
    fn a_long_list_names_a_few_and_counts_the_rest() {
        let names = ["a", "b", "c", "d", "e", "f"];
        let state = question(&names, false).unwrap();
        assert_eq!(state.title, "Disconnect 6 live sessions?");
        assert_eq!(
            state.detail,
            "Closing this disconnects a, b, c, d and 2 more, which are still connected."
        );
        let state = question(&names[..5], false).unwrap();
        assert_eq!(
            state.detail,
            "Closing this disconnects a, b, c, d and 1 more, which are still connected."
        );
    }

    #[test]
    fn behind_the_lock_it_counts_without_naming() {
        let state = question(&["secret-host", "other"], true).unwrap();
        assert_eq!(state.title, "Disconnect 2 live sessions?");
        assert!(!state.detail.contains("secret-host"));
        assert!(!state.title.contains("secret-host"));
    }
}
