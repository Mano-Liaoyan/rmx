//! What a file pane shows in place of its columns: a text file, or two of them compared.
//!
//! A file short enough to edit is a text field. A longer one and a comparison are drawn like the
//! listings: only the lines on screen exist, however many there are.

use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Galley, Id, Key, Layout, Modifiers, Painter, Rect, ScrollArea,
    Sense, TextBuffer, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use crate::action::{FilesAction, PaneId, UiAction};
use crate::draw::{elided_at, hline, icon, palette, snap, vline};
use crate::theme::{CONTROL_HEIGHT, Palette, TEXT_SM};
use crate::virtual_list::{clamp_scroll, max_scroll, scroll_for_thumb, thumb, window};
use crate::{fonts, icons};

use super::controls::{ButtonKind, button_full, paint_icon_button};
use super::files::FilesKey;

const BAR: f32 = 40.;
/// Every line is this tall, whichever font its glyphs come from, so that numbers stay beside them.
const LINE: f32 = 18.;
const THUMB_WIDTH: f32 = 6.;
const THUMB_MIN: f32 = 24.;
/// How much of a line is drawn: a minified file is one line, and nobody scrolls to its end.
const LINE_BYTES: usize = 8192;
/// Room around the numbers of a gutter, in points.
const GUTTER_PAD: f32 = 20.;

/// A text file as the pane shows it.
#[derive(Clone, Copy, Debug)]
pub struct TextView<'a> {
    /// Which opening of a file this is.
    pub id: u64,
    /// Goes up with every save.
    pub revision: u64,
    pub title: &'a str,
    pub path: &'a str,
    /// The file as last read or saved.
    pub text: &'a str,
    /// Where each line starts, in a file too long to edit, which is only shown.
    pub lines: Option<&'a [u32]>,
    pub saving: bool,
    pub error: Option<&'a str>,
}

/// One side of a row of a comparison, as offsets into its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub number: u32,
    pub start: u32,
    pub end: u32,
    /// The part of the line that differs from the one across from it.
    pub changed: Option<(u32, u32)>,
}

/// A row of a comparison: a line and what it lines up with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffRow {
    pub left: Option<DiffLine>,
    pub right: Option<DiffLine>,
    pub same: bool,
}

/// Two texts compared.
#[derive(Clone, Copy, Debug)]
pub struct DiffView<'a> {
    pub id: u64,
    pub titles: [&'a str; 2],
    pub texts: [&'a str; 2],
    pub rows: &'a [DiffRow],
    /// The rows where a run of changes starts, in order.
    pub changes: &'a [u32],
    /// The most characters on a line.
    pub longest: usize,
    /// What to say when there are no changes to see.
    pub note: Option<&'a str>,
}

/// What the pane shows.
#[derive(Clone, Copy, Debug)]
pub enum FilesBody<'a> {
    Columns,
    /// A file on its way: what it is called.
    Loading(&'a str),
    Text(TextView<'a>),
    Diff(DiffView<'a>),
}

/// Where the user is in what the pane shows.
#[derive(Debug, Default)]
pub struct BodyState {
    /// Which file and revision `buffer` was last matched against.
    shown: (u64, u64),
    buffer: String,
    /// Whether `buffer` differs from the file.
    dirty: bool,
    /// How many lines `buffer` has.
    line_count: usize,
    wants_focus: bool,
    /// Which comparison the scroll belongs to.
    diff: u64,
    /// The change last jumped to, and the scroll that showed it.
    jumped: Option<(usize, f64)>,
    scroll: f64,
    across: f32,
    viewport: f32,
    thumb_grab: Option<f32>,
}

impl BodyState {
    /// Whether there are edits that would be lost.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Let go of the edits.
    pub fn forget_edits(&mut self) {
        self.dirty = false;
    }

    /// Act on a key while the columns are out of sight.
    pub fn key(&mut self, key: FilesKey, body: &FilesBody<'_>) -> Option<FilesAction> {
        let len = match body {
            FilesBody::Text(TextView { lines: Some(lines), .. }) => lines.len(),
            FilesBody::Diff(diff) => diff.rows.len(),
            _ => 0,
        };
        let page = f64::from((self.viewport - LINE).max(LINE));
        let step = f64::from(LINE);
        self.scroll = match key {
            FilesKey::Escape => return Some(FilesAction::CloseBody),
            FilesKey::Up => self.scroll - step,
            FilesKey::Down => self.scroll + step,
            FilesKey::PageUp => self.scroll - page,
            FilesKey::PageDown => self.scroll + page,
            FilesKey::Home => 0.,
            FilesKey::End => max_scroll(len, LINE, self.viewport),
            _ => return None,
        };
        self.scroll = clamp_scroll(self.scroll, len, LINE, self.viewport);
        None
    }

    /// Take the wheel and hold the view within `len` lines of at most `longest` characters.
    fn wheel(&mut self, ui: &Ui, rows: Rect, len: usize, longest: f32) {
        self.viewport = rows.height();
        if ui.rect_contains_pointer(rows) {
            let delta = ui.input(|i| i.smooth_scroll_delta);
            self.scroll -= f64::from(delta.y);
            self.across -= delta.x;
        }
        self.scroll = clamp_scroll(self.scroll, len, LINE, self.viewport);
        self.across = self.across.clamp(0., longest.max(0.));
    }
}

/// The widest a digit is.
fn digit_width(ui: &Ui) -> f32 {
    ui.painter().layout_no_wrap("0".to_owned(), fonts::mono(TEXT_SM), Color32::PLACEHOLDER).size().x
}

/// How wide a gutter for `lines` lines is.
fn gutter_width(ui: &Ui, lines: usize) -> f32 {
    let digits = lines.max(1).ilog10() as f32 + 1.;
    digits.max(3.) * digit_width(ui) + GUTTER_PAD
}

/// A line number, flush right against `right`.
fn paint_number(painter: &Painter, right: f32, y: f32, number: usize, color: Color32) {
    let at = pos2(right, y + LINE / 2.);
    painter.text(at, Align2::RIGHT_CENTER, number, fonts::mono(TEXT_SM), color);
}

/// `line` with no more of it than is drawn, and no line ending.
fn drawn_part(line: &str) -> &str {
    let line = line.trim_end_matches(['\r', '\n']);
    &line[..line.floor_char_boundary(LINE_BYTES)]
}

/// A line starting at `x`, with its `changed` bytes marked.
fn paint_line(
    painter: &Painter,
    x: f32,
    y: f32,
    line: &str,
    changed: Option<(usize, usize)>,
    color: Color32,
    mark: Color32,
) {
    let line = drawn_part(line);
    if line.is_empty() {
        return;
    }
    let format = TextFormat { font_id: fonts::mono(TEXT_SM), color, line_height: Some(LINE), ..Default::default() };
    let mut job = LayoutJob::default();
    let parts = match changed {
        Some((start, end)) if start < end => {
            let (start, end) = (line.floor_char_boundary(start), line.floor_char_boundary(end));
            [(&line[..start], false), (&line[start..end], true), (&line[end..], false)]
        },
        _ => [(line, false), ("", false), ("", false)],
    };
    for (part, marked) in parts {
        if !part.is_empty() {
            let background = if marked { mark } else { Color32::TRANSPARENT };
            job.append(part, 0., TextFormat { background, ..format.clone() });
        }
    }
    let galley: Arc<Galley> = painter.layout_job(job);
    painter.galley(pos2(snap(painter, x), snap(painter, y)), galley, color);
}

/// The bar over what the pane shows: `title`, then the elided `path`, in the room left of `right`.
fn paint_title(ui: &Ui, bar: Rect, title: &str, path: &str, right: f32) {
    let p = palette(ui);
    let y = bar.center().y;
    let room = (right - bar.left() - 20.).max(0.);
    let title_width = elided_at(ui.painter(), bar.left() + 10., y, title, fonts::medium(TEXT_SM), p.foreground, room * 0.4);
    let x = bar.left() + 10. + title_width + 10.;
    elided_at(ui.painter(), x, y, path, fonts::mono(TEXT_SM), p.muted_fg, right - x - 10.);
}

/// A button of a bar, in a right-to-left layout.
fn tool(ui: &mut Ui, id: Id, glyph: char, tip: &str, enabled: bool) -> bool {
    let (cell, _) = ui.allocate_exact_size(vec2(26., 26.), Sense::hover());
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(cell, id, sense).on_hover_text(tip);
    paint_icon_button(ui, cell, glyph, 13., &response);
    enabled && response.clicked()
}

/// A right-to-left row filling `bar`. Returns where it got to.
fn tools(ui: &mut Ui, bar: Rect, id: Id, add: impl FnOnce(&mut Ui)) -> f32 {
    let inner = bar.shrink2(vec2(8., 4.));
    let mut row = ui.new_child(
        UiBuilder::new().id_salt(id).max_rect(inner).layout(Layout::right_to_left(Align::Center)),
    );
    row.set_min_height(CONTROL_HEIGHT);
    add(&mut row);
    row.min_rect().left()
}

/// The vertical scrollbar of `len` lines at `rows`, with `ticks` marked along it.
fn scrollbar(ui: &mut Ui, rows: Rect, id: Id, len: usize, state: &mut BodyState, ticks: &[u32], p: &Palette) {
    let viewport = rows.height();
    if len as f64 * f64::from(LINE) <= f64::from(viewport) {
        return;
    }
    let track = Rect::from_min_max(pos2(rows.right() - THUMB_WIDTH - 2., rows.top()), pos2(rows.right() - 1., rows.bottom()));
    let bar = ui.interact(track, id.with("thumb"), Sense::drag());
    let (top, height) = thumb(state.scroll, len, LINE, viewport, THUMB_MIN);
    if bar.drag_started() {
        let pointer = bar.interact_pointer_pos().map_or(0., |pos| pos.y - rows.top());
        state.thumb_grab = Some(if (top..top + height).contains(&pointer) { pointer - top } else { height / 2. });
    }
    if let (true, Some(grab), Some(pos)) = (bar.dragged(), state.thumb_grab, bar.interact_pointer_pos()) {
        state.scroll = scroll_for_thumb(pos.y - rows.top() - grab, len, LINE, viewport, THUMB_MIN);
    }
    if bar.drag_stopped() {
        state.thumb_grab = None;
    }
    let painter = ui.painter_at(rows);
    let mut last = f32::NEG_INFINITY;
    for &row in ticks {
        let y = rows.top() + (row as f32 / len as f32 * viewport).floor();
        if y > last {
            painter.rect_filled(Rect::from_min_size(pos2(track.left(), y), vec2(THUMB_WIDTH, 2.)), 0., p.warning);
            last = y;
        }
    }
    let color = if bar.dragged() { p.muted_fg } else { p.border };
    painter.rect_filled(
        Rect::from_min_size(pos2(track.left(), rows.top() + top), vec2(THUMB_WIDTH, height)),
        CornerRadius::same(3),
        color,
    );
}

/// A file too long to edit: its lines on screen.
fn show_lines(ui: &mut Ui, rows: Rect, id: Id, view: &TextView<'_>, lines: &[u32], state: &mut BodyState) {
    let p = palette(ui);
    let gutter = gutter_width(ui, lines.len());
    let wide = digit_width(ui) * LINE_BYTES as f32;
    state.wheel(ui, rows, lines.len(), wide);
    let painter = ui.painter_at(rows);
    let text_left = rows.left() + gutter + 8.;
    let visible = window(state.scroll, lines.len(), LINE, rows.height());
    let text_clip = rows.with_min_x(text_left - 2.);
    let text_painter = ui.painter_at(text_clip);
    for index in visible.first..visible.end {
        let y = rows.top() + visible.top + (index - visible.first) as f32 * LINE;
        paint_number(&painter, rows.left() + gutter - 10., y, index + 1, p.muted_fg);
        let start = lines[index] as usize;
        let end = lines.get(index + 1).map_or(view.text.len(), |&next| next as usize);
        paint_line(&text_painter, text_left - state.across, y, &view.text[start..end], None, p.foreground, p.accent);
    }
    scrollbar(ui, rows, id, lines.len(), state, &[], p);
}

/// The field a file is edited in, with its line numbers. Returns whether to save.
fn show_editor(
    ui: &mut Ui,
    rows: Rect,
    id: Id,
    view: &TextView<'_>,
    pane: PaneId,
    focused: bool,
    state: &mut BodyState,
    actions: &mut Vec<UiAction>,
) -> bool {
    let p = palette(ui);
    let font = fonts::mono(TEXT_SM);
    let gutter = gutter_width(ui, state.line_count);
    let area = rows.with_min_x(rows.left() + gutter + 8.);

    let mut child = ui.new_child(UiBuilder::new().id_salt(id.with("editor")).max_rect(area).layout(Layout::top_down(Align::LEFT)));
    let color = p.foreground;
    let mut layouter = |ui: &Ui, text: &dyn TextBuffer, _wrap: f32| {
        let format = TextFormat { font_id: font.clone(), color, line_height: Some(LINE), ..Default::default() };
        ui.painter().layout_job(LayoutJob::single_section(text.as_str().to_owned(), format))
    };
    let buffer = &mut state.buffer;
    let output = ScrollArea::both().id_salt("scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        TextEdit::multiline(buffer)
            .id_salt("text")
            .frame(Frame::NONE)
            .lock_focus(true)
            .desired_width(f32::INFINITY)
            .min_size(area.size())
            .layouter(&mut layouter)
            .show(ui)
            .response
            .response
    });
    let response = output.inner;
    if state.wants_focus && focused {
        response.request_focus();
        state.wants_focus = false;
    }
    if response.changed() {
        state.dirty = state.buffer != view.text;
        state.line_count = state.buffer.bytes().filter(|byte| *byte == b'\n').count() + 1;
    }

    let painter = ui.painter_at(rows.with_max_x(area.left()));
    let top = f64::from(output.state.clamped_offset().y);
    let visible = window(top, state.line_count, LINE, rows.height());
    for index in visible.first..visible.end {
        let y = rows.top() + visible.top + (index - visible.first) as f32 * LINE;
        paint_number(&painter, rows.left() + gutter - 10., y, index + 1, p.muted_fg);
    }

    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
        actions.push(UiAction::Files(pane, FilesAction::CloseBody));
    }
    response.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S))
}

fn show_text(
    ui: &mut Ui,
    rect: Rect,
    view: &TextView<'_>,
    pane: PaneId,
    focused: bool,
    state: &mut BodyState,
    actions: &mut Vec<UiAction>,
) {
    let p = palette(ui);
    let id = Id::unique(("files-text", pane.0));
    if state.shown != (view.id, view.revision) {
        if state.shown.0 != view.id {
            state.buffer = view.text.to_owned();
            state.line_count = state.buffer.bytes().filter(|byte| *byte == b'\n').count() + 1;
            state.scroll = 0.;
            state.across = 0.;
        }
        state.shown = (view.id, view.revision);
        state.dirty = state.buffer != view.text;
        state.wants_focus = view.lines.is_none();
    }

    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), BAR));
    let (dirty, editable) = (state.dirty, view.lines.is_none());
    let mut save = false;
    let mut diff = false;
    let left = tools(ui, bar, id.with("tools"), |ui| {
        if tool(ui, id.with("close"), icons::X, "Close", true) {
            actions.push(UiAction::Files(pane, FilesAction::CloseBody));
        }
        if editable {
            let glyph = view.saving.then_some(icons::LOADER_CIRCLE);
            save = button_full(ui, glyph, "Save", ButtonKind::Secondary, dirty && !view.saving, None).clicked();
            diff = tool(ui, id.with("diff"), icons::COLUMNS_2, "Show changes", dirty);
        }
    });
    let right = match view.error {
        Some(error) => {
            let room = ((left - bar.left()) * 0.5).max(0.);
            let width = ui.painter().layout_no_wrap(error.to_owned(), fonts::regular(TEXT_SM), p.danger).size().x.min(room);
            let x = left - width - 8.;
            elided_at(ui.painter(), x, bar.center().y, error, fonts::regular(TEXT_SM), p.danger, room);
            ui.interact(Rect::from_min_max(pos2(x, bar.top()), pos2(left, bar.bottom())), id.with("error"), Sense::hover())
                .on_hover_text(error);
            x
        },
        None => left,
    };
    paint_title(ui, bar, view.title, view.path, right - 12.);
    if dirty {
        ui.painter().circle_filled(pos2(right - 12., bar.center().y), 3., p.warning);
    }
    hline(ui.painter(), bar.left(), bar.right(), bar.bottom() - 1., p.border);

    let rows = Rect::from_min_max(pos2(rect.left(), bar.bottom()), rect.max);
    save |= match view.lines {
        Some(lines) => {
            show_lines(ui, rows, id, view, lines, state);
            false
        },
        None => show_editor(ui, rows, id, view, pane, focused, state, actions),
    };
    if save && state.dirty && !view.saving {
        actions.push(UiAction::Files(pane, FilesAction::Save { text: state.buffer.clone() }));
    }
    if diff {
        actions.push(UiAction::Files(pane, FilesAction::DiffEdits { text: state.buffer.clone() }));
    }
}

fn show_diff(ui: &mut Ui, rect: Rect, view: &DiffView<'_>, pane: PaneId, state: &mut BodyState, actions: &mut Vec<UiAction>) {
    let p = palette(ui);
    let id = Id::unique(("files-diff", pane.0));
    // The field lost the keyboard when it went out of sight.
    state.wants_focus = true;
    if state.diff != view.id {
        state.diff = view.id;
        state.jumped = None;
        state.scroll = 0.;
        state.across = 0.;
    }
    let middle = rect.center().x;
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), BAR));

    let top_row = (state.scroll / f64::from(LINE)).ceil() as usize;
    // From the change last jumped to, for as long as the view has stayed on it.
    let jumped = state.jumped.filter(|&(_, scroll)| scroll == state.scroll).map(|(index, _)| index);
    let next = jumped.map_or_else(|| view.changes.partition_point(|&row| (row as usize) < top_row), |index| index + 1);
    let previous = jumped.unwrap_or_else(|| view.changes.partition_point(|&row| (row as usize) < top_row)).checked_sub(1);
    let mut go_to = None;
    let left = tools(ui, bar, id.with("tools"), |ui| {
        if tool(ui, id.with("close"), icons::X, "Close", true) {
            actions.push(UiAction::Files(pane, FilesAction::CloseBody));
        }
        if tool(ui, id.with("next"), icons::CHEVRON_DOWN, "Next change", next < view.changes.len()) {
            go_to = Some(next);
        }
        if tool(ui, id.with("previous"), icons::CHEVRON_UP, "Previous change", previous.is_some()) {
            go_to = previous;
        }
    });
    if let Some(index) = go_to {
        let lead = f64::from(state.viewport) / 3.;
        let wanted = f64::from(view.changes[index]) * f64::from(LINE) - lead;
        state.scroll = clamp_scroll(wanted, view.rows.len(), LINE, state.viewport);
        state.jumped = Some((index, state.scroll));
    }
    let font = fonts::medium(TEXT_SM);
    elided_at(ui.painter(), bar.left() + 10., bar.center().y, view.titles[0], font.clone(), p.foreground, middle - bar.left() - 20.);
    elided_at(ui.painter(), middle + 10., bar.center().y, view.titles[1], font, p.foreground, left - middle - 20.);
    hline(ui.painter(), bar.left(), bar.right(), bar.bottom() - 1., p.border);

    let rows = Rect::from_min_max(pos2(rect.left(), bar.bottom()), rect.max);
    let width = digit_width(ui);
    state.wheel(ui, rows, view.rows.len(), width * view.longest.min(LINE_BYTES) as f32);
    let gutter = gutter_width(ui, view.rows.len());
    let halves = [
        Rect::from_min_max(rows.min, pos2(middle, rows.bottom())),
        Rect::from_min_max(pos2(middle + 1., rows.top()), rows.max),
    ];
    let visible = window(state.scroll, view.rows.len(), LINE, rows.height());
    let washes = [p.danger.gamma_multiply(0.14), p.success.gamma_multiply(0.14)];
    let marks = [p.danger.gamma_multiply(0.38), p.success.gamma_multiply(0.38)];
    for (side, half) in halves.iter().enumerate() {
        let painter = ui.painter_at(*half);
        let text_left = half.left() + gutter + 8.;
        let text_painter = ui.painter_at(half.with_min_x(text_left - 2.));
        for index in visible.first..visible.end {
            let row = &view.rows[index];
            let y = rows.top() + visible.top + (index - visible.first) as f32 * LINE;
            let band = Rect::from_min_size(pos2(half.left(), y), vec2(half.width(), LINE));
            let line = if side == 0 { row.left } else { row.right };
            let Some(line) = line else {
                painter.rect_filled(band, 0., p.hover);
                continue;
            };
            if !row.same {
                painter.rect_filled(band, 0., washes[side]);
            }
            paint_number(&painter, half.left() + gutter - 10., y, line.number as usize, p.muted_fg);
            let text = &view.texts[side][line.start as usize..line.end as usize];
            let changed = line.changed.map(|(start, end)| {
                ((start - line.start) as usize, (end - line.start) as usize)
            });
            paint_line(&text_painter, text_left - state.across, y, text, changed, p.foreground, marks[side]);
        }
    }
    vline(ui.painter(), middle, rows.top(), rows.bottom(), p.border);
    if let Some(note) = view.note {
        let at = pos2(rows.center().x, rows.top() + 24.);
        let font = fonts::regular(TEXT_SM);
        let galley = ui.painter().layout_no_wrap(note.to_owned(), font, p.muted_fg);
        let tag = Rect::from_center_size(at, galley.size() + vec2(20., 10.));
        ui.painter().rect_filled(tag, CornerRadius::same(4), p.chrome);
        ui.painter().galley(tag.center() - galley.size() / 2., galley, p.muted_fg);
    }
    scrollbar(ui, rows, id, view.rows.len(), state, view.changes, p);
}

fn show_loading(ui: &mut Ui, rect: Rect, title: &str, pane: PaneId, actions: &mut Vec<UiAction>) {
    let p = palette(ui);
    let id = Id::unique(("files-loading", pane.0));
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), BAR));
    let left = tools(ui, bar, id, |ui| {
        if tool(ui, id.with("close"), icons::X, "Cancel", true) {
            actions.push(UiAction::Files(pane, FilesAction::CloseBody));
        }
    });
    paint_title(ui, bar, title, "", left);
    hline(ui.painter(), bar.left(), bar.right(), bar.bottom() - 1., p.border);
    icon(ui.painter(), rect.center(), icons::LOADER_CIRCLE, 18., p.muted_fg);
}

/// Draw `body`, anything but the columns, over `rect`.
pub(super) fn show_body(
    ui: &mut Ui,
    rect: Rect,
    body: &FilesBody<'_>,
    pane: PaneId,
    focused: bool,
    state: &mut BodyState,
    actions: &mut Vec<UiAction>,
) {
    match body {
        FilesBody::Columns => (),
        FilesBody::Loading(title) => show_loading(ui, rect, title, pane, actions),
        FilesBody::Text(view) => show_text(ui, rect, view, pane, focused, state, actions),
        FilesBody::Diff(view) => show_diff(ui, rect, view, pane, state, actions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_drawn_without_its_ending_and_never_at_full_length() {
        assert_eq!(drawn_part("text\r\n"), "text");
        assert_eq!(drawn_part("text"), "text");
        let long = "字".repeat(LINE_BYTES);
        let drawn = drawn_part(&long);
        assert!(drawn.len() <= LINE_BYTES);
        assert!(long.is_char_boundary(drawn.len()));
    }

    #[test]
    fn keys_scroll_a_long_file_and_escape_closes_it() {
        let lines: Vec<u32> = (0..1000).collect();
        let view = TextView {
            id: 1,
            revision: 0,
            title: "",
            path: "",
            text: "",
            lines: Some(&lines),
            saving: false,
            error: None,
        };
        let body = FilesBody::Text(view);
        let mut state = BodyState { viewport: 180., ..Default::default() };
        assert_eq!(state.key(FilesKey::Down, &body), None);
        assert_eq!(state.scroll, f64::from(LINE));
        state.key(FilesKey::End, &body);
        assert_eq!(state.scroll, 1000. * f64::from(LINE) - 180.);
        state.key(FilesKey::PageDown, &body);
        assert_eq!(state.scroll, 1000. * f64::from(LINE) - 180., "and no further");
        state.key(FilesKey::Home, &body);
        assert_eq!(state.scroll, 0.);
        assert_eq!(state.key(FilesKey::Escape, &body), Some(FilesAction::CloseBody));
    }
}
