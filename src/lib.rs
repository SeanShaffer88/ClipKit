//! ClipKit — clipboard text and file-list access, plus
//! change detection, behind a small C ABI.
//!
//! The shape is deliberately boring: three stateless
//! reads/writes against the Windows clipboard
//! (`CF_UNICODETEXT` text, `CF_HDROP` dropped-file lists)
//! and one cheap change signal — the clipboard *sequence
//! number* (`GetClipboardSequenceNumber`), an unsigned
//! counter Windows bumps on every clipboard change. A
//! host that wants "tell me when the clipboard changes"
//! polls `cp_sequence` (no clipboard open, no window
//! needed) or blocks in `cp_wait_change`, and only opens
//! the clipboard when the number actually moved.
//!
//! Contracts that are load-bearing, not decoration:
//!
//! - **`cp_set_text` transfers ownership.** The global
//!   memory block handed to `SetClipboardData` belongs
//!   to the clipboard afterwards and must never be freed
//!   by the caller — and the block is allocated *before*
//!   the clipboard is opened, so an allocation failure
//!   leaves the current clipboard contents untouched.
//! - **The clipboard is a shared, contended resource.**
//!   Another app may hold it open at any moment, so
//!   every open retries (10 attempts, 10 ms apart) and
//!   every successful open is closed on every path out.
//! - **A missing format is not an error.** Text reads
//!   return `{"text": null}` when the clipboard holds no
//!   text; file reads return an empty array. Errors are
//!   for when the clipboard itself could not be reached.
//! - **Sequence numbers are compared, never ordered.**
//!   The value is the raw 32-bit counter; `cp_sequence`'
//!   C `int` return reinterprets those bits as signed,
//!   and `cp_wait_change` accepts the signed spelling
//!   back, so the pair round-trips across the wrap.
//!
//! The core in this module is pure and unit-tested on
//! every platform: spec parsing, JSON shaping, and the
//! wait-for-change decision loop (run against an
//! injected sequence source). The OS surface is exactly
//! `src/platform.rs`; the C ABI is `src/ffi.rs`.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/clipkit.h).
pub const CP_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;

pub type CpResult<T> = Result<T, (i32, String)>;

/// How often `cp_wait_change` re-checks the sequence
/// number while waiting. Part of the contract: a change
/// is noticed within one interval, and the wait never
/// busy-spins.
pub const WAIT_POLL_MS: u64 = 25;

/// The longest `cp_wait_change` will block, however
/// large a `timeout_ms` the caller asks for.
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Spec parsing
// ---------------------------------------------------------------------------

fn parse_spec_object(text: &str, what: &str) -> CpResult<json::Value> {
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("{what} is not valid JSON: {e}")))?;
    if value.as_object().is_none() {
        return Err((ERR_PARSE, format!("{what} must be a JSON object")));
    }
    Ok(value)
}

fn reject_unknown_keys(value: &json::Value, allowed: &[&str], what: &str) -> CpResult<()> {
    if let Some(obj) = value.as_object() {
        for (key, _) in obj {
            if !allowed.contains(&key.as_str()) {
                return Err((ERR_PARSE, format!("unknown {what} key \"{key}\"")));
            }
        }
    }
    Ok(())
}

/// Parse the `cp_set_text` spec: `{"text": string}`.
/// The text is required and must be a string (empty is
/// fine — it sets an empty text clipboard). Returns the
/// text to place on the clipboard.
pub fn parse_set_text_spec(text: &str) -> CpResult<String> {
    let value = parse_spec_object(text, "set-text spec")?;
    reject_unknown_keys(&value, &["text"], "set-text spec")?;
    match value.get("text") {
        None => Err((
            ERR_INVALID_ARG,
            "set-text spec is missing required key \"text\"".to_string(),
        )),
        Some(v) => match v.as_str() {
            Some(s) => Ok(s.to_string()),
            None => Err((
                ERR_INVALID_ARG,
                format!("\"text\" must be a string, got {}", v.type_name()),
            )),
        },
    }
}

/// The parsed `cp_wait_change` spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitChangeSpec {
    /// The sequence number the caller last saw; the
    /// wait ends as soon as the live number differs.
    pub last_seq: u32,
    /// How long to wait, already capped at
    /// `MAX_TIMEOUT_MS`. 0 = check once, return at once.
    pub timeout_ms: u64,
}

/// Interpret an integer as a clipboard sequence number.
/// The counter is unsigned 32-bit, but `cp_sequence`
/// hands it back as a C `int`, so a caller feeding that
/// value straight back in may present the negative
/// (signed) spelling of the same bits — accept both.
/// Anything outside the two spellings' ranges is an
/// error, not a guess.
fn parse_sequence_number(v: &json::Value, key: &str) -> CpResult<u32> {
    let n = v.as_i64().ok_or((
        ERR_INVALID_ARG,
        format!("\"{key}\" must be an integer, got {}", v.type_name()),
    ))?;
    if (0..=u32::MAX as i64).contains(&n) {
        Ok(n as u32)
    } else if (i32::MIN as i64..=-1).contains(&n) {
        Ok(n as i32 as u32)
    } else {
        Err((
            ERR_INVALID_ARG,
            format!("\"{key}\" is out of range for a sequence number: {n}"),
        ))
    }
}

/// Parse the `cp_wait_change` spec:
/// `{"last_seq": u32, "timeout_ms": n}`. `last_seq` is
/// required; `timeout_ms` defaults to 0 (a single
/// immediate check) and is *capped* — not rejected — at
/// `MAX_TIMEOUT_MS`.
pub fn parse_wait_change_spec(text: &str) -> CpResult<WaitChangeSpec> {
    let value = parse_spec_object(text, "wait-change spec")?;
    reject_unknown_keys(&value, &["last_seq", "timeout_ms"], "wait-change spec")?;
    let last_seq = match value.get("last_seq") {
        None => {
            return Err((
                ERR_INVALID_ARG,
                "wait-change spec is missing required key \"last_seq\"".to_string(),
            ))
        }
        Some(v) => parse_sequence_number(v, "last_seq")?,
    };
    let timeout_ms = match value.get("timeout_ms") {
        None => 0,
        Some(v) => {
            let n = v.as_i64().ok_or((
                ERR_INVALID_ARG,
                format!("\"timeout_ms\" must be an integer, got {}", v.type_name()),
            ))?;
            if n < 0 {
                return Err((
                    ERR_INVALID_ARG,
                    format!("\"timeout_ms\" must be non-negative, got {n}"),
                ));
            }
            (n as u64).min(MAX_TIMEOUT_MS)
        }
    };
    Ok(WaitChangeSpec {
        last_seq,
        timeout_ms,
    })
}

// ---------------------------------------------------------------------------
// Result shaping
// ---------------------------------------------------------------------------

/// `cp_get_text` result: `{"text": "..."}` or
/// `{"text": null}` when the clipboard holds no text.
pub fn text_result_json(text: Option<&str>) -> String {
    match text {
        Some(t) => format!("{{\"text\":\"{}\"}}", json_escape(t)),
        None => "{\"text\":null}".to_string(),
    }
}

/// `cp_get_files` result: `{"files": [...]}` — an
/// empty array when the clipboard holds no file list.
pub fn files_result_json(files: &[String]) -> String {
    let mut out = String::from("{\"files\":[");
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&json_escape(f));
        out.push('"');
    }
    out.push_str("]}");
    out
}

/// The outcome of one wait: whether the sequence number
/// moved away from `last_seq`, and the number observed
/// when the wait ended (the new current value when
/// `changed`, the unchanged value otherwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitOutcome {
    pub changed: bool,
    pub seq: u32,
}

/// `cp_wait_change` result: `{"changed": bool, "seq": n}`.
pub fn wait_result_json(outcome: WaitOutcome) -> String {
    format!(
        "{{\"changed\":{},\"seq\":{}}}",
        outcome.changed, outcome.seq
    )
}

// ---------------------------------------------------------------------------
// Wait-for-change decision logic
// ---------------------------------------------------------------------------

/// Poll `next_seq` until it differs from `last_seq` or
/// `timeout_ms` (capped at `MAX_TIMEOUT_MS`) elapses.
/// Checks happen immediately, then every `WAIT_POLL_MS`
/// (the final step is shortened so the wait ends on the
/// timeout, not one interval past it). `sleep_ms` is
/// injected so tests drive the loop with a fake clock;
/// production passes a real sleep. A source error
/// propagates immediately — a wait that cannot read the
/// sequence number says so rather than timing out
/// silently.
pub fn wait_for_change(
    mut next_seq: impl FnMut() -> CpResult<u32>,
    mut sleep_ms: impl FnMut(u64),
    last_seq: u32,
    timeout_ms: u64,
) -> CpResult<WaitOutcome> {
    let timeout = timeout_ms.min(MAX_TIMEOUT_MS);
    let mut elapsed: u64 = 0;
    loop {
        let seq = next_seq()?;
        if seq != last_seq {
            return Ok(WaitOutcome { changed: true, seq });
        }
        if elapsed >= timeout {
            return Ok(WaitOutcome {
                changed: false,
                seq,
            });
        }
        let step = WAIT_POLL_MS.min(timeout - elapsed);
        sleep_ms(step);
        elapsed += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn escapes_json() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    // -- set-text spec -------------------------------------------------

    #[test]
    fn set_text_spec_happy_path() {
        assert_eq!(parse_set_text_spec(r#"{"text":"hello"}"#).unwrap(), "hello");
        assert_eq!(parse_set_text_spec(r#"{"text":""}"#).unwrap(), "");
        assert_eq!(
            parse_set_text_spec("{\"text\":\"a\\nb é\"}").unwrap(),
            "a\nb é"
        );
    }

    #[test]
    fn set_text_spec_rejects_bad_input() {
        // Missing / wrong-typed text.
        assert_eq!(parse_set_text_spec("{}").unwrap_err().0, ERR_INVALID_ARG);
        assert_eq!(
            parse_set_text_spec(r#"{"text":42}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_set_text_spec(r#"{"text":null}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        // Unknown key, duplicate key, non-object, malformed.
        assert_eq!(
            parse_set_text_spec(r#"{"text":"x","extra":1}"#)
                .unwrap_err()
                .0,
            ERR_PARSE
        );
        assert_eq!(
            parse_set_text_spec(r#"{"text":"x","text":"y"}"#)
                .unwrap_err()
                .0,
            ERR_PARSE
        );
        assert_eq!(parse_set_text_spec(r#"["text"]"#).unwrap_err().0, ERR_PARSE);
        assert_eq!(parse_set_text_spec("{").unwrap_err().0, ERR_PARSE);
    }

    // -- wait-change spec ----------------------------------------------

    #[test]
    fn wait_spec_happy_path_and_defaults() {
        let s = parse_wait_change_spec(r#"{"last_seq":7,"timeout_ms":500}"#).unwrap();
        assert_eq!(
            s,
            WaitChangeSpec {
                last_seq: 7,
                timeout_ms: 500
            }
        );
        // timeout_ms is optional; the default is a single
        // immediate check.
        let s = parse_wait_change_spec(r#"{"last_seq":0}"#).unwrap();
        assert_eq!(s.timeout_ms, 0);
    }

    #[test]
    fn wait_spec_timeout_is_capped_not_rejected() {
        let s = parse_wait_change_spec(r#"{"last_seq":1,"timeout_ms":999999999}"#).unwrap();
        assert_eq!(s.timeout_ms, MAX_TIMEOUT_MS);
        let s = parse_wait_change_spec(r#"{"last_seq":1,"timeout_ms":60000}"#).unwrap();
        assert_eq!(s.timeout_ms, 60_000);
    }

    #[test]
    fn wait_spec_sequence_number_spellings() {
        // The unsigned value at the top of the range.
        let s = parse_wait_change_spec(r#"{"last_seq":4294967295}"#).unwrap();
        assert_eq!(s.last_seq, u32::MAX);
        // The same bits spelled as a C int (what
        // cp_sequence returns) round-trip.
        let s = parse_wait_change_spec(r#"{"last_seq":-1}"#).unwrap();
        assert_eq!(s.last_seq, u32::MAX);
        let s = parse_wait_change_spec(r#"{"last_seq":-2147483648}"#).unwrap();
        assert_eq!(s.last_seq, 0x8000_0000);
        // Out of range either way is an error.
        assert_eq!(
            parse_wait_change_spec(r#"{"last_seq":4294967296}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_wait_change_spec(r#"{"last_seq":-2147483649}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
    }

    #[test]
    fn wait_spec_rejects_bad_input() {
        assert_eq!(
            parse_wait_change_spec(r#"{"timeout_ms":10}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_wait_change_spec(r#"{"last_seq":"7"}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_wait_change_spec(r#"{"last_seq":1,"timeout_ms":-5}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_wait_change_spec(r#"{"last_seq":1,"timeout_ms":1.5}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_wait_change_spec(r#"{"last_seq":1,"bogus":2}"#)
                .unwrap_err()
                .0,
            ERR_PARSE
        );
        assert_eq!(parse_wait_change_spec("42").unwrap_err().0, ERR_PARSE);
    }

    // -- result shaping --------------------------------------------------

    #[test]
    fn text_result_shaping() {
        assert_eq!(text_result_json(Some("hi")), r#"{"text":"hi"}"#);
        assert_eq!(text_result_json(None), r#"{"text":null}"#);
        // Quotes, backslashes and control chars are escaped.
        assert_eq!(
            text_result_json(Some("a\"b\\c\td")),
            r#"{"text":"a\"b\\c\td"}"#
        );
    }

    #[test]
    fn files_result_shaping() {
        assert_eq!(files_result_json(&[]), r#"{"files":[]}"#);
        let files = vec![
            "C:\\Users\\sean\\a.txt".to_string(),
            "D:\\b c.txt".to_string(),
        ];
        assert_eq!(
            files_result_json(&files),
            r#"{"files":["C:\\Users\\sean\\a.txt","D:\\b c.txt"]}"#
        );
    }

    #[test]
    fn wait_result_shaping() {
        assert_eq!(
            wait_result_json(WaitOutcome {
                changed: true,
                seq: 12
            }),
            r#"{"changed":true,"seq":12}"#
        );
        assert_eq!(
            wait_result_json(WaitOutcome {
                changed: false,
                seq: 0
            }),
            r#"{"changed":false,"seq":0}"#
        );
    }

    // -- wait decision logic --------------------------------------------

    /// A scripted sequence source + recorded sleeps.
    struct FakeSeq {
        values: Vec<u32>,
        calls: usize,
        sleeps: Vec<u64>,
    }

    fn run_fake(values: Vec<u32>, last: u32, timeout: u64) -> (WaitOutcome, Vec<u64>, usize) {
        let fake = RefCell::new(FakeSeq {
            values,
            calls: 0,
            sleeps: Vec::new(),
        });
        let outcome = wait_for_change(
            || {
                let mut f = fake.borrow_mut();
                let idx = f.calls.min(f.values.len() - 1);
                f.calls += 1;
                Ok(f.values[idx])
            },
            |ms| fake.borrow_mut().sleeps.push(ms),
            last,
            timeout,
        )
        .unwrap();
        let f = fake.borrow();
        (outcome, f.sleeps.clone(), f.calls)
    }

    #[test]
    fn wait_detects_an_immediate_change_without_sleeping() {
        let (out, sleeps, calls) = run_fake(vec![9], 7, 5_000);
        assert_eq!(
            out,
            WaitOutcome {
                changed: true,
                seq: 9
            }
        );
        assert!(sleeps.is_empty());
        assert_eq!(calls, 1);
    }

    #[test]
    fn wait_detects_a_change_after_some_polls() {
        // Same number twice, then a new one: two 25 ms
        // sleeps, three reads.
        let (out, sleeps, calls) = run_fake(vec![7, 7, 8], 7, 5_000);
        assert_eq!(
            out,
            WaitOutcome {
                changed: true,
                seq: 8
            }
        );
        assert_eq!(sleeps, vec![25, 25]);
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_times_out_with_no_change() {
        // Timeout 100 = checks at 0/25/50/75/100 ms:
        // 5 reads, 4 full-interval sleeps, none past the
        // deadline.
        let (out, sleeps, calls) = run_fake(vec![7], 7, 100);
        assert_eq!(
            out,
            WaitOutcome {
                changed: false,
                seq: 7
            }
        );
        assert_eq!(sleeps, vec![25, 25, 25, 25]);
        assert_eq!(calls, 5);
    }

    #[test]
    fn wait_final_sleep_is_shortened_to_the_deadline() {
        // Timeout 30: one full 25 ms step, then a 5 ms
        // step so the wait ends at 30, not 50.
        let (out, sleeps, _) = run_fake(vec![7], 7, 30);
        assert!(!out.changed);
        assert_eq!(sleeps, vec![25, 5]);
    }

    #[test]
    fn wait_with_zero_timeout_checks_exactly_once() {
        let (out, sleeps, calls) = run_fake(vec![7], 7, 0);
        assert!(!out.changed);
        assert!(sleeps.is_empty());
        assert_eq!(calls, 1);
        // ... but an already-different number still reads
        // as changed on that single check.
        let (out, _, _) = run_fake(vec![8], 7, 0);
        assert!(out.changed);
    }

    #[test]
    fn wait_propagates_a_source_error_immediately() {
        let mut calls = 0;
        let err = wait_for_change(
            || {
                calls += 1;
                if calls < 3 {
                    Ok(7)
                } else {
                    Err((ERR_NOT_SUPPORTED, "gone".to_string()))
                }
            },
            |_| {},
            7,
            5_000,
        )
        .unwrap_err();
        assert_eq!(err.0, ERR_NOT_SUPPORTED);
        assert_eq!(calls, 3);
    }
}
