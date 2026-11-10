//! Which font terminals are drawn with.

use crossfont::Size;

/// A delta for a point in a 2 dimensional plane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delta<T: Default> {
    /// Horizontal change.
    pub x: T,
    /// Vertical change.
    pub y: T,
}

/// A family and optionally a style of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontDescription {
    pub family: String,
    pub style: Option<String>,
}

impl FontDescription {
    pub fn new(family: impl Into<String>) -> Self {
        Self { family: family.into(), style: None }
    }
}

/// A terminal font.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Font {
    /// Extra spacing per character.
    pub offset: Delta<i8>,
    /// Glyph offset within character cell.
    pub glyph_offset: Delta<i8>,
    /// Whether to use the built-in font for box drawing characters.
    pub builtin_box_drawing: bool,
    normal: FontDescription,
    size: Size,
}

impl Font {
    pub fn new(normal: FontDescription, size: Size) -> Self {
        Self {
            offset: Delta::default(),
            glyph_offset: Delta::default(),
            builtin_box_drawing: true,
            normal,
            size,
        }
    }

    /// Get a font clone with a size modification.
    pub fn with_size(self, size: Size) -> Font {
        Font { size, ..self }
    }

    #[inline]
    pub fn size(&self) -> Size {
        self.size
    }

    pub fn normal(&self) -> &FontDescription {
        &self.normal
    }

    pub fn bold(&self) -> FontDescription {
        self.normal.clone()
    }

    pub fn italic(&self) -> FontDescription {
        self.normal.clone()
    }

    pub fn bold_italic(&self) -> FontDescription {
        self.normal.clone()
    }
}

impl Default for Font {
    fn default() -> Self {
        Self::new(FontDescription::new(DEFAULT_FAMILY), Size::new(11.25))
    }
}

/// A fixed-width family every install of the platform has. `monospace` is a fontconfig alias:
/// DirectWrite and Core Text know no family by that name, and the terminal would fail to start.
#[cfg(windows)]
const DEFAULT_FAMILY: &str = "Consolas";
#[cfg(target_os = "macos")]
const DEFAULT_FAMILY: &str = "Menlo";
#[cfg(not(any(windows, target_os = "macos")))]
const DEFAULT_FAMILY: &str = "monospace";
