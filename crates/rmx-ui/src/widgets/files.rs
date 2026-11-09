//! A pane browsing two file systems side by side, with the queue of what is being copied.
//!
//! Only the rows on screen exist as widgets, however long the listing: a column lays out 40 rows
//! whether the directory holds 40 or 100,000, and sizes and dates are formatted for those alone.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::action::{FileSide, FilesAction, PaneId, UiAction};
use crate::draw::{elided_at, file_icon, hline, icon, palette, vline};
use crate::theme::{CONTROL_HEIGHT, RADIUS_SM, TEXT_SM, TEXT_XS};
use crate::virtual_list::{clamp_scroll, scroll_for_thumb, scroll_to_row, thumb, window};
use crate::{fonts, icons};

use super::controls::{InputOptions, paint_icon_button, submitted, text_input};
use super::panels::{FILES_DATE, FILES_QUEUE, FILES_SIZE, FILES_SPLIT, Panels, resize_handle};
use super::text_view::{BodyState, FilesBody, show_body};

const ROW: f32 = 26.;
const BAR: f32 = 36.;
const HEAD: f32 = 24.;
const PAD: f32 = 8.;
const SIZE_MIN: f32 = 44.;
const DATE_MIN: f32 = 60.;
const NAME_MIN: f32 = 80.;
const COLUMN_MIN: f32 = 200.;
const THUMB_WIDTH: f32 = 6.;
const THUMB_MIN: f32 = 24.;
const QUEUE_BAR: f32 = 36.;
/// The most of a path bar the name of a column takes.
const TITLE_MAX: f32 = 220.;
/// Dragged this close to its bar, the queue folds down to it.
const QUEUE_FOLD_AT: f32 = QUEUE_BAR + ROW / 2.;
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

    /// Whether going into it, or reading it, makes sense. What is neither a file nor a directory
    /// could block whoever reads it for ever.
    pub fn openable(&self) -> bool {
        matches!(self.kind, FileKind::Dir | FileKind::Link | FileKind::File)
    }

    /// Whether it can be compared with another file.
    fn comparable(&self) -> bool {
        matches!(self.kind, FileKind::File | FileKind::Link)
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
    /// What is shown instead of the columns, if anything.
    pub body: FilesBody<'a>,
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
    /// Go back from what is shown instead of the columns.
    Escape,
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
    body: BodyState,
}

impl FilesState {
    /// The column the keyboard is in.
    pub fn active_side(&self) -> FileSide {
        if self.second_active { FileSide::Remote } else { FileSide::Local }
    }

    /// Act on a key. However long the listing, this takes the same few steps and allocates
    /// nothing, except for copying, which lists what to copy. `extend` keeps what is selected.
    pub fn key(&mut self, key: FilesKey, extend: bool, view: &FilesView<'_>) -> Option<FilesAction> {
        if !matches!(view.body, FilesBody::Columns) {
            return self.body.key(key, &view.body);
        }
        self.keyboard_used |= key != FilesKey::Escape;
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
            FilesKey::Escape => return None,
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

    /// Whether there are edits to a file that would be lost.
    pub fn dirty(&self) -> bool {
        self.body.dirty()
    }

    /// Let go of the edits to a file.
    pub fn forget_edits(&mut self) {
        self.body.forget_edits();
    }

    /// Which two files to compare, the one of the first column and the one of the second: what is
    /// selected or else `at`, with what is selected across from it or else the file of that name.
    pub fn compare_pair(
        &self,
        view: &FilesView<'_>,
        at: Option<(FileSide, &Arc<str>)>,
    ) -> Option<(Arc<str>, Arc<str>)> {
        let sole = |side: FileSide| {
            let selected = &self.sides[side.index()].selected;
            let mut names = selected.iter();
            names.next().filter(|_| names.next().is_none()).cloned()
        };
        let file = |side: FileSide, name: &Arc<str>| {
            view.columns[side.index()].rows.iter().find(|row| row.name == *name && row.comparable()).map(|row| row.name.clone())
        };
        let (side, name) = match at {
            Some((side, name)) => (side, name.clone()),
            None => {
                let side = self.active_side();
                match sole(side) {
                    Some(name) => (side, name),
                    None => (side.other(), sole(side.other())?),
                }
            },
        };
        let here = file(side, &name)?;
        let there = sole(side.other()).and_then(|other| file(side.other(), &other)).or_else(|| file(side.other(), &name))?;
        Some(if side == FileSide::Local { (here, there) } else { (there, here) })
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

/// Where the two columns meet in a pane drawn in `area`.
fn split_x(area: Rect, panels: &Panels) -> f32 {
    let wanted = area.left() + area.width() * panels.files_split;
    let (low, high) = (area.left() + COLUMN_MIN, area.right() - COLUMN_MIN);
    if low < high { wanted.clamp(low, high) } else { area.center().x }
}

/// Where the rows of a column sit in a pane drawn in `area`.
fn column_rect(area: Rect, side: FileSide, panels: &Panels) -> Rect {
    let top = area.top();
    let bottom = area.bottom() - queue_height(area, panels);
    let middle = split_x(area, panels);
    match side {
        FileSide::Local => Rect::from_min_max(pos2(area.left(), top), pos2(middle - GAP, bottom)),
        FileSide::Remote => Rect::from_min_max(pos2(middle + GAP, top), pos2(area.right(), bottom)),
    }
}

/// How tall the queue is in `area`: at least its bar and a row, and leaving the columns a few.
fn queue_height(area: Rect, panels: &Panels) -> f32 {
    if panels.files_queue_folded {
        return QUEUE_BAR.min(area.height());
    }
    let low = QUEUE_BAR + ROW;
    let high = (area.height() - BAR - HEAD - ROW * 3.).max(low);
    panels.files_queue.clamp(low, high).min(area.height())
}

fn rows_rect(column: Rect) -> Rect {
    Rect::from_min_max(pos2(column.left(), column.top() + BAR + HEAD), column.max)
}

/// Where the pointer goes to reach row `index` of a column, in points, for driving the pane from a
/// script: the middle of its name.
pub fn file_row_point(area: Rect, side: FileSide, index: usize, panels: &Panels) -> egui::Pos2 {
    let rows = rows_rect(column_rect(area, side, panels));
    pos2(rows.left() + 60., rows.top() + (index as f32 + 0.5) * ROW)
}

/// Where the edges a pane drawn in `area` is resized by are: between the columns, and over the
/// queue. For driving the pane from a script.
pub fn files_edges(area: Rect, panels: &Panels) -> (f32, f32) {
    (split_x(area, panels), column_rect(area, FileSide::Local, panels).bottom())
}

/// Where the button folding the queue is in a pane drawn in `area`. For driving the pane from a
/// script.
pub fn files_fold_point(area: Rect, panels: &Panels) -> egui::Pos2 {
    let top = column_rect(area, FileSide::Local, panels).bottom();
    pos2(area.right() - PAD - 13., top + QUEUE_BAR / 2.)
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

#[allow(clippy::too_many_arguments)]
fn show_column(
    ui: &mut Ui,
    rect: Rect,
    view: &FileColumnView<'_>,
    pane: PaneId,
    focused: bool,
    state: &mut FilesState,
    panels: &mut Panels,
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
    // However long the name of the host, the path keeps most of the bar.
    let title_room = (bar.width() * 0.35).min(TITLE_MAX);
    let active = focused && state.active_side() == side;
    let title_color = if active { p.foreground } else { p.muted_fg };
    let title_width =
        elided_at(ui.painter(), bar.left() + 10., bar.center().y, view.title, fonts::medium(TEXT_SM), title_color, title_room);
    let title_rect = Rect::from_min_size(bar.min, vec2(title_width + 10., BAR));
    ui.interact(title_rect, id.with("title"), Sense::hover()).on_hover_text(view.title);
    let up = Rect::from_center_size(pos2(bar.left() + 10. + title_width + 20., bar.center().y), vec2(26., 26.));
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
        mono: true,
        width: Some(input.width()),
        height: Some(input.height()),
        ..Default::default()
    });
    if submitted(ui, &path_input) {
        let path = side_state.typed.trim().to_owned();
        if !path.is_empty() {
            actions.push(UiAction::Files(pane, FilesAction::Goto { side, path }));
        }
    }
    hline(ui.painter(), bar.left(), bar.right(), bar.bottom() - 1., p.border);

    // The heads of the columns.
    let head = Rect::from_min_size(pos2(rect.left(), bar.bottom()), vec2(rect.width(), HEAD));
    let font = fonts::regular(TEXT_XS);
    let name_x = head.left() + 34.;
    let room = (head.right() - 18. - name_x - NAME_MIN).max(SIZE_MIN + DATE_MIN);
    let date_w = panels.files_date.clamp(DATE_MIN, (room - SIZE_MIN).max(DATE_MIN));
    let size_w = panels.files_size.clamp(SIZE_MIN, (room - date_w).max(SIZE_MIN));
    let (size_right, date_x) = (head.right() - date_w - 14., head.right() - date_w - 4.);
    let text = |at: egui::Pos2, align: Align2, label: &str| {
        ui.painter().text(at, align, label, font.clone(), p.muted_fg);
    };
    text(pos2(name_x, head.center().y), Align2::LEFT_CENTER, "Name");
    text(pos2(size_right, head.center().y), Align2::RIGHT_CENTER, "Size");
    text(pos2(date_x, head.center().y), Align2::LEFT_CENTER, "Modified");
    hline(ui.painter(), head.left(), head.right(), head.bottom() - 1., p.border);

    // The edges between the heads move the columns; they are the same in both columns.
    let band = |x: f32| Rect::from_min_max(pos2(x - 3., head.top()), pos2(x + 3., head.bottom() - 1.));
    let date_edge = date_x - 5.;
    let date = resize_handle(ui, band(date_edge), id.with("edge-date"), true, date_edge);
    if let Some(x) = date.to {
        panels.files_date = (head.right() - x - 9.).clamp(DATE_MIN, (room - size_w).max(DATE_MIN));
    }
    let size_edge = size_right - size_w - 6.;
    let size = resize_handle(ui, band(size_edge), id.with("edge-size"), true, size_edge);
    if let Some(x) = size.to {
        panels.files_size = (size_right - 6. - x).clamp(SIZE_MIN, (room - date_w).max(SIZE_MIN));
    }
    if date.reset {
        panels.files_date = FILES_DATE;
    }
    if size.reset {
        panels.files_size = FILES_SIZE;
    }
    if date.released || date.reset || size.released || size.reset {
        actions.push(UiAction::PanelsChanged);
    }

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
    let empty = ui.interact(rows, id.with("empty"), Sense::click());
    if empty.secondary_clicked() {
        let pos = empty.interact_pointer_pos().unwrap_or(rows.center());
        actions.push(UiAction::FilesMenu { pane, side, name: None, pos });
    }
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
            FileKind::Dir => icon(&painter, at, icons::FOLDER, 14., p.folder),
            FileKind::Link => icon(&painter, at, icons::CORNER_DOWN_LEFT, 13., p.muted_fg),
            FileKind::File | FileKind::Other => file_icon(&painter, at, 14., p.muted_fg),
        }
        let font = fonts::regular(TEXT_SM);
        let name_room = (size_right - size_w - name_x).max(20.);
        elided_at(&painter, name_x, center, &row.name, font.clone(), p.foreground, name_room);
        if let Some(size) = row.size.filter(|_| row.kind == FileKind::File) {
            let at = pos2(size_right, center);
            painter.text(at, Align2::RIGHT_CENTER, format_size(size), fonts::regular(TEXT_XS), p.muted_fg);
        }
        if let Some(modified) = row.modified {
            let at = pos2(date_x, center);
            painter.text(at, Align2::LEFT_CENTER, format_time(modified), fonts::regular(TEXT_XS), p.muted_fg);
        }

        if response.double_clicked() && row.openable() {
            actions.push(UiAction::Files(pane, FilesAction::Open { side, name: row.name.clone() }));
        } else if response.clicked() {
            clicked = Some(index);
        }
        if response.drag_started() {
            drag_row = Some(index);
        }
        if response.secondary_clicked() {
            let side_state = &mut state.sides[side.index()];
            if !side_state.selected.contains(&row.name) {
                side_state.selected.clear();
                side_state.selected.insert(row.name.clone());
                side_state.anchor = Some(index);
            }
            side_state.cursor = index;
            let pos = response.interact_pointer_pos().unwrap_or(row_rect.center());
            actions.push(UiAction::FilesMenu { pane, side, name: Some(row.name.clone()), pos });
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
        let band = Rect::from_min_size(pos2(rows.left(), rows.top() + 4.), vec2(rows.width(), ROW));
        painter.rect_filled(band, 0., p.background);
        elided_at(&painter, at.x, at.y, text, fonts::regular(TEXT_SM), color, rows.width() - 28.);
    };
    if let Some(error) = view.error {
        note(error, p.danger);
    } else if view.busy && len == 0 {
        icon(&painter, pos2(rows.left() + 22., rows.top() + 16.), icons::LOADER_CIRCLE, 13., p.muted_fg);
    }
}

fn show_queue(
    ui: &mut Ui,
    rect: Rect,
    view: &FilesView<'_>,
    state: &mut FilesState,
    panels: &mut Panels,
    actions: &mut Vec<UiAction>,
) {
    let p = palette(ui);
    let pane = view.pane;
    let folded = panels.files_queue_folded;
    let id = Id::unique(("files-queue", pane.0));
    hline(ui.painter(), rect.left(), rect.right(), rect.top(), p.border);

    // What to do about files in the way, and the buttons for copying what is selected.
    let bar = Rect::from_min_size(rect.min + vec2(0., 1.), vec2(rect.width(), QUEUE_BAR - 1.));
    let row_ui_rect = bar.shrink2(vec2(PAD, 3.));
    let mut bar_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id.with("bar"))
            .max_rect(row_ui_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let label = CONFLICT_POLICIES[view.policy.min(CONFLICT_POLICIES.len() - 1)];
    let font = fonts::regular(TEXT_XS);
    let text_width = bar_ui.painter().layout_no_wrap(label.to_owned(), font.clone(), p.muted_fg).size().x;
    let width = text_width + 34.;
    let (cell, _) = bar_ui.allocate_exact_size(vec2(width, 26.), Sense::hover());
    let response = bar_ui.interact(cell, id.with("policy"), Sense::click()).on_hover_text("When a file already exists");
    if response.hovered() {
        bar_ui.painter().rect_filled(cell, CornerRadius::same(RADIUS_SM), p.hover);
    }
    bar_ui.painter().text(pos2(cell.left() + 8., cell.center().y), Align2::LEFT_CENTER, label, font, p.foreground);
    icon(bar_ui.painter(), pos2(cell.right() - 12., cell.center().y), icons::CHEVRON_DOWN, 12., p.muted_fg);
    if response.clicked() {
        actions.push(UiAction::FilesPolicyMenu { pane, pos: cell.left_top() });
    }
    let upload: Vec<Arc<str>> = state.selected(FileSide::Local);
    let download: Vec<Arc<str>> = state.selected(FileSide::Remote);
    let any_finished = view.transfers.iter().any(|t| {
        matches!(t.state, TransferRowState::Done | TransferRowState::Failed | TransferRowState::Cancelled)
    });
    let compare = state.compare_pair(view, None);
    let tools_left = bar_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        let tool = |ui: &mut Ui, salt: &str, glyph: char, tip: &str, enabled: bool| {
            let (cell, _) = ui.allocate_exact_size(vec2(26., 26.), Sense::hover());
            let sense = if enabled { Sense::click() } else { Sense::hover() };
            let response = ui.interact(cell, id.with(salt), sense).on_hover_text(tip);
            paint_icon_button(ui, cell, glyph, 13., &response);
            enabled && response.clicked()
        };
        let (glyph, tip) = if folded { (icons::CHEVRON_UP, "Show transfers") } else { (icons::CHEVRON_DOWN, "Hide transfers") };
        if tool(ui, "fold", glyph, tip, true) {
            panels.files_queue_folded = !folded;
            actions.push(UiAction::PanelsChanged);
        }
        if tool(ui, "clear", icons::TRASH_2, "Clear finished", any_finished) {
            actions.push(UiAction::Files(pane, FilesAction::ClearFinished));
        }
        if tool(ui, "download", icons::ARROW_LEFT, "Download", !download.is_empty()) {
            let names = download.clone();
            actions.push(UiAction::Files(pane, FilesAction::Transfer { from: FileSide::Remote, names }));
        }
        if tool(ui, "upload", icons::ARROW_RIGHT, "Upload", !upload.is_empty()) {
            let names = upload.clone();
            actions.push(UiAction::Files(pane, FilesAction::Transfer { from: FileSide::Local, names }));
        }
        if tool(ui, "compare", icons::COLUMNS_2, "Compare", compare.is_some())
            && let Some((local, remote)) = compare.clone()
        {
            actions.push(UiAction::Files(pane, FilesAction::Compare { local, remote }));
        }
        ui.min_rect().left()
    }).inner;

    if folded {
        paint_folded_queue(ui, bar, row_ui_rect.left() + width + 12., tools_left - 8., view.transfers);
        return;
    }

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
    let mut list_ui = ui.new_child(egui::UiBuilder::new().id_salt(id.with("list")).max_rect(list));
    list_ui.set_clip_rect(list.intersect(ui.clip_rect()));
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
        elided_at(&painter, detail_right - 160., center, &row.detail, fonts::regular(TEXT_XS), p.muted_fg, 160.);

        let track = Rect::from_min_size(pos2(bar_left, center - 2.), vec2(bar_width, 4.));
        painter.rect_filled(track, CornerRadius::same(2), p.border);
        if let Some(fraction) = row.fraction {
            let filled = Rect::from_min_size(track.min, vec2(bar_width * fraction.clamp(0., 1.), 4.));
            painter.rect_filled(filled, CornerRadius::same(2), color);
        }

        let button = Rect::from_center_size(pos2(actions_x + 13., center), vec2(22., 22.));
        if cancel || retry {
            let glyph = if cancel { icons::X } else { icons::FOLDER_SYNC };
            let response = list_ui.interact(button.intersect(list), id.with(("act", row.id)), Sense::click());
            paint_icon_button(&list_ui, button, glyph, 12., &response);
            if response.clicked() {
                let action = if cancel { FilesAction::Cancel(row.id) } else { FilesAction::Retry(row.id) };
                actions.push(UiAction::Files(pane, action));
            }
        }
    }
}

/// What a folded queue still says: what is being copied, and how far along it all is.
fn paint_folded_queue(ui: &Ui, bar: Rect, left: f32, right: f32, transfers: &[TransferRow]) {
    let p = palette(ui);
    let active = || transfers.iter().filter(|t| matches!(t.state, TransferRowState::Queued | TransferRowState::Running));
    let failed = transfers.iter().filter(|t| t.state == TransferRowState::Failed).count();
    let current = active().find(|t| t.state == TransferRowState::Running).or_else(|| active().next());
    let room = right - left;
    let font = fonts::regular(TEXT_SM);
    let y = bar.center().y;
    if let Some(current) = current {
        let more = active().count() - 1;
        let text = if more > 0 { format!("{} +{more}", current.label) } else { current.label.clone() };
        elided_at(ui.painter(), left, y, &text, font, p.muted_fg, room);
        let (mut done, mut count) = (0., 0.);
        for transfer in active() {
            done += transfer.fraction.unwrap_or(0.);
            count += 1.;
        }
        let line = Rect::from_min_size(bar.min, vec2(bar.width() * done / f32::max(count, 1.), 2.));
        ui.painter().rect_filled(line, 0., p.info);
    } else if failed > 0 {
        elided_at(ui.painter(), left, y, &format!("{failed} failed"), font, p.danger, room);
    }
}

/// Draw a file pane over `area`, adding what the user asked for to `actions`.
pub fn files_pane(
    ui: &mut Ui,
    area: Rect,
    view: &FilesView<'_>,
    focused: bool,
    state: &mut FilesState,
    panels: &mut Panels,
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
    let middle = split_x(area, panels);
    let bottom = column_rect(area, FileSide::Local, panels).bottom();
    let columns = matches!(view.body, FilesBody::Columns);
    if columns {
        for column in &view.columns {
            let rect = column_rect(area, column.side, panels);
            show_column(ui, rect, column, view.pane, focused, state, panels, actions);
        }
        vline(ui.painter(), middle, area.top(), bottom, p.border);
    } else {
        let rect = Rect::from_min_max(area.min, pos2(area.right(), bottom));
        show_body(ui, rect, &view.body, view.pane, focused, &mut state.body, actions);
    }

    let queue = Rect::from_min_max(pos2(area.left(), bottom), area.max);
    show_queue(ui, queue, view, state, panels, actions);

    // Between the columns, and above the queue.
    if columns {
        let band = Rect::from_min_max(pos2(middle - 3., area.top()), pos2(middle + 3., bottom));
        let split = resize_handle(ui, band, id.with("edge-split"), true, middle);
        if let Some(x) = split.to {
            panels.files_split = ((x - area.left()) / area.width()).clamp(0., 1.);
        }
        if split.reset {
            panels.files_split = FILES_SPLIT;
        }
        if split.released || split.reset {
            actions.push(UiAction::PanelsChanged);
        }
    }
    let band = Rect::from_min_max(pos2(area.left(), bottom - 3.), pos2(area.right(), bottom + 3.));
    let height = resize_handle(ui, band, id.with("edge-queue"), false, bottom);
    if let Some(y) = height.to {
        let wanted = area.bottom() - y;
        panels.files_queue_folded = wanted < QUEUE_FOLD_AT;
        if !panels.files_queue_folded {
            panels.files_queue = wanted;
        }
    }
    if height.reset {
        panels.files_queue = FILES_QUEUE;
        panels.files_queue_folded = false;
    }
    if height.released || height.reset {
        actions.push(UiAction::PanelsChanged);
    }

    // A selection being dragged: it follows the pointer, and lands in the column it is let go on.
    if let Some(drag) = &state.drag {
        let (pointer, released) =
            ui.input(|i| (i.pointer.latest_pos(), i.pointer.any_released() || !i.pointer.any_down()));
        if let Some(pointer) = pointer {
            let target = column_rect(area, drag.from.other(), panels);
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
        assert!(row.openable());
        assert!(FileRow::new("d", FileKind::Dir, None, None).openable());
        assert!(!FileRow::new("fifo", FileKind::Other, None, None).openable(), "reading one could block for ever");
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
            body: FilesBody::Columns,
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
    fn enter_opens_what_the_cursor_is_on_and_backspace_goes_up() {
        let rows = listing(3);
        let view = view(&rows, &[]);
        let mut state = FilesState::default();
        let side = FileSide::Local;
        assert_eq!(state.key(FilesKey::Open, false, &view), Some(FilesAction::Open { side, name: "f0".into() }));
        state.key(FilesKey::Down, false, &view);
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
        state.sides[0].viewport = ROW * 10.;
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
        let panels = Panels::default();
        let local = column_rect(area, FileSide::Local, &panels);
        let remote = column_rect(area, FileSide::Remote, &panels);
        assert!(local.right() <= remote.left());
        assert_eq!(local.top(), remote.top());
        assert!(local.bottom() < area.bottom());
        let point = file_row_point(area, FileSide::Remote, 2, &panels);
        assert!(remote.contains(point));
    }

    #[test]
    fn the_columns_follow_their_split_and_keep_their_smallest_size() {
        let area = Rect::from_min_size(pos2(100., 50.), vec2(800., 600.));
        let narrow = Panels { files_split: 0.1, ..Panels::default() };
        assert_eq!(split_x(area, &narrow), 300.);
        let wide = Panels { files_split: 0.95, ..Panels::default() };
        assert_eq!(split_x(area, &wide), 700.);
        let thirds = Panels { files_split: 0.25, ..Panels::default() };
        assert_eq!(split_x(area, &thirds), 300.);
    }

    #[test]
    fn a_folded_queue_is_just_its_bar_and_gives_the_rest_to_the_columns() {
        let area = Rect::from_min_size(pos2(0., 0.), vec2(800., 600.));
        let folded = Panels { files_queue_folded: true, ..Panels::default() };
        assert_eq!(queue_height(area, &folded), QUEUE_BAR);
        assert_eq!(column_rect(area, FileSide::Local, &folded).bottom(), 600. - QUEUE_BAR);
    }

    #[test]
    fn what_to_compare_is_the_selection_or_the_file_of_the_same_name() {
        let rows = listing(3);
        let other = vec![FileRow::new("f0", FileKind::File, None, None), FileRow::new("other", FileKind::File, None, None)];
        let view = view(&rows, &other);
        let mut state = FilesState::default();
        assert_eq!(state.compare_pair(&view, None), None, "nothing is selected");

        state.select(FileSide::Local, "f0");
        assert_eq!(state.compare_pair(&view, None), Some(("f0".into(), "f0".into())));
        state.select(FileSide::Remote, "other");
        assert_eq!(state.compare_pair(&view, None), Some(("f0".into(), "other".into())));

        state.select(FileSide::Local, "f2");
        state.select(FileSide::Remote, "missing");
        assert_eq!(state.compare_pair(&view, None), None, "nothing of that name across from it");
        state.select(FileSide::Local, "f1");
        state.select(FileSide::Remote, "f0");
        assert_eq!(state.compare_pair(&view, None), None, "f1 is a directory");

        let at: Arc<str> = "other".into();
        let mut state = FilesState::default();
        state.select(FileSide::Local, "f2");
        assert_eq!(state.compare_pair(&view, Some((FileSide::Remote, &at))), Some(("f2".into(), "other".into())));
    }

    #[test]
    fn the_queue_keeps_a_row_and_leaves_the_columns_some() {
        let area = Rect::from_min_size(pos2(0., 0.), vec2(800., 600.));
        let tiny = Panels { files_queue: 1., ..Panels::default() };
        assert_eq!(queue_height(area, &tiny), QUEUE_BAR + ROW);
        let tall = Panels { files_queue: 900., ..Panels::default() };
        assert_eq!(queue_height(area, &tall), 600. - BAR - HEAD - ROW * 3.);
    }
}
