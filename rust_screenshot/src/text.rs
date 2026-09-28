//! Text rendering: load a system font and fill glyph outlines with tiny-skia.

use ab_glyph::{Font, FontVec, OutlineCurve, Point as GPoint};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

pub struct TextRenderer {
    font: FontVec,
    upem: f32,
}

impl TextRenderer {
    pub fn load() -> Option<TextRenderer> {
        for (path, index) in font_candidates() {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = FontVec::try_from_vec_and_index(bytes, index) {
                    let upem = font.units_per_em().unwrap_or(1000.0);
                    return Some(TextRenderer { font, upem });
                }
            }
        }
        None
    }

    fn scale(&self, px: f32) -> f32 {
        px / self.upem
    }

    pub fn ascent(&self, px: f32) -> f32 {
        self.font.ascent_unscaled() * self.scale(px)
    }

    #[allow(dead_code)]
    pub fn line_height(&self, px: f32) -> f32 {
        (self.font.ascent_unscaled() - self.font.descent_unscaled() + self.font.line_gap_unscaled())
            * self.scale(px)
    }

    /// Total advance width of `text` at `px`.
    pub fn measure(&self, text: &str, px: f32) -> f32 {
        let s = self.scale(px);
        text.chars()
            .map(|ch| self.font.h_advance_unscaled(self.font.glyph_id(ch)) * s)
            .sum()
    }

    /// Draw `text` with its top-left at (x, y_top).
    pub fn draw(&self, pm: &mut Pixmap, text: &str, x: f32, y_top: f32, px: f32, rgba: [u8; 4]) {
        self.draw_transform(pm, text, x, y_top, px, rgba, Transform::identity());
    }

    /// Draw `text` with its top-left at (x, y_top), applying `transform` to every
    /// glyph (used to rotate a text block around its center).
    pub fn draw_transform(
        &self,
        pm: &mut Pixmap,
        text: &str,
        x: f32,
        y_top: f32,
        px: f32,
        rgba: [u8; 4],
        transform: Transform,
    ) {
        let s = self.scale(px);
        let baseline = y_top + self.ascent(px);
        let mut pen_x = x;

        let mut paint = Paint::default();
        paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
        paint.anti_alias = true;

        for ch in text.chars() {
            let id = self.font.glyph_id(ch);
            if let Some(outline) = self.font.outline(id) {
                let map = |p: GPoint| (pen_x + p.x * s, baseline - p.y * s);
                let mut pb = PathBuilder::new();
                let mut started = false;
                let mut last: Option<GPoint> = None;

                for curve in &outline.curves {
                    let (start, end): (GPoint, GPoint) = match curve {
                        OutlineCurve::Line(a, b) => (*a, *b),
                        OutlineCurve::Quad(a, _, b) => (*a, *b),
                        OutlineCurve::Cubic(a, _, _, b) => (*a, *b),
                    };
                    if last != Some(start) {
                        if started {
                            pb.close();
                        }
                        let (mx, my) = map(start);
                        pb.move_to(mx, my);
                    }
                    match curve {
                        OutlineCurve::Line(_, b) => {
                            let (bx, by) = map(*b);
                            pb.line_to(bx, by);
                        }
                        OutlineCurve::Quad(_, c, b) => {
                            let (cx, cy) = map(*c);
                            let (bx, by) = map(*b);
                            pb.quad_to(cx, cy, bx, by);
                        }
                        OutlineCurve::Cubic(_, c1, c2, b) => {
                            let (c1x, c1y) = map(*c1);
                            let (c2x, c2y) = map(*c2);
                            let (bx, by) = map(*b);
                            pb.cubic_to(c1x, c1y, c2x, c2y, bx, by);
                        }
                    }
                    last = Some(end);
                    started = true;
                }
                if started {
                    pb.close();
                    if let Some(path) = pb.finish() {
                        pm.fill_path(&path, &paint, FillRule::Winding, transform, None);
                    }
                }
            }
            pen_x += self.font.h_advance_unscaled(id) * s;
        }
    }
}

#[cfg(target_os = "windows")]
fn font_candidates() -> Vec<(&'static str, u32)> {
    vec![
        ("C:\\Windows\\Fonts\\msyh.ttc", 0),
        ("C:\\Windows\\Fonts\\msyh.ttf", 0),
        ("C:\\Windows\\Fonts\\segoeui.ttf", 0),
        ("C:\\Windows\\Fonts\\arial.ttf", 0),
    ]
}

#[cfg(target_os = "macos")]
fn font_candidates() -> Vec<(&'static str, u32)> {
    vec![
        ("/System/Library/Fonts/PingFang.ttc", 0),
        ("/System/Library/Fonts/STHeiti Light.ttc", 0),
        ("/System/Library/Fonts/Helvetica.ttc", 0),
        ("/Library/Fonts/Arial.ttf", 0),
    ]
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn font_candidates() -> Vec<(&'static str, u32)> {
    vec![
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
        ("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc", 0),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
        ("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf", 0),
    ]
}
