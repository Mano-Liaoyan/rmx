//! The screen shown until the vault is unlocked.

use std::mem;

use egui::text_edit::TextEditState;
use egui::{Align2, CornerRadius, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::action::{Sensitive, UiAction};
use crate::draw::{icon, palette};
use crate::theme::{RADIUS, TEXT_LG, TEXT_SM};
use crate::{fonts, icons};

use super::controls::{ButtonKind, InputOptions, button_full, checkbox, text_input};
use super::settings::{SyncDraft, SyncKind, sync_fields};

const WIDTH: f32 = 320.;

/// What only the screen itself holds.
#[derive(Default)]
pub struct VaultState {
    password: String,
    key_file: String,
    focused_once: bool,
    /// The vault that what was typed is for.
    name: String,
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

    if state.name != view.name {
        state.name = view.name.to_owned();
        state.password.clear();
        state.key_file.clear();
        state.focused_once = false;
        ui.ctx().data_mut(|data| data.remove::<TextEditState>(ui.make_persistent_id(Id::unique("vault-password"))));
    }

    let height = if view.error.is_some() { 296. } else { 274. };
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
    let other = button_full(ui, None, "Other vaults", ButtonKind::Ghost, !view.busy, Some(WIDTH));
    if other.clicked() {
        return Some(UiAction::VaultMenu(other.rect.left_bottom() + vec2(0., 4.)));
    }
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
    /// The user has read the warning about having no master password, and agreed.
    plain_ok: bool,
}

/// What the first-run form amounts to as filled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Filled {
    /// Nothing in it would protect the database.
    plain: bool,
    ready: bool,
    problem: Option<&'static str>,
}

impl VaultSetupState {
    /// Whether the form may be submitted as it is. A database with no password takes the user's
    /// own word for it, so this is the one place that word is asked for.
    fn filled(&self, busy: bool) -> Filled {
        let has_key_file = !self.key_file.trim().is_empty();
        let plain = self.password.is_empty() && self.confirm.is_empty() && !has_key_file;
        let differs = !self.joining && !self.confirm.is_empty() && self.password != self.confirm;
        let keyless = has_key_file && self.password.is_empty();
        let problem = if differs {
            Some("The two passwords differ")
        } else if keyless {
            Some("A key file needs a password as well")
        } else {
            None
        };
        let complete = if plain {
            self.plain_ok
        } else {
            !self.password.is_empty() && (self.joining || !self.confirm.is_empty())
        };
        let target = !self.joining || self.sync.kind != SyncKind::None;
        Filled { plain, ready: !busy && complete && target && problem.is_none(), problem }
    }
}

/// The warning shown for as long as the form would make a database nobody needs a password for.
const PLAIN_WARNING: &str = "Without a master password, saved passwords and keys are stored unencrypted \
     on disk and in any sync target. Anyone who can read the file can read them.";
/// How much taller the form is while the warning shows, so that the fields above it stay put.
const WARNING_ROOM: f32 = 96.;

/// The warning, with the box that has to be ticked to go on.
fn plain_warning(ui: &mut Ui, salt: Id, width: f32, joining: bool, agreed: &mut bool) {
    const PADDING: f32 = 12.;
    const INDENT: f32 = PADDING + 22.;
    const ROW: f32 = 20.;
    const GAP: f32 = 8.;
    let p = palette(ui);
    let wrap = width - INDENT - PADDING;
    let galley = ui.painter().layout(PLAIN_WARNING.to_owned(), fonts::regular(TEXT_SM), p.foreground, wrap);
    let height = PADDING + galley.size().y + GAP + ROW + PADDING;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter();
    painter.rect_stroke(rect, CornerRadius::same(RADIUS), Stroke::new(1., p.border), StrokeKind::Inside);
    icon(painter, pos2(rect.left() + PADDING + 7., rect.top() + PADDING + 7.), icons::TRIANGLE_ALERT, 14., p.warning);
    painter.galley(rect.min + vec2(INDENT, PADDING), galley, p.foreground);
    let line = Rect::from_min_size(pos2(rect.left() + INDENT, rect.bottom() - PADDING - ROW), vec2(wrap, ROW));
    let label = if joining { "I understand, join it without encryption" } else { "I understand, create it without encryption" };
    checkbox(ui, line, salt, agreed, label);
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

    let height = f32::min(if state.joining { 560. } else { 360. } + WARNING_ROOM, rect.height() - 16.);
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
        let hint = if state.joining { "Master password, if the vault has one" } else { "Master password, optional" };
        let input = text_input(ui, salt.with("password"), &mut state.password, InputOptions {
            hint,
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

    let filled = state.filled(view.busy);
    if filled.plain {
        ui.add_enabled_ui(!view.busy, |ui| {
            plain_warning(ui, salt.with("plain-ok"), TALL_WIDTH, state.joining, &mut state.plain_ok);
        });
    } else {
        // Agreeing was to the form as it was then.
        state.plain_ok = false;
    }

    let error = view.error.or(filled.problem);
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

    // After the box, so that ticking it counts in this pass.
    let ready = state.filled(view.busy).ready;
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
    let vaults = button_full(ui, None, "Other vaults", ButtonKind::Ghost, !view.busy, Some(TALL_WIDTH));
    if vaults.clicked() {
        return Some(UiAction::VaultMenu(vaults.rect.left_bottom() + vec2(0., 4.)));
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
        let plain_ok = mem::take(&mut state.plain_ok);
        let password = Sensitive::new(mem::take(&mut state.password));
        let key_file = state.key_file.clone();
        return Some(if state.joining {
            UiAction::VaultJoin { password, key_file, sync: Box::new(mem::take(&mut state.sync)), plain_ok }
        } else {
            UiAction::VaultCreate { password, key_file, plain_ok }
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(password: &str, confirm: &str, key_file: &str) -> VaultSetupState {
        VaultSetupState {
            password: password.to_owned(),
            confirm: confirm.to_owned(),
            key_file: key_file.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_form_warns_and_waits_for_the_box() {
        let mut state = form("", "", "");
        let filled = state.filled(false);
        assert!(filled.plain);
        assert!(!filled.ready, "nothing is created before the user has agreed");

        state.plain_ok = true;
        assert!(state.filled(false).ready);
        assert!(!state.filled(true).ready, "not twice");
    }

    #[test]
    fn a_password_needs_no_warning_and_goes_as_before() {
        let state = form("secret", "secret", "");
        assert_eq!(state.filled(false), Filled { plain: false, ready: true, problem: None });

        let half = form("secret", "", "");
        assert_eq!(half.filled(false), Filled { plain: false, ready: false, problem: None });

        let wrong = form("secret", "other", "");
        assert!(!wrong.filled(false).ready);
        assert_eq!(wrong.filled(false).problem, Some("The two passwords differ"));

        let with_key = form("secret", "secret", "/k");
        assert_eq!(with_key.filled(false), Filled { plain: false, ready: true, problem: None });
    }

    #[test]
    fn a_key_file_alone_is_not_a_database_without_protection() {
        let mut state = form("", "", "/k");
        let filled = state.filled(false);
        assert!(!filled.plain);
        assert!(!filled.ready);
        assert_eq!(filled.problem, Some("A key file needs a password as well"));
        state.plain_ok = true;
        assert!(!state.filled(false).ready);
    }

    #[test]
    fn joining_without_a_password_also_takes_the_box_and_a_target() {
        let mut state = VaultSetupState { joining: true, ..Default::default() };
        assert!(state.filled(false).plain);
        state.plain_ok = true;
        assert!(!state.filled(false).ready, "there is nowhere to join from yet");
        state.sync.kind = SyncKind::Local;
        assert!(state.filled(false).ready);

        state.plain_ok = false;
        assert!(!state.filled(false).ready);
        state.password = "secret".to_owned();
        assert_eq!(state.filled(false), Filled { plain: false, ready: true, problem: None });
    }
}
