//! Annotation model + tiny-skia drawing.

use crate::text::TextRenderer;
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, PathBuilder, Pattern, Pixmap, PixmapRef,
    SpreadMode, Stroke, Transform,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Rect,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Marker,
    Mosaic,
    Text,
    Serial,
    Eraser,
}

impl Tool {
    pub const ALL: [Tool; 10] = [
        Tool::Rect,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Line,
        Tool::Pen,
        Tool::Marker,
        Tool::Mosaic,
        Tool::Text,
        Tool::Serial,
        Tool::Eraser,
    ];
}

pub const PALETTE: [[u8; 4]; 8] = [
    [255, 59, 48, 255],   // red
    [255, 149, 0, 255],   // orange
    [255, 204, 0, 255],   // yellow
    [52, 199, 89, 255],   // green
    [0, 122, 255, 255],   // blue
    [175, 82, 222, 255],  // purple
    [255, 255, 255, 255], // white
    [0, 0, 0, 255],       // black
];

pub const WIDTHS: [f32; 3] = [3.0, 5.0, 8.0];

/// Eraser brush diameter (physical px) for a given base width. The eraser is a
/// chunkier brush than the pen so it feels like a real eraser.
pub fn eraser_diameter(base: f32) -> f32 {
    (base * 4.0).max(18.0)
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub tool: Tool,
    pub color: [u8; 4],
    pub width: f32,
    pub p0: (f32, f32),
    pub p1: (f32, f32),
    pub points: Vec<(f32, f32)>,
    /// Mosaic resolved cells: (x, y, w, h, rgba).
    pub cells: Vec<(f32, f32, f32, f32, [u8; 4])>,
    pub text: String,
    pub font_px: f32,
    /// Rotation in radians around the text box center (text tool only).
    pub angle: f32,
    pub serial: i32,
}

impl Annotation {
    pub fn new(tool: Tool, color: [u8; 4], width: f32) -> Self {
        Annotation {
            tool,
            color,
            width,
            p0: (0.0, 0.0),
            p1: (0.0, 0.0),
            points: Vec::new(),
            cells: Vec::new(),
            text: String::new(),
            font_px: 26.0,
            angle: 0.0,
            serial: 0,
        }
    }
}

/// Rotate point `p` by `angle` radians around `center`.
pub fn rotate_point(p: (f32, f32), center: (f32, f32), angle: f32) -> (f32, f32) {
    if angle == 0.0 {
        return p;
    }
    let (s, c) = angle.sin_cos();
    let dx = p.0 - center.0;
    let dy = p.1 - center.1;
    (center.0 + dx * c - dy * s, center.1 + dx * s + dy * c)
}

fn solid(rgba: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
    p.anti_alias = true;
    p
}

fn stroke(width: f32) -> Stroke {
    Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

pub fn rect_path(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x, y);
    pb.line_to(x + w, y);
    pb.line_to(x + w, y + h);
    pb.line_to(x, y + h);
    pb.close();
    pb.finish()
}

pub fn oval_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Option<tiny_skia::Path> {
    const K: f32 = 0.552_284_8;
    let (ox, oy) = (rx * K, ry * K);
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy - ry);
    pb.cubic_to(cx + ox, cy - ry, cx + rx, cy - oy, cx + rx, cy);
    pb.cubic_to(cx + rx, cy + oy, cx + ox, cy + ry, cx, cy + ry);
    pb.cubic_to(cx - ox, cy + ry, cx - rx, cy + oy, cx - rx, cy);
    pb.cubic_to(cx - rx, cy - oy, cx - ox, cy - ry, cx, cy - ry);
    pb.close();
    pb.finish()
}

fn line_path(a: (f32, f32), b: (f32, f32)) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    pb.finish()
}

fn polyline_path(points: &[(f32, f32)]) -> Option<tiny_skia::Path> {
    if points.len() < 2 {
        return None;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(points[0].0, points[0].1);
    for p in &points[1..] {
        pb.line_to(p.0, p.1);
    }
    pb.finish()
}

fn arrow_head_path(from: (f32, f32), to: (f32, f32), width: f32) -> Option<tiny_skia::Path> {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = (width * 4.0).max(14.0);
    let ang = 0.5_f32;
    let (ca, sa) = (ang.cos(), ang.sin());
    let p1 = (
        to.0 - head * (ux * ca - uy * sa),
        to.1 - head * (uy * ca + ux * sa),
    );
    let p2 = (
        to.0 - head * (ux * ca + uy * sa),
        to.1 - head * (uy * ca - ux * sa),
    );
    let mut pb = PathBuilder::new();
    pb.move_to(to.0, to.1);
    pb.line_to(p1.0, p1.1);
    pb.line_to(p2.0, p2.1);
    pb.close();
    pb.finish()
}

/// 箭杆终点：缩进到箭头三角底边附近，避免圆头描边顶出尖端形成「圆点」。
fn arrow_shaft_end(from: (f32, f32), to: (f32, f32), width: f32) -> (f32, f32) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return to;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = (width * 4.0).max(14.0);
    // 三角底边距尖端约 head·cos(0.5)；再略缩进，让圆头完全落在填充三角内。
    let inset = (head * 0.5_f32.cos()).min(len * 0.85);
    (to.0 - ux * inset, to.1 - uy * inset)
}

/// Draw one annotation onto `pm`, offsetting every coordinate by (ox, oy).
///
/// `snap` is the frozen, full-desktop snapshot in physical-pixel coordinates.
/// The eraser uses it as a fill pattern to restore the original pixels along the
/// brush path, which visually erases whatever annotations sit beneath it without
/// touching the rest of the drawing.
pub fn draw(
    pm: &mut Pixmap,
    a: &Annotation,
    text: Option<&TextRenderer>,
    snap: Option<PixmapRef>,
    ox: f32,
    oy: f32,
) {
    let tf = Transform::from_translate(ox, oy);
    let paint = solid(a.color);
    let st = stroke(a.width);

    match a.tool {
        Tool::Rect => {
            let r = norm(a.p0, a.p1);
            if let Some(p) = rect_path(r.0, r.1, r.2, r.3) {
                pm.stroke_path(&p, &paint, &st, tf, None);
            }
        }
        Tool::Ellipse => {
            let r = norm(a.p0, a.p1);
            if let Some(p) = oval_path(r.0 + r.2 / 2.0, r.1 + r.3 / 2.0, r.2 / 2.0, r.3 / 2.0) {
                pm.stroke_path(&p, &paint, &st, tf, None);
            }
        }
        Tool::Line => {
            if let Some(p) = line_path(a.p0, a.p1) {
                pm.stroke_path(&p, &paint, &st, tf, None);
            }
        }
        Tool::Arrow => {
            // 箭杆止于三角底边内侧，圆头不会顶出尖端。
            let shaft_end = arrow_shaft_end(a.p0, a.p1, a.width);
            if let Some(p) = line_path(a.p0, shaft_end) {
                pm.stroke_path(&p, &paint, &st, tf, None);
            }
            if let Some(p) = arrow_head_path(a.p0, a.p1, a.width) {
                pm.fill_path(&p, &paint, FillRule::Winding, tf, None);
            }
        }
        Tool::Pen => {
            if let Some(p) = polyline_path(&a.points) {
                pm.stroke_path(&p, &paint, &st, tf, None);
            }
        }
        Tool::Marker => {
            let hi = [a.color[0], a.color[1], a.color[2], 110];
            let mp = solid(hi);
            let ms = stroke((a.width * 3.0).max(14.0));
            if let Some(p) = polyline_path(&a.points) {
                pm.stroke_path(&p, &mp, &ms, tf, None);
            }
        }
        Tool::Mosaic => {
            for (cx, cy, cw, ch, col) in &a.cells {
                if let Some(rect) = tiny_skia::Rect::from_xywh(*cx, *cy, *cw, *ch) {
                    pm.fill_rect(rect, &solid(*col), tf, None);
                }
            }
        }
        Tool::Text => {
            if let Some(tr) = text {
                let lh = tr.line_height(a.font_px);
                // Rotate around the text box center so the whole block spins as one.
                let mut maxw = 0.0_f32;
                let mut nlines = 0usize;
                for line in a.text.split('\n') {
                    maxw = maxw.max(tr.measure(line, a.font_px));
                    nlines += 1;
                }
                let h = nlines.max(1) as f32 * lh;
                let cx = a.p0.0 + ox + maxw / 2.0;
                let cy = a.p0.1 + oy + h / 2.0;
                let tf = if a.angle != 0.0 {
                    Transform::from_rotate_at(a.angle.to_degrees(), cx, cy)
                } else {
                    Transform::identity()
                };
                for (i, line) in a.text.split('\n').enumerate() {
                    let ly = a.p0.1 + oy + i as f32 * lh;
                    // subtle dark halo for readability
                    tr.draw_transform(pm, line, a.p0.0 + ox + 1.0, ly + 1.0, a.font_px, [0, 0, 0, 130], tf);
                    tr.draw_transform(pm, line, a.p0.0 + ox, ly, a.font_px, a.color, tf);
                }
            }
        }
        Tool::Serial => {
            let d = (a.width * 5.0).max(26.0);
            let cx = a.p0.0;
            let cy = a.p0.1;
            if let Some(p) = oval_path(cx, cy, d / 2.0, d / 2.0) {
                pm.fill_path(&p, &paint, FillRule::Winding, tf, None);
                let ring = solid([255, 255, 255, 255]);
                pm.stroke_path(&p, &ring, &stroke(2.0), tf, None);
            }
            if let Some(tr) = text {
                let s = a.serial.to_string();
                let fpx = d * 0.6;
                let tw = tr.measure(&s, fpx);
                let tx = cx + ox - tw / 2.0;
                let ty = cy + oy - tr.ascent(fpx) / 2.0 - tr.ascent(fpx) * 0.08;
                tr.draw(pm, &s, tx, ty, fpx, [255, 255, 255, 255]);
            }
        }
        Tool::Eraser => {
            // Restore the original snapshot pixels along the brush path. Because
            // annotations are painted in list order, this only "erases" the
            // annotations that were drawn before this eraser stroke. The fill
            // pattern is sampled in snapshot coordinates and the shared `tf`
            // offset maps it onto the destination, so it lines up in both the
            // live overlay (ox=oy=0) and the cropped export (ox=-sel.x …).
            let snap = match snap {
                Some(s) => s,
                None => return,
            };
            if a.points.is_empty() {
                return;
            }
            let mut paint = Paint::default();
            paint.shader = Pattern::new(
                snap,
                SpreadMode::Pad,
                FilterQuality::Nearest,
                1.0,
                Transform::identity(),
            );
            paint.anti_alias = true;
            let d = eraser_diameter(a.width);
            if a.points.len() == 1 {
                // A single tap erases a round dab.
                let p = a.points[0];
                if let Some(path) = oval_path(p.0, p.1, d / 2.0, d / 2.0) {
                    pm.fill_path(&path, &paint, FillRule::Winding, tf, None);
                }
            } else if let Some(p) = polyline_path(&a.points) {
                pm.stroke_path(&p, &paint, &stroke(d), tf, None);
            }
        }
    }
}

/// Shift every coordinate of an annotation by (dx, dy). Used to drag a placed
/// annotation (text/shape/…) to a new position.
pub fn translate(a: &mut Annotation, dx: f32, dy: f32) {
    a.p0 = (a.p0.0 + dx, a.p0.1 + dy);
    a.p1 = (a.p1.0 + dx, a.p1.1 + dy);
    for p in &mut a.points {
        p.0 += dx;
        p.1 += dy;
    }
    for c in &mut a.cells {
        c.0 += dx;
        c.1 += dy;
    }
}

fn norm(a: (f32, f32), b: (f32, f32)) -> (f32, f32, f32, f32) {
    let x = a.0.min(b.0);
    let y = a.1.min(b.1);
    (x, y, (a.0 - b.0).abs(), (a.1 - b.1).abs())
}

/// Hit-test for the eraser (does point (x,y) touch this annotation?).
pub fn hit_test(a: &Annotation, x: f32, y: f32) -> bool {
    let tol = a.width + 6.0;
    match a.tool {
        Tool::Rect => {
            let r = norm(a.p0, a.p1);
            let on_v = (x >= r.0 - tol && x <= r.0 + tol) || (x >= r.0 + r.2 - tol && x <= r.0 + r.2 + tol);
            let on_h = (y >= r.1 - tol && y <= r.1 + tol) || (y >= r.1 + r.3 - tol && y <= r.1 + r.3 + tol);
            let inside_x = x >= r.0 - tol && x <= r.0 + r.2 + tol;
            let inside_y = y >= r.1 - tol && y <= r.1 + r.3 + tol;
            (on_v && inside_y) || (on_h && inside_x)
        }
        Tool::Ellipse => {
            let r = norm(a.p0, a.p1);
            r.0 - tol <= x && x <= r.0 + r.2 + tol && r.1 - tol <= y && y <= r.1 + r.3 + tol
        }
        Tool::Line | Tool::Arrow => dist_to_seg((x, y), a.p0, a.p1) < tol,
        Tool::Pen | Tool::Marker => a
            .points
            .windows(2)
            .any(|w| dist_to_seg((x, y), w[0], w[1]) < tol.max(a.width * 1.6)),
        Tool::Mosaic => a
            .cells
            .iter()
            .any(|(cx, cy, cw, ch, _)| x >= *cx && x <= cx + cw && y >= *cy && y <= cy + ch),
        Tool::Text => {
            let lines: Vec<&str> = a.text.split('\n').collect();
            let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let w = max_chars as f32 * a.font_px * 0.6 + 8.0;
            let h = lines.len().max(1) as f32 * a.font_px * 1.3;
            // Undo the box rotation, then test against the upright bounds.
            let center = (a.p0.0 + w / 2.0, a.p0.1 + h / 2.0);
            let (lx, ly) = rotate_point((x, y), center, -a.angle);
            lx >= a.p0.0 - 4.0 && lx <= a.p0.0 + w && ly >= a.p0.1 - 4.0 && ly <= a.p0.1 + h + 6.0
        }
        Tool::Serial => {
            let d = (a.width * 5.0).max(26.0);
            (x - a.p0.0).abs() < d / 2.0 && (y - a.p0.1).abs() < d / 2.0
        }
        Tool::Eraser => false,
    }
}

fn dist_to_seg(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len2 = dx * dx + dy * dy;
    if len2 < 0.0001 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    let px = a.0 + t * dx;
    let py = a.1 + t * dy;
    ((p.0 - px).powi(2) + (p.1 - py).powi(2)).sqrt()
}
