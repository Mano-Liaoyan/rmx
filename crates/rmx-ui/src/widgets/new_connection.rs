//! The two-step new-connection dialog: choose a protocol, then fill in a sectioned form.

use std::fmt;

use egui::{
    Align, Align2, Context, CornerRadius, Id, Layout, Rect, Sense, Stroke, StrokeKind, Ui, pos2,
    vec2,
};

use crate::action::{NodeId, Protocol, UiAction};
use crate::draw::{hline, icon, palette};
use crate::theme::{CONTROL_HEIGHT, RADIUS, TEXT_LG, TEXT_SM};
use crate::{float, fonts, icons};

use super::controls::{
    ButtonKind, InputOptions, button, button_full, icon_button, segmented, text_input,
};
use super::rdp_advanced::{RdpAdvanced, rdp_advanced_page};

const WIDTH: f32 = 540.;
const LABEL: f32 = 96.;
const TILE_H: f32 = 64.;
const TILES_PER_ROW: usize = 4;
const TILE_GAP: f32 = 8.;
const PAD: i8 = 16;

/// How a connection authenticates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthKind {
    #[default]
    Password,
    Key,
    Agent,
}

/// How a remote desktop is shown in its pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScaleChoice {
    /// The whole desktop, as large as the pane allows.
    #[default]
    Fit,
    /// One remote pixel to one window pixel, scrolling what does not fit.
    Native,
}

/// How an RDP server may authenticate the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RdpSecurity {
    /// Offer NLA and TLS, and let the server choose.
    #[default]
    Negotiate,
    Nla,
    Tls,
}

/// How much of an RDP or RustDesk picture to trade for bandwidth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RdpQuality {
    /// Full color and every desktop effect.
    Best,
    #[default]
    Balanced,
    /// Fewer colors and no desktop effects, for a slow link.
    Fast,
    /// RDP: colors, link and effects as set on the advanced page. RustDesk takes it as
    /// balanced.
    Custom,
}

/// How many colors a VNC session asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VncColors {
    #[default]
    Full,
    Bits16,
    Bits8,
}

/// What of a terminal session is written to a log file as it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionLogging {
    #[default]
    Off,
    /// What the session shows.
    Output,
    /// What it shows, and what is typed into it, passwords included.
    Everything,
}

/// A connection as entered in the dialog.
#[derive(Clone, PartialEq)]
pub struct ConnectionDraft {
    /// The connection being changed; none makes a new one.
    pub editing: Option<NodeId>,
    pub protocol: Protocol,
    pub name: String,
    pub group: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub auth: AuthKind,
    pub password: String,
    pub key_path: String,
    pub domain: String,
    pub device: String,
    pub baud: String,
    /// Serial: indices into the choices the form offers: 5 to 8 data bits.
    pub data_bits: usize,
    /// Serial: none, odd or even.
    pub parity: usize,
    /// Serial: one or two.
    pub stop_bits: usize,
    /// Serial: none, software or hardware.
    pub flow_control: usize,
    /// Name of the saved connection to jump through, if any.
    pub jump: String,
    /// Port forwards, as `L8080:db:5432, D1080, R9000:localhost:80`.
    pub forwards: String,
    /// Remote desktops: how the picture fits the pane.
    pub scale: ScaleChoice,
    /// Remote desktops: whether the desktop takes the size of the pane.
    pub follow_size: bool,
    /// RDP: how the server may authenticate.
    pub rdp_security: RdpSecurity,
    /// RDP and RustDesk: how much picture to trade for bandwidth.
    pub rdp_quality: RdpQuality,
    /// RDP: what the advanced page sets.
    pub rdp: RdpAdvanced,
    /// RustDesk: the rendezvous server, `host[:port]`; empty for RustDesk's public one.
    pub rustdesk_server: String,
    /// RustDesk: the rendezvous server's public key; empty for the public server's.
    pub rustdesk_key: String,
    /// VNC: how many colors to ask for.
    pub vnc_colors: VncColors,
    /// FTP: whether to upgrade with `AUTH TLS`.
    pub ftps: bool,
    /// Terminals: what of each session to write to a log file.
    pub log: SessionLogging,
    /// Whether to use the login a folder above its group hands down, when one does, rather than
    /// its own. Saving with this set clears its own username, password and key.
    pub inherit: bool,
}

impl fmt::Debug for ConnectionDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionDraft")
            .field("protocol", &self.protocol)
            .field("name", &self.name)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl ConnectionDraft {
    /// An empty form for `protocol`.
    pub fn new(protocol: Protocol) -> Self {
        Self {
            editing: None,
            protocol,
            name: String::new(),
            group: String::new(),
            host: String::new(),
            port: protocol.default_port().map(|p| p.to_string()).unwrap_or_default(),
            username: String::new(),
            auth: AuthKind::default(),
            password: String::new(),
            key_path: String::new(),
            domain: String::new(),
            device: "/dev/ttyUSB0".to_owned(),
            baud: "115200".to_owned(),
            data_bits: 3,
            parity: 0,
            stop_bits: 0,
            flow_control: 0,
            jump: String::new(),
            forwards: String::new(),
            scale: ScaleChoice::default(),
            follow_size: true,
            rdp_security: RdpSecurity::default(),
            rdp_quality: RdpQuality::default(),
            rdp: RdpAdvanced::default(),
            rustdesk_server: String::new(),
            rustdesk_key: String::new(),
            vnc_colors: VncColors::default(),
            ftps: false,
            log: SessionLogging::Off,
            inherit: true,
        }
    }

    /// Whether there is enough to attempt a connection.
    pub fn is_complete(&self) -> bool {
        match self.protocol {
            Protocol::Serial => !self.device.is_empty() && self.baud.parse::<u32>().is_ok(),
            _ => !self.host.is_empty() && self.port.parse::<u16>().is_ok(),
        }
    }
}

/// Whether a RustDesk host is an address to connect to directly, rather than a peer's ID.
///
/// IDs are digits and letters, so a dot or a colon means a host name or an IP address.
pub fn is_rustdesk_address(host: &str) -> bool {
    let host = host.trim();
    host.contains('.') || host.contains(':')
}

/// A login a folder hands down to the connections in it and below, as the form shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InheritedLogin {
    /// The folder, as the form's Group field names it: `Production/Web`.
    pub group: String,
    /// The username it gives; empty for none.
    pub username: String,
    /// Whether it gives a password.
    pub password: bool,
    /// Whether it gives a key.
    pub key: bool,
}

impl InheritedLogin {
    /// The login a connection in `group` inherits: that of the nearest folder, `group` itself
    /// included, that has one.
    pub fn nearest<'a>(logins: &'a [Self], group: &str) -> Option<&'a Self> {
        logins
            .iter()
            .filter_map(|login| depth_within(&login.group, group).map(|depth| (depth, login)))
            .max_by_key(|&(depth, _)| depth)
            .map(|(_, login)| login)
    }

    /// The folder's own name: the last part of its group.
    pub fn folder(&self) -> &str {
        segments(&self.group).last().unwrap_or_default()
    }

    /// What the form says about it.
    fn describe(&self) -> String {
        let mut text = format!("Inherited from folder {}", self.folder());
        let mut parts = Vec::new();
        if !self.username.is_empty() {
            parts.push(self.username.as_str());
        }
        if self.password {
            parts.push("with a saved password");
        }
        if self.key {
            parts.push("with a stored key");
        }
        if !parts.is_empty() {
            text += ": ";
            text += &parts.join(", ");
        }
        text
    }
}

/// The parts of a group path, as the vault splits it.
fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').map(str::trim).filter(|part| !part.is_empty())
}

/// How many folders deep `folder` is, if `group` is it or inside it.
fn depth_within(folder: &str, group: &str) -> Option<usize> {
    let mut inside = segments(group);
    let mut depth = 0;
    for part in segments(folder) {
        if inside.next() != Some(part) {
            return None;
        }
        depth += 1;
    }
    (depth > 0).then_some(depth)
}

/// A serial port this machine has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialPortChoice {
    pub path: String,
    /// What it is, when it says.
    pub label: String,
}

/// The dialog's state.
#[derive(Clone, Debug, Default)]
pub struct NewConnectionState {
    open: bool,
    /// `None` on the first step.
    draft: Option<Box<ConnectionDraft>>,
    /// The serial ports found, once they have been looked for.
    ports: Option<Vec<SerialPortChoice>>,
    /// Whether they have been asked for since the form opened.
    ports_asked: bool,
    /// Showing the RDP form's advanced page.
    advanced: bool,
}

impl NewConnectionState {
    /// Show the dialog at its first step.
    pub fn open(&mut self) {
        *self = Self { open: true, ..Self::default() };
    }

    /// Show the dialog at the form for `protocol`.
    pub fn open_form(&mut self, protocol: Protocol) {
        *self = Self {
            open: true,
            draft: Some(Box::new(ConnectionDraft::new(protocol))),
            ..Self::default()
        };
    }

    /// Show the dialog at the form, filled in with `draft`.
    pub fn open_draft(&mut self, draft: ConnectionDraft) {
        *self = Self { open: true, draft: Some(Box::new(draft)), ..Self::default() };
    }

    /// Whether it is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The serial ports this machine turned out to have.
    pub fn set_ports(&mut self, ports: Vec<SerialPortChoice>) {
        self.ports = Some(ports);
    }

    /// Hide it and forget what was entered.
    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// The form's current contents, if past the first step.
    pub fn draft_mut(&mut self) -> Option<&mut ConnectionDraft> {
        self.draft.as_deref_mut()
    }
}

/// The modal dialog. `Some` is returned as it closes or when the form is submitted.
///
/// `logins` are what folders hand down, for the form to say what a connection would inherit.
pub fn new_connection_dialog(
    ctx: &Context,
    state: &mut NewConnectionState,
    logins: &[InheritedLogin],
) -> Option<UiAction> {
    if !state.open {
        return None;
    }
    let modal = float::modal(ctx, Id::unique("new-connection"), WIDTH, |ui| {
        new_connection_body(ui, state, logins)
    });
    let action = modal.inner.or(modal.dismissed.then_some(UiAction::ConnectionCancel));
    if action.is_some() {
        state.close();
        ctx.request_repaint();
        return action;
    }
    let serial = state.draft.as_ref().is_some_and(|draft| draft.protocol == Protocol::Serial);
    if serial && !state.ports_asked {
        state.ports_asked = true;
        return Some(UiAction::ListSerialPorts);
    }
    None
}

/// The dialog's content, for hosting in any surface.
pub fn new_connection_body(
    ui: &mut Ui,
    state: &mut NewConnectionState,
    logins: &[InheritedLogin],
) -> Option<UiAction> {
    ui.spacing_mut().item_spacing.y = 0.;
    match &mut state.draft {
        None => tiles_step(ui, state),
        Some(_) => form_step(ui, state, logins),
    }
}

fn header(ui: &mut Ui, title: &str, back: Option<&mut bool>) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.), Sense::hover());
    let mut x = rect.left() + f32::from(PAD);
    if let Some(back) = back {
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(Rect::from_min_size(pos2(x - 4., rect.top() + 8.), vec2(28., 28.))),
        );
        if icon_button(&mut child, icons::ARROW_LEFT, 28., 14.).clicked() {
            *back = true;
        }
        x += 30.;
    }
    let painter = ui.painter();
    painter.text(
        pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        fonts::medium(TEXT_LG),
        p.foreground,
    );
    hline(painter, rect.left(), rect.right(), rect.bottom() - 1., p.border);
}

fn tiles_step(ui: &mut Ui, state: &mut NewConnectionState) -> Option<UiAction> {
    let p = palette(ui);
    let mut action = None;
    header(ui, "New connection", None);
    let mut picked = None;
    egui::Frame::new().inner_margin(PAD).show(ui, |ui| {
        let row = |i: usize| i / TILES_PER_ROW;
        let width = ui.available_width();
        let tile_w = (width - TILE_GAP * (TILES_PER_ROW - 1) as f32) / TILES_PER_ROW as f32;
        let rows = row(Protocol::ALL.len() - 1) + 1;
        let height = rows as f32 * TILE_H + (rows - 1) as f32 * TILE_GAP;
        let (area, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        for (i, protocol) in Protocol::ALL.into_iter().enumerate() {
            let cell = Rect::from_min_size(
                area.min
                    + vec2(
                        (i % TILES_PER_ROW) as f32 * (tile_w + TILE_GAP),
                        row(i) as f32 * (TILE_H + TILE_GAP),
                    ),
                vec2(tile_w, TILE_H),
            );
            let response = ui.interact(cell, Id::unique(("protocol-tile", i)), Sense::click());
            let fill = if response.hovered() { p.hover } else { p.background };
            let painter = ui.painter();
            painter.rect(
                cell,
                CornerRadius::same(RADIUS),
                fill,
                Stroke::new(1., p.border),
                StrokeKind::Inside,
            );
            icon(
                painter,
                pos2(cell.left() + 21., cell.top() + 21.),
                protocol.icon(),
                16.,
                p.foreground,
            );
            painter.text(
                pos2(cell.left() + 12., cell.top() + 46.),
                Align2::LEFT_CENTER,
                protocol.label(),
                fonts::medium(TEXT_SM),
                p.foreground,
            );
            if response.clicked() {
                picked = Some(protocol);
            }
        }
    });
    footer_rule(ui);
    egui::Frame::new().inner_margin(egui::Margin::symmetric(PAD, 12)).show(ui, |ui| {
        let size = vec2(ui.available_width(), CONTROL_HEIGHT);
        ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
            if button(ui, "Cancel", ButtonKind::Secondary).clicked() {
                action = Some(UiAction::ConnectionCancel);
            }
        });
    });
    if let Some(protocol) = picked {
        state.open_form(protocol);
        ui.ctx().request_repaint();
    }
    action
}

fn footer_rule(ui: &mut Ui) {
    let p = palette(ui);
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), Sense::hover());
    hline(ui.painter(), line.left(), line.right(), line.top(), p.border);
}

/// A gap and a hairline between groups of fields, which say what they are themselves.
pub(super) fn section(ui: &mut Ui) {
    ui.add_space(2.);
    super::panels::group_rule(ui);
    ui.add_space(2.);
}

/// A line of warning, under what it is about.
fn warning(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.), Sense::hover());
    let painter = ui.painter();
    icon(painter, pos2(rect.left() + 7., rect.center().y), icons::TRIANGLE_ALERT, 14., p.warning);
    painter.text(
        pos2(rect.left() + 22., rect.center().y),
        Align2::LEFT_CENTER,
        text,
        fonts::regular(TEXT_SM),
        p.foreground,
    );
}

/// Where the login comes from, when a folder above the group hands one down: a choice between
/// that one and its own. Returns whether to show the fields for its own.
fn login_source(ui: &mut Ui, draft: &mut ConnectionDraft, logins: &[InheritedLogin]) -> bool {
    let Some(found) = InheritedLogin::nearest(logins, &draft.group) else { return true };
    row(ui, "Login", |ui| {
        let mut selected = usize::from(!draft.inherit);
        let options = ["Inherited", "Its own"];
        if segmented(ui, Id::unique("cd-login"), &options, &mut selected).changed() {
            draft.inherit = selected == 0;
        }
    });
    if draft.inherit {
        let p = palette(ui);
        row(ui, "", |ui| {
            ui.label(egui::RichText::new(found.describe()).size(TEXT_SM).color(p.muted_fg));
        });
    }
    !draft.inherit
}

/// The ports found on this machine, as buttons filling in `device`. Returns whether to look again.
fn port_choices(ui: &mut Ui, ports: Option<&[SerialPortChoice]>, device: &mut String) -> bool {
    let p = palette(ui);
    let mut again = false;
    row(ui, "", |ui| {
        let shown = ports.unwrap_or_default().iter().take(3);
        for port in shown {
            let label = if port.label.is_empty() { &port.path } else { &port.label };
            if button_full(ui, None, label, ButtonKind::Secondary, true, None).clicked() {
                device.clone_from(&port.path);
            }
        }
        match ports {
            None => (),
            Some([]) => {
                ui.label(
                    egui::RichText::new("No serial ports found").size(TEXT_SM).color(p.muted_fg),
                );
            },
            Some(_) => (),
        }
        if icon_button(ui, icons::FOLDER_SYNC, CONTROL_HEIGHT, 14.)
            .on_hover_text("Look again")
            .clicked()
        {
            again = true;
        }
    });
    again
}

/// A labeled row: the label column, then whatever `add` puts in the rest.
pub(super) fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            let (cell, _) =
                ui.allocate_exact_size(vec2(LABEL - 8., CONTROL_HEIGHT), Sense::hover());
            ui.painter().text(
                cell.left_center(),
                Align2::LEFT_CENTER,
                label,
                fonts::regular(TEXT_SM),
                p.foreground,
            );
            add(ui);
        },
    );
}

fn field(
    ui: &mut Ui,
    salt: &'static str,
    label: &str,
    value: &mut String,
    hint: &str,
    password: bool,
) {
    row(ui, label, |ui| {
        text_input(
            ui,
            Id::unique(salt),
            value,
            InputOptions { hint, password, ..Default::default() },
        );
    });
}

/// Whether a terminal's sessions are logged, and what that means for the secrets in them.
fn logging_options(ui: &mut Ui, draft: &mut ConnectionDraft) {
    section(ui);
    row(ui, "Log to file", |ui| {
        let mut selected = draft.log as usize;
        let options = ["Off", "Output", "Output and typing"];
        if segmented(ui, Id::unique("cd-log"), &options, &mut selected).changed() {
            draft.log =
                [SessionLogging::Off, SessionLogging::Output, SessionLogging::Everything][selected];
        }
    });
    match draft.log {
        SessionLogging::Off => (),
        SessionLogging::Output => {
            warning(ui, "Logs are plain files: secrets shown on screen end up in them.");
        },
        SessionLogging::Everything => {
            warning(ui, "Logs are plain files: typed passwords end up in them too.");
        },
    }
}

/// How a remote desktop fits its pane: the rows VNC and RDP share.
/// The scale, and whether the remote size follows the pane where the protocol can resize.
fn display_options(ui: &mut Ui, draft: &mut ConnectionDraft, resizable: bool) {
    row(ui, "Scale", |ui| {
        let mut selected = draft.scale as usize;
        let options = ["Fit", "1:1, scrolling"];
        if segmented(ui, Id::unique("cd-scale"), &options, &mut selected).changed() {
            draft.scale = [ScaleChoice::Fit, ScaleChoice::Native][selected];
        }
    });
    if !resizable {
        return;
    }
    row(ui, "Remote size", |ui| {
        let mut selected = usize::from(!draft.follow_size);
        let options = ["Follow the pane", "Keep"];
        if segmented(ui, Id::unique("cd-follow"), &options, &mut selected).changed() {
            draft.follow_size = selected == 0;
        }
    });
}

fn form_step(
    ui: &mut Ui,
    state: &mut NewConnectionState,
    logins: &[InheritedLogin],
) -> Option<UiAction> {
    let mut back = false;
    let mut action = None;
    let draft = state.draft.as_deref_mut()?;
    let protocol = draft.protocol;
    let editing = draft.editing.is_some();
    let verb = if editing { "Edit" } else { "New" };
    let title = format!("{verb} {} connection", protocol.label());
    let advanced = state.advanced && protocol == Protocol::Rdp;
    let mut leave_advanced = false;
    let mut open_advanced = false;
    if advanced {
        header(ui, "Advanced RDP settings", Some(&mut leave_advanced));
    } else {
        let backward = (!editing).then_some(&mut back);
        header(ui, &title, backward);
    }
    let ports = state.ports.as_deref();
    let mut look_again = false;

    if advanced {
        let height = (ui.ctx().content_rect().height() - 200.).clamp(240., 560.);
        egui::ScrollArea::vertical().max_height(height).show(ui, |ui| {
            egui::Frame::new().inner_margin(PAD).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.;
                rdp_advanced_page(ui, draft);
            });
        });
    } else {
        egui::Frame::new().inner_margin(PAD).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.;
            field(ui, "cd-name", "Name", &mut draft.name, "", false);
            field(ui, "cd-group", "Group", &mut draft.group, "", false);
            section(ui);
            if protocol == Protocol::Serial {
                field(ui, "cd-device", "Device", &mut draft.device, "", false);
                look_again = port_choices(ui, ports, &mut draft.device);
                field(ui, "cd-baud", "Baud rate", &mut draft.baud, "", false);
                let mut choice = |label, salt: &'static str, options: &[&str], value: &mut usize| {
                    row(ui, label, |ui| {
                        let mut selected = *value;
                        if segmented(ui, Id::unique(salt), options, &mut selected).changed() {
                            *value = selected;
                        }
                    });
                };
                choice("Data bits", "cd-data", &["5", "6", "7", "8"], &mut draft.data_bits);
                choice("Parity", "cd-parity", &["None", "Odd", "Even"], &mut draft.parity);
                choice("Stop bits", "cd-stop", &["1", "2"], &mut draft.stop_bits);
                choice("Flow control", "cd-flow", &["None", "XON/XOFF", "RTS/CTS"], &mut draft.flow_control);
            } else {
                let (label, hint) = if protocol == Protocol::RustDesk {
                    ("ID or host", "123 456 789, or an address")
                } else {
                    ("Host", "")
                };
                row(ui, label, |ui| {
                    let width = ui.available_width() - 88.;
                    text_input(ui, Id::unique("cd-host"), &mut draft.host, InputOptions {
                        hint,
                        width: Some(width),
                        ..Default::default()
                    });
                    text_input(ui, Id::unique("cd-port"), &mut draft.port, InputOptions {
                        hint: "Port",
                        width: Some(80.),
                        ..Default::default()
                    });
                });
            }

            match protocol {
                Protocol::Ssh | Protocol::Sftp => {
                    field(ui, "cd-jump", "Jump via", &mut draft.jump, "", false);
                    if protocol == Protocol::Ssh {
                        let hint = "L8080:db:5432, D1080, R9000:localhost:80";
                        field(ui, "cd-forwards", "Forwards", &mut draft.forwards, hint, false);
                    }
                    section(ui);
                    if login_source(ui, draft, logins) {
                    field(ui, "cd-user", "Username", &mut draft.username, "", false);
                    row(ui, "Method", |ui| {
                        let mut selected = draft.auth as usize;
                        let response = segmented(
                            ui,
                            Id::unique("cd-auth"),
                            &["Password", "Key file", "Agent"],
                            &mut selected,
                        );
                        if response.changed() {
                            draft.auth = [AuthKind::Password, AuthKind::Key, AuthKind::Agent][selected];
                        }
                    });
                    match draft.auth {
                        AuthKind::Password => {
                            field(
                                ui,
                                "cd-password",
                                "Password",
                                &mut draft.password,
                                "",
                                true,
                            );
                        },
                        AuthKind::Key => {
                            field(
                                ui,
                                "cd-key",
                                "Key file",
                                &mut draft.key_path,
                                "",
                                false,
                            );
                        },
                        AuthKind::Agent => {},
                    }
                    }
                    if protocol == Protocol::Ssh {
                        logging_options(ui, draft);
                    }
                },
                Protocol::Telnet => {
                    field(ui, "cd-jump", "Jump via", &mut draft.jump, "", false);
                    warning(ui, "Telnet is not encrypted: anyone on the network can read what is typed.");
                    logging_options(ui, draft);
                },
                Protocol::Ftp => {
                    field(ui, "cd-jump", "Jump via", &mut draft.jump, "", false);
                    row(ui, "Encryption", |ui| {
                        let mut selected = usize::from(draft.ftps);
                        let options = ["None", "FTPS (AUTH TLS)"];
                        if segmented(ui, Id::unique("cd-ftps"), &options, &mut selected).changed() {
                            draft.ftps = selected == 1;
                        }
                    });
                    section(ui);
                    if login_source(ui, draft, logins) {
                        field(ui, "cd-user", "Username", &mut draft.username, "", false);
                        field(ui, "cd-password", "Password", &mut draft.password, "", true);
                    }
                },
                Protocol::Rdp => {
                    field(ui, "cd-jump", "Jump via", &mut draft.jump, "", false);
                    display_options(ui, draft, true);
                    row(ui, "Security", |ui| {
                        let mut selected = draft.rdp_security as usize;
                        let options = ["Negotiate", "NLA", "TLS"];
                        if segmented(ui, Id::unique("cd-security"), &options, &mut selected).changed() {
                            draft.rdp_security =
                                [RdpSecurity::Negotiate, RdpSecurity::Nla, RdpSecurity::Tls][selected];
                        }
                    });
                    row(ui, "Quality", |ui| {
                        let mut selected = draft.rdp_quality as usize;
                        let options = ["Best", "Balanced", "Fast", "Custom"];
                        if segmented(ui, Id::unique("cd-quality"), &options, &mut selected).changed() {
                            // Custom starts from what the preset was asking for.
                            if let Some(preset) = draft.rdp_quality.preset() {
                                draft.rdp.picture = preset;
                            }
                            draft.rdp_quality = [
                                RdpQuality::Best,
                                RdpQuality::Balanced,
                                RdpQuality::Fast,
                                RdpQuality::Custom,
                            ][selected];
                        }
                    });
                    row(ui, "", |ui| {
                        let label = "Advanced settings";
                        let arrow = Some(icons::CHEVRON_RIGHT);
                        if button_full(ui, arrow, label, ButtonKind::Secondary, true, None).clicked() {
                            open_advanced = true;
                        }
                    });
                    section(ui);
                    let own = login_source(ui, draft, logins);
                    if own {
                        field(ui, "cd-user", "Username", &mut draft.username, "", false);
                    }
                    field(ui, "cd-domain", "Domain", &mut draft.domain, "", false);
                    if own {
                        field(ui, "cd-password", "Password", &mut draft.password, "", true);
                    }
                },
                Protocol::RustDesk => {
                    field(ui, "cd-jump", "Jump via", &mut draft.jump, "", false);
                    // A RustDesk peer's resolution is its own; see rmx-rustdesk's session.
                    display_options(ui, draft, false);
                    row(ui, "Quality", |ui| {
                        let mut selected = draft.rdp_quality as usize;
                        let options = ["Best", "Balanced", "Fast"];
                        if segmented(ui, Id::unique("cd-quality"), &options, &mut selected).changed() {
                            draft.rdp_quality =
                                [RdpQuality::Best, RdpQuality::Balanced, RdpQuality::Fast][selected];
                        }
                    });
                    section(ui);
                    let hint = "rs-ny.rustdesk.com";
                    field(ui, "cd-rd-server", "Server", &mut draft.rustdesk_server, hint, false);
                    let hint = "The public server's key";
                    field(ui, "cd-rd-key", "Server key", &mut draft.rustdesk_key, hint, false);
                    if is_rustdesk_address(&draft.host) {
                        warning(ui, "By address RustDesk serves the session unencrypted, and the peer's identity is not checked. Use it over a network you trust, such as a VPN.");
                    }
                    section(ui);
                    if login_source(ui, draft, logins) {
                        let hint = "Empty to ask the person at the peer";
                        field(ui, "cd-password", "Password", &mut draft.password, hint, true);
                    }
                },
                Protocol::Vnc => {
                    field(ui, "cd-jump", "Jump via", &mut draft.jump, "", false);
                    display_options(ui, draft, true);
                    row(ui, "Colors", |ui| {
                        let mut selected = draft.vnc_colors as usize;
                        let options = ["Full", "16-bit", "8-bit"];
                        if segmented(ui, Id::unique("cd-colors"), &options, &mut selected).changed() {
                            draft.vnc_colors =
                                [VncColors::Full, VncColors::Bits16, VncColors::Bits8][selected];
                        }
                    });
                    section(ui);
                    if login_source(ui, draft, logins) {
                        field(ui, "cd-password", "Password", &mut draft.password, "", true);
                    }
                },
                Protocol::Serial => logging_options(ui, draft),
            }
        });
    }

    footer_rule(ui);
    let complete = draft.is_complete();
    let mut submit = false;
    let mut cancel = false;
    egui::Frame::new().inner_margin(egui::Margin::symmetric(PAD, 12)).show(ui, |ui| {
        let size = vec2(ui.available_width(), CONTROL_HEIGHT);
        ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
            let label = if editing { "Save" } else { "Save and connect" };
            submit = button_full(ui, None, label, ButtonKind::Primary, complete, None).clicked();
            cancel = button(ui, "Cancel", ButtonKind::Secondary).clicked();
        });
    });

    if look_again {
        state.ports_asked = false;
        ui.ctx().request_repaint();
    }
    if open_advanced || leave_advanced {
        state.advanced = open_advanced;
        ui.ctx().request_repaint();
    }
    if submit {
        action = state.draft.take().map(UiAction::ConnectionSubmit);
    } else if cancel {
        action = Some(UiAction::ConnectionCancel);
    } else if back {
        state.draft = None;
        ui.ctx().request_repaint();
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(group: &str) -> InheritedLogin {
        InheritedLogin { group: group.to_owned(), username: group.to_owned(), ..Default::default() }
    }

    #[test]
    fn the_nearest_folder_above_the_group_hands_down_its_login() {
        let logins = [login("Production"), login("Production/Web"), login("Staging")];
        let found = |group| InheritedLogin::nearest(&logins, group).map(|l| l.group.as_str());
        assert_eq!(found("Production/Web/eu"), Some("Production/Web"));
        assert_eq!(found(" Production / Web "), Some("Production/Web"));
        assert_eq!(found("Production/Db"), Some("Production"));
        assert_eq!(found("Production"), Some("Production"));
        assert_eq!(found("ProductionX"), None);
        assert_eq!(found(""), None);
        assert_eq!(found("Other/Production"), None);
    }

    #[test]
    fn it_says_where_the_login_comes_from() {
        let full = InheritedLogin {
            group: "Production/Web".to_owned(),
            username: "admin".to_owned(),
            password: true,
            key: true,
        };
        assert_eq!(full.folder(), "Web");
        assert_eq!(
            full.describe(),
            "Inherited from folder Web: admin, with a saved password, with a stored key"
        );
        let bare = InheritedLogin { group: "a".to_owned(), ..Default::default() };
        assert_eq!(bare.describe(), "Inherited from folder a");
    }
}
