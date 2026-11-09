//! The screen shown until the vault is unlocked.

use std::mem;

use egui::text_edit::TextEditState;
use egui::{Align2, CornerRadius, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::action::{Sensitive, UiAction};
use crate::draw::{icon, palette};
use crate::theme::{RADIUS, TEXT_LG, TEXT_SM};
use crate::{fonts, icons};

use super::controls::{ButtonKind, InputOptions, button_full, text_input};
use super::settings::{SyncDraft, SyncKind, sync_fields};

const WIDTH: f32 = 320.;

/// What only the screen itself holds.
#[derive(Default)]
pub struct VaultState {
    password: String,
    key_file: String,
    focused_once: bool,
}

/// What the app says about the vault.
#[derive(Clone, Copy, Debug)]
pub struct VaultView<'a> {
    /// The vault's name or path.
    pub name: &'a str,
    /// Key derivation is running.
    pub busy: bool,
    /// Why the last attempt failed.
    pub error: Option<&'a str>,
}

/// Draw the unlock screen, filling `rect`.
pub fn vault_unlock(
    ui: &mut Ui,
    rect: Rect,
    state: &mut VaultState,
    view: &VaultView<'_>,
) -> Option<UiAction> {
    let p = palette(ui);
    ui.painter().rect_filled(rect, 0., p.background);

    let height = if view.error.is_some() { 256. } else { 234. };
    let area = Rect::from_center_size(rect.center(), vec2(WIDTH, height));
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Center)),
    );
    let ui = &mut child;

    let (badge, _) = ui.allocate_exact_size(vec2(40., 40.), Sense::hover());
    ui.painter().rect(
        badge,
        CornerRadius::same(RADIUS),
        p.chrome,
        Stroke::new(1., p.border),
        StrokeKind::Inside,
    );
    icon(ui.painter(), badge.center(), icons::LOCK, 18., p.foreground);
    ui.add_space(4.);

    let (title, _) = ui.allocate_exact_size(vec2(WIDTH, 40.), Sense::hover());
    ui.painter().text(
        pos2(title.center().x, title.top() + 10.),
        Align2::CENTER_CENTER,
        "Unlock vault",
        fonts::medium(TEXT_LG),
        p.foreground,
    );
    ui.painter().text(
        pos2(title.center().x, title.top() + 30.),
        Align2::CENTER_CENTER,
        view.name,
        fonts::regular(TEXT_SM),
        p.muted_fg,
    );

    let salt = Id::unique("vault-password");
    ui.add_enabled_ui(!view.busy, |ui| {
        let input = text_input(ui, salt, &mut state.password, InputOptions {
            hint: "Master password",
            password: true,
            width: Some(WIDTH),
            ..Default::default()
        });
        if !state.focused_once {
            input.request_focus();
            state.focused_once = true;
        }
        text_input(ui, salt.with("key"), &mut state.key_file, InputOptions {
            hint: "Key file, if the vault has one",
            width: Some(WIDTH),
            ..Default::default()
        });
    });

    if let Some(error) = view.error {
        let (rect, _) = ui.allocate_exact_size(vec2(WIDTH, 14.), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            error,
            fonts::regular(TEXT_SM),
            p.danger,
        );
    }

    let ready = !view.busy && !state.password.is_empty();
    let label = if view.busy { "Unlocking..." } else { "Unlock" };
    let glyph = view.busy.then_some(icons::LOADER_CIRCLE);
    let submit = button_full(ui, glyph, label, ButtonKind::Primary, ready, Some(WIDTH));
    if view.busy {
        // Slow enough not to hold the frame rate up while the key is derived.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }

    let entered = ui.input(|i| i.key_pressed(Key::Enter));
    if ready && (submit.clicked() || entered) {
        // egui's undo history holds copies of the text: drop it with the field.
        let id = ui.make_persistent_id(salt);
        ui.ctx().data_mut(|data| data.remove::<TextEditState>(id));
        state.focused_once = false;
        return Some(UiAction::VaultUnlock {
            password: Sensitive::new(mem::take(&mut state.password)),
            key_file: state.key_file.clone(),
        });
    }
    None
}

/// What only the first-run screen holds.
#[derive(Default)]
pub struct VaultSetupState {
    password: String,
    confirm: String,
    key_file: String,
    joining: bool,
    sync: SyncDraft,
    focused_once: bool,
}

/// What the app says about the vault being set up.
#[derive(Clone, Copy, Debug)]
pub struct VaultSetupView<'a> {
    /// Where the vault will live.
    pub path: &'a str,
    /// Key derivation or a download is running.
    pub busy: bool,
    /// Why the last attempt failed.
    pub error: Option<&'a str>,
}

/// Draw the screen shown when there is no vault yet: make one, or join one from a sync backend.
pub fn vault_setup(
    ui: &mut Ui,
    rect: Rect,
    state: &mut VaultSetupState,
    view: &VaultSetupView<'_>,
) -> Option<UiAction> {
    const TALL_WIDTH: f32 = 400.;
    let p = palette(ui);
    ui.painter().rect_filled(rect, 0., p.background);

    let height = f32::min(if state.joining { 520. } else { 320. }, rect.height() - 16.);
    let area = Rect::from_center_size(rect.center(), vec2(TALL_WIDTH, height));
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Center)),
    );
    let ui = &mut child;
    ui.spacing_mut().item_spacing.y = 8.;

    let (badge, _) = ui.allocate_exact_size(vec2(40., 40.), Sense::hover());
    ui.painter().rect(
        badge,
        CornerRadius::same(RADIUS),
        p.chrome,
        Stroke::new(1., p.border),
        StrokeKind::Inside,
    );
    icon(ui.painter(), badge.center(), icons::LOCK, 18., p.foreground);

    let (title, _) = ui.allocate_exact_size(vec2(TALL_WIDTH, 40.), Sense::hover());
    let heading = if state.joining { "Join a synced vault" } else { "Create your vault" };
    ui.painter().text(
        pos2(title.center().x, title.top() + 10.),
        Align2::CENTER_CENTER,
        heading,
        fonts::medium(TEXT_LG),
        p.foreground,
    );
    ui.painter().text(
        pos2(title.center().x, title.top() + 30.),
        Align2::CENTER_CENTER,
        view.path,
        fonts::regular(TEXT_SM),
        p.muted_fg,
    );

    let salt = Id::unique("vault-setup");
    ui.add_enabled_ui(!view.busy, |ui| {
        let input = text_input(ui, salt.with("password"), &mut state.password, InputOptions {
            hint: "Master password",
            password: true,
            width: Some(TALL_WIDTH),
            ..Default::default()
        });
        if !state.focused_once {
            input.request_focus();
            state.focused_once = true;
        }
        if !state.joining {
            text_input(ui, salt.with("confirm"), &mut state.confirm, InputOptions {
                hint: "Repeat the password",
                password: true,
                width: Some(TALL_WIDTH),
                ..Default::default()
            });
        }
        text_input(ui, salt.with("key"), &mut state.key_file, InputOptions {
            hint: "Key file, optional",
            width: Some(TALL_WIDTH),
            ..Default::default()
        });
        if state.joining {
            sync_fields(ui, salt.with("sync"), &mut state.sync, false);
        }
    });

    let differs = !state.joining && !state.confirm.is_empty() && state.password != state.confirm;
    let error = view.error.or(differs.then_some("The two passwords differ"));
    if let Some(error) = error {
        let (rect, _) = ui.allocate_exact_size(vec2(TALL_WIDTH, 14.), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            error,
            fonts::regular(TEXT_SM),
            p.danger,
        );
    }

    let filled = !state.password.is_empty() && !differs;
    let ready = !view.busy
        && filled
        && if state.joining { state.sync.kind != SyncKind::None } else { !state.confirm.is_empty() };
    let label = match (view.busy, state.joining) {
        (true, _) => "Working...",
        (false, true) => "Join vault",
        (false, false) => "Create vault",
    };
    let glyph = view.busy.then_some(icons::LOADER_CIRCLE);
    let submit = button_full(ui, glyph, label, ButtonKind::Primary, ready, Some(TALL_WIDTH));
    let other = if state.joining { "Create a new vault instead" } else { "Join a vault from sync" };
    if button_full(ui, None, other, ButtonKind::Ghost, !view.busy, Some(TALL_WIDTH)).clicked() {
        state.joining = !state.joining;
        ui.ctx().request_repaint();
    }
    if view.busy {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }

    let entered = ui.input(|i| i.key_pressed(Key::Enter));
    if ready && (submit.clicked() || entered) {
        for field in ["password", "confirm"] {
            let id = ui.make_persistent_id(salt.with(field));
            ui.ctx().data_mut(|data| data.remove::<TextEditState>(id));
        }
        state.focused_once = false;
        state.confirm.clear();
        let password = Sensitive::new(mem::take(&mut state.password));
        let key_file = state.key_file.clone();
        return Some(if state.joining {
            UiAction::VaultJoin { password, key_file, sync: Box::new(mem::take(&mut state.sync)) }
        } else {
            UiAction::VaultCreate { password, key_file }
        });
    }
    None
}
