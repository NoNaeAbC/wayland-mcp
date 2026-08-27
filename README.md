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
- Vulkan loader development files providing the `vulkan` linker library.

Runtime requirements:

- Node.js 18 or newer available as `node` on `PATH`;
- an active Wayland session with `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` set;
- a Vulkan 1.1 loader and a compatible graphics driver for DMA-BUF capture;
- permission to connect to the compositor socket and, for GPU clients, the
  applicable `/dev/dri/renderD*` device.

Distribution package names vary. Common Vulkan loader packages are
`libvulkan-dev` on Debian-family systems, `vulkan-loader-devel` on Fedora, and
`vulkan-headers` plus `vulkan-loader` on Arch Linux. The project does not use
absolute paths to compilers, runtimes, or locally installed dependencies.

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

- `wayland.environment()` — environment variables for clients that should
  connect through the proxy;
- `wayland.diagnostics()` — proxy, compositor, session, and error state;
- `wayland.windows()` — mapped surface inventory and capture metadata;
- `wayland.screenshot({windowId})` — capture the current frame;
- `wayland.captureNextFrame({windowId, afterCommitSerial, timeoutMs})` — wait
  for and capture a later committed frame.

High-level input and synchronization helpers include `waitForWindow`, `click`,
`doubleClick`, `move`, `drag`, `scroll`, `pressKey`, `waitForCommit`, and
`resetInputState`. `pointerEvent` and `keyboardEvent` provide direct access to
individual protocol events.

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

Input delivery and application behavior are separate observations. A successful
input call means that the protocol event was emitted; capture a later commit to
verify the application's response.

## Client launch workflow

1. Start the MCP server in the host Wayland session.
2. Call `wayland.environment()`.
3. Launch the target client with the returned `XDG_RUNTIME_DIR` and
   `WAYLAND_DISPLAY` values.
4. Use `waitForWindow` or `windows` to identify the mapped surface.
5. Capture a baseline, perform input, then wait for a later commit.
6. Terminate the client and confirm that its window disappears.

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
- Keyboard input uses Linux evdev key codes. Text input and input-method
  composition are not synthesized automatically.

## License

Apache-2.0. See [LICENSE](LICENSE).
