use super::{
    DecodedWaylandArg as Arg, GeneratedArgKind, GeneratedArgSpec, GeneratedDecodedArg,
    GeneratedEvent, WaylandWireMessage, decode_generated_hook_request_by_id, decode_wayland_args,
    decode_wayland_event, decode_wayland_header, find_generated_event_by_opcode,
    find_generated_request_by_opcode, find_generated_request_id_by_opcode,
    read_wayland_wire_message_blocking, rewrite_wire_object_ids, send_wayland_wire_message_to_fd,
};
use crate::gui_color::{ColorDescription, SurfaceColors};
use crate::ssh_video::{
    self, BridgePicture, BridgePixels, BridgePlane, BridgeSource, DisplayPixels, FrameRequest,
    NativeShm,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[path = "wayland_bridge_display.rs"]
mod display;
pub(crate) use display::{DisplayBuffer, display_buffer_affinity};
#[path = "wayland_bridge_buffers.rs"]
mod host;
#[path = "wayland_bridge_presentation.rs"]
mod presentation;
#[path = "wayland_bridge_sync.rs"]
mod sync;

const MAX_KEYMAP: usize = 4 * 1024 * 1024;
const MAX_FDS: usize = 28;
const MAX_POOL: u64 = 512 * 1024 * 1024;
const XR24: u32 = u32::from_le_bytes(*b"XR24");
const AR24: u32 = u32::from_le_bytes(*b"AR24");
const XR30: u32 = u32::from_le_bytes(*b"XR30");
const AR30: u32 = u32::from_le_bytes(*b"AR30");
const XB30: u32 = u32::from_le_bytes(*b"XB30");
const AB30: u32 = u32::from_le_bytes(*b"AB30");
const TF_GAMMA22: u32 = 2;
const TF_SRGB: u32 = 9;
const WIRE_VERSION: &[u8; 14] = b"WMC-WAYLAND-8\0";

fn clock_identity() -> [u8; 44] {
    let mut identity = [0; 44];
    if let (Ok(boot), Ok(namespace)) = (
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id"),
        std::fs::metadata("/proc/self/ns/time"),
    ) && boot.trim().len() == 36
    {
        identity[..36].copy_from_slice(boot.trim().as_bytes());
        identity[36..].copy_from_slice(&namespace.ino().to_le_bytes());
    }
    identity
}

fn display_format(format: u32) -> u32 {
    match format {
        XR30 | XB30 => XR30,
        AR30 | AB30 => AR30,
        _ if ssh_video::has_alpha(format) => AR24,
        _ => XR24,
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum End {
    Application,
    Compositor,
}

enum Payload {
    None,
    Keymap(Vec<u8>),
    Picture(BridgePicture),
}

struct Packet {
    bytes: Vec<u8>,
    payload: Payload,
}

enum TunnelMessage {
    Wayland(Packet, Option<PictureTransfer>),
    ClockProbe(u64),
    ClockReply([u64; 3]),
    PictureReceived(u32, PictureTransfer),
}

#[derive(Clone, Copy)]
struct PictureTransfer {
    bytes: u32,
    nanos: u64,
}

#[derive(Default)]
struct TransportRate {
    samples: VecDeque<PictureTransfer>,
    limit: Option<(u32, Instant)>,
}

impl TransportRate {
    fn received(&mut self, transfer: PictureTransfer, now: Instant) -> Option<u32> {
        const WINDOW: usize = 16;
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(transfer);
        if self.samples.len() < WINDOW {
            return None;
        }
        // Require sustained waits for substantial pictures. A slow keyframe
        // or an idle window with a tiny update gives no reliable capacity data.
        let mut slow = [0; WINDOW];
        let mut count = 0;
        for &PictureTransfer { bytes, nanos } in &self.samples {
            if bytes >= 12_000 && nanos >= 12_000_000 {
                slow[count] = u64::from(bytes) * 8_000_000 / (nanos / 1_000);
                count += 1;
            }
        }
        if count < 6 {
            return None;
        }
        let (_, capacity, _) = slow[..count].select_nth_unstable(count / 2);
        // VA-API VBR allows peaks at twice the target. Keep that peak
        // within the estimated transport capacity.
        let limit = (*capacity / 2_000_000).clamp(2, 80) as u32 * 1_000_000;
        if self.limit.is_some_and(|(previous, changed)| {
            limit >= previous * 3 / 4 || now.duration_since(changed).as_secs() < 2
        }) {
            return None;
        }
        self.limit = Some((limit, now));
        self.samples.clear();
        Some(limit)
    }
}

impl TunnelMessage {
    fn read(stream: &UnixStream) -> Result<TunnelMessage, String> {
        let mut header = [0; 16];
        let mut stream = stream;
        stream.read_exact(&mut header).map_err(io_error)?;
        let length =
            |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap()) as usize;
        let (wire_len, meta_len, data_len, alpha_len) =
            (length(0), length(4), length(8), length(12));
        // A zero Wayland length identifies control messages. They share
        // the tunnel framing but never reach either Wayland connection.
        if wire_len == 0 && data_len == 0 && alpha_len == 0 {
            return match meta_len {
                8 => {
                    let mut probe = [0; 8];
                    stream.read_exact(&mut probe).map_err(io_error)?;
                    Ok(TunnelMessage::ClockProbe(u64::from_le_bytes(probe)))
                }
                16 => {
                    let mut report = [0; 16];
                    stream.read_exact(&mut report).map_err(io_error)?;
                    Ok(Self::PictureReceived(
                        u32::from_le_bytes(report[..4].try_into().unwrap()),
                        PictureTransfer {
                            bytes: u32::from_le_bytes(report[4..8].try_into().unwrap()),
                            nanos: u64::from_le_bytes(report[8..].try_into().unwrap()),
                        },
                    ))
                }
                24 => {
                    let mut reply = [0; 24];
                    stream.read_exact(&mut reply).map_err(io_error)?;
                    let (words, _) = reply.as_chunks::<8>();
                    let words: &[[u8; 8]; 3] = words.try_into().unwrap();
                    Ok(TunnelMessage::ClockReply(words.map(u64::from_le_bytes)))
                }
                _ => Err("invalid SSH Wayland control packet length".into()),
            };
        }
        if !(8..=u16::MAX as usize).contains(&wire_len)
            || (meta_len == 0 && (data_len > MAX_KEYMAP || alpha_len != 0))
            || meta_len > 1024
            || data_len > ssh_video::MAX_ACCESS_UNIT
            || alpha_len > ssh_video::MAX_ACCESS_UNIT
        {
            return Err("invalid SSH Wayland packet length".into());
        }
        let mut bytes = vec![0; wire_len];
        stream.read_exact(&mut bytes).map_err(io_error)?;
        if decode_wayland_header(&bytes)?.size as usize != wire_len {
            return Err("Wayland message length does not match its SSH packet".into());
        }
        let mut meta = vec![0; meta_len];
        stream.read_exact(&mut meta).map_err(io_error)?;
        let mut data = vec![0; data_len];
        let (payload, transfer) = if !meta.is_empty() {
            let mut alpha = vec![0; alpha_len];
            let started = Instant::now();
            stream.read_exact(&mut data).map_err(io_error)?;
            stream.read_exact(&mut alpha).map_err(io_error)?;
            let transfer = PictureTransfer {
                bytes: (data_len + alpha_len) as u32,
                nanos: u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            };
            let picture = BridgePicture {
                frame: serde_json::from_slice(&meta).map_err(io_error)?,
                color: data,
                alpha,
            };
            (Payload::Picture(picture), Some(transfer))
        } else if !data.is_empty() {
            stream.read_exact(&mut data).map_err(io_error)?;
            (Payload::Keymap(data), None)
        } else {
            (Payload::None, None)
        };
        Ok(Self::Wayland(Packet { bytes, payload }, transfer))
    }
}

impl Packet {
    fn write(self, mut stream: &UnixStream) -> Result<(), String> {
        let mut meta = Vec::new();
        let mut alpha = Vec::new();
        let data = match self.payload {
            Payload::None => Vec::new(),
            Payload::Keymap(data) => data,
            Payload::Picture(picture) => {
                meta = serde_json::to_vec(&picture.frame).map_err(io_error)?;
                alpha = picture.alpha;
                picture.color
            }
        };
        let mut prefix = Vec::with_capacity(16 + self.bytes.len() + meta.len());
        for len in [self.bytes.len(), meta.len(), data.len(), alpha.len()] {
            prefix.extend_from_slice(&(len as u32).to_le_bytes());
        }
        prefix.extend_from_slice(&self.bytes);
        prefix.extend_from_slice(&meta);
        stream.write_all(&prefix).map_err(io_error)?;
        stream.write_all(&data).map_err(io_error)?;
        stream.write_all(&alpha).map_err(io_error)
    }
}

fn write_control(mut stream: &UnixStream, words: &[u64]) -> Result<(), String> {
    let mut packet = [0; 40];
    let len = std::mem::size_of_val(words);
    packet[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    for (field, word) in packet[16..].as_chunks_mut::<8>().0.iter_mut().zip(words) {
        *field = word.to_le_bytes();
    }
    stream.write_all(&packet[..16 + len]).map_err(io_error)
}

#[derive(Clone)]
pub(crate) struct VideoSettings {
    pub request: FrameRequest,
    pub source_color: Option<ColorDescription>,
    pub preserve_alpha: bool,
    pub rate_control: ssh_video::StreamRateControl,
}

pub(crate) struct Endpoint {
    pub end: End,
    pub promote_shm: bool,
    pub warmup: Option<ssh_video::ConverterWarmup>,
    pub gpu: Option<String>,
    pub video: Arc<std::sync::RwLock<VideoSettings>>,
}

pub(crate) fn relay(
    native: &UnixStream,
    tunnel: &UnixStream,
    endpoint: &Endpoint,
) -> Result<(), String> {
    let mut peer = tunnel;
    peer.write_all(WIRE_VERSION).map_err(io_error)?;
    let mut version = [0; 14];
    peer.read_exact(&mut version).map_err(io_error)?;
    if &version != WIRE_VERSION {
        return Err("incompatible SSH Wayland endpoint".into());
    }
    let clock = clock_identity();
    peer.write_all(&clock).map_err(io_error)?;
    let mut peer_clock = [0; 44];
    peer.read_exact(&mut peer_clock).map_err(io_error)?;
    relay_wayland(
        native,
        tunnel,
        endpoint,
        clock != [0; 44] && clock == peer_clock,
    )
}

fn relay_wayland(
    native: &UnixStream,
    tunnel: &UnixStream,
    endpoint: &Endpoint,
    shared_clock: bool,
) -> Result<(), String> {
    let mut state = State::new(endpoint.end, endpoint.gpu.as_deref());
    state.shared_clock = shared_clock;
    state.promote_shm = endpoint.promote_shm;
    let state = Mutex::new(state);
    let native_write = Mutex::new(());
    let tunnel_write = Mutex::new(());
    std::thread::scope(|scope| {
        let requests = scope.spawn(|| {
            let result = requests(
                native,
                tunnel,
                endpoint,
                &state,
                &native_write,
                &tunnel_write,
            );
            let _ = native.shutdown(Shutdown::Both);
            let _ = tunnel.shutdown(Shutdown::Both);
            result
        });
        let result = events(
            native,
            tunnel,
            endpoint,
            &state,
            &native_write,
            &tunnel_write,
        );
        if let Ok(mut state) = state.lock() {
            state.closed = true;
            state.host.slot_released.notify_all();
        }
        let _ = native.shutdown(Shutdown::Both);
        let _ = tunnel.shutdown(Shutdown::Both);
        let requests = requests
            .join()
            .map_err(|_| "SSH Wayland request thread panicked")?;
        for error in [requests, result].into_iter().filter_map(Result::err) {
            if !matches!(
                error.as_str(),
                "failed to fill whole buffer"
                    | "backend Wayland stream reached EOF"
                    | "Broken pipe (os error 32)"
                    | "failed to send Wayland wire message with fds: Broken pipe (os error 32)"
                    | "Connection reset by peer (os error 104)"
                    | "failed to receive Wayland wire message with fds: Connection reset by peer (os error 104)"
            ) {
                return Err(error);
            }
        }
        Ok(())
    })
}

fn receive_native(stream: &UnixStream, queue: &mut VecDeque<OwnedFd>) -> Result<Vec<u8>, String> {
    let WaylandWireMessage { bytes, fds } = read_wayland_wire_message_blocking(stream, MAX_FDS)?;
    queue.extend(fds);
    if queue.len() > 128 {
        return Err("too many pending Wayland descriptors".into());
    }
    Ok(bytes)
}

fn take_fds(queue: &mut VecDeque<OwnedFd>, count: usize) -> Result<Vec<OwnedFd>, String> {
    if queue.len() < count {
        return Err("Wayland message is missing a declared descriptor".into());
    }
    Ok(queue.drain(..count).collect())
}

fn fd_count(specs: &[super::GeneratedArgSpec]) -> usize {
    specs
        .iter()
        .filter(|arg| arg.kind == GeneratedArgKind::Fd)
        .count()
}

fn requests(
    native: &UnixStream,
    tunnel: &UnixStream,
    endpoint: &Endpoint,
    state: &Mutex<State>,
    native_write: &Mutex<()>,
    tunnel_write: &Mutex<()>,
) -> Result<(), String> {
    let from_app = endpoint.end == End::Application;
    let mut encoders = HashMap::new();
    let mut decoders = HashMap::new();
    let mut pending_fds = VecDeque::new();
    loop {
        let (mut packet, transfer) = if from_app {
            (
                Packet {
                    bytes: receive_native(native, &mut pending_fds)?,
                    payload: Payload::None,
                },
                None,
            )
        } else {
            match TunnelMessage::read(tunnel)? {
                TunnelMessage::Wayland(packet, transfer) => (packet, transfer),
                TunnelMessage::ClockProbe(sent) => {
                    let received = presentation::monotonic()?;
                    let _writer = tunnel_write.lock().map_err(io_error)?;
                    write_control(tunnel, &[sent, received, presentation::monotonic()?])?;
                    continue;
                }
                _ => return Err("unexpected compositor tunnel control message".into()),
            }
        };
        let mut action = {
            let mut state = state.lock().map_err(io_error)?;
            state.request(&mut packet.bytes, &mut pending_fds)?
        };
        if !action.frame_ready.is_empty() {
            let _writer = native_write.lock().map_err(io_error)?;
            let now = (presentation::monotonic()? / 1_000_000) as u32;
            for callback in action.frame_ready.drain(..) {
                write_native(native, &wire(callback, 0, &[now], Vec::new()).bytes, &[])?;
            }
        }
        if let Some(surface) = action.destroy_surface {
            encoders.remove(&surface);
            decoders.remove(&surface);
        }
        if let Some(commit) = action.commit {
            let surface = commit.surface;
            if from_app {
                // A release may cross an attach in the other direction. The
                // request stream alone cannot identify an unchanged picture.
                let mut settings = endpoint.video.read().map_err(io_error)?.clone();
                if settings.rate_control == ssh_video::StreamRateControl::Auto {
                    let mut state = state.lock().map_err(io_error)?;
                    if let Some((limit, _)) = state.rates.entry(surface).or_default().limit {
                        settings.rate_control = ssh_video::StreamRateControl::TransportVbr(limit);
                    }
                }
                let gpu = settings
                    .request
                    .capture_gpu
                    .as_deref()
                    .or(endpoint.gpu.as_deref())
                    .ok_or("no capture render node for SSH Wayland")?;
                let acquire_start = ssh_video::timing_start();
                if let Some(sync) = &commit.sync {
                    sync.wait()?;
                }
                if acquire_start.is_some() {
                    eprintln!(
                        "SSH acquire surface={} acquire_ms={:.3}",
                        commit.surface,
                        ssh_video::timing_ms(acquire_start),
                    );
                }
                if let Some(warmup) = &endpoint.warmup {
                    warmup.adopt(gpu);
                }
                let color = commit.color.as_ref().or(settings.source_color.as_ref());
                packet.payload = commit.attach.buffer.encode(
                    &settings,
                    gpu,
                    color,
                    commit.sync,
                    encoders.entry(surface).or_default(),
                    || {
                        let _writer = native_write.lock().map_err(io_error)?;
                        write_native(
                            native,
                            &wire(commit.attach.id, 0, &[], Vec::new()).bytes,
                            &[],
                        )
                    },
                )?;
                if commit.changed_color && std::env::var_os("WAYLAND_MCP_TRACE").is_some() {
                    let tone_map = settings.request.effective_tone_map(
                        commit.attach.buffer.format,
                        color.is_some_and(ColorDescription::is_hdr),
                    );
                    eprintln!(
                        "SSH Wayland surface {} encoded with {} on {gpu}, tone map {tone_map:?}",
                        commit.surface,
                        color.map_or_else(
                            || serde_json::json!({"source_assumption":"sRGB"}),
                            ColorDescription::source_metadata
                        ),
                    );
                }
            } else {
                let Payload::Picture(picture) =
                    std::mem::replace(&mut packet.payload, Payload::None)
                else {
                    return Err("missing H.265 picture for the attached Wayland buffer".into());
                };
                if let Some(PictureTransfer { bytes, nanos }) = transfer {
                    let _writer = tunnel_write.lock().map_err(io_error)?;
                    let header = u64::from(surface) | (u64::from(bytes) << 32);
                    write_control(tunnel, &[header, nanos])?;
                }
                if let Some(warmup) = &endpoint.warmup {
                    warmup.adopt(picture.decode_gpu());
                }
                let slot_start = ssh_video::timing_start();
                let slot = {
                    let guard = state.lock().map_err(io_error)?;
                    let released = Arc::clone(&guard.host.slot_released);
                    let (mut guard, _) = released
                        .wait_timeout_while(guard, std::time::Duration::from_secs(5), |state| {
                            !state.closed && !state.host.can_commit(commit.attach.id)
                        })
                        .map_err(io_error)?;
                    if guard.closed {
                        return Err("backend Wayland stream reached EOF".into());
                    }
                    guard.display_commit(commit, &mut action.before)?
                };
                slot.publish(
                    &picture,
                    decoders.entry(surface).or_default(),
                    ssh_video::timing_ms(slot_start),
                )?;
            }
        }
        if from_app {
            if action.forward {
                if action.probe_clock {
                    let probe = state.lock().map_err(io_error)?.presentation.probe()?;
                    if let Some(sent) = probe {
                        write_control(tunnel, &[sent])?;
                    }
                }
                let transfer = match &packet.payload {
                    Payload::Picture(picture) => ssh_video::timing_start()
                        .map(|start| (start, picture.color.len() + picture.alpha.len())),
                    _ => None,
                };
                packet.write(tunnel)?;
                if let Some((start, bytes)) = transfer {
                    eprintln!(
                        "SSH picture handoff bytes={bytes} write_ms={:.3}",
                        ssh_video::timing_ms(Some(start))
                    );
                }
            }
            if !action.before.is_empty() || action.local_delete.is_some() {
                let _writer = native_write.lock().map_err(io_error)?;
                for message in action.before {
                    write_native(native, &message.bytes, &message.fds)?;
                }
                if let Some(id) = action.local_delete {
                    write_native(native, &wire(1, 1, &[id], Vec::new()).bytes, &[])?;
                }
            }
        } else {
            let _writer = native_write.lock().map_err(io_error)?;
            if !matches!(packet.payload, Payload::None) {
                return Err("unexpected data in a Wayland request".into());
            }
            for message in action.before {
                write_native(native, &message.bytes, &message.fds)?;
            }
            if action.forward {
                write_native(native, &packet.bytes, &action.fds)?;
            }
        }
    }
}

fn events(
    native: &UnixStream,
    tunnel: &UnixStream,
    endpoint: &Endpoint,
    state: &Mutex<State>,
    native_write: &Mutex<()>,
    tunnel_write: &Mutex<()>,
) -> Result<(), String> {
    let from_compositor = endpoint.end == End::Compositor;
    let mut pending_fds = VecDeque::new();
    loop {
        let mut packet = if from_compositor {
            Packet {
                bytes: receive_native(native, &mut pending_fds)?,
                payload: Payload::None,
            }
        } else {
            match TunnelMessage::read(tunnel)? {
                TunnelMessage::Wayland(packet, _) => packet,
                TunnelMessage::ClockReply(reply) => {
                    let now = presentation::monotonic()?;
                    state
                        .lock()
                        .map_err(io_error)?
                        .presentation
                        .sample(reply, now)?;
                    continue;
                }
                TunnelMessage::PictureReceived(surface, transfer) => {
                    let mut state = state.lock().map_err(io_error)?;
                    if let Some(rate) = state.rates.get_mut(&surface) {
                        if ssh_video::timing_enabled() {
                            eprintln!(
                                "SSH picture transfer surface={surface} bytes={} read_ms={:.3}",
                                transfer.bytes,
                                transfer.nanos as f64 / 1_000_000.0,
                            );
                        }
                        if let Some(limit) = rate.received(transfer, Instant::now())
                            && ssh_video::timing_enabled()
                        {
                            eprintln!(
                                "SSH transport rate surface={surface} target_bitrate={limit}"
                            );
                        }
                    }
                    continue;
                }
                TunnelMessage::ClockProbe(_) => return Err("unexpected clock probe".into()),
            }
        };
        let (event, mut fds, forward, before) = {
            let mut state = state.lock().map_err(io_error)?;
            state.event(&mut packet.bytes, &mut pending_fds)?
        };
        if !before.is_empty() {
            let _writer = native_write.lock().map_err(io_error)?;
            for message in before {
                write_native(native, &message.bytes, &message.fds)?;
            }
        }
        if !forward {
            continue;
        }
        match event.as_ref().map(|event| &event.generated_event) {
            Some(GeneratedEvent::WlKeyboardKeymap { size, .. }) => {
                if from_compositor {
                    let fd = fds.pop().ok_or("keyboard keymap fd missing")?;
                    packet.payload = Payload::Keymap(compress_keymap(fd, *size as usize)?);
                } else {
                    let Payload::Keymap(compressed) =
                        std::mem::replace(&mut packet.payload, Payload::None)
                    else {
                        return Err("keyboard keymap data missing".into());
                    };
                    fds.push(decompress_keymap(&compressed, *size as usize)?);
                }
            }
            _ if !fds.is_empty() => return Err("unsupported Wayland event descriptor".into()),
            _ => {}
        }
        if from_compositor {
            let _writer = tunnel_write.lock().map_err(io_error)?;
            packet.write(tunnel)?;
        } else {
            if !matches!(packet.payload, Payload::None) {
                return Err("unexpected data in a Wayland event".into());
            }
            let _writer = native_write.lock().map_err(io_error)?;
            write_native(native, &packet.bytes, &fds)?;
        }
    }
}

struct Pool {
    fd: File,
    size: u64,
    next_offset: u64,
}

struct Commit {
    surface: u32,
    surface_version: u32,
    attach: Attachment,
    color: Option<ColorDescription>,
    changed_color: bool,
    sync: Option<sync::CommitSync>,
}

struct Attachment {
    id: u32,
    buffer: Arc<Buffer>,
    x: i32,
    y: i32,
}

#[derive(Default)]
struct LegacyTransfers {
    gamma22: bool,
    srgb: bool,
}

impl LegacyTransfers {
    fn needs_srgb_alias(&self) -> bool {
        self.gamma22 && !self.srgb
    }
}

#[derive(Default)]
struct LegacyColorIdentities {
    next: u32,
    identities: HashMap<(u32, bool), (u32, usize)>,
    objects: HashMap<u32, (u32, bool)>,
}

impl LegacyColorIdentities {
    fn record(&mut self, object: u32, identity: u32, srgb_alias: bool) -> Result<u32, String> {
        let key = (identity, srgb_alias);
        if self.objects.get(&object) == Some(&key) {
            return Ok(self.identities[&key].0);
        }
        self.remove(object);
        self.objects.insert(object, key);
        if let Some((local, references)) = self.identities.get_mut(&key) {
            *references += 1;
            return Ok(*local);
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("too many Wayland color identities")?;
        self.identities.insert(key, (self.next, 1));
        Ok(self.next)
    }

    fn remove(&mut self, object: u32) {
        if let Some(key) = self.objects.remove(&object)
            && let Some((_, references)) = self.identities.get_mut(&key)
        {
            *references -= 1;
            if *references == 0 {
                self.identities.remove(&key);
            }
        }
    }
}

struct State<'a> {
    end: End,
    gpu: Option<&'a str>,
    promote_shm: bool,
    objects: HashMap<u32, String>,
    versions: HashMap<u32, u32>,
    globals: HashMap<u32, (String, u32)>,
    pools: HashMap<u32, Pool>,
    buffers: HashMap<u32, Arc<Buffer>>,
    pending: HashMap<u32, Attachment>,
    dma: display::DmaState,
    host: host::Host,
    colors: SurfaceColors,
    legacy_transfers: HashMap<u32, LegacyTransfers>,
    srgb_alias_creators: HashMap<u32, bool>,
    srgb_alias_descriptions: HashSet<u32>,
    legacy_color_ids: LegacyColorIdentities,
    traced_colors: HashMap<u32, Option<ColorDescription>>,
    sync: sync::SourceSync,
    shared_clock: bool,
    closed: bool,
    presentation: presentation::Clock,
    frames: presentation::Frames,
    xdg_surfaces: HashMap<u32, u32>,
    rates: HashMap<u32, TransportRate>,
}

struct Request {
    forward: bool,
    before: Vec<WaylandWireMessage>,
    fds: Vec<OwnedFd>,
    commit: Option<Commit>,
    destroy_surface: Option<u32>,
    local_delete: Option<u32>,
    probe_clock: bool,
    frame_ready: Vec<u32>,
}

impl<'a> State<'a> {
    fn new(end: End, gpu: Option<&'a str>) -> Self {
        Self {
            end,
            gpu,
            promote_shm: false,
            objects: HashMap::from([(1, "wl_display".into())]),
            versions: HashMap::new(),
            globals: HashMap::new(),
            pools: HashMap::new(),
            buffers: HashMap::new(),
            pending: HashMap::new(),
            dma: display::DmaState::default(),
            host: host::Host::default(),
            colors: SurfaceColors::default(),
            legacy_transfers: HashMap::new(),
            srgb_alias_creators: HashMap::new(),
            srgb_alias_descriptions: HashSet::new(),
            legacy_color_ids: LegacyColorIdentities::default(),
            traced_colors: HashMap::new(),
            sync: sync::SourceSync::default(),
            shared_clock: false,
            closed: false,
            presentation: presentation::Clock::default(),
            frames: presentation::Frames::default(),
            xdg_surfaces: HashMap::new(),
            rates: HashMap::new(),
        }
    }

    fn request(
        &mut self,
        bytes: &mut [u8],
        pending_fds: &mut VecDeque<OwnedFd>,
    ) -> Result<Request, String> {
        let header = decode_wayland_header(bytes)?;
        let id = header.object_id;
        let interface = self
            .objects
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown Wayland request object {id}"))?;
        let (name, specs) = find_generated_request_by_opcode(&interface, header.opcode)
            .map(|(_, spec)| (spec.name, spec.args))
            .or_else(|| shm_request(&interface, header.opcode))
            .ok_or_else(|| format!("unsupported Wayland request {interface}.{}", header.opcode))?;
        let args = decode_wayland_args(bytes, header.size, specs)?;
        for arg in &args {
            if let Arg::NewId(object) = arg
                && (*object == 0 || *object >= 0xff00_0000 || self.objects.contains_key(object))
            {
                return Err(format!("invalid or live Wayland new_id {object}"));
            }
        }
        let from_app = self.end == End::Application;
        let mut action = Request {
            forward: true,
            before: Vec::new(),
            fds: Vec::new(),
            commit: None,
            destroy_surface: None,
            local_delete: None,
            probe_clock: false,
            frame_ready: Vec::new(),
        };
        let mut fds = if from_app {
            take_fds(pending_fds, fd_count(specs))?
        } else {
            Vec::new()
        };
        let version = self.versions.get(&id).copied().unwrap_or(1);
        let mut promoted_shm = None;

        if from_app
            && (interface.starts_with("wp_color_")
                || interface.starts_with("wp_image_description")
                || matches!(
                    (interface.as_str(), name),
                    ("wl_surface", "commit" | "destroy")
                ))
            && let Some(request_id) = find_generated_request_id_by_opcode(&interface, header.opcode)
        {
            let generated = args
                .iter()
                .map(GeneratedDecodedArg::from_decoded)
                .collect::<Vec<_>>();
            if let Some(request) = decode_generated_hook_request_by_id(request_id, &generated) {
                self.colors.request(id, &request);
            }
        }

        match (interface.as_str(), name, args.as_slice()) {
            ("xdg_wm_base", "get_xdg_surface", [Arg::NewId(xdg), Arg::Object(Some(surface))])
                if from_app =>
            {
                self.xdg_surfaces.insert(*xdg, *surface);
            }
            ("xdg_surface", "get_toplevel", _) if from_app => {
                if let Some(&surface) = self.xdg_surfaces.get(&id) {
                    self.frames.toplevel(surface);
                }
            }
            ("xdg_surface", "destroy", _) if from_app => {
                self.xdg_surfaces.remove(&id);
            }
            ("wl_surface", "frame", [Arg::NewId(callback)]) if from_app => {
                self.frames.request(id, *callback);
            }
            ("wp_presentation", "feedback", _) => {
                action.probe_clock = from_app && !self.shared_clock;
            }
            (
                "wl_registry",
                "bind",
                [
                    Arg::Uint(name),
                    Arg::String(Some(interface)),
                    Arg::Uint(version),
                    Arg::NewId(object),
                ],
            ) => {
                if !self.globals.get(name).is_some_and(|(advertised, max)| {
                    advertised == interface && *version > 0 && version <= max
                }) {
                    return Err(format!(
                        "Wayland global {interface} {name} version {version} was not advertised"
                    ));
                }
                self.objects.insert(*object, interface.clone());
                self.versions.insert(*object, *version);
                if from_app && interface == "wp_color_manager_v1" && *version == 1 {
                    self.legacy_transfers
                        .insert(*object, LegacyTransfers::default());
                }
                if from_app
                    && matches!(
                        interface.as_str(),
                        "wp_linux_drm_syncobj_manager_v1" | "wl_data_device_manager"
                    )
                {
                    action.forward = false;
                }
            }
            ("wl_data_device_manager" | "wl_data_device" | "wl_data_source", _, _) if from_app => {
                // GTK initializes its seat only when a data-device manager is
                // present. Keep these objects local and omit remote selection
                // offers. A drag without a remote target is cancelled.
                action.forward = false;
                if let ("start_drag", [Arg::Object(Some(source)), ..]) = (name, args.as_slice()) {
                    if self.objects.get(source).map(String::as_str) != Some("wl_data_source") {
                        return Err("invalid Wayland drag source".into());
                    }
                    action.before.push(WaylandWireMessage {
                        bytes: super::encode_generated_event(
                            *source,
                            &GeneratedEvent::WlDataSourceCancelled,
                        )?,
                        fds: Vec::new(),
                    });
                }
                if matches!(name, "destroy" | "release") {
                    self.objects.remove(&id);
                    self.versions.remove(&id);
                    action.local_delete = Some(id);
                }
            }
            (
                "wp_linux_drm_syncobj_manager_v1"
                | "wp_linux_drm_syncobj_surface_v1"
                | "wp_linux_drm_syncobj_timeline_v1",
                _,
                _,
            ) if from_app => {
                self.sync
                    .request((&interface, name), id, &args, &mut fds, self.gpu)?;
                action.forward = false;
                if name == "destroy" {
                    self.objects.remove(&id);
                    self.versions.remove(&id);
                    action.local_delete = Some(id);
                }
            }
            (
                "wp_color_management_surface_v1",
                "set_image_description" | "unset_image_description",
                _,
            ) if !from_app => {
                // The replacement buffer is already converted to the default
                // SDR surface color space.
                action.forward = false;
            }
            ("wp_color_manager_v1", "create_parametric_creator", [Arg::NewId(creator)])
                if self
                    .legacy_transfers
                    .get(&id)
                    .is_some_and(LegacyTransfers::needs_srgb_alias) =>
            {
                self.srgb_alias_creators.insert(*creator, false);
            }
            ("wp_image_description_creator_params_v1", "set_tf_named", [Arg::Uint(TF_SRGB)])
                if let Some(aliased) = self.srgb_alias_creators.get_mut(&id) =>
            {
                // The source tracker above keeps sRGB for conversion. The compositor
                // validates this description, but never attaches it to the decoded buffer.
                bytes[8..12].copy_from_slice(&TF_GAMMA22.to_ne_bytes());
                *aliased = true;
            }
            ("wp_image_description_creator_params_v1", "create", [Arg::NewId(description)]) => {
                if self.srgb_alias_creators.remove(&id) == Some(true) {
                    self.srgb_alias_descriptions.insert(*description);
                }
            }
            ("wl_shm", "create_pool", [Arg::NewId(pool), Arg::Fd, Arg::Int(size)]) => {
                let size = u64::try_from(*size).map_err(io_error)?;
                if size == 0 || size > MAX_POOL {
                    return Err("wl_shm pool size is out of range".into());
                }
                let (fd, size) = if from_app {
                    (
                        File::from(fds.pop().ok_or("wl_shm pool descriptor missing")?),
                        size,
                    )
                } else {
                    let fd = memfd(4096)?;
                    action.fds.push(fd.try_clone().map_err(io_error)?.into());
                    bytes[12..16].copy_from_slice(&4096i32.to_ne_bytes());
                    (fd, 4096)
                };
                self.pools.insert(
                    *pool,
                    Pool {
                        fd,
                        size,
                        next_offset: 0,
                    },
                );
            }
            ("wl_shm_pool", "resize", [Arg::Int(size)]) => {
                if from_app {
                    let pool = self.pools.get_mut(&id).ok_or("unknown wl_shm pool")?;
                    let size = u64::try_from(*size).map_err(io_error)?;
                    if size < pool.size || size > MAX_POOL {
                        return Err("invalid wl_shm pool resize".into());
                    }
                    pool.size = size;
                } else {
                    action.forward = false;
                }
            }
            (
                "wl_shm_pool",
                "create_buffer",
                [
                    Arg::NewId(buffer),
                    Arg::Int(offset),
                    Arg::Int(width),
                    Arg::Int(height),
                    Arg::Int(stride),
                    Arg::Uint(format),
                ],
            ) => {
                let (width, height) = extent(*width, *height)?;
                let (offset, stride) = (
                    u64::try_from(*offset).map_err(io_error)?,
                    u32::try_from(*stride).map_err(io_error)?,
                );
                let format = match *format {
                    0 => AR24,
                    1 => XR24,
                    _ => return Err("unsupported SSH wl_shm format".into()),
                };
                if stride < width * 4 {
                    return Err("invalid wl_shm stride".into());
                }
                let pool = self.pools.get_mut(&id).ok_or("unknown wl_shm pool")?;
                let backing = if from_app {
                    let bound =
                        offset + u64::from(stride) * u64::from(height - 1) + u64::from(width * 4);
                    if bound > pool.size {
                        return Err("wl_shm buffer exceeds its pool".into());
                    }
                    Backing::Shm(Shm {
                        file: pool.fd.try_clone().map_err(io_error)?,
                        offset,
                        stride,
                    })
                } else if self.promote_shm
                    && self
                        .globals
                        .values()
                        .any(|(name, version)| name == "zwp_linux_dmabuf_v1" && *version >= 2)
                    && let Some(gpu) = self.gpu
                    && let Ok(allocation) = display::DisplayBuffer::new(gpu, width, height, format)
                {
                    action.forward = false;
                    promoted_shm = Some(*buffer);
                    Backing::DisplayDma(allocation, 0)
                } else {
                    let offset = pool.next_offset;
                    let stride = width * 4;
                    pool.next_offset += u64::from(stride) * u64::from(height);
                    if pool.next_offset > MAX_POOL {
                        return Err("reconstructed wl_shm pool is full".into());
                    }
                    if pool.next_offset > pool.size {
                        pool.size = pool.next_offset.next_multiple_of(4096);
                        pool.fd.set_len(pool.size).map_err(io_error)?;
                        action
                            .before
                            .push(wire(id, 2, &[pool.size as u32], Vec::new()));
                    }
                    bytes[12..16].copy_from_slice(&(offset as u32).to_ne_bytes());
                    bytes[24..28].copy_from_slice(&stride.to_ne_bytes());
                    Backing::Shm(Shm {
                        file: pool.fd.try_clone().map_err(io_error)?,
                        offset,
                        stride,
                    })
                };
                self.buffers
                    .insert(*buffer, Buffer::new(width, height, format, backing));
            }
            ("wl_shm_pool", "destroy", _) => {
                self.pools.remove(&id);
            }
            ("wl_buffer", "destroy", _) => {
                self.buffers.remove(&id);
                self.pending.retain(|_, attach| attach.id != id);
            }
            ("wl_surface", "destroy", _) => {
                action.destroy_surface = Some(id);
                self.frames.destroy(id);
                self.rates.remove(&id);
                self.pending.remove(&id);
                self.sync.destroy_surface(id);
                self.traced_colors.remove(&id);
            }
            ("wl_surface", "attach", [Arg::Object(buffer), Arg::Int(x), Arg::Int(y)]) => {
                if let Some(buffer) = buffer {
                    let storage = self
                        .buffers
                        .get(buffer)
                        .cloned()
                        .ok_or("unknown attached wl_buffer")?;
                    self.pending.insert(
                        id,
                        Attachment {
                            id: *buffer,
                            buffer: storage,
                            x: *x,
                            y: *y,
                        },
                    );
                    action.forward = from_app;
                } else {
                    self.pending.remove(&id);
                }
            }
            ("wl_surface", "commit", _) => {
                let attach = self.pending.remove(&id);
                if from_app {
                    action.frame_ready = self.frames.commit(id, attach.is_some(), Instant::now());
                }
                let sync = self
                    .sync
                    .commit(id, attach.as_ref().map(|a| a.buffer.as_ref()))?;
                if let Some(attach) = attach {
                    let color = self.colors.get(id).cloned();
                    let changed_color = from_app && self.traced_colors.get(&id) != Some(&color);
                    if changed_color {
                        self.traced_colors.insert(id, color.clone());
                    }
                    action.commit = Some(Commit {
                        surface: id,
                        surface_version: version,
                        attach,
                        color,
                        changed_color,
                        sync,
                    });
                }
            }
            ("zwp_linux_dmabuf_v1" | "zwp_linux_buffer_params_v1", _, _) => {
                let mut dma = std::mem::take(&mut self.dma);
                dma.request(
                    self,
                    (&interface, name),
                    &args,
                    bytes,
                    &mut fds,
                    &mut action,
                )?;
                self.dma = dma;
            }
            _ => {}
        }
        if !fds.is_empty() {
            return Err(format!(
                "unsupported Wayland descriptor in {interface}.{name}"
            ));
        }

        for (spec, arg) in specs.iter().zip(&args) {
            if let (Some(interface), Arg::NewId(object)) = (spec.interface, arg) {
                self.objects.insert(*object, interface.into());
                self.versions.insert(*object, version);
            }
        }
        if !from_app {
            for request in &mut action.before {
                rewrite_wire_object_ids(&mut request.bytes, &[], false, |id, _| {
                    self.host.wire_id(id)
                })?;
            }
            if action.forward {
                rewrite_wire_object_ids(bytes, specs, false, |id, new| {
                    if new {
                        self.host.register(id)
                    } else {
                        self.host.wire_id(id)
                    }
                })?;
            }
            if let Some(id) = promoted_shm {
                self.host.register_display_dma(
                    id,
                    &self.buffers[&id],
                    &self.globals,
                    &mut action.before,
                )?;
            }
            if interface == "wl_display" && name == "get_registry" {
                let [Arg::NewId(registry)] = args.as_slice() else {
                    return Err("invalid get_registry".into());
                };
                self.host.set_registry(self.host.wire_id(*registry)?);
            }
            if interface == "wl_buffer" && name == "destroy" {
                self.host.destroy_buffer(id, &mut action.before);
            }
        }
        Ok(action)
    }

    fn display_commit(
        &mut self,
        commit: Commit,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<Arc<Buffer>, String> {
        let surface = self.host.wire_id(commit.surface)?;
        let slot = self
            .host
            .commit(&commit.attach, &self.globals, self.gpu, before)?;
        before.push(wire(
            surface,
            1,
            &[slot.wire_id, commit.attach.x as u32, commit.attach.y as u32],
            Vec::new(),
        ));
        before.push(full_damage(surface, commit.surface_version, &slot.storage));
        Ok(Arc::clone(&slot.storage))
    }

    fn event(
        &mut self,
        bytes: &mut [u8],
        pending_fds: &mut VecDeque<OwnedFd>,
    ) -> Result<EventAction, String> {
        let mut header = decode_wayland_header(bytes)?;
        if self.end == End::Compositor {
            if !self
                .host
                .route_event(bytes, header.object_id, header.opcode, &self.objects)?
            {
                return Ok((None, Vec::new(), false, Vec::new()));
            }
            header = decode_wayland_header(bytes)?;
        }
        let interface = self
            .objects
            .get(&header.object_id)
            .ok_or_else(|| format!("unknown Wayland event object {}", header.object_id))?;
        if interface == "wl_shm" && header.opcode == 0 && header.size == 12 {
            return Ok((
                None,
                Vec::new(),
                matches!(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 0 | 1),
                Vec::new(),
            ));
        }
        let (_, spec) = find_generated_event_by_opcode(interface, header.opcode)
            .ok_or_else(|| format!("unsupported Wayland event {interface}.{}", header.opcode))?;
        let mut fds = if self.end == End::Compositor {
            take_fds(pending_fds, fd_count(spec.args))?
        } else {
            Vec::new()
        };
        let event = decode_wayland_event(&self.objects, bytes)?;
        let mut forward = true;
        let mut before = Vec::new();
        match &event.generated_event {
            GeneratedEvent::WlRegistryGlobal {
                name,
                interface: Some(interface),
                version,
            } => {
                let policy = crate::wayland_policy::global(interface);
                forward = policy.exposure != crate::wayland_policy::Exposure::Deny;
                if forward {
                    let max = match interface.as_str() {
                        "wl_shm" => 1,
                        // Version 7 adds per-commit release callbacks. The
                        // bridge currently translates wl_buffer.release.
                        "wl_compositor" => 6,
                        "zwp_linux_dmabuf_v1" => 5,
                        "wp_presentation" => policy
                            .max_version
                            .min(super::generated_interface_version(interface)),
                        _ => policy.max_version,
                    };
                    let version = (*version).min(max);
                    self.globals.insert(*name, (interface.clone(), version));
                    let version_pos = bytes.len() - 4;
                    bytes[version_pos..].copy_from_slice(&version.to_ne_bytes());
                }
            }
            GeneratedEvent::WlRegistryGlobalRemove { name } => {
                forward = self.globals.remove(name).is_some();
            }
            GeneratedEvent::WpColorManagerV1SupportedFeature { feature: 0 } => {
                // ICC profiles require an additional descriptor path and a
                // color transform the bridge does not implement.
                forward = false;
            }
            GeneratedEvent::WpColorManagerV1SupportedTfNamed { tf } => {
                if let Some(transfers) = self.legacy_transfers.get_mut(&header.object_id) {
                    transfers.gamma22 |= *tf == TF_GAMMA22;
                    transfers.srgb |= *tf == TF_SRGB;
                }
            }
            GeneratedEvent::WpColorManagerV1Done => {
                if self
                    .legacy_transfers
                    .get(&header.object_id)
                    .is_some_and(LegacyTransfers::needs_srgb_alias)
                {
                    before.push(WaylandWireMessage {
                        bytes: super::encode_generated_event(
                            header.object_id,
                            &GeneratedEvent::WpColorManagerV1SupportedTfNamed { tf: TF_SRGB },
                        )?,
                        fds: Vec::new(),
                    });
                }
            }
            GeneratedEvent::WpImageDescriptionV1Ready { identity }
            | GeneratedEvent::WpColorManagementSurfaceFeedbackV1PreferredChanged { identity }
                if self.end == End::Application =>
            {
                // A compositor may deduplicate the gamma 2.2 placeholder with another
                // description. The client must still distinguish the real transfer curves.
                let aliased = self.srgb_alias_descriptions.contains(&header.object_id);
                let local = self
                    .legacy_color_ids
                    .record(header.object_id, *identity, aliased)?;
                bytes[8..12].copy_from_slice(&local.to_ne_bytes());
            }
            GeneratedEvent::WpPresentationClockId { clk_id } if self.end == End::Compositor => {
                self.presentation.set_source(*clk_id as libc::clockid_t)?;
                if !self.shared_clock {
                    bytes[8..12].copy_from_slice(&(libc::CLOCK_MONOTONIC as u32).to_ne_bytes());
                }
            }
            GeneratedEvent::WpPresentationFeedbackPresented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                flags,
                ..
            } => {
                let seconds = u64::from(*tv_sec_hi) << 32 | u64::from(*tv_sec_lo);
                if self.end == End::Compositor && ssh_video::timing_enabled() {
                    eprintln!("SSH presented seconds={seconds} nanos={tv_nsec}");
                }
                self.presentation.presented(
                    self.end,
                    self.shared_clock,
                    bytes,
                    seconds,
                    *tv_nsec,
                    *flags,
                )?;
            }
            GeneratedEvent::WlCallbackDone { .. } if self.end == End::Application => {
                if let Some((sent_early, ready)) =
                    self.frames.finish(header.object_id, Some(Instant::now()))
                {
                    forward = !sent_early;
                    if (forward && !self.shared_clock) || !ready.is_empty() {
                        let now = (presentation::monotonic()? / 1_000_000) as u32;
                        if forward {
                            bytes[8..12].copy_from_slice(&now.to_ne_bytes());
                        }
                        before.extend(
                            ready
                                .into_iter()
                                .map(|callback| wire(callback, 0, &[now], Vec::new())),
                        );
                    }
                }
            }
            GeneratedEvent::WlDisplayDeleteId { id } => {
                self.frames.finish(*id, None);
                self.objects.remove(id);
                self.versions.remove(id);
                self.legacy_transfers.remove(id);
                self.srgb_alias_creators.remove(id);
                self.srgb_alias_descriptions.remove(id);
                self.legacy_color_ids.remove(*id);
                self.dma.delete_id(*id);
            }
            _ => {
                let mut dma = std::mem::take(&mut self.dma);
                forward = dma.event(self, &event, &mut fds, &mut before)?;
                self.dma = dma;
            }
        }
        super::apply_object_interfaces_from_event(&mut self.objects, &event);
        Ok((Some(event), fds, forward, before))
    }
}

type EventAction = (
    Option<super::DecodedWaylandEvent>,
    Vec<OwnedFd>,
    bool,
    Vec<WaylandWireMessage>,
);

const fn arg(kind: GeneratedArgKind, interface: Option<&'static str>) -> GeneratedArgSpec {
    GeneratedArgSpec {
        name: "",
        kind,
        interface,
        allow_null: false,
    }
}

fn shm_request(
    interface: &str,
    opcode: u16,
) -> Option<(&'static str, &'static [GeneratedArgSpec])> {
    use GeneratedArgKind::*;
    const CREATE_POOL: &[GeneratedArgSpec] = &[
        arg(NewId, Some("wl_shm_pool")),
        arg(Fd, None),
        arg(Int, None),
    ];
    const CREATE_BUFFER: &[GeneratedArgSpec] = &[
        arg(NewId, Some("wl_buffer")),
        arg(Int, None),
        arg(Int, None),
        arg(Int, None),
        arg(Int, None),
        arg(Uint, None),
    ];
    const RESIZE: &[GeneratedArgSpec] = &[arg(Int, None)];
    match (interface, opcode) {
        ("wl_shm", 0) => Some(("create_pool", CREATE_POOL)),
        ("wl_shm_pool", 0) => Some(("create_buffer", CREATE_BUFFER)),
        ("wl_shm_pool", 1) => Some(("destroy", &[])),
        ("wl_shm_pool", 2) => Some(("resize", RESIZE)),
        _ => None,
    }
}

fn extent(width: i32, height: i32) -> Result<(u32, u32), String> {
    if !(1..=4096).contains(&width)
        || !(1..=4096).contains(&height)
        || i64::from(width) * i64::from(height) > 8_388_608
    {
        return Err("SSH Wayland buffer dimensions are out of range".into());
    }
    Ok((width as u32, height as u32))
}

struct Shm {
    file: File,
    offset: u64,
    stride: u32,
}

enum Backing {
    Shm(Shm),
    SourceDma(Vec<display::Plane>),
    DisplayDma(display::DisplayBuffer, u32),
}

struct Buffer {
    width: u32,
    height: u32,
    format: u32,
    backing: Backing,
}

impl Buffer {
    fn new(width: u32, height: u32, format: u32, backing: Backing) -> Arc<Self> {
        Arc::new(Self {
            width,
            height,
            format,
            backing,
        })
    }

    fn encode(
        &self,
        settings: &VideoSettings,
        gpu: &str,
        color: Option<&ColorDescription>,
        sync: Option<sync::CommitSync>,
        encoder: &mut ssh_video::BridgeEncoder,
        release_implicit: impl FnOnce() -> Result<(), String>,
    ) -> Result<Payload, String> {
        let plane;
        let mut producer_gpu = None;
        let source = match &self.backing {
            Backing::Shm(shm) => BridgePixels::Shm {
                fd: shm.file.as_fd(),
                stride: shm.stride,
                offset: shm.offset,
            },
            Backing::SourceDma(source) => {
                let [source] = source.as_slice() else {
                    return Err("expected a single-plane RGB DMA-BUF".into());
                };
                producer_gpu = source.gpu.as_deref();
                plane = BridgePlane {
                    fd: source.fd.as_raw_fd(),
                    stride: source.stride,
                    offset: source.offset,
                    modifier: source.modifier,
                };
                BridgePixels::Dmabuf(&plane)
            }
            Backing::DisplayDma(..) => return Err("invalid SSH Wayland source buffer".into()),
        };
        Ok(Payload::Picture(ssh_video::encode_bridge(
            &settings.request,
            gpu,
            BridgeSource {
                extent: (self.width, self.height),
                format: self.format,
                pixels: source,
                producer_gpu,
                color,
                implicit_sync: sync.is_none(),
            },
            settings.preserve_alpha,
            settings.rate_control,
            encoder,
            || match sync {
                Some(sync) => sync.release(),
                None => release_implicit(),
            },
        )?))
    }

    fn publish(
        &self,
        picture: &BridgePicture,
        decoder: &mut ssh_video::BridgeDecoder,
        slot_ms: f64,
    ) -> Result<(), String> {
        if picture.has_alpha() && !ssh_video::has_alpha(self.format) {
            return Err("unexpected alpha picture for an opaque Wayland buffer".into());
        }
        let decode_start = ssh_video::timing_start();
        let extent = (self.width, self.height);
        let fourcc = display_format(self.format);
        match &self.backing {
            Backing::Shm(shm) => decoder.decode(
                picture,
                extent,
                fourcc,
                DisplayPixels::Shm(&NativeShm {
                    fd: shm.file.as_raw_fd(),
                    stride: shm.stride,
                    offset: shm.offset,
                }),
            ),
            Backing::DisplayDma(buffer, _) => decoder.decode(
                picture,
                extent,
                fourcc,
                DisplayPixels::Dmabuf(&buffer.plane()),
            ),
            Backing::SourceDma(_) => Err("invalid SSH Wayland destination buffer".into()),
        }?;
        if decode_start.is_some() {
            eprintln!(
                "SSH display {}x{} slot_ms={slot_ms:.3} decode_ms={:.3}",
                self.width,
                self.height,
                ssh_video::timing_ms(decode_start)
            );
        }
        Ok(())
    }
}

fn full_damage(surface: u32, version: u32, buffer: &Buffer) -> WaylandWireMessage {
    if version >= 4 {
        wire(surface, 9, &[0, 0, buffer.width, buffer.height], Vec::new())
    } else {
        wire(
            surface,
            2,
            &[
                (-0x4000_0000i32) as u32,
                (-0x4000_0000i32) as u32,
                i32::MAX as u32,
                i32::MAX as u32,
            ],
            Vec::new(),
        )
    }
}

fn wire(id: u32, opcode: u16, words: &[u32], fds: Vec<OwnedFd>) -> WaylandWireMessage {
    let mut bytes = Vec::with_capacity(8 + words.len() * 4);
    bytes.extend_from_slice(&id.to_ne_bytes());
    bytes.extend_from_slice(
        &(((8 + words.len() as u32 * 4) << 16) | u32::from(opcode)).to_ne_bytes(),
    );
    for word in words {
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    WaylandWireMessage { bytes, fds }
}

fn write_native(native: &UnixStream, bytes: &[u8], fds: &[OwnedFd]) -> Result<(), String> {
    send_wayland_wire_message_to_fd(native.as_raw_fd(), bytes, fds)
}

fn memfd(size: usize) -> Result<File, String> {
    let fd = unsafe {
        libc::memfd_create(
            c"wayland-mcp".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io_error(io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64).map_err(io_error)?;
    if unsafe {
        libc::fcntl(
            fd,
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(io_error(io::Error::last_os_error()));
    }
    Ok(file)
}

fn io_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[link(name = "zstd")]
unsafe extern "C" {
    fn ZSTD_compressBound(src_size: usize) -> usize;
    fn ZSTD_compress(
        dst: *mut u8,
        dst_capacity: usize,
        src: *const u8,
        src_size: usize,
        level: i32,
    ) -> usize;
    fn ZSTD_decompress(
        dst: *mut u8,
        dst_capacity: usize,
        src: *const u8,
        compressed_size: usize,
    ) -> usize;
    fn ZSTD_isError(code: usize) -> u32;
}

fn compress_keymap(fd: OwnedFd, size: usize) -> Result<Vec<u8>, String> {
    if size > MAX_KEYMAP {
        return Err("Wayland keymap exceeds bridge limit".into());
    }
    let mut raw = vec![0; size];
    File::from(fd)
        .read_exact_at(&mut raw, 0)
        .map_err(io_error)?;
    let mut compressed = vec![0; unsafe { ZSTD_compressBound(size) }];
    let len = unsafe {
        ZSTD_compress(
            compressed.as_mut_ptr(),
            compressed.len(),
            raw.as_ptr(),
            raw.len(),
            1,
        )
    };
    if unsafe { ZSTD_isError(len) } != 0 || len > MAX_KEYMAP {
        return Err("Wayland keymap compression failed".into());
    }
    compressed.truncate(len);
    Ok(compressed)
}

fn decompress_keymap(compressed: &[u8], size: usize) -> Result<OwnedFd, String> {
    if size > MAX_KEYMAP {
        return Err("Wayland keymap exceeds bridge limit".into());
    }
    let mut raw = vec![0; size];
    let len = unsafe {
        ZSTD_decompress(
            raw.as_mut_ptr(),
            size,
            compressed.as_ptr(),
            compressed.len(),
        )
    };
    if unsafe { ZSTD_isError(len) } != 0 || len != size {
        return Err("Wayland keymap decompression failed".into());
    }
    let file = memfd(size)?;
    file.write_all_at(&raw, 0).map_err(io_error)?;
    Ok(file.into())
}

#[cfg(test)]
mod tests {
    use super::super::encode_generated_event;
    use super::*;
    use std::time::Duration;

    #[test]
    fn legacy_srgb_is_negotiated_and_kept_for_gpu_conversion() -> Result<(), String> {
        for (version, advertised, aliased) in [
            (1, &[TF_GAMMA22][..], true),
            (1, &[TF_GAMMA22, TF_SRGB][..], false),
            (2, &[TF_GAMMA22][..], false),
            (1, &[][..], false),
        ] {
            let mut state = State::new(End::Application, None);
            let mut fds = VecDeque::new();
            state.request(&mut wire(1, 1, &[2], Vec::new()).bytes, &mut fds)?;
            let mut global = encode_generated_event(
                2,
                &GeneratedEvent::WlRegistryGlobal {
                    name: 10,
                    interface: Some("wp_color_manager_v1".to_owned()),
                    version: 3,
                },
            )?;
            state.event(&mut global, &mut fds)?;
            state.request(
                &mut host::bind(2, 10, "wp_color_manager_v1", version, 3).bytes,
                &mut fds,
            )?;
            for &tf in advertised {
                let mut event = encode_generated_event(
                    3,
                    &GeneratedEvent::WpColorManagerV1SupportedTfNamed { tf },
                )?;
                assert!(state.event(&mut event, &mut fds)?.2);
            }
            let mut done = encode_generated_event(3, &GeneratedEvent::WpColorManagerV1Done)?;
            let (_, _, forward, before) = state.event(&mut done, &mut fds)?;
            assert!(forward);
            assert_eq!(before.len(), usize::from(aliased));
            if aliased {
                let event = decode_wayland_event(&state.objects, &before[0].bytes)?;
                assert!(matches!(
                    event.generated_event,
                    GeneratedEvent::WpColorManagerV1SupportedTfNamed { tf: TF_SRGB }
                ));
            }
            if aliased || advertised.contains(&TF_SRGB) {
                state.request(&mut wire(3, 5, &[4], Vec::new()).bytes, &mut fds)?;
                let mut transfer = wire(4, 1, &[TF_SRGB], Vec::new());
                assert!(state.request(&mut transfer.bytes, &mut fds)?.forward);
                assert_eq!(
                    transfer.bytes[8..12],
                    if aliased { TF_GAMMA22 } else { TF_SRGB }.to_ne_bytes()
                );
                state.request(&mut wire(4, 3, &[1], Vec::new()).bytes, &mut fds)?;
                state.request(&mut wire(4, 0, &[5], Vec::new()).bytes, &mut fds)?;
                state.objects.insert(6, "wl_surface".to_owned());
                state.request(&mut wire(3, 2, &[7, 6], Vec::new()).bytes, &mut fds)?;
                state.request(&mut wire(7, 1, &[5, 0], Vec::new()).bytes, &mut fds)?;
                assert_eq!(
                    state.colors.next(6).unwrap().gpu_profile(2)?.transfer,
                    TF_SRGB
                );
                if aliased {
                    state.request(&mut wire(3, 3, &[8, 6], Vec::new()).bytes, &mut fds)?;
                    state.request(&mut wire(8, 1, &[9], Vec::new()).bytes, &mut fds)?;
                    let ready = GeneratedEvent::WpImageDescriptionV1Ready { identity: 101 };
                    let mut source = encode_generated_event(5, &ready)?;
                    let mut preferred = encode_generated_event(9, &ready)?;
                    state.event(&mut source, &mut fds)?;
                    state.event(&mut preferred, &mut fds)?;
                    assert_ne!(source[8..12], preferred[8..12]);
                    let mut changed = encode_generated_event(
                        8,
                        &GeneratedEvent::WpColorManagementSurfaceFeedbackV1PreferredChanged {
                            identity: 101,
                        },
                    )?;
                    state.event(&mut changed, &mut fds)?;
                    assert_eq!(changed[8..12], preferred[8..12]);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn gtk_seat_can_bind_a_local_data_device_and_forward_the_keyboard() -> Result<(), String> {
        let mut state = State::new(End::Application, None);
        let mut fds = VecDeque::new();
        state.request(&mut wire(1, 1, &[2], Vec::new()).bytes, &mut fds)?;
        for (name, interface, id, remote) in [
            (4, "wl_seat", 3, true),
            (5, "wl_data_device_manager", 4, false),
        ] {
            let mut global = super::super::encode_generated_event(
                2,
                &GeneratedEvent::WlRegistryGlobal {
                    name,
                    interface: Some(interface.to_owned()),
                    version: 3,
                },
            )?;
            assert!(state.event(&mut global, &mut fds)?.2);
            let mut bind = host::bind(2, name, interface, 3, id);
            assert_eq!(state.request(&mut bind.bytes, &mut fds)?.forward, remote);
        }
        let data_device = state.request(&mut wire(4, 1, &[5, 3], Vec::new()).bytes, &mut fds)?;
        assert!(!data_device.forward && data_device.before.is_empty());
        let keyboard = state.request(&mut wire(3, 1, &[6], Vec::new()).bytes, &mut fds)?;
        assert!(keyboard.forward);
        let release = state.request(&mut wire(5, 2, &[], Vec::new()).bytes, &mut fds)?;
        assert_eq!(release.local_delete, Some(5));
        assert!(!release.forward);
        Ok(())
    }

    #[test]
    fn presentation_feedback_reaches_the_application_in_its_clock_domain() -> Result<(), String> {
        for shared_clock in [true, false] {
            let (application, app_native) = UnixStream::pair().map_err(io_error)?;
            let (compositor, host_native) = UnixStream::pair().map_err(io_error)?;
            let (app_tunnel, host_tunnel) = UnixStream::pair().map_err(io_error)?;
            for stream in [&application, &compositor] {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .map_err(io_error)?;
            }
            let endpoint = |end| -> Result<Endpoint, String> {
                Ok(Endpoint {
                    end,
                    promote_shm: false,
                    warmup: None,
                    gpu: None,
                    video: Arc::new(std::sync::RwLock::new(VideoSettings {
                        request: serde_json::from_value(serde_json::json!({"windowId":""}))
                            .map_err(io_error)?,
                        source_color: None,
                        preserve_alpha: true,
                        rate_control: Default::default(),
                    })),
                })
            };
            let app = endpoint(End::Application)?;
            let host = endpoint(End::Compositor)?;
            let app_worker = std::thread::spawn(move || {
                relay_wayland(&app_native, &app_tunnel, &app, shared_clock)
            });
            let host_worker = std::thread::spawn(move || {
                relay_wayland(&host_native, &host_tunnel, &host, shared_clock)
            });
            let result = exercise_presentation(&application, &compositor, shared_clock);
            drop(application);
            drop(compositor);
            let app_result = app_worker
                .join()
                .map_err(|_| "application bridge panicked")?;
            let host_result = host_worker
                .join()
                .map_err(|_| "compositor bridge panicked")?;
            app_result?;
            host_result?;
            result?;
        }
        Ok(())
    }

    fn exercise_presentation(
        application: &UnixStream,
        compositor: &UnixStream,
        shared_clock: bool,
    ) -> Result<(), String> {
        let request = |message: WaylandWireMessage| -> Result<Vec<u8>, String> {
            write_native(application, &message.bytes, &[])?;
            Ok(read_wayland_wire_message_blocking(compositor, 0)?.bytes)
        };
        let event = |upstream, event: GeneratedEvent| -> Result<Vec<u8>, String> {
            write_native(compositor, &encode_generated_event(upstream, &event)?, &[])?;
            Ok(read_wayland_wire_message_blocking(application, 0)?.bytes)
        };
        let word = |bytes: &[u8], index| u32::from_ne_bytes(bytes.as_chunks::<4>().0[index]);

        let registry = request(wire(1, 1, &[70], Vec::new()))?;
        let mut bound = Vec::new();
        let version = super::super::generated_interface_version("wp_presentation").min(3);
        for (name, interface, version, id) in [
            (13, "wl_compositor", 1, 71),
            (15, "wp_presentation", version, 72),
            (17, "wl_output", 1, 73),
        ] {
            let global = GeneratedEvent::WlRegistryGlobal {
                name,
                interface: Some(interface.into()),
                version,
            };
            assert_eq!(
                event(word(&registry, 2), global.clone())?,
                encode_generated_event(70, &global)?
            );
            let bind = request(host::bind(70, name, interface, version, id))?;
            bound.push(word(&bind, bind.len() / 4 - 1));
        }
        let clock = event(
            bound[1],
            GeneratedEvent::WpPresentationClockId {
                clk_id: libc::CLOCK_REALTIME as u32,
            },
        )?;
        let clock_id = if shared_clock {
            libc::CLOCK_REALTIME
        } else {
            libc::CLOCK_MONOTONIC
        };
        assert_eq!(word(&clock, 2), clock_id as u32);
        let surface = request(wire(71, 0, &[74], Vec::new()))?;
        let shown = request(wire(72, 1, &[74, 75], Vec::new()))?;
        assert_eq!(word(&shown, 2), word(&surface, 2));
        let duplicate = request(wire(72, 1, &[74, 76], Vec::new()))?;
        request(wire(74, 6, &[], Vec::new()))?;
        let hidden = request(wire(72, 1, &[74, 77], Vec::new()))?;
        request(wire(74, 6, &[], Vec::new()))?;
        request(wire(72, 0, &[], Vec::new()))?;

        let mut realtime = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut realtime) },
            0
        );
        let now = presentation::monotonic()?;
        let shown_id = word(&shown, 3);
        let output = event(
            shown_id,
            GeneratedEvent::WpPresentationFeedbackSyncOutput {
                output: Some(bound[2]),
            },
        )?;
        assert_eq!(output, wire(75, 0, &[73], Vec::new()).bytes);
        let refresh_flag = if version == 3 { 0x10 } else { 0 };
        let seconds = realtime.tv_sec as u64;
        let presented = GeneratedEvent::WpPresentationFeedbackPresented {
            tv_sec_hi: (seconds >> 32) as u32,
            tv_sec_lo: seconds as u32,
            tv_nsec: realtime.tv_nsec as u32,
            refresh: 16_666_667,
            seq_hi: 1,
            seq_lo: 9,
            flags: 0xf | refresh_flag,
        };
        let feedback = event(shown_id, presented.clone())?;
        let duplicate_id = word(&duplicate, 3);
        let output = event(
            duplicate_id,
            GeneratedEvent::WpPresentationFeedbackSyncOutput {
                output: Some(bound[2]),
            },
        )?;
        assert_eq!(output, wire(76, 0, &[73], Vec::new()).bytes);
        let repeated = event(duplicate_id, presented.clone())?;
        assert_eq!(word(&feedback, 0), 75);
        assert_eq!(word(&repeated, 0), 76);
        assert_eq!(&feedback[4..], &repeated[4..]);
        assert_eq!(
            (word(&feedback, 5), word(&feedback, 6), word(&feedback, 7)),
            (16_666_667, 1, 9)
        );
        if shared_clock {
            assert_eq!(
                &feedback[8..20],
                &encode_generated_event(75, &presented)?[8..20]
            );
            assert_eq!(word(&feedback, 8), 0x7 | refresh_flag);
        } else {
            let seconds = u64::from(word(&feedback, 2)) << 32 | u64::from(word(&feedback, 3));
            let mapped = seconds * 1_000_000_000 + u64::from(word(&feedback, 4));
            assert!(mapped.abs_diff(now) < 250_000_000);
            assert_eq!(word(&feedback, 8), 0x5 | refresh_flag);
        }
        let discarded = event(
            word(&hidden, 3),
            GeneratedEvent::WpPresentationFeedbackDiscarded,
        )?;
        assert_eq!(discarded, wire(77, 2, &[], Vec::new()).bytes);
        let deleted = event(1, GeneratedEvent::WlDisplayDeleteId { id: shown_id })?;
        assert_eq!(deleted, wire(1, 1, &[75], Vec::new()).bytes);
        request(wire(1, 0, &[75], Vec::new()))?;
        Ok(())
    }

    #[test]
    fn no_commit_reaches_the_compositor_without_a_picture() -> Result<(), String> {
        let (tunnel_worker, mut tunnel) = UnixStream::pair().map_err(io_error)?;
        let (native, compositor) = UnixStream::pair().map_err(io_error)?;
        for stream in [&tunnel, &compositor] {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(io_error)?;
        }
        let endpoint = Endpoint {
            warmup: None,
            end: End::Compositor,
            promote_shm: false,
            gpu: None,
            video: Arc::new(std::sync::RwLock::new(VideoSettings {
                request: serde_json::from_value(serde_json::json!({"windowId":""}))
                    .map_err(io_error)?,
                source_color: None,
                preserve_alpha: true,
                rate_control: Default::default(),
            })),
        };
        let worker = std::thread::spawn(move || relay(&native, &tunnel_worker, &endpoint));
        let mut version = [0; 14];
        tunnel.read_exact(&mut version).map_err(io_error)?;
        tunnel.write_all(&version).map_err(io_error)?;
        let mut clock = [0; 44];
        tunnel.read_exact(&mut clock).map_err(io_error)?;
        tunnel.write_all(&clock).map_err(io_error)?;

        let request = |message: WaylandWireMessage| -> Result<WaylandWireMessage, String> {
            Packet {
                bytes: message.bytes,
                payload: Payload::None,
            }
            .write(&tunnel)?;
            read_wayland_wire_message_blocking(&compositor, MAX_FDS)
        };
        request(wire(1, 1, &[2], Vec::new()))?;
        for (global, interface, id) in [(3, "wl_compositor", 4), (5, "wl_shm", 6)] {
            let event = GeneratedEvent::WlRegistryGlobal {
                name: global,
                interface: Some(interface.into()),
                version: 1,
            };
            write_native(&compositor, &encode_generated_event(2, &event)?, &[])?;
            TunnelMessage::read(&tunnel)?;
            let name = interface.as_bytes();
            let mut string = name.to_vec();
            string.resize((name.len() + 1).next_multiple_of(4), 0);
            let mut bind = vec![global, name.len() as u32 + 1];
            bind.extend(
                string
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .copied()
                    .map(u32::from_ne_bytes),
            );
            bind.extend([1, id]);
            request(wire(2, 0, &bind, Vec::new()))?;
        }
        request(wire(4, 0, &[7], Vec::new()))?;
        let pool = request(wire(6, 0, &[8, 64], Vec::new()))?;
        assert_eq!(pool.fds.len(), 1);
        request(wire(8, 0, &[9, 0, 2, 2, 8, 1], Vec::new()))?;
        let second = request(wire(8, 0, &[10, 0, 2, 2, 8, 1], Vec::new()))?;
        assert_eq!(
            u32::from_ne_bytes(second.bytes[12..16].try_into().unwrap()),
            16
        );
        Packet {
            bytes: wire(7, 1, &[9, 0, 0], Vec::new()).bytes,
            payload: Payload::None,
        }
        .write(&tunnel)?;
        let result = request(wire(7, 6, &[], Vec::new()));
        assert_eq!(result.unwrap_err(), "backend Wayland stream reached EOF");
        let failure = worker
            .join()
            .map_err(|_| "bridge worker panicked")?
            .unwrap_err();
        assert_eq!(
            failure,
            "missing H.265 picture for the attached Wayland buffer"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires VA-API HEVC, GBM, and Vulkan"]
    fn dma_buf_frames_stay_on_gpu_through_encode_and_presentation() -> Result<(), String> {
        let node =
            std::env::var("WAYLAND_MCP_TEST_GPU").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        let request = serde_json::from_value(serde_json::json!({
            "windowId": "", "captureGpu": node, "encodeGpu": node, "decodeGpu": node,
            "chroma": 420, "bitDepth": 10, "screenContent": false
        }))
        .unwrap();
        let (width, height) = (640, 360);
        let source = memfd((width * height * 4) as usize)?;
        let first = display::DisplayBuffer::new(&node, width, height, XR30)?;
        let second = display::DisplayBuffer::new(&node, width, height, XR30)?;
        let mut encoder = ssh_video::BridgeEncoder::default();
        let mut decoder = ssh_video::BridgeDecoder::default();
        let rate = ssh_video::StreamRateControl::Vbr {
            frame_rate: 60,
            target_bitrate: Some(3_000_000),
        };
        let mut input = BridgePixels::Shm {
            fd: source.as_fd(),
            stride: width * 4,
            offset: 0,
        };
        let planes = [first.plane(), second.plane()];
        for frame in 0..7 {
            let picture = ssh_video::encode_bridge(
                &request,
                &node,
                BridgeSource {
                    extent: (width, height),
                    format: XR30,
                    pixels: input,
                    producer_gpu: Some(&node),
                    color: None,
                    implicit_sync: true,
                },
                true,
                rate,
                &mut encoder,
                || Ok(()),
            )?;
            assert!(picture.alpha.is_empty());
            let plane = &planes[frame % planes.len()];
            decoder.decode(
                &picture,
                (width, height),
                XR30,
                DisplayPixels::Dmabuf(plane),
            )?;
            input = BridgePixels::Dmabuf(plane);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires VA-API HEVC and Vulkan host memory import"]
    fn encodes_a_cursor_from_a_nonsealable_wayland_shm_file() -> Result<(), String> {
        let node =
            std::env::var("WAYLAND_MCP_TEST_GPU").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        let request = serde_json::from_value(serde_json::json!({
            "windowId": "", "captureGpu": node, "encodeGpu": node, "decodeGpu": node,
            "chroma": 420, "bitDepth": 8, "screenContent": false
        }))
        .map_err(io_error)?;
        let fd = unsafe { libc::memfd_create(c"test-wayland-cursor".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        let source = unsafe { File::from_raw_fd(fd) };
        let (width, height) = (24, 24);
        source
            .set_len(u64::from(width * height * 4))
            .map_err(io_error)?;
        let picture = ssh_video::encode_bridge(
            &request,
            &node,
            BridgeSource {
                extent: (width, height),
                format: AR24,
                pixels: BridgePixels::Shm {
                    fd: source.as_fd(),
                    stride: width * 4,
                    offset: 0,
                },
                producer_gpu: None,
                color: None,
                implicit_sync: true,
            },
            true,
            Default::default(),
            &mut ssh_video::BridgeEncoder::default(),
            || Ok(()),
        )?;
        assert!(!picture.color.is_empty());
        assert!(!picture.alpha.is_empty());
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GET_SEALS) },
            libc::F_SEAL_SEAL
        );
        Ok(())
    }

    #[test]
    fn overlapping_pictures_keep_separate_storage_and_release_only_local_slots()
    -> Result<(), String> {
        let mut state = State::new(End::Compositor, None);
        let mut fds = VecDeque::new();
        state.request(&mut wire(1, 1, &[2], Vec::new()).bytes, &mut fds)?;
        for (name, interface, id) in [(3, "wl_compositor", 3), (5, "wl_shm", 4)] {
            let mut global = encode_generated_event(
                2,
                &GeneratedEvent::WlRegistryGlobal {
                    name,
                    interface: Some(interface.into()),
                    version: 1,
                },
            )?;
            assert!(state.event(&mut global, &mut fds)?.2);
            let mut bind = host::bind(2, name, interface, 1, id);
            state.request(&mut bind.bytes, &mut fds)?;
        }
        for mut message in [
            wire(3, 0, &[5], Vec::new()),
            wire(4, 0, &[6, 64], Vec::new()),
            wire(6, 0, &[7, 0, 2, 2, 8, 0], Vec::new()),
        ] {
            state.request(&mut message.bytes, &mut fds)?;
        }
        let mut picture = |state: &mut State<'_>| {
            let attach = state.request(&mut wire(5, 1, &[7, 0, 0], Vec::new()).bytes, &mut fds)?;
            assert!(!attach.forward);
            let mut commit = state.request(&mut wire(5, 6, &[], Vec::new()).bytes, &mut fds)?;
            let storage =
                state.display_commit(commit.commit.take().unwrap(), &mut commit.before)?;
            let replica_format = commit.before.windows(2).find_map(|messages| {
                (!messages[0].fds.is_empty())
                    .then(|| u32::from_ne_bytes(messages[1].bytes[28..32].try_into().unwrap()))
            });
            let attached = &commit.before[commit.before.len() - 2].bytes;
            let id = u32::from_ne_bytes(attached[8..12].try_into().unwrap());
            Ok::<_, String>((id, storage, replica_format))
        };
        let (old_id, old, first_format) = picture(&mut state)?;
        let (new_id, new, replica_format) = picture(&mut state)?;
        assert_eq!(first_format, None);
        assert_eq!(replica_format, Some(0));
        assert_ne!(new_id, old_id);
        let (Backing::Shm(old_shm), Backing::Shm(new_shm)) = (&old.backing, &new.backing) else {
            unreachable!()
        };
        assert_ne!(
            old_shm.file.metadata().map_err(io_error)?.ino(),
            new_shm.file.metadata().map_err(io_error)?.ino()
        );

        let mut old_release = wire(old_id, 0, &[], Vec::new());
        assert!(!state.event(&mut old_release.bytes, &mut VecDeque::new())?.2);
        let mut new_release = wire(new_id, 0, &[], Vec::new());
        assert!(!state.event(&mut new_release.bytes, &mut VecDeque::new())?.2);

        let (reused_id, reused, reused_format) = picture(&mut state)?;
        assert_eq!(reused_format, None);
        assert_eq!(reused_id, old_id);
        assert!(Arc::ptr_eq(&reused, &old));
        let (replica_id, again, replica_format) = picture(&mut state)?;
        assert_eq!(replica_id, new_id);
        assert_eq!(replica_format, None);
        assert!(Arc::ptr_eq(&again, &new));

        for _ in 0..3 {
            assert!(state.host.can_commit(7));
            picture(&mut state)?;
        }
        assert!(!state.host.can_commit(7));
        let locked = Mutex::new(state);
        let released = locked.lock().unwrap().host.slot_released.clone();
        std::thread::scope(|scope| -> Result<(), String> {
            let (waiting, can_release) = std::sync::mpsc::sync_channel(0);
            let state = &locked;
            let waiter = scope.spawn(move || {
                let guard = state.lock().unwrap();
                waiting.send(()).unwrap();
                let (guard, timeout) = released
                    .wait_timeout_while(guard, Duration::from_secs(1), |guard| {
                        !guard.host.can_commit(7)
                    })
                    .unwrap();
                assert!(!timeout.timed_out());
                assert!(guard.host.can_commit(7));
            });
            can_release.recv().unwrap();
            let mut release = wire(old_id, 0, &[], Vec::new());
            assert!(
                !locked
                    .lock()
                    .unwrap()
                    .event(&mut release.bytes, &mut VecDeque::new())?
                    .2
            );
            waiter.join().unwrap();
            Ok(())
        })?;
        let mut state = locked.into_inner().unwrap();
        let (reused_id, reused, replica_format) = picture(&mut state)?;
        assert_eq!((reused_id, replica_format), (old_id, None));
        assert!(Arc::ptr_eq(&reused, &old));

        let mut sync = wire(1, 0, &[8], Vec::new());
        state.request(&mut sync.bytes, &mut fds)?;
        let native_callback = u32::from_ne_bytes(sync.bytes[8..12].try_into().unwrap());
        assert_ne!(native_callback, new_id);
        assert_ne!(native_callback, 8);
        let mut deleted = wire(1, 1, &[native_callback], Vec::new());
        assert!(state.event(&mut deleted.bytes, &mut fds)?.2);
        assert_eq!(&deleted.bytes[8..12], &8u32.to_ne_bytes());
        Ok(())
    }

    #[test]
    fn attached_picture_carries_separate_alpha_access_unit() -> Result<(), String> {
        let (sender, receiver) = UnixStream::pair().map_err(io_error)?;
        let frame = serde_json::from_value(serde_json::json!({
            "depth": 10, "chroma": 444, "screen_content": true, "decode_gpu": "/dev/dri/renderD128",
        }))
        .map_err(io_error)?;
        let bytes = wire(3, 6, &[], Vec::new()).bytes;
        Packet {
            bytes: bytes.clone(),
            payload: Payload::Picture(BridgePicture {
                frame,
                color: vec![1, 2, 3, 4],
                alpha: vec![5, 6, 7],
            }),
        }
        .write(&sender)?;
        let independent = Packet {
            bytes: wire(1, 0, &[6], Vec::new()).bytes,
            payload: Payload::None,
        };
        independent.write(&sender)?;
        let TunnelMessage::Wayland(packet, Some(transfer)) = TunnelMessage::read(&receiver)? else {
            panic!("missing Wayland packet");
        };
        assert_eq!(transfer.bytes, 7);
        assert_eq!(packet.bytes, bytes);
        let Payload::Picture(picture) = packet.payload else {
            panic!("missing picture");
        };
        assert_eq!(picture.color, [1, 2, 3, 4]);
        assert_eq!(picture.alpha, [5, 6, 7]);
        assert!(matches!(
            TunnelMessage::read(&receiver)?,
            TunnelMessage::Wayland(
                Packet {
                    payload: Payload::None,
                    ..
                },
                None
            )
        ));
        write_control(&sender, &[3 | (7 << 32), 1_234])?;
        assert!(matches!(
            TunnelMessage::read(&receiver)?,
            TunnelMessage::PictureReceived(
                3,
                PictureTransfer {
                    bytes: 7,
                    nanos: 1_234
                }
            )
        ));
        Ok(())
    }

    #[test]
    fn transport_rate_reacts_to_sustained_transfer_waits() {
        let mut rate = TransportRate::default();
        let now = Instant::now();
        let fast = PictureTransfer {
            bytes: 45_000,
            nanos: 3_000_000,
        };
        let congested = PictureTransfer {
            bytes: 45_000,
            nanos: 30_000_000,
        };
        assert_eq!(rate.received(congested, now), None);
        for _ in 0..16 {
            assert_eq!(rate.received(fast, now), None);
        }
        for _ in 0..5 {
            assert_eq!(rate.received(congested, now), None);
        }
        assert_eq!(rate.received(congested, now), Some(6_000_000));

        let worse = PictureTransfer {
            bytes: 45_000,
            nanos: 60_000_000,
        };
        for _ in 0..16 {
            assert_eq!(rate.received(worse, now), None);
        }
        assert_eq!(
            rate.received(worse, now + Duration::from_secs(2)),
            Some(3_000_000)
        );
    }

    #[test]
    fn zstd_keymap_recreates_the_descriptor_contents() -> Result<(), String> {
        let keymap = b"xkb_keymap { };\0";
        let input = memfd(keymap.len())?;
        input.write_all_at(keymap, 0).map_err(io_error)?;
        let compressed = compress_keymap(input.into(), keymap.len())?;
        let file = File::from(decompress_keymap(&compressed, keymap.len())?);
        let mut actual = [0; 16];
        file.read_exact_at(&mut actual[..keymap.len()], 0)
            .map_err(io_error)?;
        assert_eq!(&actual[..keymap.len()], keymap);
        assert!(decompress_keymap(&compressed[..compressed.len() - 1], keymap.len()).is_err());
        Ok(())
    }
}
