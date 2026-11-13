//! The second page of an RDP connection's form: everything past the basics.

use egui::{Align2, Id, Rect, Sense, Ui, vec2};

use crate::draw::palette;
use crate::fonts;
use crate::theme::{CONTROL_HEIGHT, TEXT_SM};

use super::controls::{InputOptions, checkbox, segmented, text_input};
use super::new_connection::{ConnectionDraft, RdpQuality, row, section};

/// Bits per pixel of the older bitmap updates. The graphics pipeline always carries 32.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RdpColorDepth {
    #[default]
    Bits32,
    Bits24,
    Bits16,
    Bits15,
}

impl RdpColorDepth {
    pub const ALL: [Self; 4] = [Self::Bits32, Self::Bits24, Self::Bits16, Self::Bits15];

    pub fn bits(self) -> u32 {
        match self {
            Self::Bits32 => 32,
            Self::Bits24 => 24,
            Self::Bits16 => 16,
            Self::Bits15 => 15,
        }
    }
}

/// The link a session claims to have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RdpConnection {
    Modem,
    BroadbandLow,
    Satellite,
    BroadbandHigh,
    Wan,
    #[default]
    Lan,
    /// Let the server measure it.
    Autodetect,
}

impl RdpConnection {
    pub const ALL: [Self; 7] = [
        Self::Modem,
        Self::BroadbandLow,
        Self::Satellite,
        Self::BroadbandHigh,
        Self::Wan,
        Self::Lan,
        Self::Autodetect,
    ];
}

/// The desktop effects a session lets the server show, each `true` when allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RdpEffects {
    pub wallpaper: bool,
    pub full_window_drag: bool,
    pub menu_animations: bool,
    pub themes: bool,
    pub cursor_shadow: bool,
    pub font_smoothing: bool,
    pub desktop_composition: bool,
}

impl RdpEffects {
    pub const ALL_ON: Self = Self {
        wallpaper: true,
        full_window_drag: true,
        menu_animations: true,
        themes: true,
        cursor_shadow: true,
        font_smoothing: true,
        desktop_composition: true,
    };
    pub const ALL_OFF: Self = Self {
        wallpaper: false,
        full_window_drag: false,
        menu_animations: false,
        themes: false,
        cursor_shadow: false,
        font_smoothing: false,
        desktop_composition: false,
    };

    /// Each effect with its label, to go through them in order.
    fn entries(&mut self) -> [(&mut bool, &'static str); 7] {
        [
            (&mut self.wallpaper, "Wallpaper"),
            (&mut self.themes, "Visual styles"),
            (&mut self.font_smoothing, "Font smoothing"),
            (&mut self.desktop_composition, "Desktop composition"),
            (&mut self.full_window_drag, "Contents while dragging"),
            (&mut self.menu_animations, "Menu animations"),
            (&mut self.cursor_shadow, "Cursor shadow"),
        ]
    }
}

impl Default for RdpEffects {
    fn default() -> Self {
        RdpQuality::Balanced.preset().map_or(Self::ALL_ON, |preset| preset.effects)
    }
}

/// What a quality asks for: the color depth, link and effects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RdpPicture {
    pub color_depth: RdpColorDepth,
    pub connection: RdpConnection,
    pub effects: RdpEffects,
}

impl RdpQuality {
    /// What the preset asks for; nothing for [`RdpQuality::Custom`], which has its own.
    pub fn preset(self) -> Option<RdpPicture> {
        let lan = |effects| RdpPicture {
            color_depth: RdpColorDepth::Bits32,
            connection: RdpConnection::Lan,
            effects,
        };
        Some(match self {
            Self::Best => lan(RdpEffects::ALL_ON),
            Self::Balanced => lan(RdpEffects {
                full_window_drag: false,
                menu_animations: false,
                desktop_composition: false,
                ..RdpEffects::ALL_ON
            }),
            Self::Fast => RdpPicture {
                color_depth: RdpColorDepth::Bits16,
                connection: RdpConnection::BroadbandLow,
                effects: RdpEffects::ALL_OFF,
            },
            Self::Custom => return None,
        })
    }
}

/// Bulk compression of what the server sends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RdpCompression {
    #[default]
    None,
    K8,
    K64,
    Rdp6,
    Rdp61,
}

impl RdpCompression {
    pub const ALL: [Self; 5] = [Self::None, Self::K8, Self::K64, Self::Rdp6, Self::Rdp61];
}

/// How large the remote desktop draws its interface, in percent.
pub const RDP_SCALES: [u32; 5] = [100, 125, 150, 175, 200];

/// An RDP connection's settings past the basics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RdpAdvanced {
    /// The picture asked for when the quality is [`RdpQuality::Custom`].
    pub picture: RdpPicture,
    pub graphics_pipeline: bool,
    pub remotefx: bool,
    pub lossy: bool,
    pub compression: RdpCompression,
    /// One of [`RDP_SCALES`].
    pub scale: u32,
    /// Show the remote pointer as the local cursor, rather than in the picture.
    pub local_cursor: bool,
    pub clipboard: bool,
    /// A Windows keyboard layout identifier such as `0x0407`; empty for US English.
    pub keyboard_layout: String,
    /// The name the server is told this computer has; empty for the default.
    pub client_name: String,
    pub start_program: String,
    pub work_dir: String,
}

impl Default for RdpAdvanced {
    fn default() -> Self {
        Self {
            picture: RdpPicture::default(),
            graphics_pipeline: true,
            remotefx: true,
            lossy: true,
            compression: RdpCompression::None,
            scale: 100,
            local_cursor: true,
            clipboard: true,
            keyboard_layout: String::new(),
            client_name: String::new(),
            start_program: String::new(),
            work_dir: String::new(),
        }
    }
}

/// The keyboard layout identifier `text` names, if it is one: hexadecimal, with or without `0x`.
pub fn parse_keyboard_layout(text: &str) -> Option<u32> {
    let text = text.trim();
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    u32::from_str_radix(digits, 16).ok().filter(|&layout| layout != 0)
}

/// A small heading over a group of rows.
fn heading(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.), Sense::hover());
    ui.painter().text(
        rect.left_center(),
        Align2::LEFT_CENTER,
        text,
        fonts::medium(TEXT_SM),
        p.muted_fg,
    );
}

/// A choice of `options` in a labeled row, `Some` with the new index once it changes.
fn choice(
    ui: &mut Ui,
    label: &str,
    salt: &'static str,
    options: &[&str],
    at: usize,
) -> Option<usize> {
    let mut picked = None;
    row(ui, label, |ui| {
        let mut selected = at;
        if segmented(ui, Id::unique(salt), options, &mut selected).changed() {
            picked = Some(selected);
        }
    });
    picked
}

/// Tick boxes, two to a row, under the label column.
fn ticks(ui: &mut Ui, salt: &'static str, boxes: &mut [(&mut bool, &'static str)]) -> bool {
    let mut changed = false;
    for (pair, chunk) in boxes.chunks_mut(2).enumerate() {
        row(ui, "", |ui| {
            let width = ui.available_width() / 2.;
            let origin = ui.cursor().min;
            for (i, (value, label)) in chunk.iter_mut().enumerate() {
                let rect = Rect::from_min_size(
                    origin + vec2(i as f32 * width, 0.),
                    vec2(width - 8., CONTROL_HEIGHT),
                );
                let id = Id::unique((salt, pair * 2 + i));
                changed |= checkbox(ui, rect, id, value, label).changed();
            }
            ui.allocate_exact_size(vec2(ui.available_width(), CONTROL_HEIGHT), Sense::hover());
        });
    }
    changed
}

fn text_row(ui: &mut Ui, salt: &'static str, label: &str, value: &mut String, hint: &str) {
    row(ui, label, |ui| {
        text_input(ui, Id::unique(salt), value, InputOptions { hint, ..Default::default() });
    });
}

/// The page's rows, changing `draft` as they are used: one function per group.
pub fn rdp_advanced_page(ui: &mut Ui, draft: &mut ConnectionDraft) {
    picture(ui, draft);
    graphics(ui, &mut draft.rdp);
    display(ui, &mut draft.rdp);
    session(ui, &mut draft.rdp);
    devices(ui, &mut draft.rdp);
}

/// The quality, and the color depth, link and effects it asks for.
fn picture(ui: &mut Ui, draft: &mut ConnectionDraft) {
    heading(ui, "Picture");
    let quality = [RdpQuality::Best, RdpQuality::Balanced, RdpQuality::Fast, RdpQuality::Custom];
    let at = quality.iter().position(|&q| q == draft.rdp_quality).unwrap_or(1);
    if let Some(picked) =
        choice(ui, "Quality", "ra-quality", &["Best", "Balanced", "Fast", "Custom"], at)
    {
        // Custom starts from what the preset was asking for.
        if let Some(preset) = draft.rdp_quality.preset() {
            draft.rdp.picture = preset;
        }
        draft.rdp_quality = quality[picked];
    }

    // The rows show what the preset asks for; changing one makes the quality custom.
    let mut picture = draft.rdp_quality.preset().unwrap_or(draft.rdp.picture);
    let mut changed = false;
    let depth = RdpColorDepth::ALL.iter().position(|&d| d == picture.color_depth).unwrap_or(0);
    if let Some(picked) =
        choice(ui, "Colors", "ra-depth", &["32-bit", "24-bit", "16-bit", "15-bit"], depth)
    {
        picture.color_depth = RdpColorDepth::ALL[picked];
        changed = true;
    }
    let link = RdpConnection::ALL.iter().position(|&c| c == picture.connection).unwrap_or(5);
    let links = ["Modem", "DSL", "Satellite", "Cable", "WAN", "LAN", "Detect"];
    if let Some(picked) = choice(ui, "Connection", "ra-link", &links, link) {
        picture.connection = RdpConnection::ALL[picked];
        changed = true;
    }
    row(ui, "Effects", |_| {});
    changed |= ticks(ui, "ra-effect", &mut picture.effects.entries());
    if changed {
        draft.rdp.picture = picture;
        draft.rdp_quality = RdpQuality::Custom;
    }
}

/// The graphics pipeline, codecs and compression.
fn graphics(ui: &mut Ui, rdp: &mut RdpAdvanced) {
    section(ui);
    heading(ui, "Graphics");
    ticks(
        ui,
        "ra-graphics",
        &mut [
            (&mut rdp.graphics_pipeline, "Graphics pipeline"),
            (&mut rdp.remotefx, "RemoteFX"),
            (&mut rdp.lossy, "Lossy bitmaps"),
        ],
    );
    let at = RdpCompression::ALL.iter().position(|&c| c == rdp.compression).unwrap_or(0);
    let options = ["None", "8K", "64K", "RDP 6", "RDP 6.1"];
    if let Some(picked) = choice(ui, "Compression", "ra-compression", &options, at) {
        rdp.compression = RdpCompression::ALL[picked];
    }
}

/// The remote scale and where the pointer is drawn.
fn display(ui: &mut Ui, rdp: &mut RdpAdvanced) {
    section(ui);
    heading(ui, "Display");
    let at = RDP_SCALES.iter().position(|&s| s == rdp.scale).unwrap_or(0);
    if let Some(picked) =
        choice(ui, "Remote scale", "ra-scale", &["100%", "125%", "150%", "175%", "200%"], at)
    {
        rdp.scale = RDP_SCALES[picked];
    }
    let at = usize::from(!rdp.local_cursor);
    if let Some(picked) =
        choice(ui, "Pointer", "ra-pointer", &["Local cursor", "In the picture"], at)
    {
        rdp.local_cursor = picked == 0;
    }
}

/// The clipboard, keyboard, client name and start program.
fn session(ui: &mut Ui, rdp: &mut RdpAdvanced) {
    section(ui);
    heading(ui, "Session");
    ticks(ui, "ra-session", &mut [(&mut rdp.clipboard, "Share the clipboard")]);
    text_row(ui, "ra-keyboard", "Keyboard", &mut rdp.keyboard_layout, "0x0409, US English");
    text_row(ui, "ra-client", "Client name", &mut rdp.client_name, "rmx");
    text_row(ui, "ra-program", "Start program", &mut rdp.start_program, "The desktop");
    text_row(ui, "ra-workdir", "Start in", &mut rdp.work_dir, "");
}

/// The local devices a session hands the server. No device channel exists yet, so the group
/// draws nothing; the first one starts it with `section(ui)` and `heading(ui, "Devices")`.
fn devices(_ui: &mut Ui, _rdp: &mut RdpAdvanced) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_layouts_read_as_hexadecimal() {
        assert_eq!(parse_keyboard_layout("0x0407"), Some(0x0407));
        assert_eq!(parse_keyboard_layout(" 40C "), Some(0x040c));
        assert_eq!(parse_keyboard_layout(""), None);
        assert_eq!(parse_keyboard_layout("German"), None);
    }

    #[test]
    fn only_custom_has_no_preset() {
        for quality in [RdpQuality::Best, RdpQuality::Balanced, RdpQuality::Fast] {
            assert!(quality.preset().is_some(), "{quality:?}");
        }
        assert_eq!(RdpQuality::Custom.preset(), None);
    }
}
