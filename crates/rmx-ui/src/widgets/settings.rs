//! The settings dialog, and the form describing a sync backend.

use std::fmt;

use egui::{Align, Context, Id, Layout, RichText, Sense, Ui, vec2};

use crate::action::UiAction;
use crate::draw::{hline, palette};
use crate::float::Resizable;
use crate::theme::{CONTROL_HEIGHT, TEXT_LG, TEXT_SM, ThemeMode};
use crate::{float, fonts, icons};

use super::controls::{ButtonKind, InputOptions, button, button_full, segmented, text_input};
use super::panels::{Panels, group_rule};

const WIDTH: f32 = 560.;
const MIN_SIZE: [f32; 2] = [420., 260.];
const LABEL: f32 = 112.;

/// Where a vault is synced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncKind {
    #[default]
    None,
    Local,
    WebDav,
    S3,
    Sftp,
}

const SYNC_KINDS: [SyncKind; 5] =
    [SyncKind::None, SyncKind::Local, SyncKind::WebDav, SyncKind::S3, SyncKind::Sftp];

/// A sync backend as entered. Holds passwords: its `Debug` names only the kind.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SyncDraft {
    pub kind: SyncKind,
    /// The directory or file for a local backend; the remote file for SFTP.
    pub path: String,
    /// WebDAV: the file's URL. S3: the endpoint.
    pub url: String,
    pub user: String,
    /// WebDAV or SFTP password, or the S3 secret key.
    pub secret: String,
    pub bucket: String,
    pub region: String,
    /// S3: the object key.
    pub key: String,
    pub access_key: String,
    pub host: String,
    pub port: String,
}

impl fmt::Debug for SyncDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncDraft").field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// Everything the settings dialog edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDraft {
    pub theme: ThemeMode,
    /// Minutes without input until the vault locks; empty or 0 never does.
    pub auto_lock_minutes: String,
    pub sync: SyncDraft,
    pub sync_on_save: bool,
    /// Minutes between syncs; empty or 0 only syncs when asked.
    pub sync_interval_minutes: String,
}

/// The dialog's state.
#[derive(Default)]
pub struct SettingsState {
    draft: Option<Box<SettingsDraft>>,
}

impl SettingsState {
    /// Show the dialog on these settings.
    pub fn open(&mut self, draft: SettingsDraft) {
        self.draft = Some(Box::new(draft));
    }

    /// Whether it is showing.
    pub fn is_open(&self) -> bool {
        self.draft.is_some()
    }

    /// Hide it, forgetting what was entered.
    pub fn close(&mut self) {
        self.draft = None;
    }
}

/// What the dialog reports.
#[derive(Clone, Copy, Debug)]
pub struct SettingsView<'a> {
    /// The vault's file, as it is shown.
    pub vault: &'a str,
    pub syncing: bool,
    /// The database has no master password, so there is nothing to lock.
    pub unencrypted: bool,
}

fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            let (cell, _) =
                ui.allocate_exact_size(vec2(LABEL - 8., CONTROL_HEIGHT), Sense::hover());
            ui.painter().text(
                cell.left_center(),
                egui::Align2::LEFT_CENTER,
                label,
                fonts::regular(TEXT_SM),
                p.foreground,
            );
            add(ui);
        },
    );
}

fn field(ui: &mut Ui, salt: Id, label: &str, value: &mut String, hint: &str, password: bool) {
    row(ui, label, |ui| {
        text_input(
            ui,
            salt.with(label),
            value,
            InputOptions { hint, password, ..Default::default() },
        );
    });
}

/// The fields of a sync backend. With `none`, not syncing is one of the choices.
pub fn sync_fields(ui: &mut Ui, salt: Id, sync: &mut SyncDraft, none: bool) {
    row(ui, "Sync with", |ui| {
        let (labels, kinds): (&[&str], &[SyncKind]) = if none {
            (&["Off", "Local", "WebDAV", "S3", "SFTP"], &SYNC_KINDS)
        } else {
            (&["Local", "WebDAV", "S3", "SFTP"], &SYNC_KINDS[1..])
        };
        if !none && sync.kind == SyncKind::None {
            sync.kind = SyncKind::Local;
        }
        let mut selected = kinds.iter().position(|&kind| kind == sync.kind).unwrap_or(0);
        if segmented(ui, salt.with("kind"), labels, &mut selected).changed() {
            sync.kind = kinds[selected];
        }
    });
    match sync.kind {
        SyncKind::None => {},
        SyncKind::Local => {
            field(ui, salt, "Folder or file", &mut sync.path, "", false);
        },
        SyncKind::WebDav => {
            field(ui, salt, "File URL", &mut sync.url, "", false);
            field(ui, salt, "User", &mut sync.user, "", false);
            field(ui, salt, "Password", &mut sync.secret, "", true);
        },
        SyncKind::S3 => {
            field(ui, salt, "Endpoint", &mut sync.url, "", false);
            field(ui, salt, "Bucket", &mut sync.bucket, "", false);
            field(ui, salt, "Region", &mut sync.region, "", false);
            field(ui, salt, "Object key", &mut sync.key, "", false);
            field(ui, salt, "Access key", &mut sync.access_key, "", false);
            field(ui, salt, "Secret key", &mut sync.secret, "", true);
        },
        SyncKind::Sftp => {
            field(ui, salt, "Host", &mut sync.host, "", false);
            field(ui, salt, "Port", &mut sync.port, "", false);
            field(ui, salt, "User", &mut sync.user, "", false);
            field(ui, salt, "Password", &mut sync.secret, "", true);
            field(ui, salt, "Remote file", &mut sync.path, "", false);
        },
    }
}

/// The modal dialog, if it is open.
pub fn settings_dialog(
    ctx: &Context,
    state: &mut SettingsState,
    view: &SettingsView<'_>,
    panels: &mut Panels,
) -> Option<UiAction> {
    let draft = state.draft.as_deref_mut()?;
    let id = Id::unique("settings-dialog");
    let default = [WIDTH, (ctx.content_rect().height() - 120.).max(MIN_SIZE[1])];
    let mut changed = false;
    let resize =
        Resizable { size: &mut panels.settings, default, min: MIN_SIZE, changed: &mut changed };
    let modal = float::resizable_modal(ctx, id, resize, |ui, size| {
        settings_body(ui, id, draft, view, size[1])
    });
    if changed {
        return Some(UiAction::PanelsChanged);
    }
    let action = modal.inner.or(modal.dismissed.then_some(UiAction::SettingsCancel));
    if matches!(
        action,
        Some(
            UiAction::SettingsCancel
                | UiAction::SettingsSave(_)
                | UiAction::VaultAskMove
                | UiAction::VaultAskPassword
        )
    ) {
        state.close();
        ctx.request_repaint();
    }
    action
}

fn on_off(ui: &mut Ui, salt: Id, value: &mut bool) {
    let mut selected = usize::from(*value);
    if segmented(ui, salt, &["Off", "On"], &mut selected).changed() {
        *value = selected == 1;
    }
}

fn settings_body(
    ui: &mut Ui,
    id: Id,
    draft: &mut SettingsDraft,
    view: &SettingsView<'_>,
    max_height: f32,
) -> Option<UiAction> {
    let p = palette(ui);
    let mut action = None;
    ui.spacing_mut().item_spacing.y = 0.;
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.label(RichText::new("Settings").font(fonts::medium(TEXT_LG)).color(p.foreground));
    });
    let rule = |ui: &mut Ui| {
        let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), Sense::hover());
        hline(ui.painter(), line.left(), line.right(), line.top(), p.border);
    };
    rule(ui);

    egui::ScrollArea::vertical().max_height(max_height - 110.).show(ui, |ui| {
        egui::Frame::new().inner_margin(16).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.;
            row(ui, "Theme", |ui| {
                let mut selected = usize::from(draft.theme == ThemeMode::Light);
                if segmented(ui, id.with("theme"), &["Dark", "Light"], &mut selected).changed() {
                    draft.theme = [ThemeMode::Dark, ThemeMode::Light][selected];
                }
            });

            if !view.unencrypted {
                group_rule(ui);
                field(ui, id, "Auto-lock, min", &mut draft.auto_lock_minutes, "", false);
                row(ui, "Master password", |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button(ui, "Change", ButtonKind::Secondary).clicked() {
                            action = Some(UiAction::VaultAskPassword);
                        }
                    });
                });
            }

            group_rule(ui);
            row(ui, "Vault file", |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, "Move", ButtonKind::Secondary).clicked() {
                        action = Some(UiAction::VaultAskMove);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let path =
                            RichText::new(view.vault).font(fonts::mono(TEXT_SM)).color(p.muted_fg);
                        ui.add(egui::Label::new(path).truncate()).on_hover_text(view.vault);
                    });
                });
            });

            group_rule(ui);
            sync_fields(ui, id.with("sync"), &mut draft.sync, true);
            if draft.sync.kind != SyncKind::None {
                row(ui, "Sync on save", |ui| {
                    on_off(ui, id.with("on-save"), &mut draft.sync_on_save)
                });
                field(ui, id, "Every, min", &mut draft.sync_interval_minutes, "", false);
            }

            group_rule(ui);
            let glyph = Some(icons::KEY_ROUND);
            let import =
                button_full(ui, glyph, "Import ~/.ssh/config", ButtonKind::Secondary, true, None);
            if import.clicked() {
                action = Some(UiAction::ImportSshConfig);
            }
        });
    });

    rule(ui);
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        let size = vec2(ui.available_width(), CONTROL_HEIGHT);
        ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
            if button(ui, "Save", ButtonKind::Primary).clicked() {
                action = Some(UiAction::SettingsSave(Box::new(draft.clone())));
            }
            if button(ui, "Cancel", ButtonKind::Secondary).clicked() {
                action = Some(UiAction::SettingsCancel);
            }
            let ready = draft.sync.kind != SyncKind::None && !view.syncing;
            if button_full(ui, None, "Sync now", ButtonKind::Secondary, ready, None).clicked() {
                action = Some(UiAction::SyncNow);
            }
        });
    });
    action
}
