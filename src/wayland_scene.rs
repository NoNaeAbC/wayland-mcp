//! Window-local observation geometry; the presentation branch stays untouched.
use super::*;

#[derive(Clone)]
struct Layer<'a> {
    surface: &'a TrackedSurface,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    source: [f64; 4],
}
pub(super) struct Scene<'a> {
    layers: Vec<Layer<'a>>,
    origin: (f64, f64),
    pub(super) size: (u32, u32),
    scale: f64,
}
impl WaylandFrameTracker {
    fn scene_layers<'a>(
        &'a self,
        id: u32,
        x: f64,
        y: f64,
        layers: &mut Vec<Layer<'a>>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 128 {
            return Err("surface tree depth exceeds observation limit".into());
        }
        let order = self.stacking.get(&id).cloned().unwrap_or_else(|| vec![id]);
        for node in order {
            if node != id {
                if self.surface_parent.get(&node) != Some(&id) {
                    continue;
                }
                let (cx, cy) = self
                    .surface_position
                    .get(&node)
                    .copied()
                    .unwrap_or_default();
                self.scene_layers(
                    node,
                    x + f64::from(cx),
                    y + f64::from(cy),
                    layers,
                    depth + 1,
                )?;
                continue;
            }
            let Some(surface) = self.surfaces.get(&id) else {
                continue;
            };
            if !surface.has_committed_buffer {
                continue;
            }
            let rotated = surface.buffer_transform % 2 == 1;
            let transformed = if rotated {
                (f64::from(surface.height), f64::from(surface.width))
            } else {
                (f64::from(surface.width), f64::from(surface.height))
            };
            let scale = f64::from(surface.buffer_scale.max(1));
            let source = surface
                .viewport_source
                .map(|r| r.map(|v| f64::from(v) / 256.0))
                .unwrap_or([0.0, 0.0, transformed.0 / scale, transformed.1 / scale]);
            if source[0] < 0.0
                || source[1] < 0.0
                || source[0] + source[2] > transformed.0 / scale + 1e-6
                || source[1] + source[3] > transformed.1 / scale + 1e-6
            {
                return Err(format!(
                    "viewport source exceeds committed buffer on surface {id}"
                ));
            }
            let (width, height) = surface
                .viewport_destination
                .map(|(w, h)| (f64::from(w), f64::from(h)))
                .unwrap_or((source[2], source[3]));
            if width <= 0.0 || height <= 0.0 {
                return Err("empty surface viewport".into());
            }
            layers.push(Layer {
                surface,
                x: x + f64::from(surface.offset.0),
                y: y + f64::from(surface.offset.1),
                width,
                height,
                source,
            });
        }
        let parent_offset = self
            .surfaces
            .get(&id)
            .map(|s| s.window_geometry_offset)
            .unwrap_or_default();
        for popup in &self.popup_order {
            if self.popup_parent.get(popup) != Some(&id) || self.dismissed_popups.contains(popup) {
                continue;
            }
            let position = self.popup_positions.get(popup).copied().unwrap_or_default();
            let offset = self
                .surfaces
                .get(popup)
                .map(|s| s.window_geometry_offset)
                .unwrap_or_default();
            self.scene_layers(
                *popup,
                x + f64::from(position.0) + f64::from(parent_offset.0) - f64::from(offset.0),
                y + f64::from(position.1) + f64::from(parent_offset.1) - f64::from(offset.1),
                layers,
                depth + 1,
            )?;
        }
        Ok(())
    }
    pub(super) fn observation_scene(&self, window: &str) -> Result<Scene<'_>, String> {
        let root = self
            .windows
            .get(window)
            .ok_or("window no longer exists")?
            .input_surface_id;
        let mut layers = vec![];
        self.scene_layers(root, 0.0, 0.0, &mut layers, 0)?;
        if layers.is_empty() {
            return Err("snapshot_unavailable: window has no committed content".into());
        }
        let root_layer = layers
            .iter()
            .find(|layer| layer.surface.id == root)
            .unwrap_or(&layers[0]);
        let scale = (root_layer.source[2] * f64::from(root_layer.surface.buffer_scale)
            / root_layer.width)
            .max(0.01);
        let x = layers.iter().map(|l| l.x).fold(f64::INFINITY, f64::min);
        let y = layers.iter().map(|l| l.y).fold(f64::INFINITY, f64::min);
        let right = layers
            .iter()
            .map(|l| l.x + l.width)
            .fold(f64::NEG_INFINITY, f64::max);
        let bottom = layers
            .iter()
            .map(|l| l.y + l.height)
            .fold(f64::NEG_INFINITY, f64::max);
        let width = ((right - x) * scale).ceil();
        let height = ((bottom - y) * scale).ceil();
        if width > 8192.0 || height > 8192.0 || width * height > 16_777_216.0 {
            return Err("window composition exceeds the pixel budget".into());
        }
        Ok(Scene {
            layers,
            origin: (x, y),
            size: (width as u32, height as u32),
            scale,
        })
    }
    pub(super) fn scene_capture_error(&self, window: &str) -> Option<String> {
        let scene = match self.observation_scene(window) {
            Ok(scene) => scene,
            Err(error) => return Some(error),
        };
        for layer in scene.layers {
            let surface = layer.surface;
            if let Some(color) = surface.color.as_ref()
                && let Err(error) = color.validate()
            {
                return Some(error);
            }
            if surface.rgba.len() != surface.width as usize * surface.height as usize * 4
                && surface.linear_rgba.len() != surface.width as usize * surface.height as usize
            {
                return Some(surface.capture_error.clone().unwrap_or_else(|| {
                    format!(
                        "snapshot_unavailable: surface {} has no owned committed pixels",
                        surface.id
                    )
                }));
            }
        }
        None
    }
    pub(super) fn capture_scene(&self, window: &str) -> Result<CapturedRgbaFrame, String> {
        let scene = self.observation_scene(window)?;
        let mut output = vec![[0.0f32; 4]; scene.size.0 as usize * scene.size.1 as usize];
        let mut hdr = false;
        let mut metadata = vec![];
        for layer in &scene.layers {
            let surface = layer.surface;
            let color = surface.color.as_ref();
            if let Some(color) = color {
                color.validate()?;
                hdr |= color.is_hdr();
                metadata.push(color.metadata());
            }
            if surface.rgba.len() != surface.width as usize * surface.height as usize * 4
                && surface.linear_rgba.len() != surface.width as usize * surface.height as usize
            {
                return Err(surface.capture_error.clone().unwrap_or_else(|| {
                    format!(
                        "snapshot_unavailable: surface {} has no owned committed pixels",
                        surface.id
                    )
                }));
            }
            let x0 = ((layer.x - scene.origin.0) * scene.scale).floor().max(0.0) as u32;
            let y0 = ((layer.y - scene.origin.1) * scene.scale).floor().max(0.0) as u32;
            let x1 = (((layer.x + layer.width - scene.origin.0) * scene.scale).ceil() as u32)
                .min(scene.size.0);
            let y1 = (((layer.y + layer.height - scene.origin.1) * scene.scale).ceil() as u32)
                .min(scene.size.1);
            for y in y0..y1 {
                for x in x0..x1 {
                    let sx = (f64::from(x) + 0.5) / scene.scale + scene.origin.0 - layer.x;
                    let sy = (f64::from(y) + 0.5) / scene.scale + scene.origin.1 - layer.y;
                    if sx < 0.0 || sy < 0.0 || sx >= layer.width || sy >= layer.height {
                        continue;
                    }
                    let tx = (layer.source[0] + sx / layer.width * layer.source[2])
                        * f64::from(surface.buffer_scale);
                    let ty = (layer.source[1] + sy / layer.height * layer.source[3])
                        * f64::from(surface.buffer_scale);
                    let (bx, by) = inverse_transform(
                        surface.buffer_transform,
                        tx,
                        ty,
                        (f64::from(surface.width), f64::from(surface.height)),
                    );
                    let bx = (bx.floor() as i64).clamp(0, i64::from(surface.width) - 1) as usize;
                    let by = (by.floor() as i64).clamp(0, i64::from(surface.height) - 1) as usize;
                    let offset = by * surface.width as usize + bx;
                    let value = if !surface.linear_rgba.is_empty() {
                        surface.linear_rgba[offset]
                    } else {
                        let pixel = &surface.rgba[offset * 4..][..4];
                        let input =
                            [pixel[0], pixel[1], pixel[2], pixel[3]].map(|c| f32::from(c) / 255.0);
                        if let Some(color) = color {
                            color.linear_premultiplied(input)
                        } else {
                            srgb_premultiplied(input)
                        }
                    };
                    let target = &mut output[(y * scene.size.0 + x) as usize];
                    for c in 0..4 {
                        target[c] = value[c] + target[c] * (1.0 - value[3]);
                    }
                }
            }
        }
        let rgba = output
            .into_iter()
            .flat_map(|p| crate::gui_color::encode_preview(p, hdr))
            .collect();
        let color = Some(
            serde_json::json!({"geometry":{"origin":{"x":scene.origin.0,"y":scene.origin.1},"pixels_per_logical_unit":scene.scale},"surfaces":metadata,"tone_mapped":hdr,"notice":if hdr {Some("HDR content is blended in linear light and tone mapped to SDR; original HDR appearance cannot be judged from this preview")} else {None}}),
        );
        Ok(CapturedRgbaFrame {
            width: scene.size.0,
            height: scene.size.1,
            rgba,
            color,
        })
    }
    pub(super) fn surface_pointer_target(
        &self,
        window: &str,
        surface: u32,
        x: i64,
        y: i64,
        confined: bool,
    ) -> Result<PointerClickTarget, String> {
        let scene = self.observation_scene(window)?;
        let layer = scene
            .layers
            .iter()
            .find(|l| l.surface.id == surface)
            .ok_or("grabbed surface is not mapped")?;
        let mut sx = x as f64 / scene.scale + scene.origin.0 - layer.x;
        let mut sy = y as f64 / scene.scale + scene.origin.1 - layer.y;
        if confined {
            sx = sx.clamp(0.0, (layer.width - 1.0 / 256.0).max(0.0));
            sy = sy.clamp(0.0, (layer.height - 1.0 / 256.0).max(0.0));
        }
        sx += f64::from(layer.surface.offset.0);
        sy += f64::from(layer.surface.offset.1);
        Ok(PointerClickTarget {
            window_id: window.into(),
            surface_id: surface,
            screenshot_x: x,
            screenshot_y: y,
            surface_x: sx.floor() as i64,
            surface_y: sy.floor() as i64,
            fixed_coords: Some(((sx * 256.0).round() as i32, (sy * 256.0).round() as i32)),
        })
    }

    pub(super) fn scene_pointer_target(
        &self,
        window: &str,
        x: i64,
        y: i64,
    ) -> Result<Option<PointerClickTarget>, String> {
        if !self.windows.contains_key(window) {
            return Ok(None);
        }
        let scene = self.observation_scene(window)?;
        if x < 0 || y < 0 || x >= i64::from(scene.size.0) || y >= i64::from(scene.size.1) {
            return Err(format!(
                "click coordinate ({x}, {y}) is outside screenshot bounds {}x{}",
                scene.size.0, scene.size.1
            ));
        }
        let px = x as f64 / scene.scale + scene.origin.0;
        let py = y as f64 / scene.scale + scene.origin.1;
        for layer in scene.layers.iter().rev() {
            let sx = px - layer.x;
            let sy = py - layer.y;
            if sx < 0.0 || sy < 0.0 || sx >= layer.width || sy >= layer.height {
                continue;
            }
            if layer.surface.input_region.as_ref().is_some_and(|region| {
                !region.iter().any(|r| {
                    sx >= f64::from(r.x)
                        && sy >= f64::from(r.y)
                        && sx < f64::from(r.x) + f64::from(r.width)
                        && sy < f64::from(r.y) + f64::from(r.height)
                })
            }) {
                continue;
            }
            let mut target = PointerClickTarget {
                window_id: window.into(),
                surface_id: layer.surface.id,
                screenshot_x: x,
                screenshot_y: y,
                surface_x: sx.floor() as i64,
                surface_y: sy.floor() as i64,
                fixed_coords: None,
            };
            if sx.fract() != 0.0 || sy.fract() != 0.0 {
                target.fixed_coords =
                    Some(((sx * 256.0).round() as i32, (sy * 256.0).round() as i32));
            }
            return Ok(Some(target));
        }
        Err("no input surface accepts the screenshot coordinate".into())
    }
}
fn srgb_premultiplied(pixel: [f32; 4]) -> [f32; 4] {
    let alpha = pixel[3];
    if alpha <= 0.0 {
        return [0.0; 4];
    }
    let decode = |c: f32| {
        let c = c / alpha;
        (if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }) * alpha
    };
    [decode(pixel[0]), decode(pixel[1]), decode(pixel[2]), alpha]
}
fn inverse_transform(transform: u32, x: f64, y: f64, size: (f64, f64)) -> (f64, f64) {
    let (w, h) = size;
    match transform {
        0 => (x, y),
        1 => (w - y, x),
        2 => (w - x, h - y),
        3 => (y, h - x),
        4 => (w - x, y),
        5 => (y, x),
        6 => (x, h - y),
        7 => (w - y, h - x),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn solid(tracker: &mut WaylandFrameTracker, id: u32, size: (u32, u32), pixel: [u8; 4]) {
        let s = tracker.pending_surface_mut(id);
        s.width = size.0;
        s.height = size.1;
        s.rgba = pixel.repeat((size.0 * size.1) as usize);
        s.has_committed_buffer = true;
    }
    #[test]
    fn synchronized_alpha_and_stacking_are_latched_by_parent_commit() -> Result<(), String> {
        let mut tracker = WaylandFrameTracker::new("fixture".into());
        tracker.note_xdg_surface_created(20, 1);
        let window = tracker.note_xdg_toplevel_created(20, 21)?;
        solid(&mut tracker, 1, (4, 2), [255, 0, 0, 255]);
        tracker.commit_surface(1);
        tracker.note_subsurface_created(30, 2, 1);
        tracker.note_subsurface_position(30, 1, 0);
        solid(&mut tracker, 2, (2, 1), [0, 0, 128, 128]);
        tracker.commit_surface(2);
        assert_eq!(
            tracker.capture_scene(&window)?.rgba,
            [255, 0, 0, 255].repeat(8)
        );
        tracker.commit_surface(1);
        let frame = tracker.capture_scene(&window)?;
        assert_eq!((frame.width, frame.height), (4, 2));
        let purple = [187, 0, 188, 255];
        assert_eq!(
            frame.rgba,
            [
                vec![255, 0, 0, 255],
                purple.repeat(2),
                vec![255, 0, 0, 255],
                [255, 0, 0, 255].repeat(4)
            ]
            .concat()
        );
        assert_eq!(
            tracker
                .scene_pointer_target(&window, 1, 0)?
                .unwrap()
                .surface_id,
            2
        );
        tracker.note_subsurface_stacking(30, 1, false)?;
        assert_eq!(tracker.capture_scene(&window)?.rgba, frame.rgba);
        tracker.commit_surface(1);
        assert_eq!(
            tracker.capture_scene(&window)?.rgba,
            [255, 0, 0, 255].repeat(8)
        );
        tracker.note_surface_destroyed(2);
        assert!(tracker.windows.contains_key(&window));
        Ok(())
    }
    #[test]
    fn shm_snapshot_survives_storage_overwrite_and_buffer_id_reuse() -> Result<(), String> {
        let file = tempfile::tempfile().map_err(|e| e.to_string())?;
        file.write_at(&[0, 0, 255, 255], 0)
            .map_err(|e| e.to_string())?;
        let mut tracker = WaylandFrameTracker::new("fixture".into());
        tracker.note_shm_pool_created(10, duplicate_fd(&file.into())?, 4);
        tracker.note_shm_buffer_created(ShmBufferSpec {
            pool_id: 10,
            buffer_id: 11,
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: 0,
        })?;
        tracker.note_xdg_surface_created(20, 1);
        let window = tracker.note_xdg_toplevel_created(20, 21)?;
        tracker.set_surface_buffer(1, Some(11));
        tracker.commit_surface(1);
        let retained = tracker.buffers[&11].clone();
        if let TrackedBufferSource::Shm(buffer) = &retained.source {
            let backing = std::fs::File::from(duplicate_fd(&buffer.fd)?);
            backing
                .write_at(&[255, 0, 0, 255], 0)
                .map_err(|e| e.to_string())?;
        }
        tracker.destroy_buffer(11);
        tracker.note_shm_buffer_created(ShmBufferSpec {
            pool_id: 10,
            buffer_id: 11,
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: 0,
        })?;
        assert_eq!(tracker.capture_scene(&window)?.rgba, vec![255, 0, 0, 255]);
        tracker.set_surface_buffer(1, Some(11));
        assert_eq!(tracker.capture_scene(&window)?.rgba, vec![255, 0, 0, 255]);
        tracker.commit_surface(1);
        assert_eq!(tracker.capture_scene(&window)?.rgba, vec![0, 0, 255, 255]);
        Ok(())
    }
    #[test]
    fn viewport_rotation_and_negative_child_position_match_pixels_and_input() -> Result<(), String>
    {
        let mut tracker = WaylandFrameTracker::new("fixture".into());
        tracker.note_xdg_surface_created(20, 1);
        let window = tracker.note_xdg_toplevel_created(20, 21)?;
        let s = tracker.pending_surface_mut(1);
        s.width = 2;
        s.height = 1;
        s.has_committed_buffer = true;
        s.rgba = vec![255, 0, 0, 255, 0, 255, 0, 255];
        s.buffer_transform = 1;
        tracker.commit_surface(1);
        let frame = tracker.capture_scene(&window)?;
        assert_eq!((frame.width, frame.height), (1, 2));
        assert_eq!(frame.rgba, vec![0, 255, 0, 255, 255, 0, 0, 255]);
        tracker.note_subsurface_created(30, 2, 1);
        tracker.note_subsurface_position(30, -1, 0);
        solid(&mut tracker, 2, (1, 1), [0, 0, 255, 255]);
        tracker.commit_surface(2);
        tracker.commit_surface(1);
        let frame = tracker.capture_scene(&window)?;
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(&frame.rgba[..4], &[0, 0, 255, 255]);
        let target = tracker.scene_pointer_target(&window, 0, 0)?.unwrap();
        assert_eq!(
            (target.surface_id, target.surface_x, target.surface_y),
            (2, 0, 0)
        );
        assert_eq!(frame.color.unwrap()["geometry"]["origin"]["x"], -1.0);
        Ok(())
    }
}
