# Implementation status — 2026-09-27

The native Wayland implementation keeps application presentation on the host
compositor and restricts console IO with Node permissions. Protocol bindings are
generated during the Cargo build from installed `/usr/share` XML into `OUT_DIR`.
The generated Rust file is a build artifact; it is absent from `src/` and ignored.
There is no external sandbox dependency or kernel enforcement for Node.

| Capability | Implementation | Live validation |
| --- | --- | --- |
| Persistent console, timers, listeners | Bounded broker, permission-denied IO, cancellation and watchdog | Constructor escape cannot open files/sockets/processes; state and callbacks persist |
| Pointer/keyboard | Independent human/model attribution; exact surface tokens and coordinates | Visible GTK pointer input and private clipboard exchange |
| Touch | Down/up/motion/frame/cancel/shape/orientation; scoped recording and model injection | Two contacts reached native callbacks and GTK touch event handling; physical touchscreen not exercised |
| Relative pointer | Protocol forwarding, recording, exact model injection | Real human motion; 80 samples replayed three times with 240 exact model events |
| Pointer capture | Lock/confinement protocols, model activation, confinement and implicit grab routing | Visible client lock and relative delivery; real compositor confinement activation |
| Clipboard | Independent host/human and private/model selections; current trusted source chooses route | Visible native copy/paste plus real FD transfer validation |
| Drag/drop | Separate private/host offers, actions, MIME negotiation, cancellation/finish, bounded pump | Visible model GTK/Vulkan drop completed through the private route; real human drops also reached GTK |
| SHM composition | Owned committed snapshots, synchronized/desynchronized subsurfaces, stacking, crop/scale/transform/alpha, input regions | Visible translucent subsurface composed over GTK content |
| GPU composition | Owned linear snapshots copied before commit forwarding; explicit acquire waits; idle implicit release leases | OpenGL and Vulkan GTK root surfaces composed with a SHM subsurface; Vulkan explicit acquire/release requests exercised |
| Popups | Parent association, compositor configure positions, commit-latched geometry, tree bounds and hit testing | Visible Vulkan popup captured outside parent bounds alongside the translucent SHM subsurface |
| DMA-BUF negotiation | Startup `capture-compatible` or `transparent` mode; format tables/tranches kept consistent | Visible Vulkan client presented and captured in both modes; byte-for-byte transparent feedback comparison remains unverified |
| Tablet, gestures, IME | Not implemented | Not exercised |
| Portals and restricted application launcher | Not implemented; external client isolation remains unverified | No application-isolation claim |
| Agent drawing API | Deferred low-priority extension | Not exercised |
| Rootless Xwayland | Not implemented; ordinary clients cannot bind its privileged shell | Feasibility notes in `xwayland-status.md`; not exercised |

GPU snapshots never certify an uncaptured, already released buffer as readable.
`captureNextFrame` arms observation before a future commit. A valid owned snapshot
can be returned after release; an uncaptured one reports `snapshot_unavailable`.
`beginObservation` retains future frames across a sequence of actions until its
lease expires or `endObservation` ends it. Concurrent leases are independent.
Empty surface commits preserve owned GPU pixels rather than reading released
client storage again.
Copying before forwarding preserves ordering without replacing explicit release
timelines. Implicit idle copying holds release until copying finishes, including
when the caller cancels. Readback runs outside the shared server lock, with two
concurrent GPU copies at most. Driver hangs are not validated by the live runs.

Reproduce the visible workflow on the current desktop:

```sh
cargo build
python3 scripts/demonstrate-live.py --input
python3 scripts/demonstrate-live.py --input --gl
python3 scripts/demonstrate-live.py --input --vulkan
python3 scripts/demonstrate-live.py --input --vulkan --transparent
```

The window remains open. The script prints its control directory; writing `code.js`
there evaluates ordinary agent-authored JS through the actual MCP and stores its
result in `result.json`. A `stop` file closes only that demonstration's processes.
The script generates the C extension bindings in its temporary build directory.

Validation environment: Node 26.10.0, Wayland client 1.26.0, installed
wayland-protocols 1.49, GTK 4.22.5. The automated checks are supplemental to the
visible native application runs and do not substitute for them.
