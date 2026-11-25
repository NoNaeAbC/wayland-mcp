use super::{
    Arg, Backing, Buffer, End, GeneratedEvent, Request, State, WaylandWireMessage, extent,
    io_error, memfd, wire,
};
use crate::gui_backend_wayland::{
    DecodedWaylandEvent, decode_wayland_header, encode_generated_event,
};
use crate::ssh_video;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Default)]
pub(super) struct DmaState {
    params: HashMap<u32, Vec<Plane>>,
    async_buffers: HashMap<u32, Arc<Buffer>>,
    legacy_events: HashSet<(u32, u16)>,
}

pub(super) struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
    pub gpu: Option<String>,
}

impl DmaState {
    pub(super) fn request(
        &mut self,
        state: &mut State<'_>,
        request: (&str, &str),
        arguments: &[Arg],
        bytes: &mut [u8],
        fds: &mut Vec<OwnedFd>,
        action: &mut Request,
    ) -> Result<(), String> {
        let id = decode_wayland_header(bytes)?.object_id;
        let from_app = state.end == End::Application;
        match (request.0, request.1, arguments) {
            ("zwp_linux_dmabuf_v1", "create_params", [Arg::NewId(params)]) => {
                self.params.insert(*params, Vec::new());
            }
            ("zwp_linux_buffer_params_v1", "destroy", _) => {
                self.params.remove(&id);
                self.async_buffers.remove(&id);
            }
            (
                "zwp_linux_buffer_params_v1",
                "add",
                [
                    Arg::Fd,
                    Arg::Uint(index),
                    Arg::Uint(offset),
                    Arg::Uint(stride),
                    Arg::Uint(hi),
                    Arg::Uint(lo),
                ],
            ) => {
                let planes = self.params.get_mut(&id).ok_or("unknown DMA-BUF params")?;
                if from_app {
                    if *index != 0 || !planes.is_empty() {
                        return Err("SSH Wayland requires a single-plane RGB DMA-BUF".into());
                    }
                    let fd = fds.pop().ok_or("DMA-BUF descriptor missing")?;
                    let gpu = ssh_video::dma_buf_gpu(fd.as_raw_fd());
                    planes.push(Plane {
                        fd,
                        offset: *offset,
                        stride: *stride,
                        modifier: (u64::from(*hi) << 32) | u64::from(*lo),
                        gpu,
                    });
                } else {
                    action.forward = false;
                }
            }
            ("zwp_linux_buffer_params_v1", "create" | "create_immed", args) => {
                let (buffer_id, geometry) = match args {
                    [Arg::NewId(id), geometry @ ..] => (Some(*id), geometry),
                    geometry => (None, geometry),
                };
                let [
                    Arg::Int(width),
                    Arg::Int(height),
                    Arg::Uint(format),
                    Arg::Uint(flags),
                ] = geometry
                else {
                    return Err("invalid DMA-BUF dimensions or configuration".into());
                };
                let (width, height) = extent(*width, *height)?;
                if flags & !1 != 0 {
                    return Err("SSH Wayland requires progressive DMA-BUFs".into());
                }
                let planes = self
                    .params
                    .remove(&id)
                    .ok_or("unknown or consumed DMA-BUF params")?;
                let gpu = state.gpu.ok_or("no render node for SSH Wayland DMA-BUFs")?;
                let backing = if from_app {
                    let [plane] = planes.as_slice() else {
                        return Err("SSH Wayland requires a single-plane RGB DMA-BUF".into());
                    };
                    let layout = (*format, plane.modifier);
                    let importable = formats(gpu)?.contains(&layout)
                        || plane.gpu.as_deref().is_some_and(|source| {
                            formats(source).is_ok_and(|formats| formats.contains(&layout))
                        })
                        || ssh_video::bridge_render_nodes().iter().any(|node| {
                            formats(node).is_ok_and(|formats| formats.contains(&layout))
                        });
                    if !importable {
                        return Err(format!(
                            "no GPU can import DMA-BUF format {format:#x} with modifier {:#x}",
                            plane.modifier,
                        ));
                    }
                    Backing::SourceDma(planes)
                } else {
                    let output_format = super::display_format(*format);
                    let buffer = DisplayBuffer::new(gpu, width, height, output_format)?;
                    action.before.push(buffer.plane_request(id)?);
                    let position = bytes.len() - 8;
                    bytes[position..position + 4].copy_from_slice(&output_format.to_ne_bytes());
                    Backing::DisplayDma(buffer, *flags)
                };
                let buffer = Buffer::new(width, height, *format, backing);
                if let Some(id) = buffer_id {
                    state.buffers.insert(id, buffer);
                } else {
                    self.async_buffers.insert(id, buffer);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn event(
        &mut self,
        state: &mut State<'_>,
        event: &DecodedWaylandEvent,
        fds: &mut Vec<OwnedFd>,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<bool, String> {
        let id = event.object_id;
        match &event.generated_event {
            GeneratedEvent::ZwpLinuxBufferParamsV1Created { buffer } => {
                let storage = self
                    .async_buffers
                    .remove(&id)
                    .ok_or("unexpected DMA-BUF creation")?;
                state.buffers.insert(*buffer, storage);
            }
            GeneratedEvent::ZwpLinuxBufferParamsV1Failed => {
                self.async_buffers.remove(&id);
            }
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1FormatTable { .. } => {
                fds.clear();
                return Ok(false);
            }
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1MainDevice { .. }
            | GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheTargetDevice { .. }
            | GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFormats { .. }
            | GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFlags { .. }
            | GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheDone => return Ok(false),
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1Done if state.end == End::Application => {
                *before = feedback(id, state.gpu.ok_or("no application DRM node")?)?;
            }
            GeneratedEvent::ZwpLinuxDmabufV1Format { .. }
            | GeneratedEvent::ZwpLinuxDmabufV1Modifier { .. }
                if state.end == End::Application =>
            {
                let opcode = u16::from(matches!(
                    event.generated_event,
                    GeneratedEvent::ZwpLinuxDmabufV1Modifier { .. }
                ));
                if self.legacy_events.insert((id, opcode)) {
                    let mut advertised = HashSet::new();
                    for (format, modifier) in formats(state.gpu.ok_or("no application DRM node")?)?
                        .iter()
                        .copied()
                    {
                        if opcode == 0 && advertised.insert(format) {
                            before.push(wire(id, 0, &[format], Vec::new()));
                        } else if opcode == 1 {
                            before.push(wire(
                                id,
                                1,
                                &[format, (modifier >> 32) as u32, modifier as u32],
                                Vec::new(),
                            ));
                        }
                    }
                }
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    }

    pub(super) fn delete_id(&mut self, id: u32) {
        self.legacy_events.remove(&(id, 0));
        self.legacy_events.remove(&(id, 1));
    }
}

type Formats = Arc<Vec<(u32, u64)>>;

pub(super) fn formats(gpu: &str) -> Result<Formats, String> {
    static FORMATS: OnceLock<Mutex<HashMap<String, Formats>>> = OnceLock::new();
    let mut cache = FORMATS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(io_error)?;
    Ok(cache
        .entry(gpu.to_owned())
        .or_insert_with(|| {
            Arc::new(
                [
                    *b"XR24", *b"AR24", *b"XB24", *b"AB24", *b"XR30", *b"AR30", *b"XB30", *b"AB30",
                    *b"XB4H", *b"AB4H",
                ]
                .into_iter()
                .flat_map(|format| {
                    let format = u32::from_le_bytes(format);
                    ssh_video::capture_modifiers(gpu, format)
                        .into_iter()
                        .map(move |modifier| (format, modifier))
                })
                .collect(),
            )
        })
        .clone())
}

fn feedback(id: u32, node: &str) -> Result<Vec<WaylandWireMessage>, String> {
    let formats = formats(node)?;
    if formats.is_empty() {
        return Err(format!("no importable RGB DMA-BUF formats on {node}"));
    }
    let device = std::fs::metadata(node)
        .map_err(io_error)?
        .rdev()
        .to_ne_bytes()
        .to_vec();
    let size = formats.len() * 16;
    let table = memfd(size)?;
    let mut data = Vec::with_capacity(size);
    let mut indices = Vec::with_capacity(formats.len() * 2);
    for (index, (format, modifier)) in formats.iter().enumerate() {
        data.extend_from_slice(&format.to_ne_bytes());
        data.extend_from_slice(&[0; 4]);
        data.extend_from_slice(&modifier.to_ne_bytes());
        indices.extend_from_slice(&(index as u16).to_ne_bytes());
    }
    table.write_all_at(&data, 0).map_err(io_error)?;
    [
        (
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1FormatTable {
                fd: true,
                size: size as u32,
            },
            vec![table.into()],
        ),
        (
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1MainDevice {
                device: device.clone(),
            },
            Vec::new(),
        ),
        (
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheTargetDevice { device },
            Vec::new(),
        ),
        (
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFlags { flags: 0 },
            Vec::new(),
        ),
        (
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFormats { indices },
            Vec::new(),
        ),
        (
            GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheDone,
            Vec::new(),
        ),
    ]
    .into_iter()
    .map(|(event, fds)| {
        Ok(WaylandWireMessage {
            bytes: encode_generated_event(id, &event)?,
            fds,
        })
    })
    .collect()
}

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
    fn gbm_bo_create(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_destroy(buffer: *mut c_void);
    fn gbm_bo_get_fd(buffer: *mut c_void) -> i32;
    fn gbm_bo_get_stride(buffer: *mut c_void) -> u32;
    fn gbm_bo_get_modifier(buffer: *mut c_void) -> u64;
}

struct GbmDevice {
    handle: NonNull<c_void>,
    _fd: File,
}

// GBM operations on this device are serialized by the mutex in the device cache.
unsafe impl Send for GbmDevice {}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.handle.as_ptr()) };
    }
}

type SharedDevice = Arc<Mutex<GbmDevice>>;

fn gbm_device(node: &str) -> Result<SharedDevice, String> {
    static DEVICES: OnceLock<Mutex<HashMap<String, Weak<Mutex<GbmDevice>>>>> = OnceLock::new();
    let mut devices = DEVICES
        .get_or_init(Mutex::default)
        .lock()
        .map_err(io_error)?;
    if let Some(device) = devices.get(node).and_then(Weak::upgrade) {
        return Ok(device);
    }
    let fd = File::options()
        .read(true)
        .write(true)
        .open(node)
        .map_err(io_error)?;
    let handle = NonNull::new(unsafe { gbm_create_device(fd.as_raw_fd()) })
        .ok_or("could not open GBM display device")?;
    let device = Arc::new(Mutex::new(GbmDevice { handle, _fd: fd }));
    devices.insert(node.to_owned(), Arc::downgrade(&device));
    Ok(device)
}

pub(crate) struct DisplayBuffer {
    buffer: NonNull<c_void>,
    device: SharedDevice,
    fd: OwnedFd,
    width: u32,
    stride: u32,
    modifier: u64,
    identity: Option<(libc::dev_t, libc::ino_t)>,
}

type DisplayAffinities = HashMap<(libc::dev_t, libc::ino_t), (u32, u32)>;
static DISPLAY_AFFINITIES: std::sync::LazyLock<Mutex<DisplayAffinities>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn buffer_identity(fd: RawFd) -> Option<(libc::dev_t, libc::ino_t)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some((stat.st_dev, stat.st_ino))
}

pub(crate) fn display_buffer_affinity(fd: RawFd) -> Option<(u32, u32)> {
    let identity = buffer_identity(fd)?;
    DISPLAY_AFFINITIES.lock().unwrap().get(&identity).copied()
}

// The GBM pointer is used only under the shared device lock.
unsafe impl Send for DisplayBuffer {}
unsafe impl Sync for DisplayBuffer {}

impl Drop for DisplayBuffer {
    fn drop(&mut self) {
        if let Some(identity) = self.identity {
            DISPLAY_AFFINITIES.lock().unwrap().remove(&identity);
        }
        let _device = self
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { gbm_bo_destroy(self.buffer.as_ptr()) };
    }
}

impl DisplayBuffer {
    pub(crate) fn new(node: &str, width: u32, height: u32, format: u32) -> Result<Self, String> {
        const GBM_USE_RENDERING: u32 = 1 << 2;
        const GBM_USE_LINEAR: u32 = 1 << 4;
        let device = gbm_device(node)?;
        let _device = device.lock().map_err(io_error)?;
        let buffer = NonNull::new(unsafe {
            gbm_bo_create(
                _device.handle.as_ptr(),
                width,
                height,
                format,
                GBM_USE_RENDERING | GBM_USE_LINEAR,
            )
        })
        .ok_or_else(|| {
            format!("could not allocate linear RGB DMA-BUF format {format:#x} on {node}")
        })?;
        let stride = unsafe { gbm_bo_get_stride(buffer.as_ptr()) };
        let modifier = unsafe { gbm_bo_get_modifier(buffer.as_ptr()) };
        let fd = unsafe { gbm_bo_get_fd(buffer.as_ptr()) };
        if fd < 0 || modifier != 0 {
            let error = std::io::Error::last_os_error();
            if fd >= 0 {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            unsafe { gbm_bo_destroy(buffer.as_ptr()) };
            return Err(if fd < 0 {
                format!("export reconstructed DMA-BUF: {error}")
            } else {
                format!("expected a linear display DMA-BUF, got modifier {modifier:#x}")
            });
        }
        drop(_device);
        let identity = buffer_identity(fd).filter(|identity| {
            if let Ok(affinity) = ssh_video::render_affinity(node) {
                DISPLAY_AFFINITIES
                    .lock()
                    .unwrap()
                    .insert(*identity, affinity);
                true
            } else {
                false
            }
        });
        Ok(Self {
            buffer,
            device,
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            width,
            stride,
            modifier,
            identity,
        })
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn plane(&self) -> ssh_video::BridgePlane {
        ssh_video::BridgePlane {
            fd: self.fd.as_raw_fd(),
            offset: 0,
            stride: self.stride,
            modifier: self.modifier,
        }
    }

    pub(super) fn plane_request(&self, params: u32) -> Result<WaylandWireMessage, String> {
        let fd = self.fd.as_fd().try_clone_to_owned().map_err(io_error)?;
        Ok(wire(
            params,
            1,
            &[
                0,
                0,
                self.stride,
                (self.modifier >> 32) as u32,
                self.modifier as u32,
            ],
            vec![fd],
        ))
    }
}
