//! C ABI surface — everything declared in include/clipkit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `cp_free_string`.
//! - Failures set a thread-local last-error (code +
//!   message) and return the documented sentinel: null
//!   for the string-returning calls, the CP_ERR_* code
//!   for `cp_set_text`, and 0 for `cp_sequence` (check
//!   `cp_last_error_code()` when 0 matters).
//! - Nothing unwinds across the FFI boundary
//!   (catch_unwind everywhere; release profile also sets
//!   panic = "abort").
//! - The kit is stateless: every call is independent,
//!   so all exports may be called from any thread.

// The exported functions ARE the raw-pointer C ABI
// declared in include/clipkit.h; pointer validity is
// the documented contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::catch_unwind;

use crate::{
    files_result_json, parse_set_text_spec, parse_wait_change_spec, platform, text_result_json,
    wait_for_change, wait_result_json, CP_OK, ERR_INTERNAL, ERR_INVALID_ARG,
};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(CP_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> crate::CpResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

/// Shared body of the string-returning reads: run the
/// JSON-producing call, hand the caller a fresh CString
/// on success, set the last-error and return null on
/// failure.
fn string_call<F: FnOnce() -> crate::CpResult<String>>(f: F) -> *mut c_char {
    match guard(f) {
        Ok(Ok(text)) => match CString::new(text) {
            Ok(s) => {
                clear_error();
                s.into_raw()
            }
            Err(_) => {
                set_error(ERR_INTERNAL, "result string contained a NUL byte");
                std::ptr::null_mut()
            }
        },
        Ok(Err((code, msg))) => {
            set_error(code, &msg);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cp_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn cp_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn cp_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string
/// this library returned and has not been freed yet.
/// Documented in the header.
#[no_mangle]
pub extern "C" fn cp_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

#[no_mangle]
pub extern "C" fn cp_get_text() -> *mut c_char {
    string_call(|| platform::get_text().map(|t| text_result_json(t.as_deref())))
}

#[no_mangle]
pub extern "C" fn cp_set_text(spec_json: *const c_char) -> c_int {
    match guard(|| {
        let inner = cstr(spec_json, "spec_json")
            .and_then(parse_set_text_spec)
            .and_then(|text| platform::set_text(&text));
        match inner {
            Ok(()) => {
                clear_error();
                CP_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn cp_get_files() -> *mut c_char {
    string_call(|| platform::get_files().map(|files| files_result_json(&files)))
}

/// Returns the raw clipboard sequence number with its
/// bits reinterpreted as a C int — the value is for
/// equality comparison only (see the header). On
/// failure returns 0 and sets the last-error.
#[no_mangle]
pub extern "C" fn cp_sequence() -> c_int {
    match guard(platform::sequence) {
        Ok(Ok(seq)) => {
            clear_error();
            seq as c_int
        }
        Ok(Err((code, msg))) => {
            set_error(code, &msg);
            0
        }
        Err(_) => {
            set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
            0
        }
    }
}

#[no_mangle]
pub extern "C" fn cp_wait_change(spec_json: *const c_char) -> *mut c_char {
    let spec = match cstr(spec_json, "spec_json") {
        Ok(text) => match parse_wait_change_spec(text) {
            Ok(spec) => spec,
            Err((code, msg)) => {
                set_error(code, &msg);
                return std::ptr::null_mut();
            }
        },
        Err((code, msg)) => {
            set_error(code, &msg);
            return std::ptr::null_mut();
        }
    };
    string_call(|| {
        wait_for_change(
            platform::sequence,
            |ms| std::thread::sleep(std::time::Duration::from_millis(ms)),
            spec.last_seq,
            spec.timeout_ms,
        )
        .map(wait_result_json)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    // The host tests run off Windows (the gate machine);
    // there every clipboard call is a clean
    // NOT_SUPPORTED, which is exactly what these assert.
    // The Windows behaviour is exercised by the
    // integration test in tests/ on Windows CI.

    #[cfg(not(windows))]
    #[test]
    fn ffi_reads_report_not_supported_off_windows() {
        assert!(cp_get_text().is_null());
        assert_eq!(cp_last_error_code(), crate::ERR_NOT_SUPPORTED as c_int);
        assert!(cp_get_files().is_null());
        assert_eq!(cp_last_error_code(), crate::ERR_NOT_SUPPORTED as c_int);
        assert_eq!(cp_sequence(), 0);
        assert_eq!(cp_last_error_code(), crate::ERR_NOT_SUPPORTED as c_int);
    }

    #[cfg(not(windows))]
    #[test]
    fn ffi_wait_change_reports_not_supported_off_windows() {
        let spec = cs(r#"{"last_seq":3,"timeout_ms":10}"#);
        assert!(cp_wait_change(spec.as_ptr()).is_null());
        assert_eq!(cp_last_error_code(), crate::ERR_NOT_SUPPORTED as c_int);
    }

    #[test]
    fn ffi_set_text_validates_before_touching_the_clipboard() {
        // Null spec and malformed specs fail with the
        // documented codes on every platform — argument
        // validation happens before any platform call.
        assert_eq!(cp_set_text(std::ptr::null()), ERR_INVALID_ARG as c_int);
        assert_eq!(cp_set_text(cs("{}").as_ptr()), ERR_INVALID_ARG as c_int);
        assert_eq!(cp_set_text(cs("{").as_ptr()), crate::ERR_PARSE as c_int);
        assert_eq!(
            cp_set_text(cs(r#"{"text":"x","bogus":1}"#).as_ptr()),
            crate::ERR_PARSE as c_int
        );
        // A null wait-change spec is an argument error,
        // not a crash.
        assert!(cp_wait_change(std::ptr::null()).is_null());
        assert_eq!(cp_last_error_code(), ERR_INVALID_ARG as c_int);
        // Version and free-string are platform-free.
        let v = unsafe { CStr::from_ptr(cp_version()) }.to_str().unwrap();
        assert_eq!(v, crate::VERSION);
        cp_free_string(std::ptr::null_mut()); // no-op, must not crash
    }

    #[cfg(not(windows))]
    #[test]
    fn ffi_set_text_valid_spec_reaches_the_platform_off_windows() {
        // A well-formed spec parses, then the platform
        // layer answers NOT_SUPPORTED on this host.
        assert_eq!(
            cp_set_text(cs(r#"{"text":"hello"}"#).as_ptr()),
            crate::ERR_NOT_SUPPORTED as c_int
        );
    }

    #[test]
    fn ffi_result_strings_round_trip_through_json() {
        // The shaping helpers' output parses with the
        // family engine and carries the documented keys.
        let v = crate::json::parse(&text_result_json(Some("a\"b"))).unwrap();
        assert_eq!(v.get("text").unwrap().as_str(), Some("a\"b"));
        let v = crate::json::parse(&files_result_json(&["C:\\x".to_string()])).unwrap();
        assert_eq!(
            v.get("files").unwrap().as_array().unwrap()[0].as_str(),
            Some("C:\\x")
        );
    }
}
