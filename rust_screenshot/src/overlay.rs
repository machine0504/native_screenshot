//! The fullscreen Snipaste-style overlay: winit window + softbuffer present +
//! tiny-skia rendering of the frozen snapshot, selection, magnifier, toolbar
//! and annotations.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};

/// Minimum interval between on-screen presents for the **softbuffer** path
/// (~66 fps). softbuffer re-uploads the whole framebuffer every present (it
/// ignores damage on macOS), so it can't sustain a high refresh — pacing keeps
/// each frame within budget.
const MIN_FRAME: Duration = Duration::from_millis(15);

/// Present interval for the **GPU (Metal) path** (~144 fps). The GPU present
/// only uploads the damaged region, so it's cheap enough to run at the native
/// refresh; this lets it hit every 120 Hz vsync (no 66 fps judder) while still
/// capping runaway compositing on trackpads that emit >120 events/sec.
const MIN_FRAME_GPU: Duration = Duration::from_micros(6_900);

/// Caret blink half-period for the text tool.
const BLINK: Duration = Duration::from_millis(530);

use crate::annotations::{self, Annotation, Tool, PALETTE, WIDTHS};
use crate::common::{Rect, Snapshot};
use crate::text::TextRenderer;
use crate::toolbar::{self, ButtonId, Toolbar};

/// What the user ended the session with.
pub enum Outcome {
    /// RGBA pixels of the final selection + annotations, ready to encode/clip.
    Capture {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        save: bool,
        /// 仅复制到剪贴板：宿主不应把它当作截图结果回传/插入提问框。
        copy_only: bool,
    },
    Cancelled,
    /// 基础设施类失败（窗口/绘制表面创建不出来等），区别于用户主动取消。
    /// 会以 `ERROR <msg>` 回传宿主，避免在 Wayland 等环境下被误当成“用户取消”而静默。
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Handle {
    None,
    TL,
    T,
    TR,
    R,
    BR,
    B,
    BL,
    L,
    Move,
}

/// A handle on the text-editing transform box. The top-right corner is the
/// close button; the rotation handle floats above the top edge; the remaining
/// corners/edges scale the font; the frame band moves the box.
#[derive(Clone, Copy, PartialEq)]
enum TextHandle {
    None,
    Rotate,
    Close,
    Resize,
    MoveBox,
}

/// Which gizmo drag is in progress, with the bookkeeping captured at grab time.
#[derive(Clone, Copy)]
struct GizmoDrag {
    kind: TextHandle,
    center: (f32, f32),
    init_font: f32,
    init_angle: f32,
    init_ptr_angle: f32,
    init_dist: f32,
    init_pos: (f32, f32),
    grab: (f32, f32),
}

/// Color readout format for the magnifier (cycled with Shift).
#[derive(Clone, Copy, PartialEq)]
enum ColorFmt {
    Hex,
    Rgb,
    Rgba,
    Hsv,
}

impl ColorFmt {
    fn next(self) -> ColorFmt {
        match self {
            ColorFmt::Hex => ColorFmt::Rgb,
            ColorFmt::Rgb => ColorFmt::Rgba,
            ColorFmt::Rgba => ColorFmt::Hsv,
            ColorFmt::Hsv => ColorFmt::Hex,
        }
    }

    fn format(self, r: u8, g: u8, b: u8) -> String {
        match self {
            ColorFmt::Hex => format!("#{r:02X}{g:02X}{b:02X}"),
            ColorFmt::Rgb => format!("rgb({r}, {g}, {b})"),
            ColorFmt::Rgba => format!("rgba({r}, {g}, {b}, 255)"),
            ColorFmt::Hsv => {
                let (h, s, v) = rgb_to_hsv(r, g, b);
                format!("hsv({h}, {s}%, {v}%)")
            }
        }
    }
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (i32, i32, i32) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let d = max - min;
    let mut h = if d == 0.0 {
        0.0
    } else if max == rf {
        60.0 * (((gf - bf) / d) % 6.0)
    } else if max == gf {
        60.0 * (((bf - rf) / d) + 2.0)
    } else {
        60.0 * (((rf - gf) / d) + 4.0)
    };
    if h < 0.0 {
        h += 360.0;
    }
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h.round() as i32, (s * 100.0).round() as i32, (max * 100.0).round() as i32)
}

pub struct Overlay {
    snap: Rc<Snapshot>,
    text: Option<TextRenderer>,

    // Auto-detected window rects (topmost-first) + the one under the cursor.
    regions: Vec<Rect>,
    hover_region: Option<Rect>,
    moved: bool, // whether the current press has dragged beyond a threshold

    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    // macOS GPU presenter (damage-aware). When `Some`, softbuffer is unused.
    #[cfg(target_os = "macos")]
    gpu: Option<crate::gpu::GpuPresenter>,
    scene: Option<Pixmap>, // reused framebuffer

    // Damage-render cache. `bg` is the fully composed scene WITHOUT the
    // magnifier; in the selecting phase only the magnifier follows the cursor,
    // so we restore + repaint + present just its rectangle instead of redrawing
    // the whole (HiDPI) desktop every cursor move.
    bg: Option<Pixmap>,
    /// Frozen snapshot pre-dimmed with the mask color (snapshot × 53%). Lets every
    /// repaint compose the dark area with a plain memcpy instead of a per-pixel
    /// alpha blend, which is the key to 60fps on large HiDPI desktops.
    dim_cache: Option<Pixmap>,
    /// Lazily-built tiny-skia mirror of the frozen snapshot, used as the eraser's
    /// fill pattern so a brush stroke restores the original pixels beneath it.
    snap_px: Option<Pixmap>,
    present_buf: Vec<u32>,                   // persistent 0RGB mirror of the surface
    last_mag: Option<(i32, i32, i32, i32)>,  // last magnifier widget bbox (physical px)
    last_sel_render: Option<Rect>,           // previous drag-selection rect (damage path)
    bg_sig: Option<u64>,                     // signature of bg-affecting state

    // Incremental repaint for the Adjusting/Annotating phase (selection already
    // made). `pending_damage` is the union of regions changed since the last
    // present; when only geometry moved we recompose + present just that box
    // instead of the whole desktop. `last_struct_sig` detects when a full
    // rebuild is unavoidable (annotation added/removed, tool/color changed…).
    pending_damage: Option<(i32, i32, i32, i32)>,
    last_struct_sig: Option<u64>,

    bounds: Rect,

    // selection
    have_sel: bool,
    drag_anchor: Option<(f32, f32)>,
    sel: Rect,

    // interaction
    dragging: bool,
    active_handle: Handle,
    drag_start: (f32, f32),
    drag_init: Rect,

    // moving an existing annotation (click-drag a placed text / shape)
    moving_anno: Option<usize>,
    anno_move_orig: Option<Annotation>,
    anno_move_start: (f32, f32),
    anno_drag_undo_pushed: bool,

    // tools
    tool: Option<Tool>,
    color_idx: usize,
    width_idx: usize,
    annotations: Vec<Annotation>,
    undo: Vec<Vec<Annotation>>,
    redo: Vec<Vec<Annotation>>,
    drawing: Option<Annotation>,

    // text input
    text_input: Option<TextInput>,
    // Last IME candidate-area (physical px, rounded) we handed to the compositor.
    // Deduped so we don't re-issue `set_ime_cursor_area` on every IME event: on
    // Wayland+fcitx5 that re-commit drives an endless empty-`Preedit` loop that
    // resets composition, so Chinese never reaches `Ime::Commit`. See
    // `update_ime_cursor`.
    last_ime_area: Cell<Option<(i32, i32, i32, i32)>>,
    // Transform-gizmo drag in progress while editing text (rotate/resize/move).
    gizmo: Option<GizmoDrag>,
    // Placed text annotation under the cursor (dashed "click to edit" hint).
    hover_text: Option<usize>,

    // mouse
    mouse: (f32, f32),
    modifiers: ModifiersState,
    color_format: ColorFmt,

    toolbar: Option<Toolbar>,
    // Toolbar button currently under the cursor (for hover highlight).
    tb_hover: Option<ButtonId>,
    // System appearance: true = dark mode → dark toolbar theme, else light.
    dark_mode: bool,

    // Display scale factor (e.g. 2.0 on macOS Retina). The scene is rendered in
    // physical pixels for a crisp 1:1 snapshot, so UI chrome authored in logical
    // units must be multiplied by this or it shows up half-size on HiDPI.
    scale: f32,

    // True once the overlay window has been created at least once. Combined with
    // `window.is_none()` it lets a driver (one-shot or resident server) detect
    // that the session has ended.
    opened: bool,

    // Frame pacing. `last_present` is when we last actually presented; a redraw
    // arriving sooner than `min_frame` is deferred (`redraw_pending`) and flushed
    // by the event loop via [`Overlay::pump_frame`]. `min_frame` is the present
    // interval: short on the GPU path (native refresh), longer on softbuffer.
    last_present: Option<Instant>,
    redraw_pending: bool,
    min_frame: Duration,

    // Caret blink state for the text tool.
    caret_on: bool,
    last_blink: Instant,

    // Double-click detection for re-editing a placed text annotation.
    last_click: Option<(Instant, f32, f32)>,

    pub outcome: Outcome,
}

struct TextInput {
    pos: (f32, f32),
    buffer: String,
    preedit: String,
    /// Caret position as a **char index** into `buffer` (0..=char_count).
    caret: usize,
    /// Selection anchor (char index). When `Some` and `!= caret`, the range
    /// `[min, max)` is selected. `None` means no selection.
    anchor: Option<usize>,
    /// True while a mouse drag is extending the selection.
    selecting: bool,
    /// Font size (physical px) for this text box, derived from the display scale
    /// so the typed text is the same apparent size on HiDPI and standard screens.
    font_px: f32,
    /// Rotation in radians around the box center (set via the gizmo handle).
    angle: f32,
    /// Text color (kept on the editor so re-editing shows the original color and
    /// palette changes recolor it live).
    color: [u8; 4],
    /// When re-editing an existing annotation, its index (removed from the list
    /// while editing, re-added on commit). `None` for a brand-new text box.
    editing: Option<usize>,
    /// Set once the IME actually *produces* text — i.e. a non-empty
    /// `Ime::Preedit` composition starts or an `Ime::Commit` lands. From that
    /// point committed text arrives via `Ime::Commit`, so we ignore raw
    /// `KeyboardInput` text to avoid double-typing.
    ///
    /// Deliberately NOT set by `Ime::Enabled`: winit fires that as soon as we
    /// call `set_ime_allowed(true)`, even when the desktop's IME (fcitx/ibus,
    /// common on 统信/UOS Wayland) never actually feeds us text. Arming it there
    /// used to permanently disable the raw-key fallback below, so the text tool
    /// swallowed every keystroke ("动键盘没有反应"). Leaving it false until real
    /// composition/commit lets English fall back to raw key text while still
    /// deferring to the IME once it truly engages.
    ime_seen: bool,
}

impl TextInput {
    fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Byte offset of char index `i` within `buffer`.
    fn byte_of(&self, i: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(i)
            .map(|(b, _)| b)
            .unwrap_or(self.buffer.len())
    }

    /// Selection as an ordered char range `[start, end)`, or `None` if empty.
    fn sel_range(&self) -> Option<(usize, usize)> {
        self.anchor.and_then(|a| {
            let (s, e) = if a <= self.caret { (a, self.caret) } else { (self.caret, a) };
            if s < e {
                Some((s, e))
            } else {
                None
            }
        })
    }

    /// The selected substring, if any.
    fn selected_text(&self) -> Option<String> {
        self.sel_range().map(|(s, e)| {
            let bs = self.byte_of(s);
            let be = self.byte_of(e);
            self.buffer[bs..be].to_string()
        })
    }

    /// Delete the current selection (if any). Returns whether anything was removed.
    fn delete_selection(&mut self) -> bool {
        if let Some((s, e)) = self.sel_range() {
            let bs = self.byte_of(s);
            let be = self.byte_of(e);
            self.buffer.replace_range(bs..be, "");
            self.caret = s;
            self.anchor = None;
            true
        } else {
            self.anchor = None;
            false
        }
    }

    /// Select the whole buffer.
    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.char_count();
    }

    /// Insert `s` at the caret (replacing any selection) and advance the caret.
    fn insert(&mut self, s: &str) {
        self.delete_selection();
        if s.is_empty() {
            return;
        }
        let b = self.byte_of(self.caret);
        self.buffer.insert_str(b, s);
        self.caret += s.chars().count();
    }

    /// Delete the selection, or the char before the caret (Backspace).
    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.caret == 0 {
            return;
        }
        let start = self.byte_of(self.caret - 1);
        let end = self.byte_of(self.caret);
        self.buffer.replace_range(start..end, "");
        self.caret -= 1;
    }

    /// Delete the selection, or the char after the caret (Delete).
    fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let n = self.char_count();
        if self.caret >= n {
            return;
        }
        let start = self.byte_of(self.caret);
        let end = self.byte_of(self.caret + 1);
        self.buffer.replace_range(start..end, "");
    }

    /// Begin or clear a selection before a caret move: `extend` keeps/creates the
    /// anchor (Shift held); otherwise the selection collapses.
    fn pre_move(&mut self, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
    }

    /// (line_index, column) of the caret, splitting on '\n'.
    fn caret_line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for (i, ch) in self.buffer.chars().enumerate() {
            if i == self.caret {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// Char index at (line, col), clamping col to the line length.
    fn index_at(&self, line: usize, col: usize) -> usize {
        let mut cur_line = 0;
        let mut idx = 0;
        let mut col_in_line = 0;
        for ch in self.buffer.chars() {
            if cur_line == line && col_in_line == col {
                return idx;
            }
            if ch == '\n' {
                if cur_line == line {
                    return idx; // col beyond end of this line
                }
                cur_line += 1;
                col_in_line = 0;
            } else {
                col_in_line += 1;
            }
            idx += 1;
        }
        idx
    }

    fn move_left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }
    fn move_right(&mut self) {
        self.caret = (self.caret + 1).min(self.char_count());
    }
    fn move_up(&mut self) {
        let (line, col) = self.caret_line_col();
        if line > 0 {
            self.caret = self.index_at(line - 1, col);
        } else {
            self.caret = 0;
        }
    }
    fn move_down(&mut self) {
        let (line, col) = self.caret_line_col();
        self.caret = self.index_at(line + 1, col);
    }
    fn move_home(&mut self) {
        let (line, _) = self.caret_line_col();
        self.caret = self.index_at(line, 0);
    }
    fn move_end(&mut self) {
        let (line, _) = self.caret_line_col();
        self.caret = self.index_at(line, usize::MAX);
    }

    /// Visible text = the committed buffer with the IME preedit inserted at the
    /// caret. Shared by the renderer and the gizmo box measurement so they stay
    /// in sync while composing.
    fn display(&self) -> String {
        let cb = self.byte_of(self.caret);
        let mut s = String::with_capacity(self.buffer.len() + self.preedit.len());
        s.push_str(&self.buffer[..cb]);
        s.push_str(&self.preedit);
        s.push_str(&self.buffer[cb..]);
        s
    }
}

impl Overlay {
    pub fn new(snap: Snapshot, regions: Vec<Rect>) -> Self {
        let bounds = Rect::new(0.0, 0.0, snap.width as f32, snap.height as f32);
        Overlay {
            snap: Rc::new(snap),
            text: TextRenderer::load(),
            regions,
            hover_region: None,
            moved: false,
            window: None,
            surface: None,
            #[cfg(target_os = "macos")]
            gpu: None,
            scene: None,
            bg: None,
            dim_cache: None,
            snap_px: None,
            present_buf: Vec::new(),
            last_mag: None,
            last_sel_render: None,
            bg_sig: None,
            pending_damage: None,
            last_struct_sig: None,
            bounds,
            have_sel: false,
            drag_anchor: None,
            sel: Rect::default(),
            dragging: false,
            active_handle: Handle::None,
            drag_start: (0.0, 0.0),
            drag_init: Rect::default(),
            moving_anno: None,
            anno_move_orig: None,
            anno_move_start: (0.0, 0.0),
            anno_drag_undo_pushed: false,
            tool: None,
            color_idx: 0,
            width_idx: 1,
            annotations: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            drawing: None,
            text_input: None,
            last_ime_area: Cell::new(None),
            gizmo: None,
            hover_text: None,
            mouse: (0.0, 0.0),
            modifiers: ModifiersState::empty(),
            color_format: ColorFmt::Hex,
            toolbar: None,
            tb_hover: None,
            // Default to dark until the window reports its real appearance in open().
            dark_mode: true,
            scale: 1.0,
            opened: false,
            last_present: None,
            redraw_pending: false,
            min_frame: MIN_FRAME,
            caret_on: true,
            last_blink: Instant::now(),
            last_click: None,
            outcome: Outcome::Cancelled,
        }
    }

    /// Whether the overlay session has ended (window created then closed).
    pub fn is_done(&self) -> bool {
        self.opened && self.window.is_none()
    }

    fn color(&self) -> [u8; 4] {
        PALETTE[self.color_idx]
    }
    fn width(&self) -> f32 {
        WIDTHS[self.width_idx]
    }

    /// Whether any committed or in-progress annotation is an eraser stroke; used
    /// to decide if the (expensive) snapshot pixmap mirror needs to be built.
    fn has_eraser(&self) -> bool {
        self.annotations.iter().any(|a| a.tool == Tool::Eraser)
            || matches!(&self.drawing, Some(a) if a.tool == Tool::Eraser)
    }

    /// Lazily build the tiny-skia mirror of the frozen snapshot (the eraser's
    /// fill pattern). The snapshot is opaque, so its straight RGBA equals the
    /// premultiplied RGBA tiny-skia expects.
    fn ensure_snap_px(&mut self) {
        if self.snap_px.is_some() {
            return;
        }
        if let Some(mut pm) = Pixmap::new(self.snap.width, self.snap.height) {
            let dst = pm.data_mut();
            if dst.len() == self.snap.rgba.len() {
                dst.copy_from_slice(&self.snap.rgba);
                self.snap_px = Some(pm);
            }
        }
    }

    /// Borrow the eraser fill pattern source, if one has been built.
    fn snap_px_ref(&self) -> Option<tiny_skia::PixmapRef<'_>> {
        self.snap_px.as_ref().map(|p| p.as_ref())
    }

    fn push_undo(&mut self) {
        self.undo.push(self.annotations.clone());
        if self.undo.len() > 50 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn do_undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.annotations, prev));
        }
    }
    fn do_redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.annotations, next));
        }
    }

    fn next_serial(&self) -> i32 {
        self.annotations.iter().filter(|a| a.tool == Tool::Serial).count() as i32 + 1
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    // ─────────────── hit testing ───────────────

    /// Top-most detected window rect under (x, y), clamped to the overlay
    /// bounds. `None` if the cursor isn't over any detected window.
    fn region_at(&self, x: f32, y: f32) -> Option<Rect> {
        for r in &self.regions {
            if r.contains(x, y) {
                let l = r.x.max(self.bounds.x);
                let t = r.y.max(self.bounds.y);
                let rr = r.right().min(self.bounds.right());
                let bb = r.bottom().min(self.bounds.bottom());
                let min = 8.0 * self.scale;
                if rr - l >= min && bb - t >= min {
                    return Some(Rect::new(l, t, rr - l, bb - t));
                }
            }
        }
        None
    }

    /// Apply the appropriate platform cursor for the current state.
    fn update_cursor(&self) {
        if let Some(w) = &self.window {
            w.set_cursor(self.desired_cursor());
        }
    }

    fn desired_cursor(&self) -> CursorIcon {
        if let Some(ti) = &self.text_input {
            if self.gizmo.is_some() {
                return CursorIcon::Grabbing;
            }
            // Transform-gizmo handles take precedence over the text caret.
            match self.text_handle_at(ti, self.mouse.0, self.mouse.1) {
                TextHandle::Rotate => return CursorIcon::Grab,
                TextHandle::Close => return CursorIcon::Pointer,
                TextHandle::Resize => return CursorIcon::NwseResize,
                TextHandle::MoveBox => return CursorIcon::Move,
                TextHandle::None => {}
            }
            // I-beam over the editable area, arrow elsewhere.
            if self.text_box_contains(ti, self.mouse.0, self.mouse.1) {
                return CursorIcon::Text;
            }
            return CursorIcon::Default;
        }
        // Selecting phase: always crosshair (incl. while rubber-banding).
        if !self.have_sel {
            return CursorIcon::Crosshair;
        }

        // While actively dragging, reflect the operation in progress.
        if self.dragging {
            return match self.active_handle {
                Handle::Move => CursorIcon::Grabbing,
                Handle::None => CursorIcon::Crosshair, // drawing an annotation
                h => handle_cursor(h),
            };
        }

        let (x, y) = self.mouse;
        if let Some(tb) = &self.toolbar {
            if tb.rect.contains(x, y) {
                return CursorIcon::Pointer;
            }
        }
        let h = self.handle_at(x, y);
        if h != Handle::None {
            return handle_cursor(h);
        }
        // Hovering a placed annotation we can grab to move it.
        if self.sel.contains(x, y) && (self.tool.is_none() || self.tool == Some(Tool::Text)) {
            let text_only = self.tool == Some(Tool::Text);
            if self.annotation_at(x, y, text_only).is_some() {
                return CursorIcon::Move;
            }
        }
        if self.tool.is_some() && self.sel.contains(x, y) {
            return CursorIcon::Crosshair;
        }
        if self.sel.contains(x, y) {
            return CursorIcon::Move;
        }
        CursorIcon::Crosshair
    }

    fn handle_at(&self, x: f32, y: f32) -> Handle {
        if !self.have_sel {
            return Handle::None;
        }
        let s = self.sel;
        let tol = 10.0 * self.scale;
        let pts = [
            (Handle::TL, s.x, s.y),
            (Handle::T, s.center_x(), s.y),
            (Handle::TR, s.right(), s.y),
            (Handle::R, s.right(), s.center_y()),
            (Handle::BR, s.right(), s.bottom()),
            (Handle::B, s.center_x(), s.bottom()),
            (Handle::BL, s.x, s.bottom()),
            (Handle::L, s.x, s.center_y()),
        ];
        for (h, hx, hy) in pts {
            if (x - hx).abs() <= tol && (y - hy).abs() <= tol {
                return h;
            }
        }
        Handle::None
    }

    fn apply_handle(&mut self, x: f32, y: f32) {
        let dx = x - self.drag_start.0;
        let dy = y - self.drag_start.1;
        let r = self.drag_init;
        let (mut l, mut t, mut rr, mut b) = (r.x, r.y, r.right(), r.bottom());
        match self.active_handle {
            Handle::TL => {
                l += dx;
                t += dy;
            }
            Handle::T => t += dy,
            Handle::TR => {
                rr += dx;
                t += dy;
            }
            Handle::R => rr += dx,
            Handle::BR => {
                rr += dx;
                b += dy;
            }
            Handle::B => b += dy,
            Handle::BL => {
                l += dx;
                b += dy;
            }
            Handle::L => l += dx,
            Handle::Move => {
                let nx = (r.x + dx).clamp(0.0, self.bounds.right() - r.w);
                let ny = (r.y + dy).clamp(0.0, self.bounds.bottom() - r.h);
                self.sel = Rect::new(nx, ny, r.w, r.h);
                return;
            }
            Handle::None => return,
        }
        let nr = Rect::from_points(l, t, rr, b);
        let min = 8.0 * self.scale;
        if nr.w >= min && nr.h >= min {
            self.sel = nr;
        }
    }

    // ─────────────── input handlers ───────────────

    fn on_left_down(&mut self, x: f32, y: f32) {
        // toolbar first (don't commit the editor when tweaking color/width)
        if let Some(tb) = &self.toolbar {
            if tb.rect.contains(x, y) {
                if let Some(id) = toolbar::hit(tb, x, y) {
                    self.on_toolbar(id);
                }
                return;
            }
        }

        // Double-click detection (for re-editing placed text).
        let now = Instant::now();
        let dbl = self.last_click.map_or(false, |(t, px, py)| {
            now.duration_since(t) < Duration::from_millis(400)
                && (x - px).abs() < 6.0 * self.scale
                && (y - py).abs() < 6.0 * self.scale
        });
        self.last_click = Some((now, x, y));

        // Interactions with an active text editor.
        if self.text_input.is_some() {
            // macOS: the native NSTextView consumes clicks inside its frame, so
            // any click that reaches us is outside it → commit and finish. (The
            // toolbar was already handled above.)
            if crate::macos::native_text_active() {
                self.commit_text();
                self.last_struct_sig = None;
                self.request_redraw();
                return;
            }
            // Transform gizmo (rotate / resize / close / move) takes priority.
            let handle = self
                .text_input
                .as_ref()
                .map(|ti| self.text_handle_at(ti, x, y))
                .unwrap_or(TextHandle::None);
            match handle {
                TextHandle::Close => {
                    self.close_text_editor();
                    self.request_redraw();
                    return;
                }
                TextHandle::Rotate | TextHandle::Resize | TextHandle::MoveBox => {
                    self.begin_gizmo(handle, x, y);
                    return;
                }
                TextHandle::None => {}
            }

            let inside = self
                .text_input
                .as_ref()
                .map(|ti| self.text_box_contains(ti, x, y))
                .unwrap_or(false);
            if inside {
                // Place the caret, or start a drag-selection; double-click selects all.
                let idx = self
                    .text_input
                    .as_ref()
                    .map(|ti| self.text_index_at(ti, x, y))
                    .unwrap_or(0);
                if let Some(ti) = self.text_input.as_mut() {
                    if dbl {
                        ti.select_all();
                        ti.selecting = false;
                    } else {
                        ti.caret = idx;
                        ti.anchor = Some(idx);
                        ti.selecting = true;
                    }
                }
                self.caret_on = true;
                self.last_blink = Instant::now();
                self.update_ime_cursor();
                self.request_redraw();
                return;
            }
            // Pressed outside the editor → commit it, then handle the click normally.
            self.commit_text();
        }

        // Double-click an existing text annotation → reopen it for editing.
        if self.have_sel && dbl {
            if let Some(idx) = self.text_annotation_at(x, y) {
                let a = self.annotations[idx].clone();
                let caret = a.text.chars().count();
                self.tool = Some(Tool::Text);
                self.open_text_input(a.p0, a.text.clone(), caret, a.font_px, a.color, a.angle, Some(idx));
                self.request_redraw();
                return;
            }
        }

        if !self.have_sel {
            self.drag_anchor = Some((x, y));
            self.sel = Rect::new(x, y, 0.0, 0.0);
            self.dragging = true;
            self.moved = false;
            return;
        }

        // resize handle
        let h = self.handle_at(x, y);
        if h != Handle::None {
            self.dragging = true;
            self.active_handle = h;
            self.drag_start = (x, y);
            self.drag_init = self.sel;
            return;
        }

        let inside = self.sel.contains(x, y);

        // Grab a placed annotation to move it. With no tool, any annotation is
        // grabbable; with the text tool, only existing text (so clicking empty
        // space still starts a new text box).
        if inside && (self.tool.is_none() || self.tool == Some(Tool::Text)) {
            let text_only = self.tool == Some(Tool::Text);
            if let Some(idx) = self.annotation_at(x, y, text_only) {
                self.moving_anno = Some(idx);
                self.anno_move_orig = Some(self.annotations[idx].clone());
                self.anno_move_start = (x, y);
                self.anno_drag_undo_pushed = false;
                self.dragging = true;
                self.active_handle = Handle::None;
                return;
            }
        }

        if let Some(tool) = self.tool {
            if inside {
                self.begin_tool(tool, x, y);
                return;
            }
        }

        if inside {
            // move selection
            self.dragging = true;
            self.active_handle = Handle::Move;
            self.drag_start = (x, y);
            self.drag_init = self.sel;
            return;
        }

        // outside → fresh selection
        self.commit_text();
        self.have_sel = false;
        self.annotations.clear();
        self.undo.clear();
        self.redo.clear();
        self.tool = None;
        self.toolbar = None;
        self.tb_hover = None;
        self.drag_anchor = Some((x, y));
        self.sel = Rect::new(x, y, 0.0, 0.0);
        self.dragging = true;
    }

    fn begin_tool(&mut self, tool: Tool, x: f32, y: f32) {
        match tool {
            Tool::Text => {
                self.commit_text();
                let font_px = 26.0 * self.scale;
                let color = self.color();
                self.open_text_input((x, y), String::new(), 0, font_px, color, 0.0, None);
            }
            Tool::Serial => {
                self.push_undo();
                let mut a = Annotation::new(Tool::Serial, self.color(), self.width());
                a.p0 = (x, y);
                a.serial = self.next_serial();
                self.annotations.push(a);
            }
            _ => {
                let mut a = Annotation::new(tool, self.color(), self.width());
                a.p0 = (x, y);
                a.p1 = (x, y);
                if tool == Tool::Pen || tool == Tool::Marker || tool == Tool::Eraser {
                    a.points.push((x, y));
                } else if tool == Tool::Mosaic {
                    self.add_mosaic(&mut a, x, y);
                }
                self.drawing = Some(a);
                self.dragging = true;
            }
        }
    }

    fn add_mosaic(&self, a: &mut Annotation, x: f32, y: f32) {
        let block = 12.0_f32;
        let radius = ((a.width * 2.5 / block) as i32 + 1).max(1);
        let cx = (x / block) as i32;
        let cy = (y / block) as i32;
        for gy in (cy - radius)..=(cy + radius) {
            for gx in (cx - radius)..=(cx + radius) {
                if gx < 0 || gy < 0 {
                    continue;
                }
                let px = gx as f32 * block;
                let py = gy as f32 * block;
                if a.cells.iter().any(|(cx2, cy2, _, _, _)| *cx2 == px && *cy2 == py) {
                    continue;
                }
                let (r, g, b, _) = self
                    .snap
                    .pixel((px + block / 2.0) as i32, (py + block / 2.0) as i32);
                a.cells.push((px, py, block, block, [r, g, b, 255]));
            }
        }
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        self.mouse = (x, y);

        // A transform-gizmo drag (rotate / resize / move) owns the mouse.
        if self.gizmo.is_some() {
            self.drag_gizmo(x, y);
            self.update_cursor();
            self.request_redraw();
            return;
        }

        // Drag-selecting inside the text editor takes over the mouse.
        let selecting = self
            .text_input
            .as_ref()
            .map(|ti| ti.selecting)
            .unwrap_or(false);
        if selecting {
            let idx = {
                let ti = self.text_input.as_ref().unwrap();
                self.text_index_at(ti, x, y)
            };
            if let Some(ti) = self.text_input.as_mut() {
                ti.caret = idx;
            }
            self.caret_on = true;
            self.last_blink = Instant::now();
            self.update_ime_cursor();
            self.request_redraw();
            return;
        }

        let mut needs_redraw = false;

        // Selecting phase, not pressing: auto-detect the window under cursor.
        // The magnifier follows the cursor every frame here, so always repaint.
        if !self.have_sel && !self.dragging {
            self.hover_region = self.region_at(x, y);
            needs_redraw = true;
        }

        // Idle over a placed text annotation → dashed "click to edit" hint. Only
        // hint when a click would actually land on that text (it's the topmost
        // grabbable annotation), so the cue matches what clicking does.
        let want_hover = self.have_sel
            && !self.dragging
            && self.text_input.is_none()
            && (self.tool.is_none() || self.tool == Some(Tool::Text));
        let new_hover = if want_hover {
            let text_only = self.tool == Some(Tool::Text);
            match self.annotation_at(x, y, text_only) {
                Some(i) if self.annotations[i].tool == Tool::Text => Some(i),
                _ => None,
            }
        } else {
            None
        };
        if new_hover != self.hover_text {
            if let Some(i) = self.hover_text {
                if i < self.annotations.len() {
                    let d = self.text_anno_damage(&self.annotations[i]);
                    self.add_damage(d.0, d.1, d.2, d.3);
                }
            }
            if let Some(i) = new_hover {
                if i < self.annotations.len() {
                    let d = self.text_anno_damage(&self.annotations[i]);
                    self.add_damage(d.0, d.1, d.2, d.3);
                }
            }
            self.hover_text = new_hover;
            needs_redraw = true;
        }

        // Toolbar button hover highlight (only while the bar is visible & idle).
        if self.have_sel {
            let new_tb_hover = if !self.dragging {
                self.toolbar.as_ref().and_then(|tb| {
                    if tb.rect.contains(x, y) {
                        toolbar::hit(tb, x, y)
                    } else {
                        None
                    }
                })
            } else {
                None
            };
            if new_tb_hover != self.tb_hover {
                self.tb_hover = new_tb_hover;
                if let Some(tb) = &self.toolbar {
                    let m = 6.0 * self.scale;
                    let tr = tb.rect;
                    self.add_damage(tr.x - m, tr.y - m, tr.right() + m, tr.bottom() + m * 2.0);
                }
                needs_redraw = true;
            }
        }

        if self.dragging {
            if !self.have_sel {
                if let Some(a) = self.drag_anchor {
                    let thr = 4.0 * self.scale;
                    if (x - a.0).abs() > thr || (y - a.1).abs() > thr {
                        self.moved = true;
                    }
                    self.sel = Rect::from_points(a.0, a.1, x, y);
                }
                needs_redraw = true;
            } else if let Some(idx) = self.moving_anno {
                // Drag a placed annotation: translate from its grab-time origin.
                // A sub-threshold press is treated as a click (handled on mouse
                // up: text opens for editing) rather than a move, so a little
                // pointer jitter doesn't nudge the annotation.
                if idx < self.annotations.len() {
                    if let Some(orig) = self.anno_move_orig.clone() {
                        let dx = x - self.anno_move_start.0;
                        let dy = y - self.anno_move_start.1;
                        let thr = 4.0 * self.scale;
                        if self.anno_drag_undo_pushed || dx.abs() > thr || dy.abs() > thr {
                            if !self.anno_drag_undo_pushed {
                                self.push_undo();
                                self.anno_drag_undo_pushed = true;
                            }
                            let old = self.annotation_bbox(&self.annotations[idx]);
                            let mut moved = orig;
                            annotations::translate(&mut moved, dx, dy);
                            let new = self.annotation_bbox(&moved);
                            self.annotations[idx] = moved;
                            self.add_damage(old.0, old.1, old.2, old.3);
                            self.add_damage(new.0, new.1, new.2, new.3);
                            needs_redraw = true;
                        }
                    }
                }
            } else if self.active_handle != Handle::None {
                let old = self.sel;
                self.apply_handle(x, y);
                let new = self.sel;
                self.add_sel_damage(old);
                self.add_sel_damage(new);
                needs_redraw = true;
            } else if self.drawing.is_some() {
                let old = { self.annotation_bbox(self.drawing.as_ref().unwrap()) };
                let tool = self.drawing.as_ref().unwrap().tool;
                match tool {
                    Tool::Pen | Tool::Marker | Tool::Eraser => {
                        self.drawing.as_mut().unwrap().points.push((x, y));
                    }
                    Tool::Mosaic => {
                        let mut tmp = self.drawing.take().unwrap();
                        self.add_mosaic(&mut tmp, x, y);
                        self.drawing = Some(tmp);
                    }
                    _ => self.drawing.as_mut().unwrap().p1 = (x, y),
                }
                let new = { self.annotation_bbox(self.drawing.as_ref().unwrap()) };
                self.add_damage(old.0, old.1, old.2, old.3);
                self.add_damage(new.0, new.1, new.2, new.3);
                needs_redraw = true;
            }
        }
        self.update_cursor();
        if needs_redraw {
            self.request_redraw();
        }
    }

    fn on_left_up(&mut self, x: f32, y: f32) {
        // Finish a transform-gizmo drag.
        if self.gizmo.take().is_some() {
            self.update_ime_cursor();
            self.request_redraw();
            return;
        }

        // Finish a text drag-selection.
        if let Some(ti) = self.text_input.as_mut() {
            if ti.selecting {
                ti.selecting = false;
                if ti.anchor == Some(ti.caret) {
                    ti.anchor = None;
                }
                self.request_redraw();
                return;
            }
        }

        if !self.dragging {
            return;
        }
        self.dragging = false;

        // Finished interacting with a placed annotation.
        if let Some(idx) = self.moving_anno.take() {
            let was_move = self.anno_drag_undo_pushed;
            self.anno_move_orig = None;
            self.anno_drag_undo_pushed = false;
            self.active_handle = Handle::None;
            // A click (no actual drag) on a text annotation opens it for editing;
            // dragging it just repositions (already applied incrementally).
            if !was_move && idx < self.annotations.len() && self.annotations[idx].tool == Tool::Text {
                let a = self.annotations[idx].clone();
                let caret = a.text.chars().count();
                self.tool = Some(Tool::Text);
                self.open_text_input(a.p0, a.text.clone(), caret, a.font_px, a.color, a.angle, Some(idx));
            }
            self.request_redraw();
            return;
        }

        if !self.have_sel {
            if !self.moved {
                // A click (no drag): snap to the auto-detected window region.
                if let Some(r) = self.hover_region {
                    self.sel = r;
                    self.have_sel = true;
                    // Confirming a selection adds handles + toolbar; force one full
                    // rebuild (the incremental path keys off the structural sig,
                    // which can otherwise match a previous selection's).
                    self.last_struct_sig = None;
                    self.pending_damage = None;
                } else {
                    self.drag_anchor = None;
                }
            } else if self.sel.w >= 6.0 * self.scale && self.sel.h >= 6.0 * self.scale {
                self.have_sel = true;
                self.last_struct_sig = None;
                self.pending_damage = None;
            } else {
                self.drag_anchor = None;
            }
        } else if let Some(a) = self.drawing.take() {
            let bbox = self.annotation_bbox(&a);
            let valid = match a.tool {
                Tool::Rect | Tool::Ellipse => {
                    let r = Rect::from_points(a.p0.0, a.p0.1, a.p1.0, a.p1.1);
                    r.w > 3.0 && r.h > 3.0
                }
                Tool::Line | Tool::Arrow => {
                    ((a.p1.0 - a.p0.0).powi(2) + (a.p1.1 - a.p0.1).powi(2)).sqrt() > 3.0
                }
                Tool::Pen | Tool::Marker => a.points.len() > 1,
                Tool::Eraser => !a.points.is_empty(),
                Tool::Mosaic => !a.cells.is_empty(),
                _ => false,
            };
            if valid {
                self.push_undo();
                self.annotations.push(a);
            } else {
                // Discarded (too small): the last incremental frame still shows the
                // stub — damage its box so it gets erased on the next present.
                self.add_damage(bbox.0, bbox.1, bbox.2, bbox.3);
            }
        }
        self.active_handle = Handle::None;
        let _ = (x, y);
        self.request_redraw();
    }

    fn on_toolbar(&mut self, id: ButtonId) {
        match id {
            ButtonId::Tool(i) => {
                let t = Tool::ALL[i];
                self.tool = if self.tool == Some(t) { None } else { Some(t) };
                self.commit_text();
            }
            ButtonId::Color(i) => {
                self.color_idx = i;
                // Recolor the in-progress text box live.
                if let Some(ti) = self.text_input.as_mut() {
                    ti.color = PALETTE[i];
                }
            }
            ButtonId::Width(i) => self.width_idx = i,
            ButtonId::Undo => self.do_undo(),
            ButtonId::Redo => self.do_redo(),
            ButtonId::Cancel => {
                self.outcome = Outcome::Cancelled;
                self.finish();
                return;
            }
            ButtonId::Save => {
                self.commit_text();
                self.finalize(true, false);
                return;
            }
            ButtonId::Copy => {
                // 复制：仅写入系统剪贴板并关闭，不回传宿主（不插入提问框）。
                self.commit_text();
                self.finalize(false, true);
                return;
            }
            ButtonId::Confirm => {
                self.commit_text();
                self.finalize(false, false);
                return;
            }
        }
        self.request_redraw();
    }

    /// Open the text editor at `pos` with initial `buffer`/`caret`. `editing` is
    /// the index of an existing annotation being re-edited (it is hidden while
    /// the editor is open and replaced on commit), or `None` for a new box.
    #[allow(clippy::too_many_arguments)]
    fn open_text_input(
        &mut self,
        pos: (f32, f32),
        buffer: String,
        caret: usize,
        font_px: f32,
        color: [u8; 4],
        angle: f32,
        editing: Option<usize>,
    ) {
        self.text_input = Some(TextInput {
            pos,
            buffer: buffer.clone(),
            preedit: String::new(),
            caret,
            anchor: None,
            selecting: false,
            font_px,
            angle,
            color,
            editing,
            ime_seen: false,
        });
        // New editing session → force the next `update_ime_cursor` to push a
        // fresh candidate-box position (the dedupe cache is keyed on the area).
        self.last_ime_area.set(None);
        self.caret_on = true;
        self.last_blink = Instant::now();
        if let Some(w) = &self.window {
            w.set_ime_allowed(true);
            // macOS: drop the overlay below the IME candidate level so the 候选条
            // is visible while typing (no-op on other platforms).
            crate::macos::set_text_input_active(w, true);
            // macOS: back the text tool with a real NSTextView so the system IME
            // candidate panel actually shows (the hand-rolled editor never gets
            // it inside our overlay). It owns editing + IME while active; we read
            // its string back on commit. No-op on other platforms.
            crate::macos::begin_native_text(w, pos.0, pos.1, font_px, self.scale, color, &buffer);
        }
        self.update_ime_cursor();
    }

    /// Line height (physical px) for the current text font.
    fn line_height(&self, font_px: f32) -> f32 {
        self.text
            .as_ref()
            .map(|tr| tr.line_height(font_px))
            .unwrap_or(font_px * 1.3)
    }

    /// (max line advance width, total height) of `text` at `font_px`.
    fn text_extent(&self, text: &str, font_px: f32) -> (f32, f32) {
        let lh = self.line_height(font_px);
        let lines: Vec<&str> = text.split('\n').collect();
        let w = lines
            .iter()
            .map(|l| {
                self.text
                    .as_ref()
                    .map(|tr| tr.measure(l, font_px))
                    .unwrap_or(l.chars().count() as f32 * font_px * 0.6)
            })
            .fold(0.0_f32, f32::max);
        (w, lines.len().max(1) as f32 * lh)
    }

    /// Top-left pixel of the caret within the editor (ignoring preedit).
    fn caret_pixel(&self, ti: &TextInput) -> (f32, f32) {
        let (line, col) = ti.caret_line_col();
        let line_str = ti.buffer.split('\n').nth(line).unwrap_or("");
        let prefix: String = line_str.chars().take(col).collect();
        let x = ti.pos.0
            + self
                .text
                .as_ref()
                .map(|tr| tr.measure(&prefix, ti.font_px))
                .unwrap_or(0.0);
        let y = ti.pos.1 + line as f32 * self.line_height(ti.font_px);
        (x, y)
    }

    /// Char index nearest the pixel (x, y) within the editor.
    fn text_index_at(&self, ti: &TextInput, x: f32, y: f32) -> usize {
        // Map the click back into the box's upright frame before locating it.
        let (x, y) = if ti.angle != 0.0 {
            let (c, _, _, ang) = self.editor_box(ti);
            annotations::rotate_point((x, y), c, -ang)
        } else {
            (x, y)
        };
        let lh = self.line_height(ti.font_px);
        let rel_y = y - ti.pos.1;
        let line = if rel_y <= 0.0 { 0 } else { (rel_y / lh) as usize };
        let lines: Vec<&str> = ti.buffer.split('\n').collect();
        let line = line.min(lines.len().saturating_sub(1));
        let line_str = lines.get(line).copied().unwrap_or("");
        let relx = x - ti.pos.0;
        let mut col = line_str.chars().count();
        if let Some(tr) = &self.text {
            let mut acc = 0.0;
            for (i, ch) in line_str.chars().enumerate() {
                let cw = tr.measure(&ch.to_string(), ti.font_px);
                if relx < acc + cw / 2.0 {
                    col = i;
                    break;
                }
                acc += cw;
            }
        }
        ti.index_at(line, col)
    }

    /// Keep the IME candidate window anchored to the caret.
    ///
    /// Only re-issues `set_ime_cursor_area` when the area actually moves. This is
    /// essential on Wayland+fcitx5 (统信/UOS): re-committing the text-input on
    /// every IME event makes fcitx re-emit an empty `Preedit` — a self-driving
    /// loop that resets composition, so Chinese never advances to `Ime::Commit`.
    /// The caret doesn't move during pinyin composition (app-side preedit stays
    /// empty there), so deduping breaks the loop and lets the commit through.
    fn update_ime_cursor(&self) {
        if let (Some(ti), Some(w)) = (&self.text_input, &self.window) {
            let (mut cx, mut cy) = self.caret_pixel(ti);
            // Anchor the candidate panel at the rotated caret position.
            if ti.angle != 0.0 {
                let (c, _, _, ang) = self.editor_box(ti);
                let (rx, ry) = annotations::rotate_point((cx, cy), c, ang);
                cx = rx;
                cy = ry;
            }
            let wpx = (8.0 * self.scale).round() as i32;
            let hpx = (ti.font_px + 6.0).round() as i32;
            let area = (cx.round() as i32, cy.round() as i32, wpx, hpx);
            if self.last_ime_area.get() == Some(area) {
                return;
            }
            self.last_ime_area.set(Some(area));
            w.set_ime_cursor_area(
                PhysicalPosition::new(area.0 as f64, area.1 as f64),
                PhysicalSize::new(area.2 as f64, area.3 as f64),
            );
        }
    }

    // ─────────────── text transform gizmo ───────────────

    /// The editing text box as `(center, half_width, half_height, angle)` in
    /// screen pixels. The center matches the rotation pivot used when rendering.
    fn editor_box(&self, ti: &TextInput) -> ((f32, f32), f32, f32, f32) {
        let display = ti.display();
        let (w, h) = self.text_extent(&display, ti.font_px);
        let pad = 6.0 * self.scale;
        let min_w = 12.0 * self.scale;
        let we = w.max(min_w);
        let cx = ti.pos.0 + w / 2.0;
        let cy = ti.pos.1 + h / 2.0;
        ((cx, cy), we / 2.0 + pad, h / 2.0 + pad, ti.angle)
    }

    /// Gizmo handles as `(kind, screen_pos)`, already rotated with the box.
    fn text_handles(&self, ti: &TextInput) -> [(TextHandle, (f32, f32)); 9] {
        let (c, hw, hh, ang) = self.editor_box(ti);
        let rot = |lx: f32, ly: f32| annotations::rotate_point((c.0 + lx, c.1 + ly), c, ang);
        let rot_gap = 22.0 * self.scale;
        [
            (TextHandle::Resize, rot(-hw, -hh)),       // TL
            (TextHandle::Resize, rot(0.0, -hh)),       // T
            (TextHandle::Close, rot(hw, -hh)),         // TR → close
            (TextHandle::Resize, rot(hw, 0.0)),        // R
            (TextHandle::Resize, rot(hw, hh)),         // BR
            (TextHandle::Resize, rot(0.0, hh)),        // B
            (TextHandle::Resize, rot(-hw, hh)),        // BL
            (TextHandle::Resize, rot(-hw, 0.0)),       // L
            (TextHandle::Rotate, rot(0.0, -hh - rot_gap)), // rotation handle
        ]
    }

    /// Which gizmo handle (or frame band) is under (x, y) while editing.
    fn text_handle_at(&self, ti: &TextInput, x: f32, y: f32) -> TextHandle {
        let tol = 11.0 * self.scale;
        // Rotate + close take priority, then the resize handles.
        for (h, (hx, hy)) in self.text_handles(ti) {
            if (x - hx).abs() <= tol && (y - hy).abs() <= tol {
                return h;
            }
        }
        if self.text_on_frame(ti, x, y) {
            return TextHandle::MoveBox;
        }
        TextHandle::None
    }

    /// Whether (x, y) lies on the (rotated) box outline band → drag to move.
    fn text_on_frame(&self, ti: &TextInput, x: f32, y: f32) -> bool {
        let (c, hw, hh, ang) = self.editor_box(ti);
        let (lx, ly) = annotations::rotate_point((x, y), c, -ang);
        let dx = (lx - c.0).abs();
        let dy = (ly - c.1).abs();
        let band = 5.0 * self.scale;
        let on_v = (dx - hw).abs() <= band && dy <= hh + band;
        let on_h = (dy - hh).abs() <= band && dx <= hw + band;
        on_v || on_h
    }

    /// Whether (x, y) is inside the (rotated) editor box.
    fn text_box_contains(&self, ti: &TextInput, x: f32, y: f32) -> bool {
        let (c, hw, hh, ang) = self.editor_box(ti);
        let (lx, ly) = annotations::rotate_point((x, y), c, -ang);
        (lx - c.0).abs() <= hw && (ly - c.1).abs() <= hh
    }

    /// Begin a gizmo drag (rotate / resize / move) grabbed at (x, y).
    fn begin_gizmo(&mut self, kind: TextHandle, x: f32, y: f32) {
        let (c, init_font, init_angle, init_pos) = {
            let Some(ti) = self.text_input.as_ref() else {
                return;
            };
            let (c, _, _, ang) = self.editor_box(ti);
            (c, ti.font_px, ang, ti.pos)
        };
        let init_ptr_angle = (y - c.1).atan2(x - c.0);
        let init_dist = ((x - c.0).powi(2) + (y - c.1).powi(2)).sqrt().max(1.0);
        self.gizmo = Some(GizmoDrag {
            kind,
            center: c,
            init_font,
            init_angle,
            init_ptr_angle,
            init_dist,
            init_pos,
            grab: (x, y),
        });
    }

    /// Update an in-progress gizmo drag from the current pointer (x, y).
    fn drag_gizmo(&mut self, x: f32, y: f32) {
        let Some(g) = self.gizmo else { return };
        match g.kind {
            TextHandle::Rotate => {
                let cur = (y - g.center.1).atan2(x - g.center.0);
                let mut a = g.init_angle + (cur - g.init_ptr_angle);
                // Snap near the cardinal angles for easy straightening.
                let snap = 2.5_f32.to_radians();
                let step = std::f32::consts::FRAC_PI_2;
                let nearest = (a / step).round() * step;
                if (a - nearest).abs() < snap {
                    a = nearest;
                }
                if let Some(ti) = self.text_input.as_mut() {
                    ti.angle = a;
                }
            }
            TextHandle::Resize => {
                let cur = ((x - g.center.0).powi(2) + (y - g.center.1).powi(2)).sqrt();
                let ratio = (cur / g.init_dist).clamp(0.15, 12.0);
                let new_font = (g.init_font * ratio).clamp(8.0 * self.scale, 400.0 * self.scale);
                // Re-measure with the new font, then keep the box centered so it
                // scales symmetrically about the grab-time center.
                let display = match self.text_input.as_ref() {
                    Some(ti) => ti.display(),
                    None => return,
                };
                let (w, h) = self.text_extent(&display, new_font);
                let pos = (g.center.0 - w / 2.0, g.center.1 - h / 2.0);
                if let Some(ti) = self.text_input.as_mut() {
                    ti.font_px = new_font;
                    ti.pos = pos;
                }
            }
            TextHandle::MoveBox => {
                let dx = x - g.grab.0;
                let dy = y - g.grab.1;
                if let Some(ti) = self.text_input.as_mut() {
                    ti.pos = (g.init_pos.0 + dx, g.init_pos.1 + dy);
                }
            }
            _ => {}
        }
        self.update_ime_cursor();
    }

    /// Close the editor via the gizmo's ✕: delete the annotation when re-editing
    /// an existing one, otherwise just discard the new (uncommitted) text box.
    fn close_text_editor(&mut self) {
        if let Some(idx) = self.text_input.as_ref().and_then(|ti| ti.editing) {
            if idx < self.annotations.len() {
                self.push_undo();
                self.annotations.remove(idx);
            }
        }
        self.cancel_text();
    }

    /// Top-most text annotation under (x, y), if any.
    fn text_annotation_at(&self, x: f32, y: f32) -> Option<usize> {
        self.annotation_at(x, y, true)
    }

    fn commit_text(&mut self) {
        self.gizmo = None;
        let Some(ti) = self.text_input.take() else {
            return;
        };
        // On macOS the live text lives in the native NSTextView — read it back,
        // then tear the native editor down. Falls back to our buffer elsewhere.
        let native = crate::macos::native_text_string();
        if let Some(w) = &self.window {
            w.set_ime_allowed(false);
            crate::macos::set_text_input_active(w, false);
            crate::macos::end_native_text(w);
        }
        let source = native.unwrap_or_else(|| ti.buffer.clone());
        let s = source.trim_end_matches('\n').to_string();
        match ti.editing {
            // Re-edit of an existing annotation: replace in place (or delete if
            // cleared), preserving its color.
            Some(idx) if idx < self.annotations.len() => {
                self.push_undo();
                if s.is_empty() {
                    self.annotations.remove(idx);
                } else {
                    self.annotations[idx].text = s;
                    self.annotations[idx].p0 = ti.pos;
                    self.annotations[idx].font_px = ti.font_px;
                    self.annotations[idx].angle = ti.angle;
                    self.annotations[idx].color = ti.color;
                }
            }
            _ => {
                if !s.is_empty() {
                    self.push_undo();
                    let mut a = Annotation::new(Tool::Text, ti.color, self.width());
                    a.p0 = ti.pos;
                    a.font_px = ti.font_px;
                    a.angle = ti.angle;
                    a.text = s;
                    self.annotations.push(a);
                }
            }
        }
    }

    fn cancel_text(&mut self) {
        self.gizmo = None;
        if self.text_input.take().is_some() {
            if let Some(w) = &self.window {
                w.set_ime_allowed(false);
                crate::macos::set_text_input_active(w, false);
                crate::macos::end_native_text(w);
            }
        }
    }

    fn finish(&mut self) {
        if let Some(w) = &self.window {
            // Tear down any native text editor still on screen (e.g. cancelled
            // via the toolbar, which doesn't route through cancel_text).
            crate::macos::end_native_text(w);
            w.set_visible(false);
        }
        // Drop the GPU surface before the window it borrows.
        #[cfg(target_os = "macos")]
        {
            self.gpu = None;
        }
        // event loop will exit on next poll via about_to_wait
        self.window = None;
    }

    /// Build the final RGBA of the selection + annotations.
    fn finalize(&mut self, save: bool, copy_only: bool) {
        let s = self.sel;
        let w = s.w.round().max(1.0) as u32;
        let h = s.h.round().max(1.0) as u32;

        let mut out = match Pixmap::new(w, h) {
            Some(p) => p,
            None => {
                self.outcome = Outcome::Cancelled;
                self.finish();
                return;
            }
        };

        // base crop from frozen snapshot
        let sx0 = s.x.round() as i32;
        let sy0 = s.y.round() as i32;
        {
            let data = out.data_mut();
            for row in 0..h {
                for col in 0..w {
                    let (r, g, b, _) = self.snap.pixel(sx0 + col as i32, sy0 + row as i32);
                    let idx = ((row * w + col) * 4) as usize;
                    // tiny-skia pixmap is premultiplied RGBA; opaque so direct copy ok
                    data[idx] = r;
                    data[idx + 1] = g;
                    data[idx + 2] = b;
                    data[idx + 3] = 255;
                }
            }
        }

        // annotations translated so selection origin → (0,0)
        if self.has_eraser() {
            self.ensure_snap_px();
        }
        let snap_px = self.snap_px_ref();
        for a in &self.annotations {
            annotations::draw(&mut out, a, self.text.as_ref(), snap_px, -s.x, -s.y);
        }

        // tiny-skia stores premultiplied RGBA; our content is opaque so it equals straight RGBA
        let rgba = out.data().to_vec();
        self.outcome = Outcome::Capture {
            rgba,
            width: w,
            height: h,
            save,
            copy_only,
        };
        self.finish();
    }

    // ─────────────── rendering ───────────────

    fn render(&mut self) {
        let have_target = {
            #[cfg(target_os = "macos")]
            {
                self.surface.is_some() || self.gpu.is_some()
            }
            #[cfg(not(target_os = "macos"))]
            {
                self.surface.is_some()
            }
        };
        if self.window.is_none() || !have_target {
            return;
        }
        // Frame pacing: drop presents that arrive faster than `min_frame`. The
        // latest state is captured on the next allowed frame; a trailing
        // deferred frame is flushed by `pump_frame` from the event loop.
        if let Some(lp) = self.last_present {
            if lp.elapsed() < self.min_frame {
                self.redraw_pending = true;
                return;
            }
        }
        let (pw, ph) = (self.snap.width, self.snap.height);

        // Dragging a new selection: incremental damage updates only.
        if self.dragging && self.moved && !self.have_sel {
            self.render_drag_selection();
            return;
        }

        // Selection confirmed (Adjusting/Annotating): moving/resizing/drawing all
        // recompose just the changed box instead of the whole desktop.
        if self.have_sel {
            self.render_have_sel();
            return;
        }

        // Magnifier follows the cursor before a selection exists. Keep the
        // magnifier fast path while the button is down but the drag threshold
        // has not been exceeded yet.
        let mag_fast = !self.have_sel
            && (!self.dragging || !self.moved)
            && self.drawing.is_none()
            && self.text_input.is_none();
        let sig = self.bg_signature();
        let bg_stale = self.bg.is_none() || Some(sig) != self.bg_sig;

        if !mag_fast || bg_stale {
            self.last_sel_render = None;
            // Full rebuild: recompose the magnifier-free background into `scene`.
            self.compose_bg();

            // Cache the background for the fast path.
            let mut bg = match self.bg.take() {
                Some(p) if p.width() == pw && p.height() == ph => p,
                _ => match Pixmap::new(pw, ph) {
                    Some(p) => p,
                    None => return,
                },
            };
            if let Some(scene) = self.scene.as_ref() {
                bg.data_mut().copy_from_slice(scene.data());
            }
            self.bg = Some(bg);
            self.bg_sig = Some(sig);

            // Overlay the magnifier (selecting phase only) on top of `scene`.
            let mag = if !self.have_sel {
                if let Some(mut scene) = self.scene.take() {
                    self.draw_magnifier(&mut scene);
                    self.scene = Some(scene);
                }
                Some(self.magnifier_bbox())
            } else {
                None
            };
            self.last_mag = mag;
            self.present(None);
            return;
        }

        // Hot path: only the magnifier moved. Restore its previous footprint
        // from the cached background, draw it at the new position, and present
        // just the union of the two rectangles.
        let new_mag = self.magnifier_bbox();
        let old_mag = self.last_mag;
        if let (Some(bg), Some(mut scene)) = (self.bg.take(), self.scene.take()) {
            if let Some(r) = old_mag {
                copy_region(&bg, &mut scene, r);
            }
            self.draw_magnifier(&mut scene);
            self.scene = Some(scene);
            self.bg = Some(bg);
        }
        self.last_mag = Some(new_mag);
        let dmg = union_rect(old_mag, new_mag);
        self.present(Some(dmg));
    }

    /// Incremental repaint while the user is dragging a new selection rectangle.
    fn render_drag_selection(&mut self) {
        let (pw, ph) = (self.snap.width, self.snap.height);
        let k = self.scale;
        let sel = self.sel;

        // 拖拽首帧：先整屏重建一次干净背景（整屏半透明暗遮罩 + 当前选区高亮原图），
        // 清掉起拖前“鼠标悬停窗口高亮”残留在选区之外的痕迹，确保框选时只有选区高亮、
        // 其余区域均匀变暗；之后的每一帧才走增量(damage)更新。
        if self.last_sel_render.is_none() {
            self.compose_bg();
            self.present(None);
            self.last_sel_render = Some(sel);
            return;
        }

        let old = self.last_sel_render.unwrap_or_else(|| {
            if let Some((ax, ay)) = self.drag_anchor {
                Rect::new(ax, ay, 0.0, 0.0)
            } else {
                sel
            }
        });

        let dmg = union_rect_two(
            sel_damage_rect(old, k, pw, ph),
            sel_damage_rect(sel, k, pw, ph),
        );

        let dim = match self.dim_cache.as_ref() {
            Some(c) => c,
            None => {
                self.compose_bg();
                self.present(None);
                self.last_sel_render = Some(sel);
                return;
            }
        };

        let mut scene_owned = match self.scene.take() {
            Some(s) if s.width() == pw && s.height() == ph => s,
            _ => match Pixmap::new(pw, ph) {
                Some(s) => s,
                None => return,
            },
        };

        // Dimmed background over the whole damage box, then the bright original
        // restored inside the selection ∩ damage — two memcpys, no blending.
        copy_region(dim, &mut scene_owned, dmg);
        let inner = intersect_dmg(dmg, int_rect(sel, pw, ph));
        copy_rgba_region(&self.snap.rgba, &mut scene_owned, inner);
        draw_selection_chrome(&mut scene_owned, sel, false, k, self.bounds, self.text.as_ref());

        self.scene = Some(scene_owned);
        self.last_sel_render = Some(sel);
        self.present(Some(dmg));
    }

    /// Render the Adjusting/Annotating phase (selection already made). When only
    /// geometry moved since the last frame we recompose + present just the
    /// changed box (`pending_damage`); structural changes (annotation added,
    /// tool/color switched, text editing) force one full rebuild.
    fn render_have_sel(&mut self) {
        let sig = self.struct_sig();
        let structural = self.last_struct_sig != Some(sig)
            || self.scene.is_none()
            || self.text_input.is_some();

        if !structural {
            if let Some(dmg) = self.pending_damage.take() {
                self.render_have_sel_incremental(dmg);
                return;
            }
            // Redraw requested but nothing visual changed (e.g. a press that did
            // not move yet) — re-present the existing scene without recomposing.
            self.present(None);
            return;
        }

        self.compose_bg();
        self.present(None);
        self.last_struct_sig = Some(sig);
        self.pending_damage = None;
    }

    /// Recompose only `dmg`: dimmed bg + bright selection interior + all
    /// annotations + selection chrome + toolbar, then present just that box.
    /// Drawing unclipped is safe because only `dmg` is converted/presented and
    /// every region is rebuilt from the caches before it is shown.
    fn render_have_sel_incremental(&mut self, dmg: (i32, i32, i32, i32)) {
        let (pw, ph) = (self.snap.width, self.snap.height);
        let k = self.scale;

        let mut scene = match self.scene.take() {
            Some(s) if s.width() == pw && s.height() == ph => s,
            _ => match Pixmap::new(pw, ph) {
                Some(s) => s,
                None => return,
            },
        };

        // Background: dimmed everywhere in the damage box, bright inside selection.
        if self.dim_cache.is_none() {
            // No cache yet: fall back to a full rebuild.
            self.scene = Some(scene);
            self.compose_bg();
            self.present(None);
            return;
        }
        copy_region(self.dim_cache.as_ref().unwrap(), &mut scene, dmg);
        let inner = intersect_dmg(dmg, int_rect(self.sel, pw, ph));
        copy_rgba_region(&self.snap.rgba, &mut scene, inner);

        // Annotations + in-progress drawing (geometry may straddle the damage edge;
        // the parts outside `dmg` simply aren't presented this frame).
        if self.has_eraser() {
            self.ensure_snap_px();
        }
        let snap_px = self.snap_px_ref();
        for a in &self.annotations {
            annotations::draw(&mut scene, a, self.text.as_ref(), snap_px, 0.0, 0.0);
        }
        if let Some(a) = &self.drawing {
            annotations::draw(&mut scene, a, self.text.as_ref(), snap_px, 0.0, 0.0);
        }
        self.draw_text_hover(&mut scene);

        // Selection border + handles + dimension label.
        draw_selection_chrome(&mut scene, self.sel, true, k, self.bounds, self.text.as_ref());

        // Toolbar (floats with the selection).
        let tb = toolbar::layout(self.sel, self.bounds, self.scale);
        toolbar::draw(
            &mut scene,
            &tb,
            self.text.as_ref(),
            self.tool,
            self.color_idx,
            self.width_idx,
            !self.undo.is_empty(),
            !self.redo.is_empty(),
            self.tb_hover,
            self.dark_mode,
            self.scale,
        );
        self.toolbar = Some(tb);

        self.scene = Some(scene);
        self.present(Some(dmg));
    }

    /// Structural signature: everything that forces a full rebuild. Excludes the
    /// selection rectangle and the in-progress drawing/annotation geometry, which
    /// are handled by the cheap incremental path.
    fn struct_sig(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.have_sel.hash(&mut h);
        (self.annotations.len() as u64).hash(&mut h);
        match self.tool {
            Some(t) => {
                1u8.hash(&mut h);
                std::mem::discriminant(&t).hash(&mut h);
            }
            None => 0u8.hash(&mut h),
        }
        self.color_idx.hash(&mut h);
        self.width_idx.hash(&mut h);
        self.undo.is_empty().hash(&mut h);
        self.redo.is_empty().hash(&mut h);
        self.text_input.is_some().hash(&mut h);
        h.finish()
    }

    /// Bounding box (physical px, with margin) that fully covers an annotation —
    /// used to compute damage when one is drawn or dragged.
    fn annotation_bbox(&self, a: &Annotation) -> (f32, f32, f32, f32) {
        let pad = (a.width + 8.0).max(12.0);
        match a.tool {
            Tool::Rect | Tool::Ellipse | Tool::Line | Tool::Arrow => {
                let x0 = a.p0.0.min(a.p1.0) - pad;
                let y0 = a.p0.1.min(a.p1.1) - pad;
                let x1 = a.p0.0.max(a.p1.0) + pad;
                let y1 = a.p0.1.max(a.p1.1) + pad;
                (x0, y0, x1, y1)
            }
            Tool::Pen | Tool::Marker => {
                let p = pad.max(a.width * 3.0);
                let mut x0 = f32::MAX;
                let mut y0 = f32::MAX;
                let mut x1 = f32::MIN;
                let mut y1 = f32::MIN;
                for &(px, py) in &a.points {
                    x0 = x0.min(px);
                    y0 = y0.min(py);
                    x1 = x1.max(px);
                    y1 = y1.max(py);
                }
                if x0 > x1 {
                    return (a.p0.0, a.p0.1, a.p0.0, a.p0.1);
                }
                (x0 - p, y0 - p, x1 + p, y1 + p)
            }
            Tool::Mosaic => {
                let mut x0 = f32::MAX;
                let mut y0 = f32::MAX;
                let mut x1 = f32::MIN;
                let mut y1 = f32::MIN;
                for &(cx, cy, cw, ch, _) in &a.cells {
                    x0 = x0.min(cx);
                    y0 = y0.min(cy);
                    x1 = x1.max(cx + cw);
                    y1 = y1.max(cy + ch);
                }
                if x0 > x1 {
                    return (a.p0.0, a.p0.1, a.p0.0, a.p0.1);
                }
                (x0, y0, x1, y1)
            }
            Tool::Text => {
                let (w, h) = self.text_extent(&a.text, a.font_px);
                (a.p0.0 - pad, a.p0.1 - pad, a.p0.0 + w + pad, a.p0.1 + h + pad)
            }
            Tool::Serial => {
                let d = (a.width * 5.0).max(26.0);
                (a.p0.0 - d, a.p0.1 - d, a.p0.0 + d, a.p0.1 + d)
            }
            Tool::Eraser => {
                let p = annotations::eraser_diameter(a.width) / 2.0 + 2.0;
                let mut x0 = f32::MAX;
                let mut y0 = f32::MAX;
                let mut x1 = f32::MIN;
                let mut y1 = f32::MIN;
                for &(px, py) in &a.points {
                    x0 = x0.min(px);
                    y0 = y0.min(py);
                    x1 = x1.max(px);
                    y1 = y1.max(py);
                }
                if x0 > x1 {
                    return (a.p0.0, a.p0.1, a.p0.0, a.p0.1);
                }
                (x0 - p, y0 - p, x1 + p, y1 + p)
            }
        }
    }

    /// Generous damage rect (physical px) covering a text annotation's rotated
    /// box plus its hover/transform handles, used to repaint hint chrome without
    /// a full recompose.
    fn text_anno_damage(&self, a: &Annotation) -> (f32, f32, f32, f32) {
        let (w, h) = self.text_extent(&a.text, a.font_px);
        let cx = a.p0.0 + w / 2.0;
        let cy = a.p0.1 + h / 2.0;
        let pad = 8.0 * self.scale;
        // Half-diagonal bounds any rotation; extra room for the rotate handle.
        let half = ((w / 2.0 + pad).powi(2) + (h / 2.0 + pad).powi(2)).sqrt() + 34.0 * self.scale;
        (cx - half, cy - half, cx + half, cy + half)
    }

    /// Top-most annotation under (x, y), searching newest first. When
    /// `text_only` is set, only text annotations are considered.
    fn annotation_at(&self, x: f32, y: f32, text_only: bool) -> Option<usize> {
        for i in (0..self.annotations.len()).rev() {
            let a = &self.annotations[i];
            if text_only && a.tool != Tool::Text {
                continue;
            }
            if annotations::hit_test(a, x, y) {
                return Some(i);
            }
        }
        None
    }

    /// Accumulate `r` (a logical-px rect, possibly with a label/handle margin
    /// baked in) into the pending damage box for the next incremental present.
    fn add_damage(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let (pw, ph) = (self.snap.width as i32, self.snap.height as i32);
        let rx = (x0.floor() as i32).max(0);
        let ry = (y0.floor() as i32).max(0);
        let rr = (x1.ceil() as i32).min(pw);
        let rb = (y1.ceil() as i32).min(ph);
        if rr <= rx || rb <= ry {
            return;
        }
        let d = (rx, ry, rr - rx, rb - ry);
        self.pending_damage = Some(match self.pending_damage {
            Some(p) => union_rect(Some(p), d),
            None => d,
        });
    }

    /// Damage for a selection rect including its chrome (border, handles, label).
    fn add_sel_damage(&mut self, sel: Rect) {
        let m = 12.0 * self.scale;
        let label = 24.0 * self.scale;
        self.add_damage(sel.x - m, sel.y - label - m, sel.right() + m, sel.bottom() + m);
        // The toolbar floats with the selection — include its footprint too.
        let tb = toolbar::layout(sel, self.bounds, self.scale);
        let tm = 4.0 * self.scale;
        self.add_damage(
            tb.rect.x - tm,
            tb.rect.y - tm,
            tb.rect.right() + tm,
            tb.rect.bottom() + tm * 2.0,
        );
    }

    /// Bounding box (physical px) of the magnifier widget at the current cursor,
    /// mirroring the geometry in [`Overlay::draw_magnifier`], plus a small margin.
    fn magnifier_bbox(&self) -> (i32, i32, i32, i32) {
        let k = self.scale;
        let zoom = ((8.0 * k).round() as i32).max(1);
        let dst_w = (16 * zoom) as f32;
        let dst_h = (12 * zoom) as f32;
        let mut bx = self.mouse.0 + 18.0 * k;
        let mut by = self.mouse.1 + 18.0 * k;
        if bx + dst_w + 16.0 * k > self.bounds.right() {
            bx = self.mouse.0 - dst_w - 18.0 * k;
        }
        if by + dst_h + 56.0 * k > self.bounds.bottom() {
            by = self.mouse.1 - dst_h - 56.0 * k;
        }
        bx = bx.max(2.0 * k);
        by = by.max(2.0 * k);
        let info_h = 52.0 * k;
        let m = 3.0 * k;
        (
            (bx - m).floor() as i32,
            (by - m).floor() as i32,
            (dst_w + m * 2.0).ceil() as i32,
            (dst_h + info_h + m * 2.0).ceil() as i32,
        )
    }

    /// Signature of all state that affects the cached background (everything
    /// except the magnifier). When it is unchanged we take the fast path.
    fn bg_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.have_sel.hash(&mut h);
        self.dragging.hash(&mut h);
        (
            self.sel.x.to_bits(),
            self.sel.y.to_bits(),
            self.sel.w.to_bits(),
            self.sel.h.to_bits(),
        )
            .hash(&mut h);
        match self.hover_region {
            Some(r) => {
                1u8.hash(&mut h);
                (r.x.to_bits(), r.y.to_bits(), r.w.to_bits(), r.h.to_bits()).hash(&mut h);
            }
            None => 0u8.hash(&mut h),
        }
        (self.annotations.len() as u64).hash(&mut h);
        match self.tool {
            Some(t) => {
                1u8.hash(&mut h);
                std::mem::discriminant(&t).hash(&mut h);
            }
            None => 0u8.hash(&mut h),
        }
        self.color_idx.hash(&mut h);
        self.width_idx.hash(&mut h);
        self.undo.is_empty().hash(&mut h);
        self.redo.is_empty().hash(&mut h);
        h.finish()
    }

    /// Compose everything except the magnifier into `self.scene`.
    fn compose_bg(&mut self) {
        let (pw, ph) = (self.snap.width, self.snap.height);
        let k = self.scale; // HiDPI chrome scale
        // Take the framebuffer out of `self` so that the many `&self` reads
        // below (color(), draw_magnifier(), snapshot, ...) don't conflict with
        // the mutable borrow of the pixmap.
        let mut scene_owned = match self.scene.take() {
            Some(s) => s,
            None => match Pixmap::new(pw, ph) {
                Some(s) => s,
                None => return,
            },
        };
        let scene = &mut scene_owned;

        // Rectangle currently shown: confirmed selection, the active drag, or
        // (selecting phase, no drag yet) the auto-detected window under cursor.
        let sel = if self.have_sel {
            self.sel
        } else if self.dragging && self.moved {
            self.sel
        } else {
            self.hover_region.unwrap_or(Rect::default())
        };
        let show_sel = sel.w > 0.0 && sel.h > 0.0;

        // 1) dimmed snapshot everywhere (plain memcpy), then 2) restore the bright
        // original inside the selection (another memcpy). No per-pixel blending.
        let full = (0, 0, pw as i32, ph as i32);
        match self.dim_cache.as_ref() {
            Some(dim) => scene.data_mut().copy_from_slice(dim.data()),
            None => copy_rgba_region(&self.snap.rgba, scene, full),
        }
        if show_sel {
            let r = int_rect(sel, pw, ph);
            copy_rgba_region(&self.snap.rgba, scene, r);
        }

        // 3) annotations (clipped to selection via manual clip pixmap region)
        if self.have_sel {
            if self.has_eraser() {
                self.ensure_snap_px();
            }
            let snap_px = self.snap_px_ref();
            // The annotation being re-edited is shown by the live editor instead.
            let editing = self.text_input.as_ref().and_then(|ti| ti.editing);
            for (i, a) in self.annotations.iter().enumerate() {
                if Some(i) == editing {
                    continue;
                }
                annotations::draw(scene, a, self.text.as_ref(), snap_px, 0.0, 0.0);
            }
            if let Some(a) = &self.drawing {
                annotations::draw(scene, a, self.text.as_ref(), snap_px, 0.0, 0.0);
            }
            // dashed hover hint over a placed text annotation (when idle)
            self.draw_text_hover(scene);
            // in-progress text editor: multi-line text + preedit + blinking caret.
            // On macOS the live text is rendered by the native NSTextView subview
            // (which owns IME), so skip our own editor draw to avoid double text.
            if !crate::macos::native_text_active() {
                self.draw_text_editor(scene);
            }
        }

        // 4) selection border + handles + dimension label
        if show_sel {
            let accent = stroke_paint([0, 122, 255, 255]);
            stroke_rect(scene, sel, 2.0 * k, &accent);

            if self.have_sel {
                let pts = [
                    (sel.x, sel.y),
                    (sel.center_x(), sel.y),
                    (sel.right(), sel.y),
                    (sel.right(), sel.center_y()),
                    (sel.right(), sel.bottom()),
                    (sel.center_x(), sel.bottom()),
                    (sel.x, sel.bottom()),
                    (sel.x, sel.center_y()),
                ];
                let white = {
                    let mut p = Paint::default();
                    p.set_color_rgba8(255, 255, 255, 255);
                    p.anti_alias = true;
                    p
                };
                let hs = 4.0 * k;
                let hd = 8.0 * k;
                for (hx, hy) in pts {
                    fill_rect(scene, hx - hs, hy - hs, hd, hd, &white);
                    stroke_rect(scene, Rect::new(hx - hs, hy - hs, hd, hd), 1.5 * k, &accent);
                }
            }

            if let Some(tr) = &self.text {
                let label = format!("{} × {}", sel.w.round() as i32, sel.h.round() as i32);
                let fpx = 14.0 * k;
                let tw = tr.measure(&label, fpx);
                let pad = 6.0 * k;
                let bw = tw + pad * 2.0;
                let bh = fpx + pad * 1.5;
                let mut lx = sel.x;
                let mut ly = sel.y - bh - 6.0 * k;
                if ly < 2.0 * k {
                    ly = sel.y + 4.0 * k;
                }
                lx = lx.min(self.bounds.right() - bw - 2.0 * k).max(2.0 * k);
                let bg = {
                    let mut p = Paint::default();
                    p.set_color_rgba8(0, 0, 0, 200);
                    p
                };
                if let Some(p) = toolbar::rounded_rect(lx, ly, bw, bh, 4.0 * k) {
                    scene.fill_path(&p, &bg, FillRule::Winding, Transform::identity(), None);
                }
                tr.draw(scene, &label, lx + pad, ly + pad * 0.5, fpx, [255, 255, 255, 255]);
            }
        }

        // 5) toolbar
        if self.have_sel {
            let tb = toolbar::layout(sel, self.bounds, self.scale);
            toolbar::draw(
                scene,
                &tb,
                self.text.as_ref(),
                self.tool,
                self.color_idx,
                self.width_idx,
                !self.undo.is_empty(),
                !self.redo.is_empty(),
                self.tb_hover,
                self.dark_mode,
                self.scale,
            );
            self.toolbar = Some(tb);
        } else {
            self.toolbar = None;
        }

        self.scene = Some(scene_owned);
    }

    /// Draw the live text editor: multi-line text, the IME preedit inserted at
    /// the caret, and a blinking caret.
    fn draw_text_editor(&self, scene: &mut Pixmap) {
        let (Some(ti), Some(tr)) = (self.text_input.as_ref(), self.text.as_ref()) else {
            return;
        };
        let fpx = ti.font_px;
        let lh = tr.line_height(fpx);
        let color = ti.color;

        // Visual text = buffer with the preedit inserted at the caret.
        let display = ti.display();

        // Rotation pivot (matches editor_box / committed rendering) + transform.
        let (center, _, _, ang) = self.editor_box(ti);
        let tf = if ang != 0.0 {
            Transform::from_rotate_at(ang.to_degrees(), center.0, center.1)
        } else {
            Transform::identity()
        };
        // Map an upright point into the rotated frame (for caret / selection).
        let rp = |p: (f32, f32)| annotations::rotate_point(p, center, ang);

        // Selection highlight (only when not composing).
        if ti.preedit.is_empty() {
            if let Some((s, e)) = ti.sel_range() {
                let hl = [0, 122, 255, 90];
                let mut idx = 0usize;
                for (li, line) in ti.buffer.split('\n').enumerate() {
                    let llen = line.chars().count();
                    let line_start = idx;
                    let line_end = idx + llen; // exclusive, excludes '\n'
                    let a = s.max(line_start);
                    let b = e.min(line_end);
                    let spans_through = s <= line_start && e > line_end;
                    if a < b || spans_through {
                        let ca = a.saturating_sub(line_start);
                        let cend = b.saturating_sub(line_start).min(llen);
                        let pre: String = line.chars().take(ca).collect();
                        let mid: String = line.chars().skip(ca).take(cend.saturating_sub(ca)).collect();
                        let x0 = ti.pos.0 + tr.measure(&pre, fpx);
                        let mut w = tr.measure(&mid, fpx);
                        if e > line_end {
                            w += fpx * 0.3; // hint that the newline is selected
                        }
                        let ly = ti.pos.1 + li as f32 * lh;
                        let w = w.max(1.0);
                        fill_quad(
                            scene,
                            [rp((x0, ly)), rp((x0 + w, ly)), rp((x0 + w, ly + lh)), rp((x0, ly + lh))],
                            hl,
                        );
                    }
                    idx = line_end + 1; // skip the '\n'
                }
            }
        }

        for (i, line) in display.split('\n').enumerate() {
            let ly = ti.pos.1 + i as f32 * lh;
            // subtle dark halo for readability, then the colored glyphs
            tr.draw_transform(scene, line, ti.pos.0 + 1.0, ly + 1.0, fpx, [0, 0, 0, 130], tf);
            tr.draw_transform(scene, line, ti.pos.0, ly, fpx, color, tf);
        }

        if self.caret_on {
            // Visual caret sits just after any preedit text.
            let vcaret = ti.caret + ti.preedit.chars().count();
            let (line, col) = line_col(&display, vcaret);
            let line_str = display.split('\n').nth(line).unwrap_or("");
            let prefix: String = line_str.chars().take(col).collect();
            let cx = ti.pos.0 + tr.measure(&prefix, fpx);
            let cy = ti.pos.1 + line as f32 * lh;
            let cw = (2.0 * self.scale).max(1.0);
            let chh = fpx + 2.0 * self.scale;
            fill_quad(
                scene,
                [rp((cx, cy)), rp((cx + cw, cy)), rp((cx + cw, cy + chh)), rp((cx, cy + chh))],
                [color[0], color[1], color[2], 255],
            );
        }

        // Transform gizmo (border + resize handles + close + rotation handle).
        self.draw_editor_gizmo(scene, ti);
    }

    /// Draw the editing transform box: rotated outline, resize handles (corners +
    /// edges), the top-right ✕ close button, and the rotation handle above it.
    fn draw_editor_gizmo(&self, scene: &mut Pixmap, ti: &TextInput) {
        let k = self.scale;
        let (c, hw, hh, ang) = self.editor_box(ti);
        let rot = |lx: f32, ly: f32| annotations::rotate_point((c.0 + lx, c.1 + ly), c, ang);
        let accent = [10, 132, 255, 255];

        // Rotated frame outline.
        let tl = rot(-hw, -hh);
        let tr_c = rot(hw, -hh);
        let br = rot(hw, hh);
        let bl = rot(-hw, hh);
        let outline = stroke_paint([accent[0], accent[1], accent[2], 235]);
        let mut pb = PathBuilder::new();
        pb.move_to(tl.0, tl.1);
        pb.line_to(tr_c.0, tr_c.1);
        pb.line_to(br.0, br.1);
        pb.line_to(bl.0, bl.1);
        pb.close();
        if let Some(p) = pb.finish() {
            let st = Stroke { width: 1.5 * k, ..Default::default() };
            scene.stroke_path(&p, &outline, &st, Transform::identity(), None);
        }

        // Stalk + rotation knob above the top edge.
        let top_mid = rot(0.0, -hh);
        let knob = rot(0.0, -hh - 22.0 * k);
        line(scene, top_mid, knob, 1.5 * k, &outline);
        draw_handle_circle(scene, knob, 5.5 * k, k, accent);

        // Resize handles (white square, accent ring) at corners + edge midpoints.
        for (lx, ly) in [
            (-hw, -hh), (0.0, -hh), (hw, 0.0), (hw, hh), (0.0, hh), (-hw, hh), (-hw, 0.0),
        ] {
            draw_handle_square(scene, rot(lx, ly), 4.0 * k, k, accent);
        }

        // Close button at the top-right corner (red disc with white ✕).
        let close = rot(hw, -hh);
        draw_close_button(scene, close, 8.0 * k, k);
    }

    /// Dashed outline around the placed text annotation under the cursor — the
    /// "click to edit" affordance. Skipped while an editor is open.
    fn draw_text_hover(&self, scene: &mut Pixmap) {
        if self.text_input.is_some() {
            return;
        }
        let Some(idx) = self.hover_text else { return };
        let Some(a) = self.annotations.get(idx) else { return };
        if a.tool != Tool::Text {
            return;
        }
        let (w, h) = self.text_extent(&a.text, a.font_px);
        let pad = 5.0 * self.scale;
        let cx = a.p0.0 + w / 2.0;
        let cy = a.p0.1 + h / 2.0;
        let hw = w / 2.0 + pad;
        let hh = h / 2.0 + pad;
        let rot = |lx: f32, ly: f32| annotations::rotate_point((cx + lx, cy + ly), (cx, cy), a.angle);
        let pts = [rot(-hw, -hh), rot(hw, -hh), rot(hw, hh), rot(-hw, hh)];
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        pb.line_to(pts[1].0, pts[1].1);
        pb.line_to(pts[2].0, pts[2].1);
        pb.line_to(pts[3].0, pts[3].1);
        pb.close();
        if let Some(p) = pb.finish() {
            let dash = tiny_skia::StrokeDash::new(vec![5.0 * self.scale, 4.0 * self.scale], 0.0);
            let st = Stroke {
                width: 1.4 * self.scale,
                dash,
                ..Default::default()
            };
            scene.stroke_path(&p, &stroke_paint([10, 132, 255, 215]), &st, Transform::identity(), None);
        }
    }

    fn draw_magnifier(&self, scene: &mut Pixmap) {
        let k = self.scale;
        let (mx, my) = (self.mouse.0 as i32, self.mouse.1 as i32);
        let zoom = ((8.0 * k).round() as i32).max(1);
        let src_w = 16i32;
        let src_h = 12i32;
        let dst_w = (src_w * zoom) as f32;
        let dst_h = (src_h * zoom) as f32;
        let mut bx = self.mouse.0 + 18.0 * k;
        let mut by = self.mouse.1 + 18.0 * k;
        if bx + dst_w + 16.0 * k > self.bounds.right() {
            bx = self.mouse.0 - dst_w - 18.0 * k;
        }
        if by + dst_h + 56.0 * k > self.bounds.bottom() {
            by = self.mouse.1 - dst_h - 56.0 * k;
        }
        bx = bx.max(2.0 * k);
        by = by.max(2.0 * k);

        // zoomed pixels (nearest)
        {
            let data = scene.data_mut();
            let pw = self.snap.width as i32;
            for ry in 0..(dst_h as i32) {
                for rx in 0..(dst_w as i32) {
                    let sx = mx - src_w / 2 + rx / zoom;
                    let sy = my - src_h / 2 + ry / zoom;
                    let (r, g, b, _) = self.snap.pixel(sx, sy);
                    let px = bx as i32 + rx;
                    let py = by as i32 + ry;
                    if px < 0 || py < 0 || px >= pw || py >= self.snap.height as i32 {
                        continue;
                    }
                    let idx = ((py * pw + px) * 4) as usize;
                    data[idx] = r;
                    data[idx + 1] = g;
                    data[idx + 2] = b;
                    data[idx + 3] = 255;
                }
            }
        }

        let frame = stroke_paint([255, 255, 255, 255]);
        stroke_rect(scene, Rect::new(bx, by, dst_w, dst_h), 1.0 * k, &frame);
        // crosshair
        let cross = stroke_paint([0, 122, 255, 180]);
        let cxp = bx + dst_w / 2.0;
        let cyp = by + dst_h / 2.0;
        line(scene, (cxp, by), (cxp, by + dst_h), 1.0 * k, &cross);
        line(scene, (bx, cyp), (bx + dst_w, cyp), 1.0 * k, &cross);

        // color readout
        let (r, g, b, _) = self.snap.pixel(mx, my);
        let info_h = 52.0 * k;
        let info_bg = {
            let mut p = Paint::default();
            p.set_color_rgba8(20, 20, 24, 220);
            p
        };
        fill_rect(scene, bx, by + dst_h, dst_w, info_h, &info_bg);
        let sw = {
            let mut p = Paint::default();
            p.set_color_rgba8(r, g, b, 255);
            p
        };
        fill_rect(scene, bx + 6.0 * k, by + dst_h + 8.0 * k, 14.0 * k, 14.0 * k, &sw);
        if let Some(tr) = &self.text {
            // Line 1: the color value in the currently selected format.
            let l1 = self.color_format.format(r, g, b);
            // Line 2: hint for the two interactions.
            let l2 = "Shift 切换  C 复制";
            tr.draw(scene, &l1, bx + 26.0 * k, by + dst_h + 5.0 * k, 12.0 * k, [235, 235, 240, 255]);
            tr.draw(scene, l2, bx + 26.0 * k, by + dst_h + 24.0 * k, 11.0 * k, [150, 150, 158, 255]);
        }
    }

    /// Mirror `scene` into the surface and present it. With `damage = Some(rect)`
    /// only that rectangle is converted (RGBA→0RGB) and presented, which is the
    /// key to keeping cursor-move frames cheap on HiDPI screens. `None` repaints
    /// and presents the whole surface.
    fn present(&mut self, damage: Option<(i32, i32, i32, i32)>) {
        // Mark the frame as presented for pacing (see MIN_FRAME / pump_frame).
        self.last_present = Some(Instant::now());
        self.redraw_pending = false;
        let (w, h) = match self.scene.as_ref() {
            Some(s) => (s.width(), s.height()),
            None => return,
        };
        let total = (w as usize) * (h as usize);
        // A freshly (re)allocated mirror must be fully repainted.
        let full = damage.is_none() || self.present_buf.len() != total;
        if self.present_buf.len() != total {
            self.present_buf = vec![0u32; total];
        }

        // 1) RGBA(scene) -> 0RGB(present_buf), only over the dirty region.
        {
            let scene = self.scene.as_ref().unwrap();
            let src = scene.data();
            let buf = &mut self.present_buf;
            let (rx, ry, rw, rh) = if full {
                (0, 0, w as i32, h as i32)
            } else {
                damage.unwrap()
            };
            let x0 = rx.max(0);
            let y0 = ry.max(0);
            let x1 = (rx + rw).min(w as i32);
            let y1 = (ry + rh).min(h as i32);
            let stride = w as usize;
            let (x0u, x1u) = (x0 as usize, x1 as usize);
            for yy in y0..y1 {
                let row = yy as usize * stride;
                let srow = &src[(row + x0u) * 4..(row + x1u) * 4];
                let drow = &mut buf[row + x0u..row + x1u];
                // Row-at-a-time with `chunks_exact(4)` so the compiler can drop
                // per-pixel bounds checks (this is the hot path for large drags).
                // 0xAARRGGBB; little-endian bytes are BGRA8 (opaque), valid for
                // both softbuffer (0RGB, ignores alpha) and the GPU path.
                for (px, d) in srow.chunks_exact(4).zip(drow.iter_mut()) {
                    *d = 0xFF00_0000
                        | ((px[0] as u32) << 16)
                        | ((px[1] as u32) << 8)
                        | (px[2] as u32);
                }
            }
        }

        // macOS GPU path: upload only the dirty region to the scene texture, blit.
        #[cfg(target_os = "macos")]
        if self.gpu.is_some() {
            let dmg = if full { None } else { damage };
            if let Some(g) = self.gpu.as_mut() {
                g.present(&self.present_buf, dmg);
            }
            return;
        }

        // 2) Upload to the surface and present (whole, or just the dirty rect).
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .is_err()
        {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };

        // macOS softbuffer may hand back a zeroed buffer each frame; always mirror
        // the full present_buf before presenting (CPU still only converts the dirty
        // region above — this only restores the GPU upload path).
        buffer.copy_from_slice(&self.present_buf);

        if full {
            let _ = buffer.present();
            return;
        }

        let (dx, dy, dw, dh) = damage.unwrap();
        let x0 = dx.max(0);
        let y0 = dy.max(0);
        let x1 = (dx + dw).min(w as i32);
        let y1 = (dy + dh).min(h as i32);
        if x1 <= x0 || y1 <= y0 {
            let _ = buffer.present();
            return;
        }

        let rect = softbuffer::Rect {
            x: x0 as u32,
            y: y0 as u32,
            width: NonZeroU32::new((x1 - x0) as u32).unwrap(),
            height: NonZeroU32::new((y1 - y0) as u32).unwrap(),
        };
        let _ = buffer.present_with_damage(&[rect]);
    }

    /// Frame-pacing flush, driven by the event loop's `about_to_wait`.
    ///
    /// If a redraw was deferred by the `MIN_FRAME` cap, present it once enough
    /// time has elapsed and return `None`; otherwise return the instant the loop
    /// should wake up to flush it (so it can `set_control_flow(WaitUntil(..))`).
    pub fn pump_frame(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;

        // Caret blink while a text box is active.
        if self.text_input.is_some() {
            if now.duration_since(self.last_blink) >= BLINK {
                self.caret_on = !self.caret_on;
                self.last_blink = now;
                self.request_redraw();
            }
            next = Some(min_instant(next, self.last_blink + BLINK));
        }

        // Frame pacing: flush a deferred frame when due, else schedule a wakeup.
        if self.redraw_pending {
            match self.last_present {
                Some(t) if now < t + self.min_frame => {
                    next = Some(min_instant(next, t + self.min_frame));
                }
                _ => {
                    self.redraw_pending = false;
                    self.render();
                }
            }
        }

        next
    }
}

// ─────────────── small raster helpers ───────────────

/// (line, column) of char index `idx` within `s`, splitting on '\n'.
fn line_col(s: &str, idx: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for (i, ch) in s.chars().enumerate() {
        if i == idx {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// The earlier of an optional instant and a concrete one.
fn min_instant(a: Option<Instant>, b: Instant) -> Instant {
    match a {
        Some(a) if a < b => a,
        _ => b,
    }
}

/// Integer pixel rect of `r`, clamped to `pw`×`ph`, as `(x, y, w, h)`.
fn int_rect(r: Rect, pw: u32, ph: u32) -> (i32, i32, i32, i32) {
    let x0 = (r.x.round() as i32).max(0);
    let y0 = (r.y.round() as i32).max(0);
    let x1 = (r.right().round() as i32).min(pw as i32);
    let y1 = (r.bottom().round() as i32).min(ph as i32);
    (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// Intersection of two `(x, y, w, h)` rects; zero-size if disjoint.
fn intersect_dmg(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    if x1 <= x0 || y1 <= y0 {
        (0, 0, 0, 0)
    } else {
        (x0, y0, x1 - x0, y1 - y0)
    }
}

/// Copy a rectangular region from a tightly-packed RGBA slice into a same-sized
/// pixmap. Used to restore the bright original snapshot inside the selection.
fn copy_rgba_region(src: &[u8], dst: &mut Pixmap, r: (i32, i32, i32, i32)) {
    let w = dst.width() as i32;
    let h = dst.height() as i32;
    let (rx, ry, rw, rh) = r;
    let x0 = rx.max(0);
    let y0 = ry.max(0);
    let x1 = (rx + rw).min(w);
    let y1 = (ry + rh).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let stride = (w * 4) as usize;
    let row_bytes = ((x1 - x0) * 4) as usize;
    let ddata = dst.data_mut();
    for yy in y0..y1 {
        let off = yy as usize * stride + (x0 * 4) as usize;
        if off + row_bytes <= src.len() && off + row_bytes <= ddata.len() {
            ddata[off..off + row_bytes].copy_from_slice(&src[off..off + row_bytes]);
        }
    }
}

/// Copy a rectangular region (physical px) from one same-sized pixmap to another.
fn copy_region(src: &Pixmap, dst: &mut Pixmap, r: (i32, i32, i32, i32)) {
    let w = src.width() as i32;
    let h = src.height() as i32;
    let (rx, ry, rw, rh) = r;
    let x0 = rx.max(0);
    let y0 = ry.max(0);
    let x1 = (rx + rw).min(w);
    let y1 = (ry + rh).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let stride = (w * 4) as usize;
    let row_bytes = ((x1 - x0) * 4) as usize;
    let sdata = src.data();
    let ddata = dst.data_mut();
    for yy in y0..y1 {
        let off = yy as usize * stride + (x0 * 4) as usize;
        ddata[off..off + row_bytes].copy_from_slice(&sdata[off..off + row_bytes]);
    }
}

/// Union of an optional previous rect with a new rect, each `(x, y, w, h)`.
/// Damage bbox for a selection rect, including the dimension label above it.
fn sel_damage_rect(sel: Rect, k: f32, pw: u32, ph: u32) -> (i32, i32, i32, i32) {
    let margin = (6.0 * k).ceil() as i32;
    let label_h = (22.0 * k).ceil() as i32;
    let mut x0 = (sel.x.floor() as i32) - margin;
    let mut y0 = (sel.y.floor() as i32) - label_h - margin;
    let mut x1 = (sel.right().ceil() as i32) + margin;
    let mut y1 = (sel.bottom().ceil() as i32) + margin;
    x0 = x0.max(0);
    y0 = y0.max(0);
    x1 = x1.min(pw as i32);
    y1 = y1.min(ph as i32);
    (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
}

fn union_rect_two(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    (x0, y0, x1 - x0, y1 - y0)
}

/// Selection border + dimension label (no resize handles).
fn draw_selection_chrome(
    scene: &mut Pixmap,
    sel: Rect,
    have_sel: bool,
    k: f32,
    bounds: Rect,
    text: Option<&TextRenderer>,
) {
    if sel.w <= 0.0 && sel.h <= 0.0 {
        return;
    }
    let accent = stroke_paint([0, 122, 255, 255]);
    stroke_rect(scene, sel, 2.0 * k, &accent);

    if have_sel {
        let pts = [
            (sel.x, sel.y),
            (sel.center_x(), sel.y),
            (sel.right(), sel.y),
            (sel.right(), sel.center_y()),
            (sel.right(), sel.bottom()),
            (sel.center_x(), sel.bottom()),
            (sel.x, sel.bottom()),
            (sel.x, sel.center_y()),
        ];
        let white = {
            let mut p = Paint::default();
            p.set_color_rgba8(255, 255, 255, 255);
            p.anti_alias = true;
            p
        };
        let hs = 4.0 * k;
        let hd = 8.0 * k;
        for (hx, hy) in pts {
            fill_rect(scene, hx - hs, hy - hs, hd, hd, &white);
            stroke_rect(scene, Rect::new(hx - hs, hy - hs, hd, hd), 1.5 * k, &accent);
        }
    }

    if let Some(tr) = text {
        let label = format!("{} × {}", sel.w.round() as i32, sel.h.round() as i32);
        let fpx = 14.0 * k;
        let tw = tr.measure(&label, fpx);
        let pad = 6.0 * k;
        let bw = tw + pad * 2.0;
        let bh = fpx + pad * 1.5;
        let mut lx = sel.x;
        let mut ly = sel.y - bh - 6.0 * k;
        if ly < 2.0 * k {
            ly = sel.y + 4.0 * k;
        }
        lx = lx.min(bounds.right() - bw - 2.0 * k).max(2.0 * k);
        let bg = {
            let mut p = Paint::default();
            p.set_color_rgba8(0, 0, 0, 200);
            p
        };
        if let Some(p) = toolbar::rounded_rect(lx, ly, bw, bh, 4.0 * k) {
            scene.fill_path(&p, &bg, FillRule::Winding, Transform::identity(), None);
        }
        tr.draw(scene, &label, lx + pad, ly + pad * 0.5, fpx, [255, 255, 255, 255]);
    }
}

fn union_rect(a: Option<(i32, i32, i32, i32)>, b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    match a {
        None => b,
        Some(a) => {
            let x0 = a.0.min(b.0);
            let y0 = a.1.min(b.1);
            let x1 = (a.0 + a.2).max(b.0 + b.2);
            let y1 = (a.1 + a.3).max(b.1 + b.3);
            (x0, y0, x1 - x0, y1 - y0)
        }
    }
}

fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(r, paint, Transform::identity(), None);
    }
}

/// Fill a (possibly rotated) quad given its 4 corner points (CW or CCW).
fn fill_quad(pm: &mut Pixmap, pts: [(f32, f32); 4], rgba: [u8; 4]) {
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    pb.line_to(pts[1].0, pts[1].1);
    pb.line_to(pts[2].0, pts[2].1);
    pb.line_to(pts[3].0, pts[3].1);
    pb.close();
    if let Some(path) = pb.finish() {
        let mut p = Paint::default();
        p.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
        p.anti_alias = true;
        pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    }
}

/// White square handle with an accent border, centered at `c` (half-size `half`).
fn draw_handle_square(pm: &mut Pixmap, c: (f32, f32), half: f32, k: f32, accent: [u8; 4]) {
    let mut wp = Paint::default();
    wp.set_color_rgba8(255, 255, 255, 255);
    wp.anti_alias = true;
    fill_rect(pm, c.0 - half, c.1 - half, half * 2.0, half * 2.0, &wp);
    stroke_rect(pm, Rect::new(c.0 - half, c.1 - half, half * 2.0, half * 2.0), 1.3 * k, &stroke_paint(accent));
}

/// White disc handle with an accent border (the rotation knob).
fn draw_handle_circle(pm: &mut Pixmap, c: (f32, f32), r: f32, k: f32, accent: [u8; 4]) {
    if let Some(p) = crate::annotations::oval_path(c.0, c.1, r, r) {
        let mut wp = Paint::default();
        wp.set_color_rgba8(255, 255, 255, 255);
        wp.anti_alias = true;
        pm.fill_path(&p, &wp, FillRule::Winding, Transform::identity(), None);
        pm.stroke_path(&p, &stroke_paint(accent), &Stroke { width: 1.3 * k, ..Default::default() }, Transform::identity(), None);
    }
}

/// Red disc with a white ✕ — the gizmo's close button.
fn draw_close_button(pm: &mut Pixmap, c: (f32, f32), r: f32, k: f32) {
    if let Some(p) = crate::annotations::oval_path(c.0, c.1, r, r) {
        let mut rp = Paint::default();
        rp.set_color_rgba8(255, 84, 74, 255);
        rp.anti_alias = true;
        pm.fill_path(&p, &rp, FillRule::Winding, Transform::identity(), None);
        pm.stroke_path(&p, &stroke_paint([255, 255, 255, 235]), &Stroke { width: 1.2 * k, ..Default::default() }, Transform::identity(), None);
    }
    let d = r * 0.5;
    let wx = stroke_paint([255, 255, 255, 255]);
    line(pm, (c.0 - d, c.1 - d), (c.0 + d, c.1 + d), 1.6 * k, &wx);
    line(pm, (c.0 + d, c.1 - d), (c.0 - d, c.1 + d), 1.6 * k, &wx);
}

fn handle_cursor(h: Handle) -> CursorIcon {
    match h {
        Handle::TL | Handle::BR => CursorIcon::NwseResize,
        Handle::TR | Handle::BL => CursorIcon::NeswResize,
        Handle::T | Handle::B => CursorIcon::NsResize,
        Handle::L | Handle::R => CursorIcon::EwResize,
        _ => CursorIcon::Default,
    }
}

fn stroke_paint(c: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.anti_alias = true;
    p
}

fn stroke_rect(pm: &mut Pixmap, r: Rect, w: f32, paint: &Paint) {
    let mut pb = PathBuilder::new();
    pb.move_to(r.x, r.y);
    pb.line_to(r.right(), r.y);
    pb.line_to(r.right(), r.bottom());
    pb.line_to(r.x, r.bottom());
    pb.close();
    if let Some(path) = pb.finish() {
        let st = Stroke {
            width: w,
            ..Default::default()
        };
        pm.stroke_path(&path, paint, &st, Transform::identity(), None);
    }
}

fn line(pm: &mut Pixmap, a: (f32, f32), b: (f32, f32), w: f32, paint: &Paint) {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    if let Some(path) = pb.finish() {
        let st = Stroke {
            width: w,
            ..Default::default()
        };
        pm.stroke_path(&path, paint, &st, Transform::identity(), None);
    }
}

// ─────────────── winit application ───────────────

impl Overlay {
    /// Create the fullscreen overlay window + surface for one capture session.
    /// Reused by both the one-shot [`ApplicationHandler`] and the resident
    /// server. On failure the session is marked done (window stays `None`).
    pub fn open(&mut self, event_loop: &ActiveEventLoop) {
        self.opened = true;
        if self.window.is_some() {
            return;
        }
        let size = PhysicalSize::new(self.snap.width, self.snap.height);
        let attrs = Window::default_attributes()
            .with_title("截图")
            .with_inner_size(size)
            .with_position(PhysicalPosition::new(self.snap.origin_x, self.snap.origin_y))
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(false)
            .with_window_level(WindowLevel::AlwaysOnTop);

        // Windows: create the window hidden. Its DWM open/close transition must be
        // switched off *before* it is first shown (see `crate::win`), otherwise the
        // scale/fade "open" animation that `create_window` would trigger on machines
        // with window animations enabled is already scheduled and can't be cancelled.
        // The window is shown at the end of `open()`, once transitions are disabled
        // and the first frame is ready.
        #[cfg(target_os = "windows")]
        let attrs = attrs.with_visible(false);

        // Wayland can't honor absolute position, clamps the size to the work area
        // (so the panel/status bar is left uncovered) and ignores AlwaysOnTop —
        // which lets clicks near the toolbar leak to the app underneath (mis-hit
        // buttons + a stray second capture). A borderless fullscreen surface is
        // the only reliable way to own the whole output and its input there. At
        // the common scale 1.0 the fullscreen inner_size equals the portal
        // snapshot, so the 1:1 snapshot/input model below still holds.
        #[cfg(target_os = "linux")]
        let attrs = if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
            attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)))
        } else {
            attrs
        };

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.outcome = Outcome::Error(format!("创建截图窗口失败: {e}"));
                return;
            }
        };
        // Windows: the window is still hidden here. Disable its DWM transitions AND
        // cloak it before the first show. Transitions-disabled alone does NOT stop
        // the first-show scale-in ("放大") of a freshly created window on Win10/11;
        // cloaking does — a cloaked window isn't composited, so we show + paint it
        // invisibly and uncloak (at the end of `open()`) to reveal it instantly at
        // full size, animation-free ("直出").
        #[cfg(target_os = "windows")]
        {
            crate::win::disable_window_transitions(&window);
            crate::win::set_window_cloaked(&window, true);
        }
        window.focus_window();

        // Prefer the macOS GPU presenter (uploads only the damaged region each
        // frame); fall back to softbuffer if it can't initialize.
        #[cfg(target_os = "macos")]
        if std::env::var_os("NATIVE_SCREENSHOT_NO_METAL").is_none() {
            self.gpu = crate::gpu::GpuPresenter::new(window.clone(), self.snap.width, self.snap.height);
        }
        // GPU present is damage-cheap → run at native refresh; softbuffer stays paced.
        #[cfg(target_os = "macos")]
        if self.gpu.is_some() {
            self.min_frame = MIN_FRAME_GPU;
        }
        let need_softbuffer = {
            #[cfg(target_os = "macos")]
            {
                self.gpu.is_none()
            }
            #[cfg(not(target_os = "macos"))]
            {
                true
            }
        };
        if need_softbuffer {
            let context = match softbuffer::Context::new(window.clone()) {
                Ok(c) => c,
                Err(e) => {
                    self.outcome =
                        Outcome::Error(format!("初始化绘制上下文失败(softbuffer context): {e}"));
                    return;
                }
            };
            match softbuffer::Surface::new(&context, window.clone()) {
                Ok(s) => self.surface = Some(s),
                Err(e) => {
                    self.outcome =
                        Outcome::Error(format!("创建绘制表面失败(softbuffer surface): {e}"));
                    return;
                }
            }
        }

        self.scale = window.scale_factor() as f32;
        if !(self.scale > 0.0) {
            self.scale = 1.0;
        }

        // Follow the system light/dark appearance for the toolbar theme. winit
        // reads the real value on macOS (effectiveAppearance) / Windows
        // (registry); fall back to dark when unknown.
        self.dark_mode = !matches!(window.theme(), Some(winit::window::Theme::Light));

        // Opt-in diagnostic (`NATIVE_SCREENSHOT_DEBUG=1`): confirms the HiDPI
        // mapping. If `inner_size` (window backing px) differs from `snap` (the
        // captured framebuffer px), the snapshot/chrome is not 1:1 and the scale
        // model needs adjusting; printed to stderr so it can't disturb the
        // stdout result protocol.
        if std::env::var_os("NATIVE_SCREENSHOT_DEBUG").is_some() {
            let isz = window.inner_size();
            eprintln!(
                "[native_screenshot] scale_factor={} inner_size={}x{} snapshot={}x{}",
                self.scale, isz.width, isz.height, self.snap.width, self.snap.height
            );
        }

        crate::macos::configure_overlay_window(&window);

        self.window = Some(window);

        // Pre-dim the frozen snapshot once (snapshot × 53%, i.e. black at the same
        // alpha the mask used to blend per-pixel). Repaints then compose the dark
        // area with a plain memcpy from this cache — no per-pixel blending — which
        // is what keeps dragging/resizing smooth on big HiDPI desktops.
        if self.dim_cache.is_none() {
            if let Some(mut p) = Pixmap::new(self.snap.width, self.snap.height) {
                // LUT: v -> v * (255-120)/255, matching the old mask alpha of 120.
                let mut lut = [0u8; 256];
                for (v, slot) in lut.iter_mut().enumerate() {
                    *slot = ((v as u32 * 135) / 255) as u8;
                }
                let src = &self.snap.rgba;
                let dst = p.data_mut();
                let n = dst.len() / 4;
                for px in 0..n {
                    let i = px * 4;
                    dst[i] = lut[src[i] as usize];
                    dst[i + 1] = lut[src[i + 1] as usize];
                    dst[i + 2] = lut[src[i + 2] as usize];
                    dst[i + 3] = 255;
                }
                self.dim_cache = Some(p);
            }
        }

        self.update_cursor();

        // Windows: the window was created hidden + cloaked (see above). Show and
        // activate it while still cloaked (invisible → the first-show scale-in
        // never becomes visible), paint the first frame into the surface, then
        // uncloak to reveal it instantly, fully drawn and at full size — no zoom,
        // no flash. `render()` here is the initial present, so frame pacing won't
        // drop it.
        #[cfg(target_os = "windows")]
        {
            if let Some(w) = &self.window {
                w.set_visible(true);
                w.focus_window();
            }
            self.render();
            if let Some(w) = &self.window {
                crate::win::set_window_cloaked(w, false);
            }
        }

        self.request_redraw();
    }

    /// Drive one window event. Session-ending actions route through
    /// [`Overlay::finish`] (which clears `window`), so the driver decides what to
    /// do next via [`Overlay::is_done`] — exit (one-shot) or close+wait (server).
    pub fn handle_window_event(&mut self, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.outcome = Outcome::Cancelled;
                self.finish();
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::ThemeChanged(theme) => {
                // System switched light/dark mid-session — re-theme the toolbar
                // and force a full rebuild (theme isn't part of the damage sig).
                let dark = !matches!(theme, winit::window::Theme::Light);
                if dark != self.dark_mode {
                    self.dark_mode = dark;
                    self.bg = None;
                    self.bg_sig = None;
                    self.last_struct_sig = None;
                    self.pending_damage = None;
                    self.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.on_mouse_move(position.x as f32, position.y as f32);
            }
            WindowEvent::MouseInput { state, button, .. } => match (state, button) {
                (ElementState::Pressed, MouseButton::Left) => {
                    let (x, y) = self.mouse;
                    self.on_left_down(x, y);
                    self.update_cursor();
                    self.request_redraw();
                }
                (ElementState::Released, MouseButton::Left) => {
                    let (x, y) = self.mouse;
                    self.on_left_up(x, y);
                    self.update_cursor();
                }
                (ElementState::Pressed, MouseButton::Right) => {
                    if self.text_input.is_some() {
                        self.cancel_text();
                    } else if self.tool.is_some() {
                        self.tool = None;
                    } else {
                        self.outcome = Outcome::Cancelled;
                        self.finish();
                    }
                    self.update_cursor();
                    self.request_redraw();
                }
                _ => {}
            },
            WindowEvent::Ime(ime) => {
                self.on_ime(ime);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The backing scale can change after creation (window settling on
                // a Retina screen, moving across monitors). UI chrome is sized in
                // physical px = logical × scale, so keep `scale` current and force
                // a full rebuild; otherwise the toolbar/handles render at the wrong
                // size on HiDPI.
                let s = scale_factor as f32;
                if s > 0.0 {
                    self.scale = s;
                }
                self.bg = None;
                self.bg_sig = None;
                self.last_struct_sig = None;
                self.pending_damage = None;
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.on_key(event);
            }
            _ => {}
        }
    }
}

impl ApplicationHandler for Overlay {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.open(event_loop);
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.handle_window_event(event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // One-shot: the session ends when the window is gone.
        if self.is_done() {
            event_loop.exit();
            return;
        }
        // Flush any frame deferred by the pacing cap, or sleep until it's due.
        match self.pump_frame() {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl Overlay {
    fn on_ime(&mut self, ime: Ime) {
        if self.text_input.is_none() {
            return;
        }
        if std::env::var_os("NATIVE_SCREENSHOT_DEBUG").is_some() {
            eprintln!("[native_screenshot] ime {ime:?}");
        }
        match ime {
            Ime::Preedit(text, _) => {
                if let Some(ti) = self.text_input.as_mut() {
                    // A non-empty preedit means the IME is genuinely composing;
                    // only then do we hand text duties to it (see `ime_seen`).
                    if !text.is_empty() {
                        ti.ime_seen = true;
                        // Starting composition replaces any selection.
                        ti.delete_selection();
                    }
                    ti.preedit = text;
                }
            }
            Ime::Commit(text) => {
                if let Some(ti) = self.text_input.as_mut() {
                    ti.ime_seen = true;
                    ti.insert(&text);
                    ti.preedit.clear();
                }
                self.caret_on = true;
                self.last_blink = Instant::now();
            }
            // `Enabled`/`Disabled` are lifecycle signals, not text. Don't let them
            // arm `ime_seen` (see its doc) — otherwise a non-functional IME kills
            // the raw-key fallback and no characters can be typed at all.
            Ime::Enabled | Ime::Disabled => {}
        }
        self.update_ime_cursor();
        // The IME re-queries the candidate-panel level each time it's about to
        // show the palette; refresh it (TSM bridge) as composition proceeds so
        // the 候选条 stays above the overlay (no-op off macOS).
        if let Some(w) = &self.window {
            crate::macos::raise_ime_candidate_level(w);
        }
    }

    fn on_key(&mut self, event: KeyEvent) {
        // Opt-in remote diagnosis (`NATIVE_SCREENSHOT_DEBUG=1`): shows whether
        // key events even reach us and whether they carry `text` — the two facts
        // needed to tell a focus problem apart from an IME/fallback problem when
        // the text tool "eats" typing on a given Linux/Wayland desktop.
        if std::env::var_os("NATIVE_SCREENSHOT_DEBUG").is_some() {
            eprintln!(
                "[native_screenshot] key state={:?} logical={:?} text={:?} editing={} ime_seen={:?}",
                event.state,
                event.logical_key,
                event.text,
                self.text_input.is_some(),
                self.text_input.as_ref().map(|t| t.ime_seen),
            );
        }
        if event.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.modifiers.control_key() || self.modifiers.super_key();
        let shift = self.modifiers.shift_key();

        // text editing intercepts most keys
        if self.text_input.is_some() {
            // Keep the caret solid while interacting.
            self.caret_on = true;
            self.last_blink = Instant::now();
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    // While an IME composition is in flight the system usually
                    // eats Esc itself (cancels the preedit); this is the fallback
                    // for when it reaches us. Otherwise Esc exits the whole
                    // screenshot session, matching the rest of the app.
                    let composing = self
                        .text_input
                        .as_ref()
                        .map(|ti| !ti.preedit.is_empty())
                        .unwrap_or(false);
                    if composing {
                        if let Some(ti) = self.text_input.as_mut() {
                            ti.preedit.clear();
                        }
                        self.update_ime_cursor();
                        self.request_redraw();
                    } else {
                        self.cancel_text();
                        self.outcome = Outcome::Cancelled;
                        self.finish();
                    }
                    return;
                }
                Key::Named(NamedKey::Enter) => {
                    if shift {
                        // Shift+Enter inserts a newline; plain Enter commits.
                        if let Some(ti) = self.text_input.as_mut() {
                            ti.insert("\n");
                        }
                        self.update_ime_cursor();
                        self.request_redraw();
                    } else {
                        self.commit_text();
                        self.request_redraw();
                    }
                }
                Key::Named(NamedKey::Backspace) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.backspace();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                Key::Named(NamedKey::Delete) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.delete();
                    }
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.pre_move(shift);
                        ti.move_left();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowRight) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.pre_move(shift);
                        ti.move_right();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowUp) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.pre_move(shift);
                        ti.move_up();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.pre_move(shift);
                        ti.move_down();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                Key::Named(NamedKey::Home) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.pre_move(shift);
                        ti.move_home();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                Key::Named(NamedKey::End) => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.pre_move(shift);
                        ti.move_end();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                // Select all.
                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("a") => {
                    if let Some(ti) = self.text_input.as_mut() {
                        ti.select_all();
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                // Copy selection.
                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => {
                    if let Some(ti) = self.text_input.as_ref() {
                        if let Some(sel) = ti.selected_text() {
                            let _ = crate::clipboard::copy_text(&sel);
                        }
                    }
                }
                // Cut selection.
                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("x") => {
                    if let Some(ti) = self.text_input.as_mut() {
                        if let Some(sel) = ti.selected_text() {
                            let _ = crate::clipboard::copy_text(&sel);
                            ti.delete_selection();
                        }
                    }
                    self.update_ime_cursor();
                    self.request_redraw();
                }
                // Paste at the caret (replacing any selection).
                Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") => {
                    if let Some(s) = crate::clipboard::paste_text() {
                        let s = s.replace('\r', "");
                        if let Some(ti) = self.text_input.as_mut() {
                            ti.insert(&s);
                        }
                        self.update_ime_cursor();
                        self.request_redraw();
                    }
                }
                _ => {
                    // Ignore other shortcuts while editing.
                    if ctrl {
                        return;
                    }
                    if let Some(txt) = &event.text {
                        if let Some(ti) = self.text_input.as_mut() {
                            // Once the IME is engaged, committed text arrives via
                            // `Ime::Commit`; skip the raw key text so we don't
                            // double-type. Only used as a fallback when no IME.
                            if !ti.ime_seen {
                                let ins: String =
                                    txt.chars().filter(|ch| !ch.is_control()).collect();
                                if !ins.is_empty() {
                                    ti.insert(&ins);
                                }
                                self.update_ime_cursor();
                                self.request_redraw();
                            }
                        }
                    }
                }
            }
            return;
        }

        // Selecting phase: magnifier color-readout controls.
        // Shift cycles the color format; C copies the value under the cursor.
        if !self.have_sel {
            match &event.logical_key {
                Key::Named(NamedKey::Shift) => {
                    if !event.repeat {
                        self.color_format = self.color_format.next();
                        self.request_redraw();
                    }
                    return;
                }
                Key::Character(c) if !ctrl && c.eq_ignore_ascii_case("c") => {
                    let (r, g, b, _) =
                        self.snap.pixel(self.mouse.0 as i32, self.mouse.1 as i32);
                    let s = self.color_format.format(r, g, b);
                    let _ = crate::clipboard::copy_text(&s);
                    return;
                }
                _ => {}
            }
        }

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.outcome = Outcome::Cancelled;
                self.finish();
            }
            Key::Named(NamedKey::Enter) => {
                if self.have_sel {
                    self.finalize(false, false);
                }
            }
            Key::Character(c) if ctrl => {
                let c = c.to_lowercase();
                if c == "z" {
                    if shift {
                        self.do_redo();
                    } else {
                        self.do_undo();
                    }
                    self.request_redraw();
                } else if c == "y" {
                    self.do_redo();
                    self.request_redraw();
                } else if c == "s" {
                    if self.have_sel {
                        self.finalize(true, false);
                    }
                } else if c == "c" {
                    // Ctrl/Cmd+C：仅复制到剪贴板并关闭。
                    if self.have_sel {
                        self.commit_text();
                        self.finalize(false, true);
                    }
                }
            }
            _ => {}
        }
    }
}
