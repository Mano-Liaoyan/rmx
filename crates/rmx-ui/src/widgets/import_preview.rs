//! The preview of an import: what it creates, and in which folder, before anything is saved.
//!
//! It knows nothing of where the connections come from: whatever reads a file fills in the rows.

use egui::{Align, Context, Id, Layout, Rect, RichText, Sense, Ui, vec2};

use crate::action::{Protocol, UiAction};
use crate::draw::{hline, icon, palette};
use crate::theme::{CONTROL_HEIGHT, TEXT_LG, TEXT_SM};
use crate::{float, fonts, icons};

use super::controls::{ButtonKind, InputOptions, button, button_full, checkbox, text_input};

const WIDTH: f32 = 560.;
/// The list scrolls past this height.
const LIST_HEIGHT: f32 = 340.;
/// Where a row's details start, under its name rather than its tick box.
const INDENT: f32 = 26.;

/// A connection the import would create.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportRow {
    pub name: String,
    pub protocol: Protocol,
    /// Where it connects to, and as whom.
    pub detail: String,
    /// What of the source it does not carry over, a line each.
    pub notes: Vec<String>,
    /// Whether to create it: the user can untick it.
    pub include: bool,
}

/// Something of the source that is not a connection, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportProblem {
    pub source: String,
    pub reason: String,
}

/// What the preview shows, and what the user has made of it so far.
#[derive(Clone, Debug, Default)]
pub struct ImportPreviewState {
    pub title: String,
    pub rows: Vec<ImportRow>,
    pub problems: Vec<ImportProblem>,
    /// The folder they go in, as `a/b`; empty for the top of the tree.
    pub group: String,
}

impl ImportPreviewState {
    /// An empty preview, putting what it gets in `group`.
    pub fn new(title: impl Into<String>, group: impl Into<String>) -> Self {
        Self { title: title.into(), group: group.into(), ..Self::default() }
    }

    /// How many rows are ticked.
    pub fn included(&self) -> usize {
        self.rows.iter().filter(|row| row.include).count()
    }

    fn confirm(&self) -> UiAction {
        UiAction::ImportConfirm {
            group: self.group.trim().to_owned(),
            include: self.rows.iter().map(|row| row.include).collect(),
        }
    }
}

/// The modal dialog: [`UiAction::ImportConfirm`] to go ahead, [`UiAction::PromptCancel`] not to.
pub fn import_preview_dialog(ctx: &Context, state: &mut ImportPreviewState) -> Option<UiAction> {
    let id = Id::unique("import-preview");
    let modal = float::modal(ctx, id, WIDTH, |ui| body(ui, id, state));
    modal.inner.or(modal.dismissed.then_some(UiAction::PromptCancel))
}

fn body(ui: &mut Ui, id: Id, state: &mut ImportPreviewState) -> Option<UiAction> {
    let p = palette(ui);
    let mut action = None;
    ui.spacing_mut().item_spacing.y = 0.;
    egui::Frame::new().inner_margin(16).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 8.;
        ui.label(RichText::new(&state.title).font(fonts::medium(TEXT_LG)).color(p.foreground));
        ui.label(
            RichText::new(
                "Nothing is saved until you import. Passwords are asked for on connecting.",
            )
            .color(p.muted_fg)
            .size(TEXT_SM),
        );
        ui.label(RichText::new("Into the folder").color(p.foreground).size(TEXT_SM));
        let hint = "The top of the tree";
        text_input(
            ui,
            id.with("group"),
            &mut state.group,
            InputOptions { hint, ..Default::default() },
        );
        egui::ScrollArea::vertical().max_height(LIST_HEIGHT).id_salt(id.with("list")).show(
            ui,
            |ui| {
                for (index, row) in state.rows.iter_mut().enumerate() {
                    show_row(ui, id.with(("row", index)), row);
                }
                for problem in &state.problems {
                    show_problem(ui, problem);
                }
            },
        );
    });
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), Sense::hover());
    hline(ui.painter(), line.left(), line.right(), line.top(), p.border);
    let count = state.included();
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        let size = vec2(ui.available_width(), CONTROL_HEIGHT);
        ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
            let label = format!("Import {count}");
            let submit = button_full(ui, None, &label, ButtonKind::Primary, count > 0, None);
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if count > 0 && (submit.clicked() || enter) {
                action = Some(state.confirm());
            }
            if button(ui, "Cancel", ButtonKind::Secondary).clicked() {
                action = Some(UiAction::PromptCancel);
            }
        });
    });
    action
}

/// A connection: its tick box and name, where it goes, and what it leaves behind.
fn show_row(ui: &mut Ui, id: Id, row: &mut ImportRow) {
    let p = palette(ui);
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), CONTROL_HEIGHT), Sense::hover());
    let name = format!("{} \u{b7} {}", row.name, row.protocol.label());
    checkbox(ui, rect, id, &mut row.include, &name);
    let faded = if row.include { 1. } else { 0.5 };
    ui.spacing_mut().item_spacing.y = 2.;
    let indented = |ui: &mut Ui, text: RichText| {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(INDENT);
            ui.set_max_width(ui.available_width());
            ui.label(text);
        });
    };
    indented(ui, RichText::new(&row.detail).color(p.muted_fg.gamma_multiply(faded)).size(TEXT_SM));
    for note in &row.notes {
        indented(ui, RichText::new(note).color(p.warning.gamma_multiply(faded)).size(TEXT_SM));
    }
    ui.spacing_mut().item_spacing.y = 8.;
}

/// A source that gave no connection.
fn show_problem(ui: &mut Ui, problem: &ImportProblem) {
    let p = palette(ui);
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), CONTROL_HEIGHT), Sense::hover());
    let mark = Rect::from_center_size(rect.left_center() + vec2(8., 0.), vec2(16., 16.));
    icon(ui.painter(), mark.center(), icons::TRIANGLE_ALERT, 14., p.danger);
    ui.painter().text(
        rect.left_center() + vec2(INDENT, 0.),
        egui::Align2::LEFT_CENTER,
        format!("{}: {}", problem.source, problem.reason),
        fonts::regular(TEXT_SM),
        p.danger,
    );
}
