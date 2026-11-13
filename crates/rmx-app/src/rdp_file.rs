//! `.rdp` files, as mstsc and other managers save them: read into a connection form, and
//! written back out from one.
//!
//! What rmx has a setting for is mapped; everything else is named in the import's notes, never
//! dropped without a word. Saved passwords are DPAPI-encrypted for whoever saved the file, so
//! they are never read: the password is left empty and rmx asks for it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ironrdp_propertyset::{PropertySet, Value};
use ironrdp_rdpfile::{Error as LineError, ErrorKind};
use rmx_ui::Protocol;
use rmx_ui::widgets::{
    ConnectionDraft, RDP_SCALES, RdpColorDepth, RdpCompression, RdpConnection, RdpPicture,
    RdpQuality, RdpSecurity, parse_keyboard_layout,
};

use crate::background::off_main;

/// The port an address without one has.
const RDP_PORT: u16 = 3389;
/// The size rmx gives a desktop that does not follow its pane.
const KEPT_SIZE: (i64, i64) = (1920, 1080);
/// Where mstsc keeps the saved password, sealed for whoever saved it.
const PASSWORD_KEY: &str = "password 51";

/// The keys read into the form. Every other key of a file is named as not used.
const MAPPED: [&str; 26] = [
    "full address",
    "server port",
    "username",
    "domain",
    "enablecredsspsupport",
    // rmx always checks the server's certificate, and asks about one it does not know.
    "authentication level",
    "session bpp",
    "connection type",
    "disable wallpaper",
    "disable full window drag",
    "disable menu anims",
    "disable themes",
    "allow font smoothing",
    "allow desktop composition",
    "screen mode id",
    "desktopwidth",
    "desktopheight",
    "dynamic resolution",
    "smart sizing",
    "redirectclipboard",
    "keyboardlayout",
    "alternate shell",
    "shell working directory",
    "desktopscalefactor",
    "compression",
    // Said in a note of its own when it asks for a program rather than the desktop.
    "remoteapplicationmode",
];

/// `connection type` as mstsc numbers it.
const LINKS: [(i64, RdpConnection); 7] = [
    (1, RdpConnection::Modem),
    (2, RdpConnection::BroadbandLow),
    (3, RdpConnection::Satellite),
    (4, RdpConnection::BroadbandHigh),
    (5, RdpConnection::Wan),
    (6, RdpConnection::Lan),
    (7, RdpConnection::Autodetect),
];

/// A file read into a connection form.
#[derive(Debug)]
pub struct RdpImport {
    pub draft: ConnectionDraft,
    /// What the file asked for that the connection does not do, for the preview.
    pub notes: Vec<String>,
}

/// One file of an import, by its name, and what came of reading it.
#[derive(Debug)]
pub struct ImportedFile {
    pub source: String,
    pub result: Result<RdpImport, String>,
}

/// The text of a file, however it is encoded: mstsc writes UTF-16 LE with a byte order mark.
pub fn decode(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8], little: bool| {
        let (pairs, _) = bytes.as_chunks::<2>();
        let units = pairs
            .iter()
            .map(|&pair| if little { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) });
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
    };
    match bytes {
        [0xff, 0xfe, rest @ ..] => utf16(rest, true),
        [0xfe, 0xff, rest @ ..] => utf16(rest, false),
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        // Without a mark, the zero high bytes of ASCII in UTF-16 give it away.
        [low, 0, ..] if *low != 0 => utf16(bytes, true),
        [0, high, ..] if *high != 0 => utf16(bytes, false),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The connection the file `name` (without `.rdp`) describes.
pub fn import(name: &str, bytes: &[u8]) -> Result<RdpImport, String> {
    let text = decode(bytes);
    let parsed = ironrdp_rdpfile::parse(&text);
    let mut notes = Vec::new();
    let mut sealed = false;
    for error in &parsed.errors {
        match &error.kind {
            ErrorKind::UnknownType { key, .. } if key.eq_ignore_ascii_case(PASSWORD_KEY) => {
                sealed = true;
            },
            // A blank line says nothing, and is no mistake.
            ErrorKind::MalformedLine
                if text.lines().nth(error.line - 1).is_some_and(|line| line.trim().is_empty()) => {
            },
            _ => notes.push(format!("Not read: {}", line_error(error))),
        }
    }
    // Keys as mstsc writes them, whatever case another program used.
    let settings: BTreeMap<String, Value> =
        parsed.properties.into_iter().map(|(key, value)| (key.to_lowercase(), value)).collect();
    let file = File(&settings);

    let address = file.text("full address").map(str::trim).filter(|a| !a.is_empty());
    let Some(address) = address else {
        return Err(if parsed.errors.is_empty() {
            "It has no full address to connect to".to_owned()
        } else {
            "It is not an .rdp file with an address to connect to".to_owned()
        });
    };
    let (host, port) = split_address(address);
    let port = port.or_else(|| file.int("server port").and_then(|p| u16::try_from(p).ok()));

    let mut draft = ConnectionDraft::new(Protocol::Rdp);
    draft.name = if name.trim().is_empty() { host.clone() } else { name.trim().to_owned() };
    draft.host = host;
    draft.port = port.unwrap_or(RDP_PORT).to_string();
    login(&file, &mut draft);
    draft.rdp_security = match file.int("enablecredsspsupport") {
        Some(0) => RdpSecurity::Tls,
        _ => RdpSecurity::Negotiate,
    };
    picture(&file, &mut draft);
    remote_size(&file, &mut draft, &mut notes);
    session(&file, &mut draft, &mut notes);

    if sealed {
        notes.insert(
            0,
            "The saved password is not imported: only whoever saved the file can open it. rmx \
             asks for it."
                .to_owned(),
        );
    }
    special_notes(&file, &mut notes);
    let skipped: Vec<&str> = settings
        .iter()
        .filter(|(key, value)| {
            !MAPPED.contains(&key.as_str()) && value.as_str().is_none_or(|text| !text.is_empty())
        })
        .map(|(key, _)| if key.is_empty() { "a setting with no name" } else { key.as_str() })
        .collect();
    if !skipped.is_empty() {
        notes.push(format!("Not used by rmx: {}", skipped.join(", ")));
    }
    Ok(RdpImport { draft, notes })
}

/// What went wrong with a line, saying which.
fn line_error(error: &LineError) -> String {
    match &error.kind {
        ErrorKind::UnknownType { key, .. } => {
            format!("line {}, {key}, is of a type .rdp files do not have", error.line)
        },
        ErrorKind::InvalidValue { key, .. } => {
            format!("line {}, {key}, is not a number", error.line)
        },
        ErrorKind::MalformedLine => format!("line {} is not a setting", error.line),
    }
}

/// The settings of a file, by their lowercase keys.
struct File<'a>(&'a BTreeMap<String, Value>);

impl File<'_> {
    fn text(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_int)
    }

    /// A setting that is on with 1 and off with 0.
    fn flag(&self, key: &str) -> Option<bool> {
        self.int(key).map(|value| value != 0)
    }
}

/// `host`, `host:port`, `[v6]`, `[v6]:port`, or a bare IPv6 address, which has no port.
fn split_address(address: &str) -> (String, Option<u16>) {
    if let Some(rest) = address.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after.strip_prefix(':').and_then(|port| port.parse().ok());
        return (host.to_owned(), port);
    }
    match address.split_once(':') {
        Some((host, port)) if !port.contains(':') => match port.parse() {
            Ok(port) => (host.to_owned(), Some(port)),
            Err(_) => (host.to_owned(), None),
        },
        _ => (address.to_owned(), None),
    }
}

/// The user and domain: `DOMAIN\user` splits, `user@domain` is a name of its own.
fn login(file: &File<'_>, draft: &mut ConnectionDraft) {
    let user = file.text("username").unwrap_or("").trim();
    let (domain, user) = match user.split_once('\\') {
        Some((domain, user)) => (domain, user),
        None => ("", user),
    };
    draft.username = user.to_owned();
    draft.domain = file
        .text("domain")
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
        .unwrap_or(domain)
        .to_owned();
    // A file naming its user keeps it, rather than taking a folder's.
    draft.inherit = draft.username.is_empty();
}

/// The color depth, link and effects: a preset when they are one, else a custom quality.
fn picture(file: &File<'_>, draft: &mut ConnectionDraft) {
    let mut picture = RdpQuality::Balanced.preset().unwrap_or_default();
    if let Some(bits) = file.int("session bpp") {
        let depth = RdpColorDepth::ALL.into_iter().find(|depth| i64::from(depth.bits()) == bits);
        picture.color_depth = depth.unwrap_or(picture.color_depth);
    }
    if let Some(link) = file.int("connection type")
        && let Some(&(_, link)) = LINKS.iter().find(|(number, _)| *number == link)
    {
        picture.connection = link;
    }
    let effects = &mut picture.effects;
    let allowed = |key: &str, on_means_allowed: bool, effect: &mut bool| {
        if let Some(on) = file.flag(key) {
            *effect = on == on_means_allowed;
        }
    };
    allowed("disable wallpaper", false, &mut effects.wallpaper);
    allowed("disable full window drag", false, &mut effects.full_window_drag);
    allowed("disable menu anims", false, &mut effects.menu_animations);
    allowed("disable themes", false, &mut effects.themes);
    allowed("allow font smoothing", true, &mut effects.font_smoothing);
    allowed("allow desktop composition", true, &mut effects.desktop_composition);

    let preset = [RdpQuality::Best, RdpQuality::Balanced, RdpQuality::Fast]
        .into_iter()
        .find(|quality| quality.preset() == Some(picture));
    match preset {
        Some(quality) => {
            draft.rdp_quality = quality;
            draft.rdp.picture = RdpPicture::default();
        },
        None => {
            draft.rdp_quality = RdpQuality::Custom;
            draft.rdp.picture = picture;
        },
    }
}

/// Whether the desktop follows the pane: full screen or a dynamic resolution do.
fn remote_size(file: &File<'_>, draft: &mut ConnectionDraft, notes: &mut Vec<String>) {
    let windowed = file.int("screen mode id") == Some(1);
    draft.follow_size = file.flag("dynamic resolution").unwrap_or(!windowed);
    if file.flag("smart sizing") == Some(true) {
        draft.scale = rmx_ui::widgets::ScaleChoice::Fit;
    }
    let size = (file.int("desktopwidth"), file.int("desktopheight"));
    if !draft.follow_size
        && let (Some(width), Some(height)) = size
        && (width, height) != KEPT_SIZE
    {
        let (kept_width, kept_height) = KEPT_SIZE;
        notes.push(format!(
            "The desktop is {width}x{height} in the file: rmx keeps one of {kept_width}x{kept_height}"
        ));
    }
}

/// The clipboard, keyboard, start program, scale and compression.
fn session(file: &File<'_>, draft: &mut ConnectionDraft, notes: &mut Vec<String>) {
    let rdp = &mut draft.rdp;
    if let Some(on) = file.flag("redirectclipboard") {
        rdp.clipboard = on;
    }
    let layout = match file.0.get("keyboardlayout") {
        Some(Value::Str(text)) => parse_keyboard_layout(text),
        Some(Value::Int(number)) => u32::try_from(*number).ok().filter(|&layout| layout != 0),
        None => None,
    };
    if let Some(layout) = layout {
        rdp.keyboard_layout = format!("0x{layout:04x}");
    }
    rdp.start_program = file.text("alternate shell").unwrap_or("").trim().to_owned();
    rdp.work_dir = file.text("shell working directory").unwrap_or("").trim().to_owned();
    if let Some(scale) = file.int("desktopscalefactor") {
        let nearest = RDP_SCALES.into_iter().min_by_key(|&s| (i64::from(s) - scale).abs());
        rdp.scale = nearest.unwrap_or(100);
        if i64::from(rdp.scale) != scale {
            notes.push(format!("A scale of {scale}% is not offered: {}% instead", rdp.scale));
        }
    }
    if let Some(on) = file.flag("compression") {
        rdp.compression = if on { RdpCompression::Rdp61 } else { RdpCompression::None };
    }
}

/// What deserves more than its key in the list of what is not used.
fn special_notes(file: &File<'_>, notes: &mut Vec<String>) {
    let gateway = file.text("gatewayhostname").map(str::trim).filter(|host| !host.is_empty());
    // 1 always, 2 when the server cannot be reached directly, 3 the default gateway.
    if let Some(gateway) = gateway
        && matches!(file.int("gatewayusagemethod"), Some(1..=3))
    {
        notes.push(format!(
            "RD Gateway {gateway} is not used: rmx connects directly, as it has no RD Gateway yet"
        ));
    }
    if file.flag("remoteapplicationmode") == Some(true) {
        let program = file.text("remoteapplicationprogram").unwrap_or("").trim();
        notes.push(format!("RemoteApp {program} is not used: rmx shows the whole desktop"));
    }
}

/// Read every `.rdp` file of `paths`, a folder standing for the ones directly in it. Reads
/// files, so never on the main thread.
pub fn read_paths(paths: &[PathBuf]) -> Vec<ImportedFile> {
    off_main("reading .rdp files");
    let mut files = Vec::new();
    for path in paths {
        if !path.is_dir() {
            files.push(read_file(path));
            continue;
        }
        let mut found: Vec<PathBuf> = match std::fs::read_dir(path) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && is_rdp(path))
                .collect(),
            Err(err) => {
                files
                    .push(ImportedFile { source: source_name(path), result: Err(err.to_string()) });
                continue;
            },
        };
        if found.is_empty() {
            let result = Err("There are no .rdp files in it".to_owned());
            files.push(ImportedFile { source: source_name(path), result });
        }
        found.sort();
        files.extend(found.iter().map(|path| read_file(path)));
    }
    files
}

/// Whether `path` names an `.rdp` file, in any case.
pub fn is_rdp(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("rdp"))
}

fn read_file(path: &Path) -> ImportedFile {
    let name = path.file_stem().map(|stem| stem.to_string_lossy()).unwrap_or_default();
    let result =
        std::fs::read(path).map_err(|err| err.to_string()).and_then(|bytes| import(&name, &bytes));
    ImportedFile { source: source_name(path), result }
}

/// The file that makes `draft` again: UTF-16 LE with a byte order mark and CRLF, as mstsc
/// writes it. No password goes in it.
pub fn export(draft: &ConnectionDraft) -> Vec<u8> {
    let mut file = PropertySet::new();
    let port = draft.port.trim().parse().unwrap_or(RDP_PORT);
    let host = draft.host.trim();
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    let address = if port == RDP_PORT { host } else { format!("{host}:{port}") };
    file.insert("full address", address);
    if !draft.username.is_empty() {
        file.insert("username", draft.username.as_str());
    }
    if !draft.domain.is_empty() {
        file.insert("domain", draft.domain.as_str());
    }
    file.insert("enablecredsspsupport", i32::from(draft.rdp_security != RdpSecurity::Tls));
    file.insert("prompt for credentials", 0);

    let picture = draft.rdp_quality.preset().unwrap_or(draft.rdp.picture);
    file.insert("session bpp", picture.color_depth.bits());
    let link = LINKS.iter().find(|(_, link)| *link == picture.connection).map_or(6, |&(n, _)| n);
    file.insert("connection type", link);
    let effects = picture.effects;
    file.insert("disable wallpaper", !effects.wallpaper);
    file.insert("disable full window drag", !effects.full_window_drag);
    file.insert("disable menu anims", !effects.menu_animations);
    file.insert("disable themes", !effects.themes);
    file.insert("allow font smoothing", effects.font_smoothing);
    file.insert("allow desktop composition", effects.desktop_composition);

    if draft.follow_size {
        file.insert("screen mode id", 2);
        file.insert("dynamic resolution", 1);
    } else {
        let (width, height) = KEPT_SIZE;
        file.insert("screen mode id", 1);
        file.insert("dynamic resolution", 0);
        file.insert("desktopwidth", width);
        file.insert("desktopheight", height);
    }
    file.insert("smart sizing", 1);
    let rdp = &draft.rdp;
    file.insert("redirectclipboard", rdp.clipboard);
    if let Some(layout) = parse_keyboard_layout(&rdp.keyboard_layout) {
        file.insert("keyboardlayout", format!("{layout:08x}"));
    }
    file.insert("alternate shell", rdp.start_program.as_str());
    file.insert("shell working directory", rdp.work_dir.as_str());
    if rdp.scale != 100 {
        file.insert("desktopscalefactor", rdp.scale);
    }
    file.insert("compression", rdp.compression != RdpCompression::None);

    let text = ironrdp_rdpfile::write(&file).replace('\n', "\r\n");
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

/// What a file of this path is called in an import.
fn source_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.display().to_string())
}

#[cfg(test)]
mod tests {
    use rmx_ui::widgets::{
        RdpColorDepth, RdpConnection, RdpEffects, RdpPicture, RdpQuality, RdpSecurity,
    };

    use super::*;

    const DEFAULT: &[u8] = include_bytes!("../tests/rdp/mstsc-default.rdp");
    const GATEWAY: &[u8] = include_bytes!("../tests/rdp/gateway-custom-effects.rdp");
    const UTF16: &[u8] = include_bytes!("../tests/rdp/utf16-bom.rdp");
    const MALFORMED: &[u8] = include_bytes!("../tests/rdp/malformed.rdp");

    fn noted(import: &RdpImport, text: &str) -> bool {
        import.notes.iter().any(|note| note.contains(text))
    }

    #[test]
    fn an_mstsc_default_save_maps_what_rmx_has() {
        let import = import("server01", DEFAULT).unwrap();
        let draft = &import.draft;
        assert_eq!(draft.protocol, Protocol::Rdp);
        assert_eq!(draft.name, "server01");
        assert_eq!(draft.host, "server01.corp.example");
        assert_eq!(draft.port, "3389");
        assert_eq!(draft.username, "alice");
        assert_eq!(draft.domain, "CORP");
        assert!(draft.password.is_empty());
        assert!(!draft.inherit, "a file with its own user keeps it");
        assert_eq!(draft.rdp_security, RdpSecurity::Negotiate);
        assert!(draft.follow_size);
        assert!(draft.rdp.clipboard);
        assert!(draft.rdp.start_program.is_empty());
        // Autodetected link and no font smoothing is no preset.
        assert_eq!(draft.rdp_quality, RdpQuality::Custom);
        assert_eq!(
            draft.rdp.picture,
            RdpPicture {
                color_depth: RdpColorDepth::Bits32,
                connection: RdpConnection::Autodetect,
                effects: RdpEffects {
                    wallpaper: true,
                    full_window_drag: false,
                    menu_animations: false,
                    themes: true,
                    cursor_shadow: true,
                    font_smoothing: false,
                    desktop_composition: false,
                },
            }
        );
        // An unused gateway is not worth a word of its own, but its settings are still named.
        assert!(!noted(&import, "RD Gateway"));
        for key in ["redirectprinters", "audiomode", "gatewayusagemethod", "use multimon"] {
            assert!(noted(&import, key), "{key} is not in {:?}", import.notes);
        }
        for key in ["full address", "session bpp", "redirectclipboard", "username"] {
            assert!(!noted(&import, key), "{key} is mapped, yet in {:?}", import.notes);
        }
    }

    #[test]
    fn a_gateway_and_custom_effects_map_or_are_named() {
        let import = import("jump", GATEWAY).unwrap();
        let draft = &import.draft;
        assert_eq!(draft.host, "10.20.30.40");
        assert_eq!(draft.port, "3390");
        assert_eq!(draft.username, "bob@corp.example");
        assert_eq!(draft.domain, "");
        assert_eq!(draft.rdp_security, RdpSecurity::Tls);
        assert!(!draft.follow_size);
        assert!(!draft.rdp.clipboard);
        assert_eq!(draft.rdp.keyboard_layout, "0x0407");
        assert_eq!(draft.rdp.start_program, r"C:\Tools\console.exe");
        assert_eq!(draft.rdp.work_dir, r"C:\Tools");
        assert_eq!(draft.rdp.scale, 150);
        assert_eq!(draft.rdp_quality, RdpQuality::Custom);
        assert_eq!(draft.rdp.picture.color_depth, RdpColorDepth::Bits16);
        assert_eq!(draft.rdp.picture.connection, RdpConnection::BroadbandLow);
        let effects = draft.rdp.picture.effects;
        assert!(!effects.wallpaper && !effects.full_window_drag && !effects.menu_animations);
        assert!(effects.themes && effects.font_smoothing && !effects.desktop_composition);
        // The password is the saver's, sealed with DPAPI: never taken.
        assert!(draft.password.is_empty());
        assert!(noted(&import, "password"));
        assert!(noted(&import, "RD Gateway gateway.corp.example"));
        assert!(noted(&import, "gatewayhostname"));
        // A size rmx would not keep is said.
        assert!(noted(&import, "1600"));
    }

    #[test]
    fn utf16_with_a_byte_order_mark_reads_as_mstsc_writes_it() {
        assert!(decode(UTF16).starts_with("screen mode id:i:2"));
        let import = import("werkstatt", UTF16).unwrap();
        let draft = &import.draft;
        assert_eq!(draft.host, "fd00::17");
        assert_eq!(draft.port, "3391");
        assert_eq!(draft.username, "j\u{fc}rgen");
        assert_eq!(draft.domain, "WERKSTATT");
        assert_eq!(draft.rdp_quality, RdpQuality::Best);
        assert!(draft.follow_size);
        assert!(import.notes.is_empty(), "{:?}", import.notes);
    }

    #[test]
    fn a_malformed_file_keeps_what_it_can_and_says_what_it_could_not() {
        let import = import("broken", MALFORMED).unwrap();
        assert_eq!(import.draft.host, "broken.example");
        assert_eq!(import.draft.username, "carol");
        assert_eq!(import.draft.rdp.picture, RdpPicture::default());
        assert!(noted(&import, "line 2"), "{:?}", import.notes);
        assert!(noted(&import, "line 3"), "{:?}", import.notes);
        assert!(noted(&import, "no name"), "{:?}", import.notes);
        assert!(noted(&import, "line 6"), "{:?}", import.notes);
        // A blank line is not an error.
        assert!(!noted(&import, "line 5"), "{:?}", import.notes);
    }

    #[test]
    fn a_file_with_no_address_or_no_text_is_not_a_connection() {
        assert!(import("x", b"username:s:dave\r\n").is_err());
        assert!(import("x", &[0x00, 0xd8, 0x00, 0xd8, 0xff]).is_err());
        assert!(import("x", b"").is_err());
    }

    #[test]
    fn addresses_take_their_port_from_the_file() {
        let host = |text: &str| {
            let draft = import("x", text.as_bytes()).unwrap().draft;
            (draft.host, draft.port)
        };
        assert_eq!(host("full address:s:box"), ("box".into(), "3389".into()));
        assert_eq!(host("full address:s:box:4000"), ("box".into(), "4000".into()));
        assert_eq!(host("full address:s:box\nserver port:i:4001"), ("box".into(), "4001".into()));
        assert_eq!(host("full address:s:[::1]"), ("::1".into(), "3389".into()));
        assert_eq!(host("full address:s:fe80::1"), ("fe80::1".into(), "3389".into()));
    }

    #[test]
    fn utf8_and_utf16_big_endian_decode_too() {
        assert_eq!(decode(b"\xef\xbb\xbfa:s:b"), "a:s:b");
        assert_eq!(decode(b"\xfe\xff\x00a\x00:"), "a:");
        // Without a mark, the zero bytes of ASCII in UTF-16 give it away.
        assert_eq!(decode(b"a\x00:\x00"), "a:");
    }

    #[test]
    fn an_exported_connection_imports_as_it_was() {
        let original = import("jump", GATEWAY).unwrap().draft;
        let bytes = export(&original);
        assert!(bytes.starts_with(&[0xff, 0xfe]), "UTF-16 LE with its mark, as mstsc reads it");
        let back = import("jump", &bytes).unwrap();
        let draft = back.draft;
        assert_eq!(draft.host, original.host);
        assert_eq!(draft.port, original.port);
        assert_eq!(draft.username, original.username);
        assert_eq!(draft.rdp_security, original.rdp_security);
        assert_eq!(draft.follow_size, original.follow_size);
        assert_eq!(draft.rdp_quality, original.rdp_quality);
        assert_eq!(draft.rdp, original.rdp);
        assert!(back.notes.iter().all(|note| !note.contains("password")), "{:?}", back.notes);

        let mut with_domain = original.clone();
        with_domain.username = "erin".into();
        with_domain.domain = "LAB".into();
        let back = import("x", &export(&with_domain)).unwrap().draft;
        assert_eq!((back.username.as_str(), back.domain.as_str()), ("erin", "LAB"));
    }

    #[test]
    fn folders_stand_for_the_rdp_files_in_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rdp"), DEFAULT).unwrap();
        std::fs::write(dir.path().join("B.RDP"), UTF16).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "full address:s:no").unwrap();
        let single = dir.path().join("single.rdp");
        std::fs::write(&single, GATEWAY).unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("deeper.rdp"), DEFAULT).unwrap();

        let files = read_paths(&[sub.clone(), dir.path().join("missing.rdp")]);
        let names: Vec<&str> = files.iter().map(|file| file.source.as_str()).collect();
        assert_eq!(names, ["deeper.rdp", "missing.rdp"]);
        assert!(files[0].result.is_ok());
        assert!(files[1].result.is_err());

        let files = read_paths(&[dir.path().to_owned()]);
        let names: Vec<&str> = files.iter().map(|file| file.source.as_str()).collect();
        assert_eq!(names, ["B.RDP", "a.rdp", "single.rdp"]);
        assert_eq!(files[0].result.as_ref().unwrap().draft.name, "B");
    }
}
