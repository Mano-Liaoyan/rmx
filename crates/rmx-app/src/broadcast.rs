//! Typing into several sessions at once: which panes a keystroke reaches.
//!
//! Broadcast fans out where keys are turned into bytes, not on the input tap. Each target encodes
//! the key for its own terminal's modes (application cursor keys, the kitty protocol), and gets
//! one write. Pastes, snippets and the terminal's own answers to its program never come through
//! here, so they stay with the pane they were meant for, and nothing typed can come back round.

use rmx_layout::PaneId;

use crate::pane::Pane;

/// Hand `each` every pane that typing in `focused` reaches, once each: the whole broadcast group
/// when `focused` is in it, else `focused` alone.
pub fn each_target(panes: &mut [Pane], focused: PaneId, mut each: impl FnMut(&mut Pane)) {
    let group = panes.iter().any(|pane| pane.id == focused && pane.broadcast);
    for pane in panes.iter_mut() {
        if (group && pane.broadcast) || pane.id == focused {
            each(pane);
        }
    }
}

/// How many panes are in the broadcast group.
#[cfg(any(test, feature = "bench"))]
pub fn group_size(panes: &[Pane]) -> usize {
    panes.iter().filter(|pane| pane.broadcast).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes(group: &[PaneId]) -> Vec<Pane> {
        (1..=4)
            .map(|id| {
                let mut pane = Pane::local(id);
                pane.broadcast = group.contains(&id);
                pane
            })
            .collect()
    }

    fn reached(panes: &mut [Pane], focused: PaneId) -> Vec<PaneId> {
        let mut seen = Vec::new();
        each_target(panes, focused, |pane| seen.push(pane.id));
        seen
    }

    #[test]
    fn typing_in_the_group_reaches_each_of_it_once_and_nothing_else() {
        let mut panes = panes(&[1, 3]);
        assert_eq!(reached(&mut panes, 1), [1, 3]);
        assert_eq!(reached(&mut panes, 3), [1, 3]);
        assert_eq!(group_size(&panes), 2);
    }

    #[test]
    fn typing_outside_the_group_reaches_only_the_focused_pane() {
        let mut panes = panes(&[1, 3]);
        assert_eq!(reached(&mut panes, 2), [2]);
        assert_eq!(reached(&mut panes, 4), [4]);
    }

    #[test]
    fn with_no_group_typing_stays_with_the_focused_pane() {
        let mut panes = panes(&[]);
        assert_eq!(reached(&mut panes, 2), [2]);
        assert_eq!(reached(&mut panes, 9), Vec::<PaneId>::new());
        assert_eq!(group_size(&panes), 0);
    }

    #[test]
    fn a_group_of_one_is_just_that_pane() {
        let mut panes = panes(&[2]);
        assert_eq!(reached(&mut panes, 2), [2]);
        assert_eq!(reached(&mut panes, 1), [1]);
    }
}
