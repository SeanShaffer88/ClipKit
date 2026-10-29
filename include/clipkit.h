/* ClipKit.dll — clipboard text and file-list access
 * plus change detection behind one small C ABI.
 *
 *   unsigned start = (unsigned)cp_sequence();
 *   cp_set_text("{\"text\":\"hello\"}");
 *   char *s = cp_get_text();          // {"text":"hello"}
 *   printf("%s\n", s); cp_free_string(s);
 *
 * WHAT IT DOES. Three stateless calls against the
 * Windows clipboard — read its text (CF_UNICODETEXT),
 * replace its text, read its dropped-file list
 * (CF_HDROP) — and one change signal: the clipboard
 * SEQUENCE NUMBER, an unsigned counter Windows bumps
 * on every clipboard change (GetClipboardSequenceNumber).
 * Poll the number (cp_sequence — it opens nothing and
 * needs no window) or block until it moves
 * (cp_wait_change), and only open the clipboard when
 * something actually changed.
 *
 * OWNERSHIP AND ERRORS. Every char* result is
 * caller-owned UTF-8, NUL-terminated: free it with
 * cp_free_string(). A failed string-returning call
 * returns NULL and sets a thread-local last-error
 * (cp_last_error() / cp_last_error_code()). cp_set_text
 * returns a CP_* code directly. Nothing here keeps
 * state, so every export may be called from any thread.
 *
 * A MISSING FORMAT IS NOT AN ERROR. When the clipboard
 * holds no text, cp_get_text succeeds with
 * {"text": null}; when it holds no file list,
 * cp_get_files succeeds with {"files": []}. Errors
 * mean the clipboard itself could not be reached.
 *
 * THE CLIPBOARD IS SHARED. Any application may hold it
 * open at any moment, so every operation retries the
 * open up to 10 times, 10 ms apart, before failing
 * with CP_ERR_ACCESS_DENIED. Every successful open is
 * closed again before the call returns.
 *
 * SEQUENCE NUMBERS ARE COMPARED, NEVER ORDERED. The
 * counter is an unsigned 32-bit value; cp_sequence
 * returns its bits reinterpreted as a C int (so a very
 * large counter reads negative — that is the same
 * value, not an error). cp_wait_change's "last_seq"
 * accepts either spelling of the same bits, so the
 * pair round-trips. Only equality/inequality of the
 * number carries meaning.
 *
 * PLATFORMS. The clipboard calls are Windows-only; on
 * any other platform they fail cleanly with
 * CP_ERR_NOT_SUPPORTED. (The DLL itself builds
 * anywhere, which is what the test suite runs on.)
 *
 * SPECS. Input specs are single JSON objects. Unknown
 * keys are rejected (CP_ERR_PARSE), as are duplicate
 * keys — a spec with two values for one key is
 * ambiguous, and silently picking one is how bugs hide.
 */
#ifndef CLIPKIT_H
#define CLIPKIT_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
#if defined(_WIN32) && defined(CP_BUILD)
#define CP_API __declspec(dllexport)
#elif defined(_WIN32)
#define CP_API __declspec(dllimport)
#else
#define CP_API
#endif
#define CP_OK 0
#define CP_ERR_INVALID_ARG 1
#define CP_ERR_NOT_SUPPORTED 2
#define CP_ERR_NOT_FOUND 3
#define CP_ERR_ACCESS_DENIED 4
#define CP_ERR_PLATFORM 5
#define CP_ERR_INTERNAL 6
#define CP_ERR_PARSE 7

/* The library version string, e.g. "0.1.0". Static
 * storage — do NOT free. */
CP_API const char *cp_version(void);

/* Thread-local message describing the most recent
 * failure on this thread ("" when none). Static
 * storage — do NOT free. */
CP_API const char *cp_last_error(void);

/* The CP_ERR_* code matching cp_last_error(), or
 * CP_OK when the last call on this thread succeeded. */
CP_API int cp_last_error_code(void);

/* Free a string this library returned. NULL is a
 * no-op. Freeing anything else, or freeing twice, is
 * a contract violation. */
CP_API void cp_free_string(char *p);

/* Read the clipboard's text.
 * Returns {"text": "..."} — or {"text": null} when the
 * clipboard holds no text format. The text is the
 * CF_UNICODETEXT content; Windows synthesizes that
 * format from CF_TEXT/CF_OEMTEXT, so plain-text
 * clipboards read correctly either way. NULL on
 * failure (see cp_last_error()). */
CP_API char *cp_get_text(void);

/* Replace the clipboard's contents with text.
 * spec: {"text": "..."} — the string is required
 * (empty sets an empty text clipboard). The call
 * empties the clipboard first, so afterwards the text
 * is all it holds. Returns CP_OK or a CP_ERR_* code.
 *
 * The memory handed to the clipboard is allocated
 * before the clipboard is opened, so an allocation
 * failure leaves the current clipboard contents
 * untouched. */
CP_API int cp_set_text(const char *spec_json);

/* Read the clipboard's dropped-file list (CF_HDROP):
 * {"files": ["C:\\a.txt", ...]} — absolute paths, in
 * the order the source application stored them, or an
 * empty array when the clipboard holds no file list.
 * NULL on failure (see cp_last_error()). */
CP_API char *cp_get_files(void);

/* The current clipboard sequence number, bits
 * reinterpreted as a C int (see the note at the top:
 * compare for equality only). Reading it opens
 * nothing and needs no window — this is the cheap
 * "has anything changed?" poll. On failure returns 0
 * and sets the last-error; 0 is also a legitimate (if
 * rare) counter value, so check cp_last_error_code()
 * when the distinction matters. */
CP_API int cp_sequence(void);

/* Block until the clipboard changes or a timeout
 * passes. spec: {"last_seq": N, "timeout_ms": M} —
 * "last_seq" is required (the number you last saw;
 * either the unsigned value or cp_sequence's signed
 * spelling of the same bits), "timeout_ms" is
 * optional (default 0 = one immediate check) and is
 * capped at 60000. The sequence is re-checked every
 * 25 ms; a change is reported within one interval of
 * happening.
 * Returns {"changed": true, "seq": N} with the new
 * current number, or {"changed": false, "seq": N}
 * after the timeout. This call BLOCKS the calling
 * thread for up to the timeout. NULL on failure (see
 * cp_last_error()). */
CP_API char *cp_wait_change(const char *spec_json);

#ifdef __cplusplus
}
#endif
#endif
