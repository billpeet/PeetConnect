//! Host clipboard access (Windows).
//!
//! Text-only for now: `CF_UNICODETEXT` read and write through the Win32 clipboard. Image and
//! file-list formats can come later with file transfer. Every call opens and closes the
//! clipboard; the Win32 clipboard is a global lock, so calls are serialised by the OS itself.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

const CF_TEXT: u32 = CF_UNICODETEXT.0 as u32;

/// Closes the clipboard when it leaves scope, so early returns never leave it open.
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: only constructed after a successful OpenClipboard.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Opens the clipboard, retrying briefly because another process may hold it momentarily.
fn open() -> Result<ClipboardGuard> {
    let mut last = None;
    for _ in 0..10 {
        // SAFETY: passing no owner window is valid; the clipboard binds to this thread.
        match unsafe { OpenClipboard(Some(HWND(std::ptr::null_mut()))) } {
            Ok(()) => return Ok(ClipboardGuard),
            Err(err) => {
                last = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    Err(last.map_or_else(
        || anyhow::anyhow!("could not open clipboard"),
        |e| anyhow::Error::new(e).context("opening the clipboard"),
    ))
}

/// Reads the clipboard as text. `Ok(None)` means it is empty or holds no Unicode text.
pub fn get_text() -> Result<Option<String>> {
    let _guard = open()?;
    // SAFETY: the clipboard is open for the guard's lifetime. GetClipboardData returns a handle
    // owned by the clipboard that we must not free; we only lock it to copy the string out.
    unsafe {
        let handle = match GetClipboardData(CF_TEXT) {
            Ok(h) if !h.0.is_null() => h,
            _ => return Ok(None),
        };
        let ptr = GlobalLock(HGLOBAL(handle.0)).cast::<u16>();
        if ptr.is_null() {
            return Ok(None);
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(HGLOBAL(handle.0));
        Ok(Some(text))
    }
}

/// Replaces the clipboard contents with `text`.
pub fn set_text(text: &str) -> Result<()> {
    let _guard = open()?;
    // UTF-16 with a trailing NUL, as CF_UNICODETEXT requires.
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = std::mem::size_of_val(utf16.as_slice());

    // SAFETY: the clipboard is open. On success SetClipboardData takes ownership of the global
    // block, so we must not free it; on every failure path we free it ourselves.
    unsafe {
        EmptyClipboard().context("emptying the clipboard")?;
        let hglobal: HGLOBAL =
            GlobalAlloc(GMEM_MOVEABLE, bytes).context("allocating clipboard memory")?;
        let dst = GlobalLock(hglobal).cast::<u16>();
        if dst.is_null() {
            let _ = GlobalFree(Some(hglobal));
            anyhow::bail!("locking clipboard memory failed");
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        let _ = GlobalUnlock(hglobal);
        if let Err(err) = SetClipboardData(CF_TEXT, Some(HANDLE(hglobal.0))) {
            let _ = GlobalFree(Some(hglobal));
            return Err(anyhow::Error::new(err).context("setting clipboard data"));
        }
    }
    Ok(())
}
