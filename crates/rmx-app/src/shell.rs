//! The interface around the panes: what the chrome shows, and what it asks of the app.
//!
//! Everything here runs inside an egui pass, which [`rmx_ui::Chrome`] only runs on input, while
//! something animates, or when the app says the state shown has changed.

use std::path::Path;
use std::time::Instant;

use rmx_layout::anim::{Curve, Tween};
use rmx_layout::dnd::{Scene, Source, Target};
use rmx_layout::motion::Motion;
use rmx_layout::split::Axis;
use rmx_layout::workspace::{Direction, Tab, Workspace};
use rmx_layout::{PaneId, Rect};
use rmx_ui::egui::{
    self, Color32, Context, CornerRadius, Id, LayerId, Order, Stroke, StrokeKind, Ui, pos2, vec2,
};
use rmx_ui::theme::{self, PANE_BAR_HEIGHT, Palette, RADIUS, RADIUS_SM, SIDEBAR_MAX, SIDEBAR_MIN, SIDEBAR_WIDTH, STRIP_HEIGHT};
use rmx_ui::widgets::{
    ContextMenu, FilesKey, FilesState, HostKeyView, MenuItem, NewConnectionState, NodeKind, PaneBarView,
    Panels, PromptState, QuickItem, QuickOpenState, Reveal, SIDEBAR_STATUS_HEIGHT, SessionDot, SettingsState,
    SettingsView, SidebarStatus, SnippetRow, SnippetsState, TabStripState, TabView, TitleBarView, TreeModel,
    TreeState, VaultSetupState, VaultSetupView, VaultState, VaultView, context_menu, empty_state, files_pane,
    host_key_dialog, new_connection_dialog, paint_pill, pane_bar, pane_notice, pane_overlay, prompt_dialog,
    quick_open, resize_handle, settings_dialog, sidebar, sidebar_status, snippets_dialog, title_bar, vault_setup,
    vault_unlock,
};
use rmx_ui::{FileSide, NodeId, Protocol, TabId, UiAction, fonts, icons};
use std::collections::HashMap;
use std::sync::Arc;

use crate::app::Action;
use crate::connect::{Goal, HostKeyPrompt};
use crate::pane::Pane;
use crate::store::{NodeInfo, Phase, SyncState, TreeUpdate};

/// How long the drop preview takes to reach a new place, in milliseconds.
const PREVIEW_MOVE_MS: u64 = 140;
/// How long it takes to appear or go, in milliseconds.
const PREVIEW_FADE_MS: u64 = 100;
/// Thickness of the bar marking a slot between tabs, in points.
const SLOT_BAR: f32 = 3.;
/// Width of the label following a dragged pane, in points.
const DRAG_LABEL: f32 = 180.;
/// Opacity of the chrome over a pane being dragged.
const VEIL: f32 = 0.4;
/// How long a divider's pill takes to show, hide or grow, in milliseconds.
const PILL_MS: u64 = 140;
/// The least room the sidebar leaves the panes, in points.
const PANES_MIN: f32 = 240.;

/// What the shell needs from the app, for one pass.
pub struct ShellView<'a> {
    /// Which window this is.
    pub window: u64,
    pub workspace: &'a Workspace,
    pub panes: &'a [Pane],
    pub rects: &'a [(PaneId, Rect)],
    pub motion: &'a Motion,
    pub now: Instant,
    /// The space the panes tile, in pixels.
    pub area: Rect,
    /// Pixels between panes.
    pub spacing: f32,
    /// The height of a pane's bar, in pixels.
    pub bar: f32,
    /// The smallest pane, in pixels.
    pub min_pane: (f32, f32),
    pub maximized: bool,
}

/// A pane dropped somewhere it can go.
pub struct PaneDrop {
    pub pane: PaneId,
    pub source: Source,
    pub target: Target,
}

struct PaneDrag {
    pane: PaneId,
    source: Source,
}

/// The rectangle showing where a drag would land, sliding between places and fading in and out.
struct Preview {
    frame: Option<Tween<Rect>>,
    alpha: Tween<f32>,
}

impl Default for Preview {
    fn default() -> Self {
        Self { frame: None, alpha: Tween::new(0.) }
    }
}

impl Preview {
    /// Show `rect`, or nothing when none, in pixels.
    fn show(&mut self, rect: Option<Rect>, now: Instant) {
        let Some(rect) = rect else {
            self.alpha.to(0., now, PREVIEW_FADE_MS, Curve::EASE_OUT);
            return;
        };
        match &mut self.frame {
            Some(frame) if self.alpha.get(now) > 0.01 => {
                frame.to(rect, now, PREVIEW_MOVE_MS, Curve::SNAP);
            },
            frame => *frame = Some(Tween::new(rect)),
        }
        self.alpha.to(1., now, PREVIEW_FADE_MS, Curve::EASE_OUT);
    }

    fn running(&self, now: Instant) -> bool {
        self.alpha.running(now) || self.frame.is_some_and(|frame| frame.running(now))
    }

    fn paint(&self, ctx: &Context, scale: f32, palette: &Palette, now: Instant) {
        let (Some(frame), alpha) = (self.frame, self.alpha.get(now)) else { return };
        if alpha <= 0. {
            return;
        }
        let rect = points(frame.get(now), scale);
        let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique("drop-preview")));
        let radius = CornerRadius::same(RADIUS);
        painter.rect_filled(rect, radius, palette.foreground.gamma_multiply(0.10 * alpha));
        let stroke = Stroke::new(1.5, palette.foreground.gamma_multiply(0.4 * alpha));
        painter.rect_stroke(rect, radius, stroke, StrokeKind::Inside);
    }
}

/// The pill on the divider under the pointer.
struct Pill {
    /// The divider's band in points, whether it stands upright, and the pointer's place along it.
    place: Option<(egui::Rect, bool, f32)>,
    /// How shown it is: 0 hidden, 1 hovered, 2 dragged.
    level: Tween<f32>,
    target: f32,
}

impl Default for Pill {
    fn default() -> Self {
        Self { place: None, level: Tween::new(0.), target: 0. }
    }
}

/// What keeps the panes from being shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Waiting for the vault's thread to say what there is.
    Starting,
    Setup(String),
    Locked(String),
    Open,
}

impl From<&Phase> for Gate {
    fn from(phase: &Phase) -> Self {
        match phase {
            Phase::Setup(path) => Self::Setup(path.display().to_string()),
            Phase::Locked(path) => Self::Locked(path.display().to_string()),
            Phase::Unlocked { .. } => Self::Open,
        }
    }
}

/// The question on screen.
pub enum Asking {
    HostKey(HostKeyPrompt),
    Fields(PromptState),
}

/// A line of the palette and what picking it does.
struct PaletteEntry {
    label: String,
    detail: &'static str,
    glyph: char,
    pick: Pick,
}

enum Pick {
    Run(Action),
    SelectTab(usize),
    /// A connection, for when connections exist.
    Connection(u64),
    /// A snippet, to send to the focused terminal.
    Snippet(u64),
}

/// `path` with the home directory as `~`.
pub fn short_path(path: &str) -> String {
    let home = std::env::home_dir();
    let home = home.as_deref().and_then(Path::to_str);
    match home.and_then(|home| path.strip_prefix(home)) {
        Some(rest) if rest.starts_with(['/', '\\']) => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// What the interface of every window shows alike.
pub struct Common {
    pub tree: TreeModel,
    tree_state: TreeState,
    pub nodes: HashMap<u64, NodeInfo>,
    pub gate: Gate,
    unlock: VaultState,
    setup: VaultSetupState,
    /// The open database has no master password: nothing can be locked, and it says so.
    pub unencrypted: bool,
    /// A key is being derived.
    pub vault_busy: bool,
    pub vault_error: Option<String>,
    pub sync: SyncState,
    /// The outcome of the last thing done, shown instead of the state of sync.
    pub status_note: Option<(String, bool)>,
    pub asking: Option<Asking>,
    /// The question is about a session, which the lock screen has out of sight.
    pub asking_hidden: bool,
    /// Which window shows the question: the one of the pane it is about.
    pub asking_window: u64,
    /// Where the user is in each file pane, which goes with the pane between windows.
    files: HashMap<PaneId, FilesState>,
    /// The vault open or being asked about, and the others this device knows.
    pub vault_path: String,
    pub vault_others: Vec<String>,
    /// The vault's snippets. A new list is a new `Arc`, which is how the dialog tells.
    pub snippets: Arc<Vec<SnippetRow>>,
    /// The sizes the panels were dragged to, the same in every window.
    pub panels: Panels,
}

impl Default for Common {
    fn default() -> Self {
        Self {
            tree: TreeModel::default(),
            tree_state: TreeState::default(),
            nodes: HashMap::new(),
            gate: Gate::Starting,
            unlock: VaultState::default(),
            setup: VaultSetupState::default(),
            unencrypted: false,
            vault_busy: false,
            vault_error: None,
            sync: SyncState::Off,
            status_note: None,
            asking: None,
            asking_hidden: false,
            asking_window: 0,
            files: HashMap::new(),
            vault_path: String::new(),
            vault_others: Vec::new(),
            snippets: Arc::default(),
            panels: Panels::default(),
        }
    }
}

impl Common {
    /// With `window` to show questions in.
    pub fn for_window(window: u64) -> Self {
        Self { asking_window: window, ..Self::default() }
    }

    /// The panes are out of sight until the vault is open.
    pub fn locked(&self) -> bool {
        self.gate != Gate::Open
    }

    /// A column of file pane `pane` moved: what was selected there is gone.
    pub fn files_reset(&mut self, pane: PaneId, side: FileSide) {
        self.files.entry(pane).or_default().reset_side(side);
    }

    /// Select `name` in a column of file pane `pane`, as a click on it would.
    #[cfg(feature = "bench")]
    pub fn files_select(&mut self, pane: PaneId, side: FileSide, name: &str) {
        self.files.entry(pane).or_default().select(side, name);
    }

    /// Whether file pane `pane` has edits that would be lost.
    pub fn files_dirty(&self, pane: PaneId) -> bool {
        self.files.get(&pane).is_some_and(FilesState::dirty)
    }

    /// Let go of the edits in file pane `pane`.
    pub fn files_forget_edits(&mut self, pane: PaneId) {
        if let Some(files) = self.files.get_mut(&pane) {
            files.forget_edits();
        }
    }

    /// The files to compare from the menu of `name` in a column of file pane `pane`.
    pub fn files_compare_pair(
        &self,
        pane: PaneId,
        view: &rmx_ui::widgets::FilesView<'_>,
        side: FileSide,
        name: &Arc<str>,
    ) -> Option<(Arc<str>, Arc<str>)> {
        self.files.get(&pane)?.compare_pair(view, Some((side, name)))
    }

    /// What is selected in a column of file pane `pane`, or else `name`.
    pub fn files_selected_or(&self, pane: PaneId, side: FileSide, name: &Arc<str>) -> Vec<Arc<str>> {
        let selected = self.files.get(&pane).map_or_else(Vec::new, |files| files.selected(side));
        if selected.is_empty() || !selected.contains(name) { vec![name.clone()] } else { selected }
    }

    /// What is selected in a column of file pane `pane`.
    #[cfg(feature = "bench")]
    pub fn files_selected(&mut self, pane: PaneId, side: FileSide) -> Vec<std::sync::Arc<str>> {
        self.files.entry(pane).or_default().selected(side)
    }

    /// Show a freshly built tree, keeping the search and the green dots.
    pub fn set_tree(&mut self, update: TreeUpdate, panes: &[Pane]) {
        let query = self.tree.query().to_owned();
        let TreeUpdate { model, nodes, snippets } = update;
        self.tree = model;
        self.nodes = nodes;
        self.snippets = Arc::new(snippets);
        self.tree.set_query(&query);
        for pane in panes {
            if let Some(remote) = &pane.remote {
                self.tree.set_state(NodeId(remote.node), session_dot(panes, remote.node));
            }
        }
    }

    /// A key was pressed with file pane `pane` focused, showing `view`.
    pub fn files_key(
        &mut self,
        pane: PaneId,
        key: FilesKey,
        extend: bool,
        view: &rmx_ui::widgets::FilesView<'_>,
    ) -> Option<rmx_ui::FilesAction> {
        self.files.entry(pane).or_default().key(key, extend, view)
    }

    /// What the foot of the sidebar says.
    fn status_line(&self) -> SidebarStatus<'_> {
        let can_sync = !matches!(self.sync, SyncState::Off);
        let (text, failed): (&str, bool) = match (&self.status_note, &self.sync) {
            (_, SyncState::Running) => ("Syncing...", false),
            (_, SyncState::Failed(reason)) => (reason, true),
            (Some((note, bad)), _) => (note, *bad),
            (None, SyncState::Done(text)) => (text, false),
            (None, _) => ("", false),
        };
        let busy = matches!(self.sync, SyncState::Running);
        SidebarStatus { unencrypted: self.unencrypted, text, failed, busy, can_sync }
    }
}

/// How the sessions to `node` are doing, the best of them.
pub fn session_dot(panes: &[Pane], node: u64) -> SessionDot {
    use crate::pane::Link;
    let links = || panes.iter().filter(|pane| pane.remote.as_ref().is_some_and(|r| r.node == node)).map(|pane| &pane.link);
    if links().any(|link| *link == Link::Live) {
        SessionDot::Live
    } else if links().any(|link| matches!(link, Link::Connecting { .. })) {
        SessionDot::Connecting
    } else if links().any(|link| matches!(link, Link::Failed { .. })) {
        SessionDot::Failed
    } else if links().any(|link| matches!(link, Link::Closed { .. })) {
        SessionDot::Closed
    } else {
        SessionDot::None
    }
}

/// The state of everything around the panes of one window.
pub struct Shell {
    pub sidebar_open: bool,
    /// How wide the sidebar is, in points.
    pub sidebar_width: f32,
    /// How far along the controls of each pane's bar have come.
    bars: HashMap<PaneId, Reveal>,
    pill: Pill,
    tabs: TabStripState,
    pub settings: SettingsState,
    pub quick: QuickOpenState,
    palette: Vec<PaletteEntry>,
    pub dialog: NewConnectionState,
    pub snippets: SnippetsState,
    menu: ContextMenu,
    menu_items: Vec<MenuItem>,
    drag: Option<PaneDrag>,
    preview: Preview,
    /// What the last pass asked for.
    pub actions: Vec<UiAction>,
    /// A drag that ended over somewhere it can go.
    pub dropped: Option<PaneDrop>,
}

impl Default for Shell {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            sidebar_width: SIDEBAR_WIDTH,
            bars: HashMap::new(),
            pill: Pill::default(),
            tabs: TabStripState::default(),
            settings: SettingsState::default(),
            quick: QuickOpenState::default(),
            palette: Vec::new(),
            dialog: NewConnectionState::default(),
            snippets: SnippetsState::default(),
            menu: ContextMenu::default(),
            menu_items: Vec::new(),
            drag: None,
            preview: Preview::default(),
            actions: Vec::new(),
            dropped: None,
        }
    }
}

fn points(rect: Rect, scale: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        pos2(rect.x / scale, rect.y / scale),
        vec2(rect.w / scale, rect.h / scale),
    )
}

fn pixels(rect: egui::Rect, scale: f32) -> Rect {
    Rect::new(rect.left() * scale, rect.top() * scale, rect.width() * scale, rect.height() * scale)
}

/// The part of a pane's panel that its content takes, below its bar.
pub fn terminal_rect(panel: Rect, bar: f32) -> Rect {
    Rect::new(panel.x, panel.y + bar, panel.w, (panel.h - bar).max(1.))
}

/// The part of a pane's panel its content takes, in points.
pub fn content_points(panel: Rect, bar: f32, scale: f32) -> egui::Rect {
    points(terminal_rect(panel, bar), scale)
}

/// The bar of a pane's panel, in points.
pub fn bar_rect(panel: Rect, scale: f32) -> egui::Rect {
    let rect = points(panel, scale);
    egui::Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + PANE_BAR_HEIGHT))
}

fn pane_title(panes: &[Pane], id: PaneId) -> &str {
    panes.iter().find(|pane| pane.id == id).map_or("local shell", Pane::label)
}

/// The icon of what the tab has focused: of its kind of connection, a terminal otherwise.
fn tab_protocol(panes: &[Pane], common: &Common, tab: &Tab) -> Option<Protocol> {
    let remote = panes.iter().find(|pane| pane.id == tab.focus)?.remote.as_ref()?;
    let protocol = common.nodes.get(&remote.node).and_then(|info| match info.kind {
        NodeKind::Connection(protocol) => Some(protocol),
        NodeKind::Folder => None,
    })?;
    // Files opened from a shell are not a shell.
    Some(if protocol == Protocol::Ssh && matches!(remote.goal, Goal::Files { .. }) { Protocol::Sftp } else { protocol })
}

fn tab_glyph(panes: &[Pane], common: &Common, tab: &Tab) -> char {
    tab_protocol(panes, common, tab).map_or(icons::TERMINAL, Protocol::icon)
}

fn tab_label(panes: &[Pane], tab: &Tab) -> String {
    if let Some(name) = &tab.name {
        return name.clone();
    }
    let title = pane_title(panes, tab.focus);
    match tab.layout.panes().len() {
        1 => title.to_owned(),
        count => format!("{title} ({count})"),
    }
}

impl Shell {
    /// Show the palette, listing the tabs as they are now.
    pub fn open_palette(&mut self, workspace: &Workspace, panes: &[Pane], common: &Common) {
        self.palette.clear();
        let command = |label: &str, glyph, pick| PaletteEntry {
            label: label.to_owned(),
            detail: "",
            glyph,
            pick,
        };
        self.palette.extend([
            command("New tab", icons::PLUS, Pick::Run(Action::NewTab)),
            command("Split right", icons::COLUMNS_2, Pick::Run(Action::Split(Axis::Row))),
            command("Split down", icons::ROWS_2, Pick::Run(Action::Split(Axis::Column))),
            command("Local shell right", icons::TERMINAL, Pick::Run(Action::SplitLocal(Axis::Row))),
            command("Local shell below", icons::TERMINAL, Pick::Run(Action::SplitLocal(Axis::Column))),
            command("Zoom pane", icons::SQUARE, Pick::Run(Action::Zoom)),
            command("Close pane", icons::X, Pick::Run(Action::ClosePane)),
            command("Next tab", icons::CHEVRON_RIGHT, Pick::Run(Action::CycleTab(1))),
            command("Previous tab", icons::ARROW_LEFT, Pick::Run(Action::CycleTab(-1))),
            command("Focus left", icons::ARROW_LEFT, Pick::Run(Action::Focus(Direction::Left))),
            command("Focus right", icons::ARROW_RIGHT, Pick::Run(Action::Focus(Direction::Right))),
            command("Focus up", icons::CHEVRON_UP, Pick::Run(Action::Focus(Direction::Up))),
            command("Focus down", icons::CHEVRON_DOWN, Pick::Run(Action::Focus(Direction::Down))),
            command("Copy", icons::COPY, Pick::Run(Action::Copy)),
            command("Paste", icons::COPY, Pick::Run(Action::Paste)),
            command("Toggle sidebar", icons::PANEL_LEFT, Pick::Run(Action::ToggleSidebar)),
            command("New connection", icons::PLUS, Pick::Run(Action::NewConnection)),
            command("Snippets", icons::PENCIL, Pick::Run(Action::Snippets)),
            command("Settings", icons::SETTINGS, Pick::Run(Action::OpenSettings)),
            command("Toggle theme", icons::MONITOR, Pick::Run(Action::ToggleTheme)),
            command("Sync now", icons::FOLDER_SYNC, Pick::Run(Action::SyncNow)),
            command("Send Ctrl+Alt+Del", icons::MONITOR, Pick::Run(Action::SendCtrlAltDel)),
            command("Toggle desktop scale", icons::MONITOR_COG, Pick::Run(Action::ToggleScale)),
        ]);
        if !common.unencrypted {
            self.palette.push(command("Lock vault", icons::LOCK, Pick::Run(Action::Lock)));
        }
        self.palette.extend(workspace.tabs().iter().enumerate().map(|(index, tab)| PaletteEntry {
            label: format!("Go to tab: {}", tab_label(panes, tab)),
            detail: "",
            glyph: tab_glyph(panes, common, tab),
            pick: Pick::SelectTab(index),
        }));
        self.palette.extend(common.snippets.iter().map(|row| PaletteEntry {
            label: row.name.clone(),
            detail: "",
            glyph: icons::CORNER_DOWN_LEFT,
            pick: Pick::Snippet(row.id),
        }));
        self.palette.extend(common.tree.nodes().iter().filter_map(|node| {
            let NodeKind::Connection(protocol) = node.kind else { return None };
            Some(PaletteEntry {
                label: node.label.clone(),
                detail: "",
                glyph: protocol.icon(),
                pick: Pick::Connection(node.id.0),
            })
        }));
        self.quick.open();
    }

    /// What picking line `id` of the palette asks for.
    pub fn picked(&self, id: u64) -> Option<Picked> {
        match &self.palette.get(id as usize)?.pick {
            Pick::Run(action) => Some(Picked::Run(action.clone())),
            Pick::SelectTab(index) => Some(Picked::Run(Action::SelectTab(*index))),
            Pick::Connection(id) => Some(Picked::Connection(*id)),
            Pick::Snippet(id) => Some(Picked::Snippet(*id)),
        }
    }

    /// Open the context menu at `pos`, offering `items`.
    pub fn open_menu(&mut self, pos: egui::Pos2, items: Vec<MenuItem>) {
        self.menu_items = items;
        self.menu.open(pos);
    }

    /// Whether the context menu is open.
    pub fn menu_open(&self) -> bool {
        self.menu.is_open()
    }

    /// The labels of what the context menu offers, and where a row is, for driving it from a script.
    #[cfg(feature = "bench")]
    pub fn menu_offers(&self, label: &str) -> Option<egui::Pos2> {
        self.menu.row_center(&self.menu_items, label)
    }

    /// Whether the context menu covers `pos`, in points.
    pub fn menu_contains(&self, pos: egui::Pos2) -> bool {
        self.menu.contains(pos)
    }

    pub fn close_menu(&mut self) {
        self.menu.close();
    }

    /// Edit the name of the tab `id`, starting from `name`.
    pub fn start_tab_rename(&mut self, id: TabId, name: &str) {
        self.tabs.start_rename(id, name);
    }

    /// Show the pill on a divider's band at `place`, or hide it, in points. Returns whether what
    /// shows changed.
    pub fn hover_divider(&mut self, place: Option<(egui::Rect, bool, f32)>, dragged: bool, now: Instant) -> bool {
        let target = match (&place, dragged) {
            (_, true) => 2.,
            (Some(_), false) => 1.,
            (None, false) => 0.,
        };
        let changed = target != self.pill.target || (place.is_some() && place != self.pill.place);
        if place.is_some() {
            self.pill.place = place;
        }
        self.pill.target = target;
        self.pill.level.to(target, now, PILL_MS, Curve::EASE_OUT);
        changed
    }

    /// A press on a pane's bar became a drag.
    pub fn start_pane_drag(&mut self, workspace: &Workspace, pane: PaneId) {
        let index = workspace.tabs().iter().position(|tab| tab.layout.panes().contains(&pane));
        let Some(tab) = index else { return };
        let panes = workspace.tabs()[tab].layout.panes();
        // The bar of a tab's only pane stands for the tab.
        let source = match panes.iter().position(|&id| id == pane) {
            Some(at) if panes.len() > 1 => Source::Pane(tab, at),
            _ => Source::Tab(tab),
        };
        self.drag = Some(PaneDrag { pane, source });
    }

    /// A tab was dragged off the strip.
    pub fn start_tab_drag(&mut self, workspace: &Workspace, index: usize) {
        let Some(tab) = workspace.tabs().get(index) else { return };
        self.drag = Some(PaneDrag { pane: tab.focus, source: Source::Tab(index) });
    }

    /// The index in the strip that a tab put down at `x`, in points, goes to.
    pub fn tab_slot(&self, x: f32) -> usize {
        self.tabs.slot_at(x)
    }

    /// The middle of the tab at `index`, in points, for driving the strip from a script.
    #[cfg(feature = "bench")]
    pub fn tab_center(&self, index: usize) -> Option<egui::Pos2> {
        self.tabs.rects().get(index).map(|&(_, rect)| rect.center())
    }

    pub fn dragging_pane(&self) -> bool {
        self.drag.is_some()
    }

    /// Drop nothing, wherever the pointer is.
    pub fn cancel_pane_drag(&mut self, now: Instant) {
        self.drag = None;
        self.preview.show(None, now);
    }

    /// Lay out the interface on the thread preparing it, with nothing open and nothing to show
    /// yet, so that the first frame finds its glyphs already rasterized.
    pub fn rehearse(&mut self, ui: &mut Ui) {
        let (mut workspace, motion) = (Workspace::default(), Motion::default());
        workspace.add_tab(0);
        let rects = [(0, Rect::new(0., 0., 640., 480.))];
        let mut common = Common::default();
        let view = ShellView {
            window: 0,
            workspace: &workspace,
            panes: &[],
            rects: &rects,
            motion: &motion,
            now: Instant::now(),
            area: Rect::default(),
            spacing: 0.,
            bar: 0.,
            min_pane: (0., 0.),
            maximized: false,
        };
        common.gate = Gate::Open;
        self.show(ui, &view, &mut common);
        let _ = empty_state(ui, egui::Rect::from_min_size(pos2(0., 0.), vec2(640., 480.)));
    }

    /// Run the whole interface for one pass.
    pub fn show(&mut self, ui: &mut Ui, view: &ShellView<'_>, common: &mut Common) {
        let ctx = ui.ctx().clone();
        let scale = ctx.pixels_per_point();
        let screen = ctx.content_rect();
        let palette = if ui.visuals().dark_mode { &theme::DARK } else { &theme::LIGHT };
        let now = view.now;

        let labels: Vec<String> =
            view.workspace.tabs().iter().map(|tab| tab_label(view.panes, tab)).collect();
        let active = view.workspace.active_index();
        let tabs: Vec<TabView<'_>> = view
            .workspace
            .tabs()
            .iter()
            .zip(&labels)
            .enumerate()
            .map(|(index, (tab, label))| TabView {
                id: TabId(tab.id),
                label,
                glyph: tab_glyph(view.panes, common, &view.workspace.tabs()[index]),
                protocol: tab_protocol(view.panes, common, &view.workspace.tabs()[index]),
                active: index == active,
                status: view
                    .panes
                    .iter()
                    .find(|pane| pane.id == view.workspace.tabs()[index].focus)
                    .map_or(rmx_ui::widgets::PaneStatus::Connected, Pane::status),
            })
            .collect();

        let bar = egui::Rect::from_min_size(screen.min, vec2(screen.width(), STRIP_HEIGHT));
        let title = TitleBarView {
            // What is open is nobody's business while the vault is locked.
            tabs: if common.locked() { &[] } else { &tabs },
            maximized: view.maximized,
            sidebar_open: self.sidebar_open,
            sidebar_width: self.sidebar_width,
        };
        let action = title_bar(ui, bar, &title, &mut self.tabs, now);
        self.actions.extend(action);

        if common.locked() {
            let rect = egui::Rect::from_min_max(pos2(0., STRIP_HEIGHT), screen.max);
            self.show_gate(ui, rect, common);
            let action = context_menu(&ctx, Id::unique("shell-menu"), &mut self.menu, &self.menu_items);
            self.actions.extend(action);
            if !common.asking_hidden {
                self.show_asking(&ctx, view, common);
            }
            return;
        }

        if self.sidebar_open {
            let status = SIDEBAR_STATUS_HEIGHT;
            let side = egui::Rect::from_min_max(
                pos2(0., STRIP_HEIGHT),
                pos2(self.sidebar_width, screen.bottom() - status),
            );
            let vault = std::path::Path::new(&common.vault_path).file_stem().and_then(|name| name.to_str()).unwrap_or("");
            let action = sidebar(ui, side, vault, &mut common.tree, &mut common.tree_state);
            self.actions.extend(action);
            let foot = egui::Rect::from_min_max(
                pos2(0., screen.bottom() - status),
                pos2(self.sidebar_width, screen.bottom()),
            );
            let line = common.status_line();
            let action = sidebar_status(ui, foot, &line);
            self.actions.extend(action);

            let edge = self.sidebar_width;
            let band = egui::Rect::from_min_max(pos2(edge - 3., STRIP_HEIGHT), pos2(edge + 4., screen.bottom()));
            let handle = resize_handle(ui, band, Id::unique("sidebar-edge"), true, edge);
            if let Some(x) = handle.to {
                let most = SIDEBAR_MAX.min((screen.width() - PANES_MIN).max(SIDEBAR_MIN));
                self.actions.push(UiAction::SidebarResize(x.clamp(SIDEBAR_MIN, most)));
            }
            if handle.reset {
                self.actions.push(UiAction::SidebarResize(SIDEBAR_WIDTH));
            }
            if handle.released || handle.reset {
                self.actions.push(UiAction::PanelsChanged);
            }
        }

        self.show_panes(ui, palette, view, common);
        if view.workspace.tabs().is_empty() {
            let action = empty_state(ui, points(view.area, scale));
            self.actions.extend(action);
        }
        self.follow_pane_drag(&ctx, view);
        self.preview.paint(&ctx, scale, palette, now);
        self.paint_drag_label(&ctx, palette, view);
        if self.preview.running(now) {
            ctx.request_repaint();
        }

        let action = new_connection_dialog(&ctx, &mut self.dialog);
        self.actions.extend(action);

        let action = snippets_dialog(&ctx, &mut self.snippets, &common.snippets, &mut common.panels);
        self.actions.extend(action);

        let syncing = matches!(common.sync, SyncState::Running);
        let vault = short_path(&common.vault_path);
        let settings = SettingsView { vault: &vault, syncing, unencrypted: common.unencrypted };
        let action = settings_dialog(&ctx, &mut self.settings, &settings, &mut common.panels);
        self.actions.extend(action);

        let items: Vec<QuickItem<'_>> = self
            .palette
            .iter()
            .enumerate()
            .map(|(id, entry)| QuickItem {
                id: id as u64,
                glyph: entry.glyph,
                label: &entry.label,
                detail: entry.detail,
            })
            .collect();
        let action = quick_open(&ctx, &mut self.quick, &items);
        self.actions.extend(action);

        let id = Id::unique("shell-menu");
        let action = context_menu(&ctx, id, &mut self.menu, &self.menu_items);
        self.actions.extend(action);

        self.show_asking(&ctx, view, common);
    }

    /// The screen shown instead of the panes.
    fn show_gate(&mut self, ui: &mut Ui, rect: egui::Rect, common: &mut Common) {
        let error = common.vault_error.as_deref();
        let action = match &common.gate {
            Gate::Setup(path) => {
                let view = VaultSetupView { path, busy: common.vault_busy, error };
                vault_setup(ui, rect, &mut common.setup, &view)
            },
            Gate::Locked(path) => {
                let view = VaultView { name: path, busy: common.vault_busy, error };
                vault_unlock(ui, rect, &mut common.unlock, &view)
            },
            Gate::Starting | Gate::Open => {
                let p = if ui.visuals().dark_mode { &theme::DARK } else { &theme::LIGHT };
                ui.painter().rect_filled(rect, 0., p.background);
                None
            },
        };
        self.actions.extend(action);
    }

    /// The question waiting for an answer, if any.
    fn show_asking(&mut self, ctx: &Context, view: &ShellView<'_>, common: &mut Common) {
        if common.asking_window != view.window {
            return;
        }
        let action = match &mut common.asking {
            Some(Asking::HostKey(prompt)) => host_key_dialog(ctx, &HostKeyView {
                status: prompt.status,
                host: &prompt.host,
                port: prompt.port,
                algorithm: &prompt.algorithm,
                fingerprint: &prompt.fingerprint,
                previous: prompt.previous.as_deref(),
            }),
            Some(Asking::Fields(state)) => prompt_dialog(ctx, state),
            None => None,
        };
        self.actions.extend(action);
    }

    /// The bar and the rounded outline of each pane.
    fn show_panes(&mut self, ui: &mut Ui, palette: &Palette, view: &ShellView<'_>, common: &mut Common) {
        let scale = ui.ctx().pixels_per_point();
        let tab = view.workspace.active();
        let grouped = tab.is_some_and(|tab| tab.layout.panes().len() > 1);
        let zoomed = tab.is_some_and(|tab| tab.zoomed);
        for &(id, rest) in view.rects {
            let panel = view.motion.frame(id, view.now).unwrap_or(rest);
            let top = bar_rect(panel, scale);
            let Some(pane) = view.panes.iter().find(|pane| pane.id == id) else { continue };
            let forwards = pane
                .remote
                .as_ref()
                .filter(|remote| remote.forwards > 0)
                .map(|remote| match remote.forwards {
                    1 => "1 forward".to_owned(),
                    count => format!("{count} forwards"),
                });
            let target = pane.remote.as_ref().map(|remote| remote.target.clone()).filter(|target| target != pane.label());
            let tooltip: Vec<String> = target.into_iter().chain(pane.content.size_fact()).chain(forwards).collect();
            let tooltip = tooltip.join("  ");
            let ssh = pane.remote.as_ref().is_some_and(|remote| {
                common.nodes.get(&remote.node).is_some_and(|info| info.kind == NodeKind::Connection(Protocol::Ssh))
            });
            let bar = PaneBarView {
                pane: rmx_ui::PaneId(id),
                status: pane.status(),
                title: pane.label(),
                tooltip: &tooltip,
                focused: view.workspace.focused() == Some(id),
                grouped,
                zoomed,
                files: ssh && pane.content.terminal().is_some() && pane.link == crate::pane::Link::Live,
            };
            let reveal = self.bars.entry(id).or_default();
            if self.drag.is_some() {
                reveal.hide();
            }
            let action = pane_bar(ui, top, &bar, reveal, view.now);
            self.actions.extend(action);

            let area = points(terminal_rect(panel, view.bar), scale);
            if let Some(files) = pane.content.files() {
                let files_view = files.view(rmx_ui::PaneId(id));
                let state = common.files.entry(id).or_default();
                let focused = view.workspace.focused() == Some(id);
                files_pane(ui, area, &files_view, focused, state, &mut common.panels, &mut self.actions);
            }
            if let Some(overlay) = pane.overlay() {
                let action = pane_overlay(ui, area, rmx_ui::PaneId(id), &overlay);
                self.actions.extend(action);
            }
            if let Some(notice) = &pane.notice {
                let action = pane_notice(ui, area, rmx_ui::PaneId(id), notice);
                self.actions.extend(action);
            }
        }
        self.bars.retain(|id, _| view.panes.iter().any(|pane| pane.id == *id));

        // The pane being held is veiled, as lifted out of its place.
        if let Some(drag) = &self.drag
            && let Some(&(_, rest)) = view.rects.iter().find(|(id, _)| *id == drag.pane)
        {
            let panel = points(view.motion.frame(drag.pane, view.now).unwrap_or(rest), scale);
            ui.painter_at(panel).rect_filled(panel, CornerRadius::same(RADIUS), palette.chrome.gamma_multiply(VEIL));
        }

        let level = self.pill.level.get(view.now);
        if let (Some((band, upright, along)), true) = (self.pill.place, level > 0.) {
            let layer = LayerId::new(Order::Foreground, Id::unique("divider-pill"));
            paint_pill(&ui.ctx().layer_painter(layer), band, upright, along, level, palette);
        }
        if self.pill.level.running(view.now) {
            ui.ctx().request_repaint();
        }

        common.files.retain(|id, _| view.panes.iter().any(|pane| pane.id == *id));

        // The outlines go over the footers and the terminals, on one painter per pane so that a
        // partial redraw replays only the triangles of the pane it redraws.
        let gutter = view.spacing / scale;
        let radius = CornerRadius::same(RADIUS);
        for &(id, rest) in view.rects {
            let panel = points(view.motion.frame(id, view.now).unwrap_or(rest), scale);
            let painter = ui.painter_at(panel.expand(gutter));
            let cut = Stroke::new(gutter, palette.chrome);
            painter.rect_stroke(panel, radius, cut, StrokeKind::Outside);

            let light = view.motion.light(id, view.now);
            let edge = palette.border.lerp_to_gamma(palette.muted_fg, light * 0.45);
            painter.rect_stroke(panel, radius, Stroke::new(1., edge), StrokeKind::Inside);
        }
    }

    /// While a pane is held: find where it would land, and drop it when the button goes up.
    fn follow_pane_drag(&mut self, ctx: &Context, view: &ShellView<'_>) {
        let Some(drag) = &self.drag else { return };
        let scale = ctx.pixels_per_point();
        let (latest, down) = ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
        let Some(latest) = latest else { return };

        let tabs = self
            .tabs
            .rects()
            .iter()
            .filter_map(|&(id, rect)| Some((view.workspace.tab_index(id.0)?, pixels(rect, scale))))
            .collect();
        let bar = STRIP_HEIGHT * scale;
        let mut scene =
            Scene::new(view.workspace, bar, view.area, tabs, drag.source, view.min_pane, view.spacing);
        let screen = ctx.content_rect();
        scene.window = Some(Rect::new(0., 0., screen.width() * scale, screen.height() * scale));
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        let point = (f64::from(latest.x * scale), f64::from(latest.y * scale));
        let target = scene.target(drag.source, point);
        let preview = target.and_then(|target| scene.preview(target, SLOT_BAR * scale));
        self.preview.show(preview, view.now);

        if !down {
            let drag = self.drag.take().expect("a pane is held");
            self.dropped = target.map(|target| PaneDrop {
                pane: drag.pane,
                source: drag.source,
                target,
            });
            self.preview.show(None, view.now);
        }
    }

    /// A tag following the pointer, naming the pane held.
    fn paint_drag_label(&self, ctx: &Context, palette: &Palette, view: &ShellView<'_>) {
        let Some(drag) = &self.drag else { return };
        let Some(pointer) = ctx.input(|i| i.pointer.latest_pos()) else { return };

        let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::unique("drag-label")));
        let font = fonts::regular(theme::TEXT_SM);
        let title = pane_title(view.panes, drag.pane).to_owned();
        let galley = painter.layout(title, font, palette.foreground, DRAG_LABEL);
        let size = galley.size() + vec2(16., 8.);
        let rect = egui::Rect::from_min_size(pointer + vec2(12., 12.), size);
        painter.rect(
            rect,
            CornerRadius::same(RADIUS_SM),
            palette.background,
            Stroke::new(1., palette.border),
            StrokeKind::Inside,
        );
        painter.galley(rect.min + vec2(8., 4.), galley, Color32::PLACEHOLDER);
    }
}

/// What a line of the palette does.
pub enum Picked {
    Run(Action),
    Connection(u64),
    Snippet(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_starts_below_the_bar() {
        let panel = Rect::new(10., 20., 300., 200.);
        assert_eq!(terminal_rect(panel, 24.), Rect::new(10., 44., 300., 176.));
    }

    #[test]
    fn the_preview_appears_in_place_and_slides_between_places() {
        let now = Instant::now();
        let mut preview = Preview::default();
        let (a, b) = (Rect::new(0., 0., 100., 100.), Rect::new(100., 0., 100., 100.));

        preview.show(Some(a), now);
        assert_eq!(preview.frame.unwrap().get(now), a);
        assert!(preview.running(now));

        let later = now + std::time::Duration::from_millis(200);
        preview.show(Some(b), later);
        assert_eq!(preview.frame.unwrap().get(later), a);
        let after = later + std::time::Duration::from_millis(200);
        assert_eq!(preview.frame.unwrap().get(after), b);
    }

    #[test]
    fn the_palette_offers_commands_and_each_tab() {
        let mut workspace = Workspace::default();
        workspace.add_tab(1);
        workspace.add_tab(2);
        let mut shell = Shell::default();
        shell.open_palette(&workspace, &[], &Common::default());
        assert!(shell.quick.is_open());
        assert!(shell.palette.iter().any(|entry| entry.label == "Split right"));
        assert_eq!(shell.palette.iter().filter(|entry| entry.label.starts_with("Go to tab")).count(), 2);
        assert!(matches!(shell.picked(0), Some(Picked::Run(Action::NewTab))));
    }

    #[test]
    fn only_what_can_be_locked_offers_to_be() {
        let lock_offered = |unencrypted| {
            let common = Common { unencrypted, ..Common::default() };
            let mut shell = Shell::default();
            shell.open_palette(&Workspace::default(), &[], &common);
            shell.palette.iter().any(|entry| entry.label == "Lock vault")
        };
        assert!(lock_offered(false));
        assert!(!lock_offered(true));
    }
}
