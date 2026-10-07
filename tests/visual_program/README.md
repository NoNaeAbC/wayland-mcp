# Generic two-pass color-target probe

These shaders find a color target in an ROI of a read-only linear BT.2020
RGBA16F image. They are intentionally independent of any application. Pass 1
classifies pixels and atomically reduces a bounding box into scratch. Pass 2
uses that reduction and the previous GPU state to write one fixed-size result.
The source can be passed to an in-memory compiler or compiled and embedded at
build time; neither pass reads shader files at runtime.

## Descriptor and parameter contract

The descriptor set uses the common layout:

| Binding | Type | Use |
| --- | --- | --- |
| 0 | readonly `rgba16f image2D` | Current frame |
| 1 | readonly `rgba16f image2D` | Previous frame; read only when `historyValid != 0` |
| 2 | readonly storage buffer | One `uint wasPresent` from the prior state |
| 3 | writable storage buffer | One `uint isPresent` for the next state |
| 4 | writable/read-only storage buffer | Eight-word scratch reduction |
| 5 | writable storage buffer | Eight-word, 32-byte result |
| 6 | readonly storage buffer | Parameters below |

`reduce_source` reads the one-word presence state at binding 2 and writes the
next one-word state at binding 3. `reduce_feedback_source` instead reads the
previous 32-byte result/state record at binding 2 and writes only the current
32-byte result at binding 5. In feedback mode the framework copies that result
into the next 32-byte state slot on the GPU after pass 2, then swaps slots for
the next frame; binding 3 is unused. Feedback is simpler when every state field
is also part of the result. Separate state remains useful when internal
accumulators or trackers differ from the public result record.

The parameter buffer is `vec4 targetLow`, `vec4 targetHigh`, `uvec4 roi`
(`x, y, width, height`), then four `uint`s (`minPixels` plus three reserved
words). Low/high values use the same linear BT.2020 space as the images. A
match means all four RGBA components are inside the inclusive range.

Push constants are four `uint`s: `frameWidth`, `frameHeight`, `sequence`, and
`historyValid`. The framework should validate that the ROI is inside both
images and that integer products/dispatch dimensions are safe. The previous
image descriptor still needs a valid image when history is invalid; binding
the current image as a harmless fallback is sufficient because the shader
does not read the previous descriptor in that case.

Scratch is zeroed before each pass-1 dispatch. Pass 1 stores minimum x/y as
`max(frameDimension - 1 - coordinate)` so zero is a safe empty value, and
stores maximum x/y directly. Pass 2 decodes the bounds only when the match
count reaches `max(minPixels, 1)`. Scratch counts never leave the GPU.

The result is exactly eight `uint32`s (32 bytes):

1. `flags`
2. `minX`
3. `minY`
4. `maxX`
5. `maxY`
6. `sequence`
7. `reserved0` (always zero)
8. `reserved1` (always zero)

Flag bits are `PRESENT=1`, `ENTERED=2`, `EXITED=4`, `MASK_CHANGED=8`, and
`HISTORY_VALID=16`. Bounds are inclusive full-frame coordinates and are zero
when `PRESENT` is clear. The first frame can report `PRESENT`, but suppresses
edge/change bits until history is valid. `MASK_CHANGED` means at least one ROI
pixel changed membership in the target-color mask compared with the previous
frame; it does not mean the aggregate presence bit changed.

Each frame, the host/framework clears scratch and the result, dispatches pass
1, inserts a compute storage barrier, dispatches pass 2, then waits for
completion before reading the result or swapping `statePrev` and `stateNext`.
Pass 2 writes all result words and the next presence state, so result flags
cannot leak from the prior frame. Reset state/history on first frame, geometry,
source, color-profile, device, or program changes. Parameter changes should
invalidate history by default; callers may preserve it only when target-range
semantics remain compatible.

## Minimal generic JS result handling

The result callback should decode only the fixed 32-byte record and emit
application-neutral records such as `{type: "target-entered", bounds, frame}`,
`{type: "target-left", frame}`, or `{type: "target-present", bounds, frame}`.
The callback may then run the caller's ordinary input policy. It should never
receive, retain, or log frame pixels, per-pixel masks, or the scratch counts.
`result_callback.mjs` provides a small `makeOnResult({emit})` adapter for the
32-byte contract; it emits no frame sequence or scratch fields.

## Test cases

- A GPU-generated 64x32 RGBA16F sequence with the inclusive green range
  `low=(0.19,0.79,0.29,0)`, `high=(0.21,0.81,0.31,1)`: empty, one pixel at
  `(7,9)`, same pixel, moved pixel at `(11,12)`, empty, empty, then a reset
  epoch with the target present. Expect present bounds at each nonempty frame,
  entered only when valid prior state was absent, mask-changed on appear/move/
  disappear, exited on the first empty frame after presence, and no edge flags
  on the reset frame while history is invalid.
- No matching pixels: `PRESENT` clear, bounds zero, sequence echoed, reserved
  words zero.
- One exact target pixel and a rectangular target patch: correct inclusive
  bounds and present flag at `minPixels` and just above the observed count.
- Two separated patches: one union bounding box with the expected outer bounds.
- ROI edge and out-of-ROI matching pixels: only in-ROI pixels contribute.
- RGBA values exactly at each inclusive range edge and just outside it.
- First frame with a present target and `historyValid=0`: present set, history
  and edge flags clear; next valid stable frame has no entered/left edge.
- Target appears, persists, then disappears: exactly one `ENTERED` transition,
  no repeated edges while stable, then one `EXITED` transition.
- A single target pixel changes membership between consecutive frames:
  `MASK_CHANGED` set even when aggregate presence remains present.
- Empty ROI contents after a previously present frame: zero bounds and exited
  flag, with no stale min/max or edge flags from the preceding result.
- Reuse buffers over many frames with alternating presence to catch stale
  result bits, scratch accumulation, and incorrect state ping-pong swaps.
- Reset after dimensions, source, color profile, target-range parameter,
  device, or program changes; invalid history must not create false edges.
