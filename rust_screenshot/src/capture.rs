//! Frozen capture of the whole virtual desktop in physical pixels.

use crate::common::{Result, ScreenshotError, Snapshot};

#[cfg(target_os = "windows")]
pub fn capture_virtual_desktop() -> Result<Snapshot> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, HBITMAP, HGDIOBJ, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    unsafe {
        let x0 = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y0 = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w < 1 || h < 1 {
            return Err(ScreenshotError::Capture("invalid virtual screen size".into()));
        }

        let screen = GetDC(HWND(std::ptr::null_mut()));
        if screen.is_invalid() {
            return Err(ScreenshotError::Capture("GetDC failed".into()));
        }
        let mem = CreateCompatibleDC(screen);
        let bmp: HBITMAP = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));

        let _ = BitBlt(mem, 0, 0, w, h, screen, x0, y0, SRCCOPY | CAPTUREBLT);

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let scanned = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(HWND(std::ptr::null_mut()), screen);

        if scanned == 0 {
            return Err(ScreenshotError::Capture("GetDIBits failed".into()));
        }

        // BGRA -> RGBA, force opaque alpha.
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }

        Ok(Snapshot {
            rgba: buf,
            width: w as u32,
            height: h as u32,
            origin_x: x0,
            origin_y: y0,
        })
    }
}

#[cfg(not(target_os = "windows"))]
pub fn capture_virtual_desktop() -> Result<Snapshot> {
    capture_non_windows()
}

/// macOS (and any non-Windows, non-Linux target): xcap's native backend is the
/// only path and works well (CoreGraphics on macOS).
#[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
fn capture_non_windows() -> Result<Snapshot> {
    capture_via_xcap()
}

/// Linux: xcap only implements the X11 backend. Under a Wayland session the app
/// talks to XWayland, whose rootless root window rejects XGetImage, so every
/// per-monitor capture fails ("no monitor images"). Prefer the XDG desktop
/// portal there; keep xcap as the X11 fast path and mutual fallback.
#[cfg(target_os = "linux")]
fn capture_non_windows() -> Result<Snapshot> {
    if linux_prefers_portal() {
        return match capture_via_portal() {
            Ok(s) => Ok(s),
            Err(portal_err) => {
                log::warn!("portal screenshot failed ({portal_err}); trying xcap X11");
                capture_via_xcap().map_err(|xcap_err| {
                    ScreenshotError::Capture(format!(
                        "portal failed ({portal_err}); xcap X11 also failed ({xcap_err})"
                    ))
                })
            }
        };
    }
    match capture_via_xcap() {
        Ok(s) => Ok(s),
        Err(xcap_err) => {
            log::warn!("xcap X11 capture failed ({xcap_err}); trying portal");
            capture_via_portal().map_err(|portal_err| {
                ScreenshotError::Capture(format!(
                    "xcap X11 failed ({xcap_err}); portal fallback also failed ({portal_err})"
                ))
            })
        }
    }
}

/// Whether this Linux session should prefer the XDG desktop portal over xcap's
/// X11 backend. True on Wayland, where the app runs on XWayland and X11
/// `XGetImage` on the rootless root window fails.
#[cfg(target_os = "linux")]
fn linux_prefers_portal() -> bool {
    if std::env::var("WAYLAND_DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    std::env::var("XDG_SESSION_TYPE")
        .map(|t| t.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
}

/// Capture every monitor via xcap and stitch them into one physical-pixel
/// snapshot. On Linux this is the X11 backend; on macOS it is CoreGraphics.
#[cfg(not(target_os = "windows"))]
fn capture_via_xcap() -> Result<Snapshot> {
    use xcap::Monitor;

    let monitors =
        Monitor::all().map_err(|e| ScreenshotError::Capture(format!("enumerate monitors: {e}")))?;
    if monitors.is_empty() {
        return Err(ScreenshotError::Capture("no monitors".into()));
    }

    // xcap reports monitor position/size in *logical* pixels, but returns the
    // captured image in *physical* pixels. Everything below works in physical
    // pixels (image size for dimensions, logical position * scale_factor for
    // placement) so the frozen snapshot — and therefore the overlay window —
    // maps 1:1 to the real framebuffer under HiDPI scaling.
    struct Captured {
        x: i32, // physical origin
        y: i32,
        w: u32, // physical size (from the real captured image)
        h: u32,
        rgba: Vec<u8>,
    }
    let mut shots: Vec<Captured> = Vec::new();
    // Keep the real per-monitor failure so an empty result stays diagnosable
    // instead of a bare "no monitor images" (which hid the Wayland root cause).
    let mut capture_errors: Vec<String> = Vec::new();
    for m in &monitors {
        let img = match m.capture_image() {
            Ok(i) => i,
            Err(e) => {
                capture_errors.push(e.to_string());
                continue;
            }
        };
        let sf = m.scale_factor() as f64;
        shots.push(Captured {
            x: (m.x() as f64 * sf).round() as i32,
            y: (m.y() as f64 * sf).round() as i32,
            w: img.width(),
            h: img.height(),
            rgba: img.into_raw(),
        });
    }
    if shots.is_empty() {
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into());
        return Err(ScreenshotError::Capture(format!(
            "no monitor images (session={session}, monitors={}, capture_errors=[{}])",
            monitors.len(),
            capture_errors.join("; ")
        )));
    }

    // Physical bounding box of all monitors.
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for s in &shots {
        min_x = min_x.min(s.x);
        min_y = min_y.min(s.y);
        max_x = max_x.max(s.x + s.w as i32);
        max_y = max_y.max(s.y + s.h as i32);
    }
    let width = (max_x - min_x).max(1) as u32;
    let height = (max_y - min_y).max(1) as u32;
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    for px in rgba.chunks_exact_mut(4) {
        px[3] = 255;
    }

    for s in &shots {
        let ox = (s.x - min_x) as i64;
        let oy = (s.y - min_y) as i64;
        let (iw, ih) = (s.w as i64, s.h as i64);
        let raw = &s.rgba;
        for row in 0..ih {
            let dy = oy + row;
            if dy < 0 || dy >= height as i64 {
                continue;
            }
            for col in 0..iw {
                let dx = ox + col;
                if dx < 0 || dx >= width as i64 {
                    continue;
                }
                let src = ((row * iw + col) * 4) as usize;
                let dst = ((dy * width as i64 + dx) * 4) as usize;
                rgba[dst..dst + 4].copy_from_slice(&raw[src..src + 4]);
            }
        }
    }

    Ok(Snapshot {
        rgba,
        width,
        height,
        origin_x: min_x,
        origin_y: min_y,
    })
}

/// Capture the whole screen through the XDG desktop portal
/// (`org.freedesktop.portal.Screenshot`). The compositor produces the image, so
/// this works where X11 `XGetImage` cannot (Deepin/UOS, GNOME, KDE Wayland, …).
/// The portal returns a single full-desktop image, treated here as the virtual
/// desktop with origin (0,0) — correct for the common single-monitor case.
#[cfg(target_os = "linux")]
fn capture_via_portal() -> Result<Snapshot> {
    use ashpd::desktop::screenshot::Screenshot;

    // ashpd is async; block on it exactly like rfd's save dialog does. zbus runs
    // its own reactor, so pollster driving the top future is sufficient (no
    // tokio/async-std runtime required).
    let shot = pollster::block_on(async {
        Screenshot::request()
            .interactive(false)
            .modal(false)
            .send()
            .await
            .map_err(|e| ScreenshotError::Capture(format!("portal request failed: {e}")))?
            .response()
            .map_err(|e| ScreenshotError::Capture(format!("portal response failed: {e}")))
    })?;

    let url = shot.uri();
    let path = url
        .to_file_path()
        .map_err(|_| ScreenshotError::Capture(format!("portal returned non-file URI: {url}")))?;

    let decoded = image::ImageReader::open(&path)
        .map_err(|e| {
            ScreenshotError::Capture(format!("open portal image {}: {e}", path.display()))
        })?
        .with_guessed_format()
        .map_err(|e| ScreenshotError::Capture(format!("probe portal image: {e}")))?
        .decode()
        .map_err(|e| ScreenshotError::Capture(format!("decode portal image: {e}")))?
        .into_rgba8();

    // The portal wrote a temp file; remove it once decoded (best-effort).
    let _ = std::fs::remove_file(&path);

    let width = decoded.width();
    let height = decoded.height();
    Ok(Snapshot {
        rgba: decoded.into_raw(),
        width,
        height,
        origin_x: 0,
        origin_y: 0,
    })
}
