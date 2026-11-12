//! An RDP connection's advanced settings: how they are saved with it, and what they ask of a
//! session.
//!
//! Only what differs from the default is saved, under keys starting with [`PREFIX`], so a
//! connection saved before a setting existed keeps behaving as it did.

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

/// Put `advanced` into `settings`, replacing what was there. The picture is saved only for a
/// custom quality, as a preset says it already.
pub fn save(advanced: &RdpAdvanced, quality: RdpQuality, settings: &mut BTreeMap<String, String>) {
    settings.retain(|key, _| !key.starts_with(PREFIX));
    let mut put = |key: &str, value: &str| {
        settings.insert(key.to_owned(), value.to_owned());
    };
    let defaults = RdpAdvanced::default();

    if quality == RdpQuality::Custom {
        let picture = &advanced.picture;
        put(DEPTH, &picture.color_depth.bits().to_string());
        if let Some((_, name)) = LINKS.iter().find(|(link, _)| *link == picture.connection) {
            put(LINK, name);
        }
        let mut effects = picture.effects;
        let on: Vec<&str> = effect_fields(&mut effects)
            .into_iter()
            .filter(|(_, allowed)| **allowed)
            .map(|(name, _)| name)
            .collect();
        let list = if on.is_empty() { "none".to_owned() } else { on.join(",") };
        put(EFFECTS, &list);
    }
    for (key, value, default) in [
        (GFX, advanced.graphics_pipeline, defaults.graphics_pipeline),
        (REMOTEFX, advanced.remotefx, defaults.remotefx),
        (LOSSY, advanced.lossy, defaults.lossy),
        (CLIPBOARD, advanced.clipboard, defaults.clipboard),
    ] {
        if value != default {
            put(key, if value { "1" } else { "0" });
        }
    }
    if let Some((_, name)) = COMPRESSIONS.iter().find(|(c, _)| *c == advanced.compression) {
        put(COMPRESSION, name);
    }
    if advanced.scale != defaults.scale {
        put(SCALE, &advanced.scale.to_string());
    }
    if !advanced.local_cursor {
        put(POINTER, "picture");
    }
    for (key, value) in [
        (KEYBOARD, &advanced.keyboard_layout),
        (CLIENT, &advanced.client_name),
        (PROGRAM, &advanced.start_program),
        (WORK_DIR, &advanced.work_dir),
    ] {
        let value = value.trim();
        if !value.is_empty() {
            put(key, value);
        }
    }
}

/// What `settings` holds of an RDP connection's advanced settings.
pub fn load(settings: &BTreeMap<String, String>) -> RdpAdvanced {
    let get = |key: &str| settings.get(key).map(String::as_str);
    let flag = |key: &str, default: bool| match get(key) {
        Some("1") => true,
        Some("0") => false,
        _ => default,
    };
    let text = |key: &str| get(key).unwrap_or_default().to_owned();
    let defaults = RdpAdvanced::default();

    let mut picture = RdpPicture::default();
    if let Some(bits) = get(DEPTH).and_then(|bits| bits.parse::<u32>().ok())
        && let Some(depth) = RdpColorDepth::ALL.into_iter().find(|depth| depth.bits() == bits)
    {
        picture.color_depth = depth;
    }
    if let Some((link, _)) = LINKS.iter().find(|(_, name)| Some(*name) == get(LINK)) {
        picture.connection = *link;
    }
    if let Some(list) = get(EFFECTS) {
        let on: Vec<&str> = list.split(',').map(str::trim).collect();
        for (name, allowed) in effect_fields(&mut picture.effects) {
            *allowed = on.contains(&name);
        }
    }

    RdpAdvanced {
        picture,
        graphics_pipeline: flag(GFX, defaults.graphics_pipeline),
        remotefx: flag(REMOTEFX, defaults.remotefx),
        lossy: flag(LOSSY, defaults.lossy),
        compression: COMPRESSIONS
            .iter()
            .find(|(_, name)| Some(*name) == get(COMPRESSION))
            .map_or(RdpCompression::None, |(compression, _)| *compression),
        scale: get(SCALE)
            .and_then(|scale| scale.parse().ok())
            .filter(|scale| RDP_SCALES.contains(scale))
            .unwrap_or(defaults.scale),
        local_cursor: get(POINTER) != Some("picture"),
        clipboard: flag(CLIPBOARD, defaults.clipboard),
        keyboard_layout: text(KEYBOARD),
        client_name: text(CLIENT),
        start_program: text(PROGRAM),
        work_dir: text(WORK_DIR),
    }
}

/// Have `config` ask for what `quality` and `advanced` say.
pub fn apply(quality: RdpQuality, advanced: &RdpAdvanced, config: &mut RdpConfig) {
    match quality {
        RdpQuality::Best => config.quality = Quality::Best,
        RdpQuality::Balanced => config.quality = Quality::Balanced,
        RdpQuality::Fast => config.quality = Quality::Fast,
        RdpQuality::Custom => config.experience = Some(experience(&advanced.picture)),
    }
    config.graphics_pipeline = advanced.graphics_pipeline;
    config.remotefx = advanced.remotefx;
    config.lossy = advanced.lossy;
    config.compression = match advanced.compression {
        RdpCompression::None => None,
        RdpCompression::K8 => Some(Compression::K8),
        RdpCompression::K64 => Some(Compression::K64),
        RdpCompression::Rdp6 => Some(Compression::Rdp6),
        RdpCompression::Rdp61 => Some(Compression::Rdp61),
    };
    config.scale_factor = advanced.scale;
    config.local_cursor = advanced.local_cursor;
    config.clipboard = advanced.clipboard;
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
}

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
        assert!(!config.clipboard);
        assert!(config.local_cursor);
    }
}
