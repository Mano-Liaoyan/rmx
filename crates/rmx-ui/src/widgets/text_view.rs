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
use crate::theme::{CONTROL_HEIGHT, Palette, TEXT_SM, TEXT_XS, Token};
use crate::virtual_list::{clamp_scroll, max_scroll, scroll_for_thumb, thumb, window};
use crate::{fonts, icons};

use super::controls::{ButtonKind, button_full, paint_icon_button};
use super::files::FilesKey;

const BAR: f32 = 36.;
/// Every line is this tall, whichever font its glyphs come from, so that numbers stay beside them.
const LINE: f32 = 20.;
/// How long after the last keystroke the text is colored again, in seconds.
const HIGHLIGHT_AFTER: f64 = 0.12;
const THUMB_WIDTH: f32 = 6.;
const THUMB_MIN: f32 = 24.;
/// How much of a line is drawn: a minified file is one line, and nobody scrolls to its end.
const LINE_BYTES: usize = 8192;
/// Room around the numbers of a gutter, in points.
const GUTTER_PAD: f32 = 20.;

/// A run of source text, in bytes from the start of its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenRun {
    pub start: u32,
    pub end: u32,
    pub token: Token,
}

/// Where a line's runs start in [`Highlight::runs`], and how long the line was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightLine {
    pub first_run: u32,
    pub len: u32,
}

/// What each part of a text is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Highlight {
    pub runs: Vec<TokenRun>,
    pub lines: Vec<HighlightLine>,
}

impl Highlight {
    /// How long line `index` was when it was colored, and its runs.
    pub fn line(&self, index: usize) -> Option<(usize, &[TokenRun])> {
        let line = self.lines.get(index)?;
        let end = self.lines.get(index + 1).map_or(self.runs.len(), |next| next.first_run as usize);
        Some((line.len as usize, &self.runs[line.first_run as usize..end]))
    }

    /// The runs of line `index` if `text` could still be that line, which an edit may have moved.
    fn runs_for(&self, index: usize, text: &str) -> &[TokenRun] {
        match self.line(index) {
            Some((len, runs)) if len == text.len() => runs,
            _ => &[],
        }
    }
}

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
    pub highlight: Option<&'a Highlight>,
    /// Goes up with every new `highlight`.
    pub highlight_id: u64,
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
    pub highlights: [Option<&'a Highlight>; 2],
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
    /// When to color the text again, after the last keystroke.
    highlight_due: Option<f64>,
    /// The last layout, with which highlight and theme it was made.
    galley: Option<(Arc<Galley>, u64, bool)>,
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
    painter.text(at, Align2::RIGHT_CENTER, number, fonts::mono(TEXT_XS), color);
}

/// `line` with no more of it than is drawn, and no line ending.
fn drawn_part(line: &str) -> &str {
    let line = line.trim_end_matches(['\r', '\n']);
    &line[..line.floor_char_boundary(LINE_BYTES)]
}

/// The color of each stretch of `line`, split where a run or the `changed` part starts or ends.
fn line_job(line: &str, runs: &[TokenRun], changed: Option<(usize, usize)>, p: &Palette, mark: Color32) -> LayoutJob {
    let format = TextFormat { font_id: fonts::mono(TEXT_SM), color: p.foreground, line_height: Some(LINE), ..Default::default() };
    let mut cuts = vec![0, line.len()];
    let clamp = |at: usize| line.floor_char_boundary(at.min(line.len()));
    for run in runs {
        cuts.extend([clamp(run.start as usize), clamp(run.end as usize)]);
    }
    if let Some((start, end)) = changed {
        cuts.extend([clamp(start), clamp(end)]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut job = LayoutJob::default();
    for pair in cuts.windows(2) {
        let [start, end] = [pair[0], pair[1]];
        let color = runs
            .iter()
            .find(|run| (run.start as usize) <= start && end <= run.end as usize)
            .map_or(p.foreground, |run| p.token(run.token));
        let marked = changed.is_some_and(|(from, to)| from < to && from <= start && end <= to);
        let background = if marked { mark } else { Color32::TRANSPARENT };
        job.append(&line[start..end], 0., TextFormat { color, background, ..format.clone() });
    }
    job
}

/// A line starting at `x`, colored by `runs`, with its `changed` bytes marked.
#[allow(clippy::too_many_arguments)]
fn paint_line(
    painter: &Painter,
    x: f32,
    y: f32,
    line: &str,
    runs: &[TokenRun],
    changed: Option<(usize, usize)>,
    p: &Palette,
    mark: Color32,
) {
    let line = drawn_part(line);
    if line.is_empty() {
        return;
    }
    let galley: Arc<Galley> = painter.layout_job(line_job(line, runs, changed, p, mark));
    painter.galley(pos2(snap(painter, x), snap(painter, y)), galley, p.foreground);
}

/// All of `text`, each line colored by `highlight` as far as that still fits it.
fn editor_job(text: &str, highlight: Option<&Highlight>, p: &Palette) -> LayoutJob {
    let format = TextFormat { font_id: fonts::mono(TEXT_SM), color: p.foreground, line_height: Some(LINE), ..Default::default() };
    let mut job = LayoutJob::default();
    let mut section = |range: std::ops::Range<usize>, color: Color32| {
        if !range.is_empty() {
            job.append(&text[range], 0., TextFormat { color, ..format.clone() });
        }
    };
    let mut line_start = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        let mut at = 0;
        for run in highlight.map_or(&[][..], |highlight| highlight.runs_for(index, content)) {
            let (start, end) = (run.start as usize, (run.end as usize).min(content.len()));
            if start < at || start >= end || !content.is_char_boundary(start) || !content.is_char_boundary(end) {
                continue;
            }
            section(line_start + at..line_start + start, p.foreground);
            section(line_start + start..line_start + end, p.token(run.token));
            at = end;
        }
        section(line_start + at..line_start + line.len(), p.foreground);
        line_start += line.len();
    }
    job
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
        let line = &view.text[start..end];
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        let runs = view.highlight.map_or(&[][..], |highlight| highlight.runs_for(index, content));
        paint_line(&text_painter, text_left - state.across, y, line, runs, None, p, p.accent);
    }
    scrollbar(ui, rows, id, lines.len(), state, &[], p);
}

/// The field a file is edited in, with its line numbers. Returns whether to save.
#[allow(clippy::too_many_arguments)]
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
    let gutter = gutter_width(ui, state.line_count);
    let area = rows.with_min_x(rows.left() + gutter + 8.);

    let mut child = ui.new_child(UiBuilder::new().id_salt(id.with("editor")).max_rect(area).layout(Layout::top_down(Align::LEFT)));
    let key = (view.highlight_id, ui.visuals().dark_mode);
    let BodyState { buffer, galley: cache, .. } = state;
    // With every token a section of its own, this is worth not building at every pass.
    let mut layouter = |ui: &Ui, text: &dyn TextBuffer, _wrap: f32| {
        if let Some((galley, id, dark)) = cache.as_ref()
            && (*id, *dark) == key
            && galley.job.text == text.as_str()
        {
            return galley.clone();
        }
        let galley = ui.painter().layout_job(editor_job(text.as_str(), view.highlight, p));
        *cache = Some((galley.clone(), key.0, key.1));
        galley
    };
    let output = ScrollArea::both().id_salt(("scroll", view.id)).auto_shrink([false, false]).show(&mut child, |ui| {
        TextEdit::multiline(buffer)
            .id_salt(("text", view.id))
            .cursor_at_end(false)
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
        state.highlight_due = Some(ui.input(|i| i.time) + HIGHLIGHT_AFTER);
    }
    if let Some(due) = state.highlight_due {
        let now = ui.input(|i| i.time);
        if now >= due {
            state.highlight_due = None;
            actions.push(UiAction::Files(pane, FilesAction::Highlight { text: state.buffer.clone() }));
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(due - now));
        }
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
            state.highlight_due = None;
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
            let runs = view.highlights[side].map_or(&[][..], |highlight| highlight.runs_for(line.number as usize - 1, text));
            paint_line(&text_painter, text_left - state.across, y, text, runs, changed, p, marks[side]);
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

    fn run(start: u32, end: u32, token: Token) -> TokenRun {
        TokenRun { start, end, token }
    }

    #[test]
    fn colors_apply_only_to_a_line_that_still_is_the_length_it_was() {
        let found = Highlight { runs: vec![run(0, 2, Token::Keyword)], lines: vec![HighlightLine { first_run: 0, len: 4 }] };
        assert_eq!(found.runs_for(0, "abcd").len(), 1);
        assert!(found.runs_for(0, "abcde").is_empty(), "edited since");
        assert!(found.runs_for(1, "abcd").is_empty(), "added since");
    }

    #[test]
    fn stretches_cover_a_line_whatever_marks_it() {
        let p = &crate::theme::DARK;
        let job = line_job("let 名 = 1", &[run(0, 3, Token::Keyword)], Some((2, 7)), p, p.accent);
        assert_eq!(job.text, "let 名 = 1");
    }

    #[test]
    fn an_edit_that_shifted_lines_cannot_cut_a_character() {
        let p = &crate::theme::DARK;
        let stale = Highlight { runs: vec![run(1, 2, Token::Keyword)], lines: vec![HighlightLine { first_run: 0, len: 3 }] };
        assert_eq!(editor_job("字\n", Some(&stale), p).text, "字\n");
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
            highlight: None,
            highlight_id: 0,
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
