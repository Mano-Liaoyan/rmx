//! The snippets dialog: a searchable list on the left, the one being edited on the right.

use std::sync::Arc;

use egui::{
    Context, CornerRadius, Frame, Id, Margin, Rect, Sense, Stroke, TextEdit, Ui, pos2, vec2,
};

use crate::action::UiAction;
use crate::draw::{elided_at, hline, palette, vline};
use crate::theme::{CONTROL_HEIGHT, RADIUS, RADIUS_SM, TEXT_SM, TREE_ROW_HEIGHT};
use crate::virtual_list::{clamp_scroll, window};
use crate::float::Resizable;
use crate::{float, fonts, icons};

use super::controls::{ButtonKind, InputOptions, button_full, text_input};
use super::panels::{Panels, SNIPPETS_LIST, resize_handle};

const WIDTH: f32 = 720.;
const HEIGHT: f32 = 460.;
const MIN_SIZE: [f32; 2] = [520., 300.];
const LIST_MIN: f32 = 160.;
const PAD: f32 = 12.;
const ROW: f32 = TREE_ROW_HEIGHT;

/// A snippet as the vault has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetRow {
    /// The number the vault's thread knows it by.
    pub id: u64,
    pub name: String,
    pub body: String,
}

/// What the dialog remembers.
#[derive(Debug, Default)]
pub struct SnippetsState {
    open: bool,
    query: String,
    /// The snippet in the form, none for a new one.
    editing: Option<u64>,
    name: String,
    body: String,
    scroll: f64,
    /// The rows the query finds, as indices.
    matches: Vec<u32>,
    /// What `matches` was computed for: the query, and which list.
    filtered_for: Option<(String, usize)>,
    focus_query: bool,
}

impl SnippetsState {
    /// Show the dialog, starting on a new snippet.
    pub fn open(&mut self) {
        *self = Self { open: true, focus_query: true, ..Self::default() };
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Put `row` in the form.
    fn edit(&mut self, row: &SnippetRow) {
        self.editing = Some(row.id);
        row.name.clone_into(&mut self.name);
        row.body.clone_into(&mut self.body);
    }

    fn new_snippet(&mut self) {
        self.editing = None;
        self.name.clear();
        self.body.clear();
    }

    /// Bring `matches` up to date with the query and the list, doing nothing if nothing changed.
    fn refilter(&mut self, rows: &Arc<Vec<SnippetRow>>) {
        let list = Arc::as_ptr(rows) as usize;
        if self
            .filtered_for
            .as_ref()
            .is_some_and(|(query, for_list)| *query == self.query && *for_list == list)
        {
            return;
        }
        let query = self.query.to_lowercase();
        self.matches.clear();
        let hit = |row: &SnippetRow| {
            query.is_empty()
                || row.name.to_lowercase().contains(&query)
                || row.body.to_lowercase().contains(&query)
        };
        self.matches
            .extend(rows.iter().enumerate().filter(|(_, row)| hit(row)).map(|(i, _)| i as u32));
        self.filtered_for = Some((self.query.clone(), list));
        self.scroll = 0.;
    }
}

/// The modal dialog, for `rows`.
pub fn snippets_dialog(
    ctx: &Context,
    state: &mut SnippetsState,
    rows: &Arc<Vec<SnippetRow>>,
    panels: &mut Panels,
) -> Option<UiAction> {
    if !state.open {
        return None;
    }
    let id = Id::unique("snippets-dialog");
    let mut changed = false;
    let mut list = (panels.snippets_list, false);
    let resize = Resizable {
        size: &mut panels.snippets,
        default: [WIDTH, HEIGHT],
        min: MIN_SIZE,
        changed: &mut changed,
    };
    let modal = float::resizable_modal(ctx, id, resize, |ui, size| {
        snippets_body(ui, id, state, rows, size, &mut list)
    });
    panels.snippets_list = list.0;
    if changed || list.1 {
        return Some(UiAction::PanelsChanged);
    }
    if modal.dismissed {
        state.close();
        ctx.request_repaint();
        return None;
    }
    modal.inner
}

fn snippets_body(
    ui: &mut Ui,
    id: Id,
    state: &mut SnippetsState,
    rows: &Arc<Vec<SnippetRow>>,
    size: [f32; 2],
    list_width: &mut (f32, bool),
) -> Option<UiAction> {
    let p = palette(ui);
    ui.spacing_mut().item_spacing.y = 0.;
    let (rect, _) = ui.allocate_exact_size(vec2(size[0] - 2., size[1]), Sense::hover());
    let most = (size[0] - 280.).max(LIST_MIN);
    let width = list_width.0.clamp(LIST_MIN, most);
    let split = rect.left() + width;
    vline(ui.painter(), split, rect.top(), rect.bottom(), p.border);
    let band = Rect::from_min_max(pos2(split - 3., rect.top()), pos2(split + 3., rect.bottom()));
    let edge = resize_handle(ui, band, id.with("edge-list"), true, split);
    if let Some(x) = edge.to {
        list_width.0 = (x - rect.left()).clamp(LIST_MIN, most);
    }
    if edge.reset {
        list_width.0 = SNIPPETS_LIST;
    }
    list_width.1 = edge.released || edge.reset;

    let mut action = None;

    // The list, of which only what is on screen exists.
    let list = Rect::from_min_max(rect.min, pos2(split, rect.bottom()));
    let search =
        Rect::from_min_size(list.min + vec2(PAD, PAD), vec2(width - 2. * PAD, CONTROL_HEIGHT));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(search));
    let input = text_input(&mut child, id.with("query"), &mut state.query, InputOptions {
        hint: "Search",
        glyph: Some(icons::SEARCH),
        width: Some(search.width()),
        ..Default::default()
    });
    if std::mem::take(&mut state.focus_query) {
        input.request_focus();
    }
    state.refilter(rows);

    let rows_area = Rect::from_min_max(
        pos2(list.left() + 4., search.bottom() + 8.),
        pos2(list.right() - 4., list.bottom() - CONTROL_HEIGHT - PAD * 2.),
    );
    let (len, viewport) = (state.matches.len(), rows_area.height());
    if ui.rect_contains_pointer(rows_area) {
        state.scroll -= f64::from(ui.input(|i| i.smooth_scroll_delta.y));
    }
    state.scroll = clamp_scroll(state.scroll, len, ROW, viewport);
    let visible = window(state.scroll, len, ROW, viewport);
    let painter = ui.painter_at(rows_area);
    for at in visible.first..visible.end {
        let row = &rows[state.matches[at] as usize];
        let y = rows_area.top() + visible.top + (at - visible.first) as f32 * ROW;
        let row_rect = Rect::from_min_size(pos2(rows_area.left(), y), vec2(rows_area.width(), ROW));
        let response = ui.interact(row_rect.intersect(rows_area), id.with(row.id), Sense::click());
        let selected = state.editing == Some(row.id);
        if selected {
            painter.rect_filled(row_rect, CornerRadius::same(RADIUS_SM), p.accent);
        } else if response.hovered() {
            painter.rect_filled(row_rect, CornerRadius::same(RADIUS_SM), p.hover);
        }
        let color = if selected { p.foreground } else { p.muted_fg };
        elided_at(
            &painter,
            row_rect.left() + 10.,
            row_rect.center().y,
            &row.name,
            fonts::regular(TEXT_SM),
            color,
            row_rect.width() - 20.,
        );
        if response.clicked() {
            state.edit(row);
        }
    }
    let foot = Rect::from_min_size(
        pos2(list.left() + PAD, list.bottom() - CONTROL_HEIGHT - PAD),
        vec2(width - 2. * PAD, CONTROL_HEIGHT),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(foot));
    if button_full(
        &mut child,
        Some(icons::PLUS),
        "New snippet",
        ButtonKind::Secondary,
        true,
        Some(foot.width()),
    )
    .clicked()
    {
        state.new_snippet();
    }

    // The form.
    let form = Rect::from_min_max(pos2(split + 1., rect.top()), rect.max);
    let footer_y = form.bottom() - CONTROL_HEIGHT - PAD * 2.;
    hline(ui.painter(), form.left(), form.right(), footer_y, p.border);
    let body =
        Rect::from_min_max(form.min + vec2(16., 16.), pos2(form.right() - 16., footer_y - 12.));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id.with("form")).max_rect(body));
    child.spacing_mut().item_spacing.y = 6.;
    text_input(&mut child, id.with("name"), &mut state.name, InputOptions {
        hint: "Name",
        width: Some(body.width()),
        ..Default::default()
    });
    child.add_space(4.);
    let height = (body.bottom() - child.cursor().top()).max(CONTROL_HEIGHT);
    let frame = Frame::new()
        .stroke(Stroke::new(1., p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(8));
    frame.show(&mut child, |ui| {
        let size = vec2(body.width() - 18., height - 16.);
        ui.set_min_size(size);
        ui.add_sized(
            size,
            TextEdit::multiline(&mut state.body)
                .id_salt(id.with("body"))
                .frame(Frame::NONE)
                .font(fonts::mono(TEXT_SM))
                .text_color(p.foreground)
                .hint_text("{{name}} or {{name:default}} is asked for when sending"),
        );
    });

    let foot = Rect::from_min_size(
        pos2(form.left() + 16., footer_y + PAD),
        vec2(form.width() - 32., CONTROL_HEIGHT),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id.with("foot"))
            .max_rect(foot)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    child.spacing_mut().item_spacing.x = 8.;
    let can_save = !state.name.trim().is_empty() && !state.body.is_empty();
    if button_full(&mut child, None, "Save", ButtonKind::Primary, can_save, None).clicked() {
        action = Some(UiAction::SnippetSave {
            id: state.editing,
            name: state.name.trim().to_owned(),
            body: state.body.clone(),
        });
        if state.editing.is_none() {
            state.new_snippet();
        }
    }
    if let Some(id) = state.editing {
        if button_full(&mut child, None, "Send to terminal", ButtonKind::Secondary, true, None)
            .clicked()
        {
            action = Some(UiAction::SnippetSend(id));
            state.close();
        }
        if button_full(&mut child, Some(icons::TRASH_2), "Delete", ButtonKind::Ghost, true, None)
            .clicked()
        {
            action = Some(UiAction::SnippetDelete(id));
            state.new_snippet();
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, name: &str, body: &str) -> SnippetRow {
        SnippetRow { id, name: name.to_owned(), body: body.to_owned() }
    }

    #[test]
    fn the_query_finds_names_and_text_without_regard_to_case() {
        let rows = Arc::new(vec![
            row(1, "Deploy", "make release"),
            row(2, "disk", "df -h"),
            row(3, "logs", "journalctl"),
        ]);
        let mut state = SnippetsState::default();
        state.open();
        state.refilter(&rows);
        assert_eq!(state.matches, [0, 1, 2]);
        state.query = "dep".to_owned();
        state.refilter(&rows);
        assert_eq!(state.matches, [0]);
        state.query = "DF".to_owned();
        state.refilter(&rows);
        assert_eq!(state.matches, [1]);
    }

    #[test]
    fn opening_the_dialog_starts_on_a_new_snippet() {
        let mut state = SnippetsState::default();
        state.edit(&row(5, "a", "b"));
        state.open();
        assert_eq!(state.editing, None);
        assert!(state.name.is_empty());
        assert!(state.is_open());
    }
}
