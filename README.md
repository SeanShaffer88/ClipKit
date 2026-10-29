# ClipKit

## TL;DR

One tiny zero-dependency DLL for the two clipboard jobs every Windows app ends up hand-rolling: read or set the clipboard's text, read the list of files it holds, and — the part everyone gets wrong — notice when the clipboard *changes* without gluing a hidden window and a message loop into your app. Windows keeps a counter that bumps on every clipboard change; ClipKit hands you that counter and can block until it moves, so "watch the clipboard" becomes one call instead of a Win32 ceremony.

```c
#include "clipkit.h"

int main(void) {
    int seen = cp_sequence();                    /* costs nothing */
    for (;;) {
        char *w = cp_wait_change("{\"last_seq\":0,\"timeout_ms\":60000}");
        /* ... use `seen` in the spec in real code ... */
        cp_free_string(w);                       /* {"changed":true,"seq":N} */
        char *s = cp_get_text();
        printf("%s\n", s); cp_free_string(s);    /* {"text":"..."} or {"text":null} */
        seen = cp_sequence();
    }
}
```

## Why this exists

Clipboard code is small and sharp-edged. The global-memory ownership rules are backwards from instinct (the block you hand over must *never* be freed by you — the clipboard owns it now); the clipboard is a shared resource any other app can be holding open at the moment you try; the file-list format is an `HDROP` you walk with an API from the shell; and change notification, done the documented way, wants you to create a window and register as a clipboard-format listener. So every app reinvents it, and the reinventions leak handles, free memory they no longer own, or poll by *opening* the clipboard — the one thing you must not do to a resource other apps are fighting over. ClipKit is those jobs done once, with the rules written down:

- **Text both ways.** `cp_get_text` reads `CF_UNICODETEXT` (Windows synthesizes it from `CF_TEXT`/`CF_OEMTEXT`, so plain-text clipboards just work); `cp_set_text` empties the clipboard and places text, allocating its memory block *before* opening the clipboard so an allocation failure leaves the current contents untouched — and after a successful hand-off the block belongs to the clipboard and is never freed by the library.
- **File lists.** `cp_get_files` walks the `CF_HDROP` with `DragQueryFileW` and returns the absolute paths in stored order.
- **Change detection with no window.** `GetClipboardSequenceNumber` is an unsigned counter Windows bumps on every change. `cp_sequence` returns it (reading it opens nothing — this is the cheap poll); `cp_wait_change` polls it every 25 ms on your thread until it differs from the number you last saw or your timeout (capped at 60 s) passes. No listener window, no message loop, no callback thread to join on shutdown.
- **Contention handled.** Every clipboard open retries up to 10 times, 10 ms apart — another app holding the clipboard open is a normal Tuesday, not an error — and every successful open is closed on every path out.

## The API in one screen

| Area | Calls |
|---|---|
| Text | `cp_get_text` → `{"text": "..."}` or `{"text": null}`; `cp_set_text({"text": "..."})` → `CP_OK` |
| Files | `cp_get_files` → `{"files": ["C:\\a.txt", ...]}` (empty array when none) |
| Changes | `cp_sequence` → the counter as a C int; `cp_wait_change({"last_seq": N, "timeout_ms": M})` → `{"changed": bool, "seq": N}` |
| Errors | Integer `CP_*` codes + thread-local `cp_last_error()` |

A missing format is not an error: no text reads as `{"text": null}`, no file list reads as `{"files": []}`. Errors mean the clipboard itself could not be reached. Specs are single JSON objects; unknown and duplicate keys are rejected (`CP_ERR_PARSE`).

**Sequence numbers are compared, never ordered.** The counter is unsigned 32-bit; `cp_sequence` returns its bits reinterpreted as a C `int`, so a very large counter reads negative — that is the same value, not an error. `cp_wait_change`'s `"last_seq"` accepts either spelling of the same bits, so the pair round-trips across the wrap. Only "same number / different number" carries meaning.

## Honest caveats

- **Polling, not push.** `cp_wait_change` notices a change within 25 ms of it happening, not at the instant. That is the price of needing no window; for the workloads this serves (paste watchers, clipboard managers, "the user copied something" triggers) it is far below human perception, and the wait thread is yours — it starts when you call and ends when the call returns.
- **`cp_wait_change` blocks its caller** for up to the timeout. Call it from a worker thread if your thread has other duties.
- **`cp_sequence` failure and a zero counter look alike.** A failed read returns 0 with the last-error set; a genuine counter of 0 is possible in a session that has not touched the clipboard (or lacks a window station). Check `cp_last_error_code()` when the distinction matters.
- **Text only synthesizes one way reliably.** Reading benefits from Windows' format synthesis (text formats convert between each other); a clipboard holding *only* an image or rich content reads as `{"text": null}` even if some app could render it as text. HTML/RTF/image formats are roadmap, not v0.1.
- **Contents can be huge.** The kit returns whatever text the clipboard holds; a host copying a 200 MB text blob gets a 200 MB string. Bounding that is the caller's policy call, not silently the library's.

## Verification status

25 tests on the host: spec parsing for both specs (required keys, wrong types, unknown and duplicate keys, the timeout cap applied as a cap rather than an error, and both spellings of a sequence number — including the signed round-trip of the top bit), result JSON shaping (null text, escaped quotes/backslashes, Windows paths in file lists), the wait-for-change decision loop driven against a scripted sequence source (immediate change, change after polls with exact 25 ms steps, timeout with the final sleep shortened to the deadline, zero-timeout single check, source errors propagating at once), and the FFI surface off Windows (clean `CP_ERR_NOT_SUPPORTED` from every clipboard call, argument validation happening before any platform call). A Windows-only integration test (set text → sequence moves → get text round-trips → wait sees the move, then reports a quiet clipboard; it skips gracefully when no clipboard exists in the session) runs on Windows CI. The full WinDevKit gate (fmt, clippy on host and `x86_64-pc-windows-msvc`, check on the Windows target, whole suite) is green locally. **The Windows clipboard runtime path itself is Unverified**: it compiles and type-checks for Windows here, but no Windows machine has run it yet outside CI — an on-machine field pass is the remaining proof, and Windows CI's real MSVC build remains the link gate.

Building: `cargo build --release` → `clip_kit.dll` / `libclip_kit.so`; `cargo test --locked`.

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what's next.
