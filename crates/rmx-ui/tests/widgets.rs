//! Widgets driven headlessly: synthetic pointer input in, actions out.

use std::time::{Duration, Instant};

use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Ui, pos2, vec2};
use rmx_ui::chrome::ChromeCore;
use rmx_ui::fonts::{self, CjkFallback};
use rmx_ui::theme::{self, PANE_BAR_HEIGHT, STRIP_HEIGHT, TREE_ROW_HEIGHT};
use rmx_ui::virtual_list::{self, window};
use rmx_ui::widgets::tabs::{TAB_GAP, drop_index, preview_slot, slot_x, tab_width};
use rmx_ui::widgets::{
    ConnectionDraft, InheritedLogin, NewConnectionState, NodeKind, PaneBarView, PaneStatus, Panels,
    PromptState, QuickItem, QuickOpenState, Reveal, SettingsDraft, SettingsState, SettingsView,
    SyncDraft, TabStripState, TabView, TitleBarView, TreeModel, TreeNode, TreeState,
    connection_tree, new_connection_dialog, pane_bar, prompt_dialog, quick_open, settings_dialog,
    tab_strip, title_bar,
};
use rmx_ui::{NodeId, PaneId, Protocol, TabId, ThemeMode, UiAction};

const SCREEN: Rect = Rect { min: Pos2::ZERO, max: Pos2 { x: 1200., y: 800. } };

/// A context fed one input frame at a time.
struct Harness {
    ctx: Context,
    time: f64,
    repaint_delay: Duration,
}

impl Harness {
    fn new() -> Self {
        let ctx = Context::default();
        fonts::install(&ctx);
        theme::apply(&ctx, ThemeMode::Dark);
        Self { ctx, time: 0., repaint_delay: Duration::MAX }
    }

    /// Run one frame with `events`, returning what `draw` last returned.
    fn frame<R>(&mut self, events: Vec<Event>, mut draw: impl FnMut(&mut Ui) -> R) -> R {
        self.time += 1. / 60.;
        let raw = RawInput {
            screen_rect: Some(SCREEN),
            time: Some(self.time),
            events,
            ..RawInput::default()
        };
        let mut result = None;
        let output = self.ctx.run_ui(raw, |ui| result = Some(draw(ui)));
        self.repaint_delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        output.drop_without_applying_deltas();
        result.expect("the ui closure ran")
    }

    /// Run one frame with no input, returning every text it paints.
    fn texts(&mut self, mut draw: impl FnMut(&mut Ui)) -> Vec<String> {
        fn collect(shape: &egui::Shape, into: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => into.push(text.galley.text().to_owned()),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, into)),
                _ => (),
            }
        }
        self.time += 1. / 60.;
        let raw =
            RawInput { screen_rect: Some(SCREEN), time: Some(self.time), ..RawInput::default() };
        let mut output = self.ctx.run_ui(raw, |ui| draw(ui));
        let mut texts = Vec::new();
        for clipped in std::mem::take(&mut output.shapes) {
            collect(&clipped.shape, &mut texts);
        }
        output.drop_without_applying_deltas();
        texts
    }

    /// Settle hover at `at`, press and release, returning the action from the release frame.
    fn click<R>(&mut self, at: Pos2, mut draw: impl FnMut(&mut Ui) -> Option<R>) -> Option<R> {
        self.press_release(at, PointerButton::Primary, &mut draw)
    }

    fn press_release<R>(
        &mut self,
        at: Pos2,
        button: PointerButton,
        draw: &mut impl FnMut(&mut Ui) -> Option<R>,
    ) -> Option<R> {
        let pointer =
            |pressed| Event::PointerButton { pos: at, button, pressed, modifiers: Modifiers::NONE };
        self.frame(vec![Event::PointerMoved(at)], &mut *draw);
        self.frame(vec![], &mut *draw);
        self.frame(vec![pointer(true)], &mut *draw);
        self.frame(vec![pointer(false)], &mut *draw)
    }
}

fn tabs_vm(ids: &[u64]) -> Vec<TabView<'static>> {
    ids.iter()
        .map(|&id| TabView {
            protocol: None,
            id: TabId(id),
            label: "tab",
            glyph: rmx_ui::icons::TERMINAL,
            active: id == ids[0],
            status: PaneStatus::Connected,
        })
        .collect()
}

fn node(id: u64, depth: u16, kind: NodeKind, label: &str) -> TreeNode {
    TreeNode {
        id: NodeId(id),
        depth,
        kind,
        label: label.into(),
        state: Default::default(),
        expanded: true,
    }
}

fn big_tree(rows: u64) -> TreeModel {
    TreeModel::new(
        (0..rows)
            .map(|i| {
                let kind = if i % 100 == 0 {
                    NodeKind::Folder
                } else {
                    NodeKind::Connection(Protocol::Ssh)
                };
                node(i, u16::from(i % 100 != 0), kind, &format!("host-{i}"))
            })
            .collect(),
    )
}

#[test]
fn window_covers_only_rows_on_screen() {
    let view = window(0., 100_000, 28., 280.);
    assert_eq!((view.first, view.end, view.top), (0, 10, 0.));

    // Part way down, a partially hidden row at the top and bottom.
    let view = window(28. * 1000. + 14., 100_000, 28., 280.);
    assert_eq!((view.first, view.end), (1000, 1011));
    assert_eq!(view.top, -14.);
}

#[test]
fn window_stops_at_the_end_of_the_list() {
    let view = window(1e12, 100, 28., 280.);
    assert_eq!(view.end, 100);
    assert_eq!(view.first, 90);

    let view = window(0., 3, 28., 280.);
    assert_eq!((view.first, view.end), (0, 3));
    assert_eq!(virtual_list::max_scroll(3, 28., 280.), 0.);
}

#[test]
fn window_keeps_precision_deep_in_a_long_list() {
    // 2.8 million points down, an f32 scroll would resolve only quarter points.
    let view = window(99_000. * 28. + 0.125, 100_000, 28., 280.);
    assert_eq!(view.first, 99_000);
    assert_eq!(view.top, -0.125);
}

#[test]
fn scrolling_to_a_row_moves_as_little_as_possible() {
    assert_eq!(virtual_list::scroll_to_row(280., 12, 28., 280.), 280.);
    assert_eq!(virtual_list::scroll_to_row(280., 5, 28., 280.), 140.);
    assert_eq!(virtual_list::scroll_to_row(0., 20, 28., 280.), 21. * 28. - 280.);
}

#[test]
fn thumb_round_trips_scroll_position() {
    let (top, height) = virtual_list::thumb(500_000., 100_000, 28., 800., 24.);
    assert_eq!(height, 24.);
    let back = virtual_list::scroll_for_thumb(top, 100_000, 28., 800., 24.);
    assert!((back - 500_000.).abs() < 2_000., "{back}");
}

#[test]
fn only_visible_rows_become_widgets_in_a_huge_tree() {
    let mut harness = Harness::new();
    let mut tree = big_tree(100_000);
    let mut state = TreeState::default();
    state.scroll = 50_001. * f64::from(TREE_ROW_HEIGHT);
    let rect = Rect::from_min_size(pos2(0., 0.), vec2(260., 560.));

    let widgets_before =
        harness.frame(vec![], |ui| connection_tree(ui, rect, &mut tree, &mut state)).is_none();
    assert!(widgets_before);

    let first = window(state.scroll, tree.visible_len(), TREE_ROW_HEIGHT, 560.);
    assert_eq!(first.end - first.first, 20);
    // A click hits the row under it, however far down the list is.
    let action =
        harness.click(pos2(100., 14.), |ui| connection_tree(ui, rect, &mut tree, &mut state));
    let expected = tree.visible_node(first.first).id;
    assert_eq!(action, Some(UiAction::TreeSelect(expected)));
}

#[test]
fn folding_hides_a_subtree() {
    let mut tree = TreeModel::new(vec![
        node(1, 0, NodeKind::Folder, "a"),
        node(2, 1, NodeKind::Folder, "b"),
        node(3, 2, NodeKind::Connection(Protocol::Ssh), "c"),
        node(4, 0, NodeKind::Connection(Protocol::Ssh), "d"),
    ]);
    assert_eq!(tree.visible_len(), 4);
    assert!(!tree.toggle_row(0));
    assert_eq!(tree.visible_len(), 2);
    assert_eq!(tree.visible_node(1).id, NodeId(4));
    assert!(tree.toggle_row(0));
    assert_eq!(tree.visible_len(), 4);
}

#[test]
fn search_keeps_ancestors_of_matches() {
    let mut tree = TreeModel::new(vec![
        node(1, 0, NodeKind::Folder, "Prod"),
        node(2, 1, NodeKind::Folder, "Web"),
        node(3, 2, NodeKind::Connection(Protocol::Ssh), "alpha"),
        node(4, 2, NodeKind::Connection(Protocol::Ssh), "BETA"),
        node(5, 0, NodeKind::Folder, "Empty"),
        node(6, 1, NodeKind::Connection(Protocol::Ssh), "gamma"),
    ]);
    tree.set_query("Beta");
    let rows: Vec<_> = (0..tree.visible_len()).map(|i| tree.visible_node(i).id.0).collect();
    assert_eq!(rows, [1, 2, 4]);

    tree.set_query("");
    assert_eq!(tree.visible_len(), 6);

    tree.set_query("nothing");
    assert_eq!(tree.visible_len(), 0);
}

#[test]
fn search_does_not_match_across_labels() {
    let mut tree = TreeModel::new(vec![
        node(1, 0, NodeKind::Connection(Protocol::Ssh), "ab"),
        node(2, 0, NodeKind::Connection(Protocol::Ssh), "cd"),
    ]);
    tree.set_query("bc");
    assert_eq!(tree.visible_len(), 0);
}

#[test]
fn a_tab_drops_into_the_slot_under_its_center() {
    let width = 100.;
    let step = width + TAB_GAP;
    assert_eq!(drop_index(0., width, 5), 0);
    assert_eq!(drop_index(step / 2. - 1., width, 5), 0);
    assert_eq!(drop_index(step / 2. + 1., width, 5), 1);
    assert_eq!(drop_index(step * 3., width, 5), 3);
    // Past either end, the nearest slot.
    assert_eq!(drop_index(-500., width, 5), 0);
    assert_eq!(drop_index(5000., width, 5), 4);
}

#[test]
fn other_tabs_make_way_while_one_is_held() {
    // Tab 1 held over slot 3: tabs 2 and 3 move left, the rest stay.
    let slots: Vec<_> = (0..5).map(|i| preview_slot(i, 1, 3)).collect();
    assert_eq!(slots, [0, 3, 1, 2, 4]);
    // Tab 3 held over slot 1: tabs 1 and 2 move right.
    let slots: Vec<_> = (0..5).map(|i| preview_slot(i, 3, 1)).collect();
    assert_eq!(slots, [0, 2, 3, 1, 4]);
    // Held in place, nothing moves.
    let slots: Vec<_> = (0..5).map(|i| preview_slot(i, 2, 2)).collect();
    assert_eq!(slots, [0, 1, 2, 3, 4]);
}

#[test]
fn tabs_shrink_to_fit_then_stop() {
    assert_eq!(tab_width(2000., 3), 200.);
    let width = tab_width(830., 4);
    assert!(width < 200. && width > 96.);
    assert_eq!(tab_width(300., 30), 96.);
}

#[test]
fn slots_are_evenly_spaced() {
    assert_eq!(slot_x(0, 100.), 0.);
    assert_eq!(slot_x(3, 100.), 3. * (100. + TAB_GAP));
}

#[test]
fn dragging_a_tab_reports_its_new_place() {
    let mut harness = Harness::new();
    let tabs = tabs_vm(&[10, 11, 12, 13]);
    let mut state = TabStripState::default();
    let strip = Rect::from_min_size(pos2(0., 0.), vec2(900., STRIP_HEIGHT));
    let now = Instant::now();
    let mut draw = |ui: &mut Ui| tab_strip(ui, strip, &tabs, &mut state, now);

    let grab = pos2(20., 20.);
    let step = tab_width(900., 4) + TAB_GAP;
    let moved = |x: f32| Event::PointerMoved(pos2(x, 20.));
    let button = |pressed, x| Event::PointerButton {
        pos: pos2(x, 20.),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    harness.frame(vec![moved(grab.x)], &mut draw);
    harness.frame(vec![], &mut draw);
    harness.frame(vec![button(true, grab.x)], &mut draw);

    // Move past the drag threshold, then across two slots, then release.
    harness.frame(vec![moved(grab.x + 20.)], &mut draw);
    harness.frame(vec![moved(grab.x + step)], &mut draw);
    harness.frame(vec![moved(grab.x + step * 2.)], &mut draw);
    harness.frame(vec![], &mut draw);
    let action = harness.frame(vec![button(false, grab.x + step * 2.)], &mut draw);
    assert_eq!(action, Some(UiAction::TabReorder { id: TabId(10), to: 2 }));
}

#[test]
fn clicking_a_tab_activates_it_and_its_close_box_closes_it() {
    let mut harness = Harness::new();
    let tabs = tabs_vm(&[1, 2]);
    let mut state = TabStripState::default();
    let strip = Rect::from_min_size(pos2(0., 0.), vec2(900., STRIP_HEIGHT));
    let width = tab_width(900., 2);
    let now = Instant::now();
    let mut draw = |ui: &mut Ui| tab_strip(ui, strip, &tabs, &mut state, now);

    let second_tab = pos2(slot_x(1, width) + 30., 20.);
    assert_eq!(harness.click(second_tab, &mut draw), Some(UiAction::TabActivate(TabId(2))));

    let close = pos2(slot_x(0, width) + width - 14., 20.);
    assert_eq!(harness.click(close, &mut draw), Some(UiAction::TabClose(TabId(1))));

    let new = pos2(slot_x(2, width) + 14., 20.);
    assert_eq!(harness.click(new, &mut draw), Some(UiAction::TabNew));
}

#[test]
fn a_new_tab_arrangement_slides_rather_than_jumps() {
    let mut harness = Harness::new();
    let mut state = TabStripState::default();
    let strip = Rect::from_min_size(pos2(0., 0.), vec2(900., STRIP_HEIGHT));
    let now = Instant::now();

    let tabs = tabs_vm(&[1, 2, 3]);
    for _ in 0..4 {
        harness.frame(vec![], |ui| tab_strip(ui, strip, &tabs, &mut state, now));
    }
    assert_eq!(harness.repaint_delay, Duration::MAX, "at rest, nothing asks to run");

    // Closing the first tab pulls the others left, animated.
    let tabs = tabs_vm(&[2, 3]);
    harness.frame(vec![], |ui| tab_strip(ui, strip, &tabs, &mut state, now));
    assert_eq!(harness.repaint_delay, Duration::ZERO, "the slide keeps egui running");
}

#[test]
fn title_bar_routes_its_controls() {
    let mut harness = Harness::new();
    let tabs = tabs_vm(&[1]);
    let mut state = TabStripState::default();
    let bar = Rect::from_min_size(pos2(0., 0.), vec2(SCREEN.width(), STRIP_HEIGHT));
    let view =
        TitleBarView { tabs: &tabs, maximized: false, sidebar_open: true, sidebar_width: 260. };
    let now = Instant::now();
    let mut draw = |ui: &mut Ui| title_bar(ui, bar, &view, &mut state, now);

    let at = |right_offset: f32| pos2(SCREEN.right() - right_offset, 20.);
    assert_eq!(harness.click(at(20.), &mut draw), Some(UiAction::WindowClose));
    assert_eq!(harness.click(at(52.), &mut draw), Some(UiAction::WindowToggleMaximize));
    assert_eq!(harness.click(at(84.), &mut draw), Some(UiAction::WindowMinimize));
    assert_eq!(harness.click(pos2(20., 20.), &mut draw), Some(UiAction::ToggleSidebar));
}

#[test]
fn title_bar_empty_space_drags_the_window() {
    let mut harness = Harness::new();
    let tabs = tabs_vm(&[1]);
    let mut state = TabStripState::default();
    let bar = Rect::from_min_size(pos2(0., 0.), vec2(SCREEN.width(), STRIP_HEIGHT));
    let view =
        TitleBarView { tabs: &tabs, maximized: false, sidebar_open: true, sidebar_width: 260. };
    let now = Instant::now();
    let mut draw = |ui: &mut Ui| title_bar(ui, bar, &view, &mut state, now);

    let at = pos2(SCREEN.right() - 200., 20.);
    let press = Event::PointerButton {
        pos: at,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    };
    harness.frame(vec![Event::PointerMoved(at)], &mut draw);
    harness.frame(vec![], &mut draw);
    harness.frame(vec![press], &mut draw);
    let action = harness.frame(vec![Event::PointerMoved(at + vec2(16., 0.))], &mut draw);
    assert_eq!(action, Some(UiAction::WindowDrag));
}

#[test]
fn dialog_steps_from_tiles_to_form_and_cancels() {
    let mut harness = Harness::new();
    let mut state = NewConnectionState::default();
    state.open();
    // Let the float size itself and finish appearing, so it sits where it will stay.
    let ctx = harness.ctx.clone();
    for _ in 0..12 {
        harness.frame(vec![], |_| new_connection_dialog(&ctx, &mut state, &[]));
    }

    // The first tile is at the dialog's top-left, under the header; the dialog is centered.
    let center = SCREEN.center();
    let dialog_left = center.x - 270.;
    let height = 52. + 16. + 84. * 2. + 8. + 16. + 1. + 56.;
    let tile = pos2(dialog_left + 40., center.y - height / 2. + 52. + 16. + 40.);
    let action = harness.click(tile, |ui| new_connection_dialog(ui.ctx(), &mut state, &[]));
    assert_eq!(action, None);
    assert_eq!(state.draft_mut().map(|draft| draft.protocol), Some(Protocol::Ssh));

    // Escape dismisses, whichever step it is on.
    let escape = Event::Key {
        key: egui::Key::Escape,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    };
    let action = harness.frame(vec![escape], |ui| new_connection_dialog(ui.ctx(), &mut state, &[]));
    assert_eq!(action, Some(UiAction::ConnectionCancel));
    assert!(!state.is_open());
}

/// Every piece of text a frame painted.
fn painted_text(harness: &mut Harness, mut draw: impl FnMut(&mut Ui)) -> Vec<String> {
    harness.time += 1. / 60.;
    let raw =
        RawInput { screen_rect: Some(SCREEN), time: Some(harness.time), ..RawInput::default() };
    let output = harness.ctx.run_ui(raw, |ui| draw(ui));
    fn walk(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => (),
        }
    }
    let mut out = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    output.drop_without_applying_deltas();
    out
}

#[test]
fn the_form_says_which_folder_a_login_is_inherited_from() {
    let mut harness = Harness::new();
    let logins = [
        InheritedLogin {
            group: "Production".to_owned(),
            username: "admin".to_owned(),
            password: true,
            key: false,
        },
        InheritedLogin {
            group: "Staging".to_owned(),
            username: "other".to_owned(),
            ..Default::default()
        },
    ];
    let mut draft = ConnectionDraft::new(Protocol::Ssh);
    draft.group = "Production / Web".to_owned();
    let mut state = NewConnectionState::default();
    state.open_draft(draft);
    let ctx = harness.ctx.clone();
    for _ in 0..12 {
        harness.frame(vec![], |_| new_connection_dialog(&ctx, &mut state, &logins));
    }
    let text = painted_text(&mut harness, |_| {
        new_connection_dialog(&ctx, &mut state, &logins);
    });
    let inherited = text.iter().find(|t| t.contains("Inherited from folder Production"));
    assert!(inherited.is_some_and(|t| t.contains("admin")), "{text:?}");
    assert!(!text.iter().any(|t| t == "Username"), "its own fields are hidden: {text:?}");

    // Overriding shows its own fields again.
    state.draft_mut().unwrap().inherit = false;
    let text = painted_text(&mut harness, |_| {
        new_connection_dialog(&ctx, &mut state, &logins);
    });
    assert!(text.iter().any(|t| t == "Username"), "{text:?}");
    assert!(!text.iter().any(|t| t.contains("Inherited from")), "{text:?}");
}

#[test]
fn pane_bar_routes_its_buttons() {
    let mut harness = Harness::new();
    let rect = Rect::from_min_size(pos2(0., 0.), vec2(800., PANE_BAR_HEIGHT));
    let view = PaneBarView {
        pane: PaneId(7),
        status: PaneStatus::Connected,
        title: "t",
        tooltip: "",
        focused: true,
        grouped: true,
        zoomed: false,
        files: false,
        broadcast: None,
    };
    let mut reveal = Reveal::default();
    let mut draw = |ui: &mut Ui| pane_bar(ui, rect, &view, &mut reveal, std::time::Instant::now());
    let y = rect.center().y;
    // Shown once the pointer is there. From the right: close, zoom, split down, split right, each
    // 24 px and 2 px apart.
    harness.click(pos2(800. - 14., y), &mut draw);
    assert_eq!(harness.click(pos2(800. - 14., y), &mut draw), Some(UiAction::PaneClose(PaneId(7))));
    assert_eq!(harness.click(pos2(800. - 40., y), &mut draw), Some(UiAction::PaneZoom(PaneId(7))));
    assert_eq!(
        harness.click(pos2(800. - 66., y), &mut draw),
        Some(UiAction::PaneSplitDown(PaneId(7)))
    );
    assert_eq!(
        harness.click(pos2(800. - 92., y), &mut draw),
        Some(UiAction::PaneSplitRight(PaneId(7)))
    );
}

#[test]
fn a_terminal_pane_bar_toggles_broadcast_and_shows_it() {
    let mut harness = Harness::new();
    let rect = Rect::from_min_size(pos2(0., 0.), vec2(800., PANE_BAR_HEIGHT));
    let mut view = PaneBarView {
        pane: PaneId(7),
        status: PaneStatus::Connected,
        title: "t",
        tooltip: "",
        focused: true,
        grouped: false,
        zoomed: false,
        files: false,
        broadcast: Some(false),
    };
    let mut reveal = Reveal::default();
    let y = rect.center().y;
    // Close, split down, split right, then broadcast.
    let at = pos2(800. - 14. - 3. * 26., y);
    let mut draw = |ui: &mut Ui| pane_bar(ui, rect, &view, &mut reveal, std::time::Instant::now());
    harness.click(at, &mut draw);
    assert_eq!(harness.click(at, &mut draw), Some(UiAction::PaneBroadcast(PaneId(7))));

    // In the group, the bar says so, wherever the pointer is.
    view.broadcast = Some(true);
    let mut reveal = Reveal::default();
    let shown = harness.texts(|ui| {
        pane_bar(ui, rect, &view, &mut reveal, Instant::now());
    });
    assert!(shown.iter().any(|text| text == "Broadcast"), "no badge in {shown:?}");
}

fn core() -> ChromeCore {
    ChromeCore::new(ThemeMode::Dark, CjkFallback::none())
}

const SIZE: [u32; 2] = [1920, 1080];

#[test]
fn chrome_runs_egui_only_when_something_changed() {
    let mut core = core();
    let start = Instant::now();
    let mut runs = 0;
    assert!(core.update(start, SIZE, 1., |_| runs += 1));
    assert_eq!(runs, 1);

    // egui settles after a frame or two. Then nothing changes, and nothing runs, however often
    // the app comes back (as it does for every burst of terminal output).
    let mut settled = start;
    while core.needs_update(settled) {
        settled += Duration::from_millis(5);
        core.update(settled, SIZE, 1., |_| runs += 1);
    }
    let before = runs;
    for i in 0..1000 {
        assert!(!core.update(settled + Duration::from_micros(i), SIZE, 1., |_| runs += 1));
    }
    assert_eq!(runs, before);

    core.invalidate();
    assert!(core.update(settled, SIZE, 1., |_| runs += 1));
    assert_eq!(runs, before + 1);

    // A changed window size is a change.
    settled += Duration::from_secs(1);
    assert!(core.update(settled, [1000, 1000], 1., |_| runs += 1));
}

#[test]
fn chrome_runs_for_input_and_keeps_the_frame_cached() {
    let mut core = core();
    let start = Instant::now();
    core.update(start, SIZE, 1., |ui| {
        ui.painter().rect_filled(
            Rect::from_min_size(Pos2::ZERO, vec2(5., 5.)),
            0.,
            egui::Color32::RED,
        );
    });
    assert_eq!(core.primitives().len(), 1);

    let mut settled = start;
    while core.needs_update(settled) {
        settled += Duration::from_millis(5);
        core.update(settled, SIZE, 1., |ui| {
            ui.painter().rect_filled(
                Rect::from_min_size(Pos2::ZERO, vec2(5., 5.)),
                0.,
                egui::Color32::RED,
            );
        });
    }
    assert_eq!(core.primitives().len(), 1, "the cached frame outlives the passes that made it");

    core.push_event(Event::PointerMoved(pos2(5., 5.)));
    assert!(core.needs_update(settled));
}

#[test]
fn pointer_over_panes_does_not_run_egui() {
    use winit::dpi::PhysicalPosition;
    use winit::event::{DeviceId, WindowEvent};

    let mut core = core();
    let start = Instant::now();
    core.set_pane_area(Some(Rect::from_min_max(pos2(260., 40.), pos2(1920., 1080.))));
    let mut settled = start;
    core.update(settled, SIZE, 1., |_| {});
    while core.needs_update(settled) {
        settled += Duration::from_millis(5);
        core.update(settled, SIZE, 1., |_| {});
    }

    let device_id = DeviceId::dummy();
    let moved = |x: f64, y: f64| WindowEvent::CursorMoved {
        device_id,
        position: PhysicalPosition::new(x, y),
    };

    // The first move into a pane is passed on, to clear any hover on the way out of the shell.
    assert!(core.window_event(&moved(800., 500.)));
    core.update(settled, SIZE, 1., |_| {});
    while core.needs_update(settled) {
        settled += Duration::from_millis(5);
        core.update(settled, SIZE, 1., |_| {});
    }

    // From then on motion inside panes costs nothing.
    assert!(!core.window_event(&moved(810., 510.)));
    assert!(!core.needs_update(settled));

    // Crossing back onto the shell runs egui again.
    assert!(core.window_event(&moved(100., 500.)));
    assert!(core.needs_update(settled));
}

#[test]
fn closing_the_palette_gives_the_keyboard_back() {
    let items = [QuickItem { id: 0, glyph: 'x', label: "Split right", detail: "Command" }];
    let mut harness = Harness::new();
    let mut state = QuickOpenState::default();
    state.open();
    let ctx = harness.ctx.clone();
    harness.frame(Vec::new(), |_| quick_open(&ctx, &mut state, &items));
    harness.frame(Vec::new(), |_| quick_open(&ctx, &mut state, &items));
    assert!(ctx.egui_wants_keyboard_input());

    state.close();
    harness.frame(Vec::new(), |_| quick_open(&ctx, &mut state, &items));
    harness.frame(Vec::new(), |_| quick_open(&ctx, &mut state, &items));
    assert!(!ctx.egui_wants_keyboard_input());
}

/// The middle of the first painted text that reads `needle`.
fn text_center(harness: &mut Harness, mut draw: impl FnMut(&mut Ui), needle: &str) -> Option<Pos2> {
    fn walk(shape: &egui::Shape, needle: &str, out: &mut Option<Pos2>) {
        match shape {
            egui::Shape::Text(text) if out.is_none() && text.galley.text() == needle => {
                *out = Some(text.pos + text.galley.rect.center().to_vec2());
            },
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, needle, out)),
            _ => (),
        }
    }
    harness.time += 1. / 60.;
    let raw =
        RawInput { screen_rect: Some(SCREEN), time: Some(harness.time), ..RawInput::default() };
    let output = harness.ctx.run_ui(raw, |ui| draw(ui));
    let mut found = None;
    for clipped in &output.shapes {
        walk(&clipped.shape, needle, &mut found);
    }
    output.drop_without_applying_deltas();
    found
}

#[test]
fn a_confirmation_can_carry_a_tick_box_that_the_answer_keeps() {
    let mut harness = Harness::new();
    let mut state = PromptState::new("Disconnect 1 live session?", "web", Vec::new());
    state.check = Some("Don't ask again".to_owned());
    // A modal sizes itself on its first frames.
    for _ in 0..3 {
        harness.frame(vec![], |ui| drop(prompt_dialog(ui.ctx(), &mut state)));
    }
    let texts = painted_text(&mut harness, |ui| {
        prompt_dialog(ui.ctx(), &mut state);
    });
    assert!(texts.iter().any(|text| text == "Don't ask again"), "{texts:?}");
    assert!(!state.checked);

    let at = text_center(
        &mut harness,
        |ui| drop(prompt_dialog(ui.ctx(), &mut state)),
        "Don't ask again",
    )
    .expect("the tick box's label is painted");
    let action = harness.click(at, |ui| prompt_dialog(ui.ctx(), &mut state));
    assert!(action.is_none(), "ticking the box answers nothing: {action:?}");
    assert!(state.checked);

    let submit =
        text_center(&mut harness, |ui| drop(prompt_dialog(ui.ctx(), &mut state)), "Continue")
            .expect("the submit button is painted");
    let action = harness.click(submit, |ui| prompt_dialog(ui.ctx(), &mut state));
    assert!(matches!(action, Some(UiAction::PromptSubmit(_))), "{action:?}");
    assert!(state.checked, "the app reads the box once the answer is in");
}

#[test]
fn a_prompt_without_a_tick_box_paints_none() {
    let mut harness = Harness::new();
    let mut state = PromptState::new("Delete web?", "", Vec::new());
    for _ in 0..3 {
        harness.frame(vec![], |ui| drop(prompt_dialog(ui.ctx(), &mut state)));
    }
    let texts = painted_text(&mut harness, |ui| {
        prompt_dialog(ui.ctx(), &mut state);
    });
    assert!(!texts.iter().any(|text| text == "Don't ask again"), "{texts:?}");
}

/// Every painted text and the middle of where it is.
fn placed_texts(harness: &mut Harness, mut draw: impl FnMut(&mut Ui)) -> Vec<(String, Pos2)> {
    fn walk(shape: &egui::Shape, out: &mut Vec<(String, Pos2)>) {
        match shape {
            egui::Shape::Text(text) => out.push((
                text.galley.text().to_owned(),
                text.pos + text.galley.rect.center().to_vec2(),
            )),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => (),
        }
    }
    harness.time += 1. / 60.;
    let raw =
        RawInput { screen_rect: Some(SCREEN), time: Some(harness.time), ..RawInput::default() };
    let output = harness.ctx.run_ui(raw, |ui| draw(ui));
    let mut out = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    output.drop_without_applying_deltas();
    out
}

#[test]
fn settings_turn_the_question_before_closing_off_and_on() {
    let mut harness = Harness::new();
    let mut state = SettingsState::default();
    state.open(SettingsDraft {
        theme: ThemeMode::Dark,
        confirm_close: true,
        auto_lock_minutes: "15".to_owned(),
        sync: SyncDraft::default(),
        sync_on_save: true,
        sync_interval_minutes: String::new(),
    });
    let view = SettingsView { vault: "vault.rmxdb", syncing: false, unencrypted: false };
    let mut panels = Panels::default();
    let mut draw = |ui: &mut Ui| settings_dialog(ui.ctx(), &mut state, &view, &mut panels);
    for _ in 0..3 {
        harness.frame(vec![], |ui| drop(draw(ui)));
    }
    let texts = placed_texts(&mut harness, |ui| drop(draw(ui)));
    let (_, label) =
        texts.iter().find(|(text, _)| text == "Confirm close").expect("the setting is shown");
    // Its own Off, on the same line.
    let (_, off) = texts
        .iter()
        .filter(|(text, at)| text == "Off" && (at.y - label.y).abs() < 4.)
        .min_by(|a, b| (a.1.x - label.x).total_cmp(&(b.1.x - label.x)))
        .expect("the setting has an Off");
    let off = *off;
    harness.click(off, &mut draw);
    let texts = placed_texts(&mut harness, |ui| drop(draw(ui)));
    let (_, save) = texts.iter().find(|(text, _)| text == "Save").expect("Save is shown");
    let action = harness.click(*save, &mut draw);
    let Some(UiAction::SettingsSave(draft)) = action else { panic!("{action:?}") };
    assert!(!draft.confirm_close);
}
