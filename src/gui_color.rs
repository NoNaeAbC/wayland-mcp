//! Surface color descriptions and deterministic SDR screenshot rendering.
use crate::gui_wayland_generated::GeneratedHookRequest as Request;
use std::collections::HashMap;

// Protocol coordinates use millionths. Accept one unit of rounding when
// clients express an already supported gamut through the parametric API.
// Unknown gamuts must still fail validation rather than be rendered as sRGB.
fn named_primaries(xy: [i32; 8]) -> u32 {
    [
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
    ]
    .into_iter()
    .find(|(_, known)| xy.iter().zip(known).all(|(a, b)| a.abs_diff(*b) <= 1))
    .map_or(0, |(name, _)| name)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ColorDescription {
    tf: u32,
    primaries: u32,
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
            "source_transfer_name": match self.tf { 1 => "BT.1886", 2 => "gamma 2.2", 3 => "gamma 2.8", 5 => "extended linear", 11 => "ST 2084 PQ", _ => "sRGB piecewise" },
            "source_primaries_name": match self.primaries { 6 => "BT.2020", 9 => "Display P3", _ => "sRGB / BT.709" },
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
        if ![1, 2, 3, 5, 9, 10, 11, 14].contains(&self.tf) || ![1, 6, 9].contains(&self.primaries) {
            return Err(format!(
                "screenshot color conversion does not support transfer {} / primaries {}",
                self.tf, self.primaries
            ));
        }
        let (min, max, reference) = self.levels();
        if !(min >= 0.0 && max > min && reference > min) {
            return Err("invalid screenshot color luminance range".to_string());
        }
        Ok(())
    }

    fn levels(&self) -> (f32, f32, f32) {
        let (min, max, reference) = self.luminances.unwrap_or(if self.tf == 11 {
            (0.005, 10000.005, 203.0)
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

    pub(crate) fn rgba8(&self, pixel: [f32; 4]) -> [u8; 4] {
        let alpha = pixel[3].clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return [0; 4];
        }
        let (min, max, reference) = self.levels();
        let swing = if self.tf == 11 { 10000.0 } else { max - min };
        let decode = |v: f32| {
            let v = v / alpha; // Wayland's default is electrical-value premultiplication.
            let linear = match self.tf {
                1 => {
                    let black = min.powf(1.0 / 2.4);
                    (((max.powf(1.0 / 2.4) - black) * v + black)
                        .max(0.0)
                        .powf(2.4)
                        - min)
                        / swing
                }
                5 => v,
                2 => v.max(0.0).powf(2.2),
                3 => v.max(0.0).powf(2.8),
                11 => {
                    let p = v.max(0.0).powf(1.0 / 78.84375);
                    ((p - 0.8359375).max(0.0) / (18.851563 - 18.6875 * p).max(1e-6))
                        .powf(1.0 / 0.15930176)
                }
                _ => {
                    v.signum()
                        * if v.abs() <= 0.04045 {
                            v.abs() / 12.92
                        } else {
                            ((v.abs() + 0.055) / 1.055).powf(2.4)
                        }
                }
            };
            (linear * swing + min) / reference
        };
        let rgb = [decode(pixel[0]), decode(pixel[1]), decode(pixel[2])];
        let matrix = match self.primaries {
            6 => [
                [1.660491, -0.587641, -0.072850],
                [-0.124550, 1.1329, -0.008349],
                [-0.018151, -0.100579, 1.11873],
            ],
            9 => [
                [1.22494, -0.224940, 0.0],
                [-0.042057, 1.042057, 0.0],
                [-0.019638, -0.078636, 1.098274],
            ],
            _ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
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
        }
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
                    },
                );
            }
            Request::WpColorManagerV1CreateParametricCreator { obj } => {
                self.creators.insert(*obj, ColorDescription::default());
            }
            Request::WpImageDescriptionCreatorParamsV1SetTfNamed { tf } => {
                self.creators.entry(object).or_default().tf = *tf;
            }
            Request::WpImageDescriptionCreatorParamsV1SetPrimariesNamed { primaries } => {
                self.creators.entry(object).or_default().primaries = *primaries;
            }
            Request::WpImageDescriptionCreatorParamsV1SetTfPower { .. } => {
                self.creators.entry(object).or_default().tf = 0;
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
                self.creators.entry(object).or_default().primaries =
                    named_primaries([*r_x, *r_y, *g_x, *g_y, *b_x, *b_y, *w_x, *w_y]);
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
