//! Surface color descriptions and deterministic SDR screenshot rendering.
use crate::gui_wayland_generated::GeneratedHookRequest as Request;
use std::collections::HashMap;

// Named primaries from wp_color_manager_v1 / ITU-T H.273. Coordinates
// use millionths; tolerate one unit of client rounding. XYZ uses equal-energy white.
const NAMED_PRIMARIES: [[i32; 8]; 10] = [
    [
        640000, 330000, 300000, 600000, 150000, 60000, 312700, 329000,
    ],
    [
        670000, 330000, 210000, 710000, 140000, 80000, 310000, 316000,
    ],
    [
        640000, 330000, 290000, 600000, 150000, 60000, 312700, 329000,
    ],
    [
        630000, 340000, 310000, 595000, 155000, 70000, 312700, 329000,
    ],
    [
        681000, 319000, 243000, 692000, 145000, 49000, 310000, 316000,
    ],
    [
        708000, 292000, 170000, 797000, 131000, 46000, 312700, 329000,
    ],
    [1000000, 0, 0, 1000000, 0, 0, 333333, 333333],
    [
        680000, 320000, 265000, 690000, 150000, 60000, 314000, 351000,
    ],
    [
        680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
    ],
    [
        640000, 330000, 210000, 710000, 150000, 60000, 312700, 329000,
    ],
];
fn named_primaries(xy: [i32; 8]) -> u32 {
    NAMED_PRIMARIES
        .iter()
        .position(|known| xy.iter().zip(known).all(|(a, b)| a.abs_diff(*b) <= 1))
        .map_or(0, |i| i as u32 + 1)
}

// RGB-to-XYZ matrices derived from the chromaticities above, Bradford adapted
// from each source white to D65, then XYZ-to-linear-sRGB. Precomputed once;
// no allocation or matrix inversion occurs in the pixel loop.
#[allow(clippy::excessive_precision)]
fn primary_conversion(primary: u32) -> ([[f32; 3]; 3], [f32; 3]) {
    match primary {
        1 => (
            [
                [1.000000000, -0.000000000, -0.000000000],
                [0.000000000, 1.000000000, 0.000000000],
                [0.000000000, -0.000000000, 1.000000000],
            ],
            [0.212639006, 0.715168679, 0.072192315],
        ),
        2 => (
            [
                [1.486156846, -0.403554906, -0.082601940],
                [-0.025101109, 0.954024686, 0.071076423],
                [-0.027224002, -0.044095233, 1.071319235],
            ],
            [0.298966618, 0.586421210, 0.114612172],
        ),
        3 => (
            [
                [1.044043209, -0.044043209, -0.000000000],
                [-0.000000000, 1.000000000, 0.000000000],
                [0.000000000, 0.011793378, 0.988206622],
            ],
            [0.222004310, 0.706654766, 0.071340924],
        ),
        4 => (
            [
                [0.939542064, 0.050181357, 0.010276579],
                [0.017772223, 0.965792862, 0.016434914],
                [-0.001621600, -0.004369750, 1.005991350],
            ],
            [0.212376361, 0.701059857, 0.086563782],
        ),
        5 => (
            [
                [1.346175919, -0.339195075, -0.006980844],
                [-0.047351020, 1.066051531, -0.018700511],
                [-0.021664982, -0.061313102, 1.082978084],
            ],
            [0.253585363, 0.678335776, 0.068078861],
        ),
        6 => (
            [
                [1.660491002, -0.587641139, -0.072849863],
                [-0.124550475, 1.132899897, -0.008349423],
                [-0.018150763, -0.100578898, 1.118729661],
            ],
            [0.262700212, 0.677998072, 0.059301716],
        ),
        7 => (
            [
                [3.146657654, -1.666463755, -0.480193900],
                [-0.995521606, 1.955756734, 0.039764872],
                [0.063594803, -0.214563222, 1.150968419],
            ],
            [0.000000000, 1.000000000, 0.000000000],
        ),
        8 => (
            [
                [1.157516406, -0.154962378, -0.002554028],
                [-0.041500072, 1.045567923, -0.004067852],
                [-0.018050039, -0.078578273, 1.096628312],
            ],
            [0.209491678, 0.721595254, 0.068913068],
        ),
        9 => (
            [
                [1.224940176, -0.224940176, -0.000000000],
                [-0.042056955, 1.042056955, 0.000000000],
                [-0.019637555, -0.078636046, 1.098273600],
            ],
            [0.228974564, 0.691738522, 0.079286914],
        ),
        10 => (
            [
                [1.398355744, -0.398355744, -0.000000000],
                [-0.000000000, 1.000000000, 0.000000000],
                [0.000000000, -0.042928989, 1.042928989],
            ],
            [0.297344975, 0.627363566, 0.075291458],
        ),
        _ => unreachable!("validate color description before conversion"),
    }
}

type PrimaryConversion = ([[f32; 3]; 3], [f32; 3]);
type Matrix = [[f64; 3]; 3];

fn multiply(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn transform(a: Matrix, b: [f64; 3]) -> [f64; 3] {
    a.map(|row| row.iter().zip(b).map(|(x, y)| x * y).sum())
}

fn inverse(a: Matrix) -> Result<Matrix, String> {
    let cofactors = std::array::from_fn::<_, 3, _>(|i| {
        std::array::from_fn::<_, 3, _>(|j| {
            a[(i + 1) % 3][(j + 1) % 3] * a[(i + 2) % 3][(j + 2) % 3]
                - a[(i + 1) % 3][(j + 2) % 3] * a[(i + 2) % 3][(j + 1) % 3]
        })
    });
    let det: f64 = a[0].iter().zip(cofactors[0]).map(|(x, y)| x * y).sum();
    if !det.is_finite() || det.abs() < 1e-15 {
        return Err("degenerate custom color primaries matrix".into());
    }
    Ok(std::array::from_fn(|i| {
        std::array::from_fn(|j| cofactors[j][i] / det)
    }))
}

// Build once when set_primaries arrives, never once per pixel. Homogeneous
// XYZ columns support imaginary primaries and y=0 (including CIE XYZ).
fn custom_primary_conversion(xy: [i32; 8]) -> Result<PrimaryConversion, String> {
    let xy = xy.map(|v| f64::from(v) / 1_000_000.0);
    if xy[7] <= 0.0 {
        return Err("invalid custom color white point: y must be positive".into());
    }
    let white = [xy[6] / xy[7], 1.0, (1.0 - xy[6] - xy[7]) / xy[7]];
    let basis = [
        [xy[0], xy[2], xy[4]],
        [xy[1], xy[3], xy[5]],
        [
            1.0 - xy[0] - xy[1],
            1.0 - xy[2] - xy[3],
            1.0 - xy[4] - xy[5],
        ],
    ];
    let scales = transform(inverse(basis)?, white);
    let rgb_xyz: Matrix = std::array::from_fn(|i| std::array::from_fn(|j| basis[i][j] * scales[j]));
    // Bradford chromatic adaptation to the sRGB D65 white point.
    let bradford = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let source_cones = transform(bradford, white);
    let target_cones = transform(
        bradford,
        [0.3127 / 0.3290, 1.0, (1.0 - 0.3127 - 0.3290) / 0.3290],
    );
    if source_cones.iter().any(|v| v.abs() < 1e-15) {
        return Err("degenerate custom color white point for Bradford adaptation".into());
    }
    let scaled_bradford: Matrix =
        std::array::from_fn(|i| bradford[i].map(|v| v * target_cones[i] / source_cones[i]));
    let adaptation = multiply(inverse(bradford)?, scaled_bradford);
    let xyz_srgb = [
        [3.2409699419045226, -1.537383177570094, -0.4986107602930034],
        [-0.9692436362808796, 1.8759675015077202, 0.04155505740717559],
        [
            0.05563007969699366,
            -0.20397695888897652,
            1.0569715142428786,
        ],
    ];
    let matrix = multiply(xyz_srgb, multiply(adaptation, rgb_xyz)).map(|row| row.map(|v| v as f32));
    let weights = rgb_xyz[1].map(|v| v as f32);
    if !matrix
        .iter()
        .flatten()
        .chain(weights.iter())
        .all(|v| v.is_finite())
    {
        return Err("non-finite custom color conversion".into());
    }
    Ok((matrix, weights))
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ColorDescription {
    tf: u32,
    primaries: u32,
    power: Option<u32>,
    chromaticities: Option<[i32; 8]>,
    custom_conversion: Option<Result<PrimaryConversion, String>>,
    luminances: Option<(f32, f32, f32)>,
    extended: bool,
}

impl ColorDescription {
    pub(crate) fn metadata(&self) -> serde_json::Value {
        let (min, max, reference) = self.levels();
        let hdr = self.is_hdr();
        serde_json::json!({
            "source_transfer": self.tf,
            "source_primaries": self.primaries,
            "source_transfer_power": self.power.map(|e| f64::from(e) / 10000.0),
            "source_primaries_xy": self.chromaticities.map(|xy| xy.map(|v| f64::from(v) / 1_000_000.0)),
            "source_transfer_name": if self.power.is_some() { "custom power" } else { match self.tf {
                1 => "BT.1886", 2 => "gamma 2.2", 3 => "gamma 2.8",
                4 => "SMPTE ST 240", 5 => "extended linear", 6 => "log 100:1",
                7 => "log 316:1", 8 => "xvYCC", 9 => "sRGB (deprecated)",
                10 => "extended sRGB (deprecated)", 11 => "ST 2084 PQ",
                12 => "SMPTE ST 428", 13 => "HLG", 14 => "compound power 2.4",
                _ => "unknown",
            }},
            "source_primaries_name": match self.primaries {
                0 if self.chromaticities.is_some() => "custom chromaticities",
                1 => "sRGB / BT.709",
                2 => "PAL-M / BT.470 M",
                3 => "PAL / BT.601 625",
                4 => "NTSC / BT.601 525",
                5 => "generic film",
                6 => "BT.2020",
                7 => "CIE 1931 XYZ",
                8 => "DCI P3",
                9 => "Display P3",
                10 => "Adobe RGB",
                _ => "unknown",
            },
            "source_luminance_cd_m2": {"minimum": min, "maximum": max, "reference_white": reference},
            "output_colorspace": "sRGB", "output_dynamic_range": "SDR", "output_bit_depth": 8,
            "tone_mapped": hdr,
            "tone_mapper": if hdr { "luminance Reinhard, Y/(1+Y), relative to source reference white" } else { "none" },
            "notice": if hdr {
                "This image is an SDR sRGB tone-mapped preview, NOT the original HDR window. The image-delivery path to the OpenAI model uses an SDR PNG; native HDR color interpretation is not documented by OpenAI. Original absolute brightness, wide-gamut colors, and HDR highlight appearance cannot be judged from this preview."
            } else { "Source pixels have been converted to SDR sRGB for this screenshot." }
        })
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(power) = self.power {
            if !(10000..=100000).contains(&power) {
                return Err(format!(
                    "invalid screenshot transfer power exponent {power}/10000 (expected 1..=10)"
                ));
            }
        } else if !(1..=14).contains(&self.tf) {
            return Err(format!(
                "unknown or missing screenshot transfer function {}",
                self.tf
            ));
        }
        if let Some(conversion) = &self.custom_conversion {
            conversion.as_ref().map_err(Clone::clone)?;
        } else if !(1..=10).contains(&self.primaries) {
            return Err(format!(
                "unknown or missing screenshot color primaries {}",
                self.primaries
            ));
        }
        let (min, max, reference) = self.levels();
        if !(min.is_finite()
            && max.is_finite()
            && reference.is_finite()
            && min >= 0.0
            && max > min
            && reference > min)
        {
            return Err("invalid screenshot color luminance range".to_string());
        }
        Ok(())
    }

    fn levels(&self) -> (f32, f32, f32) {
        let (min, max, reference) = self.luminances.unwrap_or(if self.tf == 11 {
            (0.005, 10000.005, 203.0)
        } else if self.tf == 13 {
            (0.005, 1000.0, 203.0)
        } else if self.tf == 1 {
            (0.01, 100.0, 100.0)
        } else {
            (0.2, 80.0, 80.0)
        });
        (
            min,
            if self.tf == 11 { min + 10000.0 } else { max },
            reference,
        )
    }

    fn is_hdr(&self) -> bool {
        let (min, max, reference) = self.levels();
        self.extended || (max - min) / reference > 1.01
    }

    fn decode(&self, v: f32) -> f32 {
        if let Some(power) = self.power {
            v.signum() * v.abs().powf(power as f32 / 10000.0)
        } else {
            decode_transfer(self.tf, v)
        }
    }

    pub(crate) fn rgba8(&self, pixel: [f32; 4]) -> [u8; 4] {
        let alpha = pixel[3].clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return [0; 4];
        }
        let (min, max, reference) = self.levels();
        let swing = if self.tf == 11 { 10000.0 } else { max - min };
        let electrical = [pixel[0] / alpha, pixel[1] / alpha, pixel[2] / alpha];
        let (matrix, luminance_weights) = match &self.custom_conversion {
            Some(Ok(conversion)) => *conversion,
            Some(Err(_)) => unreachable!("validate color description before conversion"),
            None => primary_conversion(self.primaries),
        };
        let rgb = if self.tf == 13 {
            // BT.2100 HLG includes the RGB-coupled OOTF, not only inverse OETF.
            let gamma = (1.2 + 0.42 * (max / 1000.0).log10()).max(1.0);
            let beta = (3.0 * (min / max).powf(1.0 / gamma)).sqrt();
            let scene = electrical.map(|v| decode_transfer(13, ((1.0 - beta) * v + beta).max(0.0)));
            let y = scene
                .iter()
                .zip(luminance_weights)
                .map(|(v, w)| v * w)
                .sum::<f32>()
                .max(0.0);
            let scale = max / reference * y.powf(gamma - 1.0);
            scene.map(|v| v * scale)
        } else {
            electrical.map(|v| {
                if self.tf == 1 {
                    let black = min.powf(1.0 / 2.4);
                    ((max.powf(1.0 / 2.4) - black) * v + black)
                        .max(0.0)
                        .powf(2.4)
                        / reference
                } else {
                    (self.decode(v) * swing + min) / reference
                }
            })
        };
        let mut linear = matrix.map(|row| row.iter().zip(rgb).map(|(a, b)| a * b).sum::<f32>());
        // HDR preview policy: luminance-preserving Reinhard compression. Keep
        // SDR unchanged; HDR reference white maps to 0.5 in linear sRGB.
        if self.is_hdr() {
            let y = (0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]).max(0.0);
            for c in &mut linear {
                *c /= 1.0 + y;
            }
        }
        // Fit the gamut toward neutral at constant luminance, avoiding hue
        // shifts from clipping channels independently.
        let grey = (0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]).clamp(0.0, 1.0);
        let mut saturation = 1.0_f32;
        for c in linear {
            if c < 0.0 {
                saturation = saturation.min(grey / (grey - c));
            }
            if c > 1.0 {
                saturation = saturation.min((1.0 - grey) / (c - grey));
            }
        }
        linear = linear.map(|c| grey + saturation * (c - grey));
        let encode = |v: f32| {
            let v = v.clamp(0.0, 1.0);
            quantize(if v <= 0.0031308 {
                12.92 * v
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            })
        };
        [
            encode(linear[0]),
            encode(linear[1]),
            encode(linear[2]),
            quantize(alpha),
        ]
    }
}

// Inverse named encoding functions. BT.1886 and HLG display luminance/OOTF
// are applied above. Sources: Wayland color-management appendix, ITU-T H.273,
// SMPTE ST 240/428, IEC 61966-2-4, and ITU-R BT.2100.
fn decode_transfer(tf: u32, v: f32) -> f32 {
    let positive = v.max(0.0);
    let compound = |x: f32| {
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    match tf {
        2 => positive.powf(2.2),
        3 => positive.powf(2.8),
        4 => {
            if positive < 0.0912 {
                positive / 4.0
            } else {
                ((positive + 0.1115) / 1.1115).powf(1.0 / 0.45)
            }
        }
        5 => v,
        6 => 10.0_f32.powf(2.0 * (positive - 1.0)),
        7 => 10.0_f32.powf(2.5 * (positive - 1.0)),
        8 => {
            let x = v.abs();
            v.signum()
                * if x < 0.081 {
                    x / 4.5
                } else {
                    ((x + 0.099) / 1.099).powf(1.0 / 0.45)
                }
        }
        9 | 14 => compound(positive),
        10 => v.signum() * compound(v.abs()),
        11 => {
            let p = positive.min(1.0).powf(1.0 / 78.84375);
            ((p - 0.8359375).max(0.0) / (18.851563 - 18.6875 * p)).powf(1.0 / 0.15930176)
        }
        12 => positive.powf(2.6) * (52.37 / 48.0),
        13 => {
            if positive <= 0.5 {
                positive * positive / 3.0
            } else {
                (((positive - 0.5599107) / 0.17883277).exp() + 0.28466892) / 12.0
            }
        }
        _ => unreachable!("validated named transfer function"),
    }
}

pub(crate) fn quantize(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    fn linear(primaries: u32, max: f32, reference: f32) -> ColorDescription {
        ColorDescription {
            tf: 5,
            primaries,
            luminances: Some((0.0, max, reference)),
            extended: false,
            ..Default::default()
        }
    }
    #[test]
    fn every_protocol_named_primary_and_transfer_is_supported() {
        for primary in 1..=10 {
            assert_eq!(
                named_primaries(NAMED_PRIMARIES[(primary - 1) as usize]),
                primary
            );
            for tf in 1..=14 {
                let color = ColorDescription {
                    tf,
                    primaries: primary,
                    ..Default::default()
                };
                color.validate().unwrap();
                assert_ne!(color.metadata()["source_transfer_name"], "unknown");
                assert_ne!(color.metadata()["source_primaries_name"], "unknown");
                for v in [0.0, 0.18, 0.5, 1.0] {
                    assert_eq!(color.rgba8([v, v, v, 1.0])[3], 255);
                }
            }
            // Chromatic adaptation must preserve the source neutral axis.
            let color = linear(primary, 100.0, 100.0);
            assert_eq!(color.rgba8([0.18, 0.18, 0.18, 1.0]), [118, 118, 118, 255]);
        }
        for (tf, primaries) in [(0, 1), (15, 1), (5, 0), (5, 11)] {
            assert!(
                ColorDescription {
                    tf,
                    primaries,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn named_transfer_reference_values_and_extended_ranges() {
        // Published transfer equations evaluated independently at E=0.5.
        for (tf, expected) in [
            (2, 0.21763764),
            (3, 0.143_587_3),
            (4, 0.26503573),
            (5, 0.5),
            (6, 0.1),
            (7, 0.05623413),
            (8, 0.2595894),
            (9, 0.21404114),
            (10, 0.21404114),
            (11, 0.009224571),
            (12, 0.179957),
            (13, 0.083333333),
            (14, 0.21404114),
        ] {
            assert!(
                (decode_transfer(tf, 0.5) - expected).abs() < 0.00001,
                "tf {tf}: {}",
                decode_transfer(tf, 0.5)
            );
        }
        assert_eq!(decode_transfer(5, -0.25), -0.25);
        for tf in [8, 10] {
            assert!((decode_transfer(tf, -0.5) + decode_transfer(tf, 0.5)).abs() < 1e-6);
            assert!(decode_transfer(tf, 1.2) > 1.0);
        }
        assert!((decode_transfer(6, 0.0) - 0.01).abs() < 1e-7);
        assert!((decode_transfer(7, 0.0) - 0.0031622777).abs() < 1e-7);
        assert!((decode_transfer(13, 1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn hlg_uses_display_ootf_black_level_and_default_peak() {
        let color = ColorDescription {
            tf: 13,
            primaries: 6,
            ..Default::default()
        };
        assert_eq!(color.levels(), (0.005, 1000.0, 203.0));
        assert_eq!(color.metadata()["tone_mapped"], true);
        // BT.2100 reference signal 75% is approximately 203 nits, yielding
        // approximately 0.5 after the documented Reinhard preview transform.
        let reference = color.rgba8([0.75, 0.75, 0.75, 1.0]);
        assert!((i32::from(reference[0]) - 188).abs() <= 1);
        assert_eq!(reference[0], reference[1]);
        assert_eq!(reference[1], reference[2]);
        let half_alpha = color.rgba8([0.375, 0.375, 0.375, 0.5]);
        assert_eq!(&half_alpha[..3], &reference[..3]);
        assert_eq!(half_alpha[3], 128);
    }

    #[test]
    fn parametric_standard_gamuts_and_unknown_whitepoint() {
        for (expected, xy) in [
            (
                1,
                [
                    640000, 330000, 300000, 600000, 150000, 60000, 312700, 329000,
                ],
            ),
            (
                6,
                [
                    708000, 292000, 170000, 797000, 131000, 46000, 312700, 329000,
                ],
            ),
            (
                9,
                [
                    680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
                ],
            ),
        ] {
            let mut colors = SurfaceColors::default();
            colors.request(
                10,
                &Request::WpImageDescriptionCreatorParamsV1SetPrimaries {
                    r_x: xy[0],
                    r_y: xy[1],
                    g_x: xy[2],
                    g_y: xy[3],
                    b_x: xy[4],
                    b_y: xy[5],
                    w_x: xy[6],
                    w_y: xy[7],
                },
            );
            colors.request(
                10,
                &Request::WpImageDescriptionCreatorParamsV1SetTfNamed { tf: 2 },
            );
            let color = &colors.creators[&10];
            assert_eq!(color.primaries, expected);
            assert!(color.validate().is_ok());
            assert_eq!(named_primaries(xy.map(|v| v - 1)), expected);
            let mut different_white = xy;
            different_white[6] = 345700;
            assert_eq!(named_primaries(different_white), 0);
        }
        assert_eq!(named_primaries([i32::MIN; 8]), 0);
    }

    fn parametric(xy: [i32; 8], tf: u32, power: Option<u32>) -> ColorDescription {
        let mut colors = SurfaceColors::default();
        colors.request(
            10,
            &Request::WpImageDescriptionCreatorParamsV1SetPrimaries {
                r_x: xy[0],
                r_y: xy[1],
                g_x: xy[2],
                g_y: xy[3],
                b_x: xy[4],
                b_y: xy[5],
                w_x: xy[6],
                w_y: xy[7],
            },
        );
        if let Some(eexp) = power {
            colors.request(
                10,
                &Request::WpImageDescriptionCreatorParamsV1SetTfPower { eexp },
            );
        } else {
            colors.request(
                10,
                &Request::WpImageDescriptionCreatorParamsV1SetTfNamed { tf },
            );
        }
        colors.request(
            10,
            &Request::WpImageDescriptionCreatorParamsV1SetLuminances {
                min_lum: 0,
                max_lum: 100,
                reference_lum: 100,
            },
        );
        colors.request(
            10,
            &Request::WpImageDescriptionCreatorParamsV1Create {
                image_description: 11,
            },
        );
        colors.request(
            1,
            &Request::WpColorManagerV1GetSurface {
                id: 12,
                surface: Some(13),
            },
        );
        colors.request(
            12,
            &Request::WpColorManagementSurfaceV1SetImageDescription {
                image_description: Some(11),
                render_intent: 0,
            },
        );
        colors.request(11, &Request::WpImageDescriptionV1Destroy);
        assert!(colors.get(13).is_none());
        colors.request(13, &Request::WlSurfaceCommit);
        colors.get(13).unwrap().clone()
    }

    #[test]
    fn custom_matrices_match_all_named_conversions() {
        for (i, xy) in NAMED_PRIMARIES.iter().enumerate() {
            let (matrix, weights) = custom_primary_conversion(*xy).unwrap();
            let (expected_matrix, expected_weights) = primary_conversion(i as u32 + 1);
            for (a, b) in matrix
                .iter()
                .flatten()
                .zip(expected_matrix.iter().flatten())
            {
                assert!((a - b).abs() < 0.00003, "primary {}: {a} != {b}", i + 1);
            }
            for (a, b) in weights.iter().zip(expected_weights) {
                assert!((a - b).abs() < 0.00003);
            }
            for tf in 1..=14 {
                let custom = parametric(*xy, tf, None);
                custom.validate().unwrap();
                let named = ColorDescription {
                    tf,
                    ..linear(i as u32 + 1, 100.0, 100.0)
                };
                for pixel in [
                    [0.0, 0.0, 0.0, 1.0],
                    [0.18, 0.18, 0.18, 1.0],
                    [0.8, 0.3, 0.1, 1.0],
                    [0.1, 0.2, 0.05, 0.5],
                ] {
                    for (a, b) in custom.rgba8(pixel).iter().zip(named.rgba8(pixel)) {
                        assert!(a.abs_diff(b) <= 1, "primary {} tf {tf}", i + 1);
                    }
                }
            }
        }
    }

    #[test]
    fn arbitrary_primaries_whitepoints_and_client_rounding_are_preserved() {
        // More than the old one-unit whitelist tolerance, as emitted by clients.
        let mut rounded = NAMED_PRIMARIES[0];
        rounded[0] += 17;
        let prophoto = [734700, 265300, 159600, 840400, 36600, 100, 345700, 358500];
        let mut d50 = NAMED_PRIMARIES[0];
        d50[6] = 345700;
        d50[7] = 358500;
        // Chromium's actual output profile from the failed GPU capture.
        let chromium = [
            689407, 309580, 239175, 721657, 135738, 44916, 312700, 329000,
        ];
        for xy in [rounded, prophoto, d50, chromium] {
            for tf in 1..=14 {
                let color = parametric(xy, tf, None);
                color.validate().unwrap();
                assert_eq!(color.primaries, 0);
                assert_eq!(color.chromaticities, Some(xy));
                assert_eq!(
                    color.metadata()["source_primaries_name"],
                    "custom chromaticities"
                );
                assert_eq!(color.rgba8([0.0; 4]), [0; 4]);
                let neutral = color.rgba8([0.18, 0.18, 0.18, 1.0]);
                assert_eq!(neutral[0], neutral[1]);
                assert_eq!(neutral[1], neutral[2]);
            }
            let color = parametric(xy, 5, None);
            assert_eq!(color.rgba8([0.18, 0.18, 0.18, 1.0]), [118, 118, 118, 255]);
        }
        let color = parametric(rounded, 2, None);
        let expected = ColorDescription {
            tf: 2,
            ..linear(1, 100.0, 100.0)
        };
        for v in [0.0, 0.18, 0.5, 1.0] {
            assert_eq!(color.rgba8([v, v, v, 1.0]), expected.rgba8([v, v, v, 1.0]));
        }
    }

    #[test]
    fn every_legal_power_exponent_is_decoded_and_keeps_extended_sign() {
        let mut color = parametric(NAMED_PRIMARIES[0], 0, Some(10000));
        for eexp in 10000..=100000 {
            color.power = Some(eexp);
            color.validate().unwrap();
            let expected = 0.5_f64.powf(f64::from(eexp) / 10000.0) as f32;
            assert!((color.decode(0.5) - expected).abs() < 1e-7);
            assert_eq!(color.decode(-0.5), -color.decode(0.5));
            assert_eq!(color.decode(0.0), 0.0);
            assert_eq!(color.decode(1.0), 1.0);
            assert!(color.decode(1.2) > 1.0);
        }
        for eexp in [0, 9999, 100001, u32::MAX] {
            color.power = Some(eexp);
            assert!(color.validate().is_err());
        }
        let gamma = parametric(NAMED_PRIMARIES[0], 0, Some(22000));
        let named = ColorDescription {
            tf: 2,
            ..linear(1, 100.0, 100.0)
        };
        assert_eq!(
            gamma.rgba8([0.2, 0.3, 0.1, 0.5]),
            named.rgba8([0.2, 0.3, 0.1, 0.5])
        );
        assert_eq!(gamma.metadata()["source_transfer_name"], "custom power");
        assert_eq!(gamma.metadata()["source_transfer_power"], 2.2);
    }

    #[test]
    fn invalid_custom_primaries_fail_explicitly_without_srgb_fallback() {
        let mut collinear = NAMED_PRIMARIES[0];
        collinear[2] = collinear[0];
        collinear[3] = collinear[1];
        let mut invalid_white = NAMED_PRIMARIES[0];
        invalid_white[7] = 0;
        for xy in [collinear, invalid_white, [0; 8]] {
            let color = parametric(xy, 2, None);
            assert!(color.validate().unwrap_err().contains("custom color"));
        }
    }

    #[test]
    fn linear_midgrey_and_premultiplied_alpha() {
        let color = linear(1, 100.0, 100.0);
        assert_eq!(color.rgba8([0.18, 0.18, 0.18, 1.0]), [118, 118, 118, 255]);
        assert_eq!(color.rgba8([0.09, 0.09, 0.09, 0.5]), [118, 118, 118, 128]);
        assert_eq!(color.rgba8([0.0; 4]), [0; 4]);
        assert_eq!(color.metadata()["tone_mapped"], false);
    }
    #[test]
    fn bt2020_matrix_recovers_srgb_red() {
        assert_eq!(
            linear(6, 100.0, 100.0).rgba8([0.627404, 0.069097, 0.016391, 1.0]),
            [255, 0, 0, 255]
        );
    }
    #[test]
    fn adobe_rgb_linear_capture_and_parametric_identification() {
        let color = linear(10, 100.0, 100.0);
        assert!(color.validate().is_ok());
        assert_eq!(color.metadata()["source_primaries_name"], "Adobe RGB");
        assert_eq!(
            named_primaries([
                640000, 330000, 210000, 710000, 150000, 60000, 312700, 329000
            ]),
            10
        );
        assert_eq!(color.rgba8([0.18, 0.18, 0.18, 1.0]), [118, 118, 118, 255]);
        // sRGB red encoded in linear Adobe RGB. Also checks alpha unpremultiplication.
        assert_eq!(color.rgba8([0.7151256, 0.0, 0.0, 1.0]), [255, 0, 0, 255]);
        assert_eq!(color.rgba8([0.3575628, 0.0, 0.0, 0.5]), [255, 0, 0, 128]);
        assert_eq!(
            color.rgba8([0.2848744, 1.0, 0.0411619, 1.0]),
            [0, 255, 0, 255]
        );
    }
    #[test]
    fn hdr_scale_pq_and_preview_notice() {
        let color = linear(6, 10000.0, 203.0);
        assert_eq!(
            color.rgba8([0.0203, 0.0203, 0.0203, 1.0]),
            [188, 188, 188, 255]
        );
        let highlight = color.rgba8([0.1, 0.1, 0.1, 1.0]);
        assert!(highlight[0] > 188 && highlight[0] < 255);
        let pq = ColorDescription {
            tf: 11,
            ..color.clone()
        };
        assert_eq!(pq.rgba8([0.7518271, 0.7518271, 0.7518271, 1.0]), highlight);
        assert_eq!(color.metadata()["tone_mapped"], true);
        assert!(
            color.metadata()["notice"]
                .as_str()
                .unwrap()
                .contains("NOT the original HDR")
        );
    }
    #[test]
    fn description_is_copied_and_double_buffered() {
        let mut colors = SurfaceColors::default();
        colors.request(
            1,
            &Request::WpColorManagerV1CreateParametricCreator { obj: 2 },
        );
        colors.request(
            2,
            &Request::WpImageDescriptionCreatorParamsV1SetTfNamed { tf: 5 },
        );
        colors.request(
            2,
            &Request::WpImageDescriptionCreatorParamsV1SetPrimariesNamed { primaries: 6 },
        );
        colors.request(
            2,
            &Request::WpImageDescriptionCreatorParamsV1Create {
                image_description: 3,
            },
        );
        colors.request(
            1,
            &Request::WpColorManagerV1GetSurface {
                id: 4,
                surface: Some(5),
            },
        );
        colors.request(
            4,
            &Request::WpColorManagementSurfaceV1SetImageDescription {
                image_description: Some(3),
                render_intent: 0,
            },
        );
        colors.request(3, &Request::WpImageDescriptionV1Destroy);
        assert!(colors.get(5).is_none());
        colors.request(5, &Request::WlSurfaceCommit);
        assert!(colors.get(5).unwrap().validate().is_ok());
        colors.request(4, &Request::WpColorManagementSurfaceV1UnsetImageDescription);
        assert!(colors.get(5).is_some());
        colors.request(5, &Request::WlSurfaceCommit);
        assert!(colors.get(5).is_none());
        colors.request(
            4,
            &Request::WpColorManagementSurfaceV1SetImageDescription {
                image_description: Some(999),
                render_intent: 0,
            },
        );
        colors.request(5, &Request::WlSurfaceCommit);
        assert!(colors.get(5).unwrap().validate().is_err());
        colors.request(5, &Request::WlSurfaceDestroy);
        assert!(colors.get(5).is_none());
    }
}

#[derive(Default)]
pub(crate) struct SurfaceColors {
    creators: HashMap<u32, ColorDescription>,
    descriptions: HashMap<u32, ColorDescription>,
    surfaces: HashMap<u32, u32>,
    pending: HashMap<u32, Option<ColorDescription>>,
    committed: HashMap<u32, ColorDescription>,
}

impl SurfaceColors {
    pub(crate) fn get(&self, surface: u32) -> Option<&ColorDescription> {
        self.committed.get(&surface)
    }

    pub(crate) fn request(&mut self, object: u32, request: &Request) {
        match request {
            Request::WpColorManagerV1CreateWindowsScrgb { image_description } => {
                self.descriptions.insert(
                    *image_description,
                    ColorDescription {
                        tf: 5,
                        primaries: 1,
                        luminances: Some((0.0, 80.0, 203.0)),
                        extended: true,
                        ..Default::default()
                    },
                );
            }
            Request::WpColorManagerV1CreateParametricCreator { obj } => {
                self.creators.insert(*obj, ColorDescription::default());
            }
            Request::WpImageDescriptionCreatorParamsV1SetTfNamed { tf } => {
                let creator = self.creators.entry(object).or_default();
                creator.tf = *tf;
                creator.power = None;
            }
            Request::WpImageDescriptionCreatorParamsV1SetPrimariesNamed { primaries } => {
                let creator = self.creators.entry(object).or_default();
                creator.primaries = *primaries;
                creator.chromaticities = None;
                creator.custom_conversion = None;
            }
            Request::WpImageDescriptionCreatorParamsV1SetTfPower { eexp } => {
                let creator = self.creators.entry(object).or_default();
                creator.tf = 0;
                creator.power = Some(*eexp);
            }
            Request::WpImageDescriptionCreatorParamsV1SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                let xy = [*r_x, *r_y, *g_x, *g_y, *b_x, *b_y, *w_x, *w_y];
                let creator = self.creators.entry(object).or_default();
                creator.primaries = named_primaries(xy);
                creator.chromaticities = Some(xy);
                creator.custom_conversion = Some(custom_primary_conversion(xy));
            }
            Request::WpImageDescriptionCreatorParamsV1SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                self.creators.entry(object).or_default().luminances = Some((
                    *min_lum as f32 / 10000.0,
                    *max_lum as f32,
                    *reference_lum as f32,
                ));
            }
            Request::WpImageDescriptionCreatorParamsV1Create { image_description } => {
                self.descriptions.insert(
                    *image_description,
                    self.creators.remove(&object).unwrap_or_default(),
                );
            }
            Request::WpColorManagerV1GetSurface {
                id,
                surface: Some(surface),
            } => {
                self.surfaces.insert(*id, *surface);
            }
            Request::WpColorManagementSurfaceV1SetImageDescription {
                image_description, ..
            } => {
                if let Some(surface) = self.surfaces.get(&object) {
                    // Unknown descriptions must fail capture, never silently render as sRGB.
                    self.pending.insert(
                        *surface,
                        Some(
                            image_description
                                .and_then(|id| self.descriptions.get(&id).cloned())
                                .unwrap_or_default(),
                        ),
                    );
                }
            }
            Request::WpColorManagementSurfaceV1UnsetImageDescription
            | Request::WpColorManagementSurfaceV1Destroy => {
                if let Some(surface) = self.surfaces.get(&object) {
                    self.pending.insert(*surface, None);
                }
                if matches!(request, Request::WpColorManagementSurfaceV1Destroy) {
                    self.surfaces.remove(&object);
                }
            }
            Request::WpImageDescriptionV1Destroy => {
                self.descriptions.remove(&object);
            }
            Request::WlSurfaceCommit => {
                if let Some(color) = self.pending.remove(&object) {
                    if let Some(color) = color {
                        self.committed.insert(object, color);
                    } else {
                        self.committed.remove(&object);
                    }
                }
            }
            Request::WlSurfaceDestroy => {
                self.pending.remove(&object);
                self.committed.remove(&object);
                self.surfaces.retain(|_, v| *v != object);
            }
            _ => {}
        }
    }
}
