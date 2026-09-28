//! System clipboard image/text writing via `arboard`.
//!
//! Linux caveat — why we hold a *persistent* `Clipboard`:
//!
//! X11 and Wayland don't store clipboard data centrally. The process that
//! copied keeps *owning* the selection and must stay alive to answer every
//! paste request from other apps. `arboard` relinquishes that ownership and
//! tears down its background serving thread the instant its last `Clipboard`
//! handle is dropped — at which point it only tries to hand the data to a
//! desktop *clipboard manager* (with a ~100ms timeout). Many 统信 UOS / Deepin
//! sessions — especially Wayland ones, reached through XWayland — have no X11
//! clipboard manager listening, so a dropped `Clipboard` loses the image almost
//! immediately: the copy "succeeds" yet nothing can be pasted anywhere.
//!
//! Creating a fresh `Clipboard` per copy (and dropping it) is therefore wrong on
//! Linux even though our helper process stays resident — the *object*, not just
//! the process, has to outlive the copy. We keep one `Clipboard` alive for the
//! whole process so its serving thread keeps answering paste requests until the
//! next copy replaces the contents. Windows/macOS clipboards are OS-managed and
//! persist on their own, so there we keep the original create-set-drop path.

use crate::common::{Result, ScreenshotError};
use std::borrow::Cow;

/// Write raw RGBA pixels to the system clipboard as an image.
pub fn copy_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<()> {
    let image = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: Cow::Owned(rgba),
    };
    with_writer("set image", |cb| cb.set_image(image))
}

/// Write plain text (e.g. a picked color value) to the system clipboard.
pub fn copy_text(text: &str) -> Result<()> {
    let text = text.to_string();
    with_writer("set text", |cb| cb.set_text(text))
}

/// Read plain text from the system clipboard (for paste into the text tool).
pub fn paste_text() -> Option<String> {
    // Reads don't need a persistent owner. While our persistent writer owns the
    // selection this extra handle is just a clone of the same global context, so
    // dropping it won't relinquish ownership (arboard only does that when its
    // *last* handle is dropped).
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok()
}

/// Run a clipboard *write* through a handle that lives long enough for the data
/// to actually be pasteable. See the module docs for the Linux ownership caveat.
fn with_writer<F>(what: &str, op: F) -> Result<()>
where
    F: FnOnce(&mut arboard::Clipboard) -> std::result::Result<(), arboard::Error>,
{
    #[cfg(target_os = "linux")]
    {
        use std::sync::Mutex;
        // Held for the entire process lifetime on purpose: arboard's background
        // thread keeps serving the selection only while this handle is alive.
        // Writes are serialized on the helper's main thread, so a plain Mutex is
        // enough and matches arboard's "operate from a single thread" contract.
        static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
        let mut guard = CLIPBOARD.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            let cb = arboard::Clipboard::new()
                .map_err(|e| ScreenshotError::Clipboard(format!("open clipboard: {e}")))?;
            *guard = Some(cb);
        }
        op(guard.as_mut().unwrap())
            .map_err(|e| ScreenshotError::Clipboard(format!("{what}: {e}")))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut cb = arboard::Clipboard::new()
            .map_err(|e| ScreenshotError::Clipboard(format!("open clipboard: {e}")))?;
        op(&mut cb).map_err(|e| ScreenshotError::Clipboard(format!("{what}: {e}")))
    }
}
