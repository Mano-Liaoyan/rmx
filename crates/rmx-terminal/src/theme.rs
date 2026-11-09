//! Terminal colors.

use std::ops::{Index, IndexMut};

use alacritty_terminal::term::color::COUNT;
use alacritty_terminal::vte::ansi::NamedColor;
use rmx_render::Rgb;

/// Factor for automatic computation of dim colors.
pub const DIM_FACTOR: f32 = 0.66;

/// Colors a terminal is drawn with.
#[derive(Copy, Clone)]
pub struct Theme {
    colors: [Rgb; COUNT],
    pub selection_fg: Rgb,
    pub selection_bg: Rgb,
}

impl Theme {
    /// The dark theme, on the interface's neutral palette.
    pub fn dark() -> Self {
        let normal = [
            (0x1b, 0x1b, 0x1e),
            (0xf3, 0x62, 0x5f),
            (0x4a, 0xde, 0x80),
            (0xfb, 0xbf, 0x24),
            (0x60, 0xa5, 0xfa),
            (0xc0, 0x84, 0xfc),
            (0x22, 0xd3, 0xee),
            (0xd7, 0xd7, 0xdb),
        ];
        let bright = [
            (0x6e, 0x6e, 0x72),
            (0xfc, 0xa5, 0xa5),
            (0x86, 0xef, 0xac),
            (0xfd, 0xe0, 0x47),
            (0x93, 0xc5, 0xfd),
            (0xd8, 0xb4, 0xfe),
            (0x67, 0xe8, 0xf9),
            (0xfa, 0xfa, 0xfa),
        ];
        Self::new(normal, bright, Rgb::new(0xd7, 0xd7, 0xdb), Rgb::new(0x13, 0x14, 0x16))
    }

    /// The light theme, on the interface's neutral palette. The colors are the darker status
    /// shades, as the lighter ones of [`Self::dark`] wash out on white.
    pub fn light() -> Self {
        let normal = [
            (0x0a, 0x0a, 0x0a),
            (0xdc, 0x26, 0x26),
            (0x16, 0xa3, 0x4a),
            (0xd9, 0x77, 0x06),
            (0x25, 0x63, 0xeb),
            (0x93, 0x33, 0xea),
            (0x08, 0x91, 0xb2),
            (0xa3, 0xa3, 0xa3),
        ];
        let bright = [
            (0x73, 0x73, 0x73),
            (0xb9, 0x1c, 0x1c),
            (0x15, 0x80, 0x3d),
            (0xb4, 0x53, 0x09),
            (0x1d, 0x4e, 0xd8),
            (0x7e, 0x22, 0xce),
            (0x0e, 0x74, 0x90),
            (0x52, 0x52, 0x52),
        ];
        Self::new(normal, bright, Rgb::new(0x0a, 0x0a, 0x0a), Rgb::new(0xff, 0xff, 0xff))
    }

    fn new(normal: [(u8, u8, u8); 8], bright: [(u8, u8, u8); 8], fg: Rgb, bg: Rgb) -> Self {
        let mut colors = [Rgb::default(); COUNT];
        let rgb = |(r, g, b)| Rgb::new(r, g, b);

        for i in 0..8 {
            colors[i] = rgb(normal[i]);
            colors[i + 8] = rgb(bright[i]);
            colors[NamedColor::DimBlack as usize + i] = rgb(normal[i]) * DIM_FACTOR;
        }

        // The 6x6x6 color cube.
        let mut index = 16;
        for r in 0..6 {
            for g in 0..6 {
                for b in 0..6 {
                    let step = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
                    colors[index] = Rgb::new(step(r), step(g), step(b));
                    index += 1;
                }
            }
        }

        // The gray ramp.
        for i in 0..24 {
            let value = i as u8 * 10 + 8;
            colors[232 + i] = Rgb::new(value, value, value);
        }

        colors[NamedColor::Foreground as usize] = fg;
        colors[NamedColor::Background as usize] = bg;
        colors[NamedColor::BrightForeground as usize] = fg;
        colors[NamedColor::DimForeground as usize] = fg * DIM_FACTOR;
        colors[NamedColor::Cursor as usize] = fg;

        Self { colors, selection_fg: bg, selection_bg: fg }
    }

    pub fn background(&self) -> Rgb {
        self[NamedColor::Background as usize]
    }

    pub fn foreground(&self) -> Rgb {
        self[NamedColor::Foreground as usize]
    }
}

impl Index<usize> for Theme {
    type Output = Rgb;

    #[inline]
    fn index(&self, index: usize) -> &Rgb {
        &self.colors[index]
    }
}

impl IndexMut<usize> for Theme {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Rgb {
        &mut self.colors[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Rgb) -> f32 {
        let channel = |v: u8| {
            let v = f32::from(v) / 255.;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(color.0.r) + 0.7152 * channel(color.0.g) + 0.0722 * channel(color.0.b)
    }

    #[test]
    fn the_light_theme_is_readable_on_its_background() {
        let theme = Theme::light();
        let background = luminance(theme.background());
        assert!(background > 0.9, "the background is light");
        for index in 0..7 {
            let contrast = (background + 0.05) / (luminance(theme[index]) + 0.05);
            assert!(contrast >= 2.5, "color {index} has contrast {contrast}");
        }
    }

    #[test]
    fn selection_inverts_the_colors_of_either_theme() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_eq!(theme.selection_bg, theme.foreground());
            assert_eq!(theme.selection_fg, theme.background());
        }
    }
}
