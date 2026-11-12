//! An RDP connection's advanced settings: how they are saved with it, and what they ask of a
//! session.
//!
//! Only what differs from the default is saved, under keys starting with [`PREFIX`], so a
//! connection saved before a setting existed keeps behaving as it did.
//!
//! Each group of the form has one function here, listed in [`GROUPS`], which saves the group,
//! reads it back and applies it.

use std::collections::BTreeMap;

use rmx_rdp::{Compression, Connection, Effects, Experience, Quality, RdpConfig};
use rmx_ui::widgets::{
    RDP_SCALES, RdpAdvanced, RdpColorDepth, RdpCompression, RdpConnection, RdpEffects, RdpPicture,
    RdpQuality, parse_keyboard_layout,
};

/// What every key of these settings starts with.
const PREFIX: &str = "rdp.";
const DEPTH: &str = "rdp.depth";
const LINK: &str = "rdp.link";
const EFFECTS: &str = "rdp.effects";
const GFX: &str = "rdp.gfx";
const REMOTEFX: &str = "rdp.remotefx";
const LOSSY: &str = "rdp.lossy";
const COMPRESSION: &str = "rdp.compression";
const SCALE: &str = "rdp.scale";
const POINTER: &str = "rdp.pointer";
const CLIPBOARD: &str = "rdp.clipboard";
const KEYBOARD: &str = "rdp.keyboard";
const CLIENT: &str = "rdp.client";
const PROGRAM: &str = "rdp.program";
const WORK_DIR: &str = "rdp.workdir";

const LINKS: [(RdpConnection, &str); 7] = [
    (RdpConnection::Modem, "modem"),
    (RdpConnection::BroadbandLow, "broadband-low"),
    (RdpConnection::Satellite, "satellite"),
    (RdpConnection::BroadbandHigh, "broadband-high"),
    (RdpConnection::Wan, "wan"),
    (RdpConnection::Lan, "lan"),
    (RdpConnection::Autodetect, "detect"),
];

const COMPRESSIONS: [(RdpCompression, &str); 4] = [
    (RdpCompression::K8, "8k"),
    (RdpCompression::K64, "64k"),
    (RdpCompression::Rdp6, "rdp6"),
    (RdpCompression::Rdp61, "rdp61"),
];

/// The groups of settings, in the form's order.
const GROUPS: [fn(Op<'_>); 5] = [picture, graphics, display, session, devices];

type Settings = BTreeMap<String, String>;

/// What a group is asked to do with its settings.
enum Op<'a> {
    /// Put the group's settings into `settings`, which holds none yet.
    Save { advanced: &'a RdpAdvanced, quality: RdpQuality, settings: &'a mut Settings },
    /// Read the group's settings from `settings` into `advanced`, which holds the defaults.
    Load { settings: &'a Settings, advanced: &'a mut RdpAdvanced },
    /// Have `config` ask for what the group says.
    Apply { quality: RdpQuality, advanced: &'a RdpAdvanced, config: &'a mut RdpConfig },
}

/// Put `advanced` into `settings`, replacing what was there. The picture is saved only for a
/// custom quality, as a preset says it already.
pub fn save(advanced: &RdpAdvanced, quality: RdpQuality, settings: &mut Settings) {
    settings.retain(|key, _| !key.starts_with(PREFIX));
    for group in GROUPS {
        group(Op::Save { advanced, quality, settings: &mut *settings });
    }
}

/// What `settings` holds of an RDP connection's advanced settings.
pub fn load(settings: &Settings) -> RdpAdvanced {
    let mut advanced = RdpAdvanced::default();
    for group in GROUPS {
        group(Op::Load { settings, advanced: &mut advanced });
    }
    advanced
}

/// Have `config` ask for what `quality` and `advanced` say.
pub fn apply(quality: RdpQuality, advanced: &RdpAdvanced, config: &mut RdpConfig) {
    for group in GROUPS {
        group(Op::Apply { quality, advanced, config: &mut *config });
    }
}

fn put(settings: &mut Settings, key: &str, value: &str) {
    settings.insert(key.to_owned(), value.to_owned());
}

/// Saves a flag unless it has its default value.
fn put_flag(settings: &mut Settings, key: &str, value: bool, default: bool) {
    if value != default {
        put(settings, key, if value { "1" } else { "0" });
    }
}

/// Saves text unless it is blank.
fn put_text(settings: &mut Settings, key: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        put(settings, key, value);
    }
}

fn get<'a>(settings: &'a Settings, key: &str) -> Option<&'a str> {
    settings.get(key).map(String::as_str)
}

fn flag(settings: &Settings, key: &str, default: bool) -> bool {
    match get(settings, key) {
        Some("1") => true,
        Some("0") => false,
        _ => default,
    }
}

fn text(settings: &Settings, key: &str) -> String {
    get(settings, key).unwrap_or_default().to_owned()
}

/// Each effect's name in the saved list, with how to reach it.
fn effect_fields(effects: &mut RdpEffects) -> [(&'static str, &mut bool); 7] {
    [
        ("wallpaper", &mut effects.wallpaper),
        ("drag", &mut effects.full_window_drag),
        ("menus", &mut effects.menu_animations),
        ("themes", &mut effects.themes),
        ("shadow", &mut effects.cursor_shadow),
        ("smoothing", &mut effects.font_smoothing),
        ("composition", &mut effects.desktop_composition),
    ]
}

/// The quality, and the color depth, link and effects of a custom one.
fn picture(op: Op<'_>) {
    match op {
        Op::Save { advanced, quality, settings } => {
            if quality != RdpQuality::Custom {
                return;
            }
            let picture = &advanced.picture;
            put(settings, DEPTH, &picture.color_depth.bits().to_string());
            if let Some((_, name)) = LINKS.iter().find(|(link, _)| *link == picture.connection) {
                put(settings, LINK, name);
            }
            let mut effects = picture.effects;
            let on: Vec<&str> = effect_fields(&mut effects)
                .into_iter()
                .filter(|(_, allowed)| **allowed)
                .map(|(name, _)| name)
                .collect();
            let list = if on.is_empty() { "none".to_owned() } else { on.join(",") };
            put(settings, EFFECTS, &list);
        },
        Op::Load { settings, advanced } => {
            let picture = &mut advanced.picture;
            if let Some(bits) = get(settings, DEPTH).and_then(|bits| bits.parse::<u32>().ok())
                && let Some(depth) =
                    RdpColorDepth::ALL.into_iter().find(|depth| depth.bits() == bits)
            {
                picture.color_depth = depth;
            }
            let link = get(settings, LINK);
            if let Some((link, _)) = LINKS.iter().find(|(_, name)| Some(*name) == link) {
                picture.connection = *link;
            }
            if let Some(list) = get(settings, EFFECTS) {
                let on: Vec<&str> = list.split(',').map(str::trim).collect();
                for (name, allowed) in effect_fields(&mut picture.effects) {
                    *allowed = on.contains(&name);
                }
            }
        },
        Op::Apply { quality, advanced, config } => match quality {
            RdpQuality::Best => config.quality = Quality::Best,
            RdpQuality::Balanced => config.quality = Quality::Balanced,
            RdpQuality::Fast => config.quality = Quality::Fast,
            RdpQuality::Custom => config.experience = Some(experience(&advanced.picture)),
        },
    }
}

/// The graphics pipeline, codecs and compression.
fn graphics(op: Op<'_>) {
    let defaults = RdpAdvanced::default();
    match op {
        Op::Save { advanced, settings, .. } => {
            put_flag(settings, GFX, advanced.graphics_pipeline, defaults.graphics_pipeline);
            put_flag(settings, REMOTEFX, advanced.remotefx, defaults.remotefx);
            put_flag(settings, LOSSY, advanced.lossy, defaults.lossy);
            if let Some((_, name)) = COMPRESSIONS.iter().find(|(c, _)| *c == advanced.compression) {
                put(settings, COMPRESSION, name);
            }
        },
        Op::Load { settings, advanced } => {
            advanced.graphics_pipeline = flag(settings, GFX, defaults.graphics_pipeline);
            advanced.remotefx = flag(settings, REMOTEFX, defaults.remotefx);
            advanced.lossy = flag(settings, LOSSY, defaults.lossy);
            let compression = get(settings, COMPRESSION);
            advanced.compression = COMPRESSIONS
                .iter()
                .find(|(_, name)| Some(*name) == compression)
                .map_or(RdpCompression::None, |(compression, _)| *compression);
        },
        Op::Apply { advanced, config, .. } => {
            config.channels.graphics_pipeline.enabled = advanced.graphics_pipeline;
            config.remotefx = advanced.remotefx;
            config.lossy = advanced.lossy;
            config.compression = match advanced.compression {
                RdpCompression::None => None,
                RdpCompression::K8 => Some(Compression::K8),
                RdpCompression::K64 => Some(Compression::K64),
                RdpCompression::Rdp6 => Some(Compression::Rdp6),
                RdpCompression::Rdp61 => Some(Compression::Rdp61),
            };
        },
    }
}

/// The remote scale and where the pointer is drawn.
fn display(op: Op<'_>) {
    let defaults = RdpAdvanced::default();
    match op {
        Op::Save { advanced, settings, .. } => {
            if advanced.scale != defaults.scale {
                put(settings, SCALE, &advanced.scale.to_string());
            }
            if !advanced.local_cursor {
                put(settings, POINTER, "picture");
            }
        },
        Op::Load { settings, advanced } => {
            advanced.scale = get(settings, SCALE)
                .and_then(|scale| scale.parse().ok())
                .filter(|scale| RDP_SCALES.contains(scale))
                .unwrap_or(defaults.scale);
            advanced.local_cursor = get(settings, POINTER) != Some("picture");
        },
        Op::Apply { advanced, config, .. } => {
            config.scale_factor = advanced.scale;
            config.local_cursor = advanced.local_cursor;
        },
    }
}

/// The clipboard, keyboard, client name and start program.
fn session(op: Op<'_>) {
    let defaults = RdpAdvanced::default();
    match op {
        Op::Save { advanced, settings, .. } => {
            put_flag(settings, CLIPBOARD, advanced.clipboard, defaults.clipboard);
            put_text(settings, KEYBOARD, &advanced.keyboard_layout);
            put_text(settings, CLIENT, &advanced.client_name);
            put_text(settings, PROGRAM, &advanced.start_program);
            put_text(settings, WORK_DIR, &advanced.work_dir);
        },
        Op::Load { settings, advanced } => {
            advanced.clipboard = flag(settings, CLIPBOARD, defaults.clipboard);
            advanced.keyboard_layout = text(settings, KEYBOARD);
            advanced.client_name = text(settings, CLIENT);
            advanced.start_program = text(settings, PROGRAM);
            advanced.work_dir = text(settings, WORK_DIR);
        },
        Op::Apply { advanced, config, .. } => {
            config.channels.clipboard.enabled = advanced.clipboard;
            if let Some(layout) = parse_keyboard_layout(&advanced.keyboard_layout) {
                config.keyboard_layout = layout;
            }
            let client = advanced.client_name.trim();
            if !client.is_empty() {
                // The server keeps 15 characters of it.
                config.client_name = client.chars().take(15).collect();
            }
            config.start_program = advanced.start_program.trim().to_owned();
            config.work_dir = advanced.work_dir.trim().to_owned();
        },
    }
}

/// The local devices a session hands the server. The device channels fill it in; none exists
/// yet.
fn devices(_op: Op<'_>) {}

fn experience(picture: &RdpPicture) -> Experience {
    let e = &picture.effects;
    Experience {
        color_depth: picture.color_depth.bits(),
        connection: match picture.connection {
            RdpConnection::Modem => Connection::Modem,
            RdpConnection::BroadbandLow => Connection::BroadbandLow,
            RdpConnection::Satellite => Connection::Satellite,
            RdpConnection::BroadbandHigh => Connection::BroadbandHigh,
            RdpConnection::Wan => Connection::Wan,
            RdpConnection::Lan => Connection::Lan,
            RdpConnection::Autodetect => Connection::Autodetect,
        },
        effects: Effects {
            wallpaper: e.wallpaper,
            full_window_drag: e.full_window_drag,
            menu_animations: e.menu_animations,
            themes: e.themes,
            cursor_shadow: e.cursor_shadow,
            font_smoothing: e.font_smoothing,
            desktop_composition: e.desktop_composition,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RdpConfig {
        RdpConfig::new("host", "user", "secret", (800, 600), [0; 32])
    }

    /// The form shows the presets' rows, so they must say what the session asks for.
    #[test]
    fn the_form_shows_what_each_preset_asks_for() {
        for (ui, rdp) in [
            (RdpQuality::Best, Quality::Best),
            (RdpQuality::Balanced, Quality::Balanced),
            (RdpQuality::Fast, Quality::Fast),
        ] {
            let preset = ui.preset().expect("a preset");
            assert_eq!(experience(&preset), rdp.experience(), "{ui:?}");
        }
    }

    #[test]
    fn defaults_save_nothing_and_read_back_as_defaults() {
        let mut settings = BTreeMap::new();
        save(&RdpAdvanced::default(), RdpQuality::Balanced, &mut settings);
        assert!(settings.is_empty(), "{settings:?}");
        assert_eq!(load(&settings), RdpAdvanced::default());
    }

    #[test]
    fn every_setting_reads_back_as_saved() {
        let advanced = RdpAdvanced {
            picture: RdpPicture {
                color_depth: RdpColorDepth::Bits24,
                connection: RdpConnection::Wan,
                effects: RdpEffects {
                    wallpaper: false,
                    cursor_shadow: false,
                    ..RdpEffects::ALL_ON
                },
            },
            graphics_pipeline: false,
            remotefx: false,
            lossy: false,
            compression: RdpCompression::Rdp61,
            scale: 150,
            local_cursor: false,
            clipboard: false,
            keyboard_layout: "0x0407".to_owned(),
            client_name: "laptop".to_owned(),
            start_program: r"C:\Tools\app.exe --flag".to_owned(),
            work_dir: r"C:\Tools".to_owned(),
        };
        let mut settings = BTreeMap::from([("rdp.stale".to_owned(), "x".to_owned())]);
        save(&advanced, RdpQuality::Custom, &mut settings);
        assert!(!settings.contains_key("rdp.stale"));
        assert_eq!(load(&settings), advanced);
    }

    #[test]
    fn a_custom_picture_with_no_effects_stays_without_them() {
        let advanced = RdpAdvanced {
            picture: RdpPicture { effects: RdpEffects::ALL_OFF, ..RdpPicture::default() },
            ..RdpAdvanced::default()
        };
        let mut settings = BTreeMap::new();
        save(&advanced, RdpQuality::Custom, &mut settings);
        assert_eq!(load(&settings).picture.effects, RdpEffects::ALL_OFF);
    }

    #[test]
    fn a_preset_ignores_the_custom_picture() {
        let mut advanced = RdpAdvanced::default();
        advanced.picture.color_depth = RdpColorDepth::Bits15;
        let mut config = config();
        apply(RdpQuality::Fast, &advanced, &mut config);
        assert_eq!(config.quality, Quality::Fast);
        assert_eq!(config.experience, None);

        apply(RdpQuality::Custom, &advanced, &mut config);
        assert_eq!(config.experience.map(|e| e.color_depth), Some(15));
    }

    #[test]
    fn the_session_settings_reach_the_config() {
        let advanced = RdpAdvanced {
            keyboard_layout: "40c".to_owned(),
            client_name: "a-very-long-computer-name".to_owned(),
            scale: 200,
            clipboard: false,
            ..RdpAdvanced::default()
        };
        let mut config = config();
        apply(RdpQuality::Balanced, &advanced, &mut config);
        assert_eq!(config.keyboard_layout, 0x040c);
        assert_eq!(config.client_name, "a-very-long-com");
        assert_eq!(config.scale_factor, 200);
        assert!(!config.channels.clipboard.enabled);
        assert!(config.local_cursor);
    }
}
