//! System clipboard (Go: atotto/clipboard `WriteAll`). The handle is kept for the process lifetime
//! because on X11 the selection is served by the owning process.

use std::sync::{Mutex, OnceLock};

static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();

/// Copies `text` to the clipboard; the error is shown in the status line on failure.
pub fn write_all(text: &str) -> Result<(), String> {
    let cell = CLIPBOARD.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().map_err(|_| "clipboard unavailable".to_string())?;
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    match guard.as_mut() {
        Some(cb) => cb.set_text(text.to_string()).map_err(|e| e.to_string()),
        None => Err("clipboard unavailable".into()),
    }
}
