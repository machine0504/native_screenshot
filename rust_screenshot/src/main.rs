//! Snipaste-style screenshot helper executable.
//!
//! Launched by the Flutter app (via the `native_screenshot` plugin). It shows a
//! fullscreen overlay for region selection + annotation, then:
//!   * writes the final image to the system clipboard,
//!   * optionally saves a PNG via a native dialog,
//!   * prints a single result line to stdout for the caller:
//!       `OK <path-to-temp-png>`  on success
//!       `CANCELLED`               if the user cancelled
//!       `ERROR <message>`         on failure
//!
//! On Windows the binary is built as a GUI subsystem app (no console window).
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod annotations;
mod capture;
mod clipboard;
mod common;
// SVG-outline toolbar icons (rendered with tiny-skia).
mod icons;
// macOS-only GPU present path (Metal via wgpu).
#[cfg(target_os = "macos")]
mod gpu;
// Always compiled: macOS gets the real AppKit helpers, other platforms get
// no-op stubs, so call sites need no `cfg` gating.
mod macos;
// Windows-only overlay helpers: disable the DWM open/close transition so the
// capture window appears/vanishes without the system zoom-in/zoom-out animation.
#[cfg(target_os = "windows")]
mod win;
mod overlay;
mod regions;
mod text;
mod toolbar;

use std::io::Write;

use common::Result;
use overlay::{Outcome, Overlay};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

fn main() {
    let _ = env_logger::try_init();

    // macOS：尽早声明为 Accessory（后台、不进 Dock）。helper 以裸可执行文件嵌在宿主
    // bundle 内，启动时读到的是宿主的常规(前台)Info.plist；若等 winit 初始化才设
    // Accessory，中间空窗期会让 Dock 闪出一个宿主图标。放在 main 第一步关掉它。
    macos::set_accessory_activation_policy();

    // Resident mode (`--serve`): keep the process + frameworks warm and capture
    // on demand over a stdin/stdout line protocol, so clicks open instantly
    // instead of paying a cold process + framework start every time. The Flutter
    // side falls back to the one-shot mode below if anything goes wrong.
    if std::env::args().any(|a| a == "--serve") {
        if let Err(e) = serve() {
            emit(&format!("ERROR {e}"));
        }
        return;
    }

    // One-shot mode (default): capture once and exit.
    match run() {
        Ok(Some(path)) => emit(&format!("OK {}", path.display())),
        Ok(None) => emit("CANCELLED"),
        Err(e) => emit(&format!("ERROR {e}")),
    }
}

fn emit(line: &str) {
    // The Flutter side reads result lines from stdout.
    println!("{line}");
    let _ = std::io::stdout().flush();
}

/// Apply once-per-process platform setup (DPI awareness, screen-capture
/// permission). Safe to call repeatedly.
fn platform_preflight() {
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // macOS：屏幕录制(TCC)权限**不在 helper 内自行申请**。
    //
    // helper 由宿主 App 以子进程方式(posix_spawn / Dart Process.start)拉起，TCC 的
    // “responsible process（负责进程）”会归属到宿主 App，截图时直接复用宿主的屏幕
    // 录制授权——这与 Chrome/Electron 的 helper 子进程共享主程序权限是同一机制。
    //
    // 若在此调用 CGRequestScreenCaptureAccess()，helper 会以自己的独立 bundle 身份
    // 在“系统设置 > 隐私与安全性 > 屏幕录制”中注册成第二个同名条目（正是要修复的
    // 问题）。因此移除自请求，权限的申请/授予统一由宿主 App 负责。
}

fn run() -> Result<Option<std::path::PathBuf>> {
    platform_preflight();

    let snapshot = capture::capture_virtual_desktop()?;
    // Enumerate window rects at the same instant as the snapshot so one-click
    // region detection stays consistent with the frozen image.
    let regions = regions::detect_regions(
        snapshot.origin_x,
        snapshot.origin_y,
        snapshot.width,
        snapshot.height,
    );

    let mut builder = EventLoop::builder();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        // helper 现在以“宿主 bundle 内部可执行文件”的身份运行（mainBundle = 宿主、
        // 无 LSUIElement）。显式声明 Accessory，避免它继承宿主的 Regular 策略而在
        // 程序坞冒出第二个宿主图标；overlay 仍可置顶并接收键盘输入。
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder
        .build()
        .map_err(|e| common::ScreenshotError::Overlay(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = Overlay::new(snapshot, regions);
    event_loop
        .run_app(&mut app)
        .map_err(|e| common::ScreenshotError::Overlay(e.to_string()))?;

    process_outcome(std::mem::replace(&mut app.outcome, Outcome::Cancelled))
}

/// Turn an [`Outcome`] into the final artifact: clipboard copy + PNG temp file
/// (+ optional save dialog). Shared by one-shot and resident modes.
fn process_outcome(outcome: Outcome) -> Result<Option<std::path::PathBuf>> {
    match outcome {
        Outcome::Cancelled => Ok(None),
        // 基础设施失败 → 以 Err 上抛，最终 emit 成 `ERROR ...`（而非静默的 CANCELLED）。
        Outcome::Error(msg) => Err(common::ScreenshotError::Overlay(msg)),
        Outcome::Capture {
            rgba,
            width,
            height,
            save,
            copy_only,
        } => {
            // Clipboard (best-effort).
            if let Err(e) = clipboard::copy_rgba(width, height, rgba.clone()) {
                log::warn!("clipboard copy failed: {e}");
            }

            // 「复制」按钮：只写剪贴板，不回传宿主（宿主收到 CANCELLED → 不插入提问框）。
            if copy_only {
                return Ok(None);
            }

            let png = encode_png(width, height, &rgba)?;

            // 「保存」按钮：仅让用户把截图存成文件，**不回传宿主、不插入提问框**——
            // 插入提问框是「√ 确认」按钮的职责，复制到剪贴板是「复制」按钮的职责。
            // 此前无论保存成功还是取消都会返回临时路径，导致「取消保存也被插入提问框」。
            // 现在保存分支一律返回 CANCELLED（Ok(None)）：取消 → 什么都不做；
            // 选定路径 → 只写入该文件。
            if save {
                // The overlay is gone; make sure the panel comes to the front
                // (macOS-only; the helper otherwise runs as a background app).
                crate::macos::bring_app_to_front();

                let mut dialog = rfd::FileDialog::new()
                    .set_file_name(default_save_name())
                    .add_filter("PNG", &["png"]);
                if let Some(dir) = default_save_dir() {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(dest) = dialog.save_file() {
                    let _ = std::fs::write(&dest, &png);
                }
                return Ok(None);
            }

            // 「√ 确认」：写临时 PNG 供宿主读取并插入提问框。
            let path = std::env::temp_dir().join(format!(
                "screenshot_{}.png",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ));
            std::fs::write(&path, &png)?;

            Ok(Some(path))
        }
    }
}

/// Print the one result line the Flutter side expects for one capture.
fn emit_outcome(outcome: Outcome) {
    match process_outcome(outcome) {
        Ok(Some(path)) => emit(&format!("OK {}", path.display())),
        Ok(None) => emit("CANCELLED"),
        Err(e) => emit(&format!("ERROR {e}")),
    }
}

/// Resident-server commands delivered to the winit loop from the stdin thread.
enum Cmd {
    Capture,
    Quit,
}

/// Long-lived overlay host: stays idle (no window) between captures and shows a
/// fresh overlay per `Capture` command, emitting one result line each time.
#[derive(Default)]
struct Server {
    ready: bool,            // winit `resumed` seen → safe to create windows
    pending_capture: bool,  // a capture arrived before `resumed`
    active: Option<Overlay>,
}

impl Server {
    fn start_capture(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return; // a session is already on screen
        }
        platform_preflight();
        let snapshot = match capture::capture_virtual_desktop() {
            Ok(s) => s,
            Err(e) => {
                emit(&format!("ERROR {e}"));
                return;
            }
        };
        let regions = regions::detect_regions(
        snapshot.origin_x,
        snapshot.origin_y,
        snapshot.width,
        snapshot.height,
    );
        let mut overlay = Overlay::new(snapshot, regions);
        overlay.open(event_loop);
        if overlay.is_done() {
            // Window/surface creation failed inside open().
            emit_outcome(std::mem::replace(&mut overlay.outcome, Outcome::Cancelled));
            return;
        }
        self.active = Some(overlay);
    }
}

impl ApplicationHandler<Cmd> for Server {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.ready {
            self.ready = true;
            emit("READY");
            if std::mem::take(&mut self.pending_capture) {
                self.start_capture(event_loop);
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, cmd: Cmd) {
        match cmd {
            Cmd::Capture => {
                if !self.ready {
                    self.pending_capture = true;
                } else {
                    self.start_capture(event_loop);
                }
            }
            Cmd::Quit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(overlay) = self.active.as_mut() else {
            return;
        };
        overlay.handle_window_event(event);
        if overlay.is_done() {
            let overlay = self.active.take().unwrap();
            emit_outcome(overlay.outcome);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Honor the active overlay's frame pacing (deferred-redraw deadline),
        // otherwise just wait for the next event.
        match self.active.as_mut().and_then(|o| o.pump_frame()) {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

fn serve() -> Result<()> {
    let mut builder = EventLoop::<Cmd>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder
        .build()
        .map_err(|e| common::ScreenshotError::Overlay(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    // Bridge blocking stdin → loop wake-ups via the event-loop proxy.
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            let t = line.trim();
            if t.eq_ignore_ascii_case("capture") {
                if proxy.send_event(Cmd::Capture).is_err() {
                    return;
                }
            } else if t.eq_ignore_ascii_case("quit") {
                let _ = proxy.send_event(Cmd::Quit);
                return;
            }
        }
        // stdin closed (parent exited) → ask the loop to quit too.
        let _ = proxy.send_event(Cmd::Quit);
    });

    let mut server = Server::default();
    event_loop
        .run_app(&mut server)
        .map_err(|e| common::ScreenshotError::Overlay(e.to_string()))?;
    Ok(())
}

/// Default file name proposed in the Save dialog (the user can change it).
fn default_save_name() -> String {
    "截图.png".to_string()
}

/// Best-effort default folder for the Save dialog: the user's Pictures folder
/// when it exists, otherwise their home directory. `None` lets the OS decide.
fn default_save_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var_os("HOME");
    let home = std::path::PathBuf::from(home?);
    let pictures = home.join("Pictures");
    if pictures.is_dir() {
        Some(pictures)
    } else {
        Some(home)
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    use image::codecs::png::PngEncoder;
    use image::{ImageBuffer, ImageEncoder, Rgba};

    let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| common::ScreenshotError::Encoding("buffer size mismatch".into()))?;

    let mut out = Vec::new();
    PngEncoder::new(std::io::Cursor::new(&mut out))
        .write_image(buf.as_raw(), width, height, image::ExtendedColorType::Rgba8)?;
    Ok(out)
}
