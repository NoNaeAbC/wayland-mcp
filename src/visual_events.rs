//! Agent-defined predicates, trusted Vulkan kernels, bounded event-only transport.
use crate::gui_color::{ColorDescription, GpuColorProfile};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuRule {
    offset: u32,
    width: u32,
    height: u32,
    kind: u32,
    threshold: f32,
    minimum: u32,
    above: u32,
    debounce: u32,
    cooldown: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}
#[repr(C)]
pub(crate) struct Plane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Event {
    rule: u32,
    active: u32,
    frame: u32,
    reserved: u32,
}
#[repr(C)]
struct ShaderPass {
    source: *const c_char,
    source_size: u32,
    groups: [u32; 3],
}
#[repr(C)]
struct ProgramConfig {
    width: u32,
    height: u32,
    result_bytes: u32,
    state_bytes: u32,
    scratch_bytes: u32,
    parameter_bytes: u32,
    previous_frame: u32,
    feedback: u32,
    alpha_mode: u32,
    pass_count: u32,
}
unsafe extern "C" {
    fn visual_snapshot_create(
        context: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        error: *mut c_char,
    ) -> *mut c_void;
    fn visual_snapshot_destroy(snapshot: *mut c_void);
    fn visual_snapshot_capture(
        snapshot: *mut c_void,
        key: u64,
        planes: *const Plane,
        count: u32,
        error: *mut c_char,
    ) -> i32;
    fn visual_snapshot_read(
        snapshot: *mut c_void,
        output: *mut u8,
        bytes: u32,
        error: *mut c_char,
    ) -> i32;
    fn visual_runtime_create(
        context: *mut c_void,
        config: *const ProgramConfig,
        passes: *const ShaderPass,
        parameters: *const u8,
        error: *mut c_char,
    ) -> *mut c_void;
    fn visual_runtime_destroy(program: *mut c_void);
    fn visual_runtime_capture(
        program: *mut c_void,
        key: u64,
        width: u32,
        height: u32,
        format: u32,
        planes: *const Plane,
        count: u32,
        error: *mut c_char,
    ) -> i32;
    fn visual_runtime_analyze(program: *mut c_void, result: *mut u8, error: *mut c_char) -> i32;
    fn visual_runtime_set_color(
        program: *mut c_void,
        color: *const GpuColorProfile,
        error: *mut c_char,
    ) -> i32;
    fn visual_set_color(
        program: *mut c_void,
        color: *const GpuColorProfile,
        error: *mut c_char,
    ) -> i32;
    fn visual_context_create(major: u32, minor: u32, error: *mut c_char) -> *mut c_void;
    fn visual_context_destroy(context: *mut c_void);
    fn visual_snapshot_raw(
        context: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        planes: *const Plane,
        count: u32,
        rgba: *mut u8,
        output_bytes: u32,
        error: *mut c_char,
    ) -> i32;
    fn visual_program_create(
        context: *mut c_void,
        rules: *const GpuRule,
        rects: *const Rect,
        count: u32,
        error: *mut c_char,
    ) -> *mut c_void;
    fn visual_program_destroy(program: *mut c_void);
    fn visual_capture(
        program: *mut c_void,
        key: u64,
        width: u32,
        height: u32,
        format: u32,
        planes: *const Plane,
        count: u32,
        error: *mut c_char,
    ) -> i32;
    fn visual_analyze(program: *mut c_void, events: *mut Event, error: *mut c_char) -> i32;
    #[cfg(test)]
    fn visual_test(
        program: *mut c_void,
        pattern: u32,
        events: *mut Event,
        error: *mut c_char,
    ) -> i32;
}
fn error_text(error: &[c_char; 512]) -> String {
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
struct Context(*mut c_void);
// Device.submission serializes native calls sharing a queue. The Hub mutex
// protects metadata only; it must not be held during GPU work.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}
impl Drop for Context {
    fn drop(&mut self) {
        unsafe { visual_context_destroy(self.0) }
    }
}
struct Program(*mut c_void, bool);
unsafe impl Send for Program {}
impl Drop for Program {
    fn drop(&mut self) {
        unsafe {
            if self.1 {
                visual_runtime_destroy(self.0)
            } else {
                visual_program_destroy(self.0)
            }
        }
    }
}
struct Device {
    context: Context,
    submission: Mutex<()>,
}
struct SnapshotNative(*mut c_void, u64);
unsafe impl Send for SnapshotNative {}
impl Drop for SnapshotNative {
    fn drop(&mut self) {
        unsafe { visual_snapshot_destroy(self.0) }
    }
}
struct SnapshotSlot {
    native: Mutex<SnapshotNative>,
    device: Arc<Device>,
    affinity: (u32, u32),
    extent_format: (u32, u32, u32),
}
pub(crate) struct SnapshotPixels {
    pub raw: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub serial: u64,
}
struct ProgramSlot {
    native: Mutex<Program>,
    // Keep the VkDevice alive until the native program and any owned job finish.
    device: Arc<Device>,
}
pub(crate) struct CapturedFrame {
    id: u64,
    program: Arc<ProgramSlot>,
    serial: u64,
    timestamp: f64,
    acquisition_ns: u128,
    color: Value,
    epoch: u64,
    history_valid: bool,
}

fn one() -> u32 {
    1
}
fn threshold() -> f32 {
    0.5
}
fn fps() -> u32 {
    60
}
fn duration() -> u64 {
    120_000
}
fn pixels() -> u32 {
    8
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Rule {
    id: String,
    rect: [u32; 4],
    kind: String,
    #[serde(default = "threshold")]
    threshold: f32,
    #[serde(default = "pixels")]
    min_pixels: u32,
    #[serde(default)]
    polarity: Option<String>,
    #[serde(default = "one")]
    debounce_frames: u32,
    #[serde(default = "one")]
    cooldown_frames: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Args {
    window_id: String,
    rules: Vec<Rule>,
    #[serde(default)]
    source_color: Option<Value>,
    #[serde(default = "fps")]
    max_fps: u32,
    #[serde(default = "duration")]
    duration_ms: u64,
    #[serde(default)]
    callback_timeout_ms: Option<u32>,
}
fn validate(args: &Args) -> Result<(Vec<GpuRule>, Vec<Rect>), String> {
    if args.window_id.is_empty()
        || args.window_id.len() > 256
        || args.rules.is_empty()
        || args.rules.len() > 16
    {
        return Err("visual subscription needs a windowId and 1..16 rules".into());
    }
    if !(1..=120).contains(&args.max_fps) || !(1..=3_600_000).contains(&args.duration_ms) {
        return Err("maxFps must be 1..120; durationMs 1..3600000".into());
    }
    if args
        .callback_timeout_ms
        .is_some_and(|v| !(1..=120000).contains(&v))
    {
        return Err("invalid callbackTimeoutMs".into());
    }
    let mut rules = Vec::new();
    let mut rects = Vec::new();
    let mut area = 0u64;
    let mut ids = std::collections::HashSet::new();
    for rule in &args.rules {
        let [x, y, width, height] = rule.rect;
        let n = u64::from(width) * u64::from(height);
        area += n;
        if width < 4
            || height < 4
            || n < 64
            || area > 1_048_576
            || x > 16384
            || y > 16384
            || width > 16384
            || height > 16384
        {
            return Err("visual rects require width/height >=4, area >=64, total <=1048576".into());
        }
        if rule.id.is_empty() || rule.id.len() > 64 || !ids.insert(&rule.id) {
            return Err("rule ids must be unique nonempty strings <=64 bytes".into());
        }
        let kind = match rule.kind.as_str() {
            "luminance" => 0,
            "change" => 1,
            _ => return Err("kind must be luminance or change".into()),
        };
        if !rule.threshold.is_finite() || rule.threshold < 0.0 || rule.threshold == 0.0 && kind == 1
        {
            return Err("threshold must be finite and nonnegative (positive for change)".into());
        }
        if rule.min_pixels == 0
            || u64::from(rule.min_pixels) > n
            || !(1..=3600).contains(&rule.debounce_frames)
            || !(1..=3600).contains(&rule.cooldown_frames)
        {
            return Err("invalid minPixels/debounceFrames/cooldownFrames".into());
        }
        let above = match rule.polarity.as_deref() {
            None | Some("below") => 0,
            Some("above") => 1,
            _ => return Err("polarity must be below or above".into()),
        };
        rules.push(GpuRule {
            offset: 0,
            width,
            height,
            kind,
            threshold: rule.threshold,
            minimum: rule.min_pixels,
            above,
            debounce: rule.debounce_frames,
            cooldown: rule.cooldown_frames,
        });
        rects.push(Rect {
            x,
            y,
            width,
            height,
        });
    }
    Ok((rules, rects))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PassArgs {
    source: String,
    dispatch: [u32; 3],
}
fn source_color(value: Option<&Value>) -> Result<(Option<ColorDescription>, u32), String> {
    let Some(value) = value else {
        return Ok((None, 2));
    };
    let alpha_name = value
        .get("alpha")
        .map(|v| v.as_str().ok_or("sourceColor alpha must be a string"))
        .transpose()?
        .unwrap_or("premultiplied");
    let alpha = match alpha_name {
        "opaque" => 0,
        "straight" => 1,
        "premultiplied" => 2,
        _ => return Err("unknown source alpha mode".into()),
    };
    Ok((Some(ColorDescription::visual_assumption(value)?), alpha))
}
fn default_state() -> u32 {
    32
}
fn default_scratch() -> u32 {
    4096
}
fn default_feedback() -> String {
    "separateState".into()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProgramArgs {
    window_id: String,
    passes: Vec<PassArgs>,
    result_bytes: u32,
    #[serde(default = "default_state")]
    state_bytes: u32,
    #[serde(default = "default_scratch")]
    scratch_bytes: u32,
    #[serde(default)]
    parameters: Vec<u8>,
    #[serde(default = "default_feedback")]
    feedback: String,
    #[serde(default)]
    previous_frame: bool,
    #[serde(default)]
    source_color: Option<Value>,
    #[serde(default = "fps")]
    max_fps: u32,
    #[serde(default = "duration")]
    duration_ms: u64,
    #[serde(default)]
    callback_timeout_ms: Option<u32>,
}
impl ProgramArgs {
    fn config(&self, width: u32, height: u32) -> Result<ProgramConfig, String> {
        let aligned = |n: u32, max: u32| n >= 4 && n <= max && n.is_multiple_of(4);
        if self.window_id.is_empty()
            || self.passes.is_empty()
            || self.passes.len() > 8
            || self.passes.iter().any(|p| {
                p.source.is_empty()
                    || p.source.len() > 262144
                    || p.source.contains('\0')
                    || p.dispatch.contains(&0)
            })
            || !aligned(self.result_bytes, 256)
            || !aligned(self.state_bytes, 65536)
            || !aligned(self.scratch_bytes, 1048576)
            || self.parameters.len() > 4096
            || !self.parameters.len().is_multiple_of(4)
            || !(1..=120).contains(&self.max_fps)
            || !(1..=600000).contains(&self.duration_ms)
            || self
                .callback_timeout_ms
                .is_some_and(|n| !(1..=120000).contains(&n))
            || width == 0
            || height == 0
            || u64::from(width) * u64::from(height) > 8388608
        {
            return Err("invalid visual program resource/lease budget".into());
        }
        let (_, alpha_mode) = source_color(self.source_color.as_ref())?;
        let feedback = match self.feedback.as_str() {
            "separateState" => 0,
            "previousResult" => 1,
            _ => return Err("unknown feedback mode".into()),
        };
        Ok(ProgramConfig {
            width,
            height,
            result_bytes: self.result_bytes,
            state_bytes: if feedback == 1 {
                self.result_bytes
            } else {
                self.state_bytes
            },
            scratch_bytes: self.scratch_bytes,
            parameter_bytes: self.parameters.len() as u32,
            previous_frame: u32::from(self.previous_frame),
            feedback,
            alpha_mode,
            pass_count: self.passes.len() as u32,
        })
    }
}
struct Stream {
    window: String,
    affinity: (u32, u32),
    program: Arc<ProgramSlot>,
    names: Vec<String>,
    sender: mpsc::Sender<crate::input_events::QueuedInput>,
    end: watch::Sender<Option<Value>>,
    bytes: Arc<AtomicUsize>,
    deadline: Instant,
    period: Duration,
    last: Option<Instant>,
    sequence: u64,
    frames: u64,
    gpu_ns: u128,
    acquisition_ns: u128,
    in_flight: bool,
    signature: Option<(u32, u32, u32)>,
    admission: HashMap<&'static str, u64>,
    fallback_color: Option<ColorDescription>,
    alpha_mode: u32,
    color_profile: Option<GpuColorProfile>,
    epoch: u64,
}
#[derive(Default)]
struct State {
    streams: HashMap<u64, Stream>,
    snapshots: HashMap<String, Arc<SnapshotSlot>>,
    contexts: HashMap<(u32, u32), Arc<Device>>,
    affinities: HashMap<(u32, u32), (u32, u32)>,
    next: u64,
}
pub(crate) struct VisualHub {
    start: Instant,
    inner: Mutex<State>,
}
impl Default for VisualHub {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            inner: Mutex::new(State::default()),
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.streams.clear();
        self.snapshots.clear();
        self.contexts.clear();
    }
}
fn canonical_drm_device(affinity: (u32, u32)) -> (u32, u32) {
    // Primary and render nodes of one DRM device share one Vulkan context.
    let path = format!("/sys/dev/char/{}:{}/device/drm", affinity.0, affinity.1);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("renderD") {
                continue;
            }
            if let Ok(dev) = std::fs::read_to_string(entry.path().join("dev"))
                && let Some((major, minor)) = dev.trim().split_once(':')
                && let (Ok(major), Ok(minor)) = (major.parse(), minor.parse())
            {
                return (major, minor);
            }
        }
    }
    affinity
}
impl VisualHub {
    fn device(&self, affinity: (u32, u32)) -> Result<Arc<Device>, String> {
        let mut state = self.inner.lock().unwrap();
        let canonical = *state
            .affinities
            .entry(affinity)
            .or_insert_with(|| canonical_drm_device(affinity));
        if let std::collections::hash_map::Entry::Vacant(entry) = state.contexts.entry(canonical) {
            let mut error = [0 as c_char; 512];
            let pointer =
                unsafe { visual_context_create(canonical.0, canonical.1, error.as_mut_ptr()) };
            if pointer.is_null() {
                return Err(error_text(&error));
            }
            entry.insert(Arc::new(Device {
                context: Context(pointer),
                submission: Mutex::new(()),
            }));
        }
        Ok(state.contexts[&canonical].clone())
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn retain_snapshot(
        &self,
        window: &str,
        affinity: (u32, u32),
        key: u64,
        width: u32,
        height: u32,
        format: u32,
        planes: &[Plane],
        serial: u64,
        wait_acquire: impl FnOnce() -> Result<(), String>,
    ) -> Result<bool, String> {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 8_388_608 {
            return Err("invalid GPU snapshot extent".into());
        }
        let device = self.device(affinity)?;
        let existing = {
            let mut state = self.inner.lock().unwrap();
            if state.snapshots.get(window).is_some_and(|s| {
                s.affinity != affinity || s.extent_format != (width, height, format)
            }) {
                state.snapshots.remove(window);
            }
            state.snapshots.get(window).cloned()
        };
        let snapshot = if let Some(snapshot) = existing {
            snapshot
        } else {
            let creation = match device.submission.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                Err(_) => return Err("GPU device worker poisoned".into()),
            };
            let mut error = [0 as c_char; 512];
            let pointer = unsafe {
                visual_snapshot_create(device.context.0, width, height, format, error.as_mut_ptr())
            };
            if pointer.is_null() {
                return Err(error_text(&error));
            }
            let snapshot = Arc::new(SnapshotSlot {
                native: Mutex::new(SnapshotNative(pointer, 0)),
                device: device.clone(),
                affinity,
                extent_format: (width, height, format),
            });
            drop(creation);
            let mut state = self.inner.lock().unwrap();
            if state.snapshots.len() >= 8 {
                return Ok(false);
            }
            state.snapshots.insert(window.into(), snapshot.clone());
            snapshot
        };
        // Cache acquisition is bounded and skips busy devices before waiting
        // on the producer. Pixels remain solely in device-local storage.
        let submission = match device.submission.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => return Err("GPU device worker poisoned".into()),
        };
        drop(submission);
        wait_acquire()?;
        let submission = match device.submission.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => return Err("GPU device worker poisoned".into()),
        };
        let mut native = snapshot.native.lock().unwrap();
        let mut error = [0 as c_char; 512];
        let result = unsafe {
            visual_snapshot_capture(
                native.0,
                key,
                planes.as_ptr(),
                planes.len() as u32,
                error.as_mut_ptr(),
            )
        };
        drop(submission);
        if result < 0 {
            native.1 = 0;
            return Err(error_text(&error));
        }
        native.1 = serial;
        Ok(true)
    }
    pub(crate) fn read_snapshot(&self, window: &str) -> Result<SnapshotPixels, String> {
        let snapshot = self
            .inner
            .lock()
            .unwrap()
            .snapshots
            .get(window)
            .cloned()
            .ok_or("snapshot_unavailable: no GPU-owned render buffer yet")?;
        let _submission = snapshot.device.submission.lock().unwrap();
        let native = snapshot.native.lock().unwrap();
        if native.1 == 0 {
            return Err("snapshot_unavailable: no completed GPU snapshot".into());
        }
        let (width, height, format) = snapshot.extent_format;
        let bytes_per_pixel = if matches!(format, 0x48344258 | 0x48344241) {
            8
        } else {
            4
        };
        let mut raw = vec![0; width as usize * height as usize * bytes_per_pixel];
        let mut error = [0 as c_char; 512];
        let result = unsafe {
            visual_snapshot_read(
                native.0,
                raw.as_mut_ptr(),
                raw.len() as u32,
                error.as_mut_ptr(),
            )
        };
        if result < 0 {
            return Err(error_text(&error));
        }
        Ok(SnapshotPixels {
            raw,
            width,
            height,
            format,
            serial: native.1,
        })
    }
    // Explicit screenshot bootstrap uses the SAME DRM-affine context/queue as
    // observers. This output never enters the bounded visual-event transport.
    pub(crate) fn screenshot_raw(
        &self,
        affinity: (u32, u32),
        width: u32,
        height: u32,
        format: u32,
        planes: &[Plane],
    ) -> Result<Vec<u8>, String> {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 8_388_608 {
            return Err("invalid screenshot extent".into());
        }
        let mut error = [0 as c_char; 512];
        let device = {
            let mut state = self.inner.lock().unwrap();
            let canonical = *state
                .affinities
                .entry(affinity)
                .or_insert_with(|| canonical_drm_device(affinity));
            if let std::collections::hash_map::Entry::Vacant(entry) =
                state.contexts.entry(canonical)
            {
                let pointer =
                    unsafe { visual_context_create(canonical.0, canonical.1, error.as_mut_ptr()) };
                if pointer.is_null() {
                    return Err(error_text(&error));
                }
                entry.insert(Arc::new(Device {
                    context: Context(pointer),
                    submission: Mutex::new(()),
                }));
            }
            state.contexts[&canonical].clone()
        };
        let bytes_per_pixel = if matches!(format, 0x48344258 | 0x48344241) {
            8
        } else {
            4
        };
        let mut rgba = vec![0; width as usize * height as usize * bytes_per_pixel];
        let _submission = device.submission.lock().unwrap();
        let result = unsafe {
            visual_snapshot_raw(
                device.context.0,
                width,
                height,
                format,
                planes.as_ptr(),
                planes.len() as u32,
                rgba.as_mut_ptr(),
                rgba.len() as u32,
                error.as_mut_ptr(),
            )
        };
        if result < 0 {
            Err(error_text(&error))
        } else {
            Ok(rgba)
        }
    }
    pub(crate) fn subscribe(
        &self,
        args: &Value,
        affinity: (u32, u32),
    ) -> Result<crate::input_events::Subscription, String> {
        let args: Args = serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
        let (rules, rects) = validate(&args)?;
        let (fallback_color, alpha_mode) = source_color(args.source_color.as_ref())?;
        let mut state = self.inner.lock().unwrap();
        let canonical = *state
            .affinities
            .entry(affinity)
            .or_insert_with(|| canonical_drm_device(affinity));
        if state.streams.len() >= 8 || state.streams.values().any(|s| s.window == args.window_id) {
            return Err("visual budget: max8 windows, one subscription per window".into());
        }
        let mut error = [0 as c_char; 512];
        if let std::collections::hash_map::Entry::Vacant(entry) = state.contexts.entry(canonical) {
            let context =
                unsafe { visual_context_create(canonical.0, canonical.1, error.as_mut_ptr()) };
            if context.is_null() {
                return Err(error_text(&error));
            }
            entry.insert(Arc::new(Device {
                context: Context(context),
                submission: Mutex::new(()),
            }));
        }
        let device = state.contexts[&canonical].clone();
        let program = {
            let _submission = device.submission.lock().unwrap();
            unsafe {
                visual_program_create(
                    device.context.0,
                    rules.as_ptr(),
                    rects.as_ptr(),
                    rules.len() as u32,
                    error.as_mut_ptr(),
                )
            }
        };
        if program.is_null() {
            return Err(error_text(&error));
        }
        state.next += 1;
        let id = (1u64 << 52) + state.next;
        let (sender, events) = mpsc::channel(256);
        let (terminal, end) = watch::channel(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        state.streams.insert(
            id,
            Stream {
                window: args.window_id.clone(),
                affinity,
                program: Arc::new(ProgramSlot {
                    native: Mutex::new(Program(program, false)),
                    device,
                }),
                names: args.rules.into_iter().map(|r| r.id).collect(),
                sender,
                end: terminal,
                bytes: bytes.clone(),
                deadline: Instant::now() + Duration::from_millis(args.duration_ms),
                period: Duration::from_secs_f64(1.0 / f64::from(args.max_fps)),
                last: None,
                sequence: 0,
                frames: 0,
                gpu_ns: 0,
                acquisition_ns: 0,
                in_flight: false,
                signature: None,
                admission: HashMap::new(),
                fallback_color,
                alpha_mode,
                color_profile: None,
                epoch: 0,
            },
        );
        Ok(crate::input_events::Subscription {
            id,
            initial: json!({"windowId":args.window_id,"coordinateSpace":"buffer","drmDevice":{"major":affinity.0,"minor":affinity.1},"pixelsReadable":false,"maxFps":args.max_fps}),
            events,
            bytes,
            end,
        })
    }
    pub(crate) fn subscribe_program(
        &self,
        args: &Value,
        affinity: (u32, u32),
        width: u32,
        height: u32,
    ) -> Result<crate::input_events::Subscription, String> {
        let args: ProgramArgs = serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
        let config = args.config(width, height)?;
        let (fallback_color, alpha_mode) = source_color(args.source_color.as_ref())?;
        let mut error = [0 as c_char; 512];
        let device = {
            let mut state = self.inner.lock().unwrap();
            if state.streams.len() >= 8
                || state.streams.values().any(|s| s.window == args.window_id)
            {
                return Err("visual budget: max8 windows, one subscription per window".into());
            }
            let canonical = *state
                .affinities
                .entry(affinity)
                .or_insert_with(|| canonical_drm_device(affinity));
            if let std::collections::hash_map::Entry::Vacant(entry) =
                state.contexts.entry(canonical)
            {
                let context =
                    unsafe { visual_context_create(canonical.0, canonical.1, error.as_mut_ptr()) };
                if context.is_null() {
                    return Err(error_text(&error));
                }
                entry.insert(Arc::new(Device {
                    context: Context(context),
                    submission: Mutex::new(()),
                }));
            }
            state.contexts[&canonical].clone()
        };
        let passes: Vec<_> = args
            .passes
            .iter()
            .map(|p| ShaderPass {
                source: p.source.as_ptr().cast(),
                source_size: p.source.len() as u32,
                groups: p.dispatch,
            })
            .collect();
        // Creation uses new program-owned resources and never submits queue
        // work. Shader compilation must not hold the frame submission lock.
        let pointer = {
            unsafe {
                visual_runtime_create(
                    device.context.0,
                    &config,
                    passes.as_ptr(),
                    args.parameters.as_ptr(),
                    error.as_mut_ptr(),
                )
            }
        };
        if pointer.is_null() {
            return Err(error_text(&error));
        }
        let program = Arc::new(ProgramSlot {
            native: Mutex::new(Program(pointer, true)),
            device,
        });
        let mut state = self.inner.lock().unwrap();
        if state.streams.len() >= 8 || state.streams.values().any(|s| s.window == args.window_id) {
            return Err("visual subscription budget changed while compiling".into());
        }
        state.next += 1;
        let id = (1u64 << 52) + state.next;
        let (sender, events) = mpsc::channel(256);
        let (terminal, end) = watch::channel(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        state.streams.insert(
            id,
            Stream {
                window: args.window_id.clone(),
                affinity,
                program,
                names: Vec::new(),
                sender,
                end: terminal,
                bytes: bytes.clone(),
                deadline: Instant::now() + Duration::from_millis(args.duration_ms),
                period: Duration::from_secs_f64(1.0 / f64::from(args.max_fps)),
                last: None,
                sequence: 0,
                frames: 0,
                gpu_ns: 0,
                acquisition_ns: 0,
                in_flight: false,
                signature: None,
                admission: HashMap::new(),
                fallback_color,
                alpha_mode,
                color_profile: None,
                epoch: 0,
            },
        );
        Ok(crate::input_events::Subscription {
            id,
            initial: json!({"windowId":args.window_id,"bufferWidth":width,"bufferHeight":height,"coordinateSpace":"buffer","frameFormat":"rgba16f-linear-bt2020","resultBytes":args.result_bytes,"pixelsReadable":false,"maxFps":args.max_fps}),
            events,
            bytes,
            end,
        })
    }
    pub(crate) fn is_visual(id: u64) -> bool {
        id >= (1u64 << 52)
    }
    pub(crate) fn interested(&self, window: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .streams
            .values()
            .any(|s| s.window == window && s.deadline > Instant::now())
    }
    pub(crate) fn active_windows(&self) -> HashSet<String> {
        let now = Instant::now();
        self.inner
            .lock()
            .unwrap()
            .streams
            .values()
            .filter(|s| s.deadline > now)
            .map(|s| s.window.clone())
            .collect()
    }
    pub(crate) fn notice(&self, window: &str, reason: &'static str) {
        for stream in self
            .inner
            .lock()
            .unwrap()
            .streams
            .values_mut()
            .filter(|s| s.window == window)
        {
            *stream.admission.entry(reason).or_default() += 1;
        }
    }
    pub(crate) fn metrics(&self, id: u64) -> Result<Value, String> {
        let state = self.inner.lock().unwrap();
        let s = state
            .streams
            .get(&id)
            .ok_or("visual subscription is not active")?;
        Ok(
            json!({"framesProcessed":s.frames,"processingMs":s.gpu_ns as f64/1e6,"meanProcessingMs":if s.frames>0{s.gpu_ns as f64/1e6/s.frames as f64}else{0.0},"acquisitionMs":s.acquisition_ns as f64/1e6,"analysisInFlight":s.in_flight,"admission":s.admission,"leaseRemainingMs":s.deadline.saturating_duration_since(Instant::now()).as_millis()}),
        )
    }
    pub(crate) fn stop(&self, id: u64, reason: &str) -> Result<Value, String> {
        let stream = self.inner.lock().unwrap().streams.remove(&id);
        Ok(if let Some(stream) = stream {
            let error = reason.strip_prefix("visual_error: ");
            let end = json!({"reason":reason,"error":error,"sequence":stream.sequence,"framesProcessed":stream.frames,"processingMs":stream.gpu_ns as f64/1e6,"acquisitionMs":stream.acquisition_ns as f64/1e6,"meanProcessingMs":if stream.frames>0 {stream.gpu_ns as f64/1e6/stream.frames as f64}else{0.0}});
            stream.end.send_replace(Some(end.clone()));
            end
        } else {
            json!({"reason":"already_stopped"})
        })
    }
    pub(crate) fn close_window(&self, window: &str, reason: &str) {
        if matches!(reason, "window_destroyed" | "client_disconnected") {
            self.inner.lock().unwrap().snapshots.remove(window);
        }
        let ids = self
            .inner
            .lock()
            .unwrap()
            .streams
            .iter()
            .filter_map(|(id, s)| (s.window == window).then_some(*id))
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.stop(id, reason);
        }
    }
    pub(crate) fn expire(&self) {
        let ids = self
            .inner
            .lock()
            .unwrap()
            .streams
            .iter()
            .filter_map(|(id, s)| (s.deadline <= Instant::now()).then_some(*id))
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.stop(id, "expired");
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture(
        &self,
        window: &str,
        affinity: (u32, u32),
        key: u64,
        width: u32,
        height: u32,
        format: u32,
        planes: &[Plane],
        serial: u64,
        timestamp: f64,
        color: Option<&ColorDescription>,
        wait_acquire: impl FnOnce() -> Result<(), String>,
    ) -> Result<Option<CapturedFrame>, String> {
        let selected = {
            let mut state = self.inner.lock().unwrap();
            let Some((id, stream)) = state.streams.iter_mut().find(|(_, s)| s.window == window)
            else {
                return Ok(None);
            };
            if stream.affinity != affinity {
                return Err("DRM device changed; resubscribe".into());
            }
            if stream
                .signature
                .is_some_and(|sig| sig != (width, height, format))
            {
                return Err("buffer geometry/format changed; resubscribe".into());
            }
            stream.signature = Some((width, height, format));
            let now = Instant::now();
            if now >= stream.deadline {
                return Ok(None);
            }
            if stream.in_flight {
                *stream.admission.entry("analysisBusy").or_default() += 1;
                return Ok(None);
            }
            if stream
                .last
                .is_some_and(|t| now.duration_since(t) < stream.period)
            {
                *stream.admission.entry("rateLimited").or_default() += 1;
                return Ok(None);
            }
            stream.in_flight = true;
            (
                *id,
                stream.program.clone(),
                now,
                stream.fallback_color.clone(),
                stream.alpha_mode,
            )
        };
        let (id, program, start, fallback_color, alpha_mode) = selected;
        let (acquired, profile, color) = {
            let native = program.native.lock().unwrap();
            let mut wait_acquire = Some(wait_acquire);
            let mut producer_ready = false;
            loop {
                let submission = match program.device.submission.try_lock() {
                    Ok(guard) => guard,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        let mut state = self.inner.lock().unwrap();
                        if let Some(stream) = state.streams.get_mut(&id) {
                            stream.in_flight = false;
                            *stream.admission.entry("deviceBusy").or_default() += 1;
                        }
                        return Ok(None);
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err("GPU device worker poisoned".into());
                    }
                };
                if !producer_ready {
                    // First check availability so busy streams skip immediately,
                    // but do not monopolize a device while a producer submits work.
                    drop(submission);
                    wait_acquire.take().unwrap()()?;
                    producer_ready = true;
                    if !self.inner.lock().unwrap().streams.contains_key(&id) {
                        return Ok(None);
                    }
                    continue;
                }
                let mut error = [0 as c_char; 512];
                let description = color
                    .or(fallback_color.as_ref())
                    .ok_or("untagged visual source requires an explicit sourceColor assumption")?;
                let profile = description.gpu_profile(alpha_mode)?;
                let metadata = json!({"assumed":color.is_none(),"description":description.source_metadata(),"alphaMode":alpha_mode});
                let configured = unsafe {
                    (if native.1 {
                        visual_runtime_set_color
                    } else {
                        visual_set_color
                    })(native.0, &profile, error.as_mut_ptr())
                };
                if configured < 0 {
                    return Err(error_text(&error));
                }
                let n = unsafe {
                    (if native.1 {
                        visual_runtime_capture
                    } else {
                        visual_capture
                    })(
                        native.0,
                        key,
                        width,
                        height,
                        format,
                        planes.as_ptr(),
                        planes.len() as u32,
                        error.as_mut_ptr(),
                    )
                };
                drop(submission);
                if n < 0 {
                    return Err(error_text(&error));
                }
                break (start.elapsed().as_nanos(), profile, metadata);
            }
        };
        let (epoch, history_valid) =
            if let Some(stream) = self.inner.lock().unwrap().streams.get_mut(&id) {
                stream.last = Some(start);
                let history_valid = stream.color_profile == Some(profile);
                if !history_valid {
                    stream.epoch += 1;
                }
                stream.color_profile = Some(profile);
                (stream.epoch, history_valid)
            } else {
                (0, false)
            };
        Ok(Some(CapturedFrame {
            id,
            program,
            serial,
            timestamp,
            acquisition_ns: acquired,
            color,
            epoch,
            history_valid,
        }))
    }
    pub(crate) fn complete(&self, captured: CapturedFrame) {
        let start = Instant::now();
        let mut events = [Event::default(); 16];
        let mut result = [0u8; 256];
        let programmable;
        let mut error = [0 as c_char; 512];
        let n = {
            let native = captured.program.native.lock().unwrap();
            let _submission = captured.program.device.submission.lock().unwrap();
            programmable = native.1;
            unsafe {
                if programmable {
                    visual_runtime_analyze(native.0, result.as_mut_ptr(), error.as_mut_ptr())
                } else {
                    visual_analyze(native.0, events.as_mut_ptr(), error.as_mut_ptr())
                }
            }
        };
        if n < 0 {
            let _ = self.stop(
                captured.id,
                &format!("visual_error: {}", error_text(&error)),
            );
            return;
        }
        let mut failed = false;
        {
            let mut state = self.inner.lock().unwrap();
            let Some(stream) = state.streams.get_mut(&captured.id) else {
                return;
            };
            stream.in_flight = false;
            stream.acquisition_ns += captured.acquisition_ns;
            stream.gpu_ns += captured.acquisition_ns + start.elapsed().as_nanos();
            stream.frames += 1;
            if programmable {
                stream.sequence += 1;
                let words: Vec<u32> = result[..n as usize]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect();
                let value = json!({"windowId":stream.window,"words":words,"frame":stream.frames,"commitSerial":captured.serial,"timestampMs":captured.timestamp,"deliveryTimestampMs":self.start.elapsed().as_secs_f64()*1000.0,"sequence":stream.sequence,"sourceColor":captured.color,"epoch":captured.epoch,"historyValid":captured.history_valid});
                let bytes = value.to_string().len();
                stream.bytes.fetch_add(bytes, Ordering::Relaxed);
                if stream
                    .sender
                    .try_send(crate::input_events::QueuedInput { value, bytes })
                    .is_err()
                {
                    stream.bytes.fetch_sub(bytes, Ordering::Relaxed);
                    failed = true;
                }
            } else {
                // Atomic append order is not deterministic; publish stable rule order per commit.
                events[..n as usize].sort_by_key(|e| e.rule);
                for event in &events[..n as usize] {
                    stream.sequence += 1;
                    let value = json!({"windowId":stream.window,"ruleId":stream.names[event.rule as usize],"active":event.active!=0,"frame":event.frame,"commitSerial":captured.serial,"timestampMs":captured.timestamp,"deliveryTimestampMs":self.start.elapsed().as_secs_f64()*1000.0,"sequence":stream.sequence,"sourceColor":captured.color,"epoch":captured.epoch,"historyValid":captured.history_valid});
                    let bytes = value.to_string().len();
                    stream.bytes.fetch_add(bytes, Ordering::Relaxed);
                    if stream
                        .sender
                        .try_send(crate::input_events::QueuedInput { value, bytes })
                        .is_err()
                    {
                        stream.bytes.fetch_sub(bytes, Ordering::Relaxed);
                        failed = true;
                        break;
                    }
                }
            }
        }
        if failed {
            let _ = self.stop(
                captured.id,
                "visual_error: visual event queue overflow; stream has a gap",
            );
        }
    }
    pub(crate) fn timestamp(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_pattern(
        stream: &Stream,
        pattern: u32,
        events: &mut [Event; 16],
        error: &mut [c_char; 512],
    ) -> i32 {
        let program = stream.program.native.lock().unwrap();
        let _submission = stream.program.device.submission.lock().unwrap();
        unsafe { visual_test(program.0, pattern, events.as_mut_ptr(), error.as_mut_ptr()) }
    }
    #[test]
    fn programmable_configuration_has_bounded_resources_and_explicit_color() {
        let base = json!({"windowId":"w","passes":[{"source":"#version 450\nvoid main(){}","dispatch":[1,1,1]}],"resultBytes":32,"sourceColor":{"transfer":"srgb","primaries":"bt709","alpha":"opaque"},"feedback":"previousResult"});
        let args: ProgramArgs = serde_json::from_value(base.clone()).unwrap();
        let cfg = args.config(1280, 672).unwrap();
        assert_eq!(cfg.state_bytes, 32);
        assert_eq!(cfg.feedback, 1);
        for (key, value) in [
            ("resultBytes", json!(260)),
            ("stateBytes", json!(65540)),
            ("scratchBytes", json!(1048580)),
            ("parameters", json!([0, 1])),
            ("feedback", json!("unknown")),
            ("maxFps", json!(0)),
        ] {
            let mut bad = base.clone();
            bad[key] = value;
            let args: ProgramArgs = serde_json::from_value(bad).unwrap();
            assert!(args.config(1280, 672).is_err(), "{key}");
        }
        let mut bad = base.clone();
        bad["sourceColor"]["transfer"] = json!("unknown");
        assert!(
            serde_json::from_value::<ProgramArgs>(bad)
                .unwrap()
                .config(1280, 672)
                .is_err()
        );
        let mut bad = base;
        bad["shaderFile"] = json!("anything");
        assert!(serde_json::from_value::<ProgramArgs>(bad).is_err());
    }
    #[test]
    fn rejects_pixel_probes_and_bad_rules() {
        let base = json!({"windowId":"w","rules":[{"id":"a","rect":[0,0,1,1],"kind":"luminance"}]});
        assert!(validate(&serde_json::from_value(base).unwrap()).is_err());
        let base = json!({"windowId":"w","rules":[{"id":"a","rect":[0,0,8,8],"kind":"luminance","shader":"pixels"}]});
        assert!(serde_json::from_value::<Args>(base).is_err());
    }
    #[test]
    fn expired_visual_leases_stop_requesting_frames_before_stream_cleanup() {
        let Ok(minor) = std::env::var("WAYLAND_MCP_TEST_DRM_MINOR") else {
            return;
        };
        let hub = VisualHub::default();
        let sub = hub.subscribe(
            &json!({"windowId":"fixture","rules":[{"id":"signal","rect":[0,0,8,8],"kind":"luminance"}]}),
            (226, minor.parse().unwrap()),
        ).unwrap();
        assert!(hub.interested("fixture"));
        assert!(hub.active_windows().contains("fixture"));
        hub.inner
            .lock()
            .unwrap()
            .streams
            .get_mut(&sub.id)
            .unwrap()
            .deadline = Instant::now() - Duration::from_secs(1);
        assert!(!hub.interested("fixture"));
        assert!(!hub.active_windows().contains("fixture"));
        hub.expire();
        assert_eq!(sub.end.borrow().as_ref().unwrap()["reason"], "expired");
    }

    #[test]
    fn program_compilation_does_not_hold_the_submission_lock() {
        let Ok(minor) = std::env::var("WAYLAND_MCP_TEST_DRM_MINOR") else {
            return;
        };
        let affinity = (226, minor.parse().unwrap());
        let hub = Arc::new(VisualHub::default());
        let device = hub.device(affinity).unwrap();
        let submission = device.submission.lock().unwrap();
        let compiler = hub.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = compiler.subscribe_program(&json!({"windowId":"w","passes":[{"source":"#version 450\nlayout(local_size_x=1) in; layout(set=0,binding=5,std430) buffer Result { uint words[]; } result; void main(){result.words[0]=42u;}","dispatch":[1,1,1]}],"resultBytes":4}), affinity, 8, 8);
            sender.send(result.map(|s| s.id)).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(10));
        drop(submission);
        worker.join().unwrap();
        let id = result
            .expect("compilation blocked on frame submission")
            .unwrap();
        hub.stop(id, "unsubscribed").unwrap();
    }

    #[test]
    fn busy_analysis_skips_acquisition_without_waiting_for_producer() {
        let Ok(minor) = std::env::var("WAYLAND_MCP_TEST_DRM_MINOR") else {
            return;
        };
        let minor = minor.parse().unwrap();
        let hub = VisualHub::default();
        let sub = hub.subscribe(&json!({"windowId":"w","rules":[{"id":"signal","rect":[0,0,8,8],"kind":"luminance"}]}), (226,minor)).unwrap();
        {
            let mut state = hub.inner.lock().unwrap();
            state.streams.get_mut(&sub.id).unwrap().in_flight = true;
        }
        assert!(
            hub.capture(
                "w",
                (226, minor),
                0,
                64,
                64,
                0x34325258,
                &[],
                1,
                0.0,
                None,
                || panic!("busy frames must not wait for producer fences")
            )
            .unwrap()
            .is_none()
        );
        let device = {
            let mut state = hub.inner.lock().unwrap();
            let stream = state.streams.get_mut(&sub.id).unwrap();
            stream.in_flight = false;
            stream.program.device.clone()
        };
        let submission = device.submission.lock().unwrap();
        assert!(
            hub.capture(
                "w",
                (226, minor),
                0,
                64,
                64,
                0x34325258,
                &[],
                2,
                0.0,
                None,
                || panic!("busy devices must not wait for producer fences")
            )
            .unwrap()
            .is_none()
        );
        drop(submission);
        let metrics = hub.metrics(sub.id).unwrap();
        assert_eq!(metrics["admission"]["analysisBusy"], 1);
        assert_eq!(metrics["admission"]["deviceBusy"], 1);
        assert_eq!(metrics["analysisInFlight"], false);
        assert_eq!(metrics["framesProcessed"], 0);
    }
    #[test]
    fn vulkan_predicates_without_pixel_readback() {
        // Real hardware integration, opt in with the DRM render-device minor.
        let Ok(minor) = std::env::var("WAYLAND_MCP_TEST_DRM_MINOR") else {
            return;
        };
        let hub = VisualHub::default();
        let args = json!({"windowId":"w","rules":[{"id":"dark","rect":[0,0,8,8],"kind":"luminance"},{"id":"changed","rect":[0,0,8,8],"kind":"change","threshold":0.1}]});
        let sub = hub.subscribe(&args, (226, minor.parse().unwrap())).unwrap();
        let mut state = hub.inner.lock().unwrap();
        let stream = state.streams.get_mut(&sub.id).unwrap();
        let mut events = [Event::default(); 16];
        let mut error = [0 as c_char; 512];
        let n = test_pattern(stream, 0xffffffff, &mut events, &mut error);
        assert_eq!(n, 1, "{}", error_text(&error));
        assert_eq!(events[0].active, 0);
        let n = test_pattern(stream, 0xff000000, &mut events, &mut error);
        assert_eq!(n, 2, "{}", error_text(&error));
        assert!(events[..2].iter().all(|e| e.active == 1));
        let n = test_pattern(stream, 0xff000000, &mut events, &mut error);
        assert_eq!(n, 0);
        drop(state);
        hub.stop(sub.id, "test_done").unwrap();
        let args = json!({"windowId":"w","rules":[
            {"id":"debounced","rect":[0,0,8,8],"kind":"change","threshold":0.1,"debounceFrames":2},
            {"id":"bright","rect":[0,0,8,8],"kind":"luminance","polarity":"above","debounceFrames":2,"cooldownFrames":3}
        ]});
        let sub = hub.subscribe(&args, (226, minor.parse().unwrap())).unwrap();
        let mut state = hub.inner.lock().unwrap();
        let stream = state.streams.get_mut(&sub.id).unwrap();
        for (pattern, expected) in [
            (0xffffffff, 0),
            (0xff000000, 0),
            (0xffffffff, 1),
            (0xffffffff, 1),
        ] {
            let n = test_pattern(stream, pattern, &mut events, &mut error);
            // Third frame: change debounced across two changed frames; luminance
            // isn't stable yet. Fourth frame: luminance becomes stable and emits.
            assert_eq!(n, expected, "{}", error_text(&error));
        }
    }
    #[test]
    fn vulkan_background_budget() {
        let Ok(minor) = std::env::var("WAYLAND_MCP_TEST_DRM_MINOR") else {
            return;
        };
        let hub = VisualHub::default();
        let rules = (0..16)
            .map(|i| json!({"id":format!("region-{i}"),"rect":[i*128,0,128,64],"kind":"luminance"}))
            .collect::<Vec<_>>();
        let sub = hub
            .subscribe(
                &json!({"windowId":"w","rules":rules}),
                (226, minor.parse().unwrap()),
            )
            .unwrap();
        let mut state = hub.inner.lock().unwrap();
        let stream = state.streams.get_mut(&sub.id).unwrap();
        let mut events = [Event::default(); 16];
        let mut error = [0 as c_char; 512];
        let start = Instant::now();
        for i in 0..1000 {
            let pattern = if i % 2 == 0 { 0xffffffff } else { 0xff000000 };
            let n = test_pattern(stream, pattern, &mut events, &mut error);
            assert_eq!(n, 16, "{}", error_text(&error));
            assert!(
                events
                    .iter()
                    .all(|e| e.active == (i % 2) && e.frame == i + 1)
            );
        }
        eprintln!(
            "GPU-only 16-region/131072-pixel benchmark: {:.3} ms/frame, 1000 submissions",
            start.elapsed().as_secs_f64()
        );
        // GPU steady-state no-transition frames return an empty event packet.
        for _ in 0..20 {
            assert_eq!(test_pattern(stream, 0xff000000, &mut events, &mut error), 0);
        }
    }
}
