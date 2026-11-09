//! Where a dragged tab or pane would land.
//!
//! On the strip, dropping onto the middle of a tab groups with it, and dropping onto its edges puts
//! what is dragged between tabs. Below the strip, dropping onto a pane docks beside it, on the side
//! nearest the pointer. Dropping outside the window tears it off into a window of its own.

use crate::split::{self, Axis, Side, Split};
use crate::workspace::Workspace;
use crate::{PaneId, Rect};

/// What is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A tab, with all its panes.
    Tab(usize),
    /// The pane at index `.1` of the tab at `.0`, by its footer.
    Pane(usize, usize),
}

/// Where a drag would drop what it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Move the tab to this index.
    Reorder(usize),
    /// Group with the tab at this index.
    Merge(usize),
    /// Beside the pane at this index of the shown tab.
    Dock { pane: usize, axis: Axis, side: Side },
    /// Make the pane a tab of its own, at this index.
    Detach(usize),
    /// Out of the window, into a window of its own.
    Window,
}

/// Where things can be dropped: the strip's tabs, the shown tab's panes and outside the window.
#[derive(Debug)]
pub struct Scene {
    /// Bottom of the strip.
    pub bar: f32,
    /// The space below the strip that the panes tile.
    pub area: Rect,
    /// The tabs shown in the strip, by index, in order.
    pub tabs: Vec<(usize, Rect)>,
    /// The layout of every tab, by index.
    pub groups: Vec<Split<PaneId>>,
    /// The tab whose panes are shown.
    pub shown: usize,
    /// The shown panes a dock may cut, by index.
    pub panes: Vec<(usize, Rect)>,
    /// The room each of `panes` has to be cut, once a pane dragged out of them has left.
    pub room: Vec<Rect>,
    /// The panes a dock would add, as they are laid out.
    pub docked: Option<Split<PaneId>>,
    /// Smallest size a pane may be split to.
    pub min: (f32, f32),
    /// Pixels between neighbouring panes.
    pub spacing: f32,
    /// The window, if what is dragged may leave it.
    pub window: Option<Rect>,
}

impl Scene {
    /// The places to drop onto, as `workspace` is now.
    pub fn new(
        workspace: &Workspace,
        bar: f32,
        area: Rect,
        tabs: Vec<(usize, Rect)>,
        source: Source,
        min: (f32, f32),
        spacing: f32,
    ) -> Self {
        let docked = match source {
            Source::Tab(tab) => workspace.tabs().get(tab).map(|tab| tab.layout.clone()),
            Source::Pane(tab, pane) => workspace
                .tabs()
                .get(tab)
                .and_then(|tab| tab.layout.panes().get(pane).copied())
                .map(Split::Pane),
        };

        let shown = workspace.active_index();
        let shown_tab = workspace.active();
        let ids = shown_tab.map_or_else(Vec::new, |tab| tab.layout.panes());
        // A zoomed tab shows one pane over the places of the others, so nothing docks into it.
        let panes: Vec<(usize, Rect)> = match shown_tab {
            Some(tab) if !tab.zoomed => tab
                .layout
                .rects(area, spacing)
                .into_iter()
                .map(|(pane, rect)| (ids.iter().position(|&id| id == pane).unwrap_or(0), rect))
                .collect(),
            _ => Vec::new(),
        };
        // A pane moved within its group first leaves its place to its neighbours.
        let left = match (source, shown_tab) {
            (Source::Pane(tab, pane), Some(shown_tab)) if tab == shown => {
                ids.get(pane).and_then(|&id| shown_tab.layout.clone().remove(id))
            },
            _ => None,
        };
        let left = left.map_or_else(Vec::new, |layout| layout.rects(area, spacing));
        let room = panes.iter().map(|&(index, rect)| {
            let id = ids.get(index).copied();
            left.iter().find(|(pane, _)| Some(*pane) == id).map_or(rect, |&(_, room)| room)
        });

        Self {
            bar,
            area,
            tabs,
            groups: workspace.tabs().iter().map(|tab| tab.layout.clone()).collect(),
            shown,
            room: room.collect(),
            panes,
            docked,
            min,
            spacing,
            window: None,
        }
    }

    /// Where `source` would drop at `pointer`, if anywhere it would change something.
    pub fn target(&self, source: Source, pointer: (f64, f64)) -> Option<Target> {
        let (x, y) = (pointer.0 as f32, pointer.1 as f32);
        let (from, pane) = match source {
            Source::Tab(tab) => (tab, None),
            Source::Pane(tab, pane) => (tab, Some(pane)),
        };

        let area = self.area;
        if x < area.x || y < 0. || x >= area.right() || y >= area.bottom() {
            let left = self.window.is_some_and(|window| !window.contains(pointer));
            return left.then_some(Target::Window);
        }

        if y >= self.bar {
            let at = self.panes.iter().position(|(_, rect)| rect.contains(pointer))?;
            let (index, rect) = self.panes[at];
            let (axis, side) = split::drop_zone(rect, (x, y));
            let room = self.room.get(at).copied().unwrap_or(rect);
            let fits = self
                .docked
                .as_ref()
                .is_some_and(|new| split::fits(room, axis, side, new, self.min, self.spacing));
            return match pane {
                None if self.shown == from => None,
                Some(pane) if (self.shown, index) == (from, pane) => None,
                _ if !fits => None,
                _ => Some(Target::Dock { pane: index, axis, side }),
            };
        }

        // The middle half of a tab groups with it; its quarters are the slots on either side.
        let (first, last) = (self.tabs.first()?, self.tabs.last()?);
        let slot = match self.tabs.iter().find(|(_, rect)| x >= rect.x && x < rect.right()) {
            Some(&(index, rect)) => match (x - rect.x) / rect.w {
                part if (0.25..0.75).contains(&part) => {
                    return (index != from && self.joins(index)).then_some(Target::Merge(index));
                },
                part if part < 0.25 => index,
                _ => index + 1,
            },
            None if x < first.1.x => first.0,
            None => last.0 + 1,
        };
        match pane {
            Some(_) => Some(Target::Detach(slot)),
            // The tab leaves its own slot first.
            None => {
                let slot = if slot > from { slot - 1 } else { slot };
                (slot != from).then_some(Target::Reorder(slot))
            },
        }
    }

    /// Whether what is dragged can group with the tab `into`, each pane keeping the smallest size
    /// once it is placed beside them all or, where that leaves too little, beside one of them.
    fn joins(&self, into: usize) -> bool {
        let (Some(group), Some(new)) = (self.groups.get(into), &self.docked) else { return false };
        group.spot(new, self.area, self.min, self.spacing).is_some()
    }

    /// Where the preview of `target` goes, in the window: the part of the pane docked into, the
    /// tab merged with, or a bar `slot_width` wide at the slot between tabs.
    pub fn preview(&self, target: Target, slot_width: f32) -> Option<Rect> {
        match target {
            Target::Dock { pane, axis, side } => {
                let &(_, rect) = self.panes.iter().find(|(index, _)| *index == pane)?;
                let (a, b) = split::halves(rect, axis, 0.5, self.spacing);
                Some(match side {
                    Side::Before => a,
                    Side::After => b,
                })
            },
            Target::Merge(tab) => self.tabs.iter().find(|(index, _)| *index == tab).map(|t| t.1),
            Target::Detach(slot) | Target::Reorder(slot) => {
                let (&(_, first), &(_, last)) = (self.tabs.first()?, self.tabs.last()?);
                let rect = self.tabs.iter().find(|(index, _)| *index == slot).map(|t| t.1);
                let x = rect.map_or(last.right(), |rect| rect.x) - slot_width / 2.;
                Some(Rect::new(x, first.y, slot_width, first.h))
            },
            Target::Window => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u64) -> Split<PaneId> {
        Split::Pane(id)
    }

    /// Three tabs of 100 px on a 40 px bar from x = 100 in an 800 x 340 px window, showing tab
    /// 1's two panes side by side.
    fn scene() -> Scene {
        let tab = |index: usize| (index, Rect::new(100. + 100. * index as f32, 5., 100., 30.));
        Scene {
            bar: 40.,
            area: Rect::new(0., 40., 800., 300.),
            tabs: (0..3).map(tab).collect(),
            groups: vec![pane(1), pane(2).join(pane(3), Axis::Row, Side::After), pane(4)],
            shown: 1,
            panes: vec![(0, Rect::new(0., 40., 400., 300.)), (1, Rect::new(400., 40., 400., 300.))],
            room: vec![Rect::new(0., 40., 400., 300.), Rect::new(400., 40., 400., 300.)],
            docked: Some(pane(9)),
            min: (100., 50.),
            spacing: 0.,
            window: None,
        }
    }

    fn at(x: f64, y: f64) -> (f64, f64) {
        (x, y)
    }

    #[test]
    fn tabs_on_the_strip() {
        let scene = scene();
        let drop = |source, x| scene.target(source, at(x, 20.));
        // The middle half of another tab groups; its own does nothing.
        assert_eq!(drop(Source::Tab(0), 260.), Some(Target::Merge(1)));
        assert_eq!(drop(Source::Tab(0), 226.), Some(Target::Merge(1)));
        assert_eq!(drop(Source::Tab(0), 150.), None);
        // The quarters are the slots between tabs, counted once the tab has left its own.
        assert_eq!(drop(Source::Tab(0), 290.), Some(Target::Reorder(1)));
        assert_eq!(drop(Source::Tab(0), 210.), None);
        assert_eq!(drop(Source::Tab(2), 210.), Some(Target::Reorder(1)));
        assert_eq!(drop(Source::Tab(2), 105.), Some(Target::Reorder(0)));
        assert_eq!(drop(Source::Tab(2), 395.), None);
        // Past either end of the tabs.
        assert_eq!(drop(Source::Tab(2), 20.), Some(Target::Reorder(0)));
        assert_eq!(drop(Source::Tab(0), 700.), Some(Target::Reorder(2)));
    }

    #[test]
    fn panes_on_the_strip() {
        let scene = scene();
        let drop = |source, x| scene.target(source, at(x, 20.));
        // A pane joins another group, or leaves its own for a slot of its own.
        assert_eq!(drop(Source::Pane(1, 0), 150.), Some(Target::Merge(0)));
        assert_eq!(drop(Source::Pane(1, 0), 250.), None);
        assert_eq!(drop(Source::Pane(1, 0), 210.), Some(Target::Detach(1)));
        assert_eq!(drop(Source::Pane(1, 0), 290.), Some(Target::Detach(2)));
        assert_eq!(drop(Source::Pane(1, 0), 700.), Some(Target::Detach(3)));
    }

    #[test]
    fn docking_in_panes() {
        let scene = scene();
        let dock = |pane, axis, side| Some(Target::Dock { pane, axis, side });
        let drop = |source, x, y| scene.target(source, at(x, y));
        // The pane's diagonals divide it between its four sides, whatever its shape.
        assert_eq!(drop(Source::Tab(0), 30., 190.), dock(0, Axis::Row, Side::Before));
        assert_eq!(drop(Source::Tab(0), 370., 190.), dock(0, Axis::Row, Side::After));
        assert_eq!(drop(Source::Tab(0), 200., 60.), dock(0, Axis::Column, Side::Before));
        assert_eq!(drop(Source::Tab(2), 600., 320.), dock(1, Axis::Column, Side::After));
        // Panes are reordered within their group, but nothing docks beside itself.
        assert_eq!(drop(Source::Pane(1, 1), 30., 190.), dock(0, Axis::Row, Side::Before));
        assert_eq!(drop(Source::Pane(1, 0), 30., 190.), None);
        // A tab never docks into its own panes, outside them or onto too small a half.
        assert_eq!(drop(Source::Tab(1), 30., 190.), None);
        assert_eq!(drop(Source::Tab(0), 900., 190.), None);
        let small = Scene { min: (250., 50.), ..scene };
        assert_eq!(small.target(Source::Tab(0), at(30., 190.)), None);
        assert_eq!(
            small.target(Source::Tab(0), at(200., 60.)),
            dock(0, Axis::Column, Side::Before)
        );
    }

    #[test]
    fn merging_keeps_the_smallest_size() {
        let merge = |scene: &Scene, source, x| scene.target(source, at(x, 20.));
        // A pane joins tab 1's two panes as a third, a quarter of the width beside the others.
        let roomy = scene();
        assert_eq!(merge(&roomy, Source::Pane(0, 0), 250.), Some(Target::Merge(1)));
        // Too narrow for that, it shares one of them, stacked, where they are tall enough.
        let narrow = Scene { min: (250., 50.), ..scene() };
        assert_eq!(merge(&narrow, Source::Pane(0, 0), 250.), Some(Target::Merge(1)));
        let small = Scene { min: (250., 200.), ..scene() };
        assert_eq!(merge(&small, Source::Pane(0, 0), 250.), None);
        // Beside a lone pane, each still has half.
        assert_eq!(merge(&small, Source::Pane(1, 0), 150.), Some(Target::Merge(0)));
    }

    #[test]
    fn a_new_scene_finds_the_shown_panes_and_their_room() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.split(2, Axis::Row);
        workspace.split(3, Axis::Column);
        let area = Rect::new(0., 0., 400., 200.);
        let scene =
            Scene::new(&workspace, 0., area, Vec::new(), Source::Pane(0, 2), (10., 10.), 0.);
        assert_eq!(scene.panes.len(), 3);
        // Pane 3 leaves, so pane 2 has its whole half.
        assert_eq!(scene.room[1], Rect::new(200., 0., 200., 200.));
        assert_eq!(scene.docked, Some(Split::Pane(3)));

        workspace.toggle_zoom();
        let scene =
            Scene::new(&workspace, 0., area, Vec::new(), Source::Pane(0, 0), (10., 10.), 0.);
        assert!(scene.panes.is_empty());
    }

    #[test]
    fn only_outside_the_window_is_a_drop_a_new_window() {
        let torn = Scene { window: Some(Rect::new(0., 0., 800., 400.)), ..scene() };
        // Beside the panes but inside the window, as over a sidebar: nothing.
        let narrow = Scene { area: Rect::new(200., 40., 600., 300.), ..torn };
        assert_eq!(narrow.target(Source::Tab(0), at(100., 200.)), None);
        assert_eq!(narrow.target(Source::Tab(0), at(100., 500.)), Some(Target::Window));
        assert_eq!(narrow.target(Source::Tab(0), at(900., 200.)), Some(Target::Window));
        assert_eq!(narrow.target(Source::Pane(1, 0), at(-5., 200.)), Some(Target::Window));
        // Without leave to go, nothing either.
        assert_eq!(scene().target(Source::Tab(0), at(900., 200.)), None);
    }

    #[test]
    fn previews_show_where_a_drop_lands() {
        let scene = scene();
        let dock = Target::Dock { pane: 0, axis: Axis::Row, side: Side::After };
        assert_eq!(scene.preview(dock, 4.), Some(Rect::new(200., 40., 200., 300.)));
        assert_eq!(scene.preview(Target::Merge(1), 4.), Some(Rect::new(200., 5., 100., 30.)));
        assert_eq!(scene.preview(Target::Detach(3), 4.), Some(Rect::new(398., 5., 4., 30.)));
        assert_eq!(scene.preview(Target::Detach(0), 4.), Some(Rect::new(98., 5., 4., 30.)));
    }
}
