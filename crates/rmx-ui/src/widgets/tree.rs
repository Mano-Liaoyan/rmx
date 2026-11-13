//! The connection tree. Only the rows on screen exist as widgets, however long the list.

use egui::{CornerRadius, Id, Key, Modifiers, Rect, Sense, Ui, Vec2, pos2, vec2};

use crate::action::{NodeId, Protocol, UiAction};
use crate::draw::{elided_at, icon, palette};
use crate::theme::{CONTROL_HEIGHT, RADIUS_SM, TEXT_SM, TEXT_XS, TREE_ICON, TREE_ROW_HEIGHT};
use crate::virtual_list::{clamp_scroll, scroll_for_thumb, scroll_to_row, thumb, window};
use crate::{fonts, icons};

use super::controls::{InputOptions, paint_icon_button, submitted, text_input};

const INDENT: f32 = 14.;
const THUMB_WIDTH: f32 = 6.;
const THUMB_MIN: f32 = 24.;

/// What a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Folder,
    Connection(Protocol),
}

/// How the sessions to a node are doing, the best of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionDot {
    #[default]
    None,
    Live,
    Connecting,
    /// All that is left of them is a closed connection.
    Closed,
    /// One of them could not connect.
    Failed,
}

/// One node, as the app describes it.
#[derive(Clone, Debug)]
pub struct TreeNode {
    pub id: NodeId,
    /// Distance from the root; nodes come in depth-first order.
    pub depth: u16,
    pub kind: NodeKind,
    pub label: String,
    /// How its sessions are doing: the dot.
    pub state: SessionDot,
    /// Folders only: showing its children.
    pub expanded: bool,
}

/// The tree's contents and which nodes the current folding and search leave visible.
///
/// Visible rows are a flat list of indices, rebuilt only when folding or the search change, so
/// a frame's cost doesn't depend on the size of the tree.
#[derive(Default)]
pub struct TreeModel {
    nodes: Vec<TreeNode>,
    /// One past the end of each node's subtree.
    subtree_end: Vec<u32>,
    /// Every label lowercased and joined by NUL, so a search is one pass over one buffer.
    haystack: String,
    /// Where each node's label begins in `haystack`.
    offsets: Vec<u32>,
    query: String,
    visible: Vec<u32>,
    matched: Vec<bool>,
    any_at_depth: Vec<bool>,
}

impl TreeModel {
    /// A model of `nodes`, which must be in depth-first order.
    pub fn new(nodes: Vec<TreeNode>) -> Self {
        let mut model = Self::default();
        model.set_nodes(nodes);
        model
    }

    /// Replace the nodes, keeping the search.
    pub fn set_nodes(&mut self, nodes: Vec<TreeNode>) {
        self.subtree_end = vec![nodes.len() as u32; nodes.len()];
        // A stack of the ancestors still open: each node closes those at its depth or deeper.
        let mut open: Vec<usize> = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            while open.last().is_some_and(|&a| nodes[a].depth >= node.depth) {
                let ancestor = open.pop().expect("just checked");
                self.subtree_end[ancestor] = i as u32;
            }
            open.push(i);
        }

        self.haystack.clear();
        self.offsets.clear();
        for node in &nodes {
            self.offsets.push(self.haystack.len() as u32);
            self.haystack.extend(node.label.chars().flat_map(char::to_lowercase));
            self.haystack.push('\0');
        }
        let depth = nodes.iter().map(|n| usize::from(n.depth)).max().unwrap_or(0);
        self.any_at_depth = vec![false; depth + 2];
        self.nodes = nodes;
        self.rebuild();
    }

    /// Every node.
    pub fn nodes(&self) -> &[TreeNode] {
        &self.nodes
    }

    /// How many rows are showing.
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// The node showing at row `row`.
    pub fn visible_node(&self, row: usize) -> &TreeNode {
        &self.nodes[self.visible[row] as usize]
    }

    /// The row showing node `id`, if any.
    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.visible.iter().position(|&i| self.nodes[i as usize].id == id)
    }

    /// The current search.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Filter to labels containing `query`, keeping the folders on the way to each match.
    pub fn set_query(&mut self, query: &str) {
        if query != self.query {
            self.query.clear();
            self.query.extend(query.chars().flat_map(char::to_lowercase));
            self.rebuild();
        }
    }

    /// Say how the sessions to `id` are doing.
    pub fn set_state(&mut self, id: NodeId, state: SessionDot) {
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.state = state;
        }
    }

    /// Fold or unfold the folder at `row`, returning its new state.
    pub fn toggle_row(&mut self, row: usize) -> bool {
        let index = self.visible[row] as usize;
        let node = &mut self.nodes[index];
        node.expanded = !node.expanded;
        let expanded = node.expanded;
        self.rebuild();
        expanded
    }

    fn rebuild(&mut self) {
        self.visible.clear();
        if self.query.is_empty() {
            let mut i = 0;
            while i < self.nodes.len() {
                self.visible.push(i as u32);
                let node = &self.nodes[i];
                i = if node.kind == NodeKind::Folder && !node.expanded {
                    self.subtree_end[i] as usize
                } else {
                    i + 1
                };
            }
            return;
        }

        self.matched.clear();
        self.matched.resize(self.nodes.len(), false);
        let mut node = 0;
        for (at, _) in self.haystack.match_indices(self.query.as_str()) {
            let at = at as u32;
            // Matches arrive in order, so the node only ever moves forward.
            while self.offsets.get(node + 1).is_some_and(|&next| next <= at) {
                node += 1;
            }
            self.matched[node] = true;
        }

        // Walking backwards meets children before their parent. Each depth's flag says whether
        // anything kept has been seen since the last node of the depth above.
        self.any_at_depth.fill(false);
        for i in (0..self.nodes.len()).rev() {
            let depth = usize::from(self.nodes[i].depth);
            let keep = self.matched[i] || self.any_at_depth[depth + 1];
            self.any_at_depth[depth + 1] = false;
            if keep {
                self.any_at_depth[depth] = true;
                self.visible.push(i as u32);
            }
        }
        self.visible.reverse();
    }
}

/// Where the user is in the tree.
#[derive(Clone, Debug, Default)]
pub struct TreeState {
    /// Scroll offset, in points from the top.
    pub scroll: f64,
    pub selected: Option<NodeId>,
    /// Pending request to bring the selection into view.
    reveal: bool,
    search: String,
    thumb_grab: Option<f32>,
}

/// Draw the tree in `rect`.
pub fn connection_tree(
    ui: &mut Ui,
    rect: Rect,
    tree: &mut TreeModel,
    state: &mut TreeState,
) -> Option<UiAction> {
    let id = Id::unique("connection-tree");
    let p = palette(ui);
    let len = tree.visible_len();
    let viewport = rect.height();
    let hovered = ui.rect_contains_pointer(rect);
    let mut action = None;

    if hovered {
        state.scroll -= f64::from(ui.input(|i| i.smooth_scroll_delta.y));
    }

    // Keys act on the tree when it holds the focus, which clicking a row gives it.
    let focused = ui.memory(|m| m.has_focus(id));
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    vertical_arrows: true,
                    horizontal_arrows: true,
                    ..Default::default()
                },
            );
        });
        action = handle_keys(ui, tree, state);
    }
    let len = len.min(tree.visible_len());

    if state.reveal {
        if let Some(row) = state.selected.and_then(|id| tree.row_of(id)) {
            state.scroll = scroll_to_row(state.scroll, row, TREE_ROW_HEIGHT, viewport);
        }
        state.reveal = false;
    }
    state.scroll = clamp_scroll(state.scroll, len, TREE_ROW_HEIGHT, viewport);

    let visible = window(state.scroll, len, TREE_ROW_HEIGHT, viewport);
    let painter = ui.painter_at(rect);
    let mut toggle = None;
    for row in visible.first..visible.end {
        let node = tree.visible_node(row);
        let y = rect.top() + visible.top + (row - visible.first) as f32 * TREE_ROW_HEIGHT;
        let row_rect = Rect::from_min_size(
            pos2(rect.left() + 6., y),
            vec2(rect.width() - 12., TREE_ROW_HEIGHT),
        );
        let response = ui.interact(row_rect.intersect(rect), id.with(node.id.0), Sense::click());
        let selected = state.selected == Some(node.id);
        if selected {
            painter.rect_filled(row_rect, CornerRadius::same(RADIUS_SM), p.accent);
        } else if response.hovered() {
            painter.rect_filled(row_rect, CornerRadius::same(RADIUS_SM), p.hover);
        }

        let center = row_rect.center().y;
        let indent = row_rect.left() + 4. + f32::from(node.depth) * INDENT;
        let (glyph, is_folder) = match node.kind {
            NodeKind::Folder if node.expanded || !tree.query().is_empty() => {
                (icons::FOLDER_OPEN, true)
            },
            NodeKind::Folder => (icons::FOLDER, true),
            NodeKind::Connection(protocol) => (protocol.icon(), false),
        };
        if is_folder {
            let chevron = if node.expanded || !tree.query().is_empty() {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            };
            icon(&painter, pos2(indent + 6., center), chevron, 12., p.muted_fg);
        }
        let tint = match node.kind {
            NodeKind::Connection(protocol) => p.protocol(protocol),
            NodeKind::Folder => p.folder,
        };
        icon(&painter, pos2(indent + 21., center), glyph, TREE_ICON, tint);

        // Under the pointer a connection offers what is done with it most.
        let hot = !is_folder && ui.rect_contains_pointer(row_rect.intersect(rect));
        let protocol = match node.kind {
            NodeKind::Connection(protocol) => Some(protocol),
            NodeKind::Folder => None,
        };
        let buttons = if hot {
            let files = protocol == Some(Protocol::Ssh);
            let mut right = row_rect.right() - 3.;
            let mut button = |ui: &mut Ui, salt: &str, glyph: char, tip: &str| {
                right -= 22.;
                let cell = Rect::from_center_size(pos2(right + 11., center), Vec2::splat(22.));
                let response = ui
                    .interact(cell, id.with((node.id.0, salt)), Sense::click())
                    .on_hover_text(tip);
                paint_icon_button(ui, cell, glyph, 13., &response);
                response.clicked()
            };
            if button(ui, "connect", icons::PLUG, "Connect") {
                action = Some(UiAction::TreeOpen(node.id));
            }
            if button(ui, "split", icons::COLUMNS_2, "Connect in split") {
                action = Some(UiAction::TreeOpenSplit { id: node.id, down: false });
            }
            if files && button(ui, "files", icons::FOLDER_SYNC, "Files") {
                action = Some(UiAction::TreeOpenFiles(node.id));
            }
            if files { 66. } else { 44. }
        } else {
            0.
        };

        let x = indent + 34.;
        let dot = match node.state {
            SessionDot::None => None,
            SessionDot::Live => Some(p.success),
            SessionDot::Connecting => Some(p.warning),
            SessionDot::Closed => Some(p.muted_fg),
            SessionDot::Failed => Some(p.danger),
        };
        let right = row_rect.right() - 8. - buttons - if dot.is_some() { 14. } else { 0. };
        let font = if selected { fonts::medium(TEXT_SM) } else { fonts::regular(TEXT_SM) };
        elided_at(&painter, x, center, &node.label, font, p.foreground, right - x);
        if let Some(dot) = dot {
            painter.circle_filled(pos2(row_rect.right() - 12. - buttons, center), 3., dot);
        }

        if response.clicked() {
            state.selected = Some(node.id);
            ui.memory_mut(|m| m.request_focus(id));
            if is_folder {
                toggle = Some(row);
            }
            action = Some(UiAction::TreeSelect(node.id));
        }
        if response.double_clicked() && !is_folder {
            action = Some(UiAction::TreeOpen(node.id));
        }
        if response.secondary_clicked() {
            state.selected = Some(node.id);
            let pos = response.interact_pointer_pos().unwrap_or(row_rect.center());
            action = Some(UiAction::TreeContextMenu { id: node.id, pos });
        }
    }

    if let Some(row) = toggle.filter(|_| tree.query().is_empty()) {
        let id = tree.visible_node(row).id;
        let expanded = tree.toggle_row(row);
        action = Some(UiAction::TreeFolderToggled { id, expanded });
        ui.ctx().request_repaint();
    }

    let content = len as f64 * f64::from(TREE_ROW_HEIGHT);
    if content > f64::from(viewport) {
        let track = Rect::from_min_max(
            pos2(rect.right() - THUMB_WIDTH - 2., rect.top()),
            pos2(rect.right() - 1., rect.bottom()),
        );
        let bar = ui.interact(track, id.with("thumb"), Sense::drag());
        let (top, height) = thumb(state.scroll, len, TREE_ROW_HEIGHT, viewport, THUMB_MIN);
        if bar.drag_started() {
            let pointer = bar.interact_pointer_pos().map_or(0., |pos| pos.y - rect.top());
            // Grabbing beside the thumb jumps it there, centered on the pointer.
            state.thumb_grab = Some(if (top..top + height).contains(&pointer) {
                pointer - top
            } else {
                height / 2.
            });
        }
        if let (true, Some(grab), Some(pos)) =
            (bar.dragged(), state.thumb_grab, bar.interact_pointer_pos())
        {
            let wanted = pos.y - rect.top() - grab;
            state.scroll = scroll_for_thumb(wanted, len, TREE_ROW_HEIGHT, viewport, THUMB_MIN);
            ui.ctx().request_repaint();
        }
        if bar.drag_stopped() {
            state.thumb_grab = None;
        }
        if hovered || bar.dragged() {
            let thumb_rect = Rect::from_min_size(
                pos2(track.left(), rect.top() + top),
                vec2(THUMB_WIDTH, height),
            );
            let color = if bar.dragged() { p.muted_fg } else { p.border };
            painter.rect_filled(thumb_rect, CornerRadius::same(3), color);
        }
    }
    action
}

fn handle_keys(ui: &mut Ui, tree: &mut TreeModel, state: &mut TreeState) -> Option<UiAction> {
    let pressed = |ui: &mut Ui, key: Key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, key));
    let len = tree.visible_len();
    if len == 0 {
        return None;
    }
    let current = state.selected.and_then(|id| tree.row_of(id));
    let target = if pressed(ui, Key::ArrowDown) {
        Some(current.map_or(0, |row| (row + 1).min(len - 1)))
    } else if pressed(ui, Key::ArrowUp) {
        Some(current.map_or(0, |row| row.saturating_sub(1)))
    } else {
        None
    };
    if let Some(row) = target {
        let id = tree.visible_node(row).id;
        state.selected = Some(id);
        state.reveal = true;
        return Some(UiAction::TreeSelect(id));
    }

    let row = current?;
    let node = tree.visible_node(row);
    let (id, kind, expanded) = (node.id, node.kind, node.expanded);
    if pressed(ui, Key::Enter) {
        return match kind {
            NodeKind::Connection(_) => Some(UiAction::TreeOpen(id)),
            NodeKind::Folder => {
                let expanded = tree.toggle_row(row);
                Some(UiAction::TreeFolderToggled { id, expanded })
            },
        };
    }
    let folding = (kind == NodeKind::Folder)
        && ((expanded && pressed(ui, Key::ArrowLeft))
            || (!expanded && pressed(ui, Key::ArrowRight)));
    if folding {
        let expanded = tree.toggle_row(row);
        return Some(UiAction::TreeFolderToggled { id, expanded });
    }
    None
}

/// What the strip at the foot of the sidebar shows.
#[derive(Clone, Copy, Debug)]
pub struct SidebarStatus<'a> {
    /// The database has no master password, so nothing can be locked.
    pub unencrypted: bool,
    /// The state of sync, or the outcome of the last thing done.
    pub text: &'a str,
    /// Whether that is a failure.
    pub failed: bool,
    /// Whether a sync is running.
    pub busy: bool,
    /// Whether there is a backend to sync with.
    pub can_sync: bool,
}

/// Height of [`sidebar_status`].
pub const SIDEBAR_STATUS_HEIGHT: f32 = 32.;

/// What is said about a database that is not encrypted.
const UNENCRYPTED_NOTE: &str = "This database has no master password: it is not encrypted";

/// Whether `text` looks like `user@host:port`: one word with a dot, an `@` or a port.
fn looks_like_a_host(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && !text.contains(char::is_whitespace) && text.contains(['.', '@', ':'])
}

/// The strip at the foot of the sidebar: sync status, then sync, settings and lock buttons. Where
/// nothing can be locked, a quiet sign of that takes the place of the lock.
pub fn sidebar_status(ui: &mut Ui, rect: Rect, status: &SidebarStatus<'_>) -> Option<UiAction> {
    let id = Id::unique("sidebar-status");
    let p = palette(ui);
    ui.painter().rect_filled(rect, 0., p.sidebar);

    let mut action = None;
    let mut right = rect.right() - 6.;
    let mut cell = || {
        let side = SIDEBAR_STATUS_HEIGHT - 8.;
        right -= side;
        let cell = Rect::from_min_size(pos2(right, rect.top() + 4.), Vec2::splat(side));
        right -= 2.;
        cell
    };
    let button = |ui: &mut Ui, at: Rect, salt: &str, glyph: char, enabled: bool| {
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let response = ui.interact(at, id.with(salt), sense);
        paint_icon_button(ui, at, glyph, 13., &response);
        response.clicked()
    };
    let at = cell();
    if status.unencrypted {
        ui.interact(at, id.with("unencrypted"), Sense::hover()).on_hover_text(UNENCRYPTED_NOTE);
        crate::draw::lock_open(ui.painter(), at.center(), 13., p.warning);
    } else if button(ui, at, "lock", icons::LOCK, true) {
        action = Some(UiAction::VaultLock);
    }
    if button(ui, cell(), "settings", icons::SETTINGS, true) {
        action = Some(UiAction::OpenSettings);
    }
    let at = cell();
    let response = ui.interact(at, id.with("theme"), Sense::click());
    paint_icon_button(ui, at, ' ', 13., &response);
    let dark = ui.visuals().dark_mode;
    let color = if response.hovered() { p.foreground } else { p.muted_fg };
    if dark {
        crate::draw::sun(ui.painter(), at.center(), 13., color);
    } else {
        crate::draw::moon(ui.painter(), at.center(), 13., color);
    }
    let hint = if dark { "Switch to the light theme" } else { "Switch to the dark theme" };
    if response.on_hover_text(hint).clicked() {
        action = Some(UiAction::ToggleTheme);
    }
    if button(ui, cell(), "sync", icons::FOLDER_SYNC, status.can_sync && !status.busy) {
        action = Some(UiAction::SyncNow);
    }

    let color = if status.failed { p.danger } else { p.muted_fg };
    let x = rect.left() + 12.;
    let y = rect.center().y;
    let width = right - x - 4.;
    elided_at(ui.painter(), x, y, status.text, fonts::regular(TEXT_XS), color, width);
    if status.failed {
        // The strip is too short for most reasons.
        let line = Rect::from_min_size(pos2(x, rect.top()), Vec2::new(width, rect.height()));
        ui.interact(line, id.with("reason"), Sense::hover()).on_hover_text(status.text);
    }
    if status.busy {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(400));
    }
    action
}

/// The sidebar: the vault, then search, new-connection button and tree. `rect` is the full column.
pub fn sidebar(
    ui: &mut Ui,
    rect: Rect,
    vault: &str,
    tree: &mut TreeModel,
    state: &mut TreeState,
) -> Option<UiAction> {
    let id = Id::unique("sidebar");
    let p = palette(ui);
    ui.painter().rect_filled(rect, 0., p.sidebar);

    let switcher =
        Rect::from_min_size(rect.min + vec2(8., 8.), vec2(rect.width() - 16., CONTROL_HEIGHT));
    let opened = ui.interact(switcher, id.with("vault"), Sense::click());
    if opened.hovered() {
        ui.painter().rect_filled(switcher, CornerRadius::same(RADIUS_SM), p.hover);
    }
    let y = switcher.center().y;
    let mark = Rect::from_center_size(pos2(switcher.left() + 16., y), Vec2::splat(18.));
    ui.painter().rect_filled(mark, CornerRadius::same(RADIUS_SM), p.brand);
    let initial: String =
        vault.chars().next().map(|first| first.to_uppercase().collect()).unwrap_or_default();
    ui.painter().text(
        mark.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        fonts::medium(TEXT_XS),
        p.brand_fg,
    );
    let room = switcher.width() - 56.;
    elided_at(
        ui.painter(),
        switcher.left() + 32.,
        y,
        vault,
        fonts::medium(TEXT_SM),
        p.foreground,
        room,
    );
    crate::draw::icon(
        ui.painter(),
        pos2(switcher.right() - 14., y),
        icons::CHEVRON_DOWN,
        13.,
        p.muted_fg,
    );
    let vault_menu =
        opened.clicked().then(|| UiAction::VaultMenu(switcher.left_bottom() + vec2(0., 4.)));

    let header = Rect::from_min_size(
        pos2(switcher.left(), switcher.bottom() + 6.),
        vec2(switcher.width(), CONTROL_HEIGHT),
    );
    let input = header.with_max_x(header.right() - CONTROL_HEIGHT - 6.);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(input));
    let response = text_input(
        &mut child,
        id.with("search"),
        &mut state.search,
        InputOptions {
            hint: "Search or user@host",
            glyph: Some(icons::SEARCH),
            ..Default::default()
        },
    );
    if response.changed() {
        tree.set_query(&state.search);
        state.scroll = 0.;
    }

    let mut action = None;
    // Enter connects to what the search found, or else to what was typed.
    if submitted(ui, &response) {
        let first = (0..tree.visible_len())
            .map(|row| tree.visible_node(row))
            .find(|node| node.kind != NodeKind::Folder)
            .map(|node| node.id);
        action = match first {
            Some(id) => Some(UiAction::TreeOpen(id)),
            None if looks_like_a_host(&state.search) => {
                Some(UiAction::QuickConnect(state.search.trim().to_owned()))
            },
            None => None,
        };
        if action.is_some() {
            state.search.clear();
            tree.set_query("");
        }
    }
    let new_rect = Rect::from_min_size(
        pos2(header.right() - CONTROL_HEIGHT, header.top()),
        Vec2::splat(CONTROL_HEIGHT),
    );
    let new = ui.interact(new_rect, id.with("new"), Sense::click());
    paint_icon_button(ui, new_rect, icons::PLUS, 14., &new);
    if new.clicked() {
        action = Some(UiAction::SidebarAddMenu(new_rect.left_bottom() + vec2(0., 4.)));
    }

    let list = Rect::from_min_max(
        pos2(rect.left(), header.bottom() + 6.),
        pos2(rect.right() - 1., rect.bottom() - 4.),
    );
    vault_menu.or(action).or(connection_tree(ui, list, tree, state))
}
