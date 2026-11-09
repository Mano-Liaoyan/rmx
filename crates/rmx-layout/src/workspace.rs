//! The tabs of a window, each a tree of panes.

use crate::split::{Axis, Side, Split};
use crate::{PaneId, Rect};

/// A direction to move the focus in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug)]
pub struct Tab {
    /// Stays the same however the tabs are reordered, for as long as the tab exists.
    pub id: u64,
    pub layout: Split<PaneId>,
    pub focus: PaneId,
    pub zoomed: bool,
    /// What the user called it, shown instead of what its focused pane is called.
    pub name: Option<String>,
}

/// What closing a pane left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The pane was not in any tab.
    NotFound,
    /// Its tab remains, or another tab took over.
    Remaining,
    /// It was the last pane of the last tab.
    Empty,
}

#[derive(Debug, Default)]
pub struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    next_tab: u64,
}

impl Workspace {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// Whether `pane` is in any tab.
    pub fn contains(&self, pane: PaneId) -> bool {
        self.tabs.iter().any(|tab| tab.layout.panes().contains(&pane))
    }

    pub fn focused(&self) -> Option<PaneId> {
        self.active().map(|tab| tab.focus)
    }

    /// Open a tab showing `pane`, after the active one, and switch to it.
    pub fn add_tab(&mut self, pane: PaneId) {
        let at = if self.tabs.is_empty() { 0 } else { self.active + 1 };
        let tab = self.new_tab(pane);
        self.tabs.insert(at, tab);
        self.active = at;
    }

    fn new_tab(&mut self, pane: PaneId) -> Tab {
        self.next_tab += 1;
        Tab { id: self.next_tab, layout: Split::Pane(pane), focus: pane, zoomed: false, name: None }
    }

    /// The index of the tab with the id `id`.
    pub fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Put `new` next to the focused pane along `axis`, and focus it.
    pub fn split(&mut self, new: PaneId, axis: Axis) -> bool {
        let Some(tab) = self.active_mut() else { return false };
        let inserted = tab.layout.insert(tab.focus, Split::Pane(new), axis, Side::After);
        if inserted {
            tab.focus = new;
            tab.zoomed = false;
        }
        inserted
    }

    /// Remove `pane`, wherever it is. Focus goes to the pane that takes over its space.
    pub fn close(&mut self, pane: PaneId) -> Closed {
        let Some(index) = self.tabs.iter().position(|tab| tab.layout.panes().contains(&pane))
        else {
            return Closed::NotFound;
        };

        let tab = &mut self.tabs[index];
        if tab.layout.panes().len() > 1 {
            let neighbour = tab.layout.neighbour(pane);
            let layout = std::mem::replace(&mut tab.layout, Split::Pane(pane));
            tab.layout = layout.remove(pane).expect("the tab has other panes");
            if tab.focus == pane {
                tab.focus = neighbour.unwrap_or_else(|| tab.layout.panes()[0]);
                tab.zoomed = false;
            }
            return Closed::Remaining;
        }

        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
            return Closed::Empty;
        }
        if index < self.active || self.active == self.tabs.len() {
            self.active -= 1;
        }
        Closed::Remaining
    }

    /// Close the tab at `index`, returning the panes it held. Afterwards the active tab is the
    /// one that was active, or the one that took the place of the closed one.
    pub fn close_tab(&mut self, index: usize) -> Vec<PaneId> {
        if index >= self.tabs.len() {
            return Vec::new();
        }
        let tab = self.tabs.remove(index);
        if index < self.active || (self.active == self.tabs.len() && self.active > 0) {
            self.active -= 1;
        }
        tab.layout.panes()
    }

    /// Put back a tab saved earlier, as the last one, and switch to it if `active`.
    pub fn restore_tab(
        &mut self,
        layout: Split<PaneId>,
        focus: PaneId,
        zoomed: bool,
        name: Option<String>,
        active: bool,
    ) {
        let mut tab = self.new_tab(focus);
        tab.layout = layout;
        tab.name = name;
        tab.zoomed = zoomed && tab.layout.panes().len() > 1;
        self.tabs.push(tab);
        if active {
            self.active = self.tabs.len() - 1;
        }
    }

    /// Take out the tab at `index`, with its panes, for another window to put in with
    /// [`Workspace::insert_tab`].
    pub fn take_tab(&mut self, index: usize) -> Option<Tab> {
        if index >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if index < self.active || self.active == self.tabs.len() {
            self.active -= 1;
        }
        Some(tab)
    }

    /// Take `pane` out of its tab, as a tab of its own for another window to put in. The whole
    /// tab when it is the only pane there.
    pub fn take_pane(&mut self, pane: PaneId) -> Option<Tab> {
        let index = self.index_of(pane)?;
        if self.tabs[index].layout.panes().len() < 2 {
            return self.take_tab(index);
        }
        self.close(pane);
        Some(self.new_tab(pane))
    }

    /// Put in a tab from another window before index `at`, and switch to it.
    pub fn insert_tab(&mut self, mut tab: Tab, at: usize) {
        // Its number only meant something among the tabs of the window it came from.
        self.next_tab += 1;
        tab.id = self.next_tab;
        let at = at.min(self.tabs.len());
        self.tabs.insert(at, tab);
        self.active = at;
    }

    /// Move the tab at `from` to index `to`, keeping the same tab active.
    pub fn reorder_tab(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return false;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = match self.active {
            active if active == from => to,
            active if from < active && active <= to => active - 1,
            active if to <= active && active < from => active + 1,
            active => active,
        };
        true
    }

    /// Dock `pane` on `side` of `target` along `axis`, wherever they are, focusing it.
    pub fn dock(&mut self, pane: PaneId, target: PaneId, axis: Axis, side: Side) -> bool {
        if pane == target || !self.contains(pane) || !self.contains(target) {
            return false;
        }
        self.close(pane);
        let Some(index) = self.index_of(target) else { return false };
        let tab = &mut self.tabs[index];
        if !tab.layout.insert(target, Split::Pane(pane), axis, side) {
            return false;
        }
        tab.focus = pane;
        tab.zoomed = false;
        self.active = index;
        true
    }

    /// Group `pane` with the tab `into`, where [`Split::spot`] finds room for it, focusing it.
    pub fn merge(
        &mut self,
        pane: PaneId,
        into: usize,
        area: Rect,
        min: (f32, f32),
        spacing: f32,
    ) -> bool {
        let (Some(tab), true) = (self.tabs.get(into), self.contains(pane)) else { return false };
        if tab.layout.panes().contains(&pane) {
            return false;
        }
        let Some((beside, axis)) = tab.layout.spot(&Split::Pane(pane), area, min, spacing) else {
            return false;
        };
        let into = tab.id;
        self.close(pane);
        let Some(index) = self.tab_index(into) else { return false };

        let tab = &mut self.tabs[index];
        match beside {
            Some(beside) => {
                tab.layout.insert(beside, Split::Pane(pane), axis, Side::After);
            },
            None => {
                let layout = std::mem::replace(&mut tab.layout, Split::Pane(pane));
                tab.layout = layout.join(Split::Pane(pane), axis, Side::After);
            },
        }
        tab.focus = pane;
        tab.zoomed = false;
        self.active = index;
        true
    }

    /// Make `pane` a tab of its own at index `at`, as long as it is not alone in its tab, and
    /// switch to it.
    pub fn detach(&mut self, pane: PaneId, at: usize) -> bool {
        let alone = self.index_of(pane).is_none_or(|index| self.tabs[index].layout.panes().len() < 2);
        if alone {
            return false;
        }
        self.close(pane);
        let at = at.min(self.tabs.len());
        let tab = self.new_tab(pane);
        self.tabs.insert(at, tab);
        self.active = at;
        true
    }

    /// Call the tab at `index` this, or again what its focused pane is called.
    pub fn set_tab_name(&mut self, index: usize, name: Option<String>) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.name = name;
        }
    }

    /// Make each pane of the tab at `index` a tab of its own, in reading order, the first keeping
    /// the tab. The focused pane stays focused, in its new tab. Returns whether it was a group.
    pub fn ungroup(&mut self, index: usize) -> bool {
        let Some(tab) = self.tabs.get(index).filter(|tab| tab.layout.panes().len() > 1) else {
            return false;
        };
        let (focus, panes) = (tab.focus, tab.layout.panes());
        let was_active = self.active == index;
        self.tabs[index].layout = Split::Pane(panes[0]);
        self.tabs[index].focus = panes[0];
        self.tabs[index].zoomed = false;
        for (offset, &pane) in panes[1..].iter().enumerate() {
            let tab = self.new_tab(pane);
            self.tabs.insert(index + 1 + offset, tab);
        }
        if was_active {
            self.active = index + panes.iter().position(|&pane| pane == focus).unwrap_or(0);
        } else if self.active > index {
            self.active += panes.len() - 1;
        }
        true
    }

    fn index_of(&self, pane: PaneId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.layout.panes().contains(&pane))
    }

    /// Focus `pane` if it is in the active tab.
    pub fn focus(&mut self, pane: PaneId) -> bool {
        let Some(tab) = self.active_mut() else { return false };
        let found = tab.layout.panes().contains(&pane);
        if found {
            tab.focus = pane;
        }
        found
    }

    /// Focus the nearest pane in `direction` of the focused one, laid out in `area`.
    pub fn focus_direction(&mut self, direction: Direction, area: Rect, spacing: f32) -> bool {
        let Some(tab) = self.active_mut() else { return false };
        let rects = tab.layout.rects(area, spacing);
        let Some(&(_, from)) = rects.iter().find(|&&(id, _)| id == tab.focus) else {
            return false;
        };

        let nearest = rects
            .iter()
            .filter(|&&(id, _)| id != tab.focus)
            .filter_map(|&(id, rect)| Some((id, distance(direction, from, rect)?)))
            .min_by(|a, b| a.1.total_cmp(&b.1));

        match nearest {
            Some((id, _)) => {
                tab.focus = id;
                tab.zoomed = false;
                true
            },
            None => false,
        }
    }

    pub fn toggle_zoom(&mut self) {
        if let Some(tab) = self.active_mut() {
            tab.zoomed = !tab.zoomed && tab.layout.panes().len() > 1;
        }
    }

    pub fn select(&mut self, index: usize) -> bool {
        let valid = index < self.tabs.len() && index != self.active;
        if valid {
            self.active = index;
        }
        valid
    }

    /// Switch `delta` tabs along, wrapping around.
    pub fn cycle(&mut self, delta: isize) -> bool {
        let len = self.tabs.len() as isize;
        len > 1 && self.select((self.active as isize + delta).rem_euclid(len) as usize)
    }

    /// Where the panes of the active tab go in `area`.
    pub fn rects(&self, area: Rect, spacing: f32) -> Vec<(PaneId, Rect)> {
        match self.active() {
            Some(tab) if tab.zoomed => vec![(tab.focus, area)],
            Some(tab) => tab.layout.rects(area, spacing),
            None => Vec::new(),
        }
    }
}

/// How far `to` is from `from` going in `direction`, if it is that way and they overlap across it.
fn distance(direction: Direction, from: Rect, to: Rect) -> Option<f32> {
    let overlap_y = from.y < to.bottom() && to.y < from.bottom();
    let overlap_x = from.x < to.right() && to.x < from.right();
    let (ahead, across) = match direction {
        Direction::Left => (from.x - to.right(), overlap_y),
        Direction::Right => (to.x - from.right(), overlap_y),
        Direction::Up => (from.y - to.bottom(), overlap_x),
        Direction::Down => (to.y - from.bottom(), overlap_x),
    };
    (ahead >= 0. && across).then_some(ahead)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0., y: 0., w: 200., h: 100. };

    fn two() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.split(2, Axis::Row);
        workspace
    }

    #[test]
    fn splitting_focuses_the_new_pane() {
        assert_eq!(two().focused(), Some(2));
    }

    #[test]
    fn focus_moves_by_direction_only_where_there_is_a_pane() {
        let mut workspace = two();
        assert!(workspace.focus_direction(Direction::Left, AREA, 0.));
        assert_eq!(workspace.focused(), Some(1));
        assert!(!workspace.focus_direction(Direction::Left, AREA, 0.));
        assert!(!workspace.focus_direction(Direction::Up, AREA, 0.));
    }

    #[test]
    fn closing_the_focused_pane_focuses_its_neighbour() {
        let mut workspace = two();
        assert_eq!(workspace.close(2), Closed::Remaining);
        assert_eq!(workspace.focused(), Some(1));
        assert_eq!(workspace.rects(AREA, 0.), [(1, AREA)]);
    }

    #[test]
    fn closing_the_last_pane_empties_the_workspace() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        assert_eq!(workspace.close(9), Closed::NotFound);
        assert_eq!(workspace.close(1), Closed::Empty);
        assert!(workspace.focused().is_none());
    }

    #[test]
    fn closing_a_tab_before_the_active_keeps_the_active_one() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.add_tab(2);
        workspace.add_tab(3);
        assert_eq!(workspace.active_index(), 2);
        workspace.close(1);
        assert_eq!(workspace.focused(), Some(3));
    }

    #[test]
    fn closing_the_last_tab_in_order_selects_the_one_before() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.add_tab(2);
        workspace.close(2);
        assert_eq!(workspace.focused(), Some(1));
    }

    #[test]
    fn a_zoomed_pane_takes_the_whole_area() {
        let mut workspace = two();
        workspace.toggle_zoom();
        assert_eq!(workspace.rects(AREA, 0.), [(2, AREA)]);
        workspace.toggle_zoom();
        assert_eq!(workspace.rects(AREA, 0.).len(), 2);
    }

    #[test]
    fn a_lone_pane_does_not_zoom() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.toggle_zoom();
        assert!(!workspace.active().unwrap().zoomed);
    }

    #[test]
    fn reordering_keeps_the_same_tab_active() {
        let mut workspace = Workspace::default();
        for pane in 1..=4 {
            workspace.add_tab(pane);
        }
        workspace.select(1);
        assert_eq!(workspace.focused(), Some(2));
        // Tabs are 1 2 3 4; the active one is 2, at index 1.
        assert!(workspace.reorder_tab(3, 0));
        assert_eq!(workspace.focused(), Some(2));
        assert_eq!(workspace.active_index(), 2);
        assert!(workspace.reorder_tab(2, 0));
        assert_eq!(workspace.active_index(), 0);
        assert!(!workspace.reorder_tab(0, 9));
    }

    #[test]
    fn closing_a_tab_returns_its_panes_and_keeps_a_tab_active() {
        let mut workspace = two();
        workspace.add_tab(3);
        workspace.select(0);
        assert_eq!(workspace.close_tab(0), [1, 2]);
        assert_eq!(workspace.focused(), Some(3));
        assert_eq!(workspace.close_tab(0), [3]);
        assert!(workspace.focused().is_none());
    }

    #[test]
    fn docking_moves_a_pane_beside_another() {
        let mut workspace = two();
        workspace.split(3, Axis::Column);
        assert!(workspace.dock(1, 3, Axis::Row, Side::Before));
        assert_eq!(workspace.focused(), Some(1));
        let panes = workspace.active().unwrap().layout.panes();
        assert_eq!(panes.len(), 3);
        assert!(!workspace.dock(1, 1, Axis::Row, Side::Before));
    }

    #[test]
    fn merging_groups_a_pane_with_a_tab_and_removes_an_emptied_tab() {
        const MIN: (f32, f32) = (10., 10.);
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.add_tab(2);
        assert!(workspace.merge(2, 0, AREA, MIN, 0.));
        assert_eq!(workspace.tabs().len(), 1);
        assert_eq!(workspace.focused(), Some(2));
        assert_eq!(workspace.active().unwrap().layout.panes(), [1, 2]);
    }

    #[test]
    fn detaching_makes_a_new_tab_and_never_empties_one() {
        let mut workspace = two();
        assert!(workspace.detach(2, 1));
        assert_eq!(workspace.tabs().len(), 2);
        assert_eq!(workspace.focused(), Some(2));
        assert!(!workspace.detach(1, 0));
        assert!(!workspace.detach(2, 0));
    }

    #[test]
    fn a_tab_leaves_with_its_panes_and_keeps_a_tab_active() {
        let mut workspace = two();
        workspace.add_tab(3);
        workspace.add_tab(4);
        workspace.select(1);
        let tab = workspace.take_tab(0).unwrap();
        assert_eq!(tab.layout.panes(), [1, 2]);
        assert_eq!(workspace.focused(), Some(3));
        assert!(workspace.take_tab(9).is_none());

        let mut other = Workspace::default();
        other.add_tab(5);
        other.insert_tab(tab, 0);
        assert_eq!(other.focused(), Some(2));
        assert_eq!(other.tabs().len(), 2);
        assert_ne!(other.tabs()[0].id, other.tabs()[1].id);
    }

    #[test]
    fn a_pane_leaves_its_group_as_a_tab_and_a_lone_pane_takes_the_tab() {
        let mut workspace = two();
        let tab = workspace.take_pane(1).unwrap();
        assert_eq!(tab.layout.panes(), [1]);
        assert_eq!(workspace.active().unwrap().layout.panes(), [2]);
        assert_eq!(workspace.tabs().len(), 1);
        let tab = workspace.take_pane(2).unwrap();
        assert_eq!(tab.layout.panes(), [2]);
        assert!(workspace.tabs().is_empty());
    }

    #[test]
    fn ungrouping_keeps_reading_order_and_the_focused_pane() {
        let mut workspace = Workspace::default();
        workspace.add_tab(9);
        workspace.add_tab(1);
        workspace.split(2, Axis::Row);
        workspace.split(3, Axis::Column);
        workspace.add_tab(8);
        workspace.select(1);
        workspace.focus(2);
        assert!(workspace.ungroup(1));
        let panes: Vec<_> = workspace.tabs().iter().map(|tab| tab.layout.panes()).collect();
        assert_eq!(panes, [vec![9], vec![1], vec![2], vec![3], vec![8]]);
        assert_eq!(workspace.focused(), Some(2));
        assert!(!workspace.ungroup(0), "a lone pane is no group");

        // Tabs after the group move along with the active one.
        let mut workspace = two();
        workspace.add_tab(3);
        assert!(workspace.ungroup(0));
        assert_eq!(workspace.focused(), Some(3));
    }

    #[test]
    fn tabs_cycle_around() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.add_tab(2);
        assert!(workspace.cycle(1));
        assert_eq!(workspace.focused(), Some(1));
        assert!(workspace.cycle(-1));
        assert_eq!(workspace.focused(), Some(2));
    }
}
