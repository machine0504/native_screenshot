//! Floating annotation toolbar: layout, hit-testing, and tiny-skia drawing.

use crate::annotations::{Tool, PALETTE, WIDTHS};
use crate::common::Rect;
use crate::text::TextRenderer;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform,
};

#[derive(Clone, Copy, PartialEq)]
pub enum ButtonId {
    Tool(usize),
    Color(usize),
    Width(usize),
    Undo,
    Redo,
    Cancel,
    Save,
    Copy,
    Confirm,
}

pub struct Toolbar {
    pub rect: Rect,
    pub buttons: Vec<(Rect, ButtonId)>,
    /// Vertical group-separator lines as `(x, y_top, y_bottom)`.
    pub separators: Vec<(f32, f32, f32)>,
}

/// Colour palette for the floating toolbar. Two variants — [`Theme::dark`] and
/// [`Theme::light`] — track the system appearance so the bar reads well on
/// either. Every field is straight RGBA.
#[derive(Clone, Copy)]
pub struct Theme {
    shell: [u8; 4],        // bar background
    border: [u8; 4],       // hairline rim
    separator: [u8; 4],    // group divider lines
    icon: [u8; 4],         // resting icon colour
    icon_hover: [u8; 4],   // icon colour on hover
    accent: [u8; 4],       // active control colour (selected pill / width dot)
    on_accent: [u8; 4],    // icon colour atop an accent pill
    hover_pill: [u8; 4],   // background wash on hover
    sel_color_pill: [u8; 4], // neutral pill behind the selected colour swatch
    ring_normal: [u8; 4],  // colour-swatch ring, resting
    ring_hover: [u8; 4],   // colour-swatch ring, hover
    ring_selected: [u8; 4], // colour-swatch ring, selected
}

impl Theme {
    /// Charcoal bar with light icons (matches the system dark appearance).
    pub fn dark() -> Self {
        Theme {
            shell: [40, 42, 48, 252],
            border: [255, 255, 255, 30],
            separator: [255, 255, 255, 24],
            icon: [205, 208, 216, 255],
            icon_hover: [240, 242, 246, 255],
            accent: [10, 132, 255, 255],
            on_accent: [255, 255, 255, 255],
            hover_pill: [255, 255, 255, 22],
            sel_color_pill: [255, 255, 255, 32],
            ring_normal: [255, 255, 255, 75],
            ring_hover: [255, 255, 255, 165],
            ring_selected: [255, 255, 255, 255],
        }
    }

    /// Near-white bar with dark icons (matches the system light appearance).
    pub fn light() -> Self {
        Theme {
            shell: [250, 250, 252, 250],
            border: [0, 0, 0, 30],
            separator: [0, 0, 0, 22],
            icon: [78, 82, 92, 255],
            icon_hover: [24, 26, 32, 255],
            accent: [0, 122, 255, 255],
            on_accent: [255, 255, 255, 255],
            hover_pill: [0, 0, 0, 16],
            sel_color_pill: [0, 0, 0, 14],
            ring_normal: [0, 0, 0, 60],
            ring_hover: [0, 0, 0, 120],
            ring_selected: [20, 22, 28, 255],
        }
    }

    pub fn for_dark_mode(dark: bool) -> Self {
        if dark {
            Theme::dark()
        } else {
            Theme::light()
        }
    }
}

// Base sizes in *logical* units; multiplied by the display scale at layout time.
// Kept compact (Snipaste-like) so the bar doesn't dominate narrow HiDPI screens.
const PAD: f32 = 8.0;
const BTN: f32 = 28.0;
const GAP: f32 = 3.0;
const DOT: f32 = 18.0;
const WBTN: f32 = 22.0;
const SEP: f32 = 10.0;
const ACTION: f32 = 34.0;
const ROW_GAP: f32 = 4.0;

type Entry = (ButtonId, f32, bool); // (id, width, separator_before)

fn row_width(row: &[Entry], gap: f32, sep: f32) -> f32 {
    let mut w = 0.0;
    for (i, (_, bw, sep_before)) in row.iter().enumerate() {
        if i > 0 {
            w += gap;
        }
        if *sep_before {
            w += sep;
        }
        w += *bw;
    }
    w
}

pub fn layout(sel: Rect, bounds: Rect, scale: f32) -> Toolbar {
    let s = if scale > 0.0 { scale } else { 1.0 };
    let pad = PAD * s;
    let btn = BTN * s;
    let gap = GAP * s;
    let dot = DOT * s;
    let wbtn = WBTN * s;
    let sep = SEP * s;
    let action = ACTION * s;
    let row_gap = ROW_GAP * s;

    // Logical groups of controls.
    let tools: Vec<Entry> = (0..Tool::ALL.len())
        .map(|i| (ButtonId::Tool(i), btn, false))
        .collect();
    let colors: Vec<Entry> = (0..PALETTE.len())
        .map(|i| (ButtonId::Color(i), dot, i == 0))
        .collect();
    let widths: Vec<Entry> = (0..WIDTHS.len())
        .map(|i| (ButtonId::Width(i), wbtn, i == 0))
        .collect();
    let history: Vec<Entry> = vec![(ButtonId::Undo, btn, true), (ButtonId::Redo, btn, false)];
    // Linux 剪贴板与 helper 进程生命周期耦合，独立「复制」按钮体验差，故隐藏。
    #[cfg(target_os = "linux")]
    let actions: Vec<Entry> = vec![
        (ButtonId::Cancel, action, true),
        (ButtonId::Save, action, false),
        (ButtonId::Confirm, action, false),
    ];
    #[cfg(not(target_os = "linux"))]
    let actions: Vec<Entry> = vec![
        (ButtonId::Cancel, action, true),
        (ButtonId::Save, action, false),
        (ButtonId::Copy, action, false),
        (ButtonId::Confirm, action, false),
    ];

    // Single-row candidate = everything on one line.
    let mut single: Vec<Entry> = Vec::new();
    single.extend(tools.iter().copied());
    single.extend(colors.iter().copied());
    single.extend(widths.iter().copied());
    single.extend(history.iter().copied());
    single.extend(actions.iter().copied());
    let single_w = row_width(&single, gap, sep) + pad * 2.0;

    // Adaptive: if the single row would take more than half the screen width
    // (e.g. on a narrow Retina logical screen), wrap into two rows so the bar
    // stays compact. Wide screens (typical Windows) keep the single row.
    let rows: Vec<Vec<Entry>> = if single_w <= bounds.w * 0.5 {
        vec![single]
    } else {
        let mut r1: Vec<Entry> = Vec::new();
        r1.extend(tools.iter().copied());
        r1.extend(colors.iter().copied()); // colors keep their separator
        let mut r2: Vec<Entry> = Vec::new();
        let mut widths2 = widths.clone();
        if let Some(first) = widths2.first_mut() {
            first.2 = false; // no leading separator at the start of a row
        }
        r2.extend(widths2);
        r2.extend(history.iter().copied());
        r2.extend(actions.iter().copied());
        vec![r1, r2]
    };

    let inner = rows
        .iter()
        .map(|r| row_width(r, gap, sep))
        .fold(0.0_f32, f32::max);
    let bar_w = inner + pad * 2.0;
    let n_rows = rows.len() as f32;
    let h = btn * n_rows + row_gap * (n_rows - 1.0).max(0.0) + pad * 2.0;

    let mut bx = sel.x;
    let mut by = sel.bottom() + 12.0 * s;
    if by + h > bounds.bottom() - 6.0 * s {
        by = sel.y - h - 12.0 * s;
    }
    if by < bounds.y + 6.0 * s {
        by = (bounds.bottom() - h - 8.0 * s).max(bounds.y + 6.0 * s);
    }
    bx = bx.clamp(bounds.x + 6.0 * s, (bounds.right() - bar_w - 6.0 * s).max(bounds.x + 6.0 * s));

    let mut buttons = Vec::new();
    let mut separators = Vec::new();
    let mut ry = by + pad;
    for row in &rows {
        let mut cur = bx + pad;
        for (i, (id, w, sep_before)) in row.iter().enumerate() {
            if i > 0 {
                cur += gap;
            }
            if *sep_before {
                // Record a divider centered in the separator gap.
                separators.push((cur + sep * 0.5, ry + btn * 0.18, ry + btn * 0.82));
                cur += sep;
            }
            buttons.push((Rect::new(cur, ry, *w, btn), *id));
            cur += *w;
        }
        ry += btn + row_gap;
    }

    Toolbar {
        rect: Rect::new(bx, by, bar_w, h),
        buttons,
        separators,
    }
}

pub fn hit(tb: &Toolbar, x: f32, y: f32) -> Option<ButtonId> {
    tb.buttons
        .iter()
        .find(|(r, _)| r.contains(x, y))
        .map(|(_, id)| *id)
}

// ─────────────── drawing helpers ───────────────

fn paint_rgba(c: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.anti_alias = true;
    p
}

fn rstroke(w: f32) -> Stroke {
    Stroke {
        width: w,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    const K: f32 = 0.552_284_8;
    let o = r * K;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + o, y, x + w, y + r - o, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + o, x + w - r + o, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - o, y + h, x, y + h - r + o, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - o, x + r - o, y, x + r, y);
    pb.close();
    pb.finish()
}

fn line(pm: &mut Pixmap, a: (f32, f32), b: (f32, f32), color: [u8; 4], w: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    if let Some(p) = pb.finish() {
        pm.stroke_path(&p, &paint_rgba(color), &rstroke(w), Transform::identity(), None);
    }
}

fn glyph(pm: &mut Pixmap, id: ButtonId, r: Rect, fg: [u8; 4], _text: Option<&TextRenderer>, scale: f32) {
    use crate::icons;
    let k = if scale > 0.0 { scale } else { 1.0 };
    let cx = r.center_x();
    let cy = r.center_y();
    // 图标在按钮内的目标边长（24×24 viewBox 缩放到此方框）。
    let tool_box = 20.0 * k;
    let action_box = 22.0 * k;
    match id {
        ButtonId::Tool(i) => match Tool::ALL[i] {
            Tool::Rect => icons::render(pm, icons::SQUARE, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Ellipse => icons::render(pm, icons::CIRCLE, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Arrow => icons::render(pm, icons::NARROW, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Line => icons::render(pm, icons::STRAIGHT_LINE, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Pen => icons::render(pm, icons::CURVE, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Marker => icons::render(pm, icons::MARKER, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Mosaic => icons::render(pm, icons::MOSAIC, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Text => icons::render(pm, icons::TEXT, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Serial => icons::render(pm, icons::LIST_ORDERED, icons::IDENTITY, cx, cy, tool_box, fg),
            Tool::Eraser => {
                // 橡皮擦：与设计稿一致的描边图标（倾斜圆角橡皮 + 胶头/握柄分隔线 + 底部地线）。
                // 2.0 为 24×24 viewBox 下的描边宽度，缩放后 ≈1.67px，与其它工具的线重相当。
                icons::render_stroke(pm, icons::ERASER, icons::IDENTITY, cx, cy, tool_box, fg, 2.0)
            }
        },
        ButtonId::Undo => icons::render(pm, icons::RETURN, icons::IDENTITY, cx, cy, action_box, fg),
        ButtonId::Redo => icons::render(pm, icons::RETURN, icons::T_MIRROR_X, cx, cy, action_box, fg),
        ButtonId::Cancel => {
            // 取消（关闭遮罩，不输出）。红色在深/浅底上都清晰。
            let red = [232, 86, 76, 255];
            icons::render(pm, icons::CLOSE, icons::T_CLOSE, cx, cy, action_box, red);
        }
        ButtonId::Save => icons::render(pm, icons::DOWNLOAD, icons::IDENTITY, cx, cy, action_box, fg),
        ButtonId::Copy => icons::render(pm, icons::COPY, icons::T_COPY, cx, cy, action_box, fg),
        ButtonId::Confirm => {
            // 确定（写剪贴板 + 返回 PNG 供宿主上传到提问框）。绿色对勾。
            let green = [52, 199, 89, 255];
            icons::render(pm, icons::DETERMINE, icons::IDENTITY, cx, cy, action_box, green);
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    pm: &mut Pixmap,
    tb: &Toolbar,
    text: Option<&TextRenderer>,
    cur_tool: Option<Tool>,
    cur_color: usize,
    cur_width: usize,
    _can_undo: bool,
    _can_redo: bool,
    hovered: Option<ButtonId>,
    dark: bool,
    scale: f32,
) {
    let k = if scale > 0.0 { scale } else { 1.0 };
    let t = Theme::for_dark_mode(dark);
    let r = tb.rect;
    let radius = 12.0 * k;

    // Shell (theme background, near-opaque) + hairline border. (No drop shadow.)
    if let Some(p) = rounded_rect(r.x, r.y, r.w, r.h, radius) {
        let mut bg = Paint::default();
        bg.set_color(Color::from_rgba8(t.shell[0], t.shell[1], t.shell[2], t.shell[3]));
        bg.anti_alias = true;
        pm.fill_path(&p, &bg, FillRule::Winding, Transform::identity(), None);
        pm.stroke_path(&p, &paint_rgba(t.border), &rstroke(1.0 * k), Transform::identity(), None);
    }

    // Group divider hairlines.
    for (sx, y0, y1) in &tb.separators {
        line(pm, (*sx, *y0), (*sx, *y1), t.separator, 1.0 * k);
    }

    let pill_r = 7.0 * k;

    for (br, id) in &tb.buttons {
        let selected = match id {
            ButtonId::Tool(i) => cur_tool == Some(Tool::ALL[*i]),
            ButtonId::Color(i) => *i == cur_color,
            ButtonId::Width(i) => *i == cur_width,
            _ => false,
        };
        let hot = hovered == Some(*id);
        let is_color = matches!(id, ButtonId::Color(_));

        // Background pill: accent for an active control, a soft wash on hover,
        // and a neutral pill behind the selected colour swatch (so an accent
        // pill never clashes with the swatch hue).
        let pill: Option<[u8; 4]> = if selected && is_color {
            Some(t.sel_color_pill)
        } else if selected {
            Some([t.accent[0], t.accent[1], t.accent[2], if hot { 255 } else { 235 }])
        } else if hot {
            Some(t.hover_pill)
        } else {
            None
        };
        if let Some(c) = pill {
            if let Some(p) = rounded_rect(br.x, br.y, br.w, br.h, pill_r) {
                pm.fill_path(&p, &paint_rgba(c), FillRule::Winding, Transform::identity(), None);
            }
        }

        match id {
            ButtonId::Color(i) => {
                let d = 16.0 * k;
                let cx = br.x + br.w / 2.0;
                let cy = br.y + br.h / 2.0;
                if let Some(p) = crate::annotations::oval_path(cx, cy, d / 2.0, d / 2.0) {
                    pm.fill_path(&p, &paint_rgba(PALETTE[*i]), FillRule::Winding, Transform::identity(), None);
                    let ring = if selected {
                        t.ring_selected
                    } else if hot {
                        t.ring_hover
                    } else {
                        t.ring_normal
                    };
                    pm.stroke_path(
                        &p,
                        &paint_rgba(ring),
                        &rstroke(if selected { 2.0 * k } else { 1.2 * k }),
                        Transform::identity(),
                        None,
                    );
                }
            }
            ButtonId::Width(i) => {
                let dia = (WIDTHS[*i] + 2.0) * k;
                let col = if selected {
                    t.on_accent
                } else if hot {
                    t.icon_hover
                } else {
                    t.icon
                };
                if let Some(p) = crate::annotations::oval_path(br.center_x(), br.center_y(), dia / 2.0, dia / 2.0) {
                    pm.fill_path(&p, &paint_rgba(col), FillRule::Winding, Transform::identity(), None);
                }
            }
            _ => {
                let fg = if selected {
                    t.on_accent
                } else if hot {
                    t.icon_hover
                } else {
                    t.icon
                };
                glyph(pm, *id, *br, fg, text, k);
            }
        }
    }
}
