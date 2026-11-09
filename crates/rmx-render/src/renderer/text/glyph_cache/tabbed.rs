//! Several fonts sharing one [`GlyphCache`] and its atlas, for panes drawn at their own text size.
//!
//! Glyphs are keyed by face, size and `char`, so several fonts share the cache and its atlas;
//! [`GlyphCache::select`] picks the one the next glyphs are drawn with.

use crossfont::{FontKey, Metrics, Rasterizer};
use log::{error, info};

use super::GlyphCache;
use crate::font::Font;

/// Fonts kept loaded at once before a font change clears the atlas, which bounds its growth.
const MAX_FACES: usize = 8;

/// Fonts loaded at once before the atlas is cleared regardless, far more than one frame draws.
const MAX_LOADED: usize = 4 * MAX_FACES;

/// The font keys and metrics of one [`Font`].
#[derive(Copy, Clone, Debug)]
struct Faces {
    regular: FontKey,
    bold: FontKey,
    italic: FontKey,
    bold_italic: FontKey,
    metrics: Metrics,
}

/// The fonts loaded into a [`GlyphCache`].
pub(super) struct Fonts {
    /// Every font loaded since the atlas was last cleared; the selected one is at `active`.
    faces: Vec<(Font, Faces)>,
    active: usize,

    /// Fonts that could not be loaded since the atlas was last cleared, not to be retried.
    failed: Vec<Font>,
}

impl Fonts {
    /// Only `font`, loaded with `[regular, bold, italic, bold_italic]` keys and `metrics`.
    pub(super) fn new(font: &Font, keys: [FontKey; 4], metrics: Metrics) -> Self {
        let [regular, bold, italic, bold_italic] = keys;
        let faces = Faces { regular, bold, italic, bold_italic, metrics };
        Self { faces: vec![(font.clone(), faces)], active: 0, failed: Vec::new() }
    }

    /// Forget every font but the selected one, as the atlas was cleared.
    pub(super) fn reset(&mut self) {
        self.faces.swap(0, self.active);
        self.faces.truncate(1);
        self.active = 0;
        self.failed.clear();
    }
}

impl GlyphCache {
    /// Load the font keys and metrics of `font`.
    fn load_faces(rasterizer: &mut Rasterizer, font: &Font) -> Result<Faces, crossfont::Error> {
        let (regular, bold, italic, bold_italic) = Self::compute_font_keys(font, rasterizer)?;
        let metrics = Self::load_font_metrics(rasterizer, font, regular)?;
        Ok(Faces { regular, bold, italic, bold_italic, metrics })
    }

    /// The font the next glyphs are drawn with.
    pub fn font(&self) -> &Font {
        &self.tabbed.faces[self.tabbed.active].0
    }

    /// Draw the next glyphs with `font`, loading it unless it was used before.
    ///
    /// Returns the index of the font selected before, for [`Self::select_index`], or `None` when
    /// `font` cannot be loaded (logged once) and the selection is unchanged.
    ///
    /// The atlas is kept: glyphs of another size or face have their own keys. Fonts that differ
    /// only in `offset`, `glyph_offset` or `builtin_box_drawing` share keys, so the glyphs of
    /// whichever is drawn first at a size are used for all of them until the atlas is cleared.
    pub fn select(&mut self, font: &Font) -> Option<usize> {
        let previous = self.tabbed.active;
        if self.font() == font {
            return Some(previous);
        }

        let index = match faces_index(&self.tabbed.faces, font) {
            Some(index) => index,
            None if self.tabbed.failed.contains(font) => return None,
            None => match Self::load_faces(&mut self.rasterizer, font) {
                Ok(faces) => {
                    self.tabbed.faces.push((font.clone(), faces));
                    self.tabbed.faces.len() - 1
                },
                Err(err) => {
                    error!("Could not load font {:?}: {err}", font.normal().family);
                    self.tabbed.failed.push(font.clone());
                    return None;
                },
            },
        };

        self.select_index(index);
        Some(previous)
    }

    /// Draw the next glyphs with the font at `index`, as returned by [`Self::select`].
    pub fn select_index(&mut self, index: usize) {
        let (font, faces) = &self.tabbed.faces[index];
        self.font_size = font.size();
        self.font_key = faces.regular;
        self.bold_key = faces.bold;
        self.italic_key = faces.italic;
        self.bold_italic_key = faces.bold_italic;
        self.metrics = faces.metrics;
        self.font_offset = font.offset;
        self.glyph_offset = font.glyph_offset;
        self.builtin_box_drawing = font.builtin_box_drawing;
        self.tabbed.active = index;
    }

    /// Whether so many fonts are loaded that the atlas should be cleared before the next frame.
    pub fn full(&self) -> bool {
        self.tabbed.faces.len() > MAX_LOADED
    }

    /// Change the window's font.
    ///
    /// Returns whether the cached glyphs must be dropped with [`Self::reset_glyph_cache`]: when
    /// they were drawn with other offsets, or when too many fonts were loaded since the last reset.
    pub fn update_font(&mut self, font: &Font) -> bool {
        let clear = needs_clear(self.font(), font)
            || self
                .tabbed
                .faces
                .iter()
                .any(|(face, _)| face.size() == font.size() && needs_clear(face, font));
        if self.select(font).is_none() {
            return false;
        }

        info!("Font size changed to {:?} px", font.size().as_px());

        clear || self.tabbed.faces.len() > MAX_FACES
    }
}

/// The index of `font` in `faces`.
fn faces_index<T>(faces: &[(Font, T)], font: &Font) -> Option<usize> {
    faces.iter().position(|(face, _)| face == font)
}

/// Whether glyphs cached for `old` would be wrong for `new` under the same keys.
fn needs_clear(old: &Font, new: &Font) -> bool {
    old.offset != new.offset
        || old.glyph_offset != new.glyph_offset
        || old.builtin_box_drawing != new.builtin_box_drawing
}

#[cfg(test)]
mod tests {
    use crossfont::Size;

    use super::*;
    use crate::font::Delta;

    fn font(size: f32) -> Font {
        Font::default().with_size(Size::new(size))
    }

    #[test]
    fn faces_index_matches_whole_font() {
        let faces = [(font(11.), ()), (font(14.), ())];
        assert_eq!(faces_index(&faces, &font(14.)), Some(1));
        assert_eq!(faces_index(&faces, &font(12.)), None);

        let mut offset = font(11.);
        offset.glyph_offset = Delta { x: 1, y: 0 };
        assert_eq!(faces_index(&faces, &offset), None);
    }

    #[test]
    fn needs_clear_only_for_baked_settings() {
        assert!(!needs_clear(&font(11.), &font(11.)));
        assert!(!needs_clear(&font(11.), &font(22.)));

        let family =
            Font::new(crate::font::FontDescription::new("Other Mono"), Font::default().size());
        assert!(!needs_clear(&Font::default(), &family));

        let mut glyph_offset = font(11.);
        glyph_offset.glyph_offset = Delta { x: 0, y: 2 };
        assert!(needs_clear(&font(11.), &glyph_offset));

        let mut offset = font(11.);
        offset.offset = Delta { x: 1, y: 0 };
        assert!(needs_clear(&font(11.), &offset));

        let mut builtin = font(11.);
        builtin.builtin_box_drawing = false;
        assert!(needs_clear(&font(11.), &builtin));
    }
}
