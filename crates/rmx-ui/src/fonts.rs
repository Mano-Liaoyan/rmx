//! Embedded fonts, and the system CJK fallback.
//!
//! Geist (OFL) and Lucide (ISC) are subset and embedded; their licenses sit next to them in
//! `assets/`. The icon font also carries RustDesk's mark from Simple Icons, which is CC0. No CJK
//! font is embedded: [`CjkFallback`] finds one on a background thread and maps it, and egui picks
//! it up on a later frame, so the interface draws from the first frame on and the UI thread never
//! touches the disk.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::{fs, thread};

use egui::epaint::text::{
    FontData, FontDefinitions, FontInsert, FontPriority, FontTweak, InsertFontFamily, Tag,
    VariationCoords,
};
use egui::{Context, FontFamily, FontId};

const GEIST_REGULAR: &[u8] = include_bytes!("../assets/geist-regular-subset.ttf");
const GEIST_MEDIUM: &[u8] = include_bytes!("../assets/geist-medium-subset.ttf");
const GEIST_MONO: &[u8] = include_bytes!("../assets/geist-mono-subset.ttf");
const LUCIDE: &[u8] = include_bytes!("../assets/lucide-subset.ttf");

const MEDIUM: &str = "medium";
const ICONS: &str = "icons";

/// The family of medium-weight text.
pub fn medium_family() -> FontFamily {
    FontFamily::Name(MEDIUM.into())
}

/// The family holding the icon glyphs.
pub fn icon_family() -> FontFamily {
    FontFamily::Name(ICONS.into())
}

/// Regular text at `size`.
pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Medium-weight text at `size`.
pub fn medium(size: f32) -> FontId {
    FontId::new(size, medium_family())
}

/// Fixed-width text at `size`.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// An icon at `size`.
pub fn icon(size: f32) -> FontId {
    FontId::new(size, icon_family())
}

/// Install the embedded fonts on `ctx`, replacing egui's.
pub fn install(ctx: &Context) {
    let data = |bytes: &'static [u8]| Arc::new(FontData::from_static(bytes));
    let font_data = BTreeMap::from([
        ("geist".to_owned(), data(GEIST_REGULAR)),
        (MEDIUM.to_owned(), data(GEIST_MEDIUM)),
        ("geist-mono".to_owned(), data(GEIST_MONO)),
        (
            ICONS.to_owned(),
            Arc::new(FontData::from_static(LUCIDE).tweak(FontTweak {
                // Lucide draws on a 24-unit grid with a 2-unit margin; hinting would distort it.
                hinting: Some(false),
                subpixel_binning: Some(false),
                ..FontTweak::default()
            })),
        ),
    ]);
    let families = BTreeMap::from([
        (FontFamily::Proportional, vec!["geist".to_owned()]),
        (medium_family(), vec![MEDIUM.to_owned(), "geist".to_owned()]),
        (FontFamily::Monospace, vec!["geist-mono".to_owned(), "geist".to_owned()]),
        (icon_family(), vec![ICONS.to_owned()]),
    ]);
    ctx.set_fonts(FontDefinitions { font_data, families });
}

/// A system CJK font being located in the background.
pub struct CjkFallback(Option<Receiver<FontData>>);

impl CjkFallback {
    /// Start looking on a background thread.
    pub fn locate() -> Self {
        let (sender, receiver) = mpsc::channel();
        let spawned = thread::Builder::new().name("rmx-cjk-font".into()).spawn(move || {
            if let Some(font) = find_and_map() {
                let _ = sender.send(font);
            }
        });
        Self(spawned.ok().map(|_| receiver))
    }

    /// A fallback that never finds anything.
    pub fn none() -> Self {
        Self(None)
    }

    /// Whether a font is still on its way.
    pub fn pending(&self) -> bool {
        self.0.is_some()
    }

    /// Install the font if it has arrived; true on the frame it does, which needs a repaint.
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let Some(receiver) = &self.0 else { return false };
        match receiver.try_recv() {
            Ok(font) => {
                let families = [FontFamily::Proportional, medium_family(), FontFamily::Monospace]
                    .into_iter()
                    .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
                    .collect();
                ctx.add_font(FontInsert::new("cjk", font, families));
                self.0 = None;
                true
            },
            Err(TryRecvError::Disconnected) => {
                self.0 = None;
                false
            },
            Err(TryRecvError::Empty) => false,
        }
    }
}

/// Faces to prefer, best first. Simplified Chinese leads because it covers the most of what the
/// interface and hostnames show.
const PREFERRED: [&str; 7] = [
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Noto Sans SC",
    "Noto Sans CJK",
    "WenQuanYi Micro Hei",
    "WenQuanYi Zen Hei",
    "Droid Sans Fallback",
];

/// Files to accept when fontconfig is not available.
const KNOWN_FILES: [&str; 12] = [
    "NotoSansCJK-Regular.ttc",
    "NotoSansCJKsc-Regular.otf",
    "SourceHanSansSC-Regular.otf",
    "wqy-microhei.ttc",
    "wqy-zenhei.ttc",
    "DroidSansFallbackFull.ttf",
    // macOS.
    "PingFang.ttc",
    "Hiragino Sans GB.ttc",
    "STHeiti Medium.ttc",
    // Windows.
    "msyh.ttc",
    "Deng.ttf",
    "simhei.ttf",
];

fn find_and_map() -> Option<FontData> {
    let mut candidates = ask_fontconfig();
    candidates.extend(scan_font_dirs());
    let (blob, index) = first_usable(&candidates, map_file)?;
    let font = FontData::from_blob(blob, index);
    // A variable CJK face starts at its lightest; UI text wants the regular weight.
    let weighted = font.variation_axes().iter().any(|axis| axis.tag == Tag::new(b"wght"));
    if weighted {
        let coords = VariationCoords::new([(b"wght", 400.)]);
        return Some(font.tweak(FontTweak { coords, ..FontTweak::default() }));
    }
    Some(font)
}

type Bytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// The first of `candidates` that `map` can read and that is a font: egui aborts on a file it
/// cannot parse, and doing without CJK glyphs is never worth that.
fn first_usable(
    candidates: &[(String, u32)],
    map: impl Fn(&str) -> Option<Bytes>,
) -> Option<(Bytes, u32)> {
    candidates.iter().find_map(|(path, index)| {
        let blob = map(path)?;
        if !is_font((*blob).as_ref(), *index) {
            log::warn!("Skipping {path}: not a font this interface can draw with");
            return None;
        }
        log::debug!("CJK fallback: {path} face {index}");
        Some((blob, *index))
    })
}

/// Whether face `index` of `bytes` parses the way egui will parse it.
fn is_font(bytes: &[u8], index: u32) -> bool {
    skrifa::FontRef::from_index(bytes, index).is_ok()
}

/// Whether `path` is one of the kinds of file a font can be drawn from: fontconfig also lists
/// bitmap fonts, such as `.pcf.gz`.
fn is_outline_font(path: &str) -> bool {
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    ["ttf", "otf", "ttc"].iter().any(|known| extension.eq_ignore_ascii_case(known))
}

/// Ask `fc-list` for regular CJK faces, best first.
fn ask_fontconfig() -> Vec<(String, u32)> {
    let output = Command::new("fc-list")
        .args([":lang=zh-cn", "-f", "%{file}\t%{index}\t%{family}\t%{style}\n"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    output
        .map_or_else(|_| Vec::new(), |output| pick_faces(&String::from_utf8_lossy(&output.stdout)))
}

/// The usable faces of `fc-list` output, best first.
fn pick_faces(listing: &str) -> Vec<(String, u32)> {
    let rank = |family: &str| PREFERRED.iter().position(|name| family.contains(name));
    let mut faces: Vec<_> = listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (file, index, family, style) =
                (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
            if !is_outline_font(file) {
                return None;
            }
            // Variable fonts list one line per named instance, numbered in the high bits.
            let index = index.parse::<u32>().ok()?;
            let regular = style.split(',').any(|style| style == "Regular");
            Some((!regular, rank(family).unwrap_or(PREFERRED.len()), file, index & 0xffff))
        })
        .collect();
    faces.sort_by_key(|&(not_regular, rank, ..)| (not_regular, rank));
    faces.into_iter().map(|(.., file, index)| (file.to_owned(), index)).collect()
}

/// Look for a known file under the usual directories.
fn scan_font_dirs() -> Option<(String, u32)> {
    let mut stack = font_dirs();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.extend(path.to_str().map(str::to_owned));
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| KNOWN_FILES.contains(&name))
            {
                // Face 2 of Noto's collection is Simplified Chinese.
                let index = u32::from(path.to_str()?.contains("NotoSansCJK-Regular")) * 2;
                return Some((path.to_str()?.to_owned(), index));
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", windows)))]
fn font_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![
        "/usr/share/fonts".into(),
        "/usr/local/share/fonts".into(),
        format!("{home}/.local/share/fonts"),
        format!("{home}/.fonts"),
    ]
}

#[cfg(target_os = "macos")]
fn font_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![format!("{home}/Library/Fonts"), "/Library/Fonts".into(), "/System/Library/Fonts".into()]
}

#[cfg(windows)]
fn font_dirs() -> Vec<String> {
    let windows = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_owned());
    vec![format!("{windows}\\Fonts")]
}

/// A read-only mapping of a whole file. CJK fonts run to tens of megabytes, of which a session
/// touches a sliver: mapped, those pages are shared and cost only what is read.
#[cfg(unix)]
struct Mapped {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the mapping is immutable and lives until drop.
#[cfg(unix)]
unsafe impl Send for Mapped {}
#[cfg(unix)]
unsafe impl Sync for Mapped {}

#[cfg(unix)]
impl AsRef<[u8]> for Mapped {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `ptr` is a live mapping of `len` readable bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[cfg(unix)]
impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: this is the mapping made in `map_file`, unmapped once.
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}

#[cfg(unix)]
fn map_file(path: &str) -> Option<Arc<dyn AsRef<[u8]> + Send + Sync>> {
    use std::os::fd::AsRawFd;

    let file = fs::File::open(path).ok()?;
    let len = usize::try_from(file.metadata().ok()?.len()).ok().filter(|&len| len > 0)?;
    // SAFETY: a fresh private read-only mapping of an open file; the kernel checks the rest.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    (ptr != libc::MAP_FAILED).then(|| Arc::new(Mapped { ptr: ptr as *const u8, len }) as _)
}

#[cfg(not(unix))]
fn map_file(path: &str) -> Option<Arc<dyn AsRef<[u8]> + Send + Sync>> {
    Some(Arc::new(fs::read(path).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_preferred_regular_face() {
        let listing = "/a.ttc\t3\tNoto Sans CJK JP\tRegular\n/a.ttc\t2\tNoto Sans CJK SC,Noto \
                       Sans CJK SC Bold\tBold\n/a.ttc\t2\tNoto Sans CJK \
                       SC\tRegular\n/b.ttf\t0\tSome Font\tRegular\n";
        assert_eq!(pick_faces(listing).first(), Some(&("/a.ttc".to_owned(), 2)));
    }

    #[test]
    fn strips_the_named_instance_from_a_face_index() {
        assert_eq!(pick_faces("/v.ttf\t65537\tX\tRegular\n"), [("/v.ttf".to_owned(), 1)]);
    }

    #[test]
    fn only_outline_fonts_are_candidates() {
        let listing = "/usr/share/fonts/misc/18x18ja.pcf.gz\t0\tMisc \
                       Fixed\tRegular\n/fonts/a.OTF\t0\tSome Font\tRegular\n";
        assert_eq!(pick_faces(listing), [("/fonts/a.OTF".to_owned(), 0)]);
        assert!(pick_faces("/fonts/a.pcf\t0\tX\tRegular\n").is_empty());
    }

    #[test]
    fn a_file_that_does_not_parse_falls_through_to_the_next() {
        let candidates =
            [("broken".to_owned(), 0), ("missing".to_owned(), 0), ("good".to_owned(), 0)];
        let map = |path: &str| -> Option<Bytes> {
            match path {
                "broken" => Some(Arc::new(vec![0u8; 64])),
                "good" => Some(Arc::new(GEIST_REGULAR.to_vec())),
                _ => None,
            }
        };
        let (blob, index) = first_usable(&candidates, map).expect("the third one is a font");
        assert_eq!(((*blob).as_ref().len(), index), (GEIST_REGULAR.len(), 0));
        assert!(first_usable(&candidates[..2], map).is_none(), "no font is no failure");
    }
}
