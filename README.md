# wayland-mcp

`wayland-mcp` is a Linux MCP server for observing and driving native Wayland
clients. It runs a per-process Wayland proxy, tracks client surfaces, captures
shared-memory and DMA-BUF buffers, and injects compositor-side pointer and
keyboard events.

The server exposes one MCP tool, `gui_console`. The tool evaluates JavaScript
in a persistent, isolated Node.js process. Convenience functions cover routine
GUI interactions, while the raw Wayland event interface remains available for
protocol-level testing.

## Requirements

Build requirements:

- Linux;
- Rust 1.88 or newer, including Cargo and a native linker;
- libxkbcommon development files providing the `xkbcommon` linker library;
- Vulkan loader development files providing the `vulkan` linker library.

Runtime requirements:

- Node.js 18 or newer available as `node` on `PATH`;
- an active Wayland session with `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` set;
- a Vulkan 1.1 loader and a compatible graphics driver for DMA-BUF capture;
- permission to connect to the compositor socket and, for GPU clients, the
  applicable `/dev/dri/renderD*` device.

Distribution package names vary. Common packages are `libxkbcommon-dev` and
`libvulkan-dev` on Debian-family systems, `libxkbcommon-devel` and
`vulkan-loader-devel` on Fedora, and `libxkbcommon`, `vulkan-headers`, and
`vulkan-loader` on Arch Linux. The project does not use absolute paths to
compilers, runtimes, or locally installed dependencies.

## Build and validation

Cargo.lock is committed, so reproducible local builds should use `--locked`:

```sh
cargo build --locked --release
```

The release executable is `target/release/wayland-mcp`. To run formatting,
linting, unit tests, and a release build together:

```sh
./scripts/validate.sh
```

The validation script honors Cargo's standard environment variables, including
`CARGO_TARGET_DIR`, which is useful for verifying a fresh build outside an
existing target directory.

## MCP configuration

Configure an MCP client to start the release executable over standard input and
output. The server must inherit the Wayland session environment. For example:

```json
{
  "mcpServers": {
    "wayland": {
      "command": "/absolute/path/to/wayland-mcp"
    }
  }
}
```

The exact configuration format is client-specific. Standard output is reserved
for MCP transport; diagnostic logging is written to standard error.

## Console API

Start by asking the console for its built-in API reference:

```js
return wayland.help;
```

The main observation calls are:

- `wayland.environment()` — environment variables, absolute socket path, and
  launch preflight state for clients that should connect through the proxy;
- `wayland.diagnostics()` — proxy, compositor, session, and error state;
- `wayland.windows()` — mapped surface inventory and capture metadata;
- `wayland.screenshot({windowId})` — capture the current frame;
- `wayland.captureNextFrame({windowId, afterCommitSerial, timeoutMs})` — wait
  for and capture a later committed frame.

High-level input and synchronization helpers include `waitForWindow`,
`waitForWindowGone`, `click`, `doubleClick`, `move`, `drag`, `scroll`,
`pressKey`, `pressShortcut`, `typeText`, `waitForCommit`, `actAndCapture`, and
`resetInputState`. `pointerEvent` and `keyboardEvent` provide direct access to
individual protocol events.

`resizeWindow({windowId,width,height})` sends an `xdg_toplevel.configure`
size suggestion to a mapped window. Check `windows()` or a later screenshot
to confirm the client applied it; Wayland clients may choose a different size.

`pressKey` accepts an evdev code, a common name
such as `"Escape"`, or a one-character key such as `"W"` (case-insensitive).
The complete named-key vocabulary is available as `wayland.keyNames`.
`pressKey`, `typeText`, and character keys in `pressShortcut`
derive their key codes and serialized modifier masks from the exact XKB keymap
forwarded to the target client and honor its active layout group. Raw events
remain available for protocol-level keyboard testing. Raw input calls invalidate
cached helper focus automatically. `resetInputState()` clears cached focus after
external focus changes; it does not release pressed keys.

Clipboard selection claims made with injected input serials are consumed by
the proxy, so the host compositor cannot reject the synthetic serial. A client
that retains its own copied data can test its in-app copy/paste flow. The proxy
does not synthesize a new selection offer or virtualize cross-client clipboard
exchange; clients that rely on receiving an offer to paste may still see the
host clipboard.

For applications supporting Ctrl+Shift+U Unicode entry, use
`wayland.typeText({windowId, text:"🌘", inputMethod:"unicode-hex"})`.
This opt-in method enters each Unicode code point through the application's hex
input convention, without using the clipboard. It was exercised in Chromium;
applications without that convention must use their own input method.

Window inventory separates the two output domains. `on_capture_output` and
`capture_output_count` describe membership on the MCP's single virtual capture
output; a mapped, capturable window reports `true` and `1`.
`on_backend_output` and `backend_output_count` report only host-compositor
`wl_surface.enter`/`leave` membership and may remain false/zero while capture
works.

JavaScript state survives calls. Define reusable functions on `globalThis` when
an interaction needs custom timing or event sequencing:

```js
globalThis.clickCenter = async function (windowId, width, height) {
  return wayland.click({
    windowId,
    x: width / 2,
    y: height / 2,
    coordinateSpace: "full"
  });
};
return "ready";
```

Coordinates default to full screenshot pixels. Screenshot responses include
full and embedded-preview dimensions plus the conversion scale. A helper call
may instead specify `coordinateSpace: "preview"` together with the returned
`preview_to_full_scale` value; the helpers accept that response spelling as
well as `previewToFullScale`.

Screenshots convert committed Wayland color descriptions to 8-bit SDR sRGB
PNG pixels. FP16 and 10-bit values are converted before quantization; the
pipeline decodes the transfer function, applies the declared luminance scale,
converts primaries, maps HDR luminance with Reinhard `Y/(1+Y)` relative to
source reference white, compresses the gamut toward neutral, and encodes sRGB.
This is a deterministic SDR preview policy, not a reproduction of the host
display's HDR appearance or its tone mapper. HDR reference white maps to
linear sRGB 0.5. The player's own HDR10+ scene processing remains in its pixels.

Each converted capture reports a `color` object in both the screenshot result
and the tool's `images` metadata, including a model-facing notice when the
image is tone mapped. This notice remains present even if JavaScript discards
the screenshot return value. It states that original HDR brightness, gamut,
and highlight appearance cannot be judged from the SDR preview. The metadata
is also retained in the full PNG alongside an sRGB declaration.

All 10 named primaries in `wp_color_manager_v1` are supported: sRGB/BT.709,
PAL-M, PAL, NTSC, generic film, BT.2020, CIE 1931 XYZ, DCI P3, Display P3,
and Adobe RGB. Conversion adapts source white to D65 with Bradford adaptation.
All 14 named transfer functions are supported, including ST 240, both log
encodings, xvYCC, the deprecated sRGB names, ST 428, HLG, and compound power 2.4.
HLG includes its luminance-coupled display OOTF and black-level compensation.
The log encodings use the inverse encoding curve (zero maps to the lowest
representable nonzero level). Windows-scRGB is also supported. Unknown enum
values, untracked descriptions, and ICC profiles fail capture explicitly.
Custom primary chromaticities and white points are preserved and converted with
an RGB-to-XYZ matrix and Bradford adaptation computed once per description.
This includes imaginary primaries and zero-y primaries such as CIE XYZ; singular
matrices and invalid white points produce explicit errors, never an sRGB fallback.
Custom power transfer functions support every protocol exponent from 1.0000 to
10.0000, including sign-preserving negative and above-one channel values.
Capture metadata retains the original chromaticities and power exponent.
Untagged surfaces retain the existing assumed-sRGB path. Color state follows
surface commit and image-description copy semantics.

OpenAI's [image-input documentation](https://developers.openai.com/api/docs/guides/images-vision)
does not specify an HDR tone mapper or an ICC/HDR processing contract. The SDR
output policy above is this tool's compatibility choice, not a documented
OpenAI tone-mapping requirement.

Input delivery and application behavior are separate observations. A successful
input call means that the protocol event was emitted; capture a later commit to
verify the application's response.

For the common action/wait/capture sequence, `actAndCapture` reports delivery
and observation separately:

```js
return await wayland.actAndCapture({
  windowId,
  afterCommitSerial: baseline.commit_serial,
  action: () => wayland.click({windowId, x: 320, y: 48}),
  timeoutMs: 2000
});
```

Its result includes `actionResult`, both commit serials, `frameObserved`,
`surfaceDisappeared`, elapsed time, capture metadata, and any capture error. It
does not claim that the resulting pixels satisfy an application-level
assertion.

## Client launch workflow

1. Start the MCP server in the host Wayland session.
2. Call `wayland.environment()`.
3. Launch the target client with the returned `XDG_RUNTIME_DIR` and
   `WAYLAND_DISPLAY` values.
4. Use `waitForWindow` or `windows` to identify the mapped surface.
5. Capture a baseline, perform input, then wait for a later commit.
6. Terminate the client and confirm that its window disappears.

`environment()` validates the listener and socket before returning. Alongside
`WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`, it returns `socket_path` and a
`launch_preflight` object. `endpoint_state: "ready"` proves that the endpoint is
a Unix socket and the MCP accept loop is running. Because the MCP cannot inspect
the namespace or device policy of a process launched by its caller,
`caller_namespace_access` and `render_node_access` explicitly remain
`"not_tested"`; the caller should test or grant those narrow paths before
classifying `wl_display_connect` as an application failure. If an earlier proxy
endpoint stopped or its private runtime directory disappeared, the call creates
a fresh endpoint; callers must therefore use the values from the latest call
rather than caching them across MCP instances.

`diagnostics()` repeats the socket/preflight state and retains the 32 most
recent connection events with timestamps, client IDs, endpoint state, and close
details. Normal EOF, connection reset, and broken-pipe client teardown are kept
in that history without replacing the service-wide `last_runtime_error`.

The proxy does not launch applications or grant filesystem, socket, or graphics
device permissions. Those remain the responsibility of the invoking process.
Host-owned system dialogs may connect to another compositor and therefore fall
outside the proxy's observable surface set.

## Configuration

| Variable | Purpose | Default |
| --- | --- | --- |
| `WAYLAND_MCP_ARTIFACT_DIR` | Directory for full PNG captures and JSONL event records | A private, unique directory under the system temporary directory |
| `WAYLAND_MCP_EVAL_TIMEOUT_MS` | JavaScript evaluation timeout in milliseconds | 120000; values are clamped to the supported range |
| `WAYLAND_MCP_SOCKET` | Proxy socket name inside its private runtime directory | `wayland-mcp-0` |
| `WAYLAND_MCP_BACKEND_SOCKET` | Host compositor socket name or absolute socket path | The inherited `WAYLAND_DISPLAY`, otherwise `wayland-0` |
| `WAYLAND_MCP_TRACE` | Enable verbose proxy tracing on standard error | Unset |

Artifacts can contain captured application content and input metadata. Store
them in an appropriately protected location and apply the retention policy of
the environment in which the server runs.

## Limitations

- The proxy only advertises Wayland interfaces whose wire metadata it can
  safely decode and forward.
- DMA-BUF capture depends on Vulkan external-memory and DRM-format-modifier
  support in the host driver.
- The compositor boundary does not provide a semantic widget or accessibility
  tree; assertions are based on frames, surface metadata, and application
  commits.
- `typeText` supports characters directly represented in the target client's
  active XKB layout by default. The opt-in `unicode-hex` method requires
  application support for Ctrl+Shift+U; other compose or input-method-mediated
  text requires an input-method companion.

## License

Apache-2.0. See [LICENSE](LICENSE).
