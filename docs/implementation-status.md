# Local Wayland capabilities

The native Wayland proxy presents applications on the host compositor and
restricts console I/O with Node permissions. See the [README](../README.md)
for SSH workflows, build requirements, and the console API.

| Capability | Implementation |
| --- | --- |
| Persistent console, timers, listeners | Bounded broker, Node I/O restrictions, cancellation and watchdog |
| Pointer and keyboard | Human and model input attribution, surface tokens and coordinates |
| Touch | Down, up, motion, frame, cancel, shape and orientation, with scoped recording and model injection |
| Relative pointer | Protocol forwarding, recording and model injection |
| Pointer capture | Lock and confinement protocols, model activation and implicit grab routing |
| Clipboard | Independent host and private selections routed by the latest trusted input source |
| Drag and drop | Separate private and host offers, actions, MIME negotiation and bounded transfers |
| GPU capture | Owned DMA-BUF snapshots, explicit acquire waits and idle implicit release leases |
| Surface tracking | Synchronized and desynchronized subsurfaces, committed geometry, stacking, viewports, popups and input regions |
| DMA-BUF negotiation | `capture-compatible` or `transparent` startup mode with matching format tables and tranches |

Production snapshots use DMA-BUF pixel capture. `captureNextFrame` arms
observation before a future commit. An owned snapshot remains readable after
the producer's buffer is released. `beginObservation` retains future frames
until its lease expires or `endObservation` ends it. Concurrent leases are
independent. Empty surface commits preserve owned GPU pixels.

Tablet input, gestures, input methods, portals, a restricted application launcher,
and rootless Xwayland remain unsupported. See the
[Xwayland feasibility notes](xwayland-status.md).

On a Wayland desktop with Vulkan and an accessible DRM render node, run the
release capture probe. The probe requires `vkcube` and Pillow.

```sh
cargo build --locked --release
python3 tests/visual_program/live_capture_probe.py
```

The probe opens a Vulkan window through the proxy, captures in-memory images,
and checks that both visual observation APIs report changes as the window renders.
