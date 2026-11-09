//! A pane browsing two file systems side by side, with the queue of what is being copied.
//!
//! Only the rows on screen exist as widgets, however long the listing: a column lays out 40 rows
//! whether the directory holds 40 or 100,000, and sizes and dates are formatted for those alone.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::action::{FileSide, FilesAction, PaneId, UiAction};
use crate::draw::{elided_at, hline, icon, palette, vline};
use crate::theme::{CONTROL_HEIGHT, RADIUS_SM, TEXT_SM};
use crate::virtual_list::{clamp_scroll, scroll_for_thumb, scroll_to_row, thumb, window};
use crate::{fonts, icons};

use super::controls::{ButtonKind, InputOptions, button_full, paint_icon_button, segmented, text_input};

const ROW: f32 = 24.;
const BAR: f32 = 40.;
const HEAD: f32 = 22.;
const PAD: f32 = 8.;
const SIZE_COLUMN: f32 = 76.;
const DATE_COLUMN: f32 = 112.;
const THUMB_WIDTH: f32 = 6.;
const THUMB_MIN: f32 = 24.;
const QUEUE_ROWS: f32 = 4.5;
const QUEUE_BAR: f32 = 40.;
/// How far apart the columns are.
const GAP: f32 = 1.;

/// What can be done about a file that exists at the destination, in the order offered.
pub const CONFLICT_POLICIES: [&str; 5] = ["Skip", "Overwrite", "If newer", "Resume", "Fail"];

/// What a row names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File,
    /// A link, which may or may not lead to a directory.
    Link,
    Other,
}

/// One entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    pub name: Arc<str>,
    /// `name` in lower case, which listings sort by.
    pub sort: Arc<str>,
    pub kind: FileKind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl FileRow {
    /// A row for `name`.
    pub fn new(name: &str, kind: FileKind, size: Option<u64>, modified: Option<SystemTime>) -> Self {
        Self { name: name.into(), sort: name.to_lowercase().into(), kind, size, modified }
    }

    /// Whether going into it makes sense.
    pub fn openable(&self) -> bool {
        matches!(self.kind, FileKind::Dir | FileKind::Link)
    }
}

/// What a column shows.
#[derive(Clone, Copy, Debug)]
pub struct FileColumnView<'a> {
    pub side: FileSide,
    pub title: &'a str,
    pub path: &'a str,
    pub rows: &'a [FileRow],
    pub busy: bool,
    pub error: Option<&'a str>,
}

/// Where a transfer has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferRowState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// A line of the queue.
#[derive(Clone, Debug, PartialEq)]
pub struct TransferRow {
    pub id: u64,
    /// What is being copied and where to.
    pub label: String,
    /// How far along, in words.
    pub detail: String,
    /// How far along, between 0 and 1, when the total is known.
    pub fraction: Option<f32>,
    pub state: TransferRowState,
}

/// What the pane shows.
#[derive(Clone, Copy, Debug)]
pub struct FilesView<'a> {
    pub pane: PaneId,
    pub columns: [FileColumnView<'a>; 2],
    pub transfers: &'a [TransferRow],
    /// Index into [`CONFLICT_POLICIES`].
    pub policy: usize,
}

/// What a key does in a file pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Go into the directory the cursor is on.
    Open,
    /// Go up a directory.
    Back,
    /// Go over to the other column.
    Switch,
    /// Copy what is selected, or else what the cursor is on, to the other column.
    Copy,
}

#[derive(Debug, Default)]
struct SideState {
    scroll: f64,
    /// The row the keyboard is on.
    cursor: usize,
    /// How tall the rows' area last was, for keeping the cursor in it.
    viewport: f32,
    selected: HashSet<Arc<str>>,
    /// Where a shift-click extends from.
    anchor: Option<usize>,
    /// The path as being typed, and the path it was last reset to.
    typed: String,
    shown: String,
    thumb_grab: Option<f32>,
}

#[derive(Debug)]
struct Drag {
    from: FileSide,
    names: Vec<Arc<str>>,
}

/// Where the user is in a file pane.
#[derive(Debug, Default)]
pub struct FilesState {
    sides: [SideState; 2],
    drag: Option<Drag>,
    queue_scroll: f64,
    /// Whether the keyboard is in the second column.
    second_active: bool,
    /// Whether to show where the keyboard is: not for someone just using the pointer.
    keyboard_used: bool,
}

impl FilesState {
    /// The column the keyboard is in.
    pub fn active_side(&self) -> FileSide {
        if self.second_active { FileSide::Remote } else { FileSide::Local }
    }

    /// Act on a key. However long the listing, this takes the same few steps and allocates
    /// nothing, except for copying, which lists what to copy. `extend` keeps what is selected.
    pub fn key(&mut self, key: FilesKey, extend: bool, view: &FilesView<'_>) -> Option<FilesAction> {
        self.keyboard_used = true;
        let side = self.active_side();
        let column = &view.columns[side.index()];
        let rows = column.rows;
        let state = &mut self.sides[side.index()];
        let last = rows.len().saturating_sub(1);
        state.cursor = state.cursor.min(last);
        let page = ((state.viewport / ROW) as usize).saturating_sub(1).max(1);
        let target = match key {
            FilesKey::Up => state.cursor.saturating_sub(1),
            FilesKey::Down => (state.cursor + 1).min(last),
            FilesKey::PageUp => state.cursor.saturating_sub(page),
            FilesKey::PageDown => (state.cursor + page).min(last),
            FilesKey::Home => 0,
            FilesKey::End => last,
            FilesKey::Open => {
                let row = rows.get(state.cursor).filter(|row| row.openable())?;
                return Some(FilesAction::Open { side, name: row.name.clone() });
            },
            FilesKey::Back => return Some(FilesAction::Up { side }),
            FilesKey::Switch => {
                self.second_active = !self.second_active;
                return None;
            },
            FilesKey::Copy => {
                let names: Vec<Arc<str>> = if state.selected.is_empty() {
                    rows.get(state.cursor).map(|row| row.name.clone()).into_iter().collect()
                } else {
                    state.selected.iter().cloned().collect()
                };
                return (!names.is_empty()).then_some(FilesAction::Transfer { from: side, names });
            },
        };
        let row = rows.get(target)?;
        if !extend {
            state.selected.clear();
        }
        // Only the name's reference count changes: no row is copied.
        state.selected.insert(row.name.clone());
        state.cursor = target;
        state.anchor = Some(target);
        state.scroll = scroll_to_row(state.scroll, target, ROW, state.viewport);
        None
    }

    /// Whether a dragged selection is following the pointer.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Select just `name` in the column.
    pub fn select(&mut self, side: FileSide, name: &str) {
        let selected = &mut self.sides[side.index()].selected;
        selected.clear();
        selected.insert(name.into());
    }

    /// What is selected in the column, in no particular order.
    pub fn selected(&self, side: FileSide) -> Vec<Arc<str>> {
        self.sides[side.index()].selected.iter().cloned().collect()
    }

    /// Forget what was selected and scrolled in the column: it shows another directory now.
    pub fn reset_side(&mut self, side: FileSide) {
        let state = &mut self.sides[side.index()];
        state.scroll = 0.;
        state.selected.clear();
        state.anchor = None;
        state.cursor = 0;
    }
}

/// Where the rows of a column sit in a pane drawn in `area`.
fn column_rect(area: Rect, side: FileSide) -> Rect {
    let top = area.top();
    let bottom = area.bottom() - queue_height(area);
    let middle = area.center().x;
    match side {
        FileSide::Local => Rect::from_min_max(pos2(area.left(), top), pos2(middle - GAP, bottom)),
        FileSide::Remote => Rect::from_min_max(pos2(middle + GAP, top), pos2(area.right(), bottom)),
    }
}

fn queue_height(area: Rect) -> f32 {
    (QUEUE_BAR + ROW * QUEUE_ROWS).min(area.height() * 0.4)
}

fn rows_rect(column: Rect) -> Rect {
    Rect::from_min_max(pos2(column.left(), column.top() + BAR + HEAD), column.max)
}

/// Where the pointer goes to reach row `index` of a column, in points, for driving the pane from a
/// script: the middle of its name.
pub fn file_row_point(area: Rect, side: FileSide, index: usize) -> egui::Pos2 {
    let rows = rows_rect(column_rect(area, side));
    pos2(rows.left() + 60., rows.top() + (index as f32 + 0.5) * ROW)
}

/// A size, in the units people read.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (mut value, mut unit) = (bytes as f64 / 1024., 0);
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if value >= 100. {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A time in the local zone, as `2026-11-08 14:02`.
fn format_time(time: SystemTime) -> String {
    let Ok(since) = time.duration_since(SystemTime::UNIX_EPOCH) else { return String::new() };
    let seconds = since.as_secs() as libc::time_t;
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    // Safety: both pointers are to live values for the length of the call.
    if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min
    )
}

/// A plain page shape, as the icon font has no file.
fn paint_file(ui: &Ui, center: egui::Pos2, color: Color32) {
    let rect = Rect::from_center_size(center, vec2(9., 12.));
    let painter = ui.painter();
    painter.rect_stroke(rect, CornerRadius::same(1), Stroke::new(1., color), StrokeKind::Inside);
}

fn show_column(
    ui: &mut Ui,
    rect: Rect,
    view: &FileColumnView<'_>,
    pane: PaneId,
    focused: bool,
    state: &mut FilesState,
    actions: &mut Vec<UiAction>,
) {
    let p = palette(ui);
    let side = view.side;
    let show_cursor = focused && state.keyboard_used && state.active_side() == side;
    let id = Id::unique(("files-column", pane.0, side.index()));
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), BAR));

    // The path bar.
    {
        let side_state = &mut state.sides[side.index()];
        if side_state.shown != view.path {
            side_state.shown = view.path.to_owned();
            side_state.typed = view.path.to_owned();
        }
    }
    ui.painter().text(
        pos2(bar.left() + 10., bar.center().y),
        Align2::LEFT_CENTER,
        view.title,
        fonts::medium(TEXT_SM),
        p.foreground,
    );
    let up = Rect::from_center_size(pos2(bar.left() + 62., bar.center().y), vec2(26., 26.));
    let response = ui.interact(up, id.with("up"), Sense::click());
    paint_icon_button(ui, up, icons::CHEVRON_UP, 13., &response);
    if response.clicked() {
        actions.push(UiAction::Files(pane, FilesAction::Up { side }));
    }
    let refresh = Rect::from_center_size(pos2(bar.right() - 20., bar.center().y), vec2(26., 26.));
    let response = ui.interact(refresh, id.with("refresh"), Sense::click());
    paint_icon_button(ui, refresh, icons::FOLDER_SYNC, 13., &response);
    if response.clicked() {
        actions.push(UiAction::Files(pane, FilesAction::Refresh { side }));
    }
    let input = Rect::from_min_max(
        pos2(up.right() + 4., bar.top() + 4.),
        pos2(refresh.left() - 4., bar.bottom() - 4.),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(input));
    let side_state = &mut state.sides[side.index()];
    let path_input = text_input(&mut child, id.with("path"), &mut side_state.typed, InputOptions {
        hint: "Path",
        mono: true,
        width: Some(input.width()),
        height: Some(input.height()),
        ..Default::default()
    });
    if path_input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        let path = side_state.typed.trim().to_owned();
        if !path.is_empty() {
            actions.push(UiAction::Files(pane, FilesAction::Goto { side, path }));
        }
    }
    hline(ui.painter(), bar.left(), bar.right(), bar.bottom() - 1., p.border);

    // The heads of the columns.
    let head = Rect::from_min_size(pos2(rect.left(), bar.bottom()), vec2(rect.width(), HEAD));
    let font = fonts::regular(TEXT_SM);
    let (name_x, size_right, date_x) =
        (head.left() + 34., head.right() - DATE_COLUMN - 14., head.right() - DATE_COLUMN - 4.);
    let text = |at: egui::Pos2, align: Align2, label: &str| {
        ui.painter().text(at, align, label, font.clone(), p.muted_fg);
    };
    text(pos2(name_x, head.center().y), Align2::LEFT_CENTER, "Name");
    text(pos2(size_right, head.center().y), Align2::RIGHT_CENTER, "Size");
    text(pos2(date_x, head.center().y), Align2::LEFT_CENTER, "Modified");
    hline(ui.painter(), head.left(), head.right(), head.bottom() - 1., p.border);

    // The rows, of which only those on screen are drawn.
    let rows = rows_rect(rect);
    let len = view.rows.len();
    let viewport = rows.height();
    let hovered = ui.rect_contains_pointer(rows);
    let side_state = &mut state.sides[side.index()];
    side_state.viewport = viewport;
    if hovered {
        side_state.scroll -= f64::from(ui.input(|i| i.smooth_scroll_delta.y));
    }
    side_state.scroll = clamp_scroll(side_state.scroll, len, ROW, viewport);
    let visible = window(side_state.scroll, len, ROW, viewport);

    let painter = ui.painter_at(rows);
    let mut clicked = None;
    let mut drag_row = None;
    for index in visible.first..visible.end {
        let row = &view.rows[index];
        let y = rows.top() + visible.top + (index - visible.first) as f32 * ROW;
        let row_rect = Rect::from_min_size(pos2(rows.left() + 4., y), vec2(rows.width() - 12., ROW));
        let response = ui.interact(
            row_rect.intersect(rows),
            id.with(("row", row.name.as_ref())),
            Sense::click_and_drag(),
        );
        let selected = state.sides[side.index()].selected.contains(&row.name);
        if selected {
            painter.rect_filled(row_rect, CornerRadius::same(RADIUS_SM), p.accent);
        } else if response.hovered() {
            painter.rect_filled(row_rect, CornerRadius::same(RADIUS_SM), p.hover);
        }

        if show_cursor && state.sides[side.index()].cursor == index {
            let radius = CornerRadius::same(RADIUS_SM);
            painter.rect_stroke(row_rect, radius, Stroke::new(1., p.muted_fg), StrokeKind::Inside);
        }

        let center = row_rect.center().y;
        let at = pos2(row_rect.left() + 12., center);
        match row.kind {
            FileKind::Dir => icon(&painter, at, icons::FOLDER, 13., p.muted_fg),
            FileKind::Link => icon(&painter, at, icons::CORNER_DOWN_LEFT, 13., p.muted_fg),
            FileKind::File | FileKind::Other => paint_file(ui, at, p.muted_fg),
        }
        let font = fonts::regular(TEXT_SM);
        let name_room = (size_right - SIZE_COLUMN - name_x).max(20.);
        elided_at(&painter, name_x, center, &row.name, font.clone(), p.foreground, name_room);
        if let Some(size) = row.size.filter(|_| row.kind == FileKind::File) {
            let at = pos2(size_right, center);
            painter.text(at, Align2::RIGHT_CENTER, format_size(size), font.clone(), p.muted_fg);
        }
        if let Some(modified) = row.modified {
            let at = pos2(date_x, center);
            painter.text(at, Align2::LEFT_CENTER, format_time(modified), font.clone(), p.muted_fg);
        }

        if response.double_clicked() && row.openable() {
            actions.push(UiAction::Files(pane, FilesAction::Open { side, name: row.name.clone() }));
        } else if response.clicked() {
            clicked = Some(index);
        }
        if response.drag_started() {
            drag_row = Some(index);
        }
    }

    if let Some(index) = clicked {
        let modifiers = ui.input(|i| i.modifiers);
        state.keyboard_used = false;
        state.second_active = side == FileSide::Remote;
        let side_state = &mut state.sides[side.index()];
        side_state.cursor = index;
        let name = view.rows[index].name.clone();
        if modifiers.shift {
            let anchor = side_state.anchor.unwrap_or(index).min(len.saturating_sub(1));
            let (low, high) = (anchor.min(index), anchor.max(index));
            side_state.selected = view.rows[low..=high].iter().map(|r| r.name.clone()).collect();
        } else if modifiers.command || modifiers.ctrl {
            if !side_state.selected.remove(&name) {
                side_state.selected.insert(name);
            }
            side_state.anchor = Some(index);
        } else {
            side_state.selected.clear();
            side_state.selected.insert(name);
            side_state.anchor = Some(index);
        }
    }

    if let Some(index) = drag_row {
        let side_state = &mut state.sides[side.index()];
        let name = view.rows[index].name.clone();
        if !side_state.selected.contains(&name) {
            side_state.selected.clear();
            side_state.selected.insert(name);
            side_state.anchor = Some(index);
        }
        let names = side_state.selected.iter().cloned().collect();
        state.drag = Some(Drag { from: side, names });
    }

    // The scrollbar: its own drag, so that a long list can be crossed at once.
    let content = len as f64 * f64::from(ROW);
    if content > f64::from(viewport) {
        let side_state = &mut state.sides[side.index()];
        let track = Rect::from_min_max(
            pos2(rows.right() - THUMB_WIDTH - 2., rows.top()),
            pos2(rows.right() - 1., rows.bottom()),
        );
        let bar = ui.interact(track, id.with("thumb"), Sense::drag());
        let (top, height) = thumb(side_state.scroll, len, ROW, viewport, THUMB_MIN);
        if bar.drag_started() {
            let pointer = bar.interact_pointer_pos().map_or(0., |pos| pos.y - rows.top());
            side_state.thumb_grab =
                Some(if (top..top + height).contains(&pointer) { pointer - top } else { height / 2. });
        }
        if let (true, Some(grab), Some(pos)) =
            (bar.dragged(), side_state.thumb_grab, bar.interact_pointer_pos())
        {
            let wanted = pos.y - rows.top() - grab;
            side_state.scroll = scroll_for_thumb(wanted, len, ROW, viewport, THUMB_MIN);
        }
        if bar.drag_stopped() {
            side_state.thumb_grab = None;
        }
        if hovered || bar.dragged() {
            let thumb_rect =
                Rect::from_min_size(pos2(track.left(), rows.top() + top), vec2(THUMB_WIDTH, height));
            let color = if bar.dragged() { p.muted_fg } else { p.border };
            painter.rect_filled(thumb_rect, CornerRadius::same(3), color);
        }
    }

    // What the listing says about itself, over the rows.
    let note = |text: &str, color: Color32| {
        let at = pos2(rows.left() + 14., rows.top() + 16.);
        elided_at(&painter, at.x, at.y, text, fonts::regular(TEXT_SM), color, rows.width() - 28.);
    };
    if let Some(error) = view.error {
        note(error, p.danger);
    } else if view.busy && len == 0 {
        note("Listing...", p.muted_fg);
    } else if len == 0 {
        note("Empty folder", p.muted_fg);
    }
}

fn show_queue(
    ui: &mut Ui,
    rect: Rect,
    view: &FilesView<'_>,
    state: &mut FilesState,
    actions: &mut Vec<UiAction>,
) {
    let p = palette(ui);
    let pane = view.pane;
    let id = Id::unique(("files-queue", pane.0));
    hline(ui.painter(), rect.left(), rect.right(), rect.top(), p.border);

    // What to do about files in the way, and the buttons for copying what is selected.
    let bar = Rect::from_min_size(rect.min + vec2(0., 1.), vec2(rect.width(), QUEUE_BAR - 1.));
    let row_ui_rect = bar.shrink2(vec2(PAD, 4.));
    let mut bar_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id.with("bar"))
            .max_rect(row_ui_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut policy = view.policy;
    let options: [&str; 5] = CONFLICT_POLICIES;
    bar_ui.label(egui::RichText::new("If it exists").size(TEXT_SM).color(p.muted_fg));
    let width = 300f32.min((row_ui_rect.width() - 340.).max(120.));
    let response = bar_ui
        .allocate_ui(vec2(width, CONTROL_HEIGHT), |ui| {
            segmented(ui, id.with("policy"), &options, &mut policy)
        })
        .inner;
    if response.changed() {
        actions.push(UiAction::Files(pane, FilesAction::Policy(policy)));
    }
    let upload: Vec<Arc<str>> = state.selected(FileSide::Local);
    let download: Vec<Arc<str>> = state.selected(FileSide::Remote);
    let any_finished = view.transfers.iter().any(|t| {
        matches!(t.state, TransferRowState::Done | TransferRowState::Failed | TransferRowState::Cancelled)
    });
    bar_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let kind = ButtonKind::Secondary;
        if button_full(ui, None, "Clear finished", ButtonKind::Ghost, any_finished, None).clicked() {
            actions.push(UiAction::Files(pane, FilesAction::ClearFinished));
        }
        let height = CONTROL_HEIGHT;
        ui.set_min_height(height);
        if button_full(ui, Some(icons::ARROW_LEFT), "Download", kind, !download.is_empty(), None).clicked() {
            let names = download.clone();
            actions.push(UiAction::Files(pane, FilesAction::Transfer { from: FileSide::Remote, names }));
        }
        if button_full(ui, Some(icons::ARROW_RIGHT), "Upload", kind, !upload.is_empty(), None).clicked() {
            let names = upload.clone();
            actions.push(UiAction::Files(pane, FilesAction::Transfer { from: FileSide::Local, names }));
        }
    });

    // The transfers.
    let list = Rect::from_min_max(pos2(rect.left(), bar.bottom()), rect.max);
    let hovered = ui.rect_contains_pointer(list);
    let viewport = list.height();
    let len = view.transfers.len();
    if hovered {
        state.queue_scroll -= f64::from(ui.input(|i| i.smooth_scroll_delta.y));
    }
    state.queue_scroll = clamp_scroll(state.queue_scroll, len, ROW, viewport);
    let visible = window(state.queue_scroll, len, ROW, viewport);
    let painter = ui.painter_at(list);
    if len == 0 {
        let at = pos2(list.left() + 14., list.top() + 14.);
        painter.text(at, Align2::LEFT_CENTER, "Nothing is being copied", fonts::regular(TEXT_SM), p.muted_fg);
    }
    for index in visible.first..visible.end {
        let row = &view.transfers[index];
        let y = list.top() + visible.top + (index - visible.first) as f32 * ROW;
        let row_rect = Rect::from_min_size(pos2(list.left() + PAD, y), vec2(list.width() - 2. * PAD, ROW));
        let center = row_rect.center().y;

        let (color, cancel, retry) = match row.state {
            TransferRowState::Queued => (p.muted_fg, true, false),
            TransferRowState::Running => (p.info, true, false),
            TransferRowState::Done => (p.success, false, false),
            TransferRowState::Failed => (p.danger, false, true),
            TransferRowState::Cancelled => (p.muted_fg, false, true),
        };
        painter.circle_filled(pos2(row_rect.left() + 5., center), 3., color);

        let actions_x = row_rect.right() - 26.;
        let bar_width = (row_rect.width() * 0.22).clamp(60., 180.);
        let bar_right = actions_x - 6.;
        let bar_left = bar_right - bar_width;
        let detail_right = bar_left - 8.;
        let label_room = ((detail_right - 170.) - (row_rect.left() + 14.)).max(40.);
        elided_at(&painter, row_rect.left() + 14., center, &row.label, fonts::regular(TEXT_SM), p.foreground, label_room);
        elided_at(&painter, detail_right - 160., center, &row.detail, fonts::regular(TEXT_SM), p.muted_fg, 160.);

        let track = Rect::from_min_size(pos2(bar_left, center - 2.), vec2(bar_width, 4.));
        painter.rect_filled(track, CornerRadius::same(2), p.border);
        if let Some(fraction) = row.fraction {
            let filled = Rect::from_min_size(track.min, vec2(bar_width * fraction.clamp(0., 1.), 4.));
            painter.rect_filled(filled, CornerRadius::same(2), color);
        }

        let button = Rect::from_center_size(pos2(actions_x + 13., center), vec2(22., 22.));
        if cancel || retry {
            let glyph = if cancel { icons::X } else { icons::FOLDER_SYNC };
            let response = ui.interact(button.intersect(list), id.with(("act", row.id)), Sense::click());
            paint_icon_button(ui, button, glyph, 12., &response);
            if response.clicked() {
                let action = if cancel { FilesAction::Cancel(row.id) } else { FilesAction::Retry(row.id) };
                actions.push(UiAction::Files(pane, action));
            }
        }
    }
}

/// Draw a file pane over `area`, adding what the user asked for to `actions`.
pub fn files_pane(
    ui: &mut Ui,
    area: Rect,
    view: &FilesView<'_>,
    focused: bool,
    state: &mut FilesState,
    actions: &mut Vec<UiAction>,
) {
    let p = palette(ui);
    let id = Id::unique(("files-pane", view.pane.0));

    ui.painter().rect_filled(area, 0., p.background);
    let base = ui.interact(area, id.with("base"), Sense::click());
    // A press anywhere in the pane gives it the keyboard, as a press in a terminal would.
    if ui.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|at| area.contains(at))) {
        actions.push(UiAction::PaneFocus(view.pane));
    }
    let _ = base;

    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(area));
    let ui = &mut child;
    for column in &view.columns {
        let rect = column_rect(area, column.side);
        show_column(ui, rect, column, view.pane, focused, state, actions);
    }
    let middle = area.center().x;
    let bottom = column_rect(area, FileSide::Local).bottom();
    vline(ui.painter(), middle, area.top(), bottom, p.border);

    let queue = Rect::from_min_max(pos2(area.left(), bottom), area.max);
    show_queue(ui, queue, view, state, actions);

    // A selection being dragged: it follows the pointer, and lands in the column it is let go on.
    if let Some(drag) = &state.drag {
        let (pointer, released) =
            ui.input(|i| (i.pointer.latest_pos(), i.pointer.any_released() || !i.pointer.any_down()));
        if let Some(pointer) = pointer {
            let target = column_rect(area, drag.from.other());
            let over = target.contains(pointer);
            if released {
                if over {
                    let names = drag.names.clone();
                    actions.push(UiAction::Files(view.pane, FilesAction::Transfer { from: drag.from, names }));
                }
                state.drag = None;
            } else {
                let layer = LayerId::new(Order::Tooltip, id.with("drag"));
                let painter = ui.ctx().layer_painter(layer);
                if over {
                    painter.rect_filled(target, 0., p.foreground.gamma_multiply(0.06));
                    painter.rect_stroke(target, 0., Stroke::new(1.5, p.foreground.gamma_multiply(0.4)), StrokeKind::Inside);
                }
                let text = match drag.names.as_slice() {
                    [one] => one.to_string(),
                    many => format!("{} items", many.len()),
                };
                let font = fonts::regular(TEXT_SM);
                let galley = painter.layout(text, font, p.foreground, 240.);
                let size = galley.size() + vec2(16., 8.);
                let tag = Rect::from_min_size(pointer + vec2(12., 12.), size);
                painter.rect(tag, CornerRadius::same(RADIUS_SM), p.background, Stroke::new(1., p.border), StrokeKind::Inside);
                painter.galley(tag.min + vec2(8., 4.), galley, Color32::PLACEHOLDER);
                ui.ctx().request_repaint();
            }
        } else if released {
            state.drag = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_people_write_them() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_size(300 * 1024 * 1024), "300 MiB");
    }

    #[test]
    fn the_rows_of_a_directory_list_their_own_paths() {
        let row = FileRow::new("Foo.TXT", FileKind::File, Some(1), None);
        assert_eq!(row.sort.as_ref(), "foo.txt");
        assert!(!row.openable());
        assert!(FileRow::new("d", FileKind::Dir, None, None).openable());
    }

    #[test]
    fn a_new_directory_starts_with_nothing_selected() {
        let mut state = FilesState::default();
        state.select(FileSide::Remote, "a");
        assert_eq!(state.selected(FileSide::Remote).len(), 1);
        state.reset_side(FileSide::Remote);
        assert!(state.selected(FileSide::Remote).is_empty());
    }

    fn listing(count: usize) -> Vec<FileRow> {
        let kind = |i| if i == 1 { FileKind::Dir } else { FileKind::File };
        (0..count).map(|i| FileRow::new(&format!("f{i}"), kind(i), Some(1), None)).collect()
    }

    fn view<'a>(rows: &'a [FileRow], other: &'a [FileRow]) -> FilesView<'a> {
        let column = |side, rows| FileColumnView { side, title: "", path: "/", rows, busy: false, error: None };
        FilesView {
            pane: PaneId(1),
            columns: [column(FileSide::Local, rows), column(FileSide::Remote, other)],
            transfers: &[],
            policy: 0,
        }
    }

    #[test]
    fn arrows_move_a_single_selection_and_shift_extends_it() {
        let rows = listing(5);
        let view = view(&rows, &[]);
        let mut state = FilesState::default();
        state.key(FilesKey::Down, false, &view);
        assert_eq!(state.selected(FileSide::Local), [Arc::from("f1")]);
        state.key(FilesKey::Down, true, &view);
        let mut selected = state.selected(FileSide::Local);
        selected.sort();
        assert_eq!(selected, [Arc::from("f1"), Arc::from("f2")]);
        state.key(FilesKey::Up, false, &view);
        assert_eq!(state.selected(FileSide::Local), [Arc::from("f1")]);
        state.key(FilesKey::End, false, &view);
        state.key(FilesKey::Down, false, &view);
        assert_eq!(state.selected(FileSide::Local), [Arc::from("f4")]);
        state.key(FilesKey::Home, false, &view);
        state.key(FilesKey::Up, false, &view);
        assert_eq!(state.selected(FileSide::Local), [Arc::from("f0")]);
    }

    #[test]
    fn enter_opens_directories_only_and_backspace_goes_up() {
        let rows = listing(3);
        let view = view(&rows, &[]);
        let mut state = FilesState::default();
        assert_eq!(state.key(FilesKey::Open, false, &view), None, "the cursor is on a file");
        state.key(FilesKey::Down, false, &view);
        let side = FileSide::Local;
        assert_eq!(state.key(FilesKey::Open, false, &view), Some(FilesAction::Open { side, name: "f1".into() }));
        assert_eq!(state.key(FilesKey::Back, false, &view), Some(FilesAction::Up { side }));
    }

    #[test]
    fn tab_changes_column_and_copy_takes_the_selection_or_the_cursor() {
        let (rows, other) = (listing(3), listing(2));
        let view = view(&rows, &other);
        let mut state = FilesState::default();
        assert_eq!(
            state.key(FilesKey::Copy, false, &view),
            Some(FilesAction::Transfer { from: FileSide::Local, names: vec!["f0".into()] }),
        );
        state.key(FilesKey::Switch, false, &view);
        assert_eq!(state.active_side(), FileSide::Remote);
        state.key(FilesKey::Down, false, &view);
        assert_eq!(
            state.key(FilesKey::Copy, false, &view),
            Some(FilesAction::Transfer { from: FileSide::Remote, names: vec!["f1".into()] }),
        );
    }

    #[test]
    fn the_cursor_stays_in_view_of_a_huge_listing() {
        let rows = listing(100_000);
        let view = view(&rows, &[]);
        let mut state = FilesState::default();
        state.sides[0].viewport = 240.;
        state.key(FilesKey::End, false, &view);
        assert_eq!(state.sides[0].cursor, 99_999);
        let top = state.sides[0].scroll;
        assert!(top >= 99_990. * f64::from(ROW), "{top}");
        state.key(FilesKey::PageUp, false, &view);
        assert_eq!(state.sides[0].cursor, 99_990);
        // An empty listing has nowhere to go, and does not panic.
        let empty = super::tests::view(&[], &[]);
        assert_eq!(FilesState::default().key(FilesKey::Down, false, &empty), None);
    }

    #[test]
    fn the_columns_sit_side_by_side_above_the_queue() {
        let area = Rect::from_min_size(pos2(100., 50.), vec2(800., 600.));
        let local = column_rect(area, FileSide::Local);
        let remote = column_rect(area, FileSide::Remote);
        assert!(local.right() <= remote.left());
        assert_eq!(local.top(), remote.top());
        assert!(local.bottom() < area.bottom());
        let point = file_row_point(area, FileSide::Remote, 2);
        assert!(remote.contains(point));
    }
}
