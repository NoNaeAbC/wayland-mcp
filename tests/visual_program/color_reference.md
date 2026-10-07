# GPU color-normalization reference probe

`color_reference.comp` validates known pixels in the output of the trusted
normalizer. Its only image input is a read-only `rgba16f` image already
normalized to **linear BT.2020, straight alpha**. The first run uses opaque
source pixels and an explicit sRGB source assumption. A fixture generator
should paint encoded source samples directly on the GPU; no CPU frame upload or
readback is needed.

The parameter buffer at binding 6 starts with `uvec4 metadata`:

1. Number of checks, from 1 through 32.
2. Normalized image width.
3. Normalized image height.
4. Sequence number echoed into the result.

It is followed by one 48-byte record per check: `uvec4 coordAndBit` containing
pixel x, pixel y, bit index (equal to the array index), and a reserved word;
then `vec4 expectedLow` and `vec4 expectedHigh`. Each RGBA component is checked
with inclusive bounds, which allows a small fp16 tolerance to be expressed in
the CPU-authored reference table.

The 16-byte result at binding 5 contains only `failedMask`, `checkedMask`,
`sequence`, and a zero reserved word. Clear it before dispatch. The shader
checks at most 32 points and emits no image samples, color values, or debug
arrays. A successful run has equal `checkedMask` and the expected test mask,
zero `failedMask`, the supplied sequence, and zero reserved data.

## Canonical source fixture

Use a GPU-generated 64x32 opaque RGBA source under the sRGB transfer function
and Rec. 709/sRGB primaries. Reserve pixels `(0..8, 0)` for these encoded source
colors: black, white, red, green, blue, sRGB gray 64, gray 128, gray 192, and
the mixed midtone `(64,128,192)` (8-bit channel values). Compare the normalized
pixels at those exact coordinates. The mix exercises both the inverse sRGB
transfer and the primary conversion; the gray levels exercise the nonlinear
transfer curve away from black and white.

For a common linear-Rec.709-to-linear-BT.2020 matrix, approximate RGB centers
are:

| Source sRGB | Expected normalized linear BT.2020 RGB (approx.) |
| --- | --- |
| black `(0,0,0)` | `(0,0,0)` |
| white `(255,255,255)` | `(1,1,1)` |
| red `(255,0,0)` | `(0.627404,0.069097,0.016392)` |
| green `(0,255,0)` | `(0.329282,0.919540,0.088013)` |
| blue `(0,0,255)` | `(0.043314,0.011361,0.895595)` |
| gray 64 | `(0.051269,0.051269,0.051269)` |
| gray 128 | `(0.215861,0.215861,0.215861)` |
| gray 192 | `(0.527115,0.527115,0.527115)` |
| `(64,128,192)` | `(0.126077,0.208024,0.491921)` |

The reference table should use a small absolute tolerance such as `0.002` per
component around these centers, including alpha near 1.0. The fixture and
normalizer must use the same explicitly selected conversion matrix; if the
implementation selects different valid matrix coefficients, update the CPU
reference table from that declared matrix rather than widening tolerances.
No alpha compositing is expected in this first opaque-source test.

The test detects common failures: treating sRGB values as already linear,
skipping the Rec.709-to-BT.2020 primary conversion, swapping channels,
incorrect range/format interpretation, modifying opaque alpha, or using a
tolerance too tight for RGBA16F rounding.

## Alpha-mode and byte-order runs

Run these cases separately because alpha mode is a source-level normalization
parameter. In straight-alpha mode, RGB is not divided by alpha. In the
encoded-premultiplied mode, unpremultiply encoded RGB by alpha before applying
the inverse sRGB transfer function, then emit straight-alpha linear BT.2020.

| Run | Encoded source RGBA8 | Normalized expected RGBA (approx.) |
| --- | --- | --- |
| straight alpha, fully transparent red | `(255,0,0,0)` | `(0,0,0,0)` |
| encoded-premultiplied, half-alpha red | `(128,0,0,128)` | `(0.627404,0.069097,0.016392,0.501961)` |
| encoded-premultiplied, half-alpha green | `(0,128,0,128)` | `(0.329282,0.919540,0.088013,0.501961)` |
| encoded-premultiplied, half-alpha blue | `(0,0,128,128)` | `(0.043314,0.011361,0.895595,0.501961)` |

Use the same per-component `0.002` tolerance for RGB and a tolerance of
`1/255` for the half-alpha output. The normalizer maps fully transparent pixels
to transparent black in both alpha modes, so undefined RGB beneath zero alpha
cannot affect observer results.

Repeat the opaque canonical-color run once with RGBA byte order and once with
BGRA byte order, placing the same logical colors at the same coordinates. The
normalized logical RGBA values and result masks must match between runs. This
checks channel-order handling independently from color conversion.
