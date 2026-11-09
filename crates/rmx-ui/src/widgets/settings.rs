//! The settings dialog, and the form describing a sync backend.

use std::fmt;

use egui::{Align, Context, Id, Layout, RichText, Sense, Ui, vec2};

use crate::action::UiAction;
use crate::draw::{hline, palette};
use crate::theme::{CONTROL_HEIGHT, TEXT_LG, TEXT_SM, ThemeMode};
use crate::{float, fonts, icons};

use super::controls::{ButtonKind, InputOptions, button, button_full, segmented, text_input};

const WIDTH: f32 = 560.;
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
    pub sync_status: &'a str,
    pub syncing: bool,
}

fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            let (cell, _) = ui.allocate_exact_size(vec2(LABEL - 8., CONTROL_HEIGHT), Sense::hover());
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
        text_input(ui, salt.with(label), value, InputOptions {
            hint,
            password,
            ..Default::default()
        });
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
            field(ui, salt, "Folder or file", &mut sync.path, "~/Sync/rmx.rmxdb", false);
        },
        SyncKind::WebDav => {
            field(ui, salt, "File URL", &mut sync.url, "https://dav.example.com/rmx.rmxdb", false);
            field(ui, salt, "User", &mut sync.user, "Optional", false);
            field(ui, salt, "Password", &mut sync.secret, "Stored in the vault", true);
        },
        SyncKind::S3 => {
            field(ui, salt, "Endpoint", &mut sync.url, "https://s3.example.com", false);
            field(ui, salt, "Bucket", &mut sync.bucket, "rmx", false);
            field(ui, salt, "Region", &mut sync.region, "us-east-1", false);
            field(ui, salt, "Object key", &mut sync.key, "rmx.rmxdb", false);
            field(ui, salt, "Access key", &mut sync.access_key, "", false);
            field(ui, salt, "Secret key", &mut sync.secret, "Stored in the vault", true);
        },
        SyncKind::Sftp => {
            field(ui, salt, "Host", &mut sync.host, "files.example.com", false);
            field(ui, salt, "Port", &mut sync.port, "22", false);
            field(ui, salt, "User", &mut sync.user, "", false);
            field(ui, salt, "Password", &mut sync.secret, "Stored in the vault", true);
            field(ui, salt, "Remote file", &mut sync.path, "/home/me/rmx.rmxdb", false);
        },
    }
}

/// The modal dialog, if it is open.
pub fn settings_dialog(
    ctx: &Context,
    state: &mut SettingsState,
    view: &SettingsView<'_>,
) -> Option<UiAction> {
    let draft = state.draft.as_deref_mut()?;
    let id = Id::unique("settings-dialog");
    let height = (ctx.content_rect().height() - 120.).max(120.);
    let modal = float::modal(ctx, id, WIDTH, |ui| settings_body(ui, id, draft, view, height));
    let action = modal.inner.or(modal.dismissed.then_some(UiAction::SettingsCancel));
    if matches!(action, Some(UiAction::SettingsCancel | UiAction::SettingsSave(_))) {
        state.close();
        ctx.request_repaint();
    }
    action
}

fn section(ui: &mut Ui, title: &str) {
    let p = palette(ui);
    ui.label(RichText::new(title).font(fonts::medium(TEXT_SM)).color(p.muted_fg));
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
            section(ui, "Appearance");
            row(ui, "Theme", |ui| {
                let mut selected = usize::from(draft.theme == ThemeMode::Light);
                if segmented(ui, id.with("theme"), &["Dark", "Light"], &mut selected).changed() {
                    draft.theme = [ThemeMode::Dark, ThemeMode::Light][selected];
                }
            });
            ui.add_space(8.);

            section(ui, "Security");
            field(
                ui,
                id,
                "Auto-lock after",
                &mut draft.auto_lock_minutes,
                "Minutes, empty for never",
                false,
            );
            ui.add_space(8.);

            section(ui, "Sync");
            sync_fields(ui, id.with("sync"), &mut draft.sync, true);
            if draft.sync.kind != SyncKind::None {
                row(ui, "Sync on save", |ui| on_off(ui, id.with("on-save"), &mut draft.sync_on_save));
                field(
                    ui,
                    id,
                    "Every",
                    &mut draft.sync_interval_minutes,
                    "Minutes, empty for only when asked",
                    false,
                );
            }
            ui.label(RichText::new(view.sync_status).color(p.muted_fg).size(TEXT_SM));
            ui.add_space(8.);

            section(ui, "Import");
            let glyph = Some(icons::KEY_ROUND);
            let import = button_full(ui, glyph, "Import ~/.ssh/config", ButtonKind::Secondary, true, None);
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
