# Plan to address feedback.md

Baseline: commit `94d22a7` (the same revision reviewed in `feedback.md`).
Reviewed against the source on 2026-09-27. Implementation is in progress; see [implementation status](docs/implementation-status.md)
for delivered behavior and remaining compatibility work. Section numbers group work by topic; the delivery
sequence at the end defines dependencies and the order of implementation.

Preserve the existing rootless Wayland proxy, window-local interaction, independent
agent input, and DMA-BUF presentation path. Prioritize confidentiality boundaries,
then observation correctness and demonstrations, then broader compatibility.

The console is a persistent programmable environment for agent-written workflows.
It needs JavaScript state, functions, timers, event listeners and scoped GUI
primitives, but no general filesystem, network, subprocess or host-device I/O.
Recording and playback are programs the agent writes, not MCP features. Drawing
and compositing through scoped image/surface primitives are a later extension.

Review outcome: the earlier plan hid substantial work in "virtualize clipboard",
"honor synchronization" and "add listeners". Those are now split below. Clipboard
routing follows the latest trusted input source per client and seat. Other gated
investigations have explicit experiments and failure outcomes.

## Findings and scope

The baseline source confirmed these findings (the implementation now changes
these paths):

- `src/js_console.rs::run_node_session` starts Node with inherited environment
  and no OS sandbox. `src/wayland_console_runtime.mjs` injects host functions into
  `vm.createContext`.
- `local_clipboard_request` in `src/gui_backend_wayland.rs` only consumes selection
  claims carrying synthetic serials; it does not isolate clipboard exchange.
- `is_supported_backend_global` admits interfaces based on decoder availability.
- `capture_window_rgba` reads one selected surface. Tracking a surface hierarchy
  does not yet provide a committed, composited window image.
- DMA-BUF advertisements are filtered through `supports_screenshot_drm_format`.

Two qualifications change the proposed work:

1. Sandboxing the console does not sandbox applications generated or launched by
   the agent. An application with ordinary host filesystem/socket access can
   bypass the proxy. Strong host confidentiality requires a restricted application
   launch environment as well as the three protocol/runtime fixes.
2. Rootless Xwayland needs an X window manager (XWM) and surface-role integration.
   Starting it on the private socket is insufficient. Treat it as a separate
   integration project, not a small launch option.

## 1. Define the boundary and build security fixtures — P0

Document trusted components (Rust broker/proxy, host compositor, kernel/driver)
and untrusted components (evaluated JavaScript and test applications). Define one
MCP instance as one test namespace. Maintain two clipboard domains: a human
clipboard shared with the host and a model clipboard private to the test
namespace. Route clipboard actions by the latest trusted input source per client
and seat, defaulting to model until human input arrives. Separate instances are required for unrelated
test sessions. Human transfers into an observed application deliberately cross
the host/test boundary; content subsequently displayed or retained by that
application cannot be promised private from the model.

Create a fake host compositor and small malicious/test Wayland clients. Use
synthetic secrets and sockets to test forbidden capabilities without accessing
real personal data. Cover malformed messages, FD handling, object lifetimes, and
resource limits at the Rust boundary, not only successful helper calls.

Foundation changes, delivered separately:

| Slice | Concrete change and location | Completion check |
| --- | --- | --- |
| F1: wire fixture | Extend the existing Unix-socket/FD tests in `gui_backend_wayland.rs` into a reusable fake backend: registry/bind, sync, scripted events, configurable fragmented writes and FD delivery. Add a tiny SHM client with an input log and selectable clipboard behaviors. | Drive a client through the real proxy transport; assert emitted messages and FD ownership, not only internal tracker state. |
| F2: one client writer | Route local replies, backend relay events and injected input through one ordered writer per client. Current `handle_proxy_client_blocking`, `relay_backend_events_blocking` and injection paths write through separate stream clones. Enqueue complete messages plus owned FDs, with bounded queues and disconnect on unrecoverable overflow. Never wait for a writer while holding the server-wide state lock. | Concurrent fragmented messages with FDs arrive intact and in assigned order; a stalled client does not freeze a second client. Subscription barriers use this same ordered delivery point. |
| F3: build-time protocol metadata | Generator restored; `build.rs` generates into Cargo `OUT_DIR` from installed `/usr/share/wayland` and `/usr/share/wayland-protocols` XML files and tracks those inputs for rebuilds. The generated Rust source has been removed from `src/`; it must never be committed. Message `since`/destructor metadata is generated and enforced for versions and object lifetimes. | Clean build generates bindings without a checked-in artifact; identical installed XML produces deterministic output. Record installed protocol versions in validation reports; reject unsupported old-version requests using generated message metadata. No vendored/pinned XML inputs. |

Keep initial fixtures small: one seat, two surfaces and scripted clipboard peers.
Add subsurface/GPU behaviors only in the slices that need them; do not build a
general compositor as a prerequisite for all work.

Add a supported restricted client launcher/profile: expose the proxy socket,
explicit test files and required runtime libraries, and narrowly selected GPU
render nodes; exclude host display sockets, session D-Bus, home directories,
agents, and unrelated devices. Network access must be an explicit test policy.
Preserve the external-launch workflow but label its isolation as unverified.
This does not constrain any separate shell or tools available to the calling agent.

Deliver this as `scripts/run-test-client` plus a reviewed profile and fixture
recipe, not a new application lifecycle manager or a console shell command.
First support the small SHM fixture with no GPU; then explicitly mount one render
node and its driver closure for the GPU fixture. Mount the exact current proxy
socket into a private runtime directory, not the whole host runtime directory.
Reject unavailable sandbox prerequisites instead of falling back to host access.
The profile's trust claim covers only tested launches; external launch remains
supported without claiming isolation.

Acceptance: the restricted client cannot read a host sentinel file or connect to
a host sentinel socket even when given its exact path. It can still display a
native Wayland window and use the permitted GPU device. Document residual trust
in the kernel, driver, compositor, and Rust protocol parser.

## 2. Isolate the programmable console and restrict its broker — P0

Keep persistent JavaScript and the existing convenience API. Expose no general
I/O APIs to evaluated code; retain only scoped GUI operations through the broker.
Keep the current Node runtime with Node's permission system denying filesystem,
network, child processes, workers, native addons, FFI, WASI and inspector access.
User correction: no bwrap dependency and no kernel-enforced sandbox, namespaces,
Landlock or seccomp. Keep persistent programmable JavaScript.

Implementation targets: `src/js_console.rs`, `src/wayland_console_runtime.mjs`,
`src/main.rs`, and `src/console_runtime.rs`.

- Start Node with `--permission`, no allow flags, an empty environment and piped
  transport. The broker owns capture/artifact I/O. Runtime dependencies load
  normally; evaluated JavaScript cannot open host files or sockets.
- Bound stdout/stderr and the JS heap. Fail closed if Node does not support the
  required permission categories; never restart without the permission flags.
- Extend the pipe-based RPC connection with ordered event delivery. Treat all
  messages on it as hostile: validate method arguments in Rust, reject
  stale/duplicate calls, and bound frame length, logs, results, requests, images,
  subscriptions and queued work. JS-side validation is convenience only.
- Distinguish a foreground evaluation from the lifetime of the JS session.
  Subscriptions and their callbacks must continue after an evaluation returns,
  including while the agent waits for the human. Scope foreground calls to an
  evaluation and background callback calls to a live, broker-issued subscription
  context; neither grants broader GUI access. Do not require every callback to
  inherit an evaluation ID that has already completed. Revocation invalidates
  callback contexts and pending work, with explicit errors.
- Keep evaluation deadlines effective during native calls and blocked writes;
  cancel outstanding operations on termination and terminate the Node
  process. Give background callbacks separate execution budgets; idle time
  awaiting human activity must not consume a foreground evaluation timeout.
  Specify that restarting clears JavaScript state and all subscriptions, and
  report that loss. Normal evaluation completion must not clear either.
- Remove `selectBackend` from the untrusted API; backend selection belongs to
  trusted startup configuration. Return sanitized diagnostics instead of the
  complete backend snapshot. Redaction complements access control; hiding a
  socket path is not isolation.

Acceptance: execute the feedback's constructor escape with Node permissions enabled and
assume it obtains Node's `process`. Host file/socket/network access must still
fail. Test environment and FD leaks, forged RPC, oversized output, infinite loops,
async hangs, subprocess attempts, restart isolation, and normal persistent helpers.
Also verify a listener continues receiving input between completed tool calls,
and that callback errors or queue overflow are visible without blocking the proxy.

Implementation slices:

| Slice | Implementation | Completion check |
| --- | --- | --- |
| J1: runtime permissions | Start Node with its permission system and no allow flags; deny filesystem, network, child processes, workers, native addons, FFI, WASI and inspector access. Clear the environment and use only piped transport. No kernel enforcement or external sandbox dependency. | Constructor-escape code cannot read/write sentinel files or open sockets/subprocesses through Node APIs. Promises and timers work; unsupported permission controls are a launch error. Record the supported Node version. |
| J2: broker scheduling | Split stdout parsing, stdin writing and native-call execution into separate bounded tasks; select deadlines/cancellation in the supervisor. `handle_native_call(...).await` and pipe writes currently run inside the main select arm. Preserve order for side-effecting input calls, while allowing an observation wait and event/control delivery to progress. | A pending `captureNextFrame` does not block input, events or timeout handling; a child that stops reading is terminated on deadline. Cancelled work cannot inject later input, and already delivered effects are reported rather than described as undone. |
| J3: capability and lifecycle enforcement | Validate session generation plus evaluation/subscription context in Rust, remove untrusted backend selection, sanitize diagnostics and enforce transport limits. Context identifies ownership/cancellation, not a new security boundary inside a compromised Node process. Add a supervisor watchdog independent of Node for hangs while idle. | Old contexts fail after restart; callbacks survive normal evaluation completion. A wedged callback cannot freeze the broker, and resource exhaustion terminates only the console. |

Initial broker limits: 2 MiB per JSON frame, 64 outstanding native requests,
32 subscriptions, and 4096 events or 4 MiB per subscription (whichever is first).
Keep current foreground evaluation timeout defaults. Stop an overflowing
subscription and expose a terminal gap/error through a reserved control channel;
never silently drop motion samples. Apply limits before allocation. These are
starting defaults to test at 1000 pointer events/second, not performance claims.
Document/tune them through trusted configuration without changing origin policy.

Per the user's correction, do not add OS resource containment. Report the JS heap
limit accurately; it is not a total process-memory limit. No unrestricted-Node
fallback is part of this plan.

## 3. Make protocol exposure an explicit policy — P0

Implementation targets: `src/gui_backend_wayland.rs` and
`src/wayland_protocol_registry.rs`; add a small, hand-maintained policy module
independent of generated protocol metadata.

- Classify globals as `Forward`, `Virtualize`, or `Deny`, with rationale and
  reviewed version limits. Default unknown/unreviewed interfaces to `Deny`.
  Forwarding requires both policy approval and complete wire/FD support.
- Enforce the policy when advertising globals AND binding them, including guessed
  names, interface/version mismatches, removal, and backend reconnects.
- Review core rendering, seat, output, presentation, color, DMA-BUF and sync
  interfaces individually. Filter host metadata where needed. Explicitly deny
  capture, foreign-toplevel, output-management, virtual-input, security-context,
  session-lock and clipboard-control capabilities unless locally implemented.
- Ensure generated decoder additions never automatically widen exposure.

Acceptance: a synthetic registry containing every generated interface plus
unknown and dangerous interfaces produces exactly the reviewed exposure set;
direct binds cannot bypass it. Existing supported native clients still map,
receive scoped input, and render through the proxy.

G1 is one policy change with a table-driven test suite. Start with reviewed
rendering globals (`wl_compositor`, `wl_subcompositor`, `wl_shm`, `xdg_wm_base`,
viewport/fractional-scale), mediate seat/output/clipboard, and separately review
DMA-BUF, sync, color, presentation, tearing and FIFO. Clamp to both host and tested
implementation versions. All others default to denied until explicitly classified.
The table must include core globals missing `is_global` metadata; that flag is not
an exposure decision. Hold data-device exposure closed until C2/C3 below are ready.
For mediated objects, malformed or undecodable messages must fail closed instead
of reaching `forward_raw_request`; retain an explicit manual decoder for `wl_shm`.

## 4. Route human and model actions to separate clipboards — P0

Replace `local_clipboard_request` with a mediated data-device service that keeps
two independent selections. Human copy/paste uses the clipboard shared with the
host; model copy/paste uses the private clipboard. This is the intended default
interaction model, not a deferred optional host bridge.

| Action origin | Copy / selection write | Paste / selection read |
| --- | --- | --- |
| Human, established by trusted input tracking | Host-shared clipboard | Host-shared clipboard |
| Model, established at injection | Private clipboard | Private clipboard |
| Before any trusted input | Private clipboard | Private clipboard |

- Track the latest input source in Rust per client and seat. Host input selects
  human; injected input selects model. Use that value for subsequent clipboard
  operations. Do not trust actor fields supplied by JavaScript or applications.
  Replayed input counts as model input. Overlapping and delayed operations use
  the current value without detection, reporting, rejection or special handling.
- Preserve both clipboard selections across actor changes. Never seed or refresh
  the private clipboard from the host. Model operations must neither read nor
  overwrite host selection; human copy must not overwrite private selection.
- Mediate all offers and receive FDs. Resolve each operation against the current
  input source, including operations using older offer IDs. Keep host selection
  metadata out of model-mode offers.
- Handle managers, devices, sources, MIME offers, independent selection ownership,
  null selection, cancellation, destruction and disconnects. Support private
  cross-client copy/paste and human copy/paste between host and proxied clients.
- Solve server-created object IDs first. The existing comment identifies a real
  collision/order constraint: local offers must coexist with backend-created
  objects. Introduce consistent ID allocation/remapping, including object and
  `new_id` arguments and lifecycle events, wherever the two namespaces overlap.
- Transfer data through local FDs with bounded buffering and cleanup/timeouts.
  Do not put host clipboard contents into diagnostics, console results or logs.
- Apply the same origin policy to drag-and-drop, which shares `wl_data_device`:
  human transfers may cross the host boundary; model transfers stay private.
  Deny primary-selection/data-control extensions until they receive equivalent
  mediation. Unsupported routes must not become host-access fallbacks.

Acceptance:

- Seed host/private clipboards with different sentinels. Human paste receives the
  host value; model paste receives the private value. Each actor's copy updates
  only its respective selection, including cross-client cases.
- Exercise alternating and overlapping actions, delayed receives, menus, focus
  changes, stale offers, forged/reused serials, multiple MIME types, object-ID reuse,
  cancellation and teardown. Confirm each operation uses the current input source.
  Do not add diagnostics or special handling for overlapping or delayed actions.
- Verify model-only hostile clients cannot read or change host selection, including
  by claiming human provenance or replaying recorded input.
- Test toolkit clipboard caching explicitly. Once a human transfer releases host
  data to an application, the application can retain or render it; switching
  offers cannot revoke those bytes. Document this exposure instead of asserting
  that subsequent model interaction cannot observe it.

Implementation slices:

| Slice | Implementation | Completion check |
| --- | --- | --- |
| C1: object namespace | Extend `WaylandResourceMap` into a live translation layer. Allocate downstream server IDs for both locally created and forwarded objects in a single space; track `Local` versus `Forwarded` ownership and generations. Rewrite message headers, object/new-ID arguments and special object-ID fields such as `delete_id`/display errors; keep FD ordering unchanged. Map proxy-created upstream objects separately from client IDs. | Interleave a local data offer with backend-created DMA-BUF buffers and callbacks; exercise destroy/reuse and nullable IDs through the real wire fixture. Current map getters are unused by raw forwarding, so populating the map alone does not complete this slice. |
| C2: private selection service | Store selection ownership at namespace/seat scope, not per connection. Implement manager/device/source/offer lifetimes, local serial bookkeeping, MIME negotiation, `send`/`receive`, cancellation and disconnect cleanup. Pump data in bounded async tasks outside proxy locks; close both endpoints on timeout/cancellation. | Two native clients exchange text and binary data; owner exit clears selection; rejected/expired offers transfer zero bytes. Use a payload larger than a pipe buffer to expose deadlocks. |
| C3: human host bridge | Keep host offers in trusted proxy state and expose only mediated offers when the latest trusted input is human. Track real host serials separately from synthetic/client-visible serials; an arbitrary integer supplied by JS must never establish host authority. Implement independent host/private ownership and focus changes, including upstream objects created by the bridge. | Test host-to-client and client-to-host transfer, then model paste/copy, overlapping actions and stale offers. Preserve both clipboard values independently. |
| C4: remaining transfer routes | Add drag-and-drop origin/actions/finish/cancellation only after the selection service passes. Keep PRIMARY/data-control denied until separately implemented. | Model drags cannot import/export host data; human drags work; aborted drags leave no active transfer or FD. |

Security milestone: steps 1–4 pass together before claiming host isolation for
the supported restricted launch workflow, with human clipboard transfers stated
as an explicit boundary crossing. Broader compatibility must not bypass these
controls.

## 5. Composite the committed surface tree for observation — P1

Implementation targets: the frame tracker and input mapping in
`src/gui_backend_wayland.rs`, plus `src/gui_vulkan_dmabuf.rs` and `src/gui_color.rs`.

- Replace the promoted-surface heuristic with an explicit capture scene:
  root, subsurface stacking/positions, effective synchronized commits, buffer
  scale/transform, viewport crop/destination, alpha, and mapped lifecycle.
- Define popup inclusion and window geometry/origin in the capture API. Apply
  the same geometry to hit testing so a click on an overlay reaches that surface.
- Snapshot effective committed state consistently, retain required buffer
  lifetimes, and honor acquire/release synchronization for every sampled buffer.
  Account for capture completion before permitting buffer reuse.
- Convert surface colors into a common compositing space; blend with correct
  alpha semantics and apply the documented SDR preview policy. Preserve notices
  for HDR content. Host decorations and display-specific tone mapping remain
  outside the observation contract.
- Begin with a deterministic CPU reference compositor on the observation branch;
  assess GPU composition after correctness. Preserve direct buffer forwarding for
  display presentation. Unsupported contributing surfaces must produce an explicit
  incomplete/error result, not a silently misleading image.

Acceptance: pixel fixtures cover layered SHM/DMA-BUF content, transparent overlays,
stacking changes, synchronized children, popups, fractional viewports, rotated and
scaled buffers, mixed color descriptions and destruction. Verify screenshot-to-input
coordinates and that observation adds no CPU upload to the presentation path.

Implementation slices (keep the existing capture path until its replacement
passes the corresponding fixtures):

| Slice | Implementation | Completion check |
| --- | --- | --- |
| S1: committed scene state | Introduce pending, synchronized-cache and effective state for each surface, and ordered parent/child relationships. Track attach/detach, offsets, viewport source/destination, scale/transform, input regions and above/below ordering. Latch synchronized children and parent-owned position/stack changes at the correct commit; implement nested sync/desync transitions. | A colored child committed under a synchronized parent stays unchanged in capture until the parent's commit; detached/destroyed children disappear at the correct boundary. Test nested children and reused buffer IDs. |
| S2: owned SHM snapshots | Replace buffer-ID-only references with generation-tagged committed buffer references. For observed SHM commits, snapshot pixels before the client may reuse storage. Move relevant tracking before forwarding the upstream commit; current `ingest_request` tracks after forwarding. Retain an immutable scene snapshot for composition and release server locks before pixel work. | Immediately overwrite SHM after release and destroy/reuse the protocol ID: the retained observation still contains the committed pixels. Unobserved/uncached content is reported unavailable. |
| S3: geometry and SDR composition | Compose immutable SHM snapshots back-to-front in linear premultiplied color. Track full x/y window geometry, define capture bounds as the union of root and included descendants, return its origin and logical-to-image transform. Implement inverse mapping and input-region hit testing, including implicit pointer grabs during drag. | Tiny hand-calculated alpha/stacking fixtures match within 1 output channel value; transformed clicks hit the intended surface and drags stay on the grabbed target. Preserve fractional coordinates. |
| S4: DMA-BUF lifetime prototype | Implement the observation lease and synchronization experiment described below before calling DMA-BUF tree capture correct. Extend `gui_vulkan_dmabuf.rs` to return an owned snapshot, not a view that may be reused by the client. | Accelerated client reuses buffers immediately on release while capture runs: no tearing, stale frames or early release signals under implicit and supported explicit sync. Unsupported sync modes return a capture error. |
| S5: GPU/color/popups integration | Connect owned GPU snapshots to composition; split `gui_color.rs` decode/linear conversion from tone mapping/quantization so surfaces are blended before output encoding. Track popup parents/positioners and actual configure/reposition events. Add mixed-color tree metadata and popup-inclusive bounds. | Mixed SHM/GPU trees, HDR+SDR overlays and nested popups match defined reference fixtures; existing single-surface color fixtures retain their documented behavior. |

S4 needs a real ownership strategy, not just duplicated DMA-BUF FDs. Start with
an explicit observation lease on the selected window: copy each newly effective
buffer needed by the scene into proxy-owned storage while observing; retain only
the current scene and bounded in-flight copies. `captureNextFrame` arms the lease
before the next commit. `screenshot` returns a valid cached snapshot or a specific
`snapshot_unavailable` error if an earlier released buffer was not captured; it
must not read potentially reused memory and call it the current frame.

The implementation copies newly committed GPU content before forwarding the
commit to the host. The producer's acquire point is honored, and the copy finishes
before host release of that use can occur; no replacement release timeline is
needed for this ordering. For idle implicit-sync content, hold `wl_buffer.release`
until the observation copy completes. An already passed release cannot be revoked.
Idle explicit-sync content must have an owned snapshot; otherwise wait for a fresh
observed commit or report `snapshot_unavailable`. Never read uncaptured storage
after an independently signalled release point. Legacy explicit-sync remains denied.

Bound capture work and preserve host presentation if a copy cannot start safely:
cancel capture and report failure before taking a lease. If an in-flight GPU copy
hangs, fail the affected client/session rather than signal unsafe early reuse.
Measure the copy overhead and any release delay; preserving the presentation
buffer-sharing path does not mean observation has zero GPU cost. This is a gated
experiment with a real GPU, independent of deterministic S1–S3 progress.

## 6. Expose input listeners for agent-written recording and playback — P1

Implement generic event subscriptions and input primitives, not
`beginHumanDemonstration`, `record`, or `playRecording` APIs. The agent implements
data collection, filtering, timing, stopping, editing and repeated playback in
ordinary JavaScript. Recordings live in its persistent JS state and need no file
I/O. Examples belong in documentation and integration fixtures, not native workflow
implementations.

Implementation targets: backend event tracking in `src/gui_backend_wayland.rs`,
the bidirectional transport in `src/js_console.rs`, and listener dispatch in
`src/wayland_console_runtime.mjs`.

Required primitive contracts (API names below are proposed):

- `wayland.onInput({windowId, origin, devices}, callback)` returns a subscription
  after an acknowledged start boundary. Offer scoped pointer and keyboard events,
  including movement, enter/leave, buttons, axes, frame boundaries and modifiers.
  Only events delivered to the selected window's surface tree may be exposed.
- The subscription also returns an input-state snapshot taken at the same start
  boundary: focused surface, last position, pressed buttons/keys, modifier/layout
  state and keymap identity. Starting while already focused otherwise omits the
  enter event needed for replay. Return unknown state explicitly; JS decides
  whether to wait for a clean state or reject the recording.
- Provide broker-assigned origin, a monotonic receive timestamp, sequence number,
  stable surface identity, coordinates with a defined space, and protocol payload.
  Preserve ordering and raw motion samples; do not silently coalesce events.
  Track real focus separately from injected focus to attribute events without a
  surface field. Never expose input from unrelated host windows.
- Dispatch callbacks while no foreground evaluation is running. Keep state alive
  across tool calls and conversation turns in the same MCP session. Define callback
  ordering/reentrancy, bounded queues, gap/overflow events and error reporting.
  A slow callback must not block host input forwarding. Allow callbacks to perform
  scoped GUI operations, using the session/subscription authority from step 2.
- `subscription.unsubscribe()` defaults to draining callbacks through an
  acknowledged end sequence before resolving, when called from another evaluation.
  Provide `unsubscribe({drain:false})` for cancellation inside a callback: cancel
  queued callbacks and resolve without waiting for that callback to finish, with
  a discarded-event count. Reject self-draining unsubscribe instead of deadlocking.
- Reuse `pointerEvent`, `keyboardEvent`, timers and a monotonic JS clock for replay.
  Expose enough surface targeting and coordinate information to reconstruct input
  on the same surface or let JS explicitly map to another valid target. Generate
  fresh injection serials and current protocol timestamps; do not reuse recorded
  authority. All replayed events are model-origin, including for clipboard routing.
- Report surface destruction, stale targets, runtime restart and unsupported event
  types explicitly. JS chooses whether to abort, remap, reset application state or
  retry. Resource limits may stop a runaway script, but there is no one-shot replay
  restriction and replay must not consume or mutate the recording.

Implementation slices:

| Slice | Implementation | Completion check |
| --- | --- | --- |
| E1: scoped event model | At F2's ordered writer, assign a session-local sequence and monotonic Rust timestamp to successfully delivered events; update real-focus state for host events and keep model input separately labeled. Resolve implicit surface targets for buttons/axes/keys. Return an opaque generation-tagged surface token, not a reusable wire ID. | Fake-host input is attributed to the correct window; socket-send failure is not recorded as successful delivery; destroyed/reused surface IDs cannot be targeted by old recordings. |
| E2: subscriptions and transport | Add `subscribe_input`/`unsubscribe_input` broker calls and `input_event`/terminal-status messages with subscription ID and session generation. Atomically take initial state/start sequence, register delivery and acknowledge before JS sees events. Choose one canonical pointer/keyboard resource per seat so multiple bindings do not multiply samples; expose a gap if that stream is replaced. Require explicit seat selection if ambiguous. | Two pointer resources get normal protocol delivery but one listener gets one ordered stream. Start/stop racing with input has a defined boundary. Queue overflow stops the stream with a visible error. |
| E3: JS dispatch | Add `performance.now` and `structuredClone` to the provided primitives (neither is currently injected). Keep a FIFO per subscription, await one callback at a time within that subscription, and let foreground evaluation/control messages progress while async callbacks wait. Keep callbacks across calls; report errors via subscription status and next-call diagnostics. | A callback awaiting a GUI response does not deadlock the reader; a later evaluation can stop a subscription. Infinite loops, callback rejection and abandoned promises have explicit failure/termination behavior. |
| E4: exact replay inputs | Extend `GuiWaylandPointerEventRequest` and Rust validation with the surface token and an explicit `surface-fixed` coordinate mode for exact 24.8 values. Existing full/preview integer coordinates stay compatible. Raw keyboard injection must also target a scoped token. Generate fresh serials; JS cannot relabel injected input as human. | Fractional pointer samples survive a record/replay round trip exactly; scale changes require an explicit JS mapping or fail clearly. Buttons and axes use established target/focus, including pointer grab state. |
| E5: agent-authored workflow | Add documentation and an integration fixture that define the recorder/replayer entirely in JS and exercise separate MCP calls. Support pointer first; add keyboard/keymap state as its own extension using the same transport. | The five-part scenario below passes with no native recorder/player API. Repeat at least three times without modifying the saved recording. |

Define the v1 payload in `gui_backend.rs` with explicit variants: `kind:"input"` contains
`sequence`, `timestampMs`, `windowId`, `surfaceId`, `device`, trusted `origin`,
`coordinateSpace` and injection-compatible `input`; terminal/status events use
`kind` and never masquerade as raw input. Sequence gaps are reported, not inferred
from filtered-out events. Successful unsubscribe is a control acknowledgement,
not an error/status callback. A screenshot from a background callback returns an image
handle plus metadata retained under that subscription; a later foreground call
explicitly presents it. Do not attach it to whichever evaluation happens to run
next. Bound retained background images separately and preserve color notices.

Listener event timestamps are receive/delivery observations, not measurements of
the physical mouse hardware clock. The record/replay acceptance test compares
protocol order and relative scheduling within a declared tolerance.

The following illustrates the required lifecycle; it is proposed API usage, not
an example that runs against the current implementation:

```js
// First tool call: the agent implements and enables a pointer recorder.
globalThis.samples = [];
globalThis.recordingError = null;
globalThis.inputSub = await wayland.onInput(
  {windowId, origin: "human", devices: ["pointer"]},
  event => {
    if (event.kind === "input") samples.push(structuredClone(event));
    else recordingError = event; // This minimal recorder rejects any gap/status.
  }
);
return "Recording enabled";
```

The agent then asks the user to "reproduce the bug" in the conversation and ends
its turn. The MCP session remains alive and listeners keep receiving events.
After the user indicates completion, another tool call stops collection:

```js
await inputSub.unsubscribe(); // Includes the delivery/drain barrier.
if (recordingError) throw new Error(JSON.stringify(recordingError));
globalThis.recording = structuredClone({
  initialPointer: inputSub.initialState.pointer,
  events: samples
});
return {eventCount: recording.events.length};
```

The agent writes its own replay function. This illustrative version assumes the
proposed event schema exposes `input` in the raw pointerEvent argument format and
a `surfaceId` that pointerEvent can target. The example assumes a known initial
pointer state with no pressed buttons and a stable target. It rejects unsupported
starting conditions instead of silently producing a different gesture. Broader
scripts can add target mapping and keyboard state.

```js
globalThis.replay = async function (targetWindowId, speed = 1) {
  if (!Number.isFinite(speed) || speed <= 0) throw new Error("Invalid speed");
  const events = recording.events;
  if (!events.length) return;
  const initial = recording.initialPointer;
  if (!initial || !initial.known || initial.buttons.length)
    throw new Error("Record from a known state with no buttons held");
  if (initial.surfaceId) {
    await wayland.pointerEvent({
      windowId: targetWindowId, surfaceId: initial.surfaceId,
      coordinateSpace: "surface-fixed",
      event: {type: "enter", x: initial.x, y: initial.y}
    });
  }
  const start = performance.now();
  const first = events[0].timestampMs;
  const held = new Map();
  try { for (const sample of events) {
    const due = (sample.timestampMs - first) / speed;
    await wayland.sleep(Math.max(0, start + due - performance.now()));
    const input = structuredClone(sample.input);
    delete input.serial;
    delete input.time;
    await wayland.pointerEvent({
      windowId: targetWindowId, surfaceId: sample.surfaceId,
      coordinateSpace: sample.coordinateSpace, event: input
    });
    if (input.type === "button") {
      if (input.state === 1) held.set(input.button, sample.surfaceId);
      else held.delete(input.button);
    }
  } } finally {
    for (const [button, surfaceId] of held) {
      await wayland.pointerEvent({
        windowId: targetWindowId, surfaceId,
        event: {type: "button", button, state: 0}
      });
      await wayland.pointerEvent({
        windowId: targetWindowId, surfaceId, event: {type: "frame"}
      });
    }
  }
};
await replay(windowId);
// Later tool calls may invoke replay(windowId) again, or edit the function/data.
```

Acceptance scenario, exercised through separate real `gui_console` calls:

1. The agent installs its own JS callback and receives confirmation before asking
   the user to reproduce the bug. The initial evaluation completes immediately.
2. The user moves, clicks, drags and scrolls in the target application while no
   evaluation is active. Include a pause longer than the foreground evaluation
   timeout and a switch to an unrelated host window. Target events accumulate;
   unrelated host input does not.
3. A later evaluation unsubscribes and snapshots the completed recording. Verify
   exact event order/count for deterministic fixtures and no changes after stop.
4. Agent-authored JS replays the same recording at least three times across
   separate evaluations, resetting the application using JS as appropriate. Check
   input delivery and resulting frames independently. Preserve motion, button and
   scroll ordering; measure timing tolerance rather than promise real-time replay.
5. Verify replay is model-origin, cannot select the human clipboard and does not
   feed a human-only listener. Exercise concurrent human input, event gaps, callback
   exceptions, cancellation, target destruction and restart; report incomplete
   recordings and release synthetic pressed state on aborted replay as appropriate.

The broker must maintain synthetic pressed-state bookkeeping as a fallback for
runtime death or cancelled in-flight injection, when JS `finally` cannot run.
Best-effort cleanup must target only synthetic state; never invent releases of
physical human keys. If human/model simultaneous presses cannot be represented
independently on the shared client seat, report that conflict and test the chosen
seat policy before claiming concurrent replay fidelity. Delivering protocol input
without moving the host cursor does not by itself solve shared client-seat state.

This validates the requested workflow without introducing an MCP recorder or
player. Live native validation has exercised agent-authored recording and three
replays, including exact relative-motion payloads; see docs/implementation-status.md.

## 7. Separate DMA-BUF advertisement from capture support — P2

Add a trusted startup setting with `capture-compatible` as the compatibility
default and `transparent` as an opt-in mode. In transparent mode preserve host
format/modifier tables, tranche ordering and indexes consistently across legacy
and feedback APIs; keep privacy filtering independent of format policy.

Report capture capability/errors per committed buffer. An unsupported format may
present successfully but must never be reported as successfully capturable.
Document that this mode preserves DMA-BUF negotiation, not every aspect of a
direct compositor connection or a guarantee of direct scanout.

Acceptance: replay host advertisement fixtures byte-for-byte where applicable in
transparent mode; verify consistent rewritten tables in capture-compatible mode.
Present a format unavailable to capture and check that observation fails clearly
without blocking presentation.

Deliver D1 as the policy/configuration switch plus legacy format/modifier event
fixtures, then D2 as feedback-table FD/tranche handling and inventory/capture
metadata. Extend existing `dmabuf_feedback_table_and_tranche_indices_are_filtered_together`
and unsupported-format tests. Keep format support, modifier/import support and
availability of a safe snapshot as separate reported facts; the current
fourcc-only predicate cannot certify a particular buffer capturable. Fix error
text that currently says unsupported formats are never advertised. Test selected
mode at startup and refuse in-place negotiation changes for already bound clients.

## 8. Extend compatibility through separate, gated projects — P2/P3

- **Portals (P2):** build a private session bus and test-specific portal backend
  for an initial file-chooser workflow using fixture files. Portal UI must connect
  through the proxy. Deny or explicitly report unsupported services; do not fall
  back to host portal dialogs. Validate that both the dialog and resulting file
  access stay in the test namespace.
  Start with one `FileChooser.OpenFile` fixture: a private service, a chooser
  window launched through the proxy, one allowed fixture file, and cancellation.
  Then test request handles/responses and URI access from the sandboxed client.
  Only after that add multiple-file/save flows; broad portal coverage is deferred.
- **Additional input (P3):** publish a capability matrix, then implement touch,
  tablet/pen, gestures, relative-pointer/constraints and text-input/IME as separate
  protocol-backed features. Validate each against purpose-built clients and real
  toolkit applications. Keep the current XKB and `unicode-hex` limitations explicit
  until an isolated input-method companion exists.
- **Programmable drawing and composition (P3, low priority):** expose in-memory
  capture images and scoped drawing/composition targets, with basic paths, strokes,
  image blits, transforms and alpha blending. Let agent JS implement overlays,
  mouse trails, crops, side-by-side frames and custom compositions. Allow an overlay
  associated with a selected proxied surface and a separate offscreen output;
  define whether each is included in capture and optionally shown to the human.
  Keep application-owned buffers intact by composing a separate layer. This is
  separate from faithful surface-tree capture in step 5. No filesystem, arbitrary
  URL loading or direct GPU/device access is needed in JS. Validate scoped access,
  coordinates, output pixels, resource cleanup and capture metadata that identifies
  agent-added layers.
  First deliver offscreen immutable capture handles plus draw/blit/export and
  disposal; verify a JS mouse-trail overlay against known pixels. Add a visible
  proxy-owned overlay only as a later prototype that proves stacking, no input
  interception and removal. Drawing directly into application-owned DMA-BUFs is
  outside this API's ownership contract.

## 9. Investigate private rootless Xwayland — P4, very low priority

Defer implementation until the native Wayland security, listener and capture
work is complete; this must not delay those milestones or low-priority drawing.
The first deliverable is a feasibility prototype and a supported-feature matrix,
not a promise to run arbitrary X11 applications.

Proposed path to investigate:

```text
X11 test clients → private Xwayland → local XWM / surface-role adapter
                → existing proxy, capture and input services → host compositor
```

Rootless Xwayland requires an X window manager integrated with its Wayland server.
Simply changing `WAYLAND_DISPLAY` does not supply window management or map its
surfaces into the proxy's current xdg-shell window model.
([Wayland Xwayland architecture](https://wayland.freedesktop.org/docs/book/Xwayland.html))

- Prototype an optional XWM/adapter module, preferably reusing an established XWM
  implementation if it can fit without replacing the proxy. Associate X11 window
  identities with Xwayland surfaces, and create the corresponding host-side
  toplevel/popup roles. Evaluate a separate rootless adapter process if this makes
  role translation simpler. Do not substitute a single nested desktop window.
- Terminate Xwayland-specific shell roles locally and translate roles on distinct
  upstream resources; do not assign incompatible roles to one surface. Resolve
  mapping/configure order, resize acknowledgments, focus, transient relationships,
  override-redirect menus, remapping and destruction before expanding scope.
- Expose `xwayland_shell_v1`, if used, only to the launched Xwayland connection,
  identified by a trusted connection capability. Ordinary proxied applications
  must not gain access by claiming an executable name or PID. This restricted
  exposure is also the model used by wlroots' Xwayland shell API.
  ([wlroots shell documentation](https://wlroots-baa023.pages.freedesktop.org/wlr/xwayland/shell.h.html))
- Launch with private X sockets and authentication, no TCP listener, only the
  proxy Wayland connection and explicitly granted render devices. Return private
  `DISPLAY`/`XAUTHORITY` values through trusted launch configuration; never inherit
  the host values. Isolate pathname and abstract X sockets and clean up on exit.
  Treat all X11 clients sharing this X server as one trust domain.
- Bridge X11 selections through the human/private clipboard policy, including
  asynchronous transfers and ownership changes. Keep unsupported PRIMARY/XDND
  routes disabled. Do not infer human origin from an X client claiming an event
  came from hardware; test client-generated input and selection requests explicitly.
- Reuse window-local capture and input listeners. The agent's JS recorder/replayer
  should work unchanged at the primitive API level. Preserve GPU buffer sharing
  for presentation; validate this rather than assuming the adapter is zero-copy.

Test in increasing scope:

| Stage | Fixture / environment | Required evidence |
| --- | --- | --- |
| Lifecycle and isolation | Private test session, missing Xwayland, invalid auth, two simultaneous instances, forced crashes | Clear optional-feature errors, distinct endpoints, host sockets inaccessible, no leaked processes/FDs/socket files, clean restart |
| Window mapping | Small XCB fixture with two toplevels, transient dialog, override-redirect menu and repeated map/unmap | Independent host windows and stable inventory lifecycle, correct titles/geometry/stacking, no nested desktop, correct association despite interleaved X11/Wayland messages |
| Capture correctness | Deterministic software-rendered patterns, resize and alpha fixtures | Expected pixels and capture dimensions for each window, menu/dialog visibility, correct screenshot-to-input coordinates |
| Human and model input | XCB event log plus separate `gui_console` calls | Independent input delivery, trusted origin, listener persistence while user reproduces a bug, agent-written replay succeeds at least three times |
| Clipboard | Separate host/private sentinels; two X11 clients and one native Wayland client | Human/model routing preserved across protocol boundaries, no host access from model-origin or ambiguous actions, correct MIME/selection conversion and large incremental transfers |
| GPU presentation | Small GLX/EGL fixture, later Vulkan XCB/XLIB fixture, on a real GPU | Report actual renderer, verify hardware path and buffer/synchronization forwarding, no added CPU readback/upload in presentation; capture may read back separately |
| Application compatibility | Available GTK and Qt applications explicitly forced to X11, then a representative X11-only app | Confirm X11 connection, exercise menus, text, scrolling, dialogs, resize and close; record supported versions and remaining gaps |

Use Unix-socket fixtures for deterministic protocol/software checks and
a separate real-compositor/GPU run for visible-window and hardware assertions.
Xvfb alone cannot validate the Xwayland-to-proxy path. Once the prototype works,
run against at least two compositor families to expose host-specific assumptions.
Report missing GPU or Xwayland prerequisites as untested, never as a passing test.

Proceed beyond the prototype only if independent real windows, scoped input and
capture, clipboard policy and the presentation path all work without weakening
native Wayland isolation. Otherwise document the blocker and keep Xwayland
explicitly unsupported. No Xwayland implementation or live tests have run as part
of this planning work.

## Delivery and validation

Use the named slices as reviewable changes, not one PR per numbered section.
Each PR must name its fixture and failure behavior; do not enable an incomplete
security boundary by default. New protocol families stay denied until their
policy and lifetime handling are ready. Existing behavior changes (sandbox
prerequisites, backend selection, clipboard and snapshot availability) must be
documented in the same PR, without silently falling back to the old unsafe path.

| Delivery stage | Slices / dependencies | Deliverable |
| --- | --- | --- |
| Foundation | F1; then F2. F3's remaining message metadata can proceed independently of F2; build-time generation is implemented. | Deterministic wire tests, single ordered writer, build-time metadata from installed system protocols. |
| Console containment | J1; J2 after F1; J3 after J1/J2. | Node permission configuration, responsive bounded broker, persistent session ownership and cancellation. |
| Protocol boundary | G1 after F1/F3. | Explicit policy enforced at registry, bind and mediated request paths. |
| Input/provenance | E1 after F2; E2/E3 after E1/J2/J3; E4 after E1; E5 after E2/E3/E4. | Agent-written record/stop/replay across calls. Does not depend on full surface composition. |
| Clipboard | C1 after F2/F3/G1; C2 after C1; C3 after C2/E1; C4 after C3. | Two independent clipboard domains selected by the latest trusted input source. |
| Restricted launch | Reference profile after J1/F1, GPU extension after the SHM launch succeeds. | Tested launch recipes; this is required for application-isolation claims, not for all externally launched clients. |
| Faithful observation | S1 after F1/F3; S2 after S1/F2; S3 after S2; S4 prototype after F2/C1; S5 after S3/S4. | Committed SHM tree first; synchronized GPU tree and mixed-color/popup capture later. |
| Later compatibility | D1/D2 after G1; portal fixture after restricted launch and clipboard policy; additional input extends E1/E4. | Individually testable features with a capability matrix. |
| Low / very low priority | Offscreen drawing, then visible-overlay prototype; Xwayland feasibility last (P4). | No dependency from native milestones to either investigation. |

The protocol generator, writer, Node permissions, listeners, clipboard selection,
input extensions and committed-tree compositor are implemented. Current status
and outstanding validation are recorded in docs/implementation-status.md. DMA-BUF S5 and Xwayland implementation are conditional on their experiments. A failed
experiment is useful evidence but is not completion of the requested feature.

| Decision gate | Evidence to produce | Outcome if it fails |
| --- | --- | --- |
| J1: runtime containment | Saved runtime manifest, Node permission configuration, versions and negative-access results plus timer/state smoke test. | Stop claiming isolated console; propose a different no-general-I/O JS runtime without sacrificing programmability. |
| S4: GPU lifetime | Real-GPU repeated-reuse test and trace of host completion, observation-copy completion and client release for each commit. | Retain safe supported captures; report unsupported/uncached snapshots. Do not claim full GPU tree capture or silently read released buffers. |
| Xwayland feasibility | Two independently visible X11 windows, input/capture proof, role/lifecycle trace and presentation-path measurements. | Keep optional Xwayland disabled/unsupported and publish the blocker. |

For each change, add focused regression tests and update `README.md`, console help
and configuration documentation together. Run `./scripts/validate.sh` (format,
Clippy, unit tests and release build), plus the relevant protocol/security fixtures.
Keep pure state/wire tests in `cargo test --locked`; place sandbox/compositor/GPU
checks behind an explicit integration runner that returns passed/failed/untested
with the prerequisite reason. Missing dependencies must not produce an apparent
pass. Extend the existing FD, synthetic-selection, viewport-coordinate, JS timeout,
DMA-BUF filtering and color tests rather than replacing them with duplicate suites.

For E5, automated tests use the fake host to emit a known input sequence across
separate console calls, with a short test-configured evaluation timeout and a
longer idle gap. A real desktop run validates the actual user prompt/wait/replay
workflow. The automated test cannot stand in for the human-focus check. Add a
1000-Hz input fixture, multiple bound pointer resources, subpixel positions and
an initially focused window. Test timing with declared tolerances and record
observed scheduling error; do not use tight wall-clock assertions in unit tests.

Use separate integration runs for a supported desktop compositor, GTK/Qt, a
browser, SHM and Vulkan/DMA-BUF clients. Record compositor/runtime/driver versions,
selected features, commands, expected/actual results and which hardware checks
actually ran in a milestone report. Synthetic secrets stay in fixtures. Do not
capture a real user's clipboard or unrelated host content for validation.

Final review must map every feedback item to implementation evidence or an explicit
deferred limitation. Feedback items 4 (GPU presentation architecture) and the virtual
capture-output observation need regression coverage/documentation, not a redesign.
Do not claim support for literally arbitrary Linux GUIs or unrestricted host
confidentiality from protocol fixes alone.

Plan-review validation: checked relevant control flow and data structures in the
source at the baseline, found and repaired the missing-generator/build-artifact
problem, verified the coordinate/API gaps, and checked dependency ordering and
sample syntax. The remaining feature review is design validation, not
evidence that the future sandbox, clipboard classifier or compositor works. Their
completion requires the slice-specific tests and gate evidence above.

Build repair validation on 2026-09-27: ran `./scripts/validate.sh` with
`CARGO_TARGET_DIR` set to a newly created, empty directory. Formatting, Clippy
with warnings denied, all 74 tests and the optimized release build passed using
installed Wayland 1.26.0 and wayland-protocols 1.49 XML. Also verified deterministic
generation across working directories, ten `/usr/share` XML dependencies recorded
by Cargo, a clear failure for missing XML, and absence of both generated Rust in
`src/` and vendored XML. This validates the build repair only; it does not claim
the planned GUI/security features have been implemented.
