//! A rectangle in window pixels.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2., self.y + self.h / 2.)
    }

    pub fn contains(&self, point: (f64, f64)) -> bool {
        let (x, y) = (point.0 as f32, point.1 as f32);
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn inset(self, by: f32) -> Self {
        Self::new(self.x + by, self.y + by, self.w - 2. * by, self.h - 2. * by)
    }

    /// A square of `size` centered in this rectangle.
    pub fn square(self, size: f32) -> Self {
        let (x, y) = self.center();
        Self::new((x - size / 2.).round(), (y - size / 2.).round(), size, size)
    }
}
