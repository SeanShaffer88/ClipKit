# ClipKit Roadmap

## v0.1.0 (current)
Clipboard text and file lists plus change detection behind a C ABI: `cp_get_text` / `cp_set_text` over CF_UNICODETEXT (with the ownership-transfer and allocate-before-open rules), `cp_get_files` over CF_HDROP via DragQueryFileW, and the sequence-number pair — `cp_sequence` (the cheap poll that opens nothing) and `cp_wait_change` (25 ms polling loop, timeout capped at 60 s, both signed and unsigned spellings of the counter accepted). Every open retries against contention and closes on every path. The wait decision loop is pure and tested against an injected sequence source; the Windows runtime path is Unverified until a field test on a real Windows machine.

## Next, in order
1. **More formats**: read (and where sensible, write) the formats real clipboards carry beyond text and files — HTML (CF_HTML, with its byte-offset header parsed properly), RTF, and bitmap/DIB as PNG bytes — behind the same "missing format is a successful empty result" rule, each as its own call rather than a format-negotiation mini-language.
2. **Listener-window change events**: an optional handle-based watcher using a message-only window and `AddClipboardFormatListener`, delivering the same `{"changed", "seq"}` outcomes as events for hosts that want push instead of polling. Same contracts, second engine — the polling pair stays the portable baseline; the listener is Windows-only and reports honestly when it cannot start.
3. **Format inventory**: a `cp_list_formats` call naming every format currently on the clipboard (standard names where known), so a host can decide which read to attempt instead of probing format by format.

## Explicitly not planned
- Clipboard history (WinRT `Clipboard.GetHistoryItemsAsync`): it needs an app identity and async WinRT plumbing that do not fit a stateless C ABI DLL, and it reads *other* moments of the user's clipboard — a different privacy shape from "the current contents the user just put there", which is this kit's whole scope.
- Delayed rendering (promising a format and producing it on paste): the kit is a consumer/producer of current contents, not a clipboard *owner* with a lifetime; owning formats across process exit is an application job.
- Monitoring other applications' clipboard *content*: ClipKit detects that a change happened and reads contents when its caller asks. Turning that into a content log of everything a user copies is a product decision for a host app, not a capability to bake into the shared library.
