//! Windows-specific overlay helpers.
//!
//! # Why some machines show a zoom-in on capture (and how we kill it)
//!
//! When a brand-new top-level window is first shown, the Desktop Window Manager
//! plays a scale + fade "open" animation — but only when the machine has window
//! animations enabled (`系统属性 → 高级 → 性能 → 视觉效果`). That is why only
//! *some* PCs see the overlay mask rapidly zoom in when it appears.
//!
//! `DWMWA_TRANSITIONS_FORCEDISABLED` alone is **not** enough: on Win10/11 it
//! suppresses minimize/maximize/restore transitions, but not the first-show
//! scale-in of a freshly created window. The reliable fix is **DWM cloaking**:
//!
//!   1. Create the window hidden (`with_visible(false)`).
//!   2. Disable transitions *and* cloak it (`DWMWA_CLOAK = TRUE`) — a cloaked
//!      window is not composited, so nothing it does is visible.
//!   3. Show + activate it and paint the first frame (all while cloaked, so the
//!      "open" animation, if any, plays on an invisible surface).
//!   4. Uncloak (`DWMWA_CLOAK = FALSE`) — the window appears instantly, already
//!      fully drawn and at final size: no zoom, no flash ("直出").
//!
//! Cloaking is likewise not animated on teardown, and transitions-disabled keeps
//! the close instant too.

use std::ffi::c_void;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{BOOL, FALSE, HWND, TRUE};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CLOAK, DWMWA_TRANSITIONS_FORCEDISABLED, DWMWINDOWATTRIBUTE,
};

/// Resolve the raw Win32 `HWND` backing a winit window, or `None` on failure.
fn hwnd_of(window: &winit::window::Window) -> Option<HWND> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return None;
    };
    Some(HWND(h.hwnd.get() as *mut c_void))
}

/// Set a `BOOL`-valued DWM window attribute (best-effort; errors are ignored so
/// the window merely keeps its default behavior).
fn set_dwm_bool(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: BOOL) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            attr,
            &value as *const _ as *const c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Turn off DWM open/close transition animations for this overlay window.
///
/// Must be called *before* the window is first shown. Kept alongside cloaking as
/// belt-and-suspenders (it also makes the close instant).
pub fn disable_window_transitions(window: &winit::window::Window) {
    if let Some(hwnd) = hwnd_of(window) {
        set_dwm_bool(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, TRUE);
    }
}

/// Cloak (`cloaked = true`) or uncloak (`false`) the window.
///
/// A cloaked window is not composited by DWM — it exists and can be shown /
/// activated / painted, but stays invisible. Cloak before the first show, paint
/// the first frame, then uncloak: the "open" scale-in never becomes visible, so
/// the overlay appears instantly at full size ("直出"). Best-effort.
pub fn set_window_cloaked(window: &winit::window::Window, cloaked: bool) {
    if let Some(hwnd) = hwnd_of(window) {
        set_dwm_bool(hwnd, DWMWA_CLOAK, if cloaked { TRUE } else { FALSE });
    }
}
