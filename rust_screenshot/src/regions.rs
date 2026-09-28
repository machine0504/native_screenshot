//! Window/region auto-detection: the list of candidate rectangles the cursor
//! can snap onto (one-click select), computed once at capture time so it stays
//! consistent with the frozen snapshot.
//!
//! Returned rects are in virtual-desktop physical pixels, re-based to the
//! snapshot origin (i.e. the same coordinate system as the overlay), ordered
//! top-most first.

use crate::common::Rect;

pub fn detect_regions(
    origin_x: i32,
    origin_y: i32,
    snap_w: u32,
    snap_h: u32,
) -> Vec<Rect> {
    #[cfg(target_os = "windows")]
    {
        return detect_regions_windows(origin_x, origin_y);
    }
    #[cfg(target_os = "macos")]
    {
        return detect_regions_macos(origin_x, origin_y, snap_w, snap_h);
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        return detect_regions_xcap(origin_x, origin_y, snap_w, snap_h);
    }
}

#[cfg(target_os = "windows")]
fn detect_regions_windows(origin_x: i32, origin_y: i32) -> Vec<Rect> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, GWL_EXSTYLE,
        WS_EX_TOOLWINDOW,
    };

    struct Ctx {
        regions: Vec<Rect>,
        ox: i32,
        oy: i32,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);

        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return TRUE;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return TRUE;
        }
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return TRUE;
        }

        let mut r = RECT::default();
        let got = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok();
        if !got && GetWindowRect(hwnd, &mut r).is_err() {
            return TRUE;
        }

        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w < 8 || h < 8 {
            return TRUE;
        }
        ctx.regions.push(Rect::new(
            (r.left - ctx.ox) as f32,
            (r.top - ctx.oy) as f32,
            w as f32,
            h as f32,
        ));
        TRUE
    }

    let mut ctx = Ctx {
        regions: Vec::new(),
        ox: origin_x,
        oy: origin_y,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.regions
}

/// Thin CoreGraphics / CoreFoundation bindings for window enumeration.
#[cfg(target_os = "macos")]
mod cg {
    use std::ffi::{c_void, CStr};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CgPoint {
        pub x: f64,
        pub y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CgSize {
        pub width: f64,
        pub height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CgRect {
        pub origin: CgPoint,
        pub size: CgSize,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFArrayGetCount(array: *const c_void) -> isize;
        pub fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
        pub fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        pub fn CFStringGetCString(s: *const c_void, buf: *mut i8, size: isize, encoding: u32) -> u8;
        pub fn CFNumberGetValue(num: *const c_void, ty: isize, value: *mut c_void) -> u8;
        pub fn CFRelease(cf: *const c_void);
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const c_void;
        pub fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CgRect)
            -> u8;
        pub static kCGWindowBounds: *const c_void;
        pub static kCGWindowOwnerName: *const c_void;
        pub static kCGWindowLayer: *const c_void;
        pub static kCGWindowAlpha: *const c_void;
    }

    // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements.
    pub const LIST_OPTION: u32 = (1 << 0) | (1 << 4);
    pub const NULL_WINDOW_ID: u32 = 0;
    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
    const SINT32: isize = 3; // kCFNumberSInt32Type
    const FLOAT64: isize = 6; // kCFNumberFloat64Type

    /// Read a CFString dictionary value as a Rust `String`.
    ///
    /// # Safety
    /// `dict` / `key` must be valid CoreFoundation pointers.
    pub unsafe fn dict_string(dict: *const c_void, key: *const c_void) -> Option<String> {
        let p = CFDictionaryGetValue(dict, key);
        if p.is_null() {
            return None;
        }
        let mut buf = [0i8; 256];
        if CFStringGetCString(p, buf.as_mut_ptr(), buf.len() as isize, UTF8) == 0 {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }

    /// Read a CFNumber dictionary value as `i32`.
    ///
    /// # Safety
    /// `dict` / `key` must be valid CoreFoundation pointers.
    pub unsafe fn dict_i32(dict: *const c_void, key: *const c_void) -> Option<i32> {
        let p = CFDictionaryGetValue(dict, key);
        if p.is_null() {
            return None;
        }
        let mut v: i32 = 0;
        if CFNumberGetValue(p, SINT32, &mut v as *mut i32 as *mut c_void) == 0 {
            return None;
        }
        Some(v)
    }

    /// Read a CFNumber dictionary value as `f64`.
    ///
    /// # Safety
    /// `dict` / `key` must be valid CoreFoundation pointers.
    pub unsafe fn dict_f64(dict: *const c_void, key: *const c_void) -> Option<f64> {
        let p = CFDictionaryGetValue(dict, key);
        if p.is_null() {
            return None;
        }
        let mut v: f64 = 0.0;
        if CFNumberGetValue(p, FLOAT64, &mut v as *mut f64 as *mut c_void) == 0 {
            return None;
        }
        Some(v)
    }

    /// Read the `kCGWindowBounds` rect (global logical points, top-left origin).
    ///
    /// # Safety
    /// `dict` must be a valid window-info dictionary.
    pub unsafe fn dict_rect(dict: *const c_void, key: *const c_void) -> Option<CgRect> {
        let p = CFDictionaryGetValue(dict, key);
        if p.is_null() {
            return None;
        }
        let mut r = CgRect {
            origin: CgPoint { x: 0.0, y: 0.0 },
            size: CgSize { width: 0.0, height: 0.0 },
        };
        if CGRectMakeWithDictionaryRepresentation(p, &mut r) == 0 {
            return None;
        }
        Some(r)
    }
}

/// macOS window auto-detection via `CGWindowListCopyWindowInfo`.
///
/// This is how Snipaste/Flameshot-style tools do it on macOS: the on-screen
/// window list comes back **in true front-to-back z-order**, so taking the first
/// rect that contains the cursor (`Overlay::region_at`) yields exactly the
/// top-most window under the pointer — accurate one-click window capture.
///
/// We keep normal application windows (layer 0) in z-order, and additionally
/// surface the Dock (Dock-owned, positive layer) at the front of the list so
/// hovering the Dock snaps to it. Bounds are Quartz global logical points
/// (top-left origin); we convert to virtual-desktop physical pixels (matching
/// the frozen snapshot) via the scale factor of the monitor under each window.
#[cfg(target_os = "macos")]
fn detect_regions_macos(
    origin_x: i32,
    origin_y: i32,
    snap_w: u32,
    snap_h: u32,
) -> Vec<Rect> {
    use cg::*;
    use xcap::Monitor;

    // Owners whose windows are system chrome, never useful one-click targets.
    const SKIP_OWNERS: &[&str] = &[
        "Window Server",
        "Control Center",
        "Notification Center",
        "Spotlight",
        "SystemUIServer",
        "loginwindow",
        "Wallpaper",
        "rust_screenshot",
    ];

    let monitors = Monitor::all().unwrap_or_default();
    // Logical (global, top-left) → physical pixels, rebased to the snapshot
    // origin, using the scale factor of the monitor under the rect's center.
    let to_physical = |r: CgRect| -> (f32, f32, f32, f32) {
        let cx = r.origin.x + r.size.width / 2.0;
        let cy = r.origin.y + r.size.height / 2.0;
        let mut sf = monitors
            .iter()
            .find(|m| {
                let (mx, my) = (m.x() as f64, m.y() as f64);
                let (mw, mh) = (m.width() as f64, m.height() as f64);
                cx >= mx && cx < mx + mw && cy >= my && cy < my + mh
            })
            .map(|m| m.scale_factor() as f64)
            .unwrap_or(0.0);
        if sf <= 0.0 {
            sf = monitors
                .iter()
                .find(|m| m.is_primary())
                .or_else(|| monitors.first())
                .map(|m| m.scale_factor() as f64)
                .filter(|s| *s > 0.0)
                .unwrap_or(1.0);
        }
        (
            (r.origin.x * sf).round() as f32 - origin_x as f32,
            (r.origin.y * sf).round() as f32 - origin_y as f32,
            (r.size.width * sf).round() as f32,
            (r.size.height * sf).round() as f32,
        )
    };

    // Keep a rect only if it's a sane size and overlaps the frozen snapshot.
    let snap_w = snap_w as f32;
    let snap_h = snap_h as f32;
    let visible = |rx: f32, ry: f32, w: f32, h: f32| -> bool {
        w >= 16.0
            && h >= 16.0
            && rx + w > 0.0
            && ry + h > 0.0
            && rx < snap_w
            && ry < snap_h
    };

    let mut windows: Vec<Rect> = Vec::new(); // normal app windows, front-to-back
    let mut dock: Option<(Rect, f64)> = None; // (rect, area) — largest Dock bar

    unsafe {
        let list = CGWindowListCopyWindowInfo(LIST_OPTION, NULL_WINDOW_ID);
        if list.is_null() {
            return Vec::new();
        }
        let count = CFArrayGetCount(list);
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() {
                continue;
            }

            let owner = dict_string(dict, kCGWindowOwnerName).unwrap_or_default();
            let layer = dict_i32(dict, kCGWindowLayer).unwrap_or(i32::MIN);
            let alpha = dict_f64(dict, kCGWindowAlpha).unwrap_or(1.0);
            if alpha < 0.05 {
                continue; // fully transparent helper layers
            }
            let Some(r) = dict_rect(dict, kCGWindowBounds) else {
                continue;
            };
            if r.size.width < 1.0 || r.size.height < 1.0 {
                continue;
            }
            let (rx, ry, w, h) = to_physical(r);
            if !visible(rx, ry, w, h) {
                continue;
            }

            if owner == "Dock" && layer > 0 {
                // The Dock process hosts several windows (icon bar, wallpaper at
                // negative layer, …); the icon bar is the largest positive-layer
                // one. Ignore near-fullscreen matches (never a real Dock bar).
                let area = (w * h) as f64;
                let nearly_full = w >= snap_w * 0.95 && h >= snap_h * 0.95;
                if !nearly_full && dock.map_or(true, |(_, a)| area > a) {
                    dock = Some((Rect::new(rx, ry, w, h), area));
                }
                continue;
            }

            // Normal application windows live on layer 0. Skip system chrome.
            if layer != 0 {
                continue;
            }
            if SKIP_OWNERS.iter().any(|s| owner.eq_ignore_ascii_case(s)) {
                continue;
            }
            windows.push(Rect::new(rx, ry, w, h));
        }
        CFRelease(list);
    }

    // Dock first (so hovering it wins over whatever is beneath), then normal
    // windows in their natural front-to-back order.
    let mut out = Vec::with_capacity(windows.len() + 1);
    if let Some((d, _)) = dock {
        out.push(d);
    }
    out.extend(windows);
    out
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn detect_regions_xcap(
    origin_x: i32,
    origin_y: i32,
    snap_w: u32,
    snap_h: u32,
) -> Vec<Rect> {
    use xcap::Window;

    let mut out = Vec::new();
    let Ok(windows) = Window::all() else {
        return out;
    };
    for w in windows {
        if w.is_minimized() {
            continue;
        }
        let sf = w.current_monitor().scale_factor() as f64;
        let x = (w.x() as f64 * sf).round() as i32;
        let y = (w.y() as f64 * sf).round() as i32;
        let width = (w.width() as f64 * sf).round() as i32;
        let height = (w.height() as f64 * sf).round() as i32;
        if width < 8 || height < 8 {
            continue;
        }
        let rx = (x - origin_x) as f32;
        let ry = (y - origin_y) as f32;
        if rx + width as f32 <= 0.0
            || ry + height as f32 <= 0.0
            || rx >= snap_w as f32
            || ry >= snap_h as f32
        {
            continue;
        }
        out.push(Rect::new(rx, ry, width as f32, height as f32));
    }
    out
}
