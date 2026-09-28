//! macOS-specific overlay helpers.
//!
//! # Why the IME candidate panel was invisible (the real root cause)
//!
//! The overlay used to sit at `CGShieldingWindowLevel() + 1`. That call returns
//! the level **at or above which the window server *shields* the display and
//! refuses to composite anything** — it's meant for screen-capture/secure
//! windows. A CJK input method places its candidate panel (拼音候选条) *just
//! above the focused client's window level* (`IMKCandidates` does
//! `clientWindowLevel + 1`, see Apple TN2128 / IMKTextInput). So with the
//! overlay at `shield + 1`, the panel landed at `shield + 2` — above the shield,
//! where macOS never draws it. No amount of "tell the IME to go higher" can win
//! that, because nothing is drawn above the shield.
//!
//! The fix has three parts, all here:
//!   1. Park the overlay just **below** the shield (`shield - MARGIN`) — still
//!      above the Dock, menu bar and every ordinary window, but low enough that
//!      the candidate panel can composite above us.
//!   2. While a text box is active, drop further to `NSStatusWindowLevel` (25),
//!      so the panel shows whether the IME places it relative to our level
//!      (→ 26) or at a fixed level such as screen-saver (1000) — both beat 25.
//!   3. Report our level to the IME through *both* bridges it might consult: the
//!      Cocoa `NSTextInputClient.windowLevel` (grafted onto winit's view) and the
//!      Carbon TSM `kTSMDocumentWindowLevelPropertyTag` (refreshed on every
//!      preedit, per TN2128).

/// AppKit `NSStatusWindowLevel`: above the Dock (20) and menu bar (24), but a
/// low, ordinary level the candidate panel always beats. Used while editing text.
#[cfg(target_os = "macos")]
const EDIT_WINDOW_LEVEL: isize = 25;

/// How far below the shield level the overlay parks normally. Big enough to
/// clear any conceivable candidate-panel increment, small enough to stay above
/// all ordinary UI.
#[cfg(target_os = "macos")]
const SHIELD_MARGIN: isize = 100;

/// Fetch the `NSView*` backing a winit window, or null on failure.
///
/// # Safety
/// Sends Objective-C messages; the returned pointer is only valid while the
/// winit window is alive and must be used on the main thread.
#[cfg(target_os = "macos")]
unsafe fn ns_view_ptr(window: &winit::window::Window) -> *mut objc2::runtime::AnyObject {
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        return core::ptr::null_mut();
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return core::ptr::null_mut();
    };
    appkit.ns_view.as_ptr() as *mut AnyObject
}

/// Fetch the `NSWindow*` backing a winit window, or null on failure.
///
/// # Safety
/// Sends Objective-C messages; the returned pointer is only valid while the
/// winit window is alive and must be used on the main thread.
#[cfg(target_os = "macos")]
unsafe fn ns_window_ptr(window: &winit::window::Window) -> *mut objc2::runtime::AnyObject {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let view = ns_view_ptr(window);
    if view.is_null() {
        return core::ptr::null_mut();
    }
    let ns_window: *mut AnyObject = msg_send![view, window];
    ns_window
}

/// The overlay's normal level: just **below** the screen-capture shield level so
/// the IME candidate panel can composite above the overlay (see module docs).
#[cfg(target_os = "macos")]
fn overlay_window_level() -> isize {
    use objc2_core_graphics::CGShieldingWindowLevel;
    CGShieldingWindowLevel() as isize - SHIELD_MARGIN
}

#[cfg(target_os = "macos")]
fn debug_on() -> bool {
    std::env::var_os("NATIVE_SCREENSHOT_DEBUG").is_some()
}

/// Carbon Text Services Manager bridge.
///
/// The built-in CJK input sources read the level above which to float their
/// candidate palette from the active `TSMDocument`'s
/// `kTSMDocumentWindowLevelPropertyTag` (Apple TN2128 — this is exactly how
/// Spotlight keeps the candidate panel above itself). winit never sets it, so we
/// do. Best-effort: a null active document is logged and ignored.
#[cfg(target_os = "macos")]
mod tsm {
    use core::ffi::c_void;

    // 'wlvl' — kTSMDocumentWindowLevelPropertyTag (value is a window level i32).
    const K_TSM_DOC_WINDOW_LEVEL: u32 = 0x776C_766C;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn TSMGetActiveDocument() -> *mut c_void;
        fn TSMSetDocumentProperty(
            document: *mut c_void,
            tag: u32,
            size: u64,
            value: *const c_void,
        ) -> i32;
    }

    /// Tell the active input document to float its candidate palette above
    /// `level`. The IME queries this when it's about to show the palette.
    pub fn set_candidate_level(level: i32) {
        unsafe {
            let doc = TSMGetActiveDocument();
            if doc.is_null() {
                if super::debug_on() {
                    eprintln!(
                        "[native_screenshot] TSM active document is null — \
                         candidate level NOT set via TSM (Cocoa windowLevel bridge only)"
                    );
                }
                return;
            }
            let lvl = level;
            let _ = TSMSetDocumentProperty(
                doc,
                K_TSM_DOC_WINDOW_LEVEL,
                core::mem::size_of::<i32>() as u64,
                &lvl as *const i32 as *const c_void,
            );
            if super::debug_on() {
                eprintln!("[native_screenshot] TSM candidate window level set to {level}");
            }
        }
    }
}

/// `windowLevel` implementation grafted onto winit's text view so the Cocoa IME
/// bridge can read our level (it returns the overlay window's current level).
#[cfg(target_os = "macos")]
extern "C-unwind" fn ime_window_level(
    this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
) -> isize {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    unsafe {
        if this.is_null() {
            return EDIT_WINDOW_LEVEL;
        }
        let window: *mut AnyObject = msg_send![this, window];
        if window.is_null() {
            return EDIT_WINDOW_LEVEL;
        }
        let level: isize = msg_send![window, level];
        level
    }
}

/// Add the `windowLevel` method to winit's text-input view class (once), so the
/// Cocoa IME bridge orders the candidate panel above the overlay.
#[cfg(target_os = "macos")]
fn install_ime_window_level(window: &winit::window::Window) {
    use objc2::runtime::{AnyClass, Imp};
    use objc2::sel;
    use std::sync::Once;

    static ADD: Once = Once::new();
    ADD.call_once(|| unsafe {
        let view = ns_view_ptr(window);
        if view.is_null() {
            return;
        }
        let cls: &AnyClass = (*view).class();
        let cls_ptr = (cls as *const AnyClass) as *mut AnyClass;
        // `- (NSInteger)windowLevel;` → type encoding "q@:" on 64-bit (q = long).
        let imp: Imp = core::mem::transmute::<
            extern "C-unwind" fn(*mut objc2::runtime::AnyObject, objc2::runtime::Sel) -> isize,
            Imp,
        >(ime_window_level);
        let _ = objc2::ffi::class_addMethod(cls_ptr, sel!(windowLevel), imp, c"q@:".as_ptr());
    });
}

/// Read the overlay window's current AppKit level (best-effort).
#[cfg(target_os = "macos")]
fn current_level(window: &winit::window::Window) -> Option<isize> {
    use objc2::msg_send;
    let ns_window = unsafe { ns_window_ptr(window) };
    if ns_window.is_null() {
        return None;
    }
    let level: isize = unsafe { msg_send![ns_window, level] };
    Some(level)
}

/// Force the input method to **re-read** our `NSTextInputClient.windowLevel`.
///
/// macOS only reads `windowLevel` when the text input session gains focus, and
/// then *caches* it (Apple text-input internals; confirmed by Firefox's fix for
/// the very same "candidate panel hidden behind a top-most window" bug, Mozilla
/// bug 875674). Our overlay's view became the input client *before* we set the
/// right level, so the IME cached a stale/low level and parked the candidate
/// panel beneath us. Emulating a focus change — `deactivate` then `activate` on
/// the view's `NSTextInputContext` — makes it re-read the (correct) level.
#[cfg(target_os = "macos")]
fn refresh_ime_input_context(window: &winit::window::Window) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let view = unsafe { ns_view_ptr(window) };
    if view.is_null() {
        return;
    }
    unsafe {
        let ctx: *mut AnyObject = msg_send![view, inputContext];
        if ctx.is_null() {
            if debug_on() {
                eprintln!("[native_screenshot] view inputContext is null — cannot refresh IME level");
            }
            return;
        }
        let _: () = msg_send![ctx, deactivate];
        let _: () = msg_send![ctx, activate];
        if debug_on() {
            eprintln!("[native_screenshot] IME input context reactivated (re-read windowLevel)");
        }
    }
}

/// Raise the overlay above Dock / menu bar (but below the shield) so the dim
/// mask covers them, and teach winit's view to report its window level to the IME.
#[cfg(target_os = "macos")]
pub fn configure_overlay_window(window: &winit::window::Window) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

    // Make sure the candidate panel will float above the overlay while typing.
    install_ime_window_level(window);

    let ns_window = unsafe { ns_window_ptr(window) };
    if ns_window.is_null() {
        return;
    }

    let level = overlay_window_level();
    unsafe {
        let nsw = ns_window as *mut NSWindow;
        let _: () = msg_send![nsw, setLevel: level];
        let behavior = NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary;
        let _: () = msg_send![nsw, setCollectionBehavior: behavior];
        let _: () = msg_send![nsw, setHidesOnDeactivate: false];
        let _: () = msg_send![nsw, setIgnoresMouseEvents: false];
        let _: () = msg_send![nsw, setAcceptsMouseMovedEvents: true];

        // Activate the app and make the overlay the key window. The helper runs
        // with an Accessory activation policy (no Dock icon); without an explicit
        // activate the input context never engages and CJK IME (Pinyin, etc.)
        // delivers nothing — only plain ASCII keystrokes arrive. Activating +
        // makeKey wires up the text input client so committed CJK text flows in.
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
        let nil: *mut AnyObject = core::ptr::null_mut();
        let _: () = msg_send![nsw, makeKeyAndOrderFront: nil];
    }
    if debug_on() {
        eprintln!(
            "[native_screenshot] overlay level set to {level} (shield-{SHIELD_MARGIN}); \
             actual now = {:?}",
            current_level(window)
        );
    }
}

/// Toggle "text input mode" for the overlay window.
///
/// On entry we drop the overlay to `NSStatusWindowLevel` (25) so the candidate
/// panel is guaranteed above us, and prime the TSM bridge with that level. On
/// exit we restore the normal (below-shield) level. The window stays key in both
/// cases so keyboard / IME input keeps flowing.
#[cfg(target_os = "macos")]
pub fn set_text_input_active(window: &winit::window::Window, active: bool) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    let ns_window = unsafe { ns_window_ptr(window) };
    if ns_window.is_null() {
        return;
    }

    // THE actual root cause: the helper runs as an **Accessory** (background,
    // no-Dock) app. macOS suppresses the system IME candidate panel for
    // accessory apps — composition + commit still work (that's why typing
    // committed but the 候选条 never appeared), but the candidate UI is hidden.
    // Regular foreground winit apps (e.g. Alacritty) show it fine. So we promote
    // the app to a **Regular** foreground app while a text box is active, and
    // demote back to Accessory afterwards so no stray Dock icon lingers.
    //   NSApplicationActivationPolicyRegular = 0, Accessory = 1.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let policy: isize = if active { 0 } else { 1 };
            let _: bool = msg_send![app, setActivationPolicy: policy];
            if active {
                let _: () = msg_send![app, activateIgnoringOtherApps: true];
            }
        }
    }

    let level: isize = if active {
        EDIT_WINDOW_LEVEL
    } else {
        overlay_window_level()
    };
    unsafe {
        let _: () = msg_send![ns_window, setLevel: level];
        // Keep the overlay key so it keeps receiving keystrokes / IME events.
        let nil: *mut AnyObject = core::ptr::null_mut();
        let _: () = msg_send![ns_window, makeKeyAndOrderFront: nil];
    }
    if active {
        // Prime both IME bridges with the level we just dropped to.
        tsm::set_candidate_level(level as i32);
        // Crucial: force the IME to re-read our windowLevel now that the window
        // is at the editing level — otherwise it keeps the stale level it cached
        // when the input client first focused, and hides the candidate panel.
        refresh_ime_input_context(window);
    }
    if debug_on() {
        eprintln!(
            "[native_screenshot] text-input {} → policy={}, requested level {level}, actual now = {:?}",
            if active { "ON(Regular)" } else { "OFF(Accessory)" },
            if active { "Regular" } else { "Accessory" },
            current_level(window)
        );
    }
}

/// Keep the candidate palette above the overlay as composition proceeds. The
/// input method re-queries the level each time it's about to show the palette,
/// so we refresh it on every preedit, using the overlay's *actual* current
/// level (robust even if the level drop above didn't take effect).
#[cfg(target_os = "macos")]
pub fn raise_ime_candidate_level(window: &winit::window::Window) {
    let level = current_level(window).unwrap_or(EDIT_WINDOW_LEVEL);
    tsm::set_candidate_level(level as i32);
}

/// 进程启动最早期就把自己声明为 Accessory（后台、不进 Dock）。
///
/// 关键：helper 是以「裸可执行文件」嵌在宿主 bundle（Contents/MacOS/rust_screenshot）
/// 里运行的，启动时 AppKit 读到的是**宿主的 Info.plist（常规前台 App）**。若只依赖
/// winit 在事件循环初始化时才设 Accessory，从进程启动到那一刻之间会有空窗期，Dock
/// 会闪出一个宿主图标（社区已知问题：只在运行时设 .accessory 不够，需尽早设置）。
/// 在 `main()` 第一行调用本函数即可关掉这个空窗期。
#[cfg(target_os = "macos")]
pub fn set_accessory_activation_policy() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            // NSApplicationActivationPolicyAccessory = 1（等价 Info.plist 的 LSUIElement=1）。
            let _: bool = msg_send![app, setActivationPolicy: 1isize];
        }
    }
}

/// Promote the (Accessory) helper to a regular foreground app and activate it.
///
/// Used right before showing the native "Save As" panel: the overlay window is
/// already gone by then, so without this the `NSSavePanel` from a background
/// (Accessory) app can open behind other windows or without keyboard focus.
#[cfg(target_os = "macos")]
pub fn bring_app_to_front() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            // NSApplicationActivationPolicyRegular = 0.
            let _: bool = msg_send![app, setActivationPolicy: 0isize];
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
    }
}

// ─────────────── native text editor (NSTextView) ───────────────
//
// The hand-rolled tiny-skia text editor never gets the system IME candidate
// panel (拼音候选条) to show inside our borderless, full-screen, top-most
// overlay — composition/commit work, but the candidate list stays hidden. Every
// tool that *does* get it (Snipaste/Qt, Satty/GTK) uses a real native text
// widget, because the input method only fully engages its candidate UI for a
// genuine `NSTextInputClient`. So on macOS the text tool is backed by a real
// `NSTextView` subview: it owns editing + IME while active, and we read its
// string back on commit to rasterize the final annotation with tiny-skia.

#[cfg(target_os = "macos")]
thread_local! {
    static NATIVE_TEXT: std::cell::RefCell<Option<objc2::rc::Retained<objc2::runtime::AnyObject>>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether a native text editor is currently on screen.
#[cfg(target_os = "macos")]
pub fn native_text_active() -> bool {
    NATIVE_TEXT.with(|c| c.borrow().is_some())
}

/// Spawn a native `NSTextView` at `(x, y)` (overlay physical px) for the text
/// tool, styled to `font_px`/`color`, seeded with `initial`, and make it the
/// first responder so the IME (and its candidate panel) engages on it.
#[cfg(target_os = "macos")]
pub fn begin_native_text(
    window: &winit::window::Window,
    x: f32,
    y: f32,
    font_px: f32,
    scale: f32,
    color: [u8; 4],
    initial: &str,
) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    // Never stack two editors.
    end_native_text(window);

    let view = unsafe { ns_view_ptr(window) };
    let win = unsafe { ns_window_ptr(window) };
    if view.is_null() || win.is_null() {
        return;
    }
    let s = if scale > 0.0 { scale as f64 } else { 1.0 };
    let pts = ((font_px / scale).max(8.0)) as f64; // points for the native view

    unsafe {
        // winit's content view is flipped (top-left origin), so the subview frame
        // origin is top-left too. Keep the box modest so its transparent area
        // doesn't swallow clicks meant for the toolbar.
        let frame = CGRect {
            origin: CGPoint {
                x: (x as f64) / s,
                y: (y as f64) / s,
            },
            size: CGSize {
                width: 460.0,
                height: (pts * 1.6 * 3.0).max(40.0),
            },
        };
        let tv: *mut AnyObject = msg_send![class!(NSTextView), alloc];
        let tv: *mut AnyObject = msg_send![tv, initWithFrame: frame];
        if tv.is_null() {
            return;
        }
        let _: () = msg_send![tv, setEditable: true];
        let _: () = msg_send![tv, setSelectable: true];
        let _: () = msg_send![tv, setRichText: false];
        let _: () = msg_send![tv, setDrawsBackground: false];
        let _: () = msg_send![tv, setUsesFontPanel: false];
        let _: () = msg_send![tv, setVerticallyResizable: false];
        let _: () = msg_send![tv, setHorizontallyResizable: false];

        // Zero the text container insets so the native glyphs start at the exact
        // same top-left as our tiny-skia render. Otherwise NSTextView's default
        // `lineFragmentPadding` (5pt) + `textContainerInset` shift the text down
        // and to the right, so re-editing a placed label made it "jump".
        let _: () = msg_send![tv, setTextContainerInset: CGSize { width: 0.0, height: 0.0 }];
        let tc: *mut AnyObject = msg_send![tv, textContainer];
        if !tc.is_null() {
            let _: () = msg_send![tc, setLineFragmentPadding: 0.0f64];
        }

        // Match the final tiny-skia render's font (PingFang) so the baseline /
        // line height line up — using the system font (SF Pro) instead shifts the
        // text vertically when entering/leaving the native editor. Fall back to
        // the system font if PingFang can't be resolved by name.
        let mut font: *mut AnyObject = core::ptr::null_mut();
        if let Ok(name) = std::ffi::CString::new("PingFangSC-Regular") {
            let pf_name: *mut AnyObject =
                msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
            if !pf_name.is_null() {
                font = msg_send![class!(NSFont), fontWithName: pf_name, size: pts];
            }
        }
        if font.is_null() {
            font = msg_send![class!(NSFont), systemFontOfSize: pts];
        }
        if !font.is_null() {
            let _: () = msg_send![tv, setFont: font];
        }
        let nscolor: *mut AnyObject = msg_send![
            class!(NSColor),
            colorWithSRGBRed: (color[0] as f64) / 255.0,
            green: (color[1] as f64) / 255.0,
            blue: (color[2] as f64) / 255.0,
            alpha: (color[3] as f64) / 255.0,
        ];
        if !nscolor.is_null() {
            let _: () = msg_send![tv, setTextColor: nscolor];
            let _: () = msg_send![tv, setInsertionPointColor: nscolor];
        }
        if !initial.is_empty() {
            if let Ok(cs) = std::ffi::CString::new(initial) {
                let ns: *mut AnyObject =
                    msg_send![class!(NSString), stringWithUTF8String: cs.as_ptr()];
                if !ns.is_null() {
                    let _: () = msg_send![tv, setString: ns];
                }
            }
        }

        let _: () = msg_send![view, addSubview: tv];
        let _: bool = msg_send![win, makeFirstResponder: tv];

        // Take ownership of the alloc/init +1 (the superview holds its own ref).
        if let Some(retained) = Retained::from_raw(tv) {
            NATIVE_TEXT.with(|c| *c.borrow_mut() = Some(retained));
        }
    }
    if debug_on() {
        eprintln!("[native_screenshot] native NSTextView created for text input");
    }
}

/// The current contents of the native text editor (`None` if not active).
#[cfg(target_os = "macos")]
pub fn native_text_string() -> Option<String> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    NATIVE_TEXT.with(|c| {
        let b = c.borrow();
        let tv = b.as_ref()?;
        unsafe {
            let obj: &AnyObject = tv;
            let ns: *mut AnyObject = msg_send![obj, string];
            if ns.is_null() {
                return Some(String::new());
            }
            let utf8: *const std::ffi::c_char = msg_send![ns, UTF8String];
            if utf8.is_null() {
                return Some(String::new());
            }
            Some(std::ffi::CStr::from_ptr(utf8).to_string_lossy().into_owned())
        }
    })
}

/// Tear down the native text editor and hand first-responder back to the overlay.
#[cfg(target_os = "macos")]
pub fn end_native_text(window: &winit::window::Window) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    let taken = NATIVE_TEXT.with(|c| c.borrow_mut().take());
    if let Some(tv) = taken {
        unsafe {
            let obj: &AnyObject = &tv;
            let _: () = msg_send![obj, removeFromSuperview];
            let view = ns_view_ptr(window);
            let win = ns_window_ptr(window);
            if !win.is_null() && !view.is_null() {
                let _: bool = msg_send![win, makeFirstResponder: view];
            }
        }
        // Dropping `tv` releases our retained reference.
    }
}

#[cfg(not(target_os = "macos"))]
pub fn configure_overlay_window(_window: &winit::window::Window) {}

#[cfg(not(target_os = "macos"))]
pub fn set_text_input_active(_window: &winit::window::Window, _active: bool) {}

#[cfg(not(target_os = "macos"))]
pub fn raise_ime_candidate_level(_window: &winit::window::Window) {}

#[cfg(not(target_os = "macos"))]
pub fn bring_app_to_front() {}

#[cfg(not(target_os = "macos"))]
pub fn set_accessory_activation_policy() {}

#[cfg(not(target_os = "macos"))]
pub fn native_text_active() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn begin_native_text(
    _window: &winit::window::Window,
    _x: f32,
    _y: f32,
    _font_px: f32,
    _scale: f32,
    _color: [u8; 4],
    _initial: &str,
) {
}

#[cfg(not(target_os = "macos"))]
pub fn native_text_string() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn end_native_text(_window: &winit::window::Window) {}
