//! Windows-only integration test: the real clipboard
//! round trip through the real C ABI.
//!
//! set text -> the sequence number moves -> get text
//! returns what was set -> cp_wait_change sees the move
//! and then correctly reports a quiet clipboard.
//!
//! The clipboard is a shared desktop resource, so the
//! test skips gracefully (prints why and passes) when
//! no clipboard is available — a headless CI session
//! without an interactive desktop is exactly that case.
//! On any normal Windows session (including the
//! windows-latest CI runners) the round trip runs for
//! real.
#![cfg(windows)]

use clip_kit::ffi::{
    cp_free_string, cp_get_files, cp_get_text, cp_last_error_code, cp_sequence, cp_set_text,
    cp_wait_change,
};
use clip_kit::{ERR_ACCESS_DENIED, ERR_NOT_SUPPORTED, ERR_PLATFORM};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

fn take(p: *mut c_char) -> String {
    assert!(!p.is_null(), "expected a result string, got null");
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
    cp_free_string(p);
    s
}

fn clipboard_unavailable(code: i32) -> bool {
    code == ERR_NOT_SUPPORTED || code == ERR_ACCESS_DENIED || code == ERR_PLATFORM
}

#[test]
fn clipboard_text_roundtrip_and_change_detection() {
    // A marker unique to this test run, so a stale
    // clipboard can never make the round trip pass.
    let marker = format!(
        "clipkit-integration-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let before = cp_sequence();
    if clipboard_unavailable(cp_last_error_code()) {
        eprintln!("skipping: clipboard unavailable (sequence read failed)");
        return;
    }

    let spec = CString::new(format!("{{\"text\":\"{marker}\"}}")).unwrap();
    let rc = cp_set_text(spec.as_ptr());
    if rc != 0 {
        if clipboard_unavailable(rc) {
            eprintln!("skipping: clipboard unavailable (set_text rc={rc})");
            return;
        }
        panic!("cp_set_text failed with code {rc}");
    }

    // Setting the text must move the sequence number.
    let after = cp_sequence();
    assert_ne!(
        before, after,
        "sequence number must change after cp_set_text"
    );

    // The text reads back exactly.
    let got = take(cp_get_text());
    let v = clip_kit::json::parse(&got).unwrap();
    assert_eq!(v.get("text").unwrap().as_str(), Some(marker.as_str()));

    // cp_wait_change with the pre-set sequence sees the
    // change immediately...
    let wait_spec = CString::new(format!(
        "{{\"last_seq\":{},\"timeout_ms\":1000}}",
        before as u32
    ))
    .unwrap();
    let waited = take(cp_wait_change(wait_spec.as_ptr()));
    let v = clip_kit::json::parse(&waited).unwrap();
    assert_eq!(v.get("changed").unwrap().as_bool(), Some(true));

    // ...and with the current sequence it reports a
    // quiet clipboard after a short timeout.
    let current = cp_sequence();
    let wait_spec = CString::new(format!(
        "{{\"last_seq\":{},\"timeout_ms\":60}}",
        current as u32
    ))
    .unwrap();
    let waited = take(cp_wait_change(wait_spec.as_ptr()));
    let v = clip_kit::json::parse(&waited).unwrap();
    assert_eq!(v.get("changed").unwrap().as_bool(), Some(false));
    assert_eq!(v.get("seq").unwrap().as_i64(), Some(current as u32 as i64));

    // The file list parses (content depends on the
    // session, so only the shape is asserted here).
    let files = take(cp_get_files());
    let v = clip_kit::json::parse(&files).unwrap();
    assert!(v.get("files").unwrap().as_array().is_some());
}
