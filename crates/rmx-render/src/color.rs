//! Terminal colors.

use std::ops::{Add, Deref, Mul};

use alacritty_terminal::vte::ansi::Rgb as VteRgb;

#[derive(Debug, Eq, PartialEq, Copy, Clone, Default)]
pub struct Rgb(pub VteRgb);

impl Rgb {
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(VteRgb { r, g, b })
    }

    #[inline]
    pub fn as_tuple(self) -> (u8, u8, u8) {
        (self.0.r, self.0.g, self.0.b)
    }
}

impl From<VteRgb> for Rgb {
    fn from(value: VteRgb) -> Self {
        Self(value)
    }
}

impl Deref for Rgb {
    type Target = VteRgb;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Mul<f32> for Rgb {
    type Output = Rgb;

    fn mul(self, rhs: f32) -> Self::Output {
        Rgb(self.0 * rhs)
    }
}

impl Add<Rgb> for Rgb {
    type Output = Rgb;

    fn add(self, rhs: Rgb) -> Self::Output {
        Rgb(self.0 + rhs.0)
    }
}
