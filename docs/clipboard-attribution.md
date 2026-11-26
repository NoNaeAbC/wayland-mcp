# Clipboard routing

Rust tracks the latest trusted input source per client and seat. Physical
pointer, keyboard and touch input sets it to human. Model injection sets it to
model, including playback of human recordings. Before any trusted input or
focus, the source defaults to model.

Binding a data device does not send a selection event. Its first trusted input
or focus establishes readiness and sends the selected actor's current selection,
including when the actor remains the default model. This prevents clipboard
callbacks during toolkit initialization and keeps unfocused helper connections
from receiving unsolicited selection changes. Host focus initializes the source
to human when there has been no input. Later focus changes preserve the source of
the last input.

Human clipboard operations use the host-shared selection. Model clipboard
operations use the independent private selection. Switching actors preserves
both selections; the private selection is never seeded from the host.

Each operation uses the current source, including delayed requests and requests
using older offer IDs. Overlap requires no detection, reporting, warnings,
rejection, transfer UI or special handling. Client-supplied actor labels cannot
change the source.

## JavaScript clipboard access

```js
await wayland.clipboard.write({
  windowId,
  formats: { "text/html": "<p><strong>Ready</strong> for review.</p>",
             "text/plain": "Ready for review." },
});
await wayland.pressShortcut({ windowId, keys: ["CTRL", "v"] });
```

`wayland.clipboard.types({windowId})` lists the current MIME types in the
window's private model selection. Use `read({windowId, mimeType})` for bytes
or `readText({windowId, mimeType})` for UTF-8. Both return `null` when the
requested format is absent. This works for selections copied by applications
through model input as well as selections published by JS.

`write` replaces the selection with the supplied MIME types. Values can be
strings or `Uint8Array` objects. An empty format map clears the selection.
Console reads and writes are limited to 1 MiB. For a window with multiple
Wayland seats, pass the `seatId` reported by `wayland.inputCapabilities`.
