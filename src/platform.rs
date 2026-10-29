//! Platform layer: the only place Windows APIs are
//! touched. On non-Windows targets everything returns
//! ERR_NOT_SUPPORTED so the pure logic and FFI contract
//! stay testable anywhere.
//!
//! Every extern block MUST carry #[link(name = "...")]
//! naming its import library (user32 / kernel32 /
//! shell32). Without it the build fails at LINK time
//! with LNK2019 — check and clippy cannot catch it;
//! CI's real build can.
//!
//! Clipboard discipline (the contracts from lib.rs,
//! enforced here):
//! - Every open goes through `ClipboardGuard::open`,
//!   which retries up to `OPEN_ATTEMPTS` times because
//!   any other app may be holding the clipboard open,
//!   and the guard closes the clipboard on drop — every
//!   path out, success or failure.
//! - `set_text` allocates and fills its global block
//!   BEFORE opening the clipboard (an allocation
//!   failure leaves the current contents untouched),
//!   and after a successful `SetClipboardData` the
//!   block belongs to the clipboard: it is never freed
//!   here. It is freed only on the paths where the
//!   hand-off did not happen.

use crate::ERR_NOT_SUPPORTED;

pub type PResult<T> = Result<T, (i32, String)>;

pub fn not_supported<T>(what: &str) -> PResult<T> {
    Err((
        ERR_NOT_SUPPORTED,
        format!("{what} is only available on Windows"),
    ))
}

/// The current clipboard sequence number — the cheap
/// change check. Needs no clipboard open and no window.
#[cfg(windows)]
pub fn sequence() -> PResult<u32> {
    win::sequence()
}

#[cfg(not(windows))]
pub fn sequence() -> PResult<u32> {
    not_supported("clipboard access")
}

/// The clipboard's text, or `None` when the clipboard
/// holds no text format.
#[cfg(windows)]
pub fn get_text() -> PResult<Option<String>> {
    win::get_text()
}

#[cfg(not(windows))]
pub fn get_text() -> PResult<Option<String>> {
    not_supported("clipboard access")
}

/// Replace the clipboard's contents with `text`
/// (CF_UNICODETEXT).
#[cfg(windows)]
pub fn set_text(text: &str) -> PResult<()> {
    win::set_text(text)
}

#[cfg(not(windows))]
pub fn set_text(_text: &str) -> PResult<()> {
    not_supported("clipboard access")
}

/// The clipboard's dropped-file list (CF_HDROP) as
/// absolute paths — empty when the clipboard holds no
/// file list.
#[cfg(windows)]
pub fn get_files() -> PResult<Vec<String>> {
    win::get_files()
}

#[cfg(not(windows))]
pub fn get_files() -> PResult<Vec<String>> {
    not_supported("clipboard access")
}

// ---------------------------------------------------------------------------
// Windows: user32 clipboard + kernel32 global memory + shell32 HDROP
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::PResult;
    use crate::{ERR_ACCESS_DENIED, ERR_PLATFORM};
    use std::os::raw::c_void;
    use std::time::Duration;

    // Clipboard formats.
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;
    // GlobalAlloc flags: movable (required for memory
    // handed to the clipboard) — ownership-transfer
    // safe because the clipboard moves the block, never
    // a pointer into it.
    const GMEM_MOVEABLE: u32 = 0x0002;
    // DragQueryFileW index meaning "give me the count".
    const HDROP_COUNT_INDEX: u32 = 0xFFFF_FFFF;

    /// Open attempts before giving up: the clipboard is
    /// a shared resource and another app may hold it
    /// open for a short stretch.
    const OPEN_ATTEMPTS: u32 = 10;
    const OPEN_RETRY_DELAY_MS: u64 = 10;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(new_window_owner: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn GetClipboardData(format: u32) -> isize;
        fn SetClipboardData(format: u32, mem: isize) -> isize;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardSequenceNumber() -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(mem: isize) -> *mut c_void;
        fn GlobalUnlock(mem: isize) -> i32;
        fn GlobalFree(mem: isize) -> isize;
        fn GlobalSize(mem: isize) -> usize;
        fn GetLastError() -> u32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn DragQueryFileW(drop: isize, index: u32, file: *mut u16, cch: u32) -> u32;
    }

    fn last_error() -> u32 {
        // SAFETY: GetLastError takes no arguments.
        unsafe { GetLastError() }
    }

    fn platform_err(what: &str) -> (i32, String) {
        (
            ERR_PLATFORM,
            format!("{what} failed (GetLastError={})", last_error()),
        )
    }

    /// RAII owner of one clipboard open: closes on
    /// drop, so no path out of a clipboard operation
    /// can leave it held open.
    struct ClipboardGuard;

    impl ClipboardGuard {
        fn open() -> PResult<ClipboardGuard> {
            for attempt in 0..OPEN_ATTEMPTS {
                // SAFETY: a NULL owner means the
                // clipboard is associated with the
                // current task — the documented owner
                // for a windowless library caller.
                if unsafe { OpenClipboard(0) } != 0 {
                    return Ok(ClipboardGuard);
                }
                if attempt + 1 < OPEN_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(OPEN_RETRY_DELAY_MS));
                }
            }
            Err((
                ERR_ACCESS_DENIED,
                format!(
                    "OpenClipboard failed after {OPEN_ATTEMPTS} attempts \
                     (GetLastError={}); another application may be holding \
                     the clipboard open",
                    last_error()
                ),
            ))
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            // SAFETY: pairs with the successful
            // OpenClipboard in `open`; the return value
            // has no actionable failure for a closer.
            unsafe {
                CloseClipboard();
            }
        }
    }

    pub fn sequence() -> PResult<u32> {
        // SAFETY: GetClipboardSequenceNumber takes no
        // arguments, needs no open clipboard, and has
        // no failure mode distinct from returning the
        // counter (0 when this thread has no window
        // station access — returned as-is; the value is
        // only ever compared for equality).
        Ok(unsafe { GetClipboardSequenceNumber() })
    }

    pub fn get_text() -> PResult<Option<String>> {
        // SAFETY: IsClipboardFormatAvailable takes a
        // format constant and cannot fail meaningfully
        // for this use; 0 = the format is not present.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return Ok(None);
        }
        let _guard = ClipboardGuard::open()?;
        // SAFETY: the clipboard is open (guard) and the
        // format was available a moment ago; a NULL here
        // means it vanished in between — a race, read as
        // "no text", not an error. The handle belongs to
        // the clipboard: never freed here.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if handle == 0 {
            return Ok(None);
        }
        // SAFETY: `handle` is a global memory block
        // owned by the clipboard; GlobalSize bounds the
        // read. The lock is released before returning.
        let text = unsafe {
            let size_units = GlobalSize(handle) / 2;
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                return Err(platform_err("GlobalLock on clipboard text"));
            }
            let units = std::slice::from_raw_parts(ptr, size_units);
            let end = units.iter().position(|&u| u == 0).unwrap_or(size_units);
            let s = String::from_utf16_lossy(&units[..end]);
            GlobalUnlock(handle);
            s
        };
        Ok(Some(text))
    }

    pub fn set_text(text: &str) -> PResult<()> {
        // Encode first, allocate second, open LAST: an
        // allocation failure must leave the current
        // clipboard contents untouched.
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = units.len() * 2;
        // SAFETY: GlobalAlloc with GMEM_MOVEABLE; the
        // block is owned by us until SetClipboardData
        // accepts it (see the hand-off below).
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle == 0 {
            return Err(platform_err("GlobalAlloc for clipboard text"));
        }
        // SAFETY: `handle` was just allocated with
        // exactly `bytes` bytes; the copy fills it and
        // the lock is released immediately.
        unsafe {
            let ptr = GlobalLock(handle) as *mut u16;
            if ptr.is_null() {
                GlobalFree(handle);
                return Err(platform_err("GlobalLock for clipboard text"));
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            GlobalUnlock(handle);
        }
        let _guard = ClipboardGuard::open().inspect_err(|_| {
            // SAFETY: the hand-off never happened, so
            // the block is still ours to free.
            unsafe {
                GlobalFree(handle);
            }
        })?;
        // SAFETY: the clipboard is open (guard).
        if unsafe { EmptyClipboard() } == 0 {
            let e = platform_err("EmptyClipboard");
            // SAFETY: no hand-off happened; still ours.
            unsafe {
                GlobalFree(handle);
            }
            return Err(e);
        }
        // SAFETY: the clipboard is open and emptied;
        // `handle` is a movable global block holding
        // NUL-terminated UTF-16 text.
        let placed = unsafe { SetClipboardData(CF_UNICODETEXT, handle) };
        if placed == 0 {
            let e = platform_err("SetClipboardData");
            // SAFETY: the hand-off failed, so the block
            // was NOT adopted by the clipboard and is
            // still ours to free.
            unsafe {
                GlobalFree(handle);
            }
            return Err(e);
        }
        // Ownership transferred: the clipboard owns the
        // block now. Never free it.
        Ok(())
    }

    pub fn get_files() -> PResult<Vec<String>> {
        // SAFETY: format-availability probe, as get_text.
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return Ok(Vec::new());
        }
        let _guard = ClipboardGuard::open()?;
        // SAFETY: the clipboard is open (guard); NULL
        // means the format vanished in between — an
        // empty list, not an error. The HDROP belongs
        // to the clipboard: never freed here.
        let drop_handle = unsafe { GetClipboardData(CF_HDROP) };
        if drop_handle == 0 {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        // SAFETY: `drop_handle` is a live HDROP for as
        // long as the clipboard stays open (guard); the
        // count call passes the documented count index
        // with a NULL buffer.
        let count =
            unsafe { DragQueryFileW(drop_handle, HDROP_COUNT_INDEX, std::ptr::null_mut(), 0) };
        for i in 0..count {
            // SAFETY: as above; a NULL-buffer call
            // returns the path's length in characters
            // (excluding the NUL).
            let len = unsafe { DragQueryFileW(drop_handle, i, std::ptr::null_mut(), 0) };
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            // SAFETY: `buf` has room for len + NUL; the
            // call writes at most that and returns the
            // count written, excluding the NUL.
            let written =
                unsafe { DragQueryFileW(drop_handle, i, buf.as_mut_ptr(), buf.len() as u32) };
            if written == 0 {
                continue;
            }
            files.push(String::from_utf16_lossy(&buf[..written as usize]));
        }
        Ok(files)
    }
}
