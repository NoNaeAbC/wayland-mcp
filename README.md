# wayland-mcp

`wayland-mcp` is a Linux MCP server for observing and driving native Wayland
clients. It runs a per-process Wayland proxy, tracks client surfaces, evaluates GPU-resident visual predicates on DMA-BUFs,
and injects compositor-side pointer and
keyboard events.

The server exposes one MCP tool, `gui_console`. The tool evaluates JavaScript
in a persistent Node.js process with Node I/O permissions disabled. Convenience
functions cover routine
GUI interactions, while the raw Wayland event interface remains available for
protocol-level testing.

## Requirements

Build requirements:

- Linux;
- Rust 1.88 or newer, including Cargo and a native linker;
- Python 3.10 or newer (`python3`, or the executable specified by `PYTHON`);
- installed Wayland core and `wayland-protocols` XML files under `/usr/share`;
- libxkbcommon development files providing the `xkbcommon` linker library;
- Vulkan loader development files providing the `vulkan` linker library;
- libva development files with DRM support, libdrm headers, and OpenSSL development files;
- GBM and Zstandard development files providing the `gbm` and `zstd` linker libraries;
- a C++26 compiler (GCC or Clang) and `ar`;
  project C++ is built with `-fno-rtti -fno-exceptions` and strict warnings;
  set `CXX` to select the compiler;
- Shaderc development headers (the `shaderc` package on Arch Linux);
- CMake and a native build tool (Ninja or Make) to build the bundled Shaderc,
  glslang and SPIRV-Tools sources. Python is also used by that build.

Runtime requirements:

- Node.js 26 or newer available as `node` on `PATH`;
- an active Wayland session with `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` set;
- a Vulkan 1.1 loader and a compatible graphics driver for DMA-BUF capture;
- the OpenSSH client (`ssh`) on the launcher host for SSH workflows;
- the libva, libva-drm, OpenSSL libcrypto, GBM, and Zstandard shared libraries;
- permission to connect to the compositor socket and, for GPU clients, the
  applicable `/dev/dri/renderD*` device.

Distribution package names vary. Common packages are `libxkbcommon-dev` and
`libvulkan-dev` on Debian-family systems, `libxkbcommon-devel` and
`vulkan-loader-devel` on Fedora, and `libxkbcommon`, `vulkan-headers`, and
`vulkan-loader` on Arch Linux. On Arch, install `shaderc`, `cmake`, and `ninja`
alongside the other build dependencies. Compiler and runtime commands are
resolved on `PATH`. The first build compiles and statically links the compiler
dependencies pinned in `Cargo.lock`.
The project limits Cargo's default build concurrency to two jobs to keep the
native compiler build within a reasonable memory budget.

## Wayland over SSH

Connect from the `gui_console` you already use for local apps. Enter the SSH
login for the host that will run the app.

```js
return await wayland.connectSsh({host: "user@compute.example"});
```

The result says `connected: true` after a Wayland check makes the round trip
over SSH. Copy the path from `environment.WAYLAND_DISPLAY` and use it when
starting the app from a terminal. For example, replace the path below with
the one you received.

```sh
ssh -t user@compute.example 'env -u WAYLAND_SOCKET WAYLAND_DISPLAY=/tmp/wmc-app-EXAMPLE/wayland vkcube --wsi wayland'
```

While the app is running, use `return await wayland.windows()` in the same
console. The usual capture and input commands work on the forwarded window.
This MCP handles both capture and input. Use `wayland.disconnectSsh()` to end
the connection. Connecting to a new host closes the old connection after the
new one is ready.

```text
Wayland server ↔ Wayland MCP (capture and input) ↔ SSH ↔ Wayland MCP ↔ Wayland client
```

### Start the bridge from a terminal

Run `--ssh-wayland` from a terminal on the desktop where you want the windows
to appear. Your local compositor places the windows and handles physical input.
To run a standalone remote MCP for testing, see
[MCP and capture over SSH](#mcp-and-capture-over-ssh).

Enable Unix-socket forwarding on the SSH server. The launcher reuses a private
copy of its exact build on the SSH host and uploads the running executable when
the cached build differs or the cache is unavailable. The SSH host needs a
compatible architecture and runtime libraries.

### Run the agent on the SSH host

```sh
target/release/wayland-mcp --ssh-wayland user@compute.example -- bash
```

Start your agent in that shell and configure its Wayland MCP server to execute
`wayland-mcp`. The launcher adds the executable to `PATH` and exports its
absolute path as `WAYLAND_MCP_EXECUTABLE`. The shell also includes
`WAYLAND_MCP_BACKEND_SOCKET` for the bridge and `WAYLAND_MCP_SSH_VIDEO_SOCKET`
for codec settings. Pass these variables through if your MCP client restricts
the server's environment. Launch tracked apps using the environment returned
by the agent's `wayland.environment()`. Running an ordinary Wayland app
directly from the shell also displays it on the local desktop.

### Run the agent on the local desktop

Start the local Wayland MCP server in your desktop session. Use the absolute
`socket_path` from its `wayland.environment()` as `--local-display`.

```sh
target/release/wayland-mcp --ssh-wayland user@compute.example \
  --local-display "$MCP_SOCKET" \
  --local-control "$XDG_RUNTIME_DIR/wayland-mcp-ssh-video" -- bash
```

Here `MCP_SOCKET` is the returned absolute path. Launch apps from the SSH shell.
To enable codec changes, give the local MCP server the `--local-control` path
as `WAYLAND_MCP_SSH_VIDEO_SOCKET`.

### Buffers and video options

Wayland requests and events travel in order over SSH. Each end creates the
descriptors it needs. Keyboard keymaps use Zstandard. Supported RGB DMA-BUFs
and RGB SHM buffers travel as H.265. The bridge passes raw pixels between
GPU surfaces and sends only compressed video over SSH. SHM pools use
memfds sealed against shrinking and require Vulkan host-memory import.
The bridge reuses up to five display slots per buffer. If they are all in
use, it waits for the compositor to release one. It waits for decode and
conversion before forwarding the attachment and commit.

Both endpoints need hardware VA-API HEVC and Vulkan compute. The bridge
checks the remote GPUs against the display host's decoder capabilities before
launching the application. It prefers the remote boot graphics device for
Wayland DMA-BUF feedback. For each DMA-BUF, it prefers the producer's GPU
when the exporter identifies it and it can encode a common profile. Another
GPU can import the original buffer or receive a GPU copy through a reusable
linear DMA-BUF when the original layout cannot be shared. The bridge prefers
the source's integer bit depth and a common 4:4:4 profile. For 10-bit sources it
tries SCC when the driver supports it in the selected rate-control mode. Use `--remote-gpu`
for capture and encoding and `--display-gpu` for local DMA-BUF allocation
on multi-GPU systems. Both take DRM render-node paths. OpenSSH handles
configuration and host keys. Use `--ssh-config` to select a configuration
file.

```js
return await wayland.configureSshVideo({
  toneMap: "knee",
  rateControl: {mode: "auto"},
});
```

This sets the session's options for newly encoded buffers. Omitted options
use their defaults. The options include
`encodeGpu`, `decodeGpu`, `captureGpu`, `chroma`, `bitDepth`,
`screenContent`, `rateControl`, `toneMap`, `toneCurve`, `exposure`,
`background`, and `sourceColor`. GPU paths refer to the host that performs
the operation.

The default `rateControl` mode is `"auto"`. It tries the driver's adaptive
constant-quality mode, then VBR. When picture transfers consistently stall on
the SSH link, auto mode can switch that surface's color stream to VBR with a
target based on the observed transfer rate. Set `mode` to `"constantQuality"`
to select `quality` from 1 to 51. Lower values request higher quality and 24 is
the default. With `"vbr"`, `targetBitrate` sets the target in bits per second
and defaults to a value based on the buffer size. Both explicit modes accept
a nominal `frameRate`, which defaults to 120.

Each Wayland surface has its own color and alpha reference state. On profiles
with driver support for inter-frame coding, each picture can reference the
preceding encoded picture of its stream. Resizes, codec changes, and periodic
refreshes produce an IDR. Commits from other surfaces can interleave with that
sequence. Each VA encoder reuses a pool of five input surfaces.

For opaque sRGB DMA-BUFs, the driver can convert directly between RGB and
encoder or decoder surfaces using VA video processing. The Vulkan shader
handles other supported layouts. The bridge reads the producer's Wayland
parametric color description and signals full-range BT.709 YCbCr with sRGB
transfer. Tone mapping applies to FP16 RGB input. The compositor receives
the converted SDR buffer, including 10-bit RGB for 10-bit RGB DMA-BUF sources.

Untagged buffers default to sRGB. Use `sourceColor` to supply their color
description, such as `{transfer: "pq", primaries: "bt2020"}` for a PQ source.
A color change takes effect on the next attached buffer. ICC-tagged buffers
and compositor-provided source descriptions are currently unsupported.

The bridge preserves transparency by default. For buffers with nonopaque
pixels, a separate 8-bit 4:2:0 HEVC stream stores alpha in luma. This can
round opacity values. When the GPU finds identical opacity samples and the
codec settings match, it reuses the receiver's alpha surface. X formats
carry no alpha stream. The GPU produces premultiplied sRGB for the
compositor. Set `background` to `"black"` or `"checkerboard"` to composite
on the GPU.
The bridge supports implicit DMA-BUF synchronization, explicit DRM timelines
on the application host, and keyboard and pointer events. The bridge forwards
`wp_presentation` feedback from the display compositor. On separate hosts or in
separate time namespaces, it translates presentation timestamps to the
application's `CLOCK_MONOTONIC` using round-trip clock samples. Network delay
asymmetry affects the accuracy of translated timestamps.
The bridge exposes the application-side data-device objects GTK needs to
initialize keyboard input. Clipboard and drag data stay on their own host.

Set `WAYLAND_MCP_TRACE_TIMING=1` on the SSH Wayland launcher to log buffer
acquisition, GPU conversion, codec work, and publication times on both hosts.
Vulkan timings separate command finalization, submission, and fence wait.

## MCP and capture over SSH

The Wayland proxy runs on the desktop host and owns the Wayland socket, application
buffers, and encode GPU. Launch applications on that host with the
`WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` returned by `wayland.environment()`.

### Start a remote MCP for terminal testing or client configuration

In a terminal on the SSH host's desktop, read its active socket name with
`printenv WAYLAND_DISPLAY`. Use that socket name and the OpenSSH destination
in your local MCP client's stdio server configuration.

```json
{
  "mcpServers": {
    "wayland": {
      "command": "/absolute/path/to/wayland-mcp",
      "args": ["--ssh", "user@desktop.example", "--remote-display", "wayland-0", "--local-js"]
    }
  }
}
```

Start or restart the server in your MCP client. In `gui_console`, run
`return await wayland.environment()` and use its returned `WAYLAND_DISPLAY`
and `XDG_RUNTIME_DIR` to launch a native Wayland app on the SSH host. Then run
`return await wayland.windows()` and look for the app with `mapped: true`.
This checks that the app connected through the remote proxy.
To keep sessions open for several hosts, configure a separate MCP server entry
for each destination and use the corresponding `gui_console`.

With `--ssh`, the local launcher starts the remote MCP. It uses the same
automatic executable staging as `--ssh-wayland`. The local JavaScript console uses
an OpenSSH reverse Unix-socket forward and requires stream-local forwarding on
the SSH server. Omit `--local-js` to keep JavaScript on the desktop host while
the local launcher still decodes video.

```sh
target/release/wayland-mcp --ssh user@desktop.example \
  --remote-display wayland-0 --local-js
```

The launcher uses non-interactive OpenSSH authentication and host-key verification.
`--remote-display` overrides the desktop host's `WAYLAND_DISPLAY`, and
`--ssh-config` selects an alternate SSH config.

### Use an installed remote executable

To run the entire MCP and JavaScript console on the desktop host using SSH
stdio directly, install a release executable on that host.

```sh
ssh -T -o BatchMode=yes user@desktop.example 'mkdir -p "$HOME/.local/bin"'
scp -o BatchMode=yes target/release/wayland-mcp user@desktop.example:.local/bin/wayland-mcp
```

Configure the MCP client to launch this SSH command over stdio. Substitute
your SSH destination and the socket name from the remote desktop session.

```sh
ssh -T -o BatchMode=yes user@desktop.example \
  'WAYLAND_DISPLAY=wayland-0 exec "$HOME/.local/bin/wayland-mcp"'
```

### H.265 capture

`wayland.videoFrame()` returns a normal in-memory image handle and attaches a PNG
to the foreground MCP result. With the local launcher, the desktop host sends a
complete H.265 access unit over SSH and the local GPU decodes it. With the full
remote MCP, the desktop GPU decodes it before attaching the PNG.

```js
return await wayland.videoFrame({
  windowId: (await wayland.windows()).find(w => w.mapped).window_id,
  chroma: 444,
  bitDepth: 10,
  toneMap: "knee",
  background: "black",
  qp: 20,
});
```

Omit `chroma`, `bitDepth`, and `screenContent` to prefer 10-bit 4:4:4 SCC and
fall back to 8-bit 4:2:0 HEVC Main when either endpoint lacks SCC support. Exact
requests also support 8- and 10-bit 4:2:0 and 4:4:4, and 10-bit 4:2:2 in the
range-extension profile, subject to VA-API support on both codec devices.
With explicit 4:4:4 sampling and no
`screenContent` setting, SCC is preferred and the range-extension profile is
the fallback at the requested bit depth. Set `screenContent` to pin the profile.
Options can change between calls. `encodeGpu` must match the GPU that owns
the captured buffer. `decodeGpu` pins the decoder on the receiving host. The image metadata reports
the selected profile parameters and render nodes. The input is a committed root
DMA-BUF in buffer coordinates. If the producer does not publish DRM feedback,
the proxy tries `captureGpu`, then the explicitly selected encode GPU, then
the only render node if there is one. The import must succeed. Use
`afterCommitSerial` and `timeoutMs` to request a newer commit.

Vulkan interprets the committed Wayland color description and assumes sRGB
with BT.709 primaries for untagged surfaces. It uses linear FP16 BT.2020
internally and composites alpha over black or a fixed checkerboard. Tone
mapping applies to FP16 RGB input. The default `auto` applies a highlight
knee to tagged HDR in that format. Set `toneMap: "curve"` and supply up to 16 ordered
linear input/output knots with `toneCurve` to define a GPU-evaluated curve. The
H.265 sequence signals full-range
BT.709 YCbCr with the sRGB transfer function and centered chroma for 4:2:0.
Source color information and the chosen conversion are included in image
metadata when available. The PNG preview uses opaque sRGB.

Each access unit carries one IDR picture with parameter sets. There are no
inter-frame dependencies or reorder delay. The decoder parses the access unit,
checks the access unit's SHA-256 and its negotiated coding and color parameters,
and waits for hardware completion. The GPU sizes and compresses the PNG preview.
An invalid or incomplete picture is returned as a tool error. The next capture
can recover independently.

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

Cargo runs `scripts/generate_wayland_protocols.py` against the installed XML files
in `/usr/share/wayland` and `/usr/share/wayland-protocols`. Generated Rust lives
only in Cargo's `OUT_DIR` under the build target directory; it is not source and
must not be committed. A fresh build regenerates it automatically. Changes to
the generator or any consumed system XML file trigger regeneration. Missing XML
files fail the build with their paths; install the corresponding Wayland development
and `wayland-protocols` packages. Protocol versions follow the installed packages.

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

Start with `return wayland.help`. JavaScript state persists between calls.

Continuous observation is **GPU-only and event-only**. Agents submit GLSL source
strings and receive their bounded result records. Explicit `screenshot({windowId})`
and `captureNextFrame({windowId})` support visual bootstrap through this MCP:
images remain in memory, attach to the foreground response and return opaque
`imageHandle` metadata. `presentImage(handle)` presents a retained image;
`disposeImage(handle)` releases it early. New captures automatically evict the oldest
handles when the 16-image/32 MiB cache fills and report `evictedImageHandles`.
Evicted handles expire; manual disposal is optional. No PNG files or image paths are created.
JavaScript does not receive image bytes or arbitrary GPU-buffer access. The server
does not create SHM pixel snapshots in production. Screenshot imports use the
same canonical DRM-device context and serialized queue as programmable observers.
When neither Wayland feedback nor the DMA-BUF exporter identifies a device,
capture and visual observers can try the only DRM render node on the host.
The import must succeed.
For shader bootstrap, `screenshot({windowId, coordinateSpace:"buffer"})` presents
the selected render buffer at its native extent, so sampled pixel coordinates
match GLSL images. The default `"window"` mode retains scene composition and its
input coordinate mapping. Apply `preview_to_full_scale` when viewing a reduced
MCP preview. Buffer mode does not crop or compose other surfaces.
If a decoration prevents window capture, the error identifies its surface
and suggests buffer mode when the render buffer is DMA-BUF-backed.
Screenshots report `commitSerial`, `baselineCommitSerial`, and `fresh`.
`fresh: false` means the capture reused pixels already available at the
baseline. A newer frame can still show an app partway through processing input.
To capture a specific rendering child rather than a SHM parent, use
`screenshot({windowId, surfaceId:30, coordinateSpace:"buffer"})` with a numeric
surface ID from `windows().subsurfaces`. Explicit surface capture validates
membership in that window's live tree and requires a committed DMA-BUF. It holds
an observation lease and waits up to one second for a fresh commit, copying the
GPU buffer before forwarding that commit. Released producer buffers are never
reimported. The returned image is in memory and excludes every other surface;
its metadata records `surface_id` and `commit_serial`.
Eligible render-buffer commits retain a bounded latest-frame cache in device-local
Vulkan storage. Buffer-mode screenshots can therefore present a completed frame
while the client is idle, without reimporting a producer buffer after release.

Capture leases, active visual subscriptions and explicitly targeted input drive committed `wl_surface.frame`
callbacks locally, so covered windows can render without being raised on the host
desktop. Input grants a 500 ms rendering lease for its target window. Callbacks
already forwarded to the host can complete once during observation; their IDs
remain reserved until the host releases them, and late duplicate completions are
suppressed.

`actAndCapture` starts capture observation before invoking its action and releases
the lease on success or failure. An action that closes the target can return
`surfaceDisappeared: true` with no image and no capture error.
`waitForCommit` holds a scoped observation lease
while waiting. Targeted input retains capture pixels for its 500 ms render lease,
so capture can follow a quick update without rereading a released producer buffer.
Visual subscriptions drive the frame clock without enabling CPU pixel capture.
`wayland.help` documents helper arguments, raw event fields, coordinate and scroll
units, observation lifetimes, image limits, and the precise color calculations used
by the fixed and programmable visual APIs.

The principal calls are `environment()`, `diagnostics()`, `windows()`,
`waitForWindow()`, `waitForWindowGone()`, `visualInfo()`, `onVisualProgram()`, and `onVisual()`.
`windows()` includes `subsurface_count` and `subsurfaces`: the exact live
`wl_subsurface` descendants of each window root, including nested and unbuffered
children. Each entry reports the role ID, surface ID, parent ID, committed
position, effective synchronization, committed buffer ID/type/dimensions, commit
serial and capture diagnostics. Pending attachments are not reported as committed
buffers. Destroyed roles are excluded. Protocol IDs are local to a client connection.
Window-level `buffer_kind` and `capture_details` describe `render_surface_id` only;
a SHM root does not imply a SHM rendering child. `visualInfo()` also includes
`subsurfaceCount` and `subsurfaces` for the same tree.

`visualInfo({windowId})` reports protocol buffer dimensions, scale/viewport, format,
and DRM affinity, without pixels or a promise of successful import.
Viewport destination dimensions describe Wayland logical coordinates; they do not
identify browser CSS dimensions, zoom or device-pixel ratio. Shader coordinates
and the default input coordinates use committed-buffer pixels.
`waitForCommit()` waits
for serial metadata and returns no image.

### Agent-defined visual reactions

Agents write general GLSL compute passes and script their reactions in JavaScript:

```js
globalThis.watch = await wayland.onVisualProgram({
  windowId,
  passes: [{source: agentWrittenGLSL, dispatch: [groupsX, groupsY, 1]}],
  resultBytes: 16,
  stateBytes: 16,
  scratchBytes: 4096,
  parameters: new Uint8Array(64),
  previousFrame: true,
  feedback: "previousResult",
  sourceColor: {transfer: "srgb", primaries: "bt709", alpha: "opaque"},
  maxFps: 60,
  durationMs: 120000
}, async (buffer, metadata) => {
  const words = new Uint32Array(buffer);
  // Interpret agent-defined bools/coordinates and conditionally generate input.
  globalThis.latestResult = {words: Array.from(words), metadata};
});
return watch.initialState;
```

All resources use descriptor set 0. Bindings 0/1 are readonly `rgba16f image2D`
current/optional previous images in linear BT.2020. Bindings 2–6 are `std430`
storage buffers: readonly previous state, next state, scratch, result, readonly
CPU-authored parameters. Push constants are four consecutive `uint` values:
width, height, processed sequence, history-valid. Atomic boolean flags use `uint`.
The agent supplies dispatch workgroup counts; local dimensions are declared in GLSL.

Scratch and result are zeroed on GPU each processed frame. Separate state is
carried forward before passes; `previousResult` copies the completed result to
GPU state. History refers to processed frames, including frames whose results JS
ignores. Multiple passes execute in order with GPU memory dependencies.

Results are delivered as fresh ArrayBuffers plus commit/timestamp/sequence metadata.
`status()`, `metrics()` and `unsubscribe()` are available. Callback authority
persists between console evaluations. Queue overflow, callback failure/timeout,
expiry and geometry/format/device changes terminate explicitly. Resubscription
starts with zero state and invalid history. Compilation occurs on subscription,
and parameters remain fixed until resubscription.

Limits: 1–8 passes, source ≤256 KiB/pass, result 4–256 bytes, state 4–65,536
bytes, scratch 4–1,048,576 bytes, parameters ≤4096 bytes; buffer sizes are multiples
of four. Frames have at most 8,388,608 pixels. One subscription per window, at most
eight windows; maxFps 1–120, lease 1–600,000 ms. SPIR-V resources, readonly access,
local workgroups and dispatch dimensions are checked before execution. Programs
are trusted agent analysis code: bounded output is not proof of semantic content.

Tracked Wayland color descriptions control GPU normalization, including named and
custom primaries, custom transfer powers, luminance ranges, PQ and HLG. An explicit
`sourceColor` supplies a fallback only for untagged buffers. Transfer names include
`srgb`, `linear`, `pq`, `hlg`, `bt1886`, `gamma22`, and `gamma28`, or Wayland IDs
1–14; primaries include `bt709`, `bt2020`, `display-p3`, and `adobe-rgb`, or Wayland
IDs 1–10. Optional `luminances: [minimum, maximum, referenceWhite]` specifies cd/m².
Alpha can be opaque, straight, or encoded premultiplied (the default); RGBX formats
always have alpha 1. Results identify the actual source description and whether
it was assumed. Color interpretation changes start a new `epoch` with
`historyValid: false` and zero GPU state. Source decoding preserves HDR highlights,
negative extended values and wide gamut in linear BT.2020 RGBA16F without SDR
clamping or tone mapping.

The earlier fixed predicate API remains available:

Agents configure fixed predicates and author their own reactions. For example,
this watches an application's status area; it contains no application-specific
controller:

```js
globalThis.statusWatch = await wayland.onVisual({
  windowId,
  rules: [{
    id: "status-lit",
    rect: [100, 100, 100, 100], // x, y, width, height in committed-buffer pixels
    kind: "luminance",
    polarity: "above",
    threshold: 0.8,
    minPixels: 100,
    debounceFrames: 2,
    cooldownFrames: 3
  }],
  maxFps: 60,
  durationMs: 120000
}, async event => {
  // Script the application's reaction here; callbacks run between console calls.
  globalThis.latestStatusEvent = event;
});
return statusWatch.initialState;
```

`luminance` reduces linear BT.2020 RGB (`0.2627002 R + 0.6779981 G + 0.0593017 B`) entirely on
GPU. A rule becomes active when at least `minPixels` meet its threshold. It emits
its initial occupancy, then occupancy transitions. `polarity` is `below` by
default; `above` is also supported. `change` compares maximum absolute RGB-channel
difference against a previous frame retained only on the GPU. Its first frame
establishes history; subsequent sufficiently changed frames emit active pulses.
Neither pixel counts nor samples are returned. Thresholds are finite and nonnegative;
HDR values may exceed 1. Both fixed and programmable observers use the tracked
source color profile or an explicit fallback for untagged buffers.

Events contain only `windowId`, `ruleId`, `active`, processed `frame`,
`commitSerial`, observation `timestampMs`, `deliveryTimestampMs`, `sequence`,
`sourceColor`, `epoch`, and `historyValid`.
Rule order is stable within a processed commit. Frame debounce and cooldown run
in the embedded compute kernel and count processed frames, not wall time.
`maxFps` limits processing; intermediate commits may intentionally be skipped.
History compares consecutive processed frames.

The handle offers `status()` and `unsubscribe({drain:true})`. Inside its own
callback use `drain:false`. Callback exceptions/timeouts, bounded-queue overflow,
expiry, window destruction, and geometry/format/device changes terminate the
stream explicitly. An overflow reports a gap; it does not silently drop events.
Terminal status includes processed frames and CPU wall time spent in GPU
submission/wait, including import work. These are operational metrics, not
image-derived values.

Budgets: one subscription per window, at most eight windows and sixteen rules
per window. Each rectangle needs width/height at least four and area at least
64 pixels; aggregate area per window is at most 1,048,576 pixels. Rule IDs are
unique strings of at most 64 bytes. `maxFps` is 1–120, `durationMs` is
1–3,600,000; debounce/cooldown are 1–3,600 processed frames.

### Vulkan ownership and readback boundary

Shaderc and its compiler dependencies are statically linked as archives. GLSL
source submitted through the console compiles entirely in memory; filesystem
includes, shader files, SPIR-V uploads and runtime compiler plugins are absent.
The fixed kernel is compiled into an embedded array by the Cargo build script,
using the same pinned, statically linked compiler as the runtime.

Contexts are retained per DRM device (primary/render nodes are canonicalized),
each with its own Vulkan instance,
device, queue, and pipeline. Wayland DMA-BUF `main_device` feedback is matched
against `VkPhysicalDeviceDrmPropertiesEXT`. Feedback is a preferred-device hint,
not proof of the producer's device: imports must succeed on that device; there is
no attempt to assume another GPU can import a compressed buffer. Missing affinity
or failed import terminates observation. No CPU/software fallback exists.

Imports are cached (eight buffers per subscription). The buffer's inode remains
pinned by imported memory and a duplicated FD, preventing reused Wayland buffer
IDs or recycled process FDs from selecting stale images. Producer fences are
exported as sync files and waited on by the Vulkan transfer submission. Explicit
Wayland acquire timeline points are honored before observation. Foreign queue
ownership is acquired and returned. The proxy completes the GPU copy into owned device-local storage before forwarding
the source commit, preventing premature buffer reuse. Programmable normalization
and analysis then run on owned resources in a worker, after forwarding. Busy or
rate-limited observations are skipped before acquisition, with counters exposed
through `metrics()`. Acquisition still adds a measured bounded commit delay.

For continuous observers, frame/history images, raw pixels, scratch and state are never mapped. Only the
agent-declared result (at most 256 bytes) is read back. The other CPU-visible
buffer holds CPU-authored parameters. The fixed predicate path maps a separate
272-byte transition packet. No frame resource becomes CPU-readable through the
observer interface, including on unified-memory hardware.

Explicit screenshots require a windowId and use a separate temporary host output
after copying and releasing the producer on its DRM device. PNG encoding and MCP
presentation stay in memory; source resources are not mapped. This bootstrap
readback does not run for `onVisualProgram`/`onVisual` subscriptions. Screenshot
handles are bounded to 16 images/32 MiB; new captures automatically evict the oldest
handles, so repeated screenshots do not require manual disposal. Each foreground
evaluation remains limited to 16 attached images/32 MiB.
The GPU cache holds at most eight windows and is released when their clients or
windows disappear. Cache copies skip busy devices; a screenshot reports its
retained commit serial, which can precede the latest commit. Observer metrics
measure observer acquisition and processing, not the additional cache copy.

Observers handle all advertised 8-bit, packed 10-bit and FP16 RGB DMA-BUF formats in raw
buffer coordinates. Destination-only viewport scaling and buffer scale are supported in these raw
coordinates. SHM, transformed/cropped content,
synchronized render subsurfaces and scene composition need additional GPU support; unsupported content
fails explicitly. It does not claim to observe host desktop windows that bypass
the proxy.

### Input and background scripts

Use `click`, `doubleClick`, `move`, `drag`, `scroll`, `pressKey`, `pressShortcut`,
`typeText`, `resizeWindow`, and `resetInputState`. Raw `pointerEvent`,
`keyboardEvent`, `touchEvent`, and `inputCapabilities` remain available.
`pressKey({windowId,key:"Space",holdMs:20})` accepts evdev codes, named keys, and
characters; `wayland.keyNames` lists named keys. Character helpers honor the
client's XKB layout. `resizeWindow` sends a size suggestion; confirm the applied
size through window metadata. A delivered input event does not prove that the
application accepted it.

Visual callbacks may schedule input using ordinary JavaScript and timers. Their
subscription authority survives foreground evaluations and is revoked on stop.
Agents should keep callbacks bounded and store summaries in `globalThis` for
later inspection. Model reasoning is not invoked by every event: the program
handles fast reactions while the agent inspects event summaries and updates it.

`onInput({windowId,origin:"human",devices:["pointer"]}, callback)` observes input
between calls with the same handle lifecycle. Input recording and playback remain
agent-written JavaScript. Clipboard routing preserves independent human and model
selections; see [clipboard attribution](docs/clipboard-attribution.md).

## Client launch workflow

1. Start the MCP server in the host Wayland session.
2. Call `wayland.environment()`.
3. Launch the target client with the returned `XDG_RUNTIME_DIR` and
   `WAYLAND_DISPLAY` values.
4. Use `waitForWindow` or `windows` to identify the mapped surface.
5. Configure `onVisual` predicates, perform input, and inspect returned events.
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

`diagnostics()` returns sanitized endpoint/preflight and console permission status.
Host socket paths, session internals and connection-history details are omitted
from the JS response. Backend selection is trusted startup configuration through
`WAYLAND_MCP_BACKEND_SOCKET`; the JS API cannot change it.

### Wine and Waydroid

These tests ran on KWin with Intel Arc B390 graphics. Capture depends on
the Wayland surface and buffer the app submits.

- **Wine 11.22, native Wayland.** A Win32 Direct3D 11 test app produced 30
  fresh render-buffer captures across three launches, with the expected
  pixel changes after injected clicks and keys. This worked with the
  default WineD3D renderer and with `WINE_D3D_CONFIG=renderer=vulkan`.
  The app closed via injected input and disappeared from `windows()`.
  `resizeWindow()` did not change its extent. The GDI-only test accepted
  input but used shared memory and could not be captured. Wine's X11 driver
  bypassed the proxy. See [rootless Xwayland status](docs/xwayland-status.md).
- **Waydroid 1.6.3.** The Android session booted through the proxy and
  exposed separate Calculator, Settings and Clock toplevels. In a separate
  Android test app, 12 consecutive child captures per launch showed
  changing animation across two starts. Fresh frames also confirmed changes
  from injected keyboard, mouse and touch input. One subsequent keypress
  produced no visible change. An injected touch also completed Android's
  uninstall dialog for the test app, and the package disappeared from the
  app list. Whole-window capture failed on shared-memory parents, and idle
  Calculator and Clock child captures timed out. Stopping the session
  removed its windows.

For Wine, set `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` from
`wayland.environment()`, unset `DISPLAY` and `WAYLAND_SOCKET`, then launch
`wine app.exe`. For a new Waydroid session, keep the desktop's
`XDG_RUNTIME_DIR` and use the absolute proxy `socket_path` as
`WAYLAND_DISPLAY` when running `waydroid session start`. Then run
`waydroid app launch PACKAGE`. If the app does not map, try
`waydroid show-full-ui` and launch the app again.
[Waydroid's session and app commands](https://docs.waydro.id/usage/waydroid-command-line-options).

In these tests the GPU image was in a subsurface. Find its current numeric
`surface_id` in the target window's `subsurfaces` with
`has_committed_buffer: true` and `buffer_kind: "dmabuf"`. Use
`screenshot({windowId, surfaceId, coordinateSpace: "buffer"})` during an
animation or start the capture before triggering a redraw. The raw image
omits window decoration and viewport cropping. Input helpers still use
window coordinates. Both visual subscription APIs rejected the
shared-memory toplevels, and neither accepts a child `surfaceId`.

## Configuration

| Variable | Purpose | Default |
| --- | --- | --- |
| `WAYLAND_MCP_ARTIFACT_DIR` | Directory for JSONL operational records (no production PNG captures) | A private, unique directory under the system temporary directory |
| `WAYLAND_MCP_DMABUF_MODE` | Trusted startup negotiation policy: `capture-compatible` filters formats, `transparent` preserves host format/modifier advertisements | capture-compatible |
| `WAYLAND_MCP_EVAL_TIMEOUT_MS` | JavaScript evaluation timeout in milliseconds | 120000; values are clamped to the supported range |
| `WAYLAND_MCP_SOCKET` | Proxy socket name inside its private runtime directory | `wayland-mcp-0` |
| `WAYLAND_MCP_BACKEND_SOCKET` | Host compositor socket name or absolute socket path | The inherited `WAYLAND_DISPLAY`, otherwise `wayland-0` |
| `WAYLAND_MCP_TRACE` | Enable verbose proxy tracing on standard error | Unset |

Artifacts can contain input metadata and agent-returned event summaries. Store
them in an appropriately protected location and apply the retention policy of
the environment in which the server runs.

## Limitations

- Protocol exposure follows an explicit capability policy; decoder availability
  alone does not grant access. Registry versions and binds are checked.
- GPU events require successful DMA-BUF import on the selected DRM device.
- The compositor boundary does not provide a semantic widget or accessibility
  tree; assertions are based on frames, surface metadata, and application
  commits.
- `typeText` supports characters directly represented in the target client's
  active XKB layout by default. The opt-in `unicode-hex` method requires
  application support for Ctrl+Shift+U; other compose or input-method-mediated
  text requires an input-method companion.

## License

Apache-2.0. See [LICENSE](LICENSE).


### Programmable observer validation

`./scripts/validate.sh` runs the release tests and lints, including console/input
lifetime and configuration checks. `python3 tests/visual_program/console_contract_probe.py`
checks cross-VM ArrayBuffer packing and callback decoding. Run
`python3 scripts/validate-gpu-boundary.py` against the built production binary
to check that failed captures create no image files and the memory-only API is advertised.

Native probes live in `tests/visual_program`. Configure CMake with
`BASELINE_HEADER_DIR` pointing to Cargo's generated `visual_spirv.h` directory
and `SHADERC_ARCHIVE` pointing to Cargo's complete
`target/release/build/shaderc-sys-*/out/lib/libshaderc_combined.a`. Select the
archive from the current build when more than one build directory exists.
Both Cargo and CMake enforce C++26, `-fno-exceptions`, `-fno-rtti`,
`-Wall -Wextra -Wpedantic -Werror -Wold-style-cast` without warning suppressions
for project-owned C++. Cargo respects `CXX`; CMake accepts
`CMAKE_CXX_COMPILER=g++` or `clang++`. Third-party compiler sources use their
upstream build flags; project warning and sanitizer flags do not rebuild or
instrument the bundled compiler archive.
Run native configurations sequentially with `cmake --build ... --parallel 2`
and remove temporary probe artifacts after validation.
For GCC static analysis, configure the CMake build with
`CMAKE_CXX_COMPILER=g++` and `CMAKE_CXX_FLAGS="-fanalyzer"`, then build all targets.
For native sanitizers, configure the CMake build with
`CMAKE_CXX_FLAGS="-fsanitize=address,undefined -fno-omit-frame-pointer"` and run
the compiler, runtime, normalization, and edge probes. Leak detection remains
enabled. A configuration with `-fsanitize=thread -fno-omit-frame-pointer` runs
`concurrency_probe`: compilation/destruction overlaps 200 GPU submissions on
one context.

`runtime_probe --validation` exercises the actual programmable engine with
GPU-generated sources, both state modes, bounded results and invalid-resource
rejections. `runtime_edge_probe --validation` checks zero/partial writes in both
feedback modes, rejection of oversized push blocks, all ten advertised RGB
formats, all fourteen transfer functions, HDR/extended FP16 values, alpha,
color-change history resets and import-cache layout identity.
`normalization_probe --validation` checks channel order, color and
alpha through predicate outputs, with `--wrong-reference` as a negative control.
These probes select DRM device major 226, minor 128 (`/dev/dri/renderD128`)
and leave source frames on the GPU.

`python3 tests/visual_program/live_capture_probe.py` launches the release MCP and
an actual Wayland Vulkan cube. It decodes in-memory screenshots, checks that the
pixels change, then verifies changing program results and fixed motion events.
It also checks that native reads complete during shader compilation and that
ending an evaluation during compilation reclaims the late subscription.
It requires `vkcube`, Pillow and access to the host Wayland/DRM session. This
checks live DMA-BUF acquisition and callback delivery; synthetic GPU numerical
probes separately cover every supported source format and color transfer.
