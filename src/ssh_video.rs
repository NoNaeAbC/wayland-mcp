use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gui_backend_wayland::bridge::DisplayBuffer;
use crate::gui_color::{ColorDescription, GpuColorProfile};

pub(crate) const MIME: &str = "application/vnd.wayland-mcp.hevc";
pub(crate) const MAX_ACCESS_UNIT: usize = 16 * 1024 * 1024;
const MAX_PIXELS: u64 = 8_388_608;
const MAX_TONE_POINTS: usize = 16;
pub(crate) const SSH_DECODERS_ENV: &str = "WAYLAND_MCP_SSH_DECODERS_V1";
pub(crate) type CodecDevices = BTreeMap<String, u32>;
static SSH_DECODERS: OnceLock<CodecDevices> = OnceLock::new();

pub(crate) fn timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("WAYLAND_MCP_TRACE_TIMING").is_some())
}

pub(crate) fn timing_start() -> Option<Instant> {
    timing_enabled().then(Instant::now)
}

pub(crate) fn timing_ms(start: Option<Instant>) -> f64 {
    start.map_or(0.0, |start| start.elapsed().as_secs_f64() * 1000.0)
}

pub(crate) fn enable_ssh_worker() -> Result<(), String> {
    let devices = match std::env::var(SSH_DECODERS_ENV) {
        Ok(value) => serde_json::from_str::<CodecDevices>(&value)
            .map_err(|error| format!("invalid SSH decoder capabilities: {error}"))?,
        Err(std::env::VarError::NotPresent) => CodecDevices::new(),
        Err(error) => return Err(format!("invalid SSH decoder capabilities: {error}")),
    };
    for node in devices.keys() {
        check_device_path(node)?;
    }
    SSH_DECODERS
        .set(devices)
        .map_err(|_| "SSH worker is already configured".into())
}

pub(crate) fn is_ssh_worker() -> bool {
    SSH_DECODERS.get().is_some()
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FrameRequest {
    pub window_id: String,
    pub after_commit_serial: Option<u64>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    chroma: Option<u32>,
    bit_depth: Option<u32>,
    screen_content: Option<bool>,
    pub capture_gpu: Option<String>,
    pub encode_gpu: Option<String>,
    pub decode_gpu: Option<String>,
    #[serde(default = "default_qp")]
    pub qp: u8,
    #[serde(default)]
    pub tone_map: ToneMap,
    #[serde(default)]
    pub tone_curve: Vec<[f32; 2]>,
    #[serde(default = "default_exposure")]
    pub exposure: f32,
    #[serde(default)]
    pub background: Background,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[repr(u32)]
pub(crate) enum ToneMap {
    Clip,
    Knee,
    Reinhard,
    Curve,
    #[default]
    Auto,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Background {
    #[default]
    Black,
    Checkerboard,
}

fn default_timeout() -> u64 {
    1000
}
fn default_qp() -> u8 {
    20
}
fn default_exposure() -> f32 {
    1.0
}

pub(crate) struct VideoSnapshot {
    pub options: VideoOptions,
    pub selection: VideoSelection,
    pub(crate) stream: NativeStream,
}

#[derive(Eq, PartialEq)]
pub(crate) struct VideoSelection {
    format: VideoFormat,
    capture_gpu: Option<String>,
    encode_gpu: String,
    decode_gpu: String,
    predictive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct VideoFormat {
    chroma: u32,
    depth: u32,
    screen_content: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BridgeFrame {
    depth: u32,
    chroma: u32,
    screen_content: bool,
    decode_gpu: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prediction: Option<FramePrediction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alpha_prediction: Option<FramePrediction>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    reuse_alpha: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FramePrediction {
    group: u64,
    poc: u32,
}

pub(crate) struct BridgePicture {
    pub frame: BridgeFrame,
    pub color: Vec<u8>,
    pub alpha: Vec<u8>,
}

impl BridgePicture {
    pub(crate) fn has_alpha(&self) -> bool {
        self.frame.reuse_alpha || !self.alpha.is_empty()
    }

    pub(crate) fn decode_gpu(&self) -> &str {
        &self.frame.decode_gpu
    }
}

const WITH_ALPHA: u32 = 2;
const ALPHA_PICTURE: u32 = 4;
const PREDICTIVE_PLAN: u32 = 64;
const INDEPENDENT: u32 = u32::MAX;
const GROUP_LENGTH: u32 = 240;
const VA_DECODERS_SHIFT: u32 = 16;

pub(crate) fn has_alpha(format: u32) -> bool {
    // DRM_FORMAT_ARGB8888, ABGR8888, ARGB2101010, ABGR2101010, ABGR16161616F
    matches!(
        format,
        0x34325241 | 0x34324241 | 0x30335241 | 0x30334241 | 0x48344241
    )
}

#[repr(C)]
pub(crate) struct BridgePlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum BridgePixels<'a> {
    Shm {
        fd: BorrowedFd<'a>,
        stride: u32,
        offset: u64,
    },
    Dmabuf(&'a BridgePlane),
}

#[repr(C)]
struct NativeEncodeTarget {
    device: *const c_char,
    stream: *mut c_void,
    with_alpha: bool,
    compare_alpha: bool,
}

#[repr(C)]
struct NativeBitstream {
    color: *const u8,
    alpha: *const u8,
    color_size: u32,
    alpha_size: u32,
    poc: u32,
    alpha_poc: u32,
    scc: bool,
    reuse_alpha: bool,
}

#[repr(C)]
struct NativeDisplayTarget {
    dma: *const BridgePlane,
    shm: *const NativeShm,
}

#[repr(C)]
pub(crate) struct NativeShm {
    pub fd: i32,
    pub stride: u32,
    pub offset: u64,
}

impl VideoFormat {
    const fn new(chroma: u32, depth: u32, screen_content: bool) -> Self {
        Self {
            chroma,
            depth,
            screen_content: screen_content as u32,
        }
    }

    fn scc(self) -> bool {
        self.screen_content != 0
    }

    fn bit(self) -> Option<u32> {
        FORMATS
            .iter()
            .position(|entry| *entry == self)
            .map(|i| 1 << i)
    }

    fn supported_by(self, mask: u32) -> bool {
        self.bit().is_some_and(|bit| mask & bit != 0)
    }
}

const SCC: VideoFormat = VideoFormat::new(444, 10, true);
const MAIN: VideoFormat = VideoFormat::new(420, 8, false);
const AUTO_FORMATS: [VideoFormat; 2] = [SCC, MAIN];
const STREAM_FORMATS: [VideoFormat; 5] = [
    SCC,
    VideoFormat::new(444, 10, false),
    VideoFormat::new(420, 10, false),
    VideoFormat::new(444, 8, false),
    MAIN,
];
const STREAM_FORMATS_8: [VideoFormat; 5] = [
    VideoFormat::new(444, 8, false),
    MAIN,
    SCC,
    VideoFormat::new(444, 10, false),
    VideoFormat::new(420, 10, false),
];
// The order defines the capability bits in WAYLAND_MCP_SSH_DECODERS_V1.
const FORMATS: [VideoFormat; 9] = [
    SCC,
    MAIN,
    VideoFormat::new(420, 8, true),
    VideoFormat::new(420, 10, false),
    VideoFormat::new(420, 10, true),
    VideoFormat::new(422, 10, false),
    VideoFormat::new(444, 8, false),
    VideoFormat::new(444, 8, true),
    VideoFormat::new(444, 10, false),
];

#[repr(C)]
pub(crate) struct VideoOptions {
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub format: u32,
    pub depth: u32,
    pub chroma: u32,
    pub mode: u32,
    pub tone_map: u32,
    pub exposure: f32,
    pub background: u32,
    pub tone_points: u32,
    pub curve: [[f32; 2]; MAX_TONE_POINTS],
    source_stride: u32,
    source_offset: u32,
    output_offset: u32,
    reserved: u32,
}

fn stream_frame_rate() -> u32 {
    120
}

fn stream_quality() -> u8 {
    24
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum StreamRateControl {
    #[default]
    Auto,
    #[serde(skip)]
    TransportVbr(u32),
    Vbr {
        #[serde(default = "stream_frame_rate")]
        frame_rate: u32,
        #[serde(default)]
        target_bitrate: Option<u32>,
    },
    ConstantQuality {
        #[serde(default = "stream_frame_rate")]
        frame_rate: u32,
        #[serde(default = "stream_quality")]
        quality: u8,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(C)]
struct EncodeRate {
    mode: u32,
    frame_rate: u32,
    target_bitrate: u32,
    quality: u32,
    automatic_bitrate: u32,
}

impl StreamRateControl {
    fn select<T>(
        self,
        (width, height): (u32, u32),
        alpha: bool,
        mut choose: impl FnMut(&EncodeRate) -> Result<T, String>,
    ) -> Result<(T, EncodeRate, Self), String> {
        let mut attempt = |mode: Self| {
            let rate = mode.resolve(width, height, alpha)?;
            Ok((choose(&rate)?, rate, mode))
        };
        match self {
            Self::TransportVbr(target_bitrate) => {
                if !alpha
                    && let Ok(selected) = attempt(Self::Vbr {
                        frame_rate: 60,
                        target_bitrate: Some(target_bitrate),
                    })
                {
                    return Ok(selected);
                }
            }
            Self::Auto => {}
            _ => return attempt(self),
        }
        attempt(Self::ConstantQuality {
            frame_rate: stream_frame_rate(),
            quality: stream_quality(),
        })
        .or_else(|quality_error| {
            attempt(Self::Vbr {
                frame_rate: stream_frame_rate(),
                target_bitrate: None,
            })
            .map_err(|vbr_error| {
                format!("Constant quality failed with {quality_error}. VBR failed with {vbr_error}")
            })
        })
    }

    fn resolve(self, width: u32, height: u32, alpha: bool) -> Result<EncodeRate, String> {
        let rate = match self {
            Self::Auto | Self::TransportVbr(_) => {
                return Err("automatic rate control requires codec selection".into());
            }
            Self::Vbr {
                frame_rate,
                target_bitrate,
            } => {
                let auto_bitrate = (u64::from(width) * u64::from(height) * u64::from(frame_rate)
                    / if alpha { 2 } else { 4 })
                .clamp(250_000, 80_000_000) as u32;
                let explicit_bitrate = target_bitrate.filter(|_| !alpha);
                let target_bitrate = explicit_bitrate.unwrap_or(auto_bitrate);
                if target_bitrate == 0 || target_bitrate > u32::MAX / 2 {
                    return Err("invalid VBR target bitrate in bits per second".into());
                }
                EncodeRate {
                    mode: 1,
                    frame_rate,
                    target_bitrate,
                    quality: 0,
                    automatic_bitrate: u32::from(explicit_bitrate.is_none()),
                }
            }
            Self::ConstantQuality {
                frame_rate,
                quality,
            } => {
                if !(1..=51).contains(&quality) {
                    return Err("constantQuality requires quality in 1..51".into());
                }
                EncodeRate {
                    mode: 2,
                    frame_rate,
                    target_bitrate: 0,
                    quality: u32::from(if alpha { quality.min(16) } else { quality }),
                    automatic_bitrate: 0,
                }
            }
        };
        if !(1..=u32::from(u16::MAX)).contains(&rate.frame_rate) {
            return Err("invalid H.265 frameRate".into());
        }
        Ok(rate)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PacketInfo {
    pub width: u32,
    pub height: u32,
    coded_width: u32,
    coded_height: u32,
    depth: u32,
    chroma: u32,
    screen_content: bool,
    decode_gpu: Option<String>,
    access_unit_sha256: [u8; 32],
}

impl FrameRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self
            .requested_format()
            .is_some_and(|format| format.bit().is_none())
        {
            return Err(
                "H.265 supports 4:2:0 and 4:4:4 at 8/10 bits, plus 10-bit 4:2:2 without SCC".into(),
            );
        }
        if self.qp > 51 || !self.exposure.is_finite() || !(0.01..=32.0).contains(&self.exposure) {
            return Err("videoFrame requires QP in 0..51 and exposure in 0.01..32".into());
        }
        if self.tone_map == ToneMap::Curve {
            if !(2..=MAX_TONE_POINTS).contains(&self.tone_curve.len())
                || self.tone_curve.first() != Some(&[0.0, 0.0])
                || self.tone_curve.iter().any(|[input, output]| {
                    !(0.0..=65504.0).contains(input) || !(0.0..=1.0).contains(output)
                })
                || self
                    .tone_curve
                    .windows(2)
                    .any(|pair| pair[0][0] >= pair[1][0])
            {
                return Err("toneCurve requires 2..16 increasing input knots starting at [0,0] and outputs in 0..1".into());
            }
        } else if !self.tone_curve.is_empty() {
            return Err("toneCurve requires toneMap: \"curve\"".into());
        }
        if !(1..=10_000).contains(&self.timeout_ms) {
            return Err("videoFrame timeoutMs must be in 1..10000".into());
        }
        for path in [
            self.capture_gpu.as_deref(),
            self.encode_gpu.as_deref(),
            self.decode_gpu.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            check_device_path(path)?;
        }
        Ok(())
    }

    pub(crate) fn capture_affinity(&self) -> Result<Option<(u32, u32)>, String> {
        if let Some(node) = self.capture_gpu.as_deref() {
            return render_affinity(node).map(Some);
        }
        let nodes = render_nodes();
        match nodes.as_slice() {
            [node] => render_affinity(node).map(Some),
            _ => Ok(None),
        }
    }

    fn requested_format(&self) -> Option<VideoFormat> {
        if self.chroma.is_none() && self.bit_depth.is_none() && self.screen_content.is_none() {
            return None;
        }
        let chroma = self.chroma.unwrap_or(SCC.chroma);
        Some(VideoFormat::new(
            chroma,
            self.bit_depth.unwrap_or(SCC.depth),
            self.screen_content.unwrap_or(chroma == 444),
        ))
    }

    pub(crate) fn options(
        &self,
        width: u32,
        height: u32,
        format: u32,
        hdr: bool,
        selection: &VideoSelection,
    ) -> Result<VideoOptions, String> {
        let mut options = options_for_extent(
            width,
            height,
            selection.format.depth,
            selection.format.chroma,
        )?;
        options.format = format;
        options.tone_map = self.effective_tone_map(format, hdr) as u32;
        options.exposure = self.exposure;
        options.background = self.background as u32;
        options.tone_points = self.tone_curve.len() as u32;
        options.curve[..self.tone_curve.len()].copy_from_slice(&self.tone_curve);
        Ok(options)
    }

    pub(crate) fn effective_tone_map(&self, format: u32, hdr: bool) -> ToneMap {
        if !matches!(format.to_le_bytes(), [b'X' | b'A', b'B', b'4', b'H']) {
            return ToneMap::Clip;
        }
        match self.tone_map {
            ToneMap::Auto if hdr => ToneMap::Knee,
            ToneMap::Auto => ToneMap::Clip,
            mode => mode,
        }
    }
}

fn options_for_extent(
    width: u32,
    height: u32,
    depth: u32,
    chroma: u32,
) -> Result<VideoOptions, String> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("video dimensions must be in 1..4096".into());
    }
    let coded_width = width.max(160).next_multiple_of(64);
    let coded_height = height.max(128).next_multiple_of(64);
    if u64::from(coded_width) * u64::from(coded_height) > MAX_PIXELS {
        return Err("coded video extent exceeds the frame pixel limit".into());
    }
    Ok(VideoOptions {
        width,
        height,
        coded_width,
        coded_height,
        format: 0,
        depth,
        chroma,
        mode: 0,
        tone_map: 0,
        exposure: 1.0,
        background: 0,
        tone_points: 0,
        curve: [[0.0; 2]; MAX_TONE_POINTS],
        source_stride: 0,
        source_offset: 0,
        output_offset: 0,
        reserved: 0,
    })
}

fn check_device_path(path: &str) -> Result<(), String> {
    if !path.strip_prefix("/dev/dri/renderD").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return Err("select a GPU by its /dev/dri/renderD<number> node".into());
    }
    Ok(())
}

pub(crate) fn render_affinity(path: &str) -> Result<(u32, u32), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    check_device_path(path)?;
    let metadata = std::fs::metadata(path).map_err(|error| format!("open GPU {path}: {error}"))?;
    if !metadata.file_type().is_char_device() {
        return Err(format!("GPU {path} is not a DRM character device"));
    }
    Ok((libc::major(metadata.rdev()), libc::minor(metadata.rdev())))
}

fn device_for_affinity(affinity: (u32, u32)) -> Option<String> {
    let physical_device = |(major, minor): (u32, u32)| {
        std::fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device")).ok()
    };
    let expected = physical_device(affinity);
    render_nodes().into_iter().find(|name| {
        render_affinity(name).is_ok_and(|node| {
            node == affinity
                || expected
                    .as_ref()
                    .is_some_and(|device| physical_device(node).as_ref() == Some(device))
        })
    })
}

pub(crate) fn render_nodes() -> Vec<String> {
    let mut nodes = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.path().into_os_string().into_string().ok())
        .filter(|name| check_device_path(name).is_ok())
        .collect::<Vec<_>>();
    nodes.sort_unstable();
    nodes
}

pub(crate) fn bridge_render_nodes() -> &'static [String] {
    static NODES: OnceLock<Vec<String>> = OnceLock::new();
    NODES.get_or_init(render_nodes)
}

pub(crate) fn dma_buf_gpu(fd: i32) -> Option<String> {
    static DRIVERS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    let exporter = info
        .lines()
        .find_map(|line| line.strip_prefix("exp_name:"))?
        .trim();
    let exporter = exporter.strip_suffix("-drm").unwrap_or(exporter);
    let nodes = DRIVERS.get_or_init(|| {
        bridge_render_nodes()
            .iter()
            .filter_map(|node| {
                let (major, minor) = render_affinity(node).ok()?;
                let link =
                    std::fs::read_link(format!("/sys/dev/char/{major}:{minor}/device/driver"))
                        .ok()?;
                Some((link.file_name()?.to_str()?.to_owned(), node.clone()))
            })
            .collect()
    });
    let mut matching = nodes.iter().filter(|(driver, _)| driver == exporter);
    let (_, node) = matching.next()?;
    // An exporter name identifies a driver and can be shared by several GPUs.
    matching.next().is_none().then(|| node.clone())
}

unsafe extern "C" {
    fn ssh_video_create_stream() -> *mut c_void;
    fn ssh_video_capture_stream() -> *mut c_void;
    fn ssh_video_drop_stream(stream: *mut c_void);
    fn ssh_video_create_converter(device: *const c_char) -> *mut c_void;
    fn ssh_video_take_converter(pointer: *mut c_void);
    fn ssh_video_drop_converter(pointer: *mut c_void);
    fn ssh_video_encode_formats(
        device: *const c_char,
        formats: *const VideoFormat,
        count: u32,
        rate_mode: u32,
    ) -> u32;
    fn ssh_video_decode_formats(
        device: *const c_char,
        formats: *const VideoFormat,
        count: u32,
    ) -> u32;
    fn ssh_video_prepare_encoder(
        device: *const c_char,
        options: *const VideoOptions,
        scc: bool,
        rate: *const EncodeRate,
        error: *mut c_char,
        stream: *mut c_void,
    ) -> i32;
    fn ssh_video_capture_modifiers(
        device: *const c_char,
        format: u32,
        modifiers: *mut u64,
        capacity: u32,
    ) -> u32;
    fn ssh_video_encode(
        device: *const c_char,
        options: *const VideoOptions,
        scc: bool,
        qp: u8,
        rate: *const EncodeRate,
        stream_poc: u32,
        error: *mut c_char,
        stream: *mut c_void,
    ) -> *mut c_void;
    fn ssh_video_convert_buffer(
        options: *const VideoOptions,
        shm: *const NativeShm,
        plane: *const BridgePlane,
        plane_width: u32,
        profile: *const GpuColorProfile,
        implicit_sync: bool,
        target: *const NativeEncodeTarget,
        error: *mut c_char,
    ) -> i32;
    fn ssh_video_copy_dma_buf(
        device: *const c_char,
        options: *const VideoOptions,
        source: *const BridgePlane,
        destination: *const BridgePlane,
        destination_width: u32,
        implicit_sync: bool,
        error: *mut c_char,
    ) -> i32;
    fn ssh_video_convert_snapshot(
        snapshot: *mut c_void,
        options: *const VideoOptions,
        profile: *const GpuColorProfile,
        stream: *mut c_void,
        error: *mut c_char,
    ) -> i32;
    fn ssh_video_copy_snapshot(
        snapshot: *mut c_void,
        options: *const VideoOptions,
        plane: *const BridgePlane,
        destination_width: u32,
        error: *mut c_char,
    ) -> i32;
    fn ssh_video_packet_data(packet: *const c_void, size: *mut u32) -> *const u8;
    fn ssh_video_packet_destroy(packet: *mut c_void);
    fn ssh_video_decode_preview(
        device: *const c_char,
        options: *const VideoOptions,
        scc: bool,
        data: *const u8,
        size: u32,
        width: u32,
        height: u32,
        error: *mut c_char,
    ) -> *mut c_void;
    fn SHA256(data: *const u8, length: usize, digest: *mut u8) -> *mut u8;
    fn ssh_video_decode(
        device: *const c_char,
        options: *const VideoOptions,
        stream: *mut c_void,
        picture: *const NativeBitstream,
        target: *const NativeDisplayTarget,
        error: *mut c_char,
    ) -> i32;
}

pub(crate) struct NativeStream {
    pointer: std::ptr::NonNull<c_void>,
    owned: bool,
}

impl Default for NativeStream {
    fn default() -> Self {
        Self {
            pointer: std::ptr::NonNull::new(unsafe { ssh_video_create_stream() }).unwrap(),
            owned: true,
        }
    }
}

impl NativeStream {
    pub(crate) fn capture() -> Self {
        Self {
            pointer: std::ptr::NonNull::new(unsafe { ssh_video_capture_stream() }).unwrap(),
            owned: false,
        }
    }

    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.pointer.as_ptr()
    }
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        if self.owned {
            unsafe { ssh_video_drop_stream(self.as_ptr()) };
        }
    }
}

struct NativeConverter(std::ptr::NonNull<c_void>);

// The Vulkan resources have unique ownership. The warm-up thread finishes
// using them before handing them to the request thread for conversion.
unsafe impl Send for NativeConverter {}

impl Drop for NativeConverter {
    fn drop(&mut self) {
        unsafe { ssh_video_drop_converter(self.0.as_ptr()) }
    }
}

pub(crate) struct ConverterWarmup {
    node: String,
    receiver: Mutex<Option<mpsc::Receiver<NativeConverter>>>,
}

impl ConverterWarmup {
    pub(crate) fn start(node: &str) -> Option<Self> {
        let device = CString::new(node).ok()?;
        let (send, receive) = mpsc::sync_channel(1);
        tokio::task::spawn_blocking(move || {
            let started = timing_start();
            let pointer =
                std::ptr::NonNull::new(unsafe { ssh_video_create_converter(device.as_ptr()) });
            if started.is_some() {
                eprintln!(
                    "SSH GPU warm-up ready={} elapsed_ms={:.3}",
                    pointer.is_some(),
                    timing_ms(started)
                );
            }
            if let Some(pointer) = pointer {
                let _ = send.send(NativeConverter(pointer));
            }
        });
        Some(Self {
            node: node.to_owned(),
            receiver: Mutex::new(Some(receive)),
        })
    }

    pub(crate) fn adopt(&self, node: &str) {
        if self.node != node {
            return;
        }
        let receiver = self
            .receiver
            .lock()
            .ok()
            .and_then(|mut receiver| receiver.take());
        if let Some(converter) = receiver.and_then(|receiver| receiver.recv().ok()) {
            let converter = std::mem::ManuallyDrop::new(converter);
            unsafe { ssh_video_take_converter(converter.0.as_ptr()) };
        }
    }
}

pub(crate) fn capture_modifiers(device: &str, format: u32) -> Vec<u64> {
    let Ok(device) = CString::new(device) else {
        return Vec::new();
    };
    let mut modifiers = vec![0; 64];
    let len = unsafe {
        ssh_video_capture_modifiers(
            device.as_ptr(),
            format,
            modifiers.as_mut_ptr(),
            modifiers.len() as u32,
        )
    };
    modifiers.truncate(len as usize);
    modifiers
}

fn probe_devices(probe: impl Fn(*const c_char, *const VideoFormat, u32) -> u32) -> CodecDevices {
    render_nodes()
        .into_iter()
        .filter_map(|node| {
            let device = CString::new(node.as_str()).ok()?;
            let mask = probe(device.as_ptr(), FORMATS.as_ptr(), FORMATS.len() as u32);
            (mask != 0).then_some((node, mask))
        })
        .collect()
}

pub(crate) fn local_decoders() -> &'static CodecDevices {
    static DEVICES: OnceLock<CodecDevices> = OnceLock::new();
    DEVICES.get_or_init(|| {
        probe_devices(|node, formats, count| unsafe {
            ssh_video_decode_formats(node, formats, count)
        })
    })
}

fn local_encoders(rate: Option<&EncodeRate>) -> &'static CodecDevices {
    static DEVICES: [OnceLock<CodecDevices>; 3] = [const { OnceLock::new() }; 3];
    let mode = rate.map_or(0, |rate| rate.mode);
    DEVICES[mode as usize].get_or_init(|| {
        probe_devices(|node, formats, count| unsafe {
            ssh_video_encode_formats(node, formats, count, mode)
        })
    })
}

pub(crate) fn bridge_config(
    request: &FrameRequest,
    default_gpu: &str,
    rate_control: StreamRateControl,
    preserve_alpha: bool,
) -> Result<Value, String> {
    request.validate()?;
    let capture = request.capture_gpu.as_deref().unwrap_or(default_gpu);
    render_affinity(capture)?;
    let decoders = va_decoders(
        SSH_DECODERS
            .get()
            .ok_or("SSH Wayland decode capabilities are missing")?,
        preserve_alpha,
    );
    let (selection, effective_rate) = bridge_profile(
        request,
        capture,
        rate_control,
        preserve_alpha,
        &decoders,
        |rate| local_encoders(Some(rate)),
    )?;
    Ok(json!({
        "ok": true,
        "captureGpu": capture,
        "encodeGpu": selection.encode_gpu,
        "decodeGpu": selection.decode_gpu,
        "chroma": selection.format.chroma,
        "bitDepth": selection.format.depth,
        "screenContent": selection.format.scc(),
        "rateControl": effective_rate,
    }))
}

fn bridge_profile<'a>(
    request: &FrameRequest,
    capture: &str,
    rate_control: StreamRateControl,
    preserve_alpha: bool,
    decoders: &CodecDevices,
    encoders: impl Fn(&EncodeRate) -> &'a CodecDevices,
) -> Result<(VideoSelection, StreamRateControl), String> {
    let (selection, _, effective_rate) = rate_control.select((64, 64), false, |rate| {
        select_on_devices(
            request,
            Some(capture.to_owned()),
            encoders(rate),
            decoders,
            false,
            &STREAM_FORMATS,
            |node, _, _| {
                if preserve_alpha {
                    rate_control.select((64, 64), true, |rate| {
                        encoders(rate)
                            .get(node)
                            .filter(|mask| MAIN.supported_by(**mask))
                            .map(|_| ())
                            .ok_or_else(|| format!("no 8-bit 4:2:0 H.265 alpha encoder on {node}"))
                    })?;
                }
                Ok(false)
            },
        )
    })?;
    Ok((selection, effective_rate))
}

fn choose_bridge_gpu<'a>(
    nodes: impl IntoIterator<Item = &'a str>,
    mut check: impl FnMut(&str) -> Result<(), String>,
) -> Result<String, String> {
    let mut failures = Vec::new();
    for node in nodes {
        match check(node) {
            Ok(()) => return Ok(node.to_owned()),
            Err(error) => failures.push(format!("{node}: {error}")),
        }
    }
    if failures.is_empty() {
        Err("no remote DRM render node for SSH Wayland".into())
    } else {
        Err(format!(
            "no GPU-only SSH Wayland video path: {}",
            failures.join("; ")
        ))
    }
}

pub(crate) fn bridge_gpu(
    requested: &str,
    request: &FrameRequest,
    rate_control: StreamRateControl,
    preserve_alpha: bool,
) -> Result<String, String> {
    request.validate()?;
    let decoders = va_decoders(
        SSH_DECODERS
            .get()
            .ok_or("SSH Wayland decode capabilities are missing")?,
        preserve_alpha,
    );
    if decoders.is_empty() {
        return Err(
            "the display host has no compatible hardware H.265 decoder for SSH Wayland".into(),
        );
    }
    let check = |node: &str| {
        render_affinity(node)?;
        bridge_profile(
            request,
            node,
            rate_control,
            preserve_alpha,
            &decoders,
            |rate| local_encoders(Some(rate)),
        )?;
        if capture_modifiers(node, u32::from_le_bytes(*b"XR24")).is_empty()
            && capture_modifiers(node, u32::from_le_bytes(*b"XB24")).is_empty()
        {
            return Err("no importable RGB DMA-BUF layouts".into());
        }
        Ok(())
    };
    if !requested.is_empty() {
        return choose_bridge_gpu([requested], check);
    }
    // DMA-BUF feedback selects the client's graphics device. A different
    // render node can run the encoder after importing or copying the buffer.
    let mut nodes = bridge_render_nodes()
        .iter()
        .rev()
        .map(String::as_str)
        .collect::<Vec<_>>();
    nodes.sort_by_cached_key(|node| {
        let name = node.rsplit('/').next().unwrap_or(node);
        !std::fs::read_to_string(format!("/sys/class/drm/{name}/device/boot_vga"))
            .is_ok_and(|value| value.trim() == "1")
    });
    choose_bridge_gpu(nodes, check)
}

fn select_decoder<'a>(
    devices: &'a CodecDevices,
    format: VideoFormat,
    requested: Option<&'a str>,
    preferred: Option<&'a str>,
) -> Option<&'a str> {
    let supports = |node: &str| {
        devices
            .get(node)
            .is_some_and(|mask| format.supported_by(*mask))
    };
    if let Some(node) = requested {
        return supports(node).then_some(node);
    }
    preferred.filter(|node| supports(node)).or_else(|| {
        devices
            .iter()
            .find_map(|(node, mask)| format.supported_by(*mask).then_some(node.as_str()))
    })
}

fn encoder_nodes<'a>(explicit: Option<&'a str>, preferred: &[&'a str]) -> Vec<&'a str> {
    if let Some(node) = explicit {
        return vec![node];
    }
    let mut nodes = Vec::new();
    for node in preferred
        .iter()
        .copied()
        .chain(bridge_render_nodes().iter().map(String::as_str))
    {
        if !nodes.contains(&node) {
            nodes.push(node);
        }
    }
    nodes
}

pub(crate) fn prepare_snapshot_video(
    request: &FrameRequest,
    affinity: (u32, u32),
    (width, height, format): (u32, u32, u32),
    color: Option<&ColorDescription>,
    mut convert: impl FnMut(
        &str,
        &VideoOptions,
        &GpuColorProfile,
        &str,
        &NativeStream,
    ) -> Result<(), String>,
) -> Result<VideoSnapshot, String> {
    let capture =
        device_for_affinity(affinity).ok_or("no DRM render node for retained video frame")?;
    let default_color;
    let profile = match color {
        Some(color) => color,
        None => {
            default_color = ColorDescription::visual_assumption(
                &json!({"transfer":"srgb","primaries":"bt709"}),
            )?;
            &default_color
        }
    }
    .gpu_profile(2)?;
    let stream = NativeStream::capture();
    let mut attempted = request.clone();
    let mut failures = Vec::new();
    for node in encoder_nodes(request.encode_gpu.as_deref(), &[&capture]) {
        attempted.encode_gpu = Some(node.to_owned());
        let candidate = (|| {
            let selection = select_video(
                &attempted,
                &capture,
                (width, height),
                (false, false),
                None,
                &stream,
                None,
            )?;
            let options = request.options(
                width,
                height,
                format,
                color.is_some_and(ColorDescription::is_hdr),
                &selection,
            )?;
            convert(&capture, &options, &profile, &selection.encode_gpu, &stream)?;
            Ok::<_, String>((selection, options))
        })();
        match candidate {
            Ok((selection, options)) => {
                return Ok(VideoSnapshot {
                    options,
                    selection,
                    stream,
                });
            }
            Err(error) if error.starts_with("snapshot_unavailable:") => return Err(error),
            Err(error) => failures.push(format!("{node}: {error}")),
        }
    }
    Err(format!(
        "no GPU-only H.265 path for retained frame on {capture}: {}",
        failures.join("; ")
    ))
}

fn allocate_linear_staging(node: &str, options: &VideoOptions) -> Result<DisplayBuffer, String> {
    // Align 32-bit RGB rows to 256 bytes for GPUs that require this pitch on linear imports.
    let width = options.width.next_multiple_of(64);
    DisplayBuffer::new(node, width, options.height, options.format)
}

pub(crate) unsafe fn convert_video_snapshot(
    snapshot: *mut c_void,
    options: &VideoOptions,
    profile: &GpuColorProfile,
    stream: &NativeStream,
    capture: &str,
    encode: &str,
    staging: &mut Option<(String, DisplayBuffer)>,
) -> Result<(), String> {
    let mut error = [0; 512];
    let direct = unsafe {
        ssh_video_convert_snapshot(
            snapshot,
            options,
            profile,
            stream.as_ptr(),
            error.as_mut_ptr(),
        )
    };
    if direct >= 0 {
        return Ok(());
    }
    let direct_error = native_error(&error);
    if capture == encode || direct_error.starts_with("snapshot_unavailable:") {
        return Err(direct_error);
    }
    let encoder = CString::new(encode).map_err(|error| error.to_string())?;
    let target = NativeEncodeTarget {
        device: encoder.as_ptr(),
        stream: stream.as_ptr(),
        with_alpha: false,
        compare_alpha: false,
    };
    let attempt = |buffer: &DisplayBuffer| {
        let plane = buffer.plane();
        let mut error = [0; 512];
        if unsafe {
            ssh_video_copy_snapshot(
                snapshot,
                options,
                &plane,
                buffer.width(),
                error.as_mut_ptr(),
            )
        } < 0
        {
            return Err(native_error(&error));
        }
        if unsafe {
            ssh_video_convert_buffer(
                options,
                std::ptr::null(),
                &plane,
                buffer.width(),
                profile,
                false,
                &target,
                error.as_mut_ptr(),
            )
        } < 0
        {
            return Err(native_error(&error));
        }
        Ok(())
    };
    let mut failures = vec![direct_error];
    for owner in [encode, capture] {
        if let Some((node, buffer)) = staging.as_ref()
            && node == owner
        {
            match attempt(buffer) {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(error),
            }
            continue;
        }
        match allocate_linear_staging(owner, options)
            .and_then(|buffer| attempt(&buffer).map(|()| buffer))
        {
            Ok(buffer) => {
                *staging = Some((owner.to_owned(), buffer));
                return Ok(());
            }
            Err(error) => failures.push(error),
        }
    }
    Err(failures.join("; "))
}

fn va_decoders(devices: &CodecDevices, with_alpha: bool) -> CodecDevices {
    devices
        .iter()
        .map(|(node, mask)| (node.clone(), mask >> VA_DECODERS_SHIFT))
        .filter(|(_, mask)| *mask != 0 && (!with_alpha || MAIN.supported_by(*mask)))
        .collect()
}

fn select_video(
    request: &FrameRequest,
    capture: &str,
    (width, height): (u32, u32),
    (alpha, with_alpha): (bool, bool),
    rate: Option<&EncodeRate>,
    stream: &NativeStream,
    source_depth: Option<u32>,
) -> Result<VideoSelection, String> {
    let (decoders, same_host) = match SSH_DECODERS.get() {
        Some(decoders) => (decoders, false),
        None => (local_decoders(), true),
    };
    let decoders = va_decoders(decoders, with_alpha);
    let automatic: &[VideoFormat] = match (rate.is_some(), source_depth) {
        (false, _) => &AUTO_FORMATS,
        (true, Some(8)) => &STREAM_FORMATS_8,
        _ => &STREAM_FORMATS,
    };
    let encoders = local_encoders(rate);
    let choose = |predictive| {
        select_on_devices(
            request,
            Some(capture.to_owned()),
            encoders,
            &decoders,
            same_host,
            automatic,
            |node, _, format| {
                if predictive
                    && !encoders
                        .get(node)
                        .is_some_and(|mask| format.supported_by(mask >> 16))
                {
                    return Err("profile has no inter-frame encoder support".into());
                }
                let mut options = options_for_extent(width, height, format.depth, format.chroma)?;
                if alpha {
                    options.mode = ALPHA_PICTURE;
                }
                if predictive {
                    options.mode |= PREDICTIVE_PLAN;
                }
                let device = CString::new(node).map_err(|error| error.to_string())?;
                let mut error = [0; 512];
                let status = unsafe {
                    ssh_video_prepare_encoder(
                        device.as_ptr(),
                        &options,
                        format.scc(),
                        rate.map_or(std::ptr::null(), std::ptr::from_ref),
                        error.as_mut_ptr(),
                        stream.as_ptr(),
                    )
                };
                if status < 0 {
                    return Err(format!(
                        "{width}x{height} {}-bit {}{} failed with {}",
                        format.depth,
                        format.chroma,
                        if alpha { " alpha" } else { "" },
                        native_error(&error)
                    ));
                }
                Ok(predictive)
            },
        )
    };
    if rate.is_some()
        && let Ok(selection) = choose(true)
    {
        return Ok(selection);
    }
    choose(false)
}

fn select_on_devices(
    request: &FrameRequest,
    capture_gpu: Option<String>,
    encoders: &CodecDevices,
    decoders: &CodecDevices,
    same_host: bool,
    automatic: &[VideoFormat],
    mut prepare: impl FnMut(&str, &str, VideoFormat) -> Result<bool, String>,
) -> Result<VideoSelection, String> {
    let requested = request.requested_format();
    let same_sampling = requested
        .filter(|format| request.screen_content.is_none() && format.scc())
        .map(|format| [format, VideoFormat::new(format.chroma, format.depth, false)]);
    let formats: &[VideoFormat] = match (&same_sampling, &requested) {
        (Some(formats), _) => formats,
        (None, Some(format)) => std::slice::from_ref(format),
        (None, None) => automatic,
    };
    let encode_nodes = if let Some(node) = request.encode_gpu.as_deref() {
        vec![node]
    } else {
        capture_gpu
            .as_deref()
            .into_iter()
            .chain(
                encoders
                    .keys()
                    .map(String::as_str)
                    .filter(|node| Some(*node) != capture_gpu.as_deref()),
            )
            .collect()
    };
    let mut prepare_error = None;
    for &format in formats {
        for &encode_gpu in &encode_nodes {
            if !encoders
                .get(encode_gpu)
                .is_some_and(|mask| format.supported_by(*mask))
            {
                continue;
            }
            let Some(decode_gpu) = select_decoder(
                decoders,
                format,
                request.decode_gpu.as_deref(),
                same_host.then_some(encode_gpu),
            ) else {
                continue;
            };
            let predictive = match prepare(encode_gpu, decode_gpu, format) {
                Ok(predictive) => predictive,
                Err(error) => {
                    prepare_error = Some(format!("prepare H.265 encode on {encode_gpu}: {error}"));
                    continue;
                }
            };
            return Ok(VideoSelection {
                format,
                encode_gpu: encode_gpu.to_owned(),
                decode_gpu: decode_gpu.to_owned(),
                capture_gpu,
                predictive,
            });
        }
    }
    if let Some(error) = prepare_error {
        return Err(error);
    }
    let formats = requested.map_or_else(
        || "an automatic H.265 profile".to_owned(),
        |format| {
            format!(
                "{}-bit 4:{}:{} {}",
                format.depth,
                (format.chroma / 10) % 10,
                format.chroma % 10,
                if format.scc() && request.screen_content.is_some() {
                    "SCC"
                } else {
                    "HEVC"
                }
            )
        },
    );
    let encode = request
        .encode_gpu
        .as_deref()
        .unwrap_or("any encode render node");
    let decode = request
        .decode_gpu
        .as_deref()
        .unwrap_or("any decode render node");
    Err(format!(
        "no common H.265 format for {formats} on {encode} and {decode}"
    ))
}

fn native_error(error: &[c_char; 512]) -> String {
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

struct NativePacket(*mut c_void);
impl Drop for NativePacket {
    fn drop(&mut self) {
        unsafe { ssh_video_packet_destroy(self.0) };
    }
}

pub(crate) fn sha256(data: &[u8]) -> Result<[u8; 32], String> {
    let mut digest = [0u8; 32];
    if unsafe { SHA256(data.as_ptr(), data.len(), digest.as_mut_ptr()) }.is_null() {
        return Err("compute SHA-256".into());
    }
    Ok(digest)
}

impl VideoSnapshot {
    pub(crate) fn encode(&self, request: &FrameRequest) -> Result<(Vec<u8>, PacketInfo), String> {
        let data = encode_packet(
            request,
            &self.options,
            &self.selection,
            None,
            INDEPENDENT,
            &self.stream,
        )?;
        let packet = PacketInfo {
            width: self.options.width,
            height: self.options.height,
            coded_width: self.options.coded_width,
            coded_height: self.options.coded_height,
            depth: self.options.depth,
            chroma: self.options.chroma,
            screen_content: self.selection.format.scc(),
            decode_gpu: Some(self.selection.decode_gpu.clone()),
            access_unit_sha256: sha256(&data)?,
        };
        Ok((data, packet))
    }
}

fn encode_packet(
    request: &FrameRequest,
    options: &VideoOptions,
    selection: &VideoSelection,
    rate: Option<&EncodeRate>,
    stream_poc: u32,
    stream: &NativeStream,
) -> Result<Vec<u8>, String> {
    let device = CString::new(selection.encode_gpu.as_str()).map_err(|error| error.to_string())?;
    let mut error = [0; 512];
    let scc = selection.format.scc();
    let native = unsafe {
        ssh_video_encode(
            device.as_ptr(),
            options,
            scc,
            request.qp,
            rate.map_or(std::ptr::null(), std::ptr::from_ref),
            stream_poc,
            error.as_mut_ptr(),
            stream.as_ptr(),
        )
    };
    if native.is_null() {
        return Err(format!(
            "H.265 encode on {} for {}x{} {}-bit {} failed with {}",
            selection.encode_gpu,
            options.width,
            options.height,
            options.depth,
            options.chroma,
            native_error(&error),
        ));
    }
    let native = NativePacket(native);
    let mut size = 0;
    let data = unsafe { ssh_video_packet_data(native.0, &mut size) };
    if size == 0 || size as usize > MAX_ACCESS_UNIT {
        return Err("H.265 access unit exceeds the video frame limit".into());
    }
    Ok(unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec())
}

pub(crate) struct BridgeSource<'a> {
    pub extent: (u32, u32),
    pub format: u32,
    pub pixels: BridgePixels<'a>,
    pub producer_gpu: Option<&'a str>,
    pub color: Option<&'a ColorDescription>,
    pub implicit_sync: bool,
}

#[derive(Default)]
pub(crate) struct BridgeEncoder {
    stream: NativeStream,
    color: PredictionEncoder,
    alpha: PredictionEncoder,
    previous_alpha: Option<AlphaSettings>,
    last_route: Option<BridgeRoute>,
    linear_copy: Option<LinearCopy>,
}

struct BridgeRoute {
    encoder: String,
    producer: String,
    format: u32,
    modifier: Option<u64>,
    extent: (u32, u32),
    requested_format: Option<VideoFormat>,
    requested_decoder: Option<String>,
    rate: StreamRateControl,
    preserve_alpha: bool,
}

struct LinearCopy {
    source: String,
    encoder: String,
    extent: (u32, u32),
    format: u32,
    modifier: u64,
    buffer: DisplayBuffer,
}

#[derive(Eq, PartialEq)]
struct AlphaSettings {
    selection: VideoSelection,
    rate: EncodeRate,
    extent: (u32, u32),
}

#[derive(Default)]
struct PredictionEncoder {
    last_group: u64,
    reference: Option<EncodeReference>,
}

struct EncodeReference {
    extent: (u32, u32),
    encode_gpu: String,
    decode_gpu: String,
    format: VideoFormat,
    rate: EncodeRate,
    prediction: FramePrediction,
}

impl PredictionEncoder {
    fn next(
        &mut self,
        extent: (u32, u32),
        selection: &VideoSelection,
        rate: EncodeRate,
    ) -> Result<Option<FramePrediction>, String> {
        if !selection.predictive {
            self.reference = None;
            return Ok(None);
        }
        if let Some(reference) = self.reference.as_mut()
            && reference.extent == extent
            && reference.encode_gpu == selection.encode_gpu
            && reference.decode_gpu == selection.decode_gpu
            && reference.format == selection.format
            && reference.rate == rate
            && reference.prediction.poc + 1 < GROUP_LENGTH
        {
            reference.prediction.poc += 1;
            return Ok(Some(reference.prediction));
        }
        self.last_group = self
            .last_group
            .checked_add(1)
            .ok_or("SSH H.265 prediction groups exhausted")?;
        let prediction = FramePrediction {
            group: self.last_group,
            poc: 0,
        };
        self.reference = Some(EncodeReference {
            extent,
            encode_gpu: selection.encode_gpu.clone(),
            decode_gpu: selection.decode_gpu.clone(),
            format: selection.format,
            rate,
            prediction,
        });
        Ok(Some(prediction))
    }
}

struct PreparedBridgeFrame {
    selection: VideoSelection,
    rate: EncodeRate,
    options: VideoOptions,
    alpha: Option<AlphaSettings>,
    conversion: i32,
}

fn prepare_bridge_frame(
    request: &FrameRequest,
    capture_gpu: &str,
    source: &BridgeSource<'_>,
    preserve_alpha: bool,
    rate_control: StreamRateControl,
    encoder: &mut BridgeEncoder,
) -> Result<PreparedBridgeFrame, String> {
    let (width, height) = source.extent;
    let source_depth = match source.format.to_le_bytes()[2] {
        b'3' | b'4' => 10,
        _ => 8,
    };
    let with_alpha = preserve_alpha && has_alpha(source.format);
    let (selection, rate, _) = rate_control.select(source.extent, false, |rate| {
        select_video(
            request,
            capture_gpu,
            source.extent,
            (false, with_alpha),
            Some(rate),
            &encoder.stream,
            Some(source_depth),
        )
    })?;
    let mut options = request.options(
        width,
        height,
        source.format,
        source.color.is_some_and(ColorDescription::is_hdr),
        &selection,
    )?;
    let profile = source.color.map(|color| color.gpu_profile(2)).transpose()?;
    let alpha_config = if with_alpha {
        options.mode = WITH_ALPHA;
        let mut alpha_request = request.clone();
        alpha_request.chroma = Some(420);
        alpha_request.bit_depth = Some(8);
        alpha_request.screen_content = Some(false);
        alpha_request.decode_gpu = Some(selection.decode_gpu.clone());
        alpha_request.encode_gpu = Some(selection.encode_gpu.clone());
        let (selection, rate, _) = rate_control.select(source.extent, true, |rate| {
            select_video(
                &alpha_request,
                capture_gpu,
                source.extent,
                (true, false),
                Some(rate),
                &encoder.stream,
                Some(8),
            )
        })?;
        Some(AlphaSettings {
            selection,
            rate,
            extent: source.extent,
        })
    } else {
        None
    };
    let shm;
    let (input, plane) = match source.pixels {
        BridgePixels::Shm { fd, stride, offset } => {
            shm = NativeShm {
                fd: fd.as_raw_fd(),
                stride,
                offset,
            };
            (std::ptr::from_ref(&shm), std::ptr::null())
        }
        BridgePixels::Dmabuf(plane) => (std::ptr::null(), std::ptr::from_ref(plane)),
    };
    let encode_device =
        CString::new(selection.encode_gpu.as_str()).map_err(|error| error.to_string())?;
    let target = NativeEncodeTarget {
        device: encode_device.as_ptr(),
        stream: encoder.stream.as_ptr(),
        with_alpha,
        compare_alpha: with_alpha && alpha_config == encoder.previous_alpha,
    };
    let convert = |plane, plane_width, implicit_sync| {
        let mut error = [0; 512];
        let status = unsafe {
            ssh_video_convert_buffer(
                &options,
                input,
                plane,
                plane_width,
                profile
                    .as_ref()
                    .map_or(std::ptr::null(), std::ptr::from_ref),
                implicit_sync,
                &target,
                error.as_mut_ptr(),
            )
        };
        if status < 0 {
            Err(native_error(&error))
        } else {
            Ok(status)
        }
    };
    let conversion = if let BridgePixels::Dmabuf(source_plane) = source.pixels {
        let copy = |source_gpu: &str, buffer: &DisplayBuffer| -> Result<i32, String> {
            copy_dma_buf(
                source_gpu,
                &options,
                source_plane,
                buffer,
                source.implicit_sync,
            )?;
            convert(&buffer.plane(), buffer.width(), false)
        };
        let encode = selection.encode_gpu.as_str();
        if let Some(cached) = encoder.linear_copy.as_ref().filter(|cached| {
            cached.encoder == encode
                && cached.extent == source.extent
                && cached.format == source.format
                && cached.modifier == source_plane.modifier
                && source.producer_gpu.is_none_or(|node| node == cached.source)
        }) && let Ok(converted) = copy(&cached.source, &cached.buffer)
        {
            converted
        } else {
            match convert(plane, width, source.implicit_sync) {
                Ok(converted) => converted,
                Err(direct_error) => {
                    let mut result = Err(direct_error);
                    let readers = source
                        .producer_gpu
                        .into_iter()
                        .chain(std::iter::once(capture_gpu))
                        .chain(bridge_render_nodes().iter().map(String::as_str));
                    let mut tried = Vec::new();
                    'readers: for reader in readers {
                        if reader == encode || tried.contains(&reader) {
                            continue;
                        }
                        tried.push(reader);
                        for owner in [encode, reader] {
                            let attempt =
                                allocate_linear_staging(owner, &options).and_then(|buffer| {
                                    copy(reader, &buffer).map(|converted| (buffer, converted))
                                });
                            match attempt {
                                Ok((buffer, converted)) => {
                                    encoder.linear_copy = Some(LinearCopy {
                                        source: reader.to_owned(),
                                        encoder: encode.to_owned(),
                                        extent: source.extent,
                                        format: source.format,
                                        modifier: source_plane.modifier,
                                        buffer,
                                    });
                                    result = Ok(converted);
                                    if timing_enabled() {
                                        eprintln!(
                                            "SSH GPU copy {width}x{height} {reader} -> {encode} via linear DMA-BUF on {owner}"
                                        );
                                    }
                                    break 'readers;
                                }
                                Err(error) => {
                                    if let Err(details) = &mut result {
                                        use std::fmt::Write;
                                        let _ = write!(
                                            details,
                                            "; linear DMA-BUF copy {reader} to {encode} on {owner}: {error}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    result?
                }
            }
        }
    } else {
        convert(plane, width, source.implicit_sync)?
    };
    Ok(PreparedBridgeFrame {
        selection,
        rate,
        options,
        alpha: alpha_config,
        conversion,
    })
}

fn copy_dma_buf(
    device: &str,
    options: &VideoOptions,
    source: &BridgePlane,
    destination: &DisplayBuffer,
    implicit_sync: bool,
) -> Result<(), String> {
    let device = CString::new(device).map_err(|error| error.to_string())?;
    let mut error = [0; 512];
    let status = unsafe {
        ssh_video_copy_dma_buf(
            device.as_ptr(),
            options,
            source,
            &destination.plane(),
            destination.width(),
            implicit_sync,
            error.as_mut_ptr(),
        )
    };
    if status < 0 {
        Err(native_error(&error))
    } else {
        Ok(())
    }
}

pub(crate) fn encode_bridge(
    request: &FrameRequest,
    capture_gpu: &str,
    source: BridgeSource<'_>,
    preserve_alpha: bool,
    rate_control: StreamRateControl,
    encoder: &mut BridgeEncoder,
    release_source: impl FnOnce() -> Result<(), String>,
) -> Result<BridgePicture, String> {
    let (width, height) = source.extent;
    let prepare_start = timing_start();
    let producer = source.producer_gpu.unwrap_or(capture_gpu);
    let modifier = match source.pixels {
        BridgePixels::Dmabuf(plane) => Some(plane.modifier),
        BridgePixels::Shm { .. } => None,
    };
    let requested_format = request.requested_format();
    let last = encoder
        .last_route
        .as_ref()
        .filter(|route| {
            route.producer == producer
                && route.format == source.format
                && route.modifier == modifier
                && route.extent == source.extent
                && route.requested_format == requested_format
                && route.requested_decoder == request.decode_gpu
                && route.rate == rate_control
                && route.preserve_alpha == preserve_alpha
        })
        .map(|route| route.encoder.clone());
    let candidates = encoder_nodes(
        request.encode_gpu.as_deref(),
        &[last.as_deref().unwrap_or(producer), producer, capture_gpu],
    );
    let mut result = Err(String::new());
    let mut attempted = request.clone();
    for node in candidates {
        attempted.encode_gpu = Some(node.to_owned());
        match prepare_bridge_frame(
            &attempted,
            producer,
            &source,
            preserve_alpha,
            rate_control,
            encoder,
        ) {
            Ok(prepared) => {
                result = Ok(prepared);
                break;
            }
            Err(error) => {
                use std::fmt::Write;
                if let Err(details) = &mut result {
                    let _ = write!(details, "\n  {node}: {error}");
                }
            }
        }
    }
    let PreparedBridgeFrame {
        selection,
        rate,
        options,
        alpha: alpha_config,
        conversion: converted,
    } = result.map_err(|details| {
        format!("no GPU-only H.265 path for {width}x{height} frame on {producer}:{details}")
    })?;
    let prepare_ms = timing_ms(prepare_start);
    if last.as_deref() != Some(selection.encode_gpu.as_str()) {
        encoder.last_route = Some(BridgeRoute {
            encoder: selection.encode_gpu.clone(),
            producer: producer.to_owned(),
            format: source.format,
            modifier,
            extent: source.extent,
            requested_format,
            requested_decoder: request.decode_gpu.clone(),
            rate: rate_control,
            preserve_alpha,
        });
    }
    release_source()?;
    let encode_start = timing_start();
    let prediction = encoder.color.next(source.extent, &selection, rate)?;
    let color = encode_packet(
        request,
        &options,
        &selection,
        Some(&rate),
        prediction.map_or(INDEPENDENT, |prediction| prediction.poc),
        &encoder.stream,
    )?;
    let encode_ms = timing_ms(encode_start);
    let alpha_start = timing_start();
    let reuse_alpha = converted == 2;
    let (alpha, alpha_prediction) = if let Some(config) = alpha_config.filter(|_| converted == 1) {
        let prediction = encoder
            .alpha
            .next(source.extent, &config.selection, config.rate)?;
        let mut alpha_options = options_for_extent(width, height, 8, 420)?;
        alpha_options.mode = ALPHA_PICTURE;
        let packet = encode_packet(
            request,
            &alpha_options,
            &config.selection,
            Some(&config.rate),
            prediction.map_or(INDEPENDENT, |prediction| prediction.poc),
            &encoder.stream,
        )?;
        encoder.previous_alpha = Some(config);
        (packet, prediction)
    } else {
        if !reuse_alpha {
            encoder.alpha.reference = None;
            encoder.previous_alpha = None;
        }
        (Vec::new(), None)
    };
    if prepare_start.is_some() {
        let coding = prediction.map_or_else(|| "independent".to_owned(), |p| p.poc.to_string());
        eprintln!(
            "SSH video {width}x{height} encoder={} prepare_and_convert_ms={prepare_ms:.3} encode_ms={encode_ms:.3} alpha_encode_ms={:.3} depth={} chroma={} poc={coding} color_bytes={} alpha_bytes={}",
            selection.encode_gpu,
            timing_ms(alpha_start),
            selection.format.depth,
            selection.format.chroma,
            color.len(),
            alpha.len(),
        );
    }
    Ok(BridgePicture {
        frame: BridgeFrame {
            depth: options.depth,
            chroma: options.chroma,
            screen_content: selection.format.scc(),
            decode_gpu: selection.decode_gpu,
            prediction,
            alpha_prediction,
            reuse_alpha,
        },
        color,
        alpha,
    })
}

#[derive(Default)]
pub(crate) struct BridgeDecoder {
    stream: NativeStream,
    color: PredictionDecoder,
    alpha: PredictionDecoder,
    alpha_surface: Option<((u32, u32), String)>,
}

#[derive(Default)]
struct PredictionDecoder {
    last_group: u64,
    reference: Option<(VideoFormat, (u32, u32), String, FramePrediction)>,
}

impl PredictionDecoder {
    fn validate(
        &self,
        format: VideoFormat,
        extent: (u32, u32),
        node: &str,
        prediction: Option<FramePrediction>,
    ) -> Result<(), String> {
        if let Some(prediction) = prediction {
            if prediction.poc >= GROUP_LENGTH {
                return Err("unsupported SSH H.265 prediction configuration".into());
            }
            if prediction.poc == 0 {
                if prediction.group <= self.last_group {
                    return Err("invalid SSH H.265 prediction group".into());
                }
            } else if !self.reference.as_ref().is_some_and(
                |(previous_format, previous_extent, previous_node, previous)| {
                    *previous_format == format
                        && *previous_extent == extent
                        && previous_node == node
                        && prediction.group == previous.group
                        && prediction.poc == previous.poc + 1
                },
            ) {
                return Err("missing SSH H.265 picture reference".into());
            }
        }
        Ok(())
    }

    fn advance(
        &mut self,
        format: VideoFormat,
        extent: (u32, u32),
        node: &str,
        prediction: Option<FramePrediction>,
    ) {
        self.reference = prediction.map(|prediction| {
            self.last_group = prediction.group;
            (format, extent, node.to_owned(), prediction)
        });
    }
}

pub(crate) enum DisplayPixels<'a> {
    Dmabuf(&'a BridgePlane),
    Shm(&'a NativeShm),
}

impl BridgeDecoder {
    pub(crate) fn decode(
        &mut self,
        picture: &BridgePicture,
        extent: (u32, u32),
        fourcc: u32,
        target: DisplayPixels<'_>,
    ) -> Result<(), String> {
        let frame = &picture.frame;
        let format = VideoFormat::new(frame.chroma, frame.depth, frame.screen_content);
        self.color
            .validate(format, extent, &frame.decode_gpu, frame.prediction)?;
        if picture.alpha.is_empty() && frame.alpha_prediction.is_some() {
            return Err("missing SSH H.265 alpha picture".into());
        }
        if frame.reuse_alpha
            && (!picture.alpha.is_empty()
                || !self
                    .alpha_surface
                    .as_ref()
                    .is_some_and(|(size, node)| *size == extent && *node == frame.decode_gpu))
        {
            return Err("no compatible SSH H.265 alpha picture to reuse".into());
        }
        self.alpha
            .validate(MAIN, extent, &frame.decode_gpu, frame.alpha_prediction)?;
        let mask = local_decoders()
            .get(&frame.decode_gpu)
            .copied()
            .unwrap_or(0)
            >> VA_DECODERS_SHIFT;
        if !format.supported_by(mask) || (!picture.alpha.is_empty() && !MAIN.supported_by(mask)) {
            return Err("unsupported GPU-only bridge H.265 decode configuration".into());
        }
        let mut options = options_for_extent(extent.0, extent.1, frame.depth, frame.chroma)?;
        options.format = fourcc;
        if picture.color.is_empty()
            || picture.color.len() > MAX_ACCESS_UNIT
            || picture.alpha.len() > MAX_ACCESS_UNIT
        {
            return Err("invalid H.265 access unit length".into());
        }
        let bitstream = NativeBitstream {
            color: picture.color.as_ptr(),
            alpha: picture.alpha.as_ptr(),
            color_size: picture.color.len() as u32,
            alpha_size: picture.alpha.len() as u32,
            poc: frame
                .prediction
                .map_or(INDEPENDENT, |prediction| prediction.poc),
            alpha_poc: frame
                .alpha_prediction
                .map_or(INDEPENDENT, |prediction| prediction.poc),
            scc: frame.screen_content,
            reuse_alpha: frame.reuse_alpha,
        };
        let output = match target {
            DisplayPixels::Dmabuf(plane) => NativeDisplayTarget {
                dma: plane,
                shm: std::ptr::null(),
            },
            DisplayPixels::Shm(shm) => NativeDisplayTarget {
                dma: std::ptr::null(),
                shm,
            },
        };
        let device = CString::new(frame.decode_gpu.as_str()).map_err(|error| error.to_string())?;
        let mut error = [0; 512];
        if unsafe {
            ssh_video_decode(
                device.as_ptr(),
                &options,
                self.stream.as_ptr(),
                &bitstream,
                &output,
                error.as_mut_ptr(),
            )
        } < 0
        {
            return Err(format!(
                "H.265 decode for {}x{} {}-bit {} failed with {}",
                options.width,
                options.height,
                options.depth,
                options.chroma,
                native_error(&error)
            ));
        }
        self.color
            .advance(format, extent, &frame.decode_gpu, frame.prediction);
        if !frame.reuse_alpha {
            self.alpha
                .advance(MAIN, extent, &frame.decode_gpu, frame.alpha_prediction);
            self.alpha_surface =
                (!picture.alpha.is_empty()).then(|| (extent, frame.decode_gpu.clone()));
        }
        Ok(())
    }
}

pub(crate) fn decode(
    data: &[u8],
    packet: PacketInfo,
    color: &mut Value,
) -> Result<Vec<u8>, String> {
    if data.is_empty() || data.len() > MAX_ACCESS_UNIT {
        return Err("invalid H.265 access unit length".into());
    }
    if sha256(data)? != packet.access_unit_sha256 {
        return Err("H.265 access unit checksum mismatch".into());
    }
    let format = VideoFormat::new(packet.chroma, packet.depth, packet.screen_content);
    let options = options_for_extent(packet.width, packet.height, packet.depth, packet.chroma)?;
    if options.coded_width != packet.coded_width || options.coded_height != packet.coded_height {
        return Err("invalid H.265 coded size".into());
    }
    let decoders = va_decoders(local_decoders(), false);
    let device = select_decoder(&decoders, format, packet.decode_gpu.as_deref(), None)
        .ok_or("no VA-API decoder for H.265 GPU preview")?;
    let device = CString::new(device).map_err(|error| error.to_string())?;

    let mut width = packet.width;
    let mut height = packet.height;
    if width > 1600 || height > 1000 {
        if width * 1000 >= height * 1600 {
            height = (height * 1600 / width).max(1);
            width = 1600;
        } else {
            width = (width * 1000 / height).max(1);
            height = 1000;
        }
    }
    let mut error = [0; 512];
    let native = unsafe {
        ssh_video_decode_preview(
            device.as_ptr(),
            &options,
            packet.screen_content,
            data.as_ptr(),
            data.len() as u32,
            width,
            height,
            error.as_mut_ptr(),
        )
    };
    if native.is_null() {
        return Err(native_error(&error));
    }
    let native = NativePacket(native);
    let mut size = 0;
    let deflate = unsafe { ssh_video_packet_data(native.0, &mut size) };
    let deflate = unsafe { std::slice::from_raw_parts(deflate, size as usize) };
    color["video_preview_extent"] = json!([packet.width, packet.height, width, height]);
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    encoder
        .add_text_chunk("wayland-mcp-color".into(), color.to_string())
        .map_err(|error| error.to_string())?;
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_chunk(png::chunk::IDAT, deflate)
        .map_err(|error| error.to_string())?;
    writer.finish().map_err(|error| error.to_string())?;
    Ok(png)
}

pub(crate) fn description(
    request: &FrameRequest,
    options: &VideoOptions,
    selection: &VideoSelection,
) -> Value {
    let tone_map = match options.tone_map {
        1 => ToneMap::Knee,
        2 => ToneMap::Reinhard,
        3 => ToneMap::Curve,
        _ => ToneMap::Clip,
    };
    json!({
        "codec": "hevc", "picture": "idr", "chroma": options.chroma, "bitDepth": options.depth,
        "screenContent": selection.format.scc(), "captureGpu": selection.capture_gpu,
        "encodeGpu": selection.encode_gpu, "decodeGpu": selection.decode_gpu,
        "qp": request.qp, "toneMap": tone_map, "exposure": request.exposure, "background": request.background,
        "toneCurve": (!request.tone_curve.is_empty()).then_some(&request.tone_curve),
        "primaries": "bt709", "transfer": "srgb", "matrix": "bt709", "range": "full",
        "chromaLocation": (options.chroma == 420).then_some("center"),
        "validation": "access-unit-sha256-and-hardware-decode",
        "width": options.width, "height": options.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = "/dev/dri/renderD128";
    const ENCODE: &str = "/dev/dri/renderD129";
    const DECODE: &str = "/dev/dri/renderD130";

    #[test]
    fn streaming_rate_resolves_nominal_cadence_and_quality() {
        let (_, preferred, _) = StreamRateControl::default()
            .select((1280, 720), false, |_| Ok(()))
            .unwrap();
        assert_eq!(
            (preferred.mode, preferred.frame_rate, preferred.quality),
            (2, 120, 24)
        );

        let mut tried = Vec::new();
        let (_, fallback, _) = StreamRateControl::default()
            .select((1280, 720), false, |rate| {
                tried.push(rate.mode);
                if rate.mode == 2 {
                    return Err("constant quality unavailable".into());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(tried, [2, 1]);
        assert_eq!(
            (fallback.mode, fallback.frame_rate, fallback.target_bitrate),
            (1, 120, 27_648_000)
        );
        let transport = StreamRateControl::TransportVbr(8_000_000);
        let (_, color, _) = transport.select((1280, 720), false, |_| Ok(())).unwrap();
        assert_eq!(
            (color.mode, color.frame_rate, color.target_bitrate),
            (1, 60, 8_000_000)
        );
        let (_, mask, _) = transport.select((1280, 720), true, |_| Ok(())).unwrap();
        assert_eq!((mask.mode, mask.frame_rate, mask.quality), (2, 120, 16));
        let quality: StreamRateControl = serde_json::from_value(json!({
            "mode": "constantQuality", "quality": 23, "frameRate": 60
        }))
        .unwrap();
        let (_, quality, _) = quality.select((1280, 720), false, |_| Ok(())).unwrap();
        assert_eq!(
            (quality.mode, quality.frame_rate, quality.quality),
            (2, 60, 23)
        );
    }

    #[test]
    fn automatic_format_requires_support_on_both_hosts() {
        let request = serde_json::from_value(json!({"windowId": "1"})).unwrap();
        let encoders =
            CodecDevices::from([(ENCODE.into(), SCC.bit().unwrap() | MAIN.bit().unwrap())]);
        for (mask, expected) in [(MAIN.bit().unwrap(), MAIN), (3, SCC)] {
            let selection = select_on_devices(
                &request,
                Some(CAPTURE.into()),
                &encoders,
                &CodecDevices::from([(DECODE.into(), mask)]),
                false,
                &AUTO_FORMATS,
                |_, _, _| Ok(false),
            )
            .unwrap();
            assert_eq!(selection.format, expected);
            assert_eq!(selection.capture_gpu.as_deref(), Some(CAPTURE));
            assert_eq!(selection.encode_gpu, ENCODE);
            assert_eq!(selection.decode_gpu, DECODE);
        }
    }

    #[test]
    fn bridge_bootstrap_can_keep_the_graphics_device_without_an_encoder() {
        let request: FrameRequest = serde_json::from_value(json!({"windowId": ""})).unwrap();
        let encoders = CodecDevices::from([(CAPTURE.into(), MAIN.bit().unwrap())]);
        let decoders = CodecDevices::from([(DECODE.into(), MAIN.bit().unwrap())]);
        let choose = |node: &str| {
            bridge_profile(
                &request,
                node,
                StreamRateControl::Auto,
                true,
                &decoders,
                |_| &encoders,
            )
            .map(|_| ())
        };
        assert_eq!(
            choose_bridge_gpu([ENCODE, CAPTURE], choose).unwrap(),
            ENCODE
        );
        let (selection, _) = bridge_profile(
            &request,
            ENCODE,
            StreamRateControl::Auto,
            true,
            &decoders,
            |_| &encoders,
        )
        .unwrap();
        assert_eq!(selection.capture_gpu.as_deref(), Some(ENCODE));
        assert_eq!(selection.encode_gpu, CAPTURE);
        assert_eq!(selection.decode_gpu, DECODE);
    }

    #[test]
    fn sampling_selection_keeps_depth_and_can_use_range_extensions() {
        let format = VideoFormat::new(444, 8, false);
        let requested = json!({"windowId": "1", "chroma": 444, "bitDepth": 8});
        let request = serde_json::from_value(requested).unwrap();
        let encoders = CodecDevices::from([(ENCODE.into(), format.bit().unwrap())]);
        let decoders = CodecDevices::from([(DECODE.into(), format.bit().unwrap())]);
        let selection = select_on_devices(
            &request,
            None,
            &encoders,
            &decoders,
            false,
            &AUTO_FORMATS,
            |_, _, _| Ok(false),
        )
        .unwrap();
        assert_eq!(selection.format, format);
    }

    #[test]
    fn explicit_profile_and_devices_stay_exact() {
        let encoders = CodecDevices::from([(ENCODE.into(), MAIN.bit().unwrap())]);
        let decoders = CodecDevices::from([(DECODE.into(), MAIN.bit().unwrap())]);
        for args in [
            json!({"windowId": "1", "screenContent": true}),
            json!({"windowId": "1", "encodeGpu": CAPTURE}),
            json!({"windowId": "1", "decodeGpu": CAPTURE}),
        ] {
            let request = serde_json::from_value(args).unwrap();
            assert!(
                select_on_devices(
                    &request,
                    Some(CAPTURE.into()),
                    &encoders,
                    &decoders,
                    false,
                    &AUTO_FORMATS,
                    |_, _, _| unreachable!("no compatible configuration")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn a_predicted_bridge_picture_requires_the_preceding_commit() {
        let mut decoder = BridgeDecoder::default();
        let picture = BridgePicture {
            frame: BridgeFrame {
                depth: 10,
                chroma: 420,
                screen_content: false,
                decode_gpu: DECODE.into(),
                prediction: Some(FramePrediction { group: 1, poc: 1 }),
                alpha_prediction: None,
                reuse_alpha: false,
            },
            color: vec![0],
            alpha: Vec::new(),
        };
        assert_eq!(
            decoder.decode(
                &picture,
                (64, 64),
                0,
                DisplayPixels::Shm(&NativeShm {
                    fd: -1,
                    stride: 256,
                    offset: 0
                })
            ),
            Err("missing SSH H.265 picture reference".into()),
        );
    }

    #[test]
    #[ignore = "requires VA-API HEVC and Vulkan external host memory"]
    fn interleaved_gpu_streams_keep_distinct_color_and_alpha_references() -> Result<(), String> {
        use std::fs::File;
        use std::os::fd::{AsFd, FromRawFd};

        fn sealed_pool(size: u64) -> File {
            let fd = unsafe {
                libc::memfd_create(
                    c"test-wayland-frame".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            assert!(fd >= 0);
            let file = unsafe { File::from_raw_fd(fd) };
            file.set_len(size).unwrap();
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) },
                0
            );
            file
        }

        let node = std::env::var("WAYLAND_MCP_TEST_GPU").unwrap_or_else(|_| CAPTURE.into());
        let (width, height) = (193, 129);
        let stride = width * 4 + 16;
        let offset = 17;
        let source = sealed_pool(offset + u64::from(height * stride));
        let output = sealed_pool(u64::from(width * 4 * height) + 32);
        let rate = StreamRateControl::ConstantQuality {
            frame_rate: 120,
            quality: 24,
        };
        let xr24 = u32::from_le_bytes(*b"XR24");
        let ar24 = u32::from_le_bytes(*b"AR24");
        let target = NativeShm {
            fd: output.as_raw_fd(),
            stride: width * 4,
            offset: 32,
        };
        let vbr = StreamRateControl::Vbr {
            frame_rate: 60,
            target_bitrate: Some(1_000_000),
        };
        for (depth, chroma, rate) in [
            (8, 420, rate),
            (10, 420, rate),
            (8, 444, rate),
            (10, 444, rate),
            (10, 422, vbr),
        ] {
            let request: FrameRequest = serde_json::from_value(json!({
                "windowId": "", "captureGpu": node, "encodeGpu": node, "decodeGpu": node,
                "chroma": chroma, "bitDepth": depth, "screenContent": false,
            }))
            .unwrap();
            let mut encoders = [BridgeEncoder::default(), BridgeEncoder::default()];
            let mut decoders = [BridgeDecoder::default(), BridgeDecoder::default()];
            for poc in 0..4 {
                for (surface, format) in [ar24, xr24].into_iter().enumerate() {
                    let picture = encode_bridge(
                        &request,
                        &node,
                        BridgeSource {
                            extent: (width, height),
                            format,
                            pixels: BridgePixels::Shm {
                                fd: source.as_fd(),
                                stride,
                                offset,
                            },
                            producer_gpu: None,
                            color: None,
                            implicit_sync: true,
                        },
                        true,
                        rate,
                        &mut encoders[surface],
                        || Ok(()),
                    )
                    .map_err(|error| format!("{depth}/{chroma} surface {surface}: {error}"))?;
                    assert_eq!(
                        picture.frame.prediction,
                        Some(FramePrediction { group: 1, poc })
                    );
                    if format == ar24 && poc == 0 {
                        assert!(!picture.alpha.is_empty());
                        assert_eq!(
                            picture.frame.alpha_prediction,
                            Some(FramePrediction { group: 1, poc: 0 })
                        );
                    } else {
                        assert!(picture.alpha.is_empty());
                        assert_eq!(picture.frame.alpha_prediction, None);
                    }
                    assert_eq!(picture.frame.reuse_alpha, format == ar24 && poc != 0);
                    decoders[surface]
                        .decode(
                            &picture,
                            (width, height),
                            format,
                            DisplayPixels::Shm(&target),
                        )
                        .map_err(|error| {
                            format!("{depth}/{chroma} surface {surface} POC {poc}: {error}")
                        })?;
                }
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires VA-API HEVC and Vulkan compute"]
    fn ssh_preview_packs_and_compresses_on_gpu() -> Result<(), String> {
        use std::fs::File;
        use std::os::fd::{AsFd, FromRawFd};

        let node = std::env::var("WAYLAND_MCP_TEST_GPU").unwrap_or_else(|_| CAPTURE.into());
        let request = serde_json::from_value(json!({
            "windowId": "", "captureGpu": node, "encodeGpu": node, "decodeGpu": node,
            "chroma": 420, "bitDepth": 8, "screenContent": false
        }))
        .unwrap();
        let (width, height) = (193, 129);
        let stride = 4 * width;
        let fd = unsafe {
            libc::memfd_create(
                c"test-wayland-preview".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(fd >= 0);
        let source = unsafe { File::from_raw_fd(fd) };
        source.set_len(u64::from(height * stride)).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) },
            0
        );
        let picture = encode_bridge(
            &request,
            &node,
            BridgeSource {
                extent: (width, height),
                format: u32::from_le_bytes(*b"XR24"),
                pixels: BridgePixels::Shm {
                    fd: source.as_fd(),
                    stride,
                    offset: 0,
                },
                producer_gpu: None,
                color: None,
                implicit_sync: true,
            },
            false,
            StreamRateControl::default(),
            &mut BridgeEncoder::default(),
            || Ok(()),
        )?;
        let format = u32::from_le_bytes(*b"XR24");
        let first = DisplayBuffer::new(&node, width, height, format)?;
        BridgeDecoder::default().decode(
            &picture,
            (width, height),
            format,
            DisplayPixels::Dmabuf(&first.plane()),
        )?;
        let mut options = options_for_extent(width, height, 8, 420)?;
        options.format = format;
        let staging = allocate_linear_staging(&node, &options)?;
        assert_eq!(staging.width(), 256);
        copy_dma_buf(&node, &options, &first.plane(), &staging, false)?;
        let picture = encode_bridge(
            &request,
            &node,
            BridgeSource {
                extent: (width, height),
                format,
                pixels: BridgePixels::Dmabuf(&staging.plane()),
                producer_gpu: Some(&node),
                color: None,
                implicit_sync: false,
            },
            false,
            StreamRateControl::default(),
            &mut BridgeEncoder::default(),
            || Ok(()),
        )?;
        let packet = PacketInfo {
            width,
            height,
            coded_width: options.coded_width,
            coded_height: options.coded_height,
            depth: 8,
            chroma: 420,
            screen_content: false,
            decode_gpu: Some(node),
            access_unit_sha256: sha256(&picture.color)?,
        };
        let mut metadata = json!({"coordinate_space": "buffer"});
        let png = decode(&picture.color, packet, &mut metadata)?;
        let reader = png::Decoder::new(std::io::Cursor::new(&png))
            .read_info()
            .map_err(|error| error.to_string())?;
        assert_eq!((reader.info().width, reader.info().height), (width, height));
        assert_eq!(
            metadata["video_preview_extent"],
            json!([width, height, width, height])
        );
        let mut cursor = 8;
        while cursor < png.len() {
            let count = u32::from_be_bytes(png[cursor..cursor + 4].try_into().unwrap()) as usize;
            if &png[cursor + 4..cursor + 8] == b"IDAT" {
                let end = cursor + 8 + count;
                let row_bytes = width * 3 + 1;
                // The filtered black image begins each scanline with a Sub-filter byte.
                let sum = 1 + height;
                let weighted = (height * row_bytes + row_bytes * height * (height + 1) / 2) % 65521;
                assert_eq!(png[end - 4..end], ((weighted << 16) | sum).to_be_bytes());
                assert!(count < (row_bytes * height / 8) as usize);
                return Ok(());
            }
            cursor += count + 12;
        }
        Err("GPU preview has no PNG image data".into())
    }

    #[test]
    fn tone_mapping_is_only_enabled_for_float_sources() {
        let request =
            serde_json::from_value::<FrameRequest>(json!({"windowId": "", "toneMap": "knee"}))
                .unwrap();
        let selection = VideoSelection {
            capture_gpu: None,
            predictive: false,
            encode_gpu: ENCODE.into(),
            decode_gpu: DECODE.into(),
            format: MAIN,
        };
        for (fourcc, mode) in [
            (b"XR24", ToneMap::Clip),
            (b"XR30", ToneMap::Clip),
            (b"XB4H", ToneMap::Knee),
        ] {
            assert_eq!(
                request
                    .options(64, 64, u32::from_le_bytes(*fourcc), true, &selection)
                    .unwrap()
                    .tone_map,
                mode as u32
            );
        }
    }
}
