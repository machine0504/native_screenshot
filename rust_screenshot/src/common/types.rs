//! Cross-platform value types.
//!
//! All coordinates are in **physical pixels** of the virtual desktop, with the
//! origin at the top-left of the virtual desktop (which may be negative on
//! multi-monitor setups; capture re-bases everything to (0,0)).

/// A captured, frozen snapshot of the whole virtual desktop.
pub struct Snapshot {
    /// Tightly packed RGBA (8-bit) pixels, row-major, `width * height * 4`.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Virtual-desktop origin in OS screen coordinates (top-left).
    pub origin_x: i32,
    pub origin_y: i32,
}

impl Snapshot {
    #[inline]
    pub fn pixel(&self, x: i32, y: i32) -> (u8, u8, u8, u8) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return (0, 0, 0, 255);
        }
        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        (
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        )
    }
}

/// An axis-aligned rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

    pub fn from_points(ax: f32, ay: f32, bx: f32, by: f32) -> Self {
        let x = ax.min(bx);
        let y = ay.min(by);
        Self {
            x,
            y,
            w: (ax - bx).abs(),
            h: (ay - by).abs(),
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center_x(&self) -> f32 {
        self.x + self.w * 0.5
    }
    pub fn center_y(&self) -> f32 {
        self.y + self.h * 0.5
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }

    /// Clamp this rect to lie within `bounds` (keeping size where possible).
    #[allow(dead_code)]
    pub fn clamp_inside(&self, bounds: Rect) -> Rect {
        let x = self.x.clamp(bounds.x, (bounds.right() - self.w).max(bounds.x));
        let y = self.y.clamp(bounds.y, (bounds.bottom() - self.h).max(bounds.y));
        Rect::new(x, y, self.w, self.h)
    }
}
