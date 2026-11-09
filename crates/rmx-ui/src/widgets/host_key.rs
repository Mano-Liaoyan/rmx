//! Asking whether to trust a server's host key.

use egui::{Context, Id, Layout, RichText, Ui, pos2, vec2};

use crate::action::UiAction;
use crate::draw::{hline, icon, palette};
use crate::theme::{CONTROL_HEIGHT, TEXT_LG, TEXT_SM};
use crate::{float, fonts, icons};

use super::controls::{ButtonKind, button, button_full};

const WIDTH: f32 = 520.;

/// How the key compares with what is on record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// Never seen this host.
    Unknown,
    /// The host's key differs from the one recorded.
    Changed,
}

/// What the dialog shows.
#[derive(Clone, Copy, Debug)]
pub struct HostKeyView<'a> {
    pub status: HostKeyStatus,
    pub host: &'a str,
    pub port: u16,
    pub algorithm: &'a str,
    /// SHA-256 fingerprint of the presented key.
    pub fingerprint: &'a str,
    /// The fingerprint on record, when it changed.
    pub previous: Option<&'a str>,
}

/// The modal host-key dialog.
pub fn host_key_dialog(ctx: &Context, view: &HostKeyView<'_>) -> Option<UiAction> {
    let modal = float::modal(ctx, Id::unique("host-key"), WIDTH, |ui| host_key_body(ui, view));
    modal.inner.or(modal.dismissed.then_some(UiAction::HostKeyReject))
}

/// The dialog's content, for hosting in any surface.
pub fn host_key_body(ui: &mut Ui, view: &HostKeyView<'_>) -> Option<UiAction> {
    let p = palette(ui);
    let mut action = None;
    ui.spacing_mut().item_spacing.y = 0.;
    egui::Frame::new().inner_margin(16).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 8.;
        ui.spacing_mut().interact_size.y = 20.;
        let (glyph, color, title, detail) = match view.status {
            HostKeyStatus::Unknown => (
                icons::SHIELD_CHECK,
                p.info,
                "Verify host key",
                "This host has not been seen before. Check the fingerprint with its administrator.",
            ),
            HostKeyStatus::Changed => (
                icons::SHIELD_ALERT,
                p.warning,
                "Host key changed",
                "The key differs from the one on record. This can mean the connection is being \
                 intercepted.",
            ),
        };
        let (rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), 20.), egui::Sense::hover());
        icon(ui.painter(), pos2(rect.left() + 8., rect.center().y), glyph, 16., color);
        ui.painter().text(
            pos2(rect.left() + 26., rect.center().y),
            egui::Align2::LEFT_CENTER,
            title,
            fonts::medium(TEXT_LG),
            p.foreground,
        );
        ui.add_space(4.);
        ui.label(RichText::new(detail).color(p.muted_fg).size(TEXT_SM));
        ui.add_space(4.);

        let field = |ui: &mut Ui, label: &str, value: &str, mono: bool| {
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    vec2(84., 20.),
                    Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_size(vec2(84., 20.));
                        ui.label(RichText::new(label).color(p.muted_fg).size(TEXT_SM));
                    },
                );
                let font = if mono { fonts::mono(TEXT_SM) } else { fonts::regular(TEXT_SM) };
                ui.add(
                    egui::Label::new(RichText::new(value).font(font).color(p.foreground)).wrap(),
                );
            });
        };
        let address = format!("{}:{}", view.host, view.port);
        field(ui, "Host", &address, false);
        field(ui, "Type", view.algorithm, false);
        field(ui, "Fingerprint", view.fingerprint, true);
        if let Some(previous) = view.previous {
            field(ui, "On record", previous, true);
        }
    });
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), egui::Sense::hover());
    hline(ui.painter(), line.left(), line.right(), line.top(), p.border);
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        let size = vec2(ui.available_width(), CONTROL_HEIGHT);
        ui.allocate_ui_with_layout(size, Layout::right_to_left(egui::Align::Center), |ui| {
            // The safe choice takes the eye; trusting a changed key should take deliberate effort.
            let changed = view.status == HostKeyStatus::Changed;
            let (accept_kind, reject_kind) = if changed {
                (ButtonKind::Secondary, ButtonKind::Primary)
            } else {
                (ButtonKind::Primary, ButtonKind::Secondary)
            };
            if button(ui, "Trust and save", accept_kind).clicked() {
                action = Some(UiAction::HostKeyAcceptAlways);
            }
            if button_full(ui, None, "Connect once", ButtonKind::Secondary, true, None).clicked() {
                action = Some(UiAction::HostKeyAcceptOnce);
            }
            if button(ui, "Reject", reject_kind).clicked() {
                action = Some(UiAction::HostKeyReject);
            }
        });
    });
    action
}
