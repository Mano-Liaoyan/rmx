//! A dialog asking for a few lines of text: a password, a passphrase, a name, or a confirmation.

use egui::{Align, Context, Id, Layout, RichText, Ui, vec2};

use crate::action::{Sensitive, UiAction};
use crate::draw::{hline, palette};
use crate::theme::{CONTROL_HEIGHT, TEXT_LG, TEXT_SM};
use crate::{float, fonts};

use super::controls::{ButtonKind, InputOptions, button, button_full, text_input};

const WIDTH: f32 = 420.;

/// One line to answer.
#[derive(Clone, Default)]
pub struct PromptField {
    pub label: String,
    /// Whether the answer is hidden as it is typed.
    pub secret: bool,
    pub value: String,
}

/// What the dialog asks.
#[derive(Clone, Default)]
pub struct PromptState {
    pub title: String,
    pub detail: String,
    /// None at all makes it a confirmation.
    pub fields: Vec<PromptField>,
    pub submit: String,
    focused: bool,
}

impl PromptState {
    /// A dialog with these contents.
    pub fn new(title: impl Into<String>, detail: impl Into<String>, fields: Vec<PromptField>) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            fields,
            submit: "Continue".to_owned(),
            focused: false,
        }
    }

    fn take_answers(&mut self) -> Vec<Sensitive> {
        self.fields.iter_mut().map(|field| Sensitive::new(std::mem::take(&mut field.value))).collect()
    }
}

/// The modal dialog.
pub fn prompt_dialog(ctx: &Context, state: &mut PromptState) -> Option<UiAction> {
    let id = Id::unique("prompt-dialog");
    let modal = float::modal(ctx, id, WIDTH, |ui| prompt_body(ui, id, state));
    modal.inner.or(modal.dismissed.then_some(UiAction::PromptCancel))
}

fn prompt_body(ui: &mut Ui, id: Id, state: &mut PromptState) -> Option<UiAction> {
    let p = palette(ui);
    let mut action = None;
    ui.spacing_mut().item_spacing.y = 0.;
    egui::Frame::new().inner_margin(16).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 8.;
        ui.label(RichText::new(&state.title).font(fonts::medium(TEXT_LG)).color(p.foreground));
        if !state.detail.is_empty() {
            ui.label(RichText::new(&state.detail).color(p.muted_fg).size(TEXT_SM));
        }
        let focus = !state.focused;
        state.focused = true;
        for (index, field) in state.fields.iter_mut().enumerate() {
            if !field.label.is_empty() {
                ui.label(RichText::new(&field.label).color(p.foreground).size(TEXT_SM));
            }
            let response = text_input(ui, id.with(index), &mut field.value, InputOptions {
                password: field.secret,
                ..Default::default()
            });
            if focus && index == 0 {
                response.request_focus();
            }
        }
    });
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), egui::Sense::hover());
    hline(ui.painter(), line.left(), line.right(), line.top(), p.border);
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        let size = vec2(ui.available_width(), CONTROL_HEIGHT);
        ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
            let submit = button_full(ui, None, &state.submit, ButtonKind::Primary, true, None);
            if submit.clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                action = Some(UiAction::PromptSubmit(state.take_answers()));
            }
            if button(ui, "Cancel", ButtonKind::Secondary).clicked() {
                action = Some(UiAction::PromptCancel);
            }
        });
    });
    action
}
