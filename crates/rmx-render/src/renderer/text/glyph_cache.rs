use std::collections::HashMap;
use std::time::{Duration, Instant};

use ahash::RandomState;
use crossfont::{
    Error as RasterizerError, FontDesc, FontKey, GlyphKey, Metrics, Rasterize, RasterizedGlyph,
    Rasterizer, Size, Slant, Style, Weight,
};
use log::{error, info};
use unicode_width::UnicodeWidthChar;

use crate::font::Delta;
use crate::font::{Font, FontDescription};
use crate::gl::types::*;

use super::builtin_font;

// tabbed: several fonts in one cache.
mod tabbed;

/// `LoadGlyph` allows for copying a rasterized glyph into graphics memory.
pub trait LoadGlyph {
    /// Load the rasterized glyph into GPU memory.
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph;

    /// Clear any state accumulated from previous loaded glyphs.
    ///
    /// This can, for instance, be used to reset the texture Atlas.
    fn clear(&mut self);
}

#[derive(Copy, Clone, Debug)]
pub struct Glyph {
    pub tex_id: GLuint,
    pub multicolor: bool,
    pub top: i16,
    pub left: i16,
    pub width: i16,
    pub height: i16,
    pub uv_bot: f32,
    pub uv_left: f32,
    pub uv_width: f32,
    pub uv_height: f32,
}

/// How long a frame may spend rasterizing glyphs it has not seen before. The glyphs over it are
/// left blank until a following frame. A frame always gets at least one glyph done.
const RASTER_BUDGET: Duration = Duration::from_micros(400);

/// Characters rasterized ahead of time that make the rasterizer open its fallback fonts, which is
/// by far the slowest thing about a new glyph.
const FALLBACK_PROBES: [char; 2] = ['\u{4e2d}', '\u{3042}'];

/// Glyphs rasterized ahead of time, waiting to go into the atlas.
pub struct RasterizedGlyphs(Vec<(GlyphKey, Result<RasterizedGlyph, RasterizerError>)>);

/// Naïve glyph cache.
///
/// Currently only keyed by `char`, and thus not possible to hold different
/// representations of the same code point.
pub struct GlyphCache {
    /// Cache of buffered glyphs.
    cache: HashMap<GlyphKey, Glyph, RandomState>,

    /// Rasterizer for loading new glyphs.
    rasterizer: Rasterizer,

    /// Regular font.
    pub font_key: FontKey,

    /// Bold font.
    pub bold_key: FontKey,

    /// Italic font.
    pub italic_key: FontKey,

    /// Bold italic font.
    pub bold_italic_key: FontKey,

    /// Font size.
    pub font_size: crossfont::Size,

    /// Font offset.
    font_offset: Delta<i8>,

    /// Glyph offset.
    glyph_offset: Delta<i8>,

    /// Font metrics.
    metrics: Metrics,

    /// Whether to use the built-in font for box drawing characters.
    builtin_box_drawing: bool,

    // tabbed: several fonts sharing the cache, one per pane text size.
    tabbed: tabbed::Fonts,

    /// Time spent rasterizing during this frame, when frames are being budgeted.
    raster_spent: Option<Duration>,

    /// A glyph drawing nothing, from the same atlas as the latest glyph to keep batches whole.
    blank: Option<Glyph>,

    /// Whether a glyph was left blank since this was last asked.
    deferred: bool,
}

impl GlyphCache {
    pub fn new(mut rasterizer: Rasterizer, font: &Font) -> Result<GlyphCache, crossfont::Error> {
        let (regular, bold, italic, bold_italic) = Self::compute_font_keys(font, &mut rasterizer)?;

        let metrics = GlyphCache::load_font_metrics(&mut rasterizer, font, regular)?;
        Ok(Self {
            cache: Default::default(),
            rasterizer,
            font_size: font.size(),
            font_key: regular,
            bold_key: bold,
            italic_key: italic,
            bold_italic_key: bold_italic,
            font_offset: font.offset,
            glyph_offset: font.glyph_offset,
            metrics,
            builtin_box_drawing: font.builtin_box_drawing,
            // tabbed: start with the window's font loaded.
            tabbed: tabbed::Fonts::new(font, [regular, bold, italic, bold_italic], metrics),
            raster_spent: None,
            blank: None,
            deferred: false,
        })
    }

    // Load font metrics and adjust for glyph offset.
    fn load_font_metrics(
        rasterizer: &mut Rasterizer,
        font: &Font,
        key: FontKey,
    ) -> Result<Metrics, crossfont::Error> {
        // Need to load at least one glyph for the face before calling metrics.
        // The glyph requested here ('m' at the time of writing) has no special
        // meaning.
        rasterizer.get_glyph(GlyphKey { font_key: key, character: 'm', size: font.size() })?;

        let mut metrics = rasterizer.metrics(key, font.size())?;
        metrics.strikeout_position += font.glyph_offset.y as f32;
        Ok(metrics)
    }

    fn load_glyphs_for_font<L: LoadGlyph>(&mut self, font: FontKey, loader: &mut L) {
        let size = self.font_size;

        // Cache all ascii characters.
        for i in 32u8..=126u8 {
            self.get(GlyphKey { font_key: font, character: i as char, size }, loader, true);
        }
    }

    /// Computes font keys for (Regular, Bold, Italic, Bold Italic).
    fn compute_font_keys(
        font: &Font,
        rasterizer: &mut Rasterizer,
    ) -> Result<(FontKey, FontKey, FontKey, FontKey), crossfont::Error> {
        let size = font.size();

        // Load regular font.
        let regular_desc = Self::make_desc(font.normal(), Slant::Normal, Weight::Normal);

        let regular = Self::load_regular_font(rasterizer, &regular_desc, size)?;

        // Helper to load a description if it is not the `regular_desc`.
        let mut load_or_regular = |desc: FontDesc| {
            if desc == regular_desc {
                regular
            } else {
                rasterizer.load_font(&desc, size).unwrap_or(regular)
            }
        };

        // Load bold font.
        let bold_desc = Self::make_desc(&font.bold(), Slant::Normal, Weight::Bold);

        let bold = load_or_regular(bold_desc);

        // Load italic font.
        let italic_desc = Self::make_desc(&font.italic(), Slant::Italic, Weight::Normal);

        let italic = load_or_regular(italic_desc);

        // Load bold italic font.
        let bold_italic_desc = Self::make_desc(&font.bold_italic(), Slant::Italic, Weight::Bold);

        let bold_italic = load_or_regular(bold_italic_desc);

        Ok((regular, bold, italic, bold_italic))
    }

    fn load_regular_font(
        rasterizer: &mut Rasterizer,
        description: &FontDesc,
        size: Size,
    ) -> Result<FontKey, crossfont::Error> {
        match rasterizer.load_font(description, size) {
            Ok(font) => Ok(font),
            Err(err) => {
                error!("{err}");

                let fallback_desc =
                    Self::make_desc(Font::default().normal(), Slant::Normal, Weight::Normal);
                rasterizer.load_font(&fallback_desc, size)
            },
        }
    }

    fn make_desc(desc: &FontDescription, slant: Slant, weight: Weight) -> FontDesc {
        let style = if let Some(ref spec) = desc.style {
            Style::Specific(spec.to_owned())
        } else {
            Style::Description { slant, weight }
        };
        FontDesc::new(desc.family.clone(), style)
    }

    /// Get a glyph from the font.
    ///
    /// If the glyph has never been loaded before, it will be rasterized and inserted into the
    /// cache.
    ///
    /// # Errors
    ///
    /// This will fail when the glyph could not be rasterized. Usually this is due to the glyph
    /// not being present in any font.
    pub fn get<L>(&mut self, glyph_key: GlyphKey, loader: &mut L, show_missing: bool) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        // Try to load glyph from cache.
        if let Some(glyph) = self.cache.get(&glyph_key) {
            return *glyph;
        };

        // Over budget, what is new waits. The glyph that took the budget has been drawn.
        if let (Some(spent), Some(blank)) = (self.raster_spent, self.blank)
            && spent >= RASTER_BUDGET
        {
            self.deferred = true;
            return blank;
        }

        let _probe = crate::probe::enter(crate::probe::Phase::Raster);
        let start = self.raster_spent.map(|_| Instant::now());
        let rasterized = self.rasterize(glyph_key);
        let glyph = self.install(glyph_key, rasterized, loader, show_missing);
        if let (Some(start), Some(spent)) = (start, &mut self.raster_spent) {
            *spent += start.elapsed();
        }
        glyph
    }

    /// Start budgeting the rasterizing of new glyphs for a frame.
    pub fn begin_frame(&mut self) {
        self.raster_spent = Some(Duration::ZERO);
    }

    /// Whether a glyph was left blank to stay within the budget, since this was last asked. The
    /// frame that drew it has to be followed by another.
    pub fn take_deferred(&mut self) -> bool {
        std::mem::take(&mut self.deferred)
    }

    /// Rasterize the glyph using the built-in font for special characters or the user's font for
    /// everything else. Touches neither the cache nor GL, so it can run ahead on another thread.
    fn rasterize(&mut self, glyph_key: GlyphKey) -> Result<RasterizedGlyph, RasterizerError> {
        self.builtin_box_drawing
            .then(|| {
                builtin_font::builtin_glyph(
                    glyph_key.character,
                    &self.metrics,
                    &self.font_offset,
                    &self.glyph_offset,
                )
            })
            .flatten()
            .map_or_else(|| self.rasterizer.get_glyph(glyph_key), Ok)
    }

    /// Put a rasterized glyph into the atlas and the cache.
    fn install<L>(
        &mut self,
        glyph_key: GlyphKey,
        rasterized: Result<RasterizedGlyph, RasterizerError>,
        loader: &mut L,
        show_missing: bool,
    ) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        let glyph = match rasterized {
            Ok(rasterized) => self.load_glyph(loader, rasterized),
            // Load fallback glyph.
            Err(RasterizerError::MissingGlyph(rasterized)) if show_missing => {
                // Use `\0` as "missing" glyph to cache it only once.
                let missing_key = GlyphKey { character: '\0', ..glyph_key };
                if let Some(glyph) = self.cache.get(&missing_key) {
                    *glyph
                } else {
                    // If no missing glyph was loaded yet, insert it as `\0`.
                    let glyph = self.load_glyph(loader, rasterized);
                    self.cache.insert(missing_key, glyph);

                    glyph
                }
            },
            Err(_) => self.load_glyph(loader, Default::default()),
        };

        self.blank = Some(Glyph { width: 0, height: 0, ..glyph });

        // Cache rasterized glyph.
        *self.cache.entry(glyph_key).or_insert(glyph)
    }

    /// Rasterize what [`Self::load_common_glyphs`] would load, without needing GL.
    pub fn rasterize_common(&mut self) -> RasterizedGlyphs {
        let size = self.font_size;
        let fonts = [self.font_key, self.bold_key, self.italic_key, self.bold_italic_key];
        let glyphs = fonts.into_iter().flat_map(|font_key| {
            let ascii = (32u8..=126).map(char::from);
            let characters = ascii.chain(FALLBACK_PROBES);
            characters.map(move |character| GlyphKey { font_key, character, size })
        });
        RasterizedGlyphs(glyphs.map(|key| (key, self.rasterize(key))).collect())
    }

    /// Load what [`Self::rasterize_common`] rasterized into the atlas.
    pub fn load_rasterized<L: LoadGlyph>(&mut self, glyphs: RasterizedGlyphs, loader: &mut L) {
        for (key, rasterized) in glyphs.0 {
            self.install(key, rasterized, loader, true);
        }
    }

    /// Load glyph into the atlas.
    ///
    /// This will apply all transforms defined for the glyph cache to the rasterized glyph before
    pub fn load_glyph<L>(&self, loader: &mut L, mut glyph: RasterizedGlyph) -> Glyph
    where
        L: LoadGlyph + ?Sized,
    {
        glyph.left += i32::from(self.glyph_offset.x);
        glyph.top += i32::from(self.glyph_offset.y);
        glyph.top -= self.metrics.descent as i32;

        // The metrics of zero-width characters are based on rendering
        // the character after the current cell, with the anchor at the
        // right side of the preceding character. Since we render the
        // zero-width characters inside the preceding character, the
        // anchor has been moved to the right by one cell.
        if glyph.character.width() == Some(0) {
            glyph.left += self.metrics.average_advance as i32;
        }

        // Add glyph to cache.
        loader.load_glyph(&glyph)
    }

    /// Reset currently cached data in both GL and the registry to default state.
    pub fn reset_glyph_cache<L: LoadGlyph>(&mut self, loader: &mut L) {
        loader.clear();
        self.cache = Default::default();
        // tabbed: forget the other fonts along with their glyphs.
        self.tabbed.reset();

        self.load_common_glyphs(loader);
    }

    /// Update the inner font size.
    ///
    /// NOTE: To reload the renderers's fonts [`Self::reset_glyph_cache`] should be called
    /// afterwards.
    pub fn update_font_size(&mut self, font: &Font) -> Result<(), crossfont::Error> {
        // Update dpi scaling.
        self.font_offset = font.offset;
        self.glyph_offset = font.glyph_offset;

        // Recompute font keys.
        let (regular, bold, italic, bold_italic) =
            Self::compute_font_keys(font, &mut self.rasterizer)?;

        let metrics = GlyphCache::load_font_metrics(&mut self.rasterizer, font, regular)?;

        info!("Font size changed to {:?} px", font.size().as_px());

        self.font_size = font.size();
        self.font_key = regular;
        self.bold_key = bold;
        self.italic_key = italic;
        self.bold_italic_key = bold_italic;
        self.metrics = metrics;
        self.builtin_box_drawing = font.builtin_box_drawing;

        Ok(())
    }

    pub fn font_metrics(&self) -> crossfont::Metrics {
        self.metrics
    }

    /// Prefetch glyphs that are almost guaranteed to be loaded anyways.
    pub fn load_common_glyphs<L: LoadGlyph>(&mut self, loader: &mut L) {
        self.load_glyphs_for_font(self.font_key, loader);
        self.load_glyphs_for_font(self.bold_key, loader);
        self.load_glyphs_for_font(self.italic_key, loader);
        self.load_glyphs_for_font(self.bold_italic_key, loader);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default family is one the platform's font system can find; without it the terminal,
    /// and with it the application, does not start.
    #[test]
    fn default_font_loads() {
        let rasterizer = Rasterizer::new().expect("rasterizer");
        let cache = GlyphCache::new(rasterizer, &Font::default()).expect("default font");
        assert!(cache.font_metrics().average_advance > 0.);
    }
}
