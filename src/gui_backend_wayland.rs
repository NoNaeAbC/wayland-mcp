#![allow(dead_code)]

#[path = "wayland_clipboard.rs"]
mod clipboard;
#[path = "wayland_scene.rs"]
mod scene;

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::env;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::mem::size_of;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Serialize;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::gui_backend::GuiBackend;
use crate::gui_backend::GuiCaptureNextFrameRequest;
use crate::gui_backend::GuiClickRequest;
use crate::gui_backend::GuiKeyboardTextPlan;
use crate::gui_backend::GuiKeyboardTextPlanRequest;
use crate::gui_backend::GuiKeyboardTextStroke;
use crate::gui_backend::GuiPointerMoveRequest;
use crate::gui_backend::GuiResizeWindowRequest;
use crate::gui_backend::GuiScreenshotRequest;
use crate::gui_backend::GuiSubsurfaceInfo;
use crate::gui_backend::GuiWaylandKeyboardEvent;
use crate::gui_backend::GuiWaylandKeyboardEventRequest;
use crate::gui_backend::GuiWaylandPointerEvent;
use crate::gui_backend::GuiWaylandPointerEventRequest;
use crate::gui_backend::GuiWindowInfo;
use crate::gui_backend::{GuiWaylandTouchEvent, GuiWaylandTouchEventRequest};
use crate::gui_wayland_generated::GENERATED_PROTOCOLS;
use crate::gui_wayland_generated::GeneratedArgKind;
use crate::gui_wayland_generated::GeneratedArgSpec;
use crate::gui_wayland_generated::GeneratedDecodedArg;
use crate::gui_wayland_generated::GeneratedEvent;
use crate::gui_wayland_generated::GeneratedHookRequest;
use crate::gui_wayland_generated::GeneratedHookRequestId;
use crate::gui_wayland_generated::GeneratedImplementedRequest;
use crate::gui_wayland_generated::GeneratedRequestId;
use crate::gui_wayland_generated::GeneratedTrackedRequest;
use crate::gui_wayland_generated::decode_generated_event_by_opcode;
use crate::gui_wayland_generated::decode_generated_hook_request;
use crate::gui_wayland_generated::decode_generated_hook_request_by_id;
use crate::gui_wayland_generated::decode_generated_implemented_request_by_id;
use crate::gui_wayland_generated::decode_generated_tracked_request_by_id;
use crate::gui_wayland_generated::encode_generated_event;
use crate::gui_wayland_generated::find_generated_event_by_opcode;
use crate::gui_wayland_generated::find_generated_request;
use crate::gui_wayland_generated::find_generated_request_by_opcode;
use crate::gui_wayland_generated::find_generated_request_id_by_opcode;
use crate::wayland_protocol_registry::WaylandGeneratedProtocolRegistry;
use crate::wayland_protocol_registry::WaylandHookRegistry;
use crate::wayland_writer::{ClientWriter, Origin, WireSink};

const DEFAULT_PROXY_SOCKET: &str = "wayland-mcp-0";
const SOCKET_ENV: &str = "WAYLAND_MCP_SOCKET";
const BACKEND_SOCKET_ENV: &str = "WAYLAND_MCP_BACKEND_SOCKET";
const BACKEND_BOOTSTRAP_REGISTRY_ID: u32 = 2;
const BACKEND_BOOTSTRAP_CALLBACK_ID: u32 = 3;
const MAX_BACKEND_EVENTS_PER_DRAIN: usize = 1024;
const MAX_CONNECTION_HISTORY: usize = 32;
// Wire support and capability exposure are separate decisions.
const MANUALLY_SUPPORTED_BACKEND_GLOBALS: &[&str] = &["wl_shm"];

#[derive(Clone)]
pub(crate) struct WaylandGuiBackend {
    state: Arc<WaylandProxyState>,
    transport: Arc<StdMutex<Option<Arc<WaylandProxyTransport>>>>,
    transport_error: Arc<StdMutex<Option<String>>>,
}

impl WaylandGuiBackend {
    pub(crate) async fn screenshot_surface(
        &self,
        window_id: String,
        surface_id: u32,
    ) -> Result<Vec<u8>, String> {
        // Copy a newly committed producer buffer while an observation lease is
        // active. Never import an old buffer after its release to the client.
        let refresh_for = Duration::from_secs(1);
        let deadline = tokio::time::Instant::now() + refresh_for;
        let (baseline, observation) = {
            let mut inner = self.state.inner.lock().await;
            let tracker = inner
                .sessions
                .values()
                .find_map(|session| {
                    session
                        .frame_tracker
                        .windows
                        .contains_key(&window_id)
                        .then_some(&session.frame_tracker)
                })
                .ok_or("unknown windowId")?;
            let surface = tracker.surface_for_capture(&window_id, surface_id)?;
            if surface.buffer_kind != Some("dmabuf") {
                return Err("surface screenshot requires a committed DMA-BUF buffer".into());
            }
            let baseline = surface.commit_serial;
            let id = inner.begin_observation(Some(window_id.clone()), refresh_for)?;
            (
                baseline,
                ObservationGuard {
                    inner: self.state.inner.clone(),
                    id,
                },
            )
        };
        let result = loop {
            let frame = {
                let inner = self.state.inner.lock().await;
                let tracker = inner
                    .sessions
                    .values()
                    .find_map(|session| {
                        session
                            .frame_tracker
                            .windows
                            .contains_key(&window_id)
                            .then_some(&session.frame_tracker)
                    })
                    .ok_or("capture window disappeared")?;
                let surface = tracker.surface_for_capture(&window_id, surface_id)?;
                if surface.commit_serial > baseline {
                    Some(tracker.capture_surface_rgba(surface))
                } else {
                    None
                }
            };
            if let Some(Ok(frame)) = frame {
                break encode_rgba_png(
                    frame.width,
                    frame.height,
                    &frame.rgba,
                    frame.color.as_ref(),
                );
            }
            if tokio::time::Instant::now() >= deadline {
                break Err(match frame {
                    Some(Err(error)) => error,
                    _ => format!("snapshot_unavailable: no fresh commit on surface {surface_id}"),
                });
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        };
        drop(observation);
        result
    }

    pub(crate) async fn screenshot_buffer(&self, window_id: String) -> Result<Vec<u8>, String> {
        let refresh_for = Duration::from_secs(1);
        let deadline = tokio::time::Instant::now() + refresh_for;
        let observation = {
            let mut inner = self.state.inner.lock().await;
            let id = inner.begin_observation(Some(window_id.clone()), refresh_for)?;
            ObservationGuard {
                inner: self.state.inner.clone(),
                id,
            }
        };
        let result = loop {
            let surface = {
                let inner = self.state.inner.lock().await;
                inner
                    .sessions
                    .values()
                    .find_map(|session| {
                        let window = session.frame_tracker.windows.get(&window_id)?;
                        session
                            .frame_tracker
                            .surfaces
                            .get(&window.wl_surface_id)
                            .cloned()
                    })
                    .ok_or("screenshot target is not mapped")?
            };
            let visual = self.state.visual.clone();
            let window = window_id.clone();
            let frame = tokio::task::spawn_blocking(move || {
                let snapshot = visual.read_snapshot(&window)?;
                let linear = surface.retained_snapshot_pixels(&snapshot)?;
                let color = surface.color;
                let hdr = color.as_ref().is_some_and(crate::gui_color::ColorDescription::is_hdr);
                let rgba: Vec<_> = linear.into_iter().flat_map(|p| crate::gui_color::encode_preview(p, hdr)).collect();
                encode_rgba_png(snapshot.width, snapshot.height, &rgba,
                    Some(&serde_json::json!({"coordinate_space":"buffer","commit_serial":snapshot.serial,
                        "tone_mapped":hdr,"source":color.as_ref().map(|color|color.metadata())})))
            }).await.map_err(|error| format!("in-memory screenshot worker failed: {error}"))?;
            match frame {
                Ok(png) => break Ok(png),
                Err(error)
                    if error.starts_with("snapshot_unavailable:")
                        && tokio::time::Instant::now() < deadline =>
                {
                    // Acquisition runs before the commit is latched. A concurrent
                    // frame can overtake the cloned surface metadata; retry with
                    // matching state instead of returning a transient stale error.
                    tokio::time::sleep(Duration::from_millis(8)).await;
                }
                Err(error) => break Err(error),
            }
        };
        drop(observation);
        result
    }
    pub(crate) async fn visual_info(&self, window: &str) -> Result<serde_json::Value, String> {
        let inner = self.state.inner.lock().await;
        let session = inner
            .sessions
            .values()
            .find(|s| {
                s.frame_tracker
                    .windows
                    .get(window)
                    .is_some_and(|w| w.mapped)
            })
            .ok_or("visual target is not mapped")?;
        let tracked = &session.frame_tracker.windows[window];
        let surface = session
            .frame_tracker
            .surfaces
            .get(&tracked.wl_surface_id)
            .ok_or("visual target has no committed surface")?;
        let buffer = surface
            .buffer_ref
            .as_ref()
            .ok_or("visual target has no committed buffer")?;
        let dma = if let TrackedBufferSource::Dmabuf(dma) = &buffer.source {
            Some(dma)
        } else {
            None
        };
        Ok(
            serde_json::json!({"windowId":window,"coordinateSpace":"buffer","bufferWidth":buffer.width,"bufferHeight":buffer.height,
            "bufferScale":surface.buffer_scale,"bufferTransform":surface.buffer_transform,"viewportSource":surface.viewport_source,
            "viewportDestination":surface.viewport_destination,"windowGeometryOffset":surface.window_geometry_offset,
            "format":dma.map(|d|d.format),"modifiers":dma.map(|d|d.planes.iter().map(|p|format!("0x{:016x}",p.modifier)).collect::<Vec<_>>()),
            "drmAffinity":session.dmabuf_main_device.map(|(major,minor)|serde_json::json!({"major":major,"minor":minor})),
            "sourceColor":surface.color.as_ref().map(|c|c.source_metadata()),"gpuImport":"not_tested","pixelsReadable":false,"bufferKind":buffer.kind_name(),"renderSurfaceId":tracked.wl_surface_id,"commitSerial":tracked.commit_serial,
            "subsurfaceCount":session.frame_tracker.subsurfaces_for_window(window).len(),
            "subsurfaces":session.frame_tracker.subsurfaces_for_window(window)}),
        )
    }
    pub(crate) fn visual_hub(&self) -> Arc<crate::visual_events::VisualHub> {
        self.state.visual.clone()
    }
    pub(crate) async fn subscribe_visual(
        &self,
        args: &serde_json::Value,
    ) -> Result<crate::input_events::Subscription, String> {
        let window = args
            .get("windowId")
            .and_then(serde_json::Value::as_str)
            .ok_or("onVisual requires windowId")?;
        let inner = self.state.inner.lock().await;
        let session = inner
            .sessions
            .values()
            .find(|s| {
                s.frame_tracker
                    .windows
                    .get(window)
                    .is_some_and(|w| w.mapped)
            })
            .ok_or("visual target is not mapped")?;
        let surface = session
            .frame_tracker
            .surface_for_window(window)
            .ok_or("visual target has no committed surface")?;
        if surface.buffer_kind != Some("dmabuf") {
            return Err("visual observers require GPU DMA-BUF frames".into());
        }
        if surface.color.is_none()
            && args
                .get("sourceColor")
                .is_none_or(serde_json::Value::is_null)
        {
            return Err(
                "untagged visual source requires an explicit sourceColor assumption".into(),
            );
        }
        let affinity = session
            .dmabuf_main_device
            .ok_or("no DMA-BUF feedback DRM-device affinity; GPU observation unavailable")?;
        let visual = self.state.visual.clone();
        let args = args.clone();
        drop(inner);
        let subscription = tokio::task::spawn_blocking(move || visual.subscribe(&args, affinity))
            .await
            .map_err(|e| e.to_string())??;
        self.confirm_visual_target(window, subscription, None).await
    }
    pub(crate) async fn subscribe_visual_program(
        &self,
        args: &serde_json::Value,
    ) -> Result<crate::input_events::Subscription, String> {
        let window = args
            .get("windowId")
            .and_then(serde_json::Value::as_str)
            .ok_or("onVisualProgram requires windowId")?;
        let info = self.visual_info(window).await?;
        if info["bufferKind"] != "dmabuf" {
            return Err("visual programs require GPU DMA-BUF frames".into());
        }
        if info["sourceColor"].is_null()
            && args
                .get("sourceColor")
                .is_none_or(serde_json::Value::is_null)
        {
            return Err(
                "untagged visual source requires an explicit sourceColor assumption".into(),
            );
        }
        let affinity = (
            info["drmAffinity"]["major"]
                .as_u64()
                .ok_or("missing DRM affinity")? as u32,
            info["drmAffinity"]["minor"]
                .as_u64()
                .ok_or("missing DRM affinity")? as u32,
        );
        let width = info["bufferWidth"].as_u64().ok_or("missing buffer width")? as u32;
        let height = info["bufferHeight"]
            .as_u64()
            .ok_or("missing buffer height")? as u32;
        let visual = self.state.visual.clone();
        let args = args.clone();
        let subscription = tokio::task::spawn_blocking(move || {
            visual.subscribe_program(&args, affinity, width, height)
        })
        .await
        .map_err(|e| e.to_string())??;
        self.confirm_visual_target(window, subscription, Some((width, height)))
            .await
    }
    async fn confirm_visual_target(
        &self,
        window: &str,
        subscription: crate::input_events::Subscription,
        extent: Option<(u32, u32)>,
    ) -> Result<crate::input_events::Subscription, String> {
        // The selected client can disappear or replace its buffer while the
        // blocking compiler runs. Do not publish an orphaned registration.
        let current = self.visual_info(window).await;
        if current.as_ref().is_ok_and(|info| {
            info["bufferKind"] == "dmabuf"
                && extent.is_none_or(|(width, height)| {
                    info["bufferWidth"] == width && info["bufferHeight"] == height
                })
        }) {
            return Ok(subscription);
        }
        let error = current
            .err()
            .unwrap_or_else(|| "visual target changed while subscribing".into());
        let _ = self
            .state
            .visual
            .stop(subscription.id, &format!("visual_error: {error}"));
        Err(error)
    }
    /// Select the compositor for subsequent proxied clients. Existing client
    /// connections cannot migrate between Wayland displays.
    pub(crate) async fn select_backend(&self, display: String) -> Result<String, String> {
        let mut server = self.state.inner.lock().await;
        if !server.sessions.is_empty() {
            return Err(
                "Close proxied windows before switching the Wayland compositor".to_string(),
            );
        }
        let socket_path = resolve_backend_choice(&display, &server.config.backend_socket)?
            .to_string_lossy()
            .into_owned();
        let mut candidate = server.config.clone();
        candidate.backend_socket = socket_path.clone();
        WaylandBackendSession::connect(&candidate)?;
        server.config.backend_socket = socket_path.clone();
        Ok(socket_path)
    }
    pub(crate) async fn resize_window(
        &self,
        request: GuiResizeWindowRequest,
    ) -> Result<String, String> {
        self.state.resize_window(request).await
    }
    pub(crate) fn new() -> Self {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig::from_env());
        register_core_protocols(&mut server.registry);
        register_frame_tracking_intercepts(&mut server.registry);
        let state = Arc::new(WaylandProxyState {
            inner: Arc::new(Mutex::new(server)),
            input: Arc::new(crate::input_events::InputHub::default()),
            visual: Arc::new(crate::visual_events::VisualHub::default()),
        });
        let (transport, transport_error) =
            match WaylandProxyTransport::try_spawn(Arc::clone(&state)) {
                Ok(transport) => (Some(Arc::new(transport)), None),
                Err(err) => (None, Some(err)),
            };
        Self {
            state: Arc::clone(&state),
            transport: Arc::new(StdMutex::new(transport)),
            transport_error: Arc::new(StdMutex::new(transport_error)),
        }
    }

    pub(crate) async fn snapshot(&self) -> WaylandBackendSnapshot {
        let mut snapshot = self.state.snapshot().await;
        // `running` describes the proxy service, not whether a GUI client is
        // mapped right now.  Conflating the two made a clean last-window exit
        // look like a compositor crash to autonomous QA clients.
        let transport = self.current_transport();
        let transport_error = match transport.as_ref() {
            Some(transport) => transport.availability_error(),
            None => self
                .current_transport_error()
                .or_else(|| Some("Wayland proxy transport is not initialized".to_string())),
        };
        snapshot.running = transport_error.is_none();
        snapshot.socket_path = transport
            .as_ref()
            .map(|transport| transport.socket_path.display().to_string());
        snapshot.launch_preflight = transport
            .as_ref()
            .map(|transport| transport.launch_preflight())
            .unwrap_or_else(|| WaylandLaunchPreflight::unavailable(transport_error.clone()));
        if !snapshot.running && snapshot.last_runtime_error.is_none() {
            snapshot.last_runtime_error = transport_error;
        }
        snapshot
    }

    /// Return a launch environment only after proving that its listener and
    /// socket path are live. A prior implementation returned the transport's
    /// cached strings even after its accept task or TempDir had disappeared.
    pub(crate) fn sandbox_env(&self) -> Result<HashMap<String, String>, String> {
        self.ensure_proxy_transport()
            .map(|transport| transport.sandbox_env())
    }

    /// Return the client-facing launch contract. Keep this separate from
    /// `sandbox_env`: only the two real environment variables may be injected
    /// into a child process, while the remaining fields are diagnostics.
    pub(crate) fn launch_environment(&self) -> Result<WaylandLaunchEnvironment, String> {
        self.ensure_proxy_transport()
            .map(|transport| transport.launch_environment())
    }

    pub(crate) fn has_proxy_transport(&self) -> bool {
        self.current_transport().is_some()
    }

    pub(crate) fn transport_startup_error(&self) -> Option<String> {
        self.current_transport_error()
    }

    pub(crate) fn transport_available(&self) -> bool {
        self.current_transport()
            .is_some_and(|transport| transport.availability_error().is_none())
    }

    fn current_transport(&self) -> Option<Arc<WaylandProxyTransport>> {
        self.transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn current_transport_error(&self) -> Option<String> {
        self.transport_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn ensure_proxy_transport(&self) -> Result<Arc<WaylandProxyTransport>, String> {
        let mut slot = self
            .transport
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(transport) = slot.as_ref()
            && transport.availability_error().is_none()
        {
            return Ok(Arc::clone(transport));
        }

        let previous_error = slot
            .as_ref()
            .and_then(|transport| transport.availability_error());
        match WaylandProxyTransport::try_spawn(Arc::clone(&self.state)) {
            Ok(transport) => {
                let transport = Arc::new(transport);
                if let Some(error) = transport.availability_error() {
                    *self
                        .transport_error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
                    return Err(error);
                }
                *slot = Some(Arc::clone(&transport));
                *self
                    .transport_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                Ok(transport)
            }
            Err(error) => {
                let error = match previous_error {
                    Some(previous) => format!(
                        "Wayland proxy endpoint became unavailable ({previous}) and could not be restarted: {error}"
                    ),
                    None => error,
                };
                *self
                    .transport_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
                Err(error)
            }
        }
    }
}

fn resolve_backend_choice(display: &str, current_backend: &str) -> Result<PathBuf, String> {
    if display.is_empty() {
        return Err(
            "selectBackend requires a Wayland display name or absolute socket path".to_string(),
        );
    }
    let path = PathBuf::from(display);
    if path.is_absolute() {
        return Ok(path);
    }
    if display.contains('/') || display == "." || display == ".." {
        return Err("selectBackend display name must be a single socket name".to_string());
    }
    // The MCP proxy replaces XDG_RUNTIME_DIR in its own process with its
    // client-facing socket directory. Resolve sibling compositors beside the
    // currently selected backend instead.
    let backend = PathBuf::from(current_backend);
    let runtime = if backend.is_absolute() {
        backend.parent().map(PathBuf::from)
    } else {
        env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
    }
    .ok_or_else(|| "Cannot resolve the Wayland backend runtime directory".to_string())?;
    Ok(runtime.join(display))
}

#[async_trait::async_trait]
impl GuiBackend for WaylandGuiBackend {
    async fn cleanup_model_input(&self) -> Result<(), String> {
        self.state.cleanup_model_input().await
    }
    fn input_hub(&self) -> Option<Arc<crate::input_events::InputHub>> {
        Some(self.state.input.clone())
    }
    async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
        self.state.list_windows().await
    }

    async fn screenshot(&self, request: GuiScreenshotRequest) -> Result<Vec<u8>, String> {
        if let Some(frame) = self.state.screenshot_png(request, false).await? {
            return Ok(frame);
        }
        Err(self.screenshot_unavailable_message())
    }

    async fn capture_next_frame(
        &self,
        request: GuiCaptureNextFrameRequest,
    ) -> Result<Vec<u8>, String> {
        if let Some(frame) = self.state.capture_next_frame_png(request).await? {
            return Ok(frame);
        }
        Err(self.screenshot_unavailable_message())
    }

    async fn emit_wayland_pointer_event(
        &self,
        request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String> {
        self.state.emit_wayland_pointer_event(request).await
    }

    async fn emit_wayland_keyboard_event(
        &self,
        request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String> {
        self.state.emit_wayland_keyboard_event(request).await
    }

    async fn keyboard_text_plan(
        &self,
        request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String> {
        self.state.keyboard_text_plan(request).await
    }
}

impl WaylandGuiBackend {
    pub(crate) async fn begin_observation(
        &self,
        window: String,
        duration_ms: u64,
    ) -> Result<serde_json::Value, String> {
        if !(1..=120_000).contains(&duration_ms) {
            return Err("observation duration must be 1..120000 milliseconds".into());
        }
        let mut inner = self.state.inner.lock().await;
        if !inner.sessions.values().any(|s| {
            s.frame_tracker
                .windows
                .get(&window)
                .is_some_and(|w| w.mapped)
        }) {
            return Err("observation target is not mapped".into());
        }
        let id = inner.begin_observation(Some(window), Duration::from_millis(duration_ms))?;
        Ok(serde_json::json!({"id":id,"durationMs":duration_ms}))
    }
    pub(crate) async fn end_observation(&self, id: u64) -> Result<serde_json::Value, String> {
        Ok(
            serde_json::json!({"ended":self.state.inner.lock().await.observations.remove(&id).is_some()}),
        )
    }

    pub(crate) async fn input_capabilities(
        &self,
        window: &str,
    ) -> Result<serde_json::Value, String> {
        let inner = self.state.inner.lock().await;
        let session = inner
            .sessions
            .values()
            .find(|s| s.frame_tracker.windows.contains_key(window))
            .ok_or("unknown windowId")?;
        let mut seats = BTreeSet::new();
        let mut resources = Vec::new();
        for (id, interface) in &session.object_interfaces {
            if ![
                "wl_pointer",
                "wl_keyboard",
                "wl_touch",
                "zwp_relative_pointer_v1",
                "zwp_locked_pointer_v1",
                "zwp_confined_pointer_v1",
            ]
            .contains(&interface.as_str())
            {
                continue;
            }
            let seat = session.logical_seat(*id);
            seats.insert(seat);
            resources.push(serde_json::json!({"interface":interface,"version":session.object_versions.get(id).copied().unwrap_or(1),"seatId":format!("seat:{}:{seat}",session.client_id.0)}));
        }
        resources.sort_by_key(|r| r["interface"].as_str().unwrap_or("").to_string());
        Ok(
            serde_json::json!({"windowId":window,"resources":resources,"streams":["pointer","keyboard","touch","relative_pointer","pointer_constraints"],"coordinateSpace":"surface-fixed","touchInjectionRequiresSingleSeat":true,"unsupported":["tablet","gestures","text-input-ime"]}),
        )
    }

    pub(crate) async fn emit_wayland_touch_event(
        &self,
        request: GuiWaylandTouchEventRequest,
    ) -> Result<String, String> {
        self.state.emit_wayland_touch_event(request).await
    }

    fn screenshot_unavailable_message(&self) -> String {
        if self.transport_available() {
            "wayland.screenshot has no proxied Wayland window to capture yet; call wayland.windows first. This path only captures windows connected through the proxy and does not fall back to whole-desktop capture".to_string()
        } else if let Some(err) = self
            .current_transport()
            .and_then(|transport| transport.availability_error())
        {
            format!(
                "wayland.screenshot is unavailable because the in-process Wayland proxy endpoint is not live: {err}"
            )
        } else if let Some(err) = self.current_transport_error() {
            format!(
                "wayland.screenshot is unavailable because the in-process Wayland proxy failed to start: {err}"
            )
        } else {
            "wayland.screenshot is unavailable until the generated protocol server is implemented; the old raw-wire proxy transport is disabled and there is no whole-desktop fallback".to_string()
        }
    }
}

static GPU_READS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

struct ObservationGuard {
    inner: Arc<Mutex<WaylandProxyServer>>,
    id: u64,
}
impl Drop for ObservationGuard {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        let id = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                inner.lock().await.observations.remove(&id);
            });
        }
    }
}

struct WaylandProxyState {
    inner: Arc<Mutex<WaylandProxyServer>>,
    input: Arc<crate::input_events::InputHub>,
    visual: Arc<crate::visual_events::VisualHub>,
}

impl WaylandProxyState {
    async fn cleanup_model_input(&self) -> Result<(), String> {
        // Include already-admitted writes before inspecting successful delivery.
        let receipts = {
            let inner = self.inner.lock().await;
            inner
                .sessions
                .values()
                .filter_map(|session| {
                    session
                        .client_event_writer
                        .as_ref()
                        .map(|writer| (writer.clone(), writer.sequence()))
                })
                .collect::<Vec<_>>()
        };
        for (writer, sequence) in receipts {
            writer.wait(sequence).await?;
        }
        self.deliver_mutation(|server| {
            server.observations.clear();
            server.input_render_deadlines.clear();
            for session in server.sessions.values_mut() {
                session.release_model_pressed()?;
            }
            Ok("released delivered synthetic presses; physical presses preserved".into())
        })
        .await
        .map(|_| ())
    }

    async fn deliver_mutation<F>(&self, mutation: F) -> Result<String, String>
    where
        F: FnOnce(&mut WaylandProxyServer) -> Result<String, String> + Send,
    {
        let mut inner = self.inner.lock().await;
        let before = inner
            .sessions
            .iter()
            .map(|(id, session)| {
                (
                    *id,
                    session
                        .client_event_writer
                        .as_ref()
                        .map_or(0, ClientWriter::sequence),
                )
            })
            .collect::<HashMap<_, _>>();
        let result = mutation(&mut inner);
        let receipts = inner
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                let writer = session.client_event_writer.as_ref()?;
                let sequence = writer.sequence();
                (sequence > before.get(id).copied().unwrap_or(0))
                    .then(|| (writer.clone(), sequence))
            })
            .collect::<Vec<_>>();
        drop(inner);
        for (writer, sequence) in receipts {
            writer.wait(sequence).await?;
        }
        result
    }

    async fn resize_window(&self, request: GuiResizeWindowRequest) -> Result<String, String> {
        self.deliver_mutation(|inner| inner.resize_window(request))
            .await
    }
    async fn snapshot(&self) -> WaylandBackendSnapshot {
        let inner = self.inner.lock().await;
        inner.snapshot()
    }

    async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
        let inner = self.inner.lock().await;
        let mut windows = inner
            .sessions
            .values()
            .flat_map(|session| session.frame_tracker.list_windows())
            .collect::<Vec<_>>();
        windows.sort_by_key(|window| Reverse(window.commit_serial));
        Ok(windows)
    }

    async fn snapshot_current_gpu(&self, window_id: Option<&str>) -> Result<(), String> {
        // The producer may already have received its release point. Read the
        // server-owned copy, never that released DMA-BUF, for idle windows.
        let retained = {
            let inner = self.inner.lock().await;
            let windows = inner
                .sessions
                .values()
                .flat_map(|s| s.frame_tracker.list_windows())
                .filter(|w| w.mapped)
                .collect::<Vec<_>>();
            select_window_for_screenshot(&windows, window_id)?.and_then(|window| {
                inner.sessions.iter().find_map(|(client, session)| {
                    let surface = session
                        .frame_tracker
                        .surface_for_window(&window.window_id)?;
                    (surface.linear_rgba.is_empty() && surface.buffer_kind == Some("dmabuf"))
                        .then(|| (*client, window.window_id.clone(), surface.clone()))
                })
            })
        };
        if let Some((client, window, surface)) = retained {
            let visual = self.visual.clone();
            let expected = surface.clone();
            let pixels = tokio::task::spawn_blocking(move || {
                let snapshot = visual.read_snapshot(&window)?;
                expected.retained_snapshot_pixels(&snapshot)
            })
            .await
            .map_err(|e| format!("retained GPU snapshot worker failed: {e}"))?;
            let mut inner = self.inner.lock().await;
            if let Some(current) = inner
                .sessions
                .get_mut(&client)
                .and_then(|s| s.frame_tracker.surfaces.get_mut(&surface.id))
                && current.commit_serial == surface.commit_serial
                && current.buffer_id == surface.buffer_id
            {
                match pixels {
                    Ok(pixels) => {
                        current.linear_rgba = Arc::new(pixels);
                        current.capture_error = None;
                    }
                    Err(error) => current.capture_error = Some(error),
                }
            }
        }
        let jobs = {
            let mut inner = self.inner.lock().await;
            let windows = inner
                .sessions
                .values()
                .flat_map(|s| s.frame_tracker.list_windows())
                .filter(|w| w.mapped)
                .collect::<Vec<_>>();
            let Some(window) = select_window_for_screenshot(&windows, window_id)?.cloned() else {
                return Ok(());
            };
            let mut jobs = Vec::new();
            for (client, session) in &mut inner.sessions {
                let tracker = &session.frame_tracker;
                for surface in tracker.surfaces.values() {
                    if tracker.surface_to_window.get(&surface.id) != Some(&window.window_id)
                        || !surface.has_committed_buffer
                        || !surface.linear_rgba.is_empty()
                    {
                        continue;
                    }
                    let Some(buffer_id) = surface.buffer_id else {
                        continue;
                    };
                    let Some(buffer) = surface.buffer_ref.clone() else {
                        continue;
                    };
                    if !matches!(buffer.source, TrackedBufferSource::Dmabuf(_)) {
                        continue;
                    }
                    // A passed release cannot be revoked. Explicit timeline releases
                    // are independent of wl_buffer.release; those uses are copied
                    // only before forwarding a commit, never by this idle path.
                    if session.released_buffers.contains(&buffer_id)
                        || surface.last_release.is_some()
                        || surface.last_acquire.is_some()
                        || session
                            .object_interfaces
                            .get(&buffer_id)
                            .map(String::as_str)
                            != Some("wl_buffer")
                    {
                        continue;
                    }
                    *session.buffer_leases.entry(buffer_id).or_default() += 1;
                    jobs.push((
                        *client,
                        surface.id,
                        surface.commit_serial,
                        buffer_id,
                        buffer,
                        surface.color.clone(),
                        session.dmabuf_main_device,
                    ));
                }
            }
            jobs
        };
        let inner_state = self.inner.clone();
        let visual_hub = self.visual.clone();
        let worker = tokio::spawn(async move {
            for (client, surface, serial, buffer_id, buffer, color, affinity) in jobs {
                let permit = GPU_READS
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| e.to_string())?;
                let visual = visual_hub.clone();
                let copy = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let TrackedBufferSource::Dmabuf(dmabuf) = &buffer.source else {
                        unreachable!()
                    };
                    read_vulkan_dmabuf_linear(&visual, affinity, dmabuf, color.as_ref())
                });
                let pixels = match tokio::time::timeout(Duration::from_secs(5), copy).await {
                    Ok(result) => result.unwrap_or_else(|error| {
                        Err(format!("GPU snapshot worker failed: {error}"))
                    }),
                    Err(_) => {
                        let mut inner = inner_state.lock().await;
                        if let Some(session) = inner.sessions.get_mut(&client) {
                            if let Some(writer) = &session.client_event_writer {
                                writer.shutdown();
                            }
                            if let Some(backend) = &session.backend {
                                let _ = backend.stream.shutdown(Shutdown::Both);
                            }
                        }
                        return Err("GPU snapshot timed out; affected client disconnected".into());
                    }
                };
                let mut inner = inner_state.lock().await;
                if let Some(session) = inner.sessions.get_mut(&client) {
                    if let Some(state) = session.frame_tracker.surfaces.get_mut(&surface)
                        && state.commit_serial == serial
                        && state.buffer_id == Some(buffer_id)
                    {
                        match pixels {
                            Ok(pixels) => {
                                state.linear_rgba = Arc::new(pixels);
                                state.capture_error = None;
                            }
                            Err(error) => state.capture_error = Some(error),
                        }
                    }
                    if let Some(count) = session.buffer_leases.get_mut(&buffer_id) {
                        *count = count.saturating_sub(1);
                    }
                    if session.buffer_leases.get(&buffer_id).copied().unwrap_or(0) == 0 {
                        session.buffer_leases.remove(&buffer_id);
                        if let Some(release) = session.deferred_releases.remove(&buffer_id) {
                            session.released_buffers.insert(buffer_id);
                            if let Some(writer) = &session.client_event_writer {
                                let _ =
                                    writer.send_origin(&release.bytes, &release.fds, Origin::Human);
                            }
                        }
                    }
                }
            }
            Ok::<(), String>(())
        });
        worker
            .await
            .map_err(|e| format!("snapshot lease worker failed: {e}"))??;
        Ok(())
    }

    async fn screenshot_png(
        &self,
        request: GuiScreenshotRequest,
        buffer_coordinates: bool,
    ) -> Result<Option<Vec<u8>>, String> {
        // Give a pending producer update time to arrive even when the previous
        // frame is readable. Only fall back to that frame if the window is idle.
        let refresh_for = Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + refresh_for;
        let (baseline_serial, observation) = {
            let mut inner = self.inner.lock().await;
            let windows = inner
                .sessions
                .values()
                .flat_map(|session| session.frame_tracker.list_windows())
                .filter(|window| window.mapped)
                .collect::<Vec<_>>();
            let selected = select_window_for_screenshot(&windows, request.window_id.as_deref())?;
            let scope = selected.map(|w| w.window_id.clone());
            let baseline = selected.map(|w| w.commit_serial).unwrap_or(0);
            let id = inner.begin_observation(scope, refresh_for)?;
            (
                baseline,
                ObservationGuard {
                    inner: self.inner.clone(),
                    id,
                },
            )
        };

        let result = loop {
            // A commit can arrive after observation starts. Recover its owned
            // GPU copy too, rather than checking only the initial snapshot.
            self.snapshot_current_gpu(request.window_id.as_deref())
                .await?;
            let (selected, maybe_frame) = {
                let inner = self.inner.lock().await;
                let windows = inner
                    .sessions
                    .values()
                    .flat_map(|session| session.frame_tracker.list_windows())
                    .filter(|window| window.mapped)
                    .collect::<Vec<_>>();
                let selected =
                    select_window_for_screenshot(&windows, request.window_id.as_deref())?.cloned();
                let maybe_frame = selected.as_ref().and_then(|window| {
                    inner.sessions.values().find_map(|session| {
                        if buffer_coordinates {
                            session.frame_tracker.capture_buffer_rgba(&window.window_id)
                        } else {
                            session.frame_tracker.capture_window_rgba(&window.window_id)
                        }
                    })
                });
                (selected, maybe_frame)
            };
            let refreshed = selected
                .as_ref()
                .map(|window| window.commit_serial > baseline_serial)
                .unwrap_or(false);
            if refreshed && maybe_frame.as_ref().is_some_and(|frame| frame.is_ok())
                || tokio::time::Instant::now() >= deadline
            {
                if let Some(window) = selected
                    && !window.capturable
                    && !buffer_coordinates
                {
                    break Err(window.capture_error.unwrap_or_else(|| {
                        format!(
                            "window `{}` exists but is not capturable yet",
                            window.window_id
                        )
                    }));
                }
                break maybe_frame
                    .transpose()?
                    .map(|frame| {
                        encode_rgba_png(
                            frame.width,
                            frame.height,
                            &frame.rgba,
                            frame.color.as_ref(),
                        )
                    })
                    .transpose();
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        };
        drop(observation);
        result
    }

    async fn capture_next_frame_png(
        &self,
        request: GuiCaptureNextFrameRequest,
    ) -> Result<Option<Vec<u8>>, String> {
        let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(1_000).clamp(1, 10_000));
        let started_at = tokio::time::Instant::now();
        let deadline = started_at + timeout;
        let mut after_commit_serial = request.after_commit_serial;

        let id = self
            .inner
            .lock()
            .await
            .begin_observation(request.window_id.clone(), timeout)?;
        let observation = ObservationGuard {
            inner: self.inner.clone(),
            id,
        };
        let result = loop {
            let fresh = {
                let inner = self.inner.lock().await;
                let windows = inner
                    .sessions
                    .values()
                    .flat_map(|s| s.frame_tracker.list_windows())
                    .filter(|w| w.mapped)
                    .collect::<Vec<_>>();
                select_window_for_screenshot(&windows, request.window_id.as_deref())?.is_some_and(
                    |w| w.commit_serial > *after_commit_serial.get_or_insert(w.commit_serial),
                )
            };
            if fresh {
                self.snapshot_current_gpu(request.window_id.as_deref())
                    .await?;
            }
            let (selected_window, selected_frame) = {
                let mut inner = self.inner.lock().await;
                let windows = inner
                    .sessions
                    .values()
                    .flat_map(|session| session.frame_tracker.list_windows())
                    .filter(|window| window.mapped)
                    .collect::<Vec<_>>();
                let selected_window =
                    select_window_for_screenshot(&windows, request.window_id.as_deref())?.cloned();
                if let Some(window) = selected_window.as_ref()
                    && let Some((scope, _)) = inner.observations.get_mut(&observation.id)
                    && scope.is_none()
                {
                    *scope = Some(window.window_id.clone());
                }
                let mut selected_frame = None;
                if let Some(window) = selected_window.as_ref() {
                    let after = *after_commit_serial.get_or_insert(window.commit_serial);
                    if window.commit_serial > after {
                        for session in inner.sessions.values() {
                            if let Some(frame) =
                                session.frame_tracker.capture_window_rgba(&window.window_id)
                            {
                                selected_frame = Some(frame);
                                break;
                            }
                        }
                    }
                }
                (selected_window, selected_frame)
            };

            if let Some(window) = selected_window
                && window.commit_serial > after_commit_serial.unwrap_or(0)
            {
                if !window.capturable {
                    break Err(window.capture_error.clone().unwrap_or_else(|| {
                        format!(
                            "window `{}` produced a new frame at commit_serial={} but is not capturable",
                            window.window_id, window.commit_serial
                        )
                    }));
                }
                let Some(surface) = selected_frame else {
                    break Err(format!(
                        "window `{}` produced commit_serial={} but its tracked surface is missing",
                        window.window_id, window.commit_serial
                    ));
                };
                let surface = match surface {
                    Ok(surface) => surface,
                    Err(err) => break Err(err),
                };
                break encode_rgba_png(
                    surface.width,
                    surface.height,
                    &surface.rgba,
                    surface.color.as_ref(),
                )
                .map(Some);
            }

            if tokio::time::Instant::now() >= deadline {
                let window = request
                    .window_id
                    .as_deref()
                    .unwrap_or("the selected window");
                let after = after_commit_serial.unwrap_or(0);
                break Err(format!(
                    "timed out after {} ms waiting for a new frame from {window} after commit_serial={after}",
                    timeout.as_millis()
                ));
            }
            tokio::time::sleep(Duration::from_millis(16)).await;
        };
        drop(observation);
        result
    }

    async fn click(&self, request: GuiClickRequest) -> Result<String, String> {
        self.deliver_mutation(|inner| inner.inject_click(request))
            .await
    }

    async fn move_pointer(&self, request: GuiPointerMoveRequest) -> Result<String, String> {
        self.deliver_mutation(|inner| inner.inject_pointer_motion(request))
            .await
    }

    async fn emit_wayland_pointer_event(
        &self,
        request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String> {
        self.deliver_mutation(|inner| inner.emit_wayland_pointer_event(request))
            .await
    }

    async fn emit_wayland_touch_event(
        &self,
        request: GuiWaylandTouchEventRequest,
    ) -> Result<String, String> {
        self.deliver_mutation(|inner| inner.emit_wayland_touch_event(request))
            .await
    }

    async fn emit_wayland_keyboard_event(
        &self,
        request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String> {
        self.deliver_mutation(|inner| inner.emit_wayland_keyboard_event(request))
            .await
    }

    async fn keyboard_text_plan(
        &self,
        request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String> {
        let inner = self.inner.lock().await;
        inner.keyboard_text_plan(request)
    }

    async fn pulse_frame_callbacks(&self, client: WaylandClientId) -> Result<(), String> {
        let visual_windows = self.visual.active_windows();
        self.deliver_mutation(|server| {
            server.visual_windows = visual_windows;
            let events = server.observed_frame_events(client, Instant::now())?;
            let session = server.sessions.get(&client).ok_or("client disconnected")?;
            if let Some(writer) = &session.client_event_writer {
                for event in events {
                    writer.send_origin(&event.encoded.bytes, &[], Origin::Local)?;
                }
            }
            Ok("completed capture-output frame callbacks".into())
        })
        .await
        .map(|_| ())
    }

    async fn register_live_client_session(&self) -> WaylandClientId {
        let mut inner = self.inner.lock().await;
        let globals = inner.synthetic_backend_globals();
        inner.register_client_session(globals)
    }

    async fn finish_client_session(&self, client_id: WaylandClientId, detail: String) {
        let mut inner = self.inner.lock().await;
        if let Some(session) = inner.sessions.get(&client_id) {
            for window in session.frame_tracker.windows.keys() {
                self.input.close_window(window, "client_disconnected");
                self.visual.close_window(window, "client_disconnected");
            }
        }
        inner.finish_client_session(client_id, detail);
    }

    async fn note_accept_error(&self, error: String) {
        let mut inner = self.inner.lock().await;
        inner.note_accept_error(error);
    }

    async fn ingest_request(
        &self,
        client_id: WaylandClientId,
        message: WaylandWireMessage,
    ) -> Result<IngestedWaylandRequest, String> {
        // Acquire an owned GPU snapshot before forwarding; analyze it afterwards.
        let visual_windows = self.visual.active_windows();
        self.inner.lock().await.visual_windows = visual_windows;
        let mut captured_visual = None;
        let mut invalid_visual = None;
        let visual_job = {
            let inner = self.inner.lock().await;
            inner.sessions.get(&client_id).and_then(|session| {
                let header = decode_wayland_header(&message.bytes).ok()?;
                if header.opcode != 6
                    || session
                        .object_interfaces
                        .get(&header.object_id)
                        .map(String::as_str)
                        != Some("wl_surface")
                {
                    return None;
                }
                let tracker = &session.frame_tracker;
                let window = tracker.surface_to_window.get(&header.object_id)?;
                self.visual.notice(window, "surfaceCommits");
                // This API observes one selected render surface, not a composite.
                // Decorative/auxiliary surfaces must not overwrite its history.
                if tracker.windows.get(window).map(|w| w.wl_surface_id) != Some(header.object_id) {
                    self.visual.notice(window, "auxiliarySurface");
                    return None;
                }
                let surface = tracker
                    .pending_surfaces
                    .get(&header.object_id)
                    .or_else(|| tracker.cached_surfaces.get(&header.object_id))
                    .or_else(|| tracker.surfaces.get(&header.object_id))?;
                if !surface.attach_pending && surface.damage.is_empty() {
                    self.visual.notice(window, "unchangedCommit");
                    return None;
                }
                let Some(buffer) = surface.buffer_ref.clone() else {
                    invalid_visual = Some((window.clone(), "visual target detached its buffer"));
                    return None;
                };
                if !matches!(buffer.source, TrackedBufferSource::Dmabuf(_)) {
                    invalid_visual = Some((
                        window.clone(),
                        "visual events require DMA-BUF; CPU/SHM fallback is forbidden",
                    ));
                    return None;
                }
                self.visual.notice(window, "eligibleCommits");
                let invalid = if tracker.effectively_synchronized(header.object_id) {
                    Some(
                        "visual events require an independently committed render surface"
                            .to_string(),
                    )
                } else if surface.buffer_transform != 0 || surface.viewport_source.is_some() {
                    Some(
                        "visual events currently require untransformed buffer coordinates"
                            .to_string(),
                    )
                } else {
                    None
                };
                let acquire = surface
                    .pending_acquire
                    .and_then(|point| point.timeline_id.map(|id| (id, point.point)))
                    .map(|(id, point)| {
                        tracker
                            .syncobj_timelines
                            .get(&id)
                            .ok_or_else(|| "missing acquire timeline".to_string())
                            .and_then(|timeline| duplicate_fd(&timeline.fd).map(|fd| (fd, point)))
                    });
                Some((
                    window.clone(),
                    session.dmabuf_main_device,
                    buffer,
                    acquire,
                    invalid,
                    tracker.next_commit_serial + 1,
                    tracker.colors.next(header.object_id).cloned(),
                ))
            })
        };
        if let Some((window, error)) = invalid_visual {
            self.visual
                .close_window(&window, &format!("visual_error: {error}"));
        }
        if let Some((window, affinity, buffer, acquire, invalid, serial, color)) = visual_job {
            let visual = self.visual.clone();
            let failed_window = window.clone();
            let timestamp = visual.timestamp();
            let result = tokio::task::spawn_blocking(move || {
                let affinity = affinity.ok_or("DRM affinity unavailable")?;
                let TrackedBufferSource::Dmabuf(dmabuf) = &buffer.source else {
                    unreachable!()
                };
                let planes = dmabuf
                    .planes
                    .iter()
                    .map(|p| crate::visual_events::Plane {
                        fd: p.fd.as_raw_fd(),
                        offset: p.offset,
                        stride: p.stride,
                        modifier: p.modifier,
                    })
                    .collect::<Vec<_>>();
                if planes.is_empty() {
                    return Err("DMA-BUF has no planes".into());
                }
                let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                if unsafe { libc::fstat(planes[0].fd, stat.as_mut_ptr()) } < 0 {
                    return Err("DMA-BUF identity unavailable".into());
                }
                let key = unsafe { stat.assume_init() }.st_ino;
                let mut acquire = acquire;
                let mut wait_acquire = || {
                    if let Some(acquire) = acquire.take() {
                        let (fd, point) = acquire?;
                        wait_drm_syncobj_timeline(&fd, point)?;
                    }
                    Ok(())
                };
                visual.retain_snapshot(
                    &window,
                    affinity,
                    key,
                    dmabuf.width,
                    dmabuf.height,
                    dmabuf.format,
                    &planes,
                    serial,
                    &mut wait_acquire,
                )?;
                if !visual.interested(&window) {
                    return Ok(None);
                }
                if let Some(error) = invalid {
                    return Err(error);
                }
                visual.capture(
                    &window,
                    affinity,
                    key,
                    dmabuf.width,
                    dmabuf.height,
                    dmabuf.format,
                    &planes,
                    serial,
                    timestamp,
                    color.as_ref(),
                    wait_acquire,
                )
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
            match result {
                Ok(captured) => captured_visual = captured,
                Err(error) => self
                    .visual
                    .close_window(&failed_window, &format!("visual_error: {error}")),
            }
        }
        // The client request thread stays ordered while GPU acquisition runs
        // outside the shared server lock. Copy completes before forwarding the
        // commit, so the compositor cannot release this use before it is owned.
        let job = {
            let mut inner = self.inner.lock().await;
            if inner.capture_tracking_active() {
                inner.sessions.get(&client_id).and_then(|session| {
                    let header = decode_wayland_header(&message.bytes).ok()?;
                    if header.opcode != 6
                        || session
                            .object_interfaces
                            .get(&header.object_id)
                            .map(String::as_str)
                            != Some("wl_surface")
                    {
                        return None;
                    }
                    if !inner.observing_surface(client_id, header.object_id) {
                        return None;
                    }
                    let tracker = &session.frame_tracker;
                    let surface = tracker
                        .pending_surfaces
                        .get(&header.object_id)
                        .or_else(|| tracker.cached_surfaces.get(&header.object_id))
                        .or_else(|| tracker.surfaces.get(&header.object_id))?;
                    let buffer = surface.buffer_ref.clone()?;
                    if !matches!(buffer.source, TrackedBufferSource::Dmabuf(_)) {
                        return None;
                    }
                    // An empty commit does not authorize reading a buffer that
                    // may already have been released. Keep its owned snapshot.
                    if !surface.attach_pending && surface.damage.is_empty() {
                        return None;
                    }
                    let acquire = surface
                        .pending_acquire
                        .and_then(|point| point.timeline_id.map(|id| (id, point.point)))
                        .map(|(id, point)| {
                            tracker
                                .syncobj_timelines
                                .get(&id)
                                .ok_or_else(|| "missing acquire timeline".to_string())
                                .and_then(|timeline| {
                                    duplicate_fd(&timeline.fd).map(|fd| (fd, point))
                                })
                        });
                    Some((
                        header.object_id,
                        buffer,
                        tracker.colors.next(header.object_id).cloned(),
                        acquire,
                        session.dmabuf_main_device,
                    ))
                })
            } else {
                None
            }
        };
        if let Some((surface, buffer, color, acquire, affinity)) = job {
            let pixels = if let Ok(permit) = GPU_READS.clone().try_acquire_owned() {
                let visual = self.visual.clone();
                let copy = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    if let Some(acquire) = acquire {
                        let (fd, point) = acquire?;
                        wait_drm_syncobj_timeline(&fd, point)?;
                    }
                    let TrackedBufferSource::Dmabuf(dmabuf) = &buffer.source else {
                        unreachable!()
                    };
                    read_vulkan_dmabuf_linear(&visual, affinity, dmabuf, color.as_ref())
                });
                match tokio::time::timeout(Duration::from_secs(5), copy).await {
                    Ok(result) => result.map_err(|e| format!("GPU snapshot worker failed: {e}"))?,
                    Err(_) => {
                        let mut inner = self.inner.lock().await;
                        if let Some(session) = inner.sessions.get_mut(&client_id) {
                            if let Some(writer) = &session.client_event_writer {
                                writer.shutdown();
                            }
                            if let Some(backend) = &session.backend {
                                let _ = backend.stream.shutdown(Shutdown::Both);
                            }
                        }
                        return Err("GPU snapshot timed out; affected client disconnected".into());
                    }
                }
            } else {
                Err("snapshot_unavailable: GPU capture slots are busy".into())
            };
            if let Some(session) = self.inner.lock().await.sessions.get_mut(&client_id) {
                session.frame_tracker.prepared_gpu.insert(surface, pixels);
            }
        }
        let mut inner = self.inner.lock().await;
        let before = inner
            .sessions
            .get(&client_id)
            .map(|session| {
                session
                    .frame_tracker
                    .windows
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let result = inner.ingest_request(client_id, message);
        for window in before {
            if !inner
                .sessions
                .get(&client_id)
                .is_some_and(|session| session.frame_tracker.windows.contains_key(&window))
            {
                self.input.close_window(&window, "window_destroyed");
                self.visual.close_window(&window, "window_destroyed");
            }
        }
        drop(inner);
        if result.is_ok()
            && let Some(captured) = captured_visual
        {
            let visual = self.visual.clone();
            tokio::task::spawn_blocking(move || visual.complete(captured));
        }
        result
    }

    async fn track_object_interface(
        &self,
        client_id: WaylandClientId,
        object_id: u32,
        interface: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.track_object_interface(client_id, object_id, interface)
    }

    async fn set_client_event_writer(
        self: &Arc<Self>,
        client_id: WaylandClientId,
        socket: StdUnixStream,
    ) -> Result<ClientWriter, String> {
        let writer = self
            .inner
            .lock()
            .await
            .set_client_event_writer(client_id, socket)?;
        let weak = Arc::downgrade(self);
        let runtime = tokio::runtime::Handle::current();
        writer.set_hook(Arc::new(move |bytes, origin, surface| {
            if let Some(state) = weak.upgrade() {
                runtime.block_on(state.input_delivered_scoped(client_id, bytes, origin, surface));
            }
        }));
        Ok(writer)
    }
    async fn input_delivered(&self, client: WaylandClientId, bytes: &[u8], origin: Origin) {
        self.input_delivered_scoped(client, bytes, origin, None)
            .await;
    }
    async fn input_delivered_scoped(
        &self,
        client: WaylandClientId,
        bytes: &[u8],
        origin: Origin,
        target: Option<u32>,
    ) {
        if origin == Origin::Local {
            return;
        }
        let mut inner = self.inner.lock().await;
        let Some(session) = inner.sessions.get_mut(&client) else {
            return;
        };
        let Ok(decoded) = decode_wayland_event(&session.object_interfaces, bytes) else {
            return;
        };
        let device = match decoded.interface.as_str() {
            "wl_pointer" => "pointer",
            "wl_keyboard" => "keyboard",
            "wl_touch" => "touch",
            "zwp_relative_pointer_v1" => "relative_pointer",
            "zwp_locked_pointer_v1" | "zwp_confined_pointer_v1" => "pointer_constraints",
            _ => return,
        };
        let kind = decoded.event_name.split('.').nth(1).unwrap_or("");
        if matches!(kind, "keymap" | "repeat_info") {
            return;
        }
        let seat = session.logical_seat(decoded.object_id);
        // Wayland permits several resources for the same seat/device. Normalize
        // their duplicate delivery to one stream, using the first live resource.
        if session
            .input_seats
            .iter()
            .filter(|(id, _)| {
                session.logical_seat(**id) == seat
                    && session.object_interfaces.get(id).map(String::as_str)
                        == Some(decoded.interface.as_str())
            })
            .map(|(id, _)| *id)
            .min()
            .is_some_and(|id| id != decoded.object_id)
        {
            return;
        }
        let key = (seat, device != "keyboard", origin == Origin::Human);
        let mut payload = serde_json::json!({"type":kind});
        let mut explicit_surface = None;
        for (spec, arg) in decoded.arg_specs.iter().zip(&decoded.args) {
            let value = match arg {
                DecodedWaylandArg::Int(v) | DecodedWaylandArg::Fixed(v) => serde_json::json!(v),
                DecodedWaylandArg::Uint(v) => serde_json::json!(v),
                DecodedWaylandArg::Object(v) => {
                    if spec.name == "surface" {
                        explicit_surface = *v;
                    }
                    serde_json::json!(v)
                }
                DecodedWaylandArg::Array(v) => serde_json::json!(
                    v.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| u32::from_ne_bytes(*b))
                        .collect::<Vec<_>>()
                ),
                _ => continue,
            };
            let name = match spec.name {
                "surface_x" => "x",
                "surface_y" => "y",
                name => name,
            };
            payload[name] = value;
        }
        if matches!(kind, "button" | "key") {
            let field = if device == "pointer" { "button" } else { "key" };
            if let Some(code) = payload[field].as_u64().and_then(|v| u32::try_from(v).ok()) {
                let held = session.delivered_pressed.entry(key).or_default();
                if payload["state"] == 1 {
                    held.insert(code);
                } else {
                    held.remove(&code);
                }
            }
        }
        if device == "keyboard" && kind == "enter" {
            session.delivered_pressed.insert(
                key,
                payload["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_u64)
                    .filter_map(|v| u32::try_from(v).ok())
                    .collect(),
            );
        }
        let human = origin == Origin::Human;
        let contact = payload["id"].as_i64().and_then(|v| i32::try_from(v).ok());
        let surface = explicit_surface
            .or(target)
            .or_else(|| session.input_surfaces.get(&decoded.object_id).copied())
            .or_else(|| {
                if device == "touch" {
                    contact
                        .and_then(|id| session.touch_contacts.get(&(seat, human, id)).copied())
                        .or_else(|| session.touch_last_surface.get(&(seat, human)).copied())
                } else {
                    session.delivered_focus.get(&key).copied()
                }
            });
        if device == "touch" {
            if let Some(surface) = surface {
                session.touch_last_surface.insert((seat, human), surface);
                if kind == "down"
                    && let Some(id) = contact
                {
                    session.touch_contacts.insert((seat, human, id), surface);
                }
            }
            if kind == "up"
                && let Some(id) = contact
            {
                session.touch_contacts.remove(&(seat, human, id));
            }
            if kind == "cancel" {
                session
                    .touch_contacts
                    .retain(|(s, h, _), _| *s != seat || *h != human);
            }
        }
        if kind == "enter"
            && let Some(surface) = surface
        {
            session.delivered_focus.insert(key, surface);
        }
        if kind == "leave" {
            session.delivered_focus.remove(&key);
        }
        if origin == Origin::Model && device == "pointer" && matches!(kind, "enter" | "leave") {
            if kind == "enter" {
                let old = session
                    .model_constraints
                    .iter()
                    .filter_map(|id| session.input_surfaces.get(id).copied())
                    .filter(|s| Some(*s) != surface)
                    .collect::<BTreeSet<_>>();
                for old in old {
                    let _ = session.update_model_constraints(old, false);
                }
            }
            if let Some(surface) = surface {
                let _ = session.update_model_constraints(surface, kind == "enter");
            }
        }
        let Some(surface) = surface else {
            return;
        };
        let Some(window) = session.frame_tracker.surface_to_window.get(&surface) else {
            return;
        };
        let token = format!(
            "surface:{}:{}:{}",
            client.0,
            surface,
            session
                .object_generations
                .get(&surface)
                .copied()
                .unwrap_or(0)
        );
        let keymap_id = session.keyboard_keymap_text.as_ref().map(|text| {
            use std::hash::{Hash, Hasher};
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hash);
            format!("keymap:{}:{:x}", client.0, hash.finish())
        });
        let window = window.clone();
        if origin == Origin::Model && device == "pointer" {
            let WaylandProxyServer {
                clipboard,
                sessions,
                ..
            } = &mut *inner;
            if let Err(error) = clipboard.model_input(sessions, client, seat, surface, &payload) {
                inner.note_runtime_error(error);
            }
        }
        self.input.publish(serde_json::json!({"seatId":format!("seat:{}:{}",client.0,seat),"keymapId":keymap_id,"windowId":window,"surfaceId":token,"device":device,"origin":if origin==Origin::Human {"human"} else {"model"},"coordinateSpace":"surface-fixed","event":payload}));
    }

    async fn note_runtime_error(&self, error: impl Into<String>) {
        let mut inner = self.inner.lock().await;
        inner.note_runtime_error(error);
    }

    async fn last_runtime_error(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner.last_runtime_error.clone()
    }

    async fn poll_backend_events(
        &self,
        client_id: WaylandClientId,
    ) -> Result<Vec<WaylandBackendEvent>, String> {
        let mut inner = self.inner.lock().await;
        inner.poll_backend_events(client_id)
    }

    async fn backend_reader_stream(
        &self,
        client_id: WaylandClientId,
    ) -> Result<Option<StdUnixStream>, String> {
        let inner = self.inner.lock().await;
        inner.backend_reader_stream(client_id)
    }

    async fn raw_forward_only_flag(
        &self,
        client_id: WaylandClientId,
    ) -> Result<Arc<AtomicBool>, String> {
        let inner = self.inner.lock().await;
        inner.raw_forward_only_flag(client_id)
    }

    async fn prepare_backend_event(
        &self,
        client_id: WaylandClientId,
        message: WaylandWireMessage,
    ) -> Result<WaylandBackendEvent, String> {
        let mut inner = self.inner.lock().await;
        inner.prepare_backend_event(client_id, message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaylandLaunchEnvironment {
    #[serde(rename = "WAYLAND_DISPLAY")]
    pub(crate) wayland_display: String,
    #[serde(rename = "XDG_RUNTIME_DIR")]
    pub(crate) xdg_runtime_dir: String,
    pub(crate) socket_path: String,
    pub(crate) launch_preflight: WaylandLaunchPreflight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaylandLaunchPreflight {
    pub(crate) endpoint_state: String,
    pub(crate) endpoint_exists: bool,
    pub(crate) endpoint_is_unix_socket: bool,
    pub(crate) accept_loop_running: bool,
    /// The MCP cannot inspect the filesystem namespace or policy of a process
    /// launched by its caller, so this is deliberately explicit.
    pub(crate) caller_namespace_access: String,
    pub(crate) render_node_access: String,
    pub(crate) detail: Option<String>,
}

impl WaylandLaunchPreflight {
    fn unavailable(detail: Option<String>) -> Self {
        Self {
            endpoint_state: "unavailable".to_string(),
            endpoint_exists: false,
            endpoint_is_unix_socket: false,
            accept_loop_running: false,
            caller_namespace_access: "not_tested".to_string(),
            render_node_access: "not_tested".to_string(),
            detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaylandConnectionHistoryEntry {
    pub(crate) timestamp_unix_ms: u64,
    pub(crate) event: String,
    pub(crate) client_id: Option<u64>,
    pub(crate) endpoint_state: String,
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaylandBackendSnapshot {
    pub(crate) socket_name: String,
    pub(crate) socket_path: Option<String>,
    pub(crate) backend_socket: String,
    pub(crate) running: bool,
    pub(crate) launch_preflight: WaylandLaunchPreflight,
    pub(crate) session_count: usize,
    pub(crate) registered_globals: Vec<String>,
    pub(crate) registered_intercepts: Vec<String>,
    pub(crate) last_runtime_error: Option<String>,
    pub(crate) connection_history: Vec<WaylandConnectionHistoryEntry>,
    pub(crate) sessions: Vec<WaylandSessionSnapshot>,
}

struct WaylandProxyTransport {
    _runtime_dir: TempDir,
    socket_path: PathBuf,
    env: HashMap<String, String>,
    shutdown: CancellationToken,
    accept_task: JoinHandle<()>,
}

impl WaylandProxyTransport {
    fn try_spawn(state: Arc<WaylandProxyState>) -> Result<Self, String> {
        let runtime_dir = create_proxy_runtime_dir()?;
        let socket_name = env::var(SOCKET_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_PROXY_SOCKET.to_string());
        let socket_path = runtime_dir.path().join(socket_name.clone());
        let listener = UnixListener::bind(&socket_path).map_err(|err| {
            format!(
                "failed to bind Wayland proxy socket {}: {err}",
                socket_path.display()
            )
        })?;
        let runtime_dir_path = runtime_dir.path().display().to_string();
        let env = HashMap::from([
            ("WAYLAND_DISPLAY".to_string(), socket_name),
            ("XDG_RUNTIME_DIR".to_string(), runtime_dir_path),
        ]);
        let shutdown = CancellationToken::new();
        let accept_shutdown = shutdown.clone();
        let accept_task = tokio::spawn(async move {
            run_proxy_accept_loop(listener, state, accept_shutdown).await;
        });
        Ok(Self {
            _runtime_dir: runtime_dir,
            socket_path,
            env,
            shutdown,
            accept_task,
        })
    }

    fn sandbox_env(&self) -> HashMap<String, String> {
        self.env.clone()
    }

    fn launch_environment(&self) -> WaylandLaunchEnvironment {
        WaylandLaunchEnvironment {
            wayland_display: self.env["WAYLAND_DISPLAY"].clone(),
            xdg_runtime_dir: self.env["XDG_RUNTIME_DIR"].clone(),
            socket_path: self.socket_path.display().to_string(),
            launch_preflight: self.launch_preflight(),
        }
    }

    fn launch_preflight(&self) -> WaylandLaunchPreflight {
        let metadata = std::fs::metadata(&self.socket_path);
        let endpoint_exists = metadata.is_ok();
        let endpoint_is_unix_socket = metadata
            .as_ref()
            .is_ok_and(|metadata| metadata.file_type().is_socket());
        let accept_loop_running = !self.shutdown.is_cancelled() && !self.accept_task.is_finished();
        let detail = self.availability_error();
        WaylandLaunchPreflight {
            endpoint_state: if detail.is_none() {
                "ready"
            } else {
                "unavailable"
            }
            .to_string(),
            endpoint_exists,
            endpoint_is_unix_socket,
            accept_loop_running,
            caller_namespace_access: "not_tested".to_string(),
            render_node_access: "not_tested".to_string(),
            detail,
        }
    }

    fn socket_path(&self) -> PathBuf {
        self.socket_path.clone()
    }

    fn availability_error(&self) -> Option<String> {
        if self.shutdown.is_cancelled() {
            return Some("Wayland proxy transport has been shut down".to_string());
        }
        if self.accept_task.is_finished() {
            return Some("Wayland proxy accept loop has stopped".to_string());
        }
        match std::fs::metadata(&self.socket_path) {
            Ok(metadata) if metadata.file_type().is_socket() => None,
            Ok(_) => Some(format!(
                "Wayland proxy endpoint {} is not a Unix socket",
                self.socket_path.display()
            )),
            Err(error) => Some(format!(
                "Wayland proxy endpoint {} is unavailable: {error}",
                self.socket_path.display()
            )),
        }
    }
}

impl Drop for WaylandProxyTransport {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.accept_task.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaylandSessionSnapshot {
    pub(crate) client_id: u64,
    pub(crate) backend_globals: Vec<String>,
    pub(crate) mapped_resource_count: usize,
    pub(crate) tracked_surface_count: usize,
    pub(crate) latest_commit_serial: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WaylandProxyConfig {
    dmabuf_transparent: bool,
    socket_name: String,
    backend_socket: String,
}

impl WaylandProxyConfig {
    fn from_env() -> Self {
        let dmabuf_transparent = match env::var("WAYLAND_MCP_DMABUF_MODE").as_deref() {
            Err(_) | Ok("capture-compatible") => false,
            Ok("transparent") => true,
            Ok(value) => panic!(
                "invalid WAYLAND_MCP_DMABUF_MODE {value:?}: expected capture-compatible or transparent"
            ),
        };
        Self {
            dmabuf_transparent,
            socket_name: env::var(SOCKET_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_PROXY_SOCKET.to_string()),
            backend_socket: env::var(BACKEND_SOCKET_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    env::var("WAYLAND_DISPLAY")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                })
                .unwrap_or_default(),
        }
    }
}

struct WaylandProxyServer {
    clipboard: clipboard::Clipboard,
    config: WaylandProxyConfig,
    running: bool,
    next_client_id: u64,
    last_runtime_error: Option<String>,
    connection_history: VecDeque<WaylandConnectionHistoryEntry>,
    capture_tracking_deadline: Option<Instant>,
    observations: HashMap<u64, (Option<String>, Instant)>,
    input_render_deadlines: HashMap<String, Instant>,
    visual_windows: HashSet<String>,
    next_observation: u64,
    sessions: HashMap<WaylandClientId, WaylandClientSession>,
    registry: WaylandProtocolRegistry,
}

impl WaylandProxyServer {
    fn mark_model_origin(&mut self, window: &str) -> Result<(), String> {
        let targets = self
            .sessions
            .iter()
            .filter(|(_, session)| session.frame_tracker.windows.contains_key(window))
            .flat_map(|(client, session)| {
                session
                    .seat_globals
                    .values()
                    .map(move |seat| (*client, *seat))
            })
            .collect::<HashSet<_>>();
        for (client, seat) in targets {
            self.clipboard
                .switch(&mut self.sessions, client, seat, Origin::Model)?;
        }
        // Explicitly targeted input needs a rendering opportunity even if the
        // host desktop is covering the window. This is not desktop activation.
        self.input_render_deadlines
            .retain(|_, deadline| *deadline > Instant::now());
        self.input_render_deadlines
            .insert(window.into(), Instant::now() + Duration::from_millis(500));
        Ok(())
    }

    fn resize_window(&mut self, request: GuiResizeWindowRequest) -> Result<String, String> {
        if !(1..=8192).contains(&request.width) || !(1..=8192).contains(&request.height) {
            return Err("resizeWindow dimensions must be between 1 and 8192".to_string());
        }
        for session in self.sessions.values_mut() {
            if session
                .frame_tracker
                .windows
                .contains_key(&request.window_id)
            {
                return session.resize_window(&request.window_id, request.width, request.height);
            }
        }
        Err(format!(
            "window `{}` is no longer available",
            request.window_id
        ))
    }
    fn new(config: WaylandProxyConfig) -> Self {
        Self {
            config,
            running: false,
            next_client_id: 0,
            last_runtime_error: None,
            connection_history: VecDeque::new(),
            capture_tracking_deadline: None,
            observations: HashMap::new(),
            input_render_deadlines: HashMap::new(),
            visual_windows: HashSet::new(),
            next_observation: 0,
            sessions: HashMap::new(),
            registry: WaylandProtocolRegistry::default(),
            clipboard: clipboard::Clipboard::default(),
        }
    }

    fn snapshot(&self) -> WaylandBackendSnapshot {
        let mut sessions = self
            .sessions
            .values()
            .map(|session| WaylandSessionSnapshot {
                client_id: session.client_id.0,
                backend_globals: session
                    .backend_globals
                    .iter()
                    .map(|global| global.interface.clone())
                    .collect(),
                mapped_resource_count: session.resource_map.len(),
                tracked_surface_count: session.frame_tracker.surfaces.len(),
                latest_commit_serial: session.frame_tracker.latest_commit_serial(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.client_id);

        let mut registered_globals = self.registry.global_names();
        registered_globals.extend(
            MANUALLY_SUPPORTED_BACKEND_GLOBALS
                .iter()
                .map(|interface| (*interface).to_string()),
        );
        registered_globals.extend(self.sessions.values().flat_map(|session| {
            session
                .backend_globals
                .iter()
                .map(|global| global.interface.clone())
        }));
        registered_globals.sort();
        registered_globals.dedup();

        WaylandBackendSnapshot {
            socket_name: self.config.socket_name.clone(),
            socket_path: None,
            backend_socket: self.config.backend_socket.clone(),
            running: self.running,
            launch_preflight: WaylandLaunchPreflight::unavailable(None),
            session_count: self.sessions.len(),
            registered_globals,
            registered_intercepts: self.registry.intercept_names(),
            last_runtime_error: self.last_runtime_error.clone(),
            connection_history: self.connection_history.iter().cloned().collect(),
            sessions,
        }
    }

    fn register_client_session(&mut self, globals: Vec<WaylandGlobalInfo>) -> WaylandClientId {
        // Diagnostics should describe the newest live client, not a prior
        // short-lived connection which failed before this one was accepted.
        self.last_runtime_error = None;
        self.next_client_id = self.next_client_id.saturating_add(1);
        self.running = true;
        let client_id = WaylandClientId(self.next_client_id);
        let (backend, backend_error) = match WaylandBackendSession::connect(&self.config) {
            Ok(backend) => (Some(backend), None),
            Err(err) => {
                self.last_runtime_error = Some(err.clone());
                (
                    None,
                    Some(format!("backend compositor connection failed: {err}")),
                )
            }
        };
        let backend_globals = backend
            .as_ref()
            .map(|session| filtered_backend_globals(&session.globals))
            .filter(|globals| !globals.is_empty())
            .unwrap_or(globals);
        self.sessions.insert(
            client_id,
            WaylandClientSession::new(client_id, backend_globals, backend),
        );
        self.record_connection_event("accepted", Some(client_id), "ready", backend_error);
        client_id
    }

    fn remove_client_session(&mut self, client_id: WaylandClientId) {
        self.clipboard.disconnect(&mut self.sessions, client_id);
        self.sessions.remove(&client_id);
        if self.sessions.is_empty() {
            self.running = false;
        }
    }

    fn finish_client_session(&mut self, client_id: WaylandClientId, detail: String) {
        self.remove_client_session(client_id);
        self.record_connection_event("closed", Some(client_id), "ready", Some(detail));
    }

    fn note_accept_error(&mut self, error: String) {
        self.record_connection_event(
            "accept_failed",
            None,
            "accept_loop_failed",
            Some(error.clone()),
        );
        self.note_runtime_error(error);
    }

    fn record_connection_event(
        &mut self,
        event: &str,
        client_id: Option<WaylandClientId>,
        endpoint_state: &str,
        detail: Option<String>,
    ) {
        if self.connection_history.len() == MAX_CONNECTION_HISTORY {
            self.connection_history.pop_front();
        }
        self.connection_history
            .push_back(WaylandConnectionHistoryEntry {
                timestamp_unix_ms: unix_time_ms(),
                event: event.to_string(),
                client_id: client_id.map(|client_id| client_id.0),
                endpoint_state: endpoint_state.to_string(),
                detail,
            });
    }

    fn note_runtime_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        trace_wayland_proxy(format_args!("runtime error: {error}"));
        self.last_runtime_error = Some(error);
    }

    fn begin_observation(
        &mut self,
        window: Option<String>,
        duration: Duration,
    ) -> Result<u64, String> {
        self.observations
            .retain(|_, (_, deadline)| *deadline > Instant::now());
        if self.observations.len() >= 32 {
            return Err("observation lease limit reached".into());
        }
        self.next_observation = self
            .next_observation
            .checked_add(1)
            .ok_or("observation IDs exhausted")?;
        self.observations
            .insert(self.next_observation, (window, Instant::now() + duration));
        Ok(self.next_observation)
    }
    fn observing_surface(&self, client: WaylandClientId, surface: u32) -> bool {
        if self
            .capture_tracking_deadline
            .is_some_and(|d| d > Instant::now())
        {
            return true;
        }
        let window = self
            .sessions
            .get(&client)
            .and_then(|s| s.frame_tracker.surface_to_window.get(&surface));
        if window.is_some_and(|w| {
            self.input_render_deadlines
                .get(w)
                .is_some_and(|deadline| *deadline > Instant::now())
        }) {
            return true;
        }
        if window.is_some_and(|w| self.visual_windows.contains(w)) {
            return true;
        }
        self.observations.values().any(|(scope, deadline)| {
            *deadline > Instant::now() && (scope.is_none() || scope.as_ref() == window)
        })
    }

    fn observed_frame_events(
        &mut self,
        client: WaylandClientId,
        now: Instant,
    ) -> Result<Vec<WaylandBackendEvent>, String> {
        let session = self.sessions.get(&client).ok_or("client disconnected")?;
        let mut ready = session
            .frame_callbacks
            .iter()
            .filter_map(|(id, callback)| {
                let surface = session.frame_tracker.surfaces.get(&callback.surface)?;
                if surface.commit_serial <= callback.requested_serial || now < callback.not_before {
                    return None;
                }
                (callback.local || self.observing_surface(client, callback.surface)).then_some(*id)
            })
            .collect::<Vec<_>>();
        // wl_surface.frame callbacks complete in request order, which need not
        // match their numeric object IDs after the client starts reusing IDs.
        ready.sort_by_key(|id| session.object_generations.get(id).copied().unwrap_or(0));
        let session = self
            .sessions
            .get_mut(&client)
            .ok_or("client disconnected")?;
        let mut events = Vec::new();
        for id in ready {
            let callback = session.frame_callbacks.remove(&id).unwrap();
            events.push(encode_local_event(
                session,
                id,
                &GeneratedEvent::WlCallbackDone {
                    callback_data: wayland_timestamp_ms_u32(),
                },
            )?);
            if callback.local {
                events.push(encode_local_event(
                    session,
                    1,
                    &GeneratedEvent::WlDisplayDeleteId { id },
                )?);
                session.remove_object(id);
            } else {
                // The host still owns this callback ID. Send done once, but let
                // its real delete_id release the ID; early reuse would collide
                // with an object that remains alive in the host compositor.
                session.synthetic_frame_done.insert(id);
            }
        }
        Ok(events)
    }
    fn enable_capture_tracking_until(&mut self, deadline: Instant) {
        self.capture_tracking_deadline = Some(deadline);
        for session in self.sessions.values() {
            session.raw_forward_only.store(false, Ordering::Relaxed);
        }
    }

    fn disable_capture_tracking(&mut self) {
        self.capture_tracking_deadline = None;
        for session in self.sessions.values() {
            // Continuous object/buffer tracking is required for trustworthy
            // captures. If requests are forwarded without decoding, clients
            // can destroy and reuse wl_buffer IDs between captures and a later
            // attachment will resolve to stale pixels (observed as Firefox
            // jumping back to its startup logo).
            session.raw_forward_only.store(false, Ordering::Relaxed);
        }
    }

    fn capture_tracking_active(&mut self) -> bool {
        let now = Instant::now();
        self.observations.retain(|_, (_, deadline)| *deadline > now);
        self.input_render_deadlines
            .retain(|_, deadline| *deadline > now);
        if self.capture_tracking_deadline.is_some_and(|d| d <= now) {
            self.disable_capture_tracking();
        }
        !self.observations.is_empty()
            || self.capture_tracking_deadline.is_some()
            || !self.input_render_deadlines.is_empty()
    }

    fn map_resource(
        &mut self,
        client_id: WaylandClientId,
        client_resource_id: u32,
        backend_resource_id: u32,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        session
            .resource_map
            .map(client_resource_id, backend_resource_id);
        Ok(())
    }

    fn track_object_interface(
        &mut self,
        client_id: WaylandClientId,
        object_id: u32,
        interface: &str,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        session.track_object_interface(object_id, interface);
        Ok(())
    }

    fn set_client_event_writer(
        &mut self,
        client_id: WaylandClientId,
        writer: StdUnixStream,
    ) -> Result<ClientWriter, String> {
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        let writer = ClientWriter::new(writer);
        session.client_event_writer = Some(writer.clone());
        Ok(writer)
    }

    fn inject_click(&mut self, request: GuiClickRequest) -> Result<String, String> {
        let windows = self
            .sessions
            .values()
            .flat_map(|session| session.frame_tracker.list_windows())
            .filter(|window| window.mapped)
            .collect::<Vec<_>>();
        let selected = select_window_for_screenshot(&windows, request.window_id.as_deref())?
            .ok_or_else(|| "gui_click has no proxied Wayland window to target".to_string())?;
        let selected_window_id = selected.window_id.clone();
        self.mark_model_origin(&selected_window_id)?;

        for session in self.sessions.values_mut() {
            let Some(target) =
                session.model_pointer_target(&selected_window_id, request.x, request.y)?
            else {
                continue;
            };
            return session.inject_click(target, request.button);
        }

        Err(format!(
            "window `{selected_window_id}` disappeared before gui_click could target it"
        ))
    }

    fn inject_pointer_motion(&mut self, request: GuiPointerMoveRequest) -> Result<String, String> {
        let windows = self
            .sessions
            .values()
            .flat_map(|session| session.frame_tracker.list_windows())
            .filter(|window| window.mapped)
            .collect::<Vec<_>>();
        let selected = select_window_for_screenshot(&windows, request.window_id.as_deref())?
            .ok_or_else(|| "pointer motion has no proxied Wayland window to target".to_string())?;
        let selected_window_id = selected.window_id.clone();
        self.mark_model_origin(&selected_window_id)?;

        for session in self.sessions.values_mut() {
            let Some(target) =
                session.model_pointer_target(&selected_window_id, request.x, request.y)?
            else {
                continue;
            };
            return session.inject_pointer_motion(target);
        }

        Err(format!(
            "window `{selected_window_id}` disappeared before pointer motion could target it"
        ))
    }

    fn emit_wayland_pointer_event(
        &mut self,
        request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String> {
        let windows = self
            .sessions
            .values()
            .flat_map(|session| session.frame_tracker.list_windows())
            .filter(|window| window.mapped)
            .collect::<Vec<_>>();
        let selected = select_window_for_screenshot(&windows, request.window_id.as_deref())?
            .ok_or_else(|| "raw Wayland pointer event has no mapped target window".to_string())?;
        let selected_window_id = selected.window_id.clone();
        self.mark_model_origin(&selected_window_id)?;

        if request.surface_fixed && request.surface_id.is_none() {
            return Err("surface-fixed input requires surfaceId".into());
        }
        for session in self.sessions.values_mut() {
            if let Some(token) = &request.surface_id {
                let parts = token.split(':').collect::<Vec<_>>();
                if parts.len() != 4 || parts[0] != "surface" {
                    return Err("invalid surfaceId".into());
                }
                let client = parts[1].parse::<u64>().map_err(|_| "invalid surfaceId")?;
                if client != session.client_id.0 {
                    continue;
                }
                let surface = parts[2].parse::<u32>().map_err(|_| "invalid surfaceId")?;
                let generation = parts[3].parse::<u64>().map_err(|_| "invalid surfaceId")?;
                if session.object_interfaces.get(&surface).map(String::as_str) != Some("wl_surface")
                    || session.object_generations.get(&surface) != Some(&generation)
                {
                    return Err("stale or destroyed surfaceId".into());
                }
                if session.frame_tracker.surface_to_window.get(&surface)
                    != Some(&selected_window_id)
                {
                    return Err("surfaceId does not belong to target window".into());
                }
                if !request.surface_fixed {
                    return Err("surfaceId requires coordinateSpace: surface-fixed".into());
                }
                let (x, y) = match request.event {
                    GuiWaylandPointerEvent::Enter { x, y, .. }
                    | GuiWaylandPointerEvent::Motion { x, y, .. } => (x, y),
                    _ => (0, 0),
                };
                if i32::try_from(x).is_err() || i32::try_from(y).is_err() {
                    return Err("surface-fixed coordinates must be signed 32-bit integers".into());
                }
                let target = PointerClickTarget {
                    fixed_coords: None,
                    window_id: selected_window_id.clone(),
                    surface_id: surface,
                    screenshot_x: 0,
                    screenshot_y: 0,
                    surface_x: x,
                    surface_y: y,
                };
                return session.emit_wayland_pointer_event(target, request.event, true);
            }
            let target = match &request.event {
                GuiWaylandPointerEvent::Enter { x, y, .. }
                | GuiWaylandPointerEvent::Motion { x, y, .. } => {
                    session.model_pointer_target(&selected_window_id, *x, *y)?
                }
                GuiWaylandPointerEvent::Leave { .. }
                | GuiWaylandPointerEvent::Button { .. }
                | GuiWaylandPointerEvent::Axis { .. }
                | GuiWaylandPointerEvent::AxisSource { .. }
                | GuiWaylandPointerEvent::AxisStop { .. }
                | GuiWaylandPointerEvent::AxisDiscrete { .. }
                | GuiWaylandPointerEvent::AxisValue120 { .. }
                | GuiWaylandPointerEvent::AxisRelativeDirection { .. }
                | GuiWaylandPointerEvent::RelativeMotion { .. }
                | GuiWaylandPointerEvent::Frame => session
                    .frame_tracker
                    .list_windows()
                    .iter()
                    .any(|window| window.window_id == selected_window_id)
                    .then(|| PointerClickTarget {
                        fixed_coords: None,
                        window_id: selected_window_id.clone(),
                        screenshot_x: 0,
                        screenshot_y: 0,
                        surface_id: session
                            .delivered_focus
                            .iter()
                            .find_map(|((_, pointer, human), surface)| {
                                (*pointer
                                    && !*human
                                    && session.frame_tracker.surface_to_window.get(surface)
                                        == Some(&selected_window_id))
                                .then_some(*surface)
                            })
                            .unwrap_or(selected.input_surface_id),
                        surface_x: 0,
                        surface_y: 0,
                    }),
            };
            let Some(target) = target else {
                continue;
            };
            return session.emit_wayland_pointer_event(target, request.event, false);
        }

        Err(format!(
            "window `{selected_window_id}` disappeared before the raw Wayland pointer event could be emitted"
        ))
    }

    fn emit_wayland_touch_event(
        &mut self,
        request: GuiWaylandTouchEventRequest,
    ) -> Result<String, String> {
        self.mark_model_origin(&request.window_id)?;
        let session = self
            .sessions
            .values_mut()
            .find(|s| {
                s.frame_tracker
                    .list_windows()
                    .iter()
                    .any(|w| w.window_id == request.window_id && w.mapped)
            })
            .ok_or("touch target is not mapped")?;
        let surface = if let Some(token) = &request.surface_id {
            session.validate_surface_token(token, &request.window_id)?
        } else {
            session
                .frame_tracker
                .list_windows()
                .iter()
                .find(|w| w.window_id == request.window_id)
                .ok_or("touch target disappeared")?
                .input_surface_id
        };
        session.emit_touch(surface, request.event)
    }

    fn emit_wayland_keyboard_event(
        &mut self,
        request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String> {
        let windows = self
            .sessions
            .values()
            .flat_map(|session| session.frame_tracker.list_windows())
            .filter(|window| window.mapped)
            .collect::<Vec<_>>();
        let selected = select_window_for_screenshot(&windows, request.window_id.as_deref())?
            .ok_or_else(|| "raw Wayland keyboard event has no mapped target window".to_string())?;
        let selected_window_id = selected.window_id.clone();
        self.mark_model_origin(&selected_window_id)?;

        for session in self.sessions.values_mut() {
            if session
                .frame_tracker
                .list_windows()
                .iter()
                .any(|window| window.window_id == selected_window_id)
            {
                let surface = if let Some(token) = request.surface_id.as_ref() {
                    session.validate_surface_token(token, &selected_window_id)?
                } else {
                    selected.input_surface_id
                };
                return session.emit_wayland_keyboard_event(
                    &selected_window_id,
                    surface,
                    request.event,
                );
            }
        }
        Err(format!(
            "window `{selected_window_id}` disappeared before the raw Wayland keyboard event could be emitted"
        ))
    }

    fn keyboard_text_plan(
        &self,
        request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String> {
        let windows = self
            .sessions
            .values()
            .flat_map(|session| session.frame_tracker.list_windows())
            .filter(|window| window.mapped)
            .collect::<Vec<_>>();
        let selected = select_window_for_screenshot(&windows, request.window_id.as_deref())?
            .ok_or_else(|| "typeText has no mapped target window".to_string())?;

        for session in self.sessions.values() {
            if !session
                .frame_tracker
                .list_windows()
                .iter()
                .any(|window| window.window_id == selected.window_id)
            {
                continue;
            }
            let keymap_text = session.keyboard_keymap_text.as_deref().ok_or_else(|| {
                format!(
                    "typeText cannot target window `{}` because its client has not received an XKB keymap",
                    selected.window_id
                )
            })?;
            let keymap_plan = crate::gui_xkb::plan_text(
                keymap_text,
                session.keyboard_layout_group,
                &request.text,
            )?;
            let strokes = keymap_plan
                .strokes
                .into_iter()
                .map(|stroke| GuiKeyboardTextStroke {
                    key: stroke.key,
                    modifiers: stroke.modifiers,
                })
                .collect();
            return Ok(GuiKeyboardTextPlan {
                layout_group: session.keyboard_layout_group,
                restore_mods_depressed: session.keyboard_mods_depressed,
                restore_mods_latched: session.keyboard_mods_latched,
                restore_mods_locked: session.keyboard_mods_locked,
                shift_modifier: keymap_plan.shift_modifier,
                control_modifier: keymap_plan.control_modifier,
                alt_modifier: keymap_plan.alt_modifier,
                logo_modifier: keymap_plan.logo_modifier,
                strokes,
            });
        }

        Err(format!(
            "window `{}` disappeared before its keyboard map could be inspected",
            selected.window_id
        ))
    }

    fn invoke_intercepts(
        &mut self,
        client_id: WaylandClientId,
        hook_request: &GeneratedHookRequest,
        resource_id: u32,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        for intercept in self.registry.intercepts_for(hook_request.id()) {
            intercept.apply(session, resource_id, hook_request);
        }
        Ok(())
    }

    fn invoke_intercepts_by_name(
        &mut self,
        client_id: WaylandClientId,
        request_name: &str,
        resource_id: u32,
        args: &[WaylandInterceptArg],
    ) -> Result<(), String> {
        let generated_args = generated_decoded_args_from_intercept_args(request_name, args)?;
        let hook_request = decode_generated_hook_request(request_name, &generated_args)
            .ok_or_else(|| format!("unknown generated hook request {request_name}"))?;
        self.invoke_intercepts(client_id, &hook_request, resource_id)
    }

    fn decode_request(
        &self,
        client_id: WaylandClientId,
        bytes: &[u8],
    ) -> Result<DecodedWaylandRequest, String> {
        let session = self
            .sessions
            .get(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        decode_wayland_request(session, bytes)
    }

    fn synthetic_backend_globals(&self) -> Vec<WaylandGlobalInfo> {
        self.registry
            .global_specs()
            .into_iter()
            .enumerate()
            .map(|(index, global)| WaylandGlobalInfo {
                name: index as u32 + 1,
                interface: global.interface,
                version: global.version,
            })
            .collect()
    }

    fn ingest_request(
        &mut self,
        client_id: WaylandClientId,
        mut message: WaylandWireMessage,
    ) -> Result<IngestedWaylandRequest, String> {
        let capture_tracking_active = self.capture_tracking_active();
        let raw_forward_only = self
            .sessions
            .get(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?
            .raw_forward_only
            .load(Ordering::Relaxed)
            && !capture_tracking_active;

        let request = match self.decode_request(client_id, &message.bytes) {
            Ok(request) => Some(request),
            Err(err) => {
                let header = decode_wayland_header(&message.bytes)?;
                let interface = self
                    .sessions
                    .get(&client_id)
                    .and_then(|session| session.interface_for_object(header.object_id));
                if !matches!(interface, Some("wl_shm" | "wl_shm_pool")) {
                    return Err(format!("refusing undecodable client request: {err}"));
                }
                None
            }
        };
        let local_frame_callback = request.as_ref().is_some_and(|request| {
            matches!(
                request.implemented_request,
                Some(GeneratedImplementedRequest::WlSurfaceFrame { .. })
            ) && self.observing_surface(client_id, request.object_id)
        });
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        if let Some(request) = request.as_ref() {
            validate_registry_bind(session, request)?;
            reassociate_client_fds(session, request, &mut message.fds);
        } else {
            apply_undecoded_client_tracking(session, &mut message)?;
        }
        if session.pending_client_fds.len() + message.fds.len() > 256 {
            return Err("pending client descriptor limit exceeded".into());
        }
        // The proxy may suggest a test size without the host compositor having
        // issued that configure serial. Consume its acknowledgement locally.
        let synthetic_ack = request.as_ref().is_some_and(|request| {
            if let Some(GeneratedTrackedRequest::XdgSurfaceAckConfigure { serial }) =
                request.tracked_request.as_ref()
            {
                session
                    .synthetic_configures
                    .remove(&(request.object_id, *serial))
            } else {
                false
            }
        });
        let tracking_fds = duplicate_fds(message.fds.as_slice())?;
        let clipboard_consumed = if let Some(request) = request.as_ref() {
            self.clipboard
                .request(&mut self.sessions, client_id, request, &mut message)?
        } else {
            false
        };
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or("client disconnected")?;
        if let Some(request) = request.as_ref() {
            trace_wayland_proxy(format_args!(
                "client {} -> {}#{}.{} fds={}",
                client_id.0,
                request.interface,
                request.object_id,
                request.request_name,
                message.fds.len()
            ));
            apply_object_tracking(session, &self.registry, request)?;
            if let Some(GeneratedImplementedRequest::WlSurfaceFrame { callback }) =
                request.implemented_request.as_ref()
            {
                if session.frame_callbacks.len() + session.synthetic_frame_done.len() >= 4096 {
                    return Err("frame callback limit reached".into());
                }
                let requested_serial = session
                    .frame_tracker
                    .surfaces
                    .get(&request.object_id)
                    .map_or(0, |surface| surface.commit_serial);
                session.frame_callbacks.insert(
                    *callback,
                    TrackedFrameCallback {
                        surface: request.object_id,
                        requested_serial,
                        local: local_frame_callback,
                        not_before: Instant::now()
                            + Duration::from_millis(if local_frame_callback { 16 } else { 100 }),
                    },
                );
            }
            apply_window_tracking(session, request)?;
            apply_dmabuf_tracking(session, request, tracking_fds.as_slice())?;
            apply_syncobj_tracking(session, request, tracking_fds.as_slice())?;
            if let Some(hook_request) = request.hook_request.as_ref() {
                session
                    .frame_tracker
                    .colors
                    .request(request.object_id, hook_request);
                for intercept in self.registry.intercepts_for(hook_request.id()) {
                    intercept.apply(session, request.object_id, hook_request);
                }
            }
        }
        let mut backend_events = if raw_forward_only {
            Vec::new()
        } else if let Some(request) = request.as_ref() {
            local_protocol_response_events(session, request)?
        } else {
            Vec::new()
        };
        let mut backend_globals = session.backend_globals.clone();
        if backend_events.is_empty()
            && !synthetic_ack
            && !clipboard_consumed
            && !local_frame_callback
            && let Some(backend) = session.backend.as_mut()
        {
            if let Some(request) = request.as_ref() {
                backend.track_request_objects(&self.registry.generated, request);
            }
            if let Some(request) = request.as_ref()
                && !message.fds.is_empty()
            {
                trace_wayland_proxy(format_args!(
                    "client {} forwarding {}#{}.{} fds={}",
                    client_id.0,
                    request.interface,
                    request.object_id,
                    request.request_name,
                    message.fds.len()
                ));
            }
            let mut forwarded = message.bytes.clone();
            if let Some(request) = &request {
                let (_, spec) = self
                    .registry
                    .request_by_id(request.request_id)
                    .ok_or("request metadata missing during translation")?;
                rewrite_wire_object_ids(&mut forwarded, spec.args, false, |id, _| {
                    session.resource_map.upstream_id(id)
                })?;
            } else {
                let id = u32::from_ne_bytes(forwarded[..4].try_into().unwrap());
                forwarded[..4]
                    .copy_from_slice(&session.resource_map.upstream_id(id)?.to_ne_bytes());
            }
            let message_upstream = WaylandWireMessage {
                bytes: forwarded,
                fds: duplicate_fds(&message.fds)?,
            };
            backend.forward_raw_request(&message_upstream)?;
            if let Some(request) = request.as_ref()
                && find_generated_request_by_opcode(&request.interface, request.opcode)
                    .is_some_and(|(_, spec)| spec.destructor)
            {
                let upstream = u32::from_ne_bytes(message_upstream.bytes[..4].try_into().unwrap());
                backend.object_interfaces.remove(&upstream);
            }

            if let Some(request) = request.as_ref()
                && !message.fds.is_empty()
            {
                trace_wayland_proxy(format_args!(
                    "client {} forwarded {}#{}.{} fds={}",
                    client_id.0,
                    request.interface,
                    request.object_id,
                    request.request_name,
                    message.fds.len()
                ));
            }
            backend_globals = filtered_backend_globals(&backend.globals);
        }
        if let Some(request) = &request
            && matches!(
                request.implemented_request,
                Some(GeneratedImplementedRequest::WlSurfaceDestroy)
            )
        {
            let callbacks = session
                .frame_callbacks
                .iter()
                .filter_map(|(id, callback)| {
                    (callback.surface == request.object_id).then_some((*id, callback.local))
                })
                .collect::<Vec<_>>();
            for (id, local) in callbacks {
                session.frame_callbacks.remove(&id);
                if local {
                    // The host never saw this resource, so only we can release
                    // it when its surface is destroyed before the next commit.
                    backend_events.push(encode_local_event(
                        session,
                        1,
                        &GeneratedEvent::WlDisplayDeleteId { id },
                    )?);
                }
            }
        }
        for event in &backend_events {
            if let Some(decoded) = event.decoded.as_ref() {
                apply_client_event_tracking(session, decoded)?;
            }
        }
        if let Some(request) = &request
            && find_generated_request_by_opcode(&request.interface, request.opcode)
                .is_some_and(|(_, spec)| spec.destructor)
        {
            session.remove_object(request.object_id);
        }
        session.backend_globals = backend_globals;
        session.raw_forward_only.store(false, Ordering::Relaxed);
        Ok(IngestedWaylandRequest {
            request,
            backend_events,
        })
    }

    fn poll_backend_events(
        &mut self,
        client_id: WaylandClientId,
    ) -> Result<Vec<WaylandBackendEvent>, String> {
        let (backend_events, backend_globals) = {
            let session = self
                .sessions
                .get_mut(&client_id)
                .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
            let Some(backend) = session.backend.as_mut() else {
                return Ok(Vec::new());
            };
            (
                backend.drain_pending_events()?,
                filtered_backend_globals(&backend.globals),
            )
        };

        let mut forwarded = Vec::new();
        for mut event in backend_events {
            if let Some(decoded) = event.decoded.as_ref()
                && self
                    .clipboard
                    .host_event(&mut self.sessions, client_id, decoded)?
            {
                continue;
            }
            let session = self
                .sessions
                .get_mut(&client_id)
                .ok_or("client disconnected")?;
            event.decoded = event
                .decoded
                .as_ref()
                .map(|decoded| translate_upstream_event(session, decoded, &mut event.encoded))
                .transpose()?;
            if let Some(decoded) = event.decoded.as_ref() {
                if matches!(
                    decoded.generated_event,
                    GeneratedEvent::WlCallbackDone { .. }
                ) {
                    session.frame_callbacks.remove(&decoded.object_id);
                    if session.synthetic_frame_done.contains(&decoded.object_id) {
                        continue;
                    }
                }
                if !self.config.dmabuf_transparent {
                    if is_unsupported_dmabuf_advertisement(decoded) {
                        continue;
                    }
                    rewrite_dmabuf_feedback_event(session, decoded, &mut event.encoded)?;
                }
                rewrite_registry_version(decoded, &mut event.encoded)?;
                rewrite_pointer_seat_capabilities(decoded, &mut event.encoded)?;
                apply_client_event_tracking(session, decoded)?;
            }
            forwarded.push(event);
        }
        self.sessions
            .get_mut(&client_id)
            .ok_or("client disconnected")?
            .backend_globals = backend_globals;
        let backend_events = forwarded;
        Ok(backend_events)
    }

    fn backend_reader_stream(
        &self,
        client_id: WaylandClientId,
    ) -> Result<Option<StdUnixStream>, String> {
        let session = self
            .sessions
            .get(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        session
            .backend
            .as_ref()
            .map(|backend| {
                backend
                    .stream
                    .try_clone()
                    .map_err(|err| format!("failed to clone Wayland backend stream: {err}"))
            })
            .transpose()
    }

    fn raw_forward_only_flag(&self, client_id: WaylandClientId) -> Result<Arc<AtomicBool>, String> {
        let session = self
            .sessions
            .get(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        Ok(Arc::clone(&session.raw_forward_only))
    }

    fn prepare_backend_event(
        &mut self,
        client_id: WaylandClientId,
        mut message: WaylandWireMessage,
    ) -> Result<WaylandBackendEvent, String> {
        let (decoded, backend_globals) = {
            let session = self
                .sessions
                .get_mut(&client_id)
                .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
            let Some(backend) = session.backend.as_mut() else {
                return Ok(WaylandBackendEvent {
                    suppressed: false,
                    encoded: message,
                    decoded: None,
                });
            };
            let decoded = decode_wayland_event(&backend.object_interfaces, &message.bytes);
            if let Ok(event) = decoded.as_ref() {
                reassociate_backend_fds(backend, event, &mut message.fds);
                trace_wayland_proxy(format_args!(
                    "backend -> {}#{}.{} fds={}",
                    event.interface,
                    event.object_id,
                    event.event_name,
                    message.fds.len()
                ));
                backend.apply_event(event);
            } else {
                reassociate_undecoded_backend_fds(backend, &message.bytes, &mut message.fds)?;
                trace_undecoded_wayland_message("backend", &message, decoded.as_ref().err());
            }
            let decoded = decoded.ok();
            (decoded, filtered_backend_globals(&backend.globals))
        };

        if let Some(event) = decoded.as_ref() {
            if self
                .clipboard
                .host_event(&mut self.sessions, client_id, event)?
            {
                return Ok(WaylandBackendEvent {
                    suppressed: true,
                    encoded: message,
                    decoded: None,
                });
            }
            if matches!(
                event.interface.as_str(),
                "wl_pointer" | "wl_keyboard" | "wl_touch" | "zwp_relative_pointer_v1"
            ) && !matches!(
                event.event_name.as_str(),
                "wl_keyboard.keymap" | "wl_keyboard.repeat_info"
            ) {
                let seat = self
                    .sessions
                    .get(&client_id)
                    .and_then(|s| {
                        s.input_seats
                            .get(&event.object_id)
                            .and_then(|seat| s.seat_globals.get(seat))
                    })
                    .copied();
                if let Some(seat) = seat {
                    self.clipboard
                        .switch(&mut self.sessions, client_id, seat, Origin::Human)?;
                }
            }
        }
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or("client disconnected")?;
        let decoded = decoded
            .as_ref()
            .map(|decoded| translate_upstream_event(session, decoded, &mut message))
            .transpose()?;
        if let Some(decoded) = decoded.as_ref() {
            if matches!(
                decoded.generated_event,
                GeneratedEvent::WlCallbackDone { .. }
            ) {
                session.frame_callbacks.remove(&decoded.object_id);
                if session.synthetic_frame_done.contains(&decoded.object_id) {
                    return Ok(WaylandBackendEvent {
                        suppressed: true,
                        encoded: message,
                        decoded: None,
                    });
                }
            }
            if matches!(decoded.generated_event, GeneratedEvent::WlBufferRelease) {
                let id = decoded.object_id;
                if session.buffer_leases.get(&id).copied().unwrap_or(0) > 0 {
                    session.deferred_releases.insert(id, message);
                    return Ok(WaylandBackendEvent {
                        suppressed: true,
                        encoded: WaylandWireMessage {
                            bytes: Vec::new(),
                            fds: Vec::new(),
                        },
                        decoded: None,
                    });
                }
                session.released_buffers.insert(id);
            }
            track_keyboard_keymap(session, decoded, &message.fds)?;
            if !self.config.dmabuf_transparent {
                rewrite_dmabuf_feedback_event(session, decoded, &mut message)?;
            }
            rewrite_registry_version(decoded, &mut message)?;
            rewrite_pointer_seat_capabilities(decoded, &mut message)?;
            apply_client_event_tracking(session, decoded)?;
        }
        session.backend_globals = backend_globals;
        let suppressed = !self.config.dmabuf_transparent
            && decoded
                .as_ref()
                .is_some_and(is_unsupported_dmabuf_advertisement);
        Ok(WaylandBackendEvent {
            suppressed,
            encoded: message,
            decoded,
        })
    }
}

fn rewrite_wire_object_ids(
    bytes: &mut [u8],
    specs: &[GeneratedArgSpec],
    special_delete: bool,
    mut translate: impl FnMut(u32, bool) -> Result<u32, String>,
) -> Result<(), String> {
    let header = decode_wayland_header(bytes)?;
    let id = translate(header.object_id, false)?;
    bytes[..4].copy_from_slice(&id.to_ne_bytes());
    let mut offset = 8;
    for spec in specs {
        match spec.kind {
            GeneratedArgKind::String | GeneratedArgKind::Array => {
                let length = read_u32_arg(bytes, header.size, &mut offset)? as usize;
                offset = offset
                    .checked_add(pad_to_4(length))
                    .ok_or("object translation length overflow")?;
                if offset > header.size as usize {
                    return Err("invalid variable argument during object translation".into());
                }
            }
            GeneratedArgKind::Fd => {}
            kind => {
                let start = offset;
                let old = read_u32_arg(bytes, header.size, &mut offset)?;
                if matches!(kind, GeneratedArgKind::Object | GeneratedArgKind::NewId)
                    || (special_delete && spec.name == "id")
                {
                    let new = if old == 0 {
                        0
                    } else {
                        translate(old, kind == GeneratedArgKind::NewId)?
                    };
                    bytes[start..offset].copy_from_slice(&new.to_ne_bytes());
                }
            }
        }
    }
    Ok(())
}

fn translate_upstream_event(
    session: &mut WaylandClientSession,
    original: &DecodedWaylandEvent,
    message: &mut WaylandWireMessage,
) -> Result<DecodedWaylandEvent, String> {
    rewrite_wire_object_ids(
        &mut message.bytes,
        original.arg_specs,
        original.event_name == "wl_display.delete_id",
        |id, new| {
            if new {
                session.resource_map.allocate_server_id(Some(id))
            } else {
                session.resource_map.downstream_id(id)
            }
        },
    )?;
    let mut event = original.clone();
    event.object_id = u32::from_ne_bytes(message.bytes[..4].try_into().unwrap());
    event.args = decode_wayland_args(&message.bytes, event.size, event.arg_specs)?;
    let args = event
        .args
        .iter()
        .map(GeneratedDecodedArg::from_decoded)
        .collect::<Vec<_>>();
    event.generated_event = decode_generated_event_by_opcode(&event.interface, event.opcode, &args)
        .ok_or("translated event could not be decoded")?;
    Ok(event)
}

fn local_protocol_response_events(
    session: &mut WaylandClientSession,
    request: &DecodedWaylandRequest,
) -> Result<Vec<WaylandBackendEvent>, String> {
    match request.implemented_request.as_ref() {
        Some(GeneratedImplementedRequest::WlDisplayGetRegistry { .. })
            if session.backend.is_some() =>
        {
            Ok(Vec::new())
        }
        Some(GeneratedImplementedRequest::WlDisplayGetRegistry { registry }) => {
            session.mark_local_registry(*registry);
            session
                .backend_globals
                .iter()
                .map(|global| {
                    encode_local_event(
                        session,
                        *registry,
                        &GeneratedEvent::WlRegistryGlobal {
                            name: global.name,
                            interface: Some(global.interface.clone()),
                            version: global.version,
                        },
                    )
                })
                .collect()
        }
        Some(GeneratedImplementedRequest::WlDisplaySync { callback }) => {
            if session.backend.is_some() {
                Ok(Vec::new())
            } else {
                session.mark_local_callback(*callback);
                Ok(vec![
                    encode_local_event(
                        session,
                        *callback,
                        &GeneratedEvent::WlCallbackDone { callback_data: 0 },
                    )?,
                    encode_local_event(
                        session,
                        1,
                        &GeneratedEvent::WlDisplayDeleteId { id: *callback },
                    )?,
                ])
            }
        }
        Some(GeneratedImplementedRequest::WlFixesDestroyRegistry {
            registry: Some(registry_id),
        }) if session.is_local_registry(*registry_id) => Ok(vec![encode_local_event(
            session,
            1,
            &GeneratedEvent::WlDisplayDeleteId { id: *registry_id },
        )?]),
        _ => Ok(Vec::new()),
    }
}

fn apply_client_event_tracking(
    session: &mut WaylandClientSession,
    event: &DecodedWaylandEvent,
) -> Result<(), String> {
    if let GeneratedEvent::ZwpLinuxDmabufFeedbackV1MainDevice { device } = &event.generated_event
        && device.len() == std::mem::size_of::<libc::dev_t>()
    {
        let dev = libc::dev_t::from_ne_bytes(device.as_slice().try_into().unwrap());
        session.dmabuf_main_device = Some((libc::major(dev), libc::minor(dev)));
    }
    let version = session
        .object_versions
        .get(&event.object_id)
        .copied()
        .unwrap_or(1);
    apply_object_tracking_from_specs(session, event.arg_specs, &event.args, version);
    if let GeneratedEvent::WlDisplayDeleteId { id } = &event.generated_event {
        session.remove_object(*id);
    }
    apply_backend_event_tracking(session, event)
}

fn track_keyboard_keymap(
    session: &mut WaylandClientSession,
    event: &DecodedWaylandEvent,
    fds: &[OwnedFd],
) -> Result<(), String> {
    let GeneratedEvent::WlKeyboardKeymap { format, size, .. } = &event.generated_event else {
        return Ok(());
    };
    if *format != 1 {
        return Err(format!(
            "wl_keyboard.keymap used unsupported format {format}; expected XKB V1"
        ));
    }
    let size = usize::try_from(*size)
        .map_err(|_| "wl_keyboard.keymap size does not fit memory".to_string())?;
    const MAX_KEYMAP_SIZE: usize = 16 * 1024 * 1024;
    if size == 0 || size > MAX_KEYMAP_SIZE {
        return Err(format!(
            "wl_keyboard.keymap size {size} is outside the supported 1..={MAX_KEYMAP_SIZE} range"
        ));
    }
    let fd = fds
        .first()
        .ok_or_else(|| "wl_keyboard.keymap was missing its file descriptor".to_string())?;
    let file = std::fs::File::from(duplicate_fd(fd)?);
    let mut bytes = vec![0u8; size];
    let mut offset = 0usize;
    while offset < bytes.len() {
        let read = file
            .read_at(&mut bytes[offset..], offset as u64)
            .map_err(|err| format!("failed to read wl_keyboard.keymap: {err}"))?;
        if read == 0 {
            return Err(format!(
                "short wl_keyboard.keymap read: {offset} of {} bytes",
                bytes.len()
            ));
        }
        offset += read;
    }
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    session.keyboard_keymap_text = Some(
        String::from_utf8(bytes)
            .map_err(|err| format!("wl_keyboard.keymap was not valid UTF-8: {err}"))?,
    );
    Ok(())
}

fn reassociate_client_fds(
    session: &mut WaylandClientSession,
    request: &DecodedWaylandRequest,
    fds: &mut Vec<OwnedFd>,
) {
    reassociate_fds(
        "client",
        &mut session.pending_client_fds,
        &request.args,
        fds,
    );
}

fn apply_undecoded_client_tracking(
    session: &mut WaylandClientSession,
    message: &mut WaylandWireMessage,
) -> Result<(), String> {
    let header = decode_wayland_header(&message.bytes)?;
    let Some(interface) = session
        .interface_for_object(header.object_id)
        .map(str::to_owned)
    else {
        // The recvmsg chunk can attach an FD needed by a later message even
        // when this first message's object/interface is unknown to us.
        reassociate_undecoded_client_fds(session, &mut message.fds, 0);
        return Ok(());
    };
    if !(interface == "wl_shm" && header.opcode == 0) {
        // Ancillary FDs can be attached to the first byte of a batch whose
        // FD-bearing request comes later. None of the other manual requests
        // below takes an FD, including wl_shm_pool.resize.
        reassociate_undecoded_client_fds(session, &mut message.fds, 0);
    }
    match (interface.as_str(), header.opcode) {
        ("wl_shm", 0) => {
            let mut offset = 8;
            let pool_id = read_u32_arg(&message.bytes, header.size, &mut offset)?;
            let size = read_u32_arg(&message.bytes, header.size, &mut offset)?;
            reassociate_undecoded_client_fds(session, &mut message.fds, 1);
            let fd = message
                .fds
                .first()
                .ok_or_else(|| "wl_shm.create_pool was missing its fd".to_string())?;
            session
                .frame_tracker
                .note_shm_pool_created(pool_id, duplicate_fd(fd)?, size as usize);
            track_undecoded_client_object(session, pool_id, "wl_shm_pool", "wl_shm.create_pool");
        }
        ("wl_shm_pool", 0) => {
            let mut offset = 8;
            let buffer_id = read_u32_arg(&message.bytes, header.size, &mut offset)?;
            let buffer_offset = read_u32_arg(&message.bytes, header.size, &mut offset)?;
            let width = read_u32_arg(&message.bytes, header.size, &mut offset)? as i32;
            let height = read_u32_arg(&message.bytes, header.size, &mut offset)? as i32;
            let stride = read_u32_arg(&message.bytes, header.size, &mut offset)? as i32;
            let format = read_u32_arg(&message.bytes, header.size, &mut offset)?;
            session
                .frame_tracker
                .note_shm_buffer_created(ShmBufferSpec {
                    pool_id: header.object_id,
                    buffer_id,
                    offset: buffer_offset,
                    width,
                    height,
                    stride,
                    format,
                })?;
            track_undecoded_client_object(
                session,
                buffer_id,
                "wl_buffer",
                "wl_shm_pool.create_buffer",
            );
        }
        ("wl_shm_pool", 1) => {
            session
                .frame_tracker
                .note_shm_pool_destroyed(header.object_id);
            session.remove_object(header.object_id);
            if let Some(backend) = session.backend.as_mut() {
                backend.object_interfaces.remove(&header.object_id);
            }
        }
        ("wl_shm_pool", 2) => {
            let mut offset = 8;
            let size = read_u32_arg(&message.bytes, header.size, &mut offset)?;
            session
                .frame_tracker
                .note_shm_pool_resized(header.object_id, size as usize)?;
        }
        _ => {
            return Err(format!(
                "unknown manual request {interface}.{}",
                header.opcode
            ));
        }
    }
    let expected_size = match (interface.as_str(), header.opcode) {
        ("wl_shm", 0) => 16,
        ("wl_shm_pool", 0) => 32,
        ("wl_shm_pool", 1) => 8,
        ("wl_shm_pool", 2) => 12,
        _ => unreachable!(),
    };
    if header.size != expected_size {
        return Err("invalid manual request size".into());
    }
    Ok(())
}

fn reassociate_undecoded_client_fds(
    session: &mut WaylandClientSession,
    fds: &mut Vec<OwnedFd>,
    expected_fds: usize,
) {
    if expected_fds == 0 {
        if !fds.is_empty() {
            session.pending_client_fds.extend(fds.drain(..));
        }
        return;
    }
    let immediate_fds = fds.len();
    let pending_before = session.pending_client_fds.len();
    let mut associated_fds = Vec::with_capacity(expected_fds);
    while associated_fds.len() < expected_fds {
        let Some(fd) = session.pending_client_fds.pop_front() else {
            break;
        };
        associated_fds.push(fd);
    }
    while associated_fds.len() < expected_fds && !fds.is_empty() {
        associated_fds.push(fds.remove(0));
    }
    if !fds.is_empty() {
        session.pending_client_fds.extend(fds.drain(..));
    }
    trace_wayland_proxy(format_args!(
        "client undecoded fd reassociation expected={expected_fds} immediate={immediate_fds} pending_before={pending_before} associated={} pending_after={}",
        associated_fds.len(),
        session.pending_client_fds.len()
    ));
    *fds = associated_fds;
}

fn track_undecoded_client_object(
    session: &mut WaylandClientSession,
    object_id: u32,
    interface: &'static str,
    request_name: &'static str,
) {
    session.track_object_interface(object_id, interface);
    if let Some(backend) = session.backend.as_mut() {
        backend
            .object_interfaces
            .insert(object_id, interface.to_string());
    }
    trace_wayland_proxy(format_args!(
        "undecoded object track {object_id} -> {interface} via {request_name}"
    ));
}

fn reassociate_backend_fds(
    backend: &mut WaylandBackendSession,
    event: &DecodedWaylandEvent,
    fds: &mut Vec<OwnedFd>,
) {
    reassociate_fds(
        "backend",
        &mut backend.pending_backend_fds,
        &event.args,
        fds,
    );
}

fn reassociate_undecoded_backend_fds(
    backend: &mut WaylandBackendSession,
    bytes: &[u8],
    fds: &mut Vec<OwnedFd>,
) -> Result<(), String> {
    if fds.is_empty() {
        return Ok(());
    }
    let header = decode_wayland_header(bytes)?;
    let interface = backend.object_interfaces.get(&header.object_id);
    if interface.is_some_and(|interface| interface == "wl_shm") && header.opcode == 0 {
        // wl_shm.format carries no FD, but libwayland may flush it in the same
        // sendmsg whose SCM_RIGHTS belongs to a following DMA-BUF feedback
        // format_table. Preserve that FD until the decoded FD-bearing event
        // claims it. Forwarding it on wl_shm.format makes the actual
        // format_table arrive without an FD and can prevent Firefox startup.
        reassociate_fds(
            "backend-manual-wl_shm",
            &mut backend.pending_backend_fds,
            &[],
            fds,
        );
        return Ok(());
    }
    Err(format!(
        "undecoded backend event {}#{} opcode {} unexpectedly carried {} fd(s)",
        interface.map(String::as_str).unwrap_or("unknown"),
        header.object_id,
        header.opcode,
        fds.len()
    ))
}

fn reassociate_fds(
    direction: &str,
    pending_fds: &mut VecDeque<OwnedFd>,
    args: &[DecodedWaylandArg],
    fds: &mut Vec<OwnedFd>,
) {
    let expected_fds = args
        .iter()
        .filter(|arg| matches!(arg, DecodedWaylandArg::Fd))
        .count();
    if expected_fds == 0 {
        let queued = fds.len();
        if queued > 0 {
            let pending_before = pending_fds.len();
            pending_fds.extend(fds.drain(..));
            trace_wayland_proxy(format_args!(
                "{direction} fd queue expected=0 queued={queued} pending_before={pending_before} pending_after={}",
                pending_fds.len()
            ));
        }
        return;
    }

    let immediate_fds = fds.len();
    let pending_before = pending_fds.len();
    let mut associated_fds = Vec::with_capacity(expected_fds);
    while associated_fds.len() < expected_fds {
        let Some(fd) = pending_fds.pop_front() else {
            break;
        };
        associated_fds.push(fd);
    }
    while associated_fds.len() < expected_fds && !fds.is_empty() {
        associated_fds.push(fds.remove(0));
    }
    if !fds.is_empty() {
        pending_fds.extend(fds.drain(..));
    }
    trace_wayland_proxy(format_args!(
        "{direction} fd reassociation expected={expected_fds} immediate={immediate_fds} pending_before={pending_before} associated={} pending_after={}",
        associated_fds.len(),
        pending_fds.len()
    ));
    *fds = associated_fds;
}

fn encode_local_event(
    session: &WaylandClientSession,
    sender_object_id: u32,
    event: &GeneratedEvent,
) -> Result<WaylandBackendEvent, String> {
    let bytes = encode_generated_event(sender_object_id, event)?;
    let decoded = decode_wayland_event(&session.object_interfaces, &bytes)?;
    Ok(WaylandBackendEvent {
        suppressed: false,
        encoded: WaylandWireMessage {
            bytes,
            fds: Vec::new(),
        },
        decoded: Some(decoded),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WaylandClientId(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedWaylandRequest {
    pub(crate) object_id: u32,
    pub(crate) size: u16,
    pub(crate) opcode: u16,
    pub(crate) interface: String,
    pub(crate) request_name: String,
    pub(crate) request_id: GeneratedRequestId,
    pub(crate) implemented_request: Option<GeneratedImplementedRequest>,
    pub(crate) hook_request: Option<GeneratedHookRequest>,
    pub(crate) tracked_request: Option<GeneratedTrackedRequest>,
    pub(crate) args: Vec<DecodedWaylandArg>,
}

#[derive(Debug)]
struct IngestedWaylandRequest {
    request: Option<DecodedWaylandRequest>,
    backend_events: Vec<WaylandBackendEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecodedWaylandEvent {
    object_id: u32,
    size: u16,
    opcode: u16,
    interface: String,
    event_name: String,
    arg_specs: &'static [GeneratedArgSpec],
    generated_event: GeneratedEvent,
    args: Vec<DecodedWaylandArg>,
}

#[derive(Debug)]
struct WaylandBackendEvent {
    suppressed: bool,
    encoded: WaylandWireMessage,
    decoded: Option<DecodedWaylandEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodedWaylandArg {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(Option<String>),
    Object(Option<u32>),
    NewId(u32),
    Array(Vec<u8>),
    Fd,
}

struct TrackedFrameCallback {
    surface: u32,
    requested_serial: u64,
    not_before: Instant,
    local: bool,
}

struct WaylandClientSession {
    client_id: WaylandClientId,
    backend_globals: Vec<WaylandGlobalInfo>,
    backend: Option<WaylandBackendSession>,
    client_event_writer: Option<ClientWriter>,
    resource_map: WaylandResourceMap,
    object_interfaces: HashMap<u32, String>,
    object_versions: HashMap<u32, u32>,
    object_generations: HashMap<u32, u64>,
    next_generation: u64,
    delivered_focus: HashMap<(u32, bool, bool), u32>,
    delivered_pressed: HashMap<(u32, bool, bool), BTreeSet<u32>>,
    input_seats: HashMap<u32, u32>,
    input_surfaces: HashMap<u32, u32>,
    released_buffers: HashSet<u32>,
    buffer_leases: HashMap<u32, u32>,
    deferred_releases: HashMap<u32, WaylandWireMessage>,
    model_constraints: BTreeSet<u32>,
    constraint_lifetimes: HashMap<u32, u32>,
    constraint_regions: HashMap<u32, Option<Vec<DamageRect>>>,
    pending_constraint_regions: HashMap<u32, Option<Vec<DamageRect>>>,
    spent_constraints: BTreeSet<u32>,
    touch_contacts: HashMap<(u32, bool, i32), u32>,
    touch_last_surface: HashMap<(u32, bool), u32>,
    seat_globals: HashMap<u32, u32>,
    local_registry_ids: HashSet<u32>,
    local_callback_ids: HashSet<u32>,
    frame_callbacks: HashMap<u32, TrackedFrameCallback>,
    synthetic_frame_done: HashSet<u32>,
    pending_client_fds: VecDeque<OwnedFd>,
    dmabuf_feedback_index_maps: HashMap<u32, Vec<Option<u16>>>,
    dmabuf_main_device: Option<(u32, u32)>,
    frame_tracker: WaylandFrameTracker,
    raw_forward_only: Arc<AtomicBool>,
    next_synthetic_serial: u32,
    synthetic_serials: HashSet<u32>,
    synthetic_configures: HashSet<(u32, u32)>,
    keyboard_keymap_text: Option<String>,
    keyboard_layout_group: u32,
    keyboard_mods_depressed: u32,
    keyboard_mods_latched: u32,
    keyboard_mods_locked: u32,
}

impl WaylandClientSession {
    fn validate_surface_token(&self, token: &str, window: &str) -> Result<u32, String> {
        let parts = token.split(':').collect::<Vec<_>>();
        if parts.len() != 4 || parts[0] != "surface" {
            return Err("invalid surfaceId".into());
        }
        let client = parts[1].parse::<u64>().map_err(|_| "invalid surfaceId")?;
        let surface = parts[2].parse::<u32>().map_err(|_| "invalid surfaceId")?;
        let generation = parts[3].parse::<u64>().map_err(|_| "invalid surfaceId")?;
        if client != self.client_id.0
            || self.object_interfaces.get(&surface).map(String::as_str) != Some("wl_surface")
            || self.object_generations.get(&surface) != Some(&generation)
        {
            return Err("stale or destroyed surfaceId".into());
        }
        if self
            .frame_tracker
            .surface_to_window
            .get(&surface)
            .map(String::as_str)
            != Some(window)
        {
            return Err("surfaceId does not belong to target window".into());
        }
        Ok(surface)
    }

    fn new(
        client_id: WaylandClientId,
        backend_globals: Vec<WaylandGlobalInfo>,
        backend: Option<WaylandBackendSession>,
    ) -> Self {
        Self {
            client_id,
            backend_globals,
            backend,
            client_event_writer: None,
            resource_map: WaylandResourceMap::default(),
            object_interfaces: HashMap::new(),
            object_versions: HashMap::new(),
            object_generations: HashMap::new(),
            next_generation: 0,
            delivered_focus: HashMap::new(),
            delivered_pressed: HashMap::new(),
            input_seats: HashMap::new(),
            input_surfaces: HashMap::new(),
            released_buffers: HashSet::new(),
            buffer_leases: HashMap::new(),
            deferred_releases: HashMap::new(),
            model_constraints: BTreeSet::new(),
            constraint_lifetimes: HashMap::new(),
            constraint_regions: HashMap::new(),
            pending_constraint_regions: HashMap::new(),
            spent_constraints: BTreeSet::new(),
            touch_contacts: HashMap::new(),
            touch_last_surface: HashMap::new(),
            seat_globals: HashMap::new(),
            local_registry_ids: HashSet::new(),
            local_callback_ids: HashSet::new(),
            frame_callbacks: HashMap::new(),
            synthetic_frame_done: HashSet::new(),
            pending_client_fds: VecDeque::new(),
            dmabuf_feedback_index_maps: HashMap::new(),
            dmabuf_main_device: None,
            frame_tracker: WaylandFrameTracker::new(format!("wayland-client-{}", client_id.0)),
            raw_forward_only: Arc::new(AtomicBool::new(false)),
            next_synthetic_serial: 1,
            synthetic_serials: HashSet::new(),
            synthetic_configures: HashSet::new(),
            keyboard_keymap_text: None,
            keyboard_layout_group: 0,
            keyboard_mods_depressed: 0,
            keyboard_mods_latched: 0,
            keyboard_mods_locked: 0,
        }
    }

    fn model_pointer_target(
        &self,
        window: &str,
        x: i64,
        y: i64,
    ) -> Result<Option<PointerClickTarget>, String> {
        for id in &self.model_constraints {
            if let Some(surface) = self.input_surfaces.get(id).copied()
                && self
                    .frame_tracker
                    .surface_to_window
                    .get(&surface)
                    .is_some_and(|w| w == window)
            {
                if self
                    .object_interfaces
                    .get(id)
                    .is_some_and(|i| i == "zwp_locked_pointer_v1")
                {
                    return Err("absolute motion is unavailable while the pointer is locked; send relative_motion".into());
                }
                let mut target = self
                    .frame_tracker
                    .surface_pointer_target(window, surface, x, y, true)?;
                let effective = self
                    .frame_tracker
                    .surfaces
                    .get(&surface)
                    .and_then(|s| s.input_region.as_ref());
                let regions = self.constraint_regions.get(id).and_then(|r| r.as_ref());
                let mut allowed = match (effective, regions) {
                    (Some(a), Some(b)) => a
                        .iter()
                        .flat_map(|a| b.iter().filter_map(move |b| intersect_rectangle(*a, *b)))
                        .collect::<Vec<_>>(),
                    (Some(a), None) | (None, Some(a)) => a.clone(),
                    (None, None) => return Ok(Some(target)),
                };
                allowed.retain(|r| r.width > 0 && r.height > 0);
                let px = f64::from(target.wire_x()) / 256.0;
                let py = f64::from(target.wire_y()) / 256.0;
                let nearest = allowed
                    .iter()
                    .map(|r| {
                        let x = px.clamp(
                            f64::from(r.x),
                            f64::from(r.x) + f64::from(r.width) - 1.0 / 256.0,
                        );
                        let y = py.clamp(
                            f64::from(r.y),
                            f64::from(r.y) + f64::from(r.height) - 1.0 / 256.0,
                        );
                        ((x - px).powi(2) + (y - py).powi(2), x, y)
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .ok_or("pointer confinement region is empty")?;
                target.surface_x = nearest.1.floor() as i64;
                target.surface_y = nearest.2.floor() as i64;
                target.fixed_coords = Some((
                    (nearest.1 * 256.0).round() as i32,
                    (nearest.2 * 256.0).round() as i32,
                ));
                return Ok(Some(target));
            }
        }
        for ((seat, pointer, human), held) in &self.delivered_pressed {
            if *pointer
                && !*human
                && !held.is_empty()
                && let Some(surface) = self.delivered_focus.get(&(*seat, true, false)).copied()
            {
                if self
                    .frame_tracker
                    .surface_to_window
                    .get(&surface)
                    .is_none_or(|w| w != window)
                {
                    return Err("pointer buttons are held by another window".into());
                }
                return self
                    .frame_tracker
                    .surface_pointer_target(window, surface, x, y, false)
                    .map(Some);
            }
        }
        self.frame_tracker.click_target_for_window(window, x, y)
    }

    fn logical_seat(&self, resource: u32) -> u32 {
        let object = self.input_seats.get(&resource).copied().unwrap_or(0);
        self.seat_globals.get(&object).copied().unwrap_or(object)
    }

    fn update_model_constraints(&mut self, surface: u32, focused: bool) -> Result<(), String> {
        let ids = self
            .input_surfaces
            .iter()
            .filter_map(|(id, s)| (*s == surface).then_some(*id))
            .collect::<Vec<_>>();
        for id in ids {
            let active = self.model_constraints.contains(&id);
            if focused == active || (focused && self.spent_constraints.contains(&id)) {
                continue;
            }
            let event = match (self.object_interfaces.get(&id).map(String::as_str), focused) {
                (Some("zwp_locked_pointer_v1"), true) => GeneratedEvent::ZwpLockedPointerV1Locked,
                (Some("zwp_locked_pointer_v1"), false) => {
                    GeneratedEvent::ZwpLockedPointerV1Unlocked
                }
                (Some("zwp_confined_pointer_v1"), true) => {
                    GeneratedEvent::ZwpConfinedPointerV1Confined
                }
                (Some("zwp_confined_pointer_v1"), false) => {
                    GeneratedEvent::ZwpConfinedPointerV1Unconfined
                }
                _ => continue,
            };
            let writer = self
                .client_event_writer
                .as_ref()
                .ok_or("client stream unavailable")?;
            writer.send_scoped(
                &encode_generated_event(id, &event)?,
                &[],
                Origin::Model,
                Some(surface),
            )?;
            if focused {
                self.model_constraints.insert(id);
            } else {
                self.model_constraints.remove(&id);
                if self.constraint_lifetimes.get(&id) == Some(&1) {
                    self.spent_constraints.insert(id);
                }
            }
        }
        Ok(())
    }

    fn release_model_pressed(&mut self) -> Result<(), String> {
        let surfaces = self
            .model_constraints
            .iter()
            .filter_map(|id| self.input_surfaces.get(id).copied())
            .collect::<BTreeSet<_>>();
        for surface in surfaces {
            self.update_model_constraints(surface, false)?;
        }
        let touch_seats = self
            .touch_contacts
            .keys()
            .filter(|(_, human, _)| !*human)
            .map(|(seat, _, _)| *seat)
            .collect::<BTreeSet<_>>();
        for seat in touch_seats {
            let resources = self
                .object_interfaces
                .iter()
                .filter_map(|(id, name)| {
                    (name == "wl_touch" && self.logical_seat(*id) == seat).then_some(*id)
                })
                .collect::<Vec<_>>();
            if let Some(writer) = &self.client_event_writer {
                for id in resources {
                    writer.send_origin(
                        &encode_generated_event(id, &GeneratedEvent::WlTouchCancel)?,
                        &[],
                        Origin::Model,
                    )?;
                }
            }
        }
        self.touch_contacts.retain(|(_, human, _), _| *human);
        let model = self
            .delivered_pressed
            .iter()
            .filter(|((_, _, human), _)| !*human)
            .map(|(key, held)| (*key, held.clone()))
            .collect::<Vec<_>>();
        for ((seat, pointer, _), held) in model {
            let physical = self
                .delivered_pressed
                .get(&(seat, pointer, true))
                .cloned()
                .unwrap_or_default();
            let interface = if pointer { "wl_pointer" } else { "wl_keyboard" };
            let resources = self
                .object_interfaces
                .iter()
                .filter(|(id, name)| name.as_str() == interface && self.logical_seat(**id) == seat)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for code in held.difference(&physical) {
                let serial = self.next_synthetic_serial();
                let time = wayland_timestamp_ms_u32();
                let event = if pointer {
                    GeneratedEvent::WlPointerButton {
                        serial,
                        time,
                        button: *code,
                        state: 0,
                    }
                } else {
                    GeneratedEvent::WlKeyboardKey {
                        serial,
                        time,
                        key: *code,
                        state: 0,
                    }
                };
                if let Some(writer) = &self.client_event_writer {
                    for id in &resources {
                        writer.send_origin(
                            &encode_generated_event(*id, &event)?,
                            &[],
                            Origin::Model,
                        )?;
                    }
                }
            }
            if pointer && let Some(writer) = &self.client_event_writer {
                for id in &resources {
                    if self.object_versions.get(id).copied().unwrap_or(1) >= 5 {
                        writer.send_origin(
                            &encode_generated_event(*id, &GeneratedEvent::WlPointerFrame)?,
                            &[],
                            Origin::Model,
                        )?;
                    }
                }
            }
            self.delivered_pressed.remove(&(seat, pointer, false));
        }
        Ok(())
    }

    fn resize_window(
        &mut self,
        window_id: &str,
        width: u32,
        height: u32,
    ) -> Result<String, String> {
        let window = self
            .frame_tracker
            .windows
            .get(window_id)
            .ok_or_else(|| format!("window `{window_id}` disappeared"))?;
        if !window.mapped {
            return Err(format!("window `{window_id}` is not mapped"));
        }
        let toplevel_id = window
            .xdg_toplevel_id
            .ok_or_else(|| format!("window `{window_id}` is not an xdg_toplevel"))?;
        let surface_id = window
            .xdg_surface_id
            .ok_or_else(|| format!("window `{window_id}` has no xdg_surface"))?;
        let serial = self.next_synthetic_serial() | 0x8000_0000;
        let writer = self
            .client_event_writer
            .as_ref()
            .ok_or_else(|| "client event stream is unavailable".to_string())?;
        let toplevel = encode_generated_event(
            toplevel_id,
            &GeneratedEvent::XdgToplevelConfigure {
                width: width as i32,
                height: height as i32,
                states: Vec::new(),
            },
        )?;
        let surface =
            encode_generated_event(surface_id, &GeneratedEvent::XdgSurfaceConfigure { serial })?;
        send_wayland_wire_message(writer, &toplevel, &[])?;
        send_wayland_wire_message(writer, &surface, &[])?;
        self.synthetic_configures.insert((surface_id, serial));
        Ok(format!(
            "sent xdg configure {width} x {height} to window `{window_id}`; inspect a later frame to verify the client applied it"
        ))
    }

    #[allow(dead_code)]
    fn find_backend_global(&self, interface: &str) -> Option<u32> {
        self.backend_globals
            .iter()
            .find(|global| global.interface == interface)
            .map(|global| global.name)
    }

    fn track_object_interface(&mut self, object_id: u32, interface: &str) {
        self.track_object_interface_version(object_id, interface, 1);
    }

    fn track_object_interface_version(&mut self, object_id: u32, interface: &str, version: u32) {
        if !self.object_interfaces.contains_key(&object_id) {
            self.next_generation += 1;
            self.object_generations
                .insert(object_id, self.next_generation);
        }
        self.object_interfaces
            .insert(object_id, interface.to_string());
        self.object_versions.insert(object_id, version);
    }

    fn interface_for_object(&self, object_id: u32) -> Option<&str> {
        self.object_interfaces.get(&object_id).map(String::as_str)
    }

    fn mark_local_registry(&mut self, object_id: u32) {
        self.local_registry_ids.insert(object_id);
    }

    fn is_local_registry(&self, object_id: u32) -> bool {
        self.local_registry_ids.contains(&object_id)
    }

    fn mark_local_callback(&mut self, object_id: u32) {
        self.local_callback_ids.insert(object_id);
    }

    fn remove_object(&mut self, object_id: u32) {
        self.frame_callbacks.remove(&object_id);
        self.synthetic_frame_done.remove(&object_id);
        self.object_interfaces.remove(&object_id);
        self.object_versions.remove(&object_id);
        self.input_seats.remove(&object_id);
        self.input_surfaces.remove(&object_id);
        self.model_constraints.remove(&object_id);
        self.constraint_lifetimes.remove(&object_id);
        self.constraint_regions.remove(&object_id);
        self.pending_constraint_regions.remove(&object_id);
        self.spent_constraints.remove(&object_id);
        self.touch_contacts
            .retain(|_, surface| *surface != object_id);
        self.touch_last_surface
            .retain(|_, surface| *surface != object_id);
        self.seat_globals.remove(&object_id);
        self.delivered_focus
            .retain(|_, surface| *surface != object_id);
        self.local_registry_ids.remove(&object_id);
        self.local_callback_ids.remove(&object_id);
        self.resource_map.unmap_client(object_id);
    }

    fn inject_click(&mut self, target: PointerClickTarget, button: u8) -> Result<String, String> {
        let mut pointer_ids = self
            .object_interfaces
            .iter()
            .filter_map(|(object_id, interface)| (interface == "wl_pointer").then_some(*object_id))
            .collect::<Vec<_>>();
        pointer_ids.sort_unstable();
        if pointer_ids.is_empty() {
            return Err(
                "gui_click cannot inject a Wayland click because the client has no wl_pointer"
                    .to_string(),
            );
        }
        let button_code = match button {
            1 => 0x110,
            2 => 0x112,
            3 => 0x111,
            _ => {
                return Err(format!(
                    "unsupported gui_click button {button}; expected 1, 2, or 3"
                ));
            }
        };
        let enter_serial = self.next_synthetic_serial();
        let press_serial = self.next_synthetic_serial();
        let release_serial = self.next_synthetic_serial();
        let time = wayland_timestamp_ms_u32();
        let writer = self.client_event_writer.as_ref().ok_or_else(|| {
            "gui_click cannot inject a Wayland click because the client stream is unavailable"
                .to_string()
        })?;
        let events = [
            GeneratedEvent::WlPointerEnter {
                serial: enter_serial,
                surface: Some(target.surface_id),
                surface_x: target.wire_x(),
                surface_y: target.wire_y(),
            },
            GeneratedEvent::WlPointerMotion {
                time,
                surface_x: target.wire_x(),
                surface_y: target.wire_y(),
            },
            GeneratedEvent::WlPointerFrame,
            GeneratedEvent::WlPointerButton {
                serial: press_serial,
                time,
                button: button_code,
                state: 1,
            },
            GeneratedEvent::WlPointerFrame,
            GeneratedEvent::WlPointerButton {
                serial: release_serial,
                time: time.wrapping_add(1),
                button: button_code,
                state: 0,
            },
            GeneratedEvent::WlPointerFrame,
        ];
        for pointer_id in &pointer_ids {
            for event in &events {
                let bytes = encode_generated_event(*pointer_id, event)?;
                send_wayland_wire_message(writer, &bytes, &[])?;
            }
        }
        Ok(format!(
            "delivered button {button} press/release through {} wl_pointer resource(s) to window `{}`: screenshot pixel ({}, {}) -> input wl_surface {} coordinate ({}, {})",
            pointer_ids.len(),
            target.window_id,
            target.screenshot_x,
            target.screenshot_y,
            target.surface_id,
            target.surface_x,
            target.surface_y
        ))
    }

    fn inject_pointer_motion(&mut self, target: PointerClickTarget) -> Result<String, String> {
        let mut pointer_ids = self
            .object_interfaces
            .iter()
            .filter_map(|(object_id, interface)| (interface == "wl_pointer").then_some(*object_id))
            .collect::<Vec<_>>();
        pointer_ids.sort_unstable();
        if pointer_ids.is_empty() {
            return Err(
                "pointer motion cannot be injected because the client has no wl_pointer"
                    .to_string(),
            );
        }
        let enter_serial = self.next_synthetic_serial();
        let time = wayland_timestamp_ms_u32();
        let writer = self.client_event_writer.as_ref().ok_or_else(|| {
            "pointer motion cannot be injected because the client stream is unavailable".to_string()
        })?;
        let events = [
            GeneratedEvent::WlPointerEnter {
                serial: enter_serial,
                surface: Some(target.surface_id),
                surface_x: target.wire_x(),
                surface_y: target.wire_y(),
            },
            GeneratedEvent::WlPointerMotion {
                time,
                surface_x: target.wire_x(),
                surface_y: target.wire_y(),
            },
            GeneratedEvent::WlPointerFrame,
        ];
        for pointer_id in &pointer_ids {
            for event in &events {
                let bytes = encode_generated_event(*pointer_id, event)?;
                send_wayland_wire_message(writer, &bytes, &[])?;
            }
        }
        Ok(format!(
            "moved pointer through {} wl_pointer resource(s) to window `{}`: screenshot pixel ({}, {}) -> input wl_surface {} coordinate ({}, {})",
            pointer_ids.len(),
            target.window_id,
            target.screenshot_x,
            target.screenshot_y,
            target.surface_id,
            target.surface_x,
            target.surface_y
        ))
    }

    fn emit_wayland_pointer_event(
        &mut self,
        target: PointerClickTarget,
        event: GuiWaylandPointerEvent,
        surface_fixed: bool,
    ) -> Result<String, String> {
        if matches!(event, GuiWaylandPointerEvent::Motion { .. })
            && self.model_constraints.iter().any(|id| {
                self.input_surfaces.get(id) == Some(&target.surface_id)
                    && self
                        .object_interfaces
                        .get(id)
                        .is_some_and(|i| i == "zwp_locked_pointer_v1")
            })
        {
            return Err(
                "absolute motion is unavailable while the pointer is locked; send relative_motion"
                    .into(),
            );
        }
        if let GuiWaylandPointerEvent::RelativeMotion {
            utime_hi,
            utime_lo,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        } = event
        {
            let ids = self
                .object_interfaces
                .iter()
                .filter_map(|(id, name)| (name == "zwp_relative_pointer_v1").then_some(*id))
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Err("client has no relative pointer resource".into());
            }
            let generated = GeneratedEvent::ZwpRelativePointerV1RelativeMotion {
                utime_hi,
                utime_lo,
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            };
            let writer = self
                .client_event_writer
                .as_ref()
                .ok_or("client stream unavailable")?;
            for id in ids {
                writer.send_scoped(
                    &encode_generated_event(id, &generated)?,
                    &[],
                    Origin::Model,
                    Some(target.surface_id),
                )?;
            }
            return Ok("emitted relative pointer motion".into());
        }
        if matches!(event, GuiWaylandPointerEvent::Leave { .. })
            && self
                .object_interfaces
                .get(&target.surface_id)
                .map(String::as_str)
                != Some("wl_surface")
        {
            return Ok(format!(
                "skipped wl_pointer.leave for destroyed wl_surface {}",
                target.surface_id
            ));
        }
        let required_version = match &event {
            GuiWaylandPointerEvent::Enter { .. }
            | GuiWaylandPointerEvent::Leave { .. }
            | GuiWaylandPointerEvent::Motion { .. }
            | GuiWaylandPointerEvent::Button { .. }
            | GuiWaylandPointerEvent::Axis { .. } => 1,
            GuiWaylandPointerEvent::Frame
            | GuiWaylandPointerEvent::AxisSource { .. }
            | GuiWaylandPointerEvent::AxisStop { .. }
            | GuiWaylandPointerEvent::AxisDiscrete { .. } => 5,
            GuiWaylandPointerEvent::AxisValue120 { .. } => 8,
            GuiWaylandPointerEvent::AxisRelativeDirection { .. } => 9,
            GuiWaylandPointerEvent::RelativeMotion { .. } => unreachable!(),
        };
        let mut pointer_ids = self
            .object_interfaces
            .iter()
            .filter_map(|(object_id, interface)| {
                (interface == "wl_pointer"
                    && self.object_versions.get(object_id).copied().unwrap_or(1)
                        >= required_version)
                    .then_some(*object_id)
            })
            .collect::<Vec<_>>();
        pointer_ids.sort_unstable();
        if pointer_ids.is_empty() {
            return Err(format!(
                "client has no wl_pointer resource supporting event version {required_version}"
            ));
        }
        let event_name = match &event {
            GuiWaylandPointerEvent::Enter { .. } => "wl_pointer.enter",
            GuiWaylandPointerEvent::Leave { .. } => "wl_pointer.leave",
            GuiWaylandPointerEvent::Motion { .. } => "wl_pointer.motion",
            GuiWaylandPointerEvent::Button { .. } => "wl_pointer.button",
            GuiWaylandPointerEvent::Axis { .. } => "wl_pointer.axis",
            GuiWaylandPointerEvent::AxisSource { .. } => "wl_pointer.axis_source",
            GuiWaylandPointerEvent::AxisStop { .. } => "wl_pointer.axis_stop",
            GuiWaylandPointerEvent::AxisDiscrete { .. } => "wl_pointer.axis_discrete",
            GuiWaylandPointerEvent::AxisValue120 { .. } => "wl_pointer.axis_value120",
            GuiWaylandPointerEvent::AxisRelativeDirection { .. } => {
                "wl_pointer.axis_relative_direction"
            }
            GuiWaylandPointerEvent::Frame => "wl_pointer.frame",
            GuiWaylandPointerEvent::RelativeMotion { .. } => unreachable!(),
        };
        let generated = match event {
            GuiWaylandPointerEvent::Enter { serial, .. } => GeneratedEvent::WlPointerEnter {
                serial: {
                    let _ = serial;
                    self.next_synthetic_serial()
                },
                surface: Some(target.surface_id),
                surface_x: if surface_fixed {
                    target.surface_x as i32
                } else {
                    target.wire_x()
                },
                surface_y: if surface_fixed {
                    target.surface_y as i32
                } else {
                    target.wire_y()
                },
            },
            GuiWaylandPointerEvent::Leave { serial } => GeneratedEvent::WlPointerLeave {
                serial: {
                    let _ = serial;
                    self.next_synthetic_serial()
                },
                surface: Some(target.surface_id),
            },
            GuiWaylandPointerEvent::Motion { time, .. } => GeneratedEvent::WlPointerMotion {
                time: time.unwrap_or_else(wayland_timestamp_ms_u32),
                surface_x: if surface_fixed {
                    target.surface_x as i32
                } else {
                    target.wire_x()
                },
                surface_y: if surface_fixed {
                    target.surface_y as i32
                } else {
                    target.wire_y()
                },
            },
            GuiWaylandPointerEvent::Button {
                button,
                state,
                serial,
                time,
            } => GeneratedEvent::WlPointerButton {
                serial: {
                    let _ = serial;
                    self.next_synthetic_serial()
                },
                time: time.unwrap_or_else(wayland_timestamp_ms_u32),
                button,
                state,
            },
            GuiWaylandPointerEvent::Axis { axis, value, time } => GeneratedEvent::WlPointerAxis {
                time: time.unwrap_or_else(wayland_timestamp_ms_u32),
                axis,
                value,
            },
            GuiWaylandPointerEvent::AxisSource { axis_source } => {
                GeneratedEvent::WlPointerAxisSource { axis_source }
            }
            GuiWaylandPointerEvent::AxisStop { axis, time } => GeneratedEvent::WlPointerAxisStop {
                time: time.unwrap_or_else(wayland_timestamp_ms_u32),
                axis,
            },
            GuiWaylandPointerEvent::AxisDiscrete { axis, discrete } => {
                GeneratedEvent::WlPointerAxisDiscrete { axis, discrete }
            }
            GuiWaylandPointerEvent::AxisValue120 { axis, value120 } => {
                GeneratedEvent::WlPointerAxisValue120 { axis, value120 }
            }
            GuiWaylandPointerEvent::AxisRelativeDirection { axis, direction } => {
                GeneratedEvent::WlPointerAxisRelativeDirection { axis, direction }
            }
            GuiWaylandPointerEvent::Frame => GeneratedEvent::WlPointerFrame,
            GuiWaylandPointerEvent::RelativeMotion { .. } => unreachable!(),
        };
        let focusing = matches!(generated, GeneratedEvent::WlPointerEnter { .. });
        let leaving = matches!(generated, GeneratedEvent::WlPointerLeave { .. });
        let writer = self.client_event_writer.as_ref().ok_or_else(|| {
            "raw Wayland event cannot be emitted because the client stream is unavailable"
                .to_string()
        })?;
        for pointer_id in &pointer_ids {
            let bytes = encode_generated_event(*pointer_id, &generated)?;
            writer.send_scoped(&bytes, &[], Origin::Model, Some(target.surface_id))?;
        }
        if focusing || leaving {
            self.update_model_constraints(target.surface_id, focusing)?;
        }
        Ok(format!(
            "emitted {event_name} through {} wl_pointer resource(s) for window `{}`",
            pointer_ids.len(),
            target.window_id
        ))
    }

    fn emit_touch(&mut self, surface: u32, event: GuiWaylandTouchEvent) -> Result<String, String> {
        let version = if matches!(
            event,
            GuiWaylandTouchEvent::Shape { .. } | GuiWaylandTouchEvent::Orientation { .. }
        ) {
            6
        } else {
            1
        };
        let ids = self
            .object_interfaces
            .iter()
            .filter_map(|(id, name)| {
                (name == "wl_touch"
                    && self.object_versions.get(id).copied().unwrap_or(1) >= version)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Err(format!(
                "client has no wl_touch resource supporting version {version}"
            ));
        }
        let seats = ids
            .iter()
            .map(|id| self.logical_seat(*id))
            .collect::<BTreeSet<_>>();
        if seats.len() != 1 {
            return Err("touch injection requires a single seat".into());
        }
        let seat = *seats.first().unwrap();
        let contact_id = match &event {
            GuiWaylandTouchEvent::Down { id, .. }
            | GuiWaylandTouchEvent::Up { id }
            | GuiWaylandTouchEvent::Motion { id, .. }
            | GuiWaylandTouchEvent::Shape { id, .. }
            | GuiWaylandTouchEvent::Orientation { id, .. } => Some(*id),
            _ => None,
        };
        if let Some(id) = contact_id {
            let existing = self.touch_contacts.get(&(seat, false, id));
            if matches!(event, GuiWaylandTouchEvent::Down { .. }) {
                if existing.is_some() {
                    return Err("touch contact is already down".into());
                }
                if self.touch_contacts.len() >= 128 {
                    return Err("touch contact limit reached".into());
                }
            } else if existing != Some(&surface) {
                return Err("touch contact is not down on target surface".into());
            }
        }
        if let GuiWaylandTouchEvent::Shape { major, minor, .. } = &event
            && (*major < 0 || *minor < 0)
        {
            return Err("touch shape must be nonnegative".into());
        }
        let serial = self.next_synthetic_serial();
        let time = wayland_timestamp_ms_u32();
        let generated = match event {
            GuiWaylandTouchEvent::Down { id, x, y } => GeneratedEvent::WlTouchDown {
                serial,
                time,
                surface: Some(surface),
                id,
                x,
                y,
            },
            GuiWaylandTouchEvent::Up { id } => GeneratedEvent::WlTouchUp { serial, time, id },
            GuiWaylandTouchEvent::Motion { id, x, y } => {
                GeneratedEvent::WlTouchMotion { time, id, x, y }
            }
            GuiWaylandTouchEvent::Frame => GeneratedEvent::WlTouchFrame,
            GuiWaylandTouchEvent::Cancel => GeneratedEvent::WlTouchCancel,
            GuiWaylandTouchEvent::Shape { id, major, minor } => {
                GeneratedEvent::WlTouchShape { id, major, minor }
            }
            GuiWaylandTouchEvent::Orientation { id, orientation } => {
                GeneratedEvent::WlTouchOrientation { id, orientation }
            }
        };
        let writer = self
            .client_event_writer
            .as_ref()
            .ok_or("client stream unavailable")?;
        for id in ids {
            writer.send_scoped(
                &encode_generated_event(id, &generated)?,
                &[],
                Origin::Model,
                Some(surface),
            )?;
        }
        // Track enqueued contact state as well as delivered state, so back-to-back
        // calls cannot reuse an ID before the writer delivery hook has run.
        match generated {
            GeneratedEvent::WlTouchDown { id, .. } => {
                self.touch_contacts.insert((seat, false, id), surface);
            }
            GeneratedEvent::WlTouchUp { id, .. } => {
                self.touch_contacts.remove(&(seat, false, id));
            }
            GeneratedEvent::WlTouchCancel => {
                self.touch_contacts.retain(|(_, human, _), _| *human);
            }
            _ => {}
        }
        Ok("emitted touch event".into())
    }

    fn emit_wayland_keyboard_event(
        &mut self,
        window_id: &str,
        surface_id: u32,
        event: GuiWaylandKeyboardEvent,
    ) -> Result<String, String> {
        let required_version = match &event {
            GuiWaylandKeyboardEvent::RepeatInfo { .. } => 4,
            _ => 1,
        };
        let mut keyboard_ids = self
            .object_interfaces
            .iter()
            .filter_map(|(object_id, interface)| {
                (interface == "wl_keyboard"
                    && self.object_versions.get(object_id).copied().unwrap_or(1)
                        >= required_version)
                    .then_some(*object_id)
            })
            .collect::<Vec<_>>();
        keyboard_ids.sort_unstable();
        if keyboard_ids.is_empty() {
            return Err(format!(
                "client has no wl_keyboard resource supporting event version {required_version}"
            ));
        }
        let event_name = match &event {
            GuiWaylandKeyboardEvent::Enter { .. } => "wl_keyboard.enter",
            GuiWaylandKeyboardEvent::Leave { .. } => "wl_keyboard.leave",
            GuiWaylandKeyboardEvent::Key { .. } => "wl_keyboard.key",
            GuiWaylandKeyboardEvent::Modifiers { .. } => "wl_keyboard.modifiers",
            GuiWaylandKeyboardEvent::RepeatInfo { .. } => "wl_keyboard.repeat_info",
        };
        let generated = match event {
            GuiWaylandKeyboardEvent::Enter { serial, keys } => {
                let mut key_bytes = Vec::with_capacity(keys.len() * 4);
                for key in keys {
                    key_bytes.extend_from_slice(&key.to_ne_bytes());
                }
                GeneratedEvent::WlKeyboardEnter {
                    serial: {
                        let _ = serial;
                        self.next_synthetic_serial()
                    },
                    surface: Some(surface_id),
                    keys: key_bytes,
                }
            }
            GuiWaylandKeyboardEvent::Leave { serial } => GeneratedEvent::WlKeyboardLeave {
                serial: {
                    let _ = serial;
                    self.next_synthetic_serial()
                },
                surface: Some(surface_id),
            },
            GuiWaylandKeyboardEvent::Key {
                key,
                state,
                serial,
                time,
            } => GeneratedEvent::WlKeyboardKey {
                serial: {
                    let _ = serial;
                    self.next_synthetic_serial()
                },
                time: time.unwrap_or_else(wayland_timestamp_ms_u32),
                key,
                state,
            },
            GuiWaylandKeyboardEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                serial,
            } => GeneratedEvent::WlKeyboardModifiers {
                serial: {
                    let _ = serial;
                    self.next_synthetic_serial()
                },
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            },
            GuiWaylandKeyboardEvent::RepeatInfo { rate, delay } => {
                GeneratedEvent::WlKeyboardRepeatInfo { rate, delay }
            }
        };
        let writer = self.client_event_writer.as_ref().ok_or_else(|| {
            "raw Wayland event cannot be emitted because the client stream is unavailable"
                .to_string()
        })?;
        for keyboard_id in &keyboard_ids {
            let bytes = encode_generated_event(*keyboard_id, &generated)?;
            writer.send_scoped(&bytes, &[], Origin::Model, Some(surface_id))?;
        }
        Ok(format!(
            "emitted {event_name} through {} wl_keyboard resource(s) for window `{window_id}`",
            keyboard_ids.len()
        ))
    }

    fn next_synthetic_serial(&mut self) -> u32 {
        let serial = self.next_synthetic_serial;
        self.next_synthetic_serial = self.next_synthetic_serial.wrapping_add(1).max(1);
        if self.synthetic_serials.len() > 4096 {
            self.synthetic_serials.clear();
        }
        self.synthetic_serials.insert(serial);
        serial
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaylandGlobalInfo {
    pub(crate) name: u32,
    pub(crate) interface: String,
    pub(crate) version: u32,
}

fn filtered_backend_globals(globals: &[WaylandGlobalInfo]) -> Vec<WaylandGlobalInfo> {
    globals
        .iter()
        .filter(|global| is_supported_backend_global(&global.interface))
        .map(|global| WaylandGlobalInfo {
            version: global
                .version
                .min(crate::wayland_policy::global(&global.interface).max_version)
                .min(generated_interface_version(&global.interface)),
            ..global.clone()
        })
        .collect()
}

fn is_suppressed_backend_global(interface: &str) -> bool {
    !is_supported_backend_global(interface)
}

fn generated_interface_version(interface: &str) -> u32 {
    GENERATED_PROTOCOLS
        .iter()
        .flat_map(|protocol| protocol.interfaces)
        .find(|candidate| candidate.name == interface)
        .map_or(2, |candidate| candidate.version)
}

fn is_supported_backend_global(interface: &str) -> bool {
    crate::wayland_policy::global(interface).exposure != crate::wayland_policy::Exposure::Deny
        && (MANUALLY_SUPPORTED_BACKEND_GLOBALS.contains(&interface)
            || GENERATED_PROTOCOLS.iter().any(|protocol| {
                protocol
                    .interfaces
                    .iter()
                    .any(|candidate| candidate.name == interface)
            }))
}

fn validate_registry_bind(
    session: &WaylandClientSession,
    request: &DecodedWaylandRequest,
) -> Result<(), String> {
    if let Some(GeneratedHookRequest::WlRegistryBind {
        name,
        id_interface,
        id_version,
        id,
    }) = &request.hook_request
    {
        let advertised = session
            .backend_globals
            .iter()
            .find(|global| global.name == *name)
            .ok_or_else(|| format!("registry global {name} is not exposed"))?;
        if id_interface.as_deref() != Some(advertised.interface.as_str())
            || *id_version == 0
            || *id_version > advertised.version
            || !is_supported_backend_global(&advertised.interface)
            || *id == 0
            || *id >= 0xff00_0000
            || session.object_interfaces.contains_key(id)
        {
            return Err(format!("invalid or denied registry bind for global {name}"));
        }
    }
    Ok(())
}

fn upsert_backend_global(
    globals: &mut Vec<WaylandGlobalInfo>,
    name: u32,
    interface: &str,
    version: u32,
) {
    if let Some(global) = globals.iter_mut().find(|global| global.name == name) {
        global.interface = interface.to_string();
        global.version = version;
    } else {
        globals.push(WaylandGlobalInfo {
            name,
            interface: interface.to_string(),
            version,
        });
    }
}

fn rewrite_registry_version(
    event: &DecodedWaylandEvent,
    message: &mut WaylandWireMessage,
) -> Result<(), String> {
    if let GeneratedEvent::WlRegistryGlobal {
        name,
        interface: Some(interface),
        version,
    } = &event.generated_event
    {
        let maximum = crate::wayland_policy::global(interface).max_version;
        if maximum > 0 {
            let generated_maximum = generated_interface_version(interface);
            message.bytes = encode_generated_event(
                event.object_id,
                &GeneratedEvent::WlRegistryGlobal {
                    name: *name,
                    interface: Some(interface.clone()),
                    version: (*version).min(maximum).min(generated_maximum),
                },
            )?;
        }
    }
    Ok(())
}

fn is_suppressed_registry_global_event(event: &DecodedWaylandEvent) -> bool {
    matches!(
        &event.generated_event,
        GeneratedEvent::WlRegistryGlobal {
            interface: Some(interface),
            ..
        } if is_suppressed_backend_global(interface)
    )
}

fn is_unsupported_dmabuf_advertisement(event: &DecodedWaylandEvent) -> bool {
    match &event.generated_event {
        GeneratedEvent::ZwpLinuxDmabufV1Format { format }
        | GeneratedEvent::ZwpLinuxDmabufV1Modifier { format, .. } => {
            !crate::gui_vulkan_dmabuf::supports_screenshot_drm_format(*format)
        }
        _ => false,
    }
}

fn rewrite_dmabuf_feedback_event(
    session: &mut WaylandClientSession,
    event: &DecodedWaylandEvent,
    message: &mut WaylandWireMessage,
) -> Result<(), String> {
    match &event.generated_event {
        GeneratedEvent::ZwpLinuxDmabufFeedbackV1FormatTable { size, .. } => {
            let fd = message.fds.first().ok_or_else(|| {
                "zwp_linux_dmabuf_feedback_v1.format_table was missing its fd".to_string()
            })?;
            let size = usize::try_from(*size)
                .map_err(|_| "DMA-BUF feedback format table size overflow".to_string())?;
            if size % 16 != 0 {
                return Err(format!(
                    "DMA-BUF feedback format table size {size} is not a multiple of 16"
                ));
            }
            let mut table = vec![0u8; size];
            let read = std::fs::File::from(duplicate_fd(fd)?)
                .read_at(&mut table, 0)
                .map_err(|err| format!("failed to read DMA-BUF feedback format table: {err}"))?;
            if read != size {
                return Err(format!(
                    "short DMA-BUF feedback format table read: {read} of {size} bytes"
                ));
            }
            let mut filtered = Vec::new();
            let mut index_map = Vec::with_capacity(size / 16);
            let (entries, remainder) = table.as_chunks::<16>();
            debug_assert!(remainder.is_empty());
            for entry in entries {
                let format = u32::from_ne_bytes([entry[0], entry[1], entry[2], entry[3]]);
                if crate::gui_vulkan_dmabuf::supports_screenshot_drm_format(format) {
                    let new_index = u16::try_from(filtered.len() / 16).map_err(|_| {
                        "filtered DMA-BUF format table exceeds u16 indices".to_string()
                    })?;
                    index_map.push(Some(new_index));
                    filtered.extend_from_slice(entry);
                } else {
                    index_map.push(None);
                }
            }
            let mut filtered_file = tempfile::tempfile()
                .map_err(|err| format!("failed to create filtered DMA-BUF format table: {err}"))?;
            filtered_file
                .write_all(&filtered)
                .map_err(|err| format!("failed to write filtered DMA-BUF format table: {err}"))?;
            session
                .dmabuf_feedback_index_maps
                .insert(event.object_id, index_map);
            message.fds = vec![filtered_file.into()];
            message.bytes = encode_generated_event(
                event.object_id,
                &GeneratedEvent::ZwpLinuxDmabufFeedbackV1FormatTable {
                    fd: true,
                    size: filtered.len() as u32,
                },
            )?;
        }
        GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFormats { indices } => {
            let index_map = session
                .dmabuf_feedback_index_maps
                .get(&event.object_id)
                .ok_or_else(|| {
                    format!(
                        "DMA-BUF feedback object {} sent tranche formats before a format table",
                        event.object_id
                    )
                })?;
            if indices.len() % 2 != 0 {
                return Err("DMA-BUF feedback tranche index array has odd byte length".to_string());
            }
            let mut filtered_indices = Vec::new();
            let (encoded_indices, remainder) = indices.as_chunks::<2>();
            debug_assert!(remainder.is_empty());
            for encoded in encoded_indices {
                let old_index = usize::from(u16::from_ne_bytes(*encoded));
                let mapped = index_map.get(old_index).ok_or_else(|| {
                    format!("DMA-BUF feedback tranche index {old_index} exceeds format table")
                })?;
                if let Some(new_index) = mapped {
                    filtered_indices.extend_from_slice(&new_index.to_ne_bytes());
                }
            }
            message.bytes = encode_generated_event(
                event.object_id,
                &GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFormats {
                    indices: filtered_indices,
                },
            )?;
        }
        _ => {}
    }
    Ok(())
}

#[derive(Default)]
struct WaylandResourceMap {
    next_server_id: u32,
    local: HashSet<u32>,
    client_to_backend: HashMap<u32, u32>,
    backend_to_client: HashMap<u32, u32>,
}

impl WaylandResourceMap {
    fn allocate_server_id(&mut self, backend: Option<u32>) -> Result<u32, String> {
        let id = self.next_server_id.max(0xff00_0000);
        self.next_server_id = id
            .checked_add(1)
            .ok_or("downstream server object IDs exhausted")?;
        if let Some(backend) = backend {
            if backend < 0xff00_0000 || self.backend_to_client.contains_key(&backend) {
                return Err("invalid or reused live upstream server ID".into());
            }
            self.map(id, backend);
        } else {
            self.local.insert(id);
        }
        Ok(id)
    }
    fn downstream_id(&self, upstream: u32) -> Result<u32, String> {
        if upstream == 0 {
            return Ok(0);
        }
        if let Some(id) = self.client_for(upstream) {
            return Ok(id);
        }
        if upstream < 0xff00_0000 {
            Ok(upstream)
        } else {
            Err(format!("unmapped upstream server object {upstream}"))
        }
    }
    fn upstream_id(&self, downstream: u32) -> Result<u32, String> {
        if self.local.contains(&downstream) {
            return Err("local object cannot be forwarded upstream".into());
        }
        if downstream == 0 {
            return Ok(0);
        }
        if let Some(id) = self.backend_for(downstream) {
            return Ok(id);
        }
        if downstream < 0xff00_0000 {
            Ok(downstream)
        } else {
            Err(format!("unmapped downstream server object {downstream}"))
        }
    }

    fn map(&mut self, client_resource_id: u32, backend_resource_id: u32) {
        if let Some(previous_backend) = self
            .client_to_backend
            .insert(client_resource_id, backend_resource_id)
        {
            self.backend_to_client.remove(&previous_backend);
        }
        if let Some(previous_client) = self
            .backend_to_client
            .insert(backend_resource_id, client_resource_id)
        {
            self.client_to_backend.remove(&previous_client);
        }
    }

    #[allow(dead_code)]
    fn unmap_client(&mut self, client_resource_id: u32) -> Option<u32> {
        self.local.remove(&client_resource_id);
        let backend_resource_id = self.client_to_backend.remove(&client_resource_id)?;
        self.backend_to_client.remove(&backend_resource_id);
        Some(backend_resource_id)
    }

    #[allow(dead_code)]
    fn backend_for(&self, client_resource_id: u32) -> Option<u32> {
        self.client_to_backend.get(&client_resource_id).copied()
    }

    #[allow(dead_code)]
    fn client_for(&self, backend_resource_id: u32) -> Option<u32> {
        self.backend_to_client.get(&backend_resource_id).copied()
    }

    fn len(&self) -> usize {
        self.client_to_backend.len()
    }
}

struct WaylandProtocolRegistry {
    generated: WaylandGeneratedProtocolRegistry,
    intercepts: WaylandHookRegistry<WaylandInterceptHook>,
}

impl WaylandProtocolRegistry {
    fn global_specs(&self) -> Vec<WaylandGlobalBinding> {
        self.generated
            .globals()
            .iter()
            .map(|global| WaylandGlobalBinding {
                interface: global.interface_name.to_string(),
                version: global.version,
            })
            .collect()
    }

    fn global_names(&self) -> Vec<String> {
        self.generated
            .globals()
            .iter()
            .map(|global| global.interface_name.to_string())
            .collect()
    }

    fn register_intercept(
        &mut self,
        request_id: GeneratedHookRequestId,
        hook: WaylandInterceptHook,
    ) {
        self.intercepts.register(request_id, hook);
    }

    fn intercept_names(&self) -> Vec<String> {
        self.generated
            .hook_requests()
            .filter(|hook| self.intercepts.has_hooks(hook.id))
            .map(|hook| hook.request_name.to_string())
            .collect()
    }

    fn intercepts_for(&self, request_id: GeneratedHookRequestId) -> &[WaylandInterceptHook] {
        self.intercepts.hooks_for(request_id)
    }

    fn request_by_id(
        &self,
        request_id: GeneratedRequestId,
    ) -> Option<(
        &'static crate::gui_wayland_generated::GeneratedInterfaceSpec,
        &'static crate::gui_wayland_generated::GeneratedMessageSpec,
    )> {
        self.generated.request_by_id(request_id)
    }

    fn hook_request_id(&self, request_name: &str) -> Option<GeneratedHookRequestId> {
        self.generated
            .hook_request_by_name(request_name)
            .map(|hook| hook.id)
    }
}

impl Default for WaylandProtocolRegistry {
    fn default() -> Self {
        Self {
            generated: WaylandGeneratedProtocolRegistry::new(),
            intercepts: WaylandHookRegistry::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WaylandGlobalBinding {
    interface: String,
    version: u32,
}

struct WaylandBackendSession {
    stream: StdUnixStream,
    globals: Vec<WaylandGlobalInfo>,
    object_interfaces: HashMap<u32, String>,
    pending_backend_fds: VecDeque<OwnedFd>,
}

impl WaylandBackendSession {
    fn connect(config: &WaylandProxyConfig) -> Result<Self, String> {
        let socket_path = backend_socket_path(config)?;
        let stream = StdUnixStream::connect(&socket_path).map_err(|err| {
            format!(
                "failed to connect to Wayland backend {}: {err}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|err| format!("failed to set Wayland backend read timeout: {err}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|err| format!("failed to set Wayland backend write timeout: {err}"))?;

        let globals = discover_backend_globals(&socket_path)?;
        let session = Self {
            stream,
            globals,
            object_interfaces: HashMap::from([(1, "wl_display".to_string())]),
            pending_backend_fds: VecDeque::new(),
        };
        session.set_runtime_timeouts()?;
        Ok(session)
    }

    fn set_runtime_timeouts(&self) -> Result<(), String> {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|err| format!("failed to set Wayland backend read timeout: {err}"))?;
        self.stream
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(|err| format!("failed to set Wayland backend write timeout: {err}"))?;
        Ok(())
    }

    fn track_request_objects(
        &mut self,
        generated: &WaylandGeneratedProtocolRegistry,
        request: &DecodedWaylandRequest,
    ) {
        apply_backend_object_tracking(&mut self.object_interfaces, generated, request);
    }

    fn forward_raw_request(&mut self, message: &WaylandWireMessage) -> Result<(), String> {
        send_wayland_wire_message(&self.stream, &message.bytes, &message.fds)
    }

    fn drain_pending_events(&mut self) -> Result<Vec<WaylandBackendEvent>, String> {
        let mut encoded_events = Vec::new();
        while encoded_events.len() < MAX_BACKEND_EVENTS_PER_DRAIN {
            let mut message = match read_wayland_wire_message_blocking(&self.stream, 16) {
                Ok(message) => message,
                Err(err) if is_timeout_error(&err) => break,
                Err(err) => return Err(err),
            };
            let decoded = decode_wayland_event(&self.object_interfaces, &message.bytes);
            if let Ok(event) = decoded.as_ref() {
                reassociate_backend_fds(self, event, &mut message.fds);
                trace_wayland_proxy(format_args!(
                    "backend -> {}#{}.{} fds={}",
                    event.interface,
                    event.object_id,
                    event.event_name,
                    message.fds.len()
                ));
                self.apply_event(event);
            } else {
                reassociate_undecoded_backend_fds(self, &message.bytes, &mut message.fds)?;
                trace_undecoded_wayland_message("backend", &message, decoded.as_ref().err());
            }
            let decoded = decoded.ok();
            encoded_events.push(WaylandBackendEvent {
                suppressed: false,
                encoded: message,
                decoded,
            });
        }
        Ok(encoded_events)
    }

    fn apply_event(&mut self, event: &DecodedWaylandEvent) {
        apply_object_interfaces_from_event(&mut self.object_interfaces, event);
        if let GeneratedEvent::WlRegistryGlobal {
            name,
            interface: Some(interface),
            version,
        } = &event.generated_event
        {
            upsert_backend_global(&mut self.globals, *name, interface, *version);
        }
    }
}

fn discover_backend_globals(socket_path: &PathBuf) -> Result<Vec<WaylandGlobalInfo>, String> {
    let mut stream = StdUnixStream::connect(socket_path).map_err(|err| {
        format!(
            "failed to connect to Wayland backend {} for global discovery: {err}",
            socket_path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("failed to set Wayland backend discovery read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("failed to set Wayland backend discovery write timeout: {err}"))?;
    stream
        .write_all(&encode_u32_message(1, 1, &[BACKEND_BOOTSTRAP_REGISTRY_ID]))
        .map_err(|err| format!("failed to request Wayland backend registry: {err}"))?;
    stream
        .write_all(&encode_u32_message(1, 0, &[BACKEND_BOOTSTRAP_CALLBACK_ID]))
        .map_err(|err| format!("failed to request Wayland backend sync: {err}"))?;
    stream
        .flush()
        .map_err(|err| format!("failed to flush Wayland backend bootstrap: {err}"))?;

    let mut object_interfaces = HashMap::from([
        (1, "wl_display".to_string()),
        (BACKEND_BOOTSTRAP_REGISTRY_ID, "wl_registry".to_string()),
        (BACKEND_BOOTSTRAP_CALLBACK_ID, "wl_callback".to_string()),
    ]);
    let mut globals = Vec::new();
    loop {
        let bytes = match read_wayland_message(&mut stream) {
            Ok(bytes) => bytes,
            Err(err) if is_timeout_error(&err) => break,
            Err(err) => return Err(err),
        };
        let event = decode_wayland_event(&object_interfaces, &bytes)?;
        apply_object_interfaces_from_event(&mut object_interfaces, &event);
        if let GeneratedEvent::WlRegistryGlobal {
            name,
            interface: Some(interface),
            version,
        } = &event.generated_event
        {
            globals.push(WaylandGlobalInfo {
                name: *name,
                interface: interface.clone(),
                version: *version,
            });
        }
        if event.object_id == BACKEND_BOOTSTRAP_CALLBACK_ID
            && event.event_name == "wl_callback.done"
        {
            break;
        }
    }
    Ok(globals)
}

#[derive(Clone)]
struct WaylandInterceptHook {
    kind: WaylandInterceptKind,
}

impl WaylandInterceptHook {
    fn apply(
        &self,
        session: &mut WaylandClientSession,
        resource_id: u32,
        request: &GeneratedHookRequest,
    ) {
        match (self.kind, request) {
            (
                WaylandInterceptKind::SurfaceAttach,
                GeneratedHookRequest::WlSurfaceAttach { buffer, .. },
            ) => {
                session
                    .frame_tracker
                    .set_surface_buffer(resource_id, *buffer);
            }
            (
                WaylandInterceptKind::SurfaceDamage,
                GeneratedHookRequest::WlSurfaceDamage {
                    x,
                    y,
                    width,
                    height,
                },
            )
            | (
                WaylandInterceptKind::SurfaceDamage,
                GeneratedHookRequest::WlSurfaceDamageBuffer {
                    x,
                    y,
                    width,
                    height,
                },
            ) => {
                session.frame_tracker.add_damage(
                    resource_id,
                    DamageRect {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                    },
                );
            }
            (WaylandInterceptKind::SurfaceCommit, GeneratedHookRequest::WlSurfaceCommit) => {
                session.frame_tracker.commit_surface(resource_id);
            }
            (WaylandInterceptKind::TimelineImport, _)
            | (WaylandInterceptKind::AcquirePoint, _)
            | (WaylandInterceptKind::ReleasePoint, _) => {}
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum WaylandInterceptKind {
    SurfaceAttach,
    SurfaceDamage,
    SurfaceCommit,
    TimelineImport,
    AcquirePoint,
    ReleasePoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaylandInterceptArg {
    I32(i32),
    U32(u32),
    Resource(u32),
}

impl WaylandInterceptArg {
    fn from_decoded(arg: &DecodedWaylandArg) -> Option<Self> {
        match arg {
            DecodedWaylandArg::Int(value) | DecodedWaylandArg::Fixed(value) => {
                Some(Self::I32(*value))
            }
            DecodedWaylandArg::Uint(value) | DecodedWaylandArg::NewId(value) => {
                Some(Self::U32(*value))
            }
            DecodedWaylandArg::Object(Some(value)) => Some(Self::Resource(*value)),
            DecodedWaylandArg::String(_)
            | DecodedWaylandArg::Object(None)
            | DecodedWaylandArg::Array(_)
            | DecodedWaylandArg::Fd => None,
        }
    }
}

impl GeneratedDecodedArg {
    fn from_decoded(arg: &DecodedWaylandArg) -> Self {
        match arg {
            DecodedWaylandArg::Int(value) => Self::Int(*value),
            DecodedWaylandArg::Uint(value) => Self::Uint(*value),
            DecodedWaylandArg::Fixed(value) => Self::Fixed(*value),
            DecodedWaylandArg::String(value) => Self::String(value.clone()),
            DecodedWaylandArg::Object(value) => Self::Object(*value),
            DecodedWaylandArg::NewId(value) => Self::NewId(*value),
            DecodedWaylandArg::Array(value) => Self::Array(value.clone()),
            DecodedWaylandArg::Fd => Self::Fd,
        }
    }
}

fn generated_decoded_args_from_intercept_args(
    request_name: &str,
    args: &[WaylandInterceptArg],
) -> Result<Vec<GeneratedDecodedArg>, String> {
    let (_, request) = find_generated_request(request_name)
        .ok_or_else(|| format!("missing generated request metadata for {request_name}"))?;
    if request.args.len() != args.len() {
        return Err(format!(
            "generated hook request {request_name} expected {} args, got {}",
            request.args.len(),
            args.len()
        ));
    }

    request
        .args
        .iter()
        .zip(args)
        .map(|(arg_spec, arg)| match (arg_spec.kind, arg) {
            (GeneratedArgKind::Int, WaylandInterceptArg::I32(value))
            | (GeneratedArgKind::Fixed, WaylandInterceptArg::I32(value)) => {
                Ok(GeneratedDecodedArg::Int(*value))
            }
            (GeneratedArgKind::Uint, WaylandInterceptArg::U32(value)) => {
                Ok(GeneratedDecodedArg::Uint(*value))
            }
            (GeneratedArgKind::NewId, WaylandInterceptArg::U32(value)) => {
                Ok(GeneratedDecodedArg::NewId(*value))
            }
            (GeneratedArgKind::Object, WaylandInterceptArg::Resource(value)) => {
                Ok(GeneratedDecodedArg::Object(Some(*value)))
            }
            (expected, actual) => Err(format!(
                "generated hook request {request_name} arg {} expected {:?}, got {:?}",
                arg_spec.name, expected, actual
            )),
        })
        .collect()
}

fn intersect_rectangle(a: DamageRect, b: DamageRect) -> Option<DamageRect> {
    let x = i64::from(a.x).max(i64::from(b.x));
    let y = i64::from(a.y).max(i64::from(b.y));
    let right = (i64::from(a.x) + i64::from(a.width)).min(i64::from(b.x) + i64::from(b.width));
    let bottom = (i64::from(a.y) + i64::from(a.height)).min(i64::from(b.y) + i64::from(b.height));
    (right > x && bottom > y).then_some(DamageRect {
        x: x as i32,
        y: y as i32,
        width: (right - x) as i32,
        height: (bottom - y) as i32,
    })
}

fn subtract_rectangle(rect: DamageRect, cut: DamageRect) -> Vec<DamageRect> {
    let x0 = i64::from(rect.x);
    let y0 = i64::from(rect.y);
    let x1 = x0 + i64::from(rect.width);
    let y1 = y0 + i64::from(rect.height);
    let cx0 = x0.max(i64::from(cut.x));
    let cy0 = y0.max(i64::from(cut.y));
    let cx1 = x1.min(i64::from(cut.x) + i64::from(cut.width));
    let cy1 = y1.min(i64::from(cut.y) + i64::from(cut.height));
    if cx0 >= cx1 || cy0 >= cy1 {
        return vec![rect];
    }
    [
        (x0, y0, x1, cy0),
        (x0, cy1, x1, y1),
        (x0, cy0, cx0, cy1),
        (cx1, cy0, x1, cy1),
    ]
    .into_iter()
    .filter_map(|(a, b, c, d)| {
        if a >= c || b >= d {
            None
        } else {
            Some(DamageRect {
                x: a as i32,
                y: b as i32,
                width: (c - a) as i32,
                height: (d - b) as i32,
            })
        }
    })
    .collect()
}

struct ShmBufferSpec {
    pool_id: u32,
    buffer_id: u32,
    offset: u32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
}

struct WaylandFrameTracker {
    colors: crate::gui_color::SurfaceColors,
    window_id_prefix: String,
    next_commit_serial: u64,
    prepared_gpu: HashMap<u32, Result<Vec<[f32; 4]>, String>>,
    popup_parent: HashMap<u32, u32>,
    popup_objects: HashMap<u32, u32>,
    popup_positions: HashMap<u32, (i32, i32)>,
    pending_popup_positions: HashMap<u32, (i32, i32)>,
    popup_order: Vec<u32>,
    dismissed_popups: BTreeSet<u32>,
    next_window_id: u64,
    dmabuf_params: HashMap<u32, PendingDmabufParams>,
    shm_pools: HashMap<u32, TrackedShmPool>,
    buffers: HashMap<u32, Arc<TrackedBuffer>>,
    surfaces: HashMap<u32, TrackedSurface>,
    pending_surfaces: HashMap<u32, TrackedSurface>,
    cached_surfaces: HashMap<u32, TrackedSurface>,
    synchronized: HashSet<u32>,
    pending_positions: HashMap<u32, (i32, i32)>,
    stacking: HashMap<u32, Vec<u32>>,
    pending_stacking: HashMap<u32, Vec<u32>>,
    regions: HashMap<u32, Vec<DamageRect>>,
    windows: HashMap<String, TrackedWindow>,
    surface_to_window: HashMap<u32, String>,
    xdg_surface_to_surface: HashMap<u32, u32>,
    xdg_toplevel_to_window: HashMap<u32, String>,
    subsurface_to_surface: HashMap<u32, u32>,
    surface_parent: HashMap<u32, u32>,
    surface_position: HashMap<u32, (i32, i32)>,
    viewport_to_surface: HashMap<u32, u32>,
    syncobj_surface_to_surface: HashMap<u32, u32>,
    syncobj_timelines: HashMap<u32, TrackedSyncobjTimeline>,
}

impl WaylandFrameTracker {
    fn new(window_id_prefix: String) -> Self {
        Self {
            colors: crate::gui_color::SurfaceColors::default(),
            window_id_prefix,
            next_commit_serial: 0,
            prepared_gpu: HashMap::new(),
            popup_parent: HashMap::new(),
            popup_objects: HashMap::new(),
            popup_positions: HashMap::new(),
            pending_popup_positions: HashMap::new(),
            popup_order: Vec::new(),
            dismissed_popups: BTreeSet::new(),
            next_window_id: 0,
            dmabuf_params: HashMap::new(),
            shm_pools: HashMap::new(),
            buffers: HashMap::new(),
            surfaces: HashMap::new(),
            pending_surfaces: HashMap::new(),
            cached_surfaces: HashMap::new(),
            synchronized: HashSet::new(),
            pending_positions: HashMap::new(),
            stacking: HashMap::new(),
            pending_stacking: HashMap::new(),
            regions: HashMap::new(),
            windows: HashMap::new(),
            surface_to_window: HashMap::new(),
            xdg_surface_to_surface: HashMap::new(),
            xdg_toplevel_to_window: HashMap::new(),
            subsurface_to_surface: HashMap::new(),
            surface_parent: HashMap::new(),
            surface_position: HashMap::new(),
            viewport_to_surface: HashMap::new(),
            syncobj_surface_to_surface: HashMap::new(),
            syncobj_timelines: HashMap::new(),
        }
    }

    fn note_shm_pool_created(&mut self, pool_id: u32, fd: OwnedFd, size: usize) {
        self.shm_pools.insert(pool_id, TrackedShmPool { fd, size });
    }

    fn note_shm_pool_resized(&mut self, pool_id: u32, size: usize) -> Result<(), String> {
        let pool = self
            .shm_pools
            .get_mut(&pool_id)
            .ok_or_else(|| format!("missing wl_shm_pool {pool_id}"))?;
        pool.size = size;
        Ok(())
    }

    fn note_shm_pool_destroyed(&mut self, pool_id: u32) {
        self.shm_pools.remove(&pool_id);
    }

    fn note_shm_buffer_created(&mut self, spec: ShmBufferSpec) -> Result<(), String> {
        let ShmBufferSpec {
            pool_id,
            buffer_id,
            offset,
            width,
            height,
            stride,
            format,
        } = spec;
        if width <= 0 || height <= 0 || stride <= 0 {
            return Err(format!(
                "wl_shm_pool.create_buffer {buffer_id} has invalid geometry {width}x{height} stride={stride}"
            ));
        }
        let pool = self
            .shm_pools
            .get(&pool_id)
            .ok_or_else(|| format!("missing wl_shm_pool {pool_id}"))?;
        let height_usize = height as usize;
        let byte_end = (offset as usize)
            .checked_add(
                (stride as usize)
                    .checked_mul(height_usize)
                    .ok_or_else(|| format!("wl_shm buffer {buffer_id} byte size overflow"))?,
            )
            .ok_or_else(|| format!("wl_shm buffer {buffer_id} byte range overflow"))?;
        if byte_end > pool.size {
            return Err(format!(
                "wl_shm buffer {buffer_id} range ends at {byte_end}, beyond pool size {}",
                pool.size
            ));
        }
        let fd = duplicate_fd(&pool.fd)?;
        self.buffers.insert(
            buffer_id,
            Arc::new(TrackedBuffer {
                width: width as u32,
                height: height as u32,
                rgba: None,
                source: TrackedBufferSource::Shm(TrackedShmBuffer {
                    fd,
                    offset,
                    width: width as u32,
                    height: height as u32,
                    stride: stride as u32,
                    format,
                }),
            }),
        );
        trace_wayland_proxy(format_args!(
            "shm create_buffer buffer={buffer_id} pool={pool_id} size={width}x{height} stride={stride} format=0x{format:08x}"
        ));
        Ok(())
    }

    fn note_dmabuf_params_created(&mut self, params_id: u32) {
        self.dmabuf_params.insert(
            params_id,
            PendingDmabufParams {
                planes: Vec::new(),
                pending_create: None,
            },
        );
    }

    fn destroy_dmabuf_params(&mut self, params_id: u32) {
        self.dmabuf_params.remove(&params_id);
    }

    fn note_dmabuf_plane(
        &mut self,
        params_id: u32,
        plane: TrackedDmabufPlane,
    ) -> Result<(), String> {
        let params = self
            .dmabuf_params
            .get_mut(&params_id)
            .ok_or_else(|| format!("missing dmabuf params object {params_id}"))?;
        params.planes.push(plane);
        params.planes.sort_by_key(|plane| plane.plane_idx);
        Ok(())
    }

    fn note_dmabuf_create_immed(
        &mut self,
        params_id: u32,
        buffer_id: u32,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> Result<(), String> {
        let params = self
            .dmabuf_params
            .get(&params_id)
            .ok_or_else(|| format!("missing dmabuf params object {params_id}"))?;
        let planes = params
            .planes
            .iter()
            .map(|plane| {
                Ok(TrackedDmabufPlane {
                    fd: duplicate_fd(&plane.fd)?,
                    plane_idx: plane.plane_idx,
                    offset: plane.offset,
                    stride: plane.stride,
                    modifier: plane.modifier,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        trace_wayland_proxy(format_args!(
            "dmabuf create_immed buffer={buffer_id} size={width}x{height} format=0x{format:08x} planes={} modifiers={:?}",
            planes.len(),
            planes
                .iter()
                .map(|plane| format!("0x{:016x}", plane.modifier))
                .collect::<Vec<_>>()
        ));
        self.buffers.insert(
            buffer_id,
            Arc::new(TrackedBuffer {
                width,
                height,
                rgba: None,
                source: TrackedBufferSource::Dmabuf(TrackedDmabufBuffer {
                    width,
                    height,
                    format,
                    flags,
                    planes,
                }),
            }),
        );
        Ok(())
    }

    fn note_dmabuf_create(
        &mut self,
        params_id: u32,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> Result<(), String> {
        let params = self
            .dmabuf_params
            .get_mut(&params_id)
            .ok_or_else(|| format!("missing dmabuf params object {params_id}"))?;
        params.pending_create = Some(PendingDmabufCreate {
            width,
            height,
            format,
            flags,
        });
        Ok(())
    }

    fn note_dmabuf_created_from_event(
        &mut self,
        params_id: u32,
        buffer_id: u32,
    ) -> Result<(), String> {
        let params = self
            .dmabuf_params
            .get(&params_id)
            .ok_or_else(|| format!("missing dmabuf params object {params_id}"))?;
        let pending_create = params.pending_create.ok_or_else(|| {
            format!("dmabuf params object {params_id} has no pending create state")
        })?;
        let planes = params
            .planes
            .iter()
            .map(|plane| {
                Ok(TrackedDmabufPlane {
                    fd: duplicate_fd(&plane.fd)?,
                    plane_idx: plane.plane_idx,
                    offset: plane.offset,
                    stride: plane.stride,
                    modifier: plane.modifier,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        trace_wayland_proxy(format_args!(
            "dmabuf created buffer={buffer_id} size={}x{} format=0x{:08x} planes={} modifiers={:?}",
            pending_create.width,
            pending_create.height,
            pending_create.format,
            planes.len(),
            planes
                .iter()
                .map(|plane| format!("0x{:016x}", plane.modifier))
                .collect::<Vec<_>>()
        ));
        self.buffers.insert(
            buffer_id,
            Arc::new(TrackedBuffer {
                width: pending_create.width,
                height: pending_create.height,
                rgba: None,
                source: TrackedBufferSource::Dmabuf(TrackedDmabufBuffer {
                    width: pending_create.width,
                    height: pending_create.height,
                    format: pending_create.format,
                    flags: pending_create.flags,
                    planes,
                }),
            }),
        );
        Ok(())
    }

    fn pending_surface_mut(&mut self, id: u32) -> &mut TrackedSurface {
        if !self.pending_surfaces.contains_key(&id) {
            let current = self
                .cached_surfaces
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.surface_mut(id).clone());
            self.pending_surfaces.insert(id, current);
        }
        self.pending_surfaces.get_mut(&id).unwrap()
    }
    fn set_surface_buffer(&mut self, surface_id: u32, buffer_id: Option<u32>) {
        self.ensure_window_for_surface(surface_id);
        let buffer = buffer_id.and_then(|id| self.buffers.get(&id)).cloned();
        let surface = self.pending_surface_mut(surface_id);
        surface.buffer_id = buffer_id;
        surface.attach_pending = true;
        surface.buffer_ref = buffer.clone();
        surface.linear_rgba = Arc::new(Vec::new());
        surface.has_committed_buffer = buffer_id.is_some();
        if let Some(buffer) = buffer {
            surface.width = buffer.width.max(1);
            surface.height = buffer.height.max(1);
            surface.rgba = buffer.rgba.clone().unwrap_or_default();
            surface.buffer_kind = Some(buffer.kind_name());
            surface.capture_error = buffer.capture_error();
        } else {
            surface.width = 1;
            surface.height = 1;
            surface.rgba.clear();
            surface.buffer_kind = None;
            surface.capture_error = None;
        }
    }
    fn destroy_buffer(&mut self, buffer_id: u32) {
        self.buffers.remove(&buffer_id);
    }
    fn add_damage(&mut self, surface_id: u32, rect: DamageRect) {
        self.pending_surface_mut(surface_id).damage.push(rect);
    }
    fn effectively_synchronized(&self, mut surface: u32) -> bool {
        for _ in 0..256 {
            if self.synchronized.contains(&surface) {
                return true;
            }
            let Some(parent) = self.surface_parent.get(&surface) else {
                return false;
            };
            surface = *parent;
        }
        true
    }
    fn commit_surface(&mut self, surface_id: u32) {
        if let Some(position) = self.pending_popup_positions.remove(&surface_id) {
            self.popup_positions.insert(surface_id, position);
        }
        self.ensure_window_for_surface(surface_id);
        let mut surface = self
            .pending_surfaces
            .remove(&surface_id)
            .unwrap_or_else(|| {
                self.cached_surfaces
                    .get(&surface_id)
                    .cloned()
                    .unwrap_or_else(|| self.surface_mut(surface_id).clone())
            });
        let color = self.colors.get(surface_id).cloned();
        if surface.color != color {
            // Retained raw pixels are still valid, but decoded pixels must use
            // the description committed with this surface state.
            surface.linear_rgba = Arc::new(Vec::new());
            if surface.buffer_kind == Some("dmabuf") {
                surface.capture_error =
                    Some("snapshot_unavailable: color interpretation changed".into());
            }
        }
        surface.color = color;
        if let Some(buffer) = surface.buffer_ref.as_ref() {
            if surface.attach_pending || !surface.damage.is_empty() {
                surface.pixel_commit_serial = self.next_commit_serial.saturating_add(1);
            }
            match &buffer.source {
                TrackedBufferSource::Shm(shm) => {
                    if cfg!(test) {
                        match shm.read_rgba() {
                            Ok(frame) => {
                                surface.width = frame.width;
                                surface.height = frame.height;
                                surface.rgba = frame.rgba;
                                surface.capture_error = None;
                            }
                            Err(error) => {
                                surface.rgba.clear();
                                surface.capture_error = Some(error);
                            }
                        }
                    } else {
                        surface.rgba.clear();
                        surface.capture_error = Some(
                            "CPU pixel snapshots are disabled; DMA-BUF visual events required"
                                .into(),
                        );
                    }
                }
                TrackedBufferSource::Dmabuf(dmabuf) => {
                    surface.rgba.clear();
                    let result = self.prepared_gpu.remove(&surface_id);
                    if let Some(result) = result {
                        surface.linear_rgba = Arc::new(Vec::new());
                        match result {
                            Ok(pixels) => {
                                surface.linear_rgba = Arc::new(pixels);
                                surface.capture_error = None;
                            }
                            Err(error) => surface.capture_error = Some(error),
                        }
                    } else if surface.attach_pending || !surface.damage.is_empty() {
                        surface.linear_rgba = Arc::new(Vec::new());
                        let _ = dmabuf;
                        surface.capture_error = Some(
                            "snapshot_unavailable: enable capture before the next GPU commit"
                                .into(),
                        );
                    }
                }
                TrackedBufferSource::Unknown => {}
            }
        }
        surface.attach_pending = false;
        surface.last_acquire = surface.pending_acquire.take();
        surface.last_release = surface.pending_release.take();
        surface.damage.clear();
        if self.effectively_synchronized(surface_id) {
            self.cached_surfaces.insert(surface_id, surface);
            return;
        }
        self.latch_surface(surface_id, surface);
        self.latch_children(surface_id);
    }
    fn latch_surface(&mut self, id: u32, mut surface: TrackedSurface) {
        self.next_commit_serial = self.next_commit_serial.saturating_add(1);
        surface.commit_serial = self.next_commit_serial;
        if surface.buffer_id.is_some() {
            surface.has_committed_buffer = true;
        }
        self.surfaces.insert(id, surface);
        self.sync_window_from_surface(id);
    }
    fn latch_children(&mut self, parent: u32) {
        if let Some(order) = self.pending_stacking.remove(&parent) {
            self.stacking.insert(parent, order);
        }
        let children = self
            .surface_parent
            .iter()
            .filter_map(|(child, p)| (*p == parent).then_some(*child))
            .collect::<Vec<_>>();
        for child in children {
            if let Some(position) = self.pending_positions.remove(&child) {
                self.surface_position.insert(child, position);
            }
            if self.effectively_synchronized(child) {
                if let Some(surface) = self.cached_surfaces.remove(&child) {
                    self.latch_surface(child, surface);
                }
                self.latch_children(child);
            }
        }
    }
    fn note_subsurface_sync(&mut self, subsurface: u32, sync: bool) {
        if let Some(surface) = self.subsurface_to_surface.get(&subsurface).copied() {
            if sync {
                self.synchronized.insert(surface);
            } else {
                self.synchronized.remove(&surface);
                if !self.effectively_synchronized(surface) {
                    if let Some(state) = self.cached_surfaces.remove(&surface) {
                        self.latch_surface(surface, state);
                    }
                    self.latch_children(surface);
                }
            }
        }
    }

    fn latest_surface(&self) -> Option<&TrackedSurface> {
        self.surfaces
            .values()
            .max_by_key(|surface| surface.commit_serial)
    }

    fn subsurfaces_for_window(&self, window_id: &str) -> Vec<GuiSubsurfaceInfo> {
        let Some(window) = self.windows.get(window_id) else {
            return Vec::new();
        };
        let root = window.input_surface_id;
        let mut result = self
            .subsurface_to_surface
            .iter()
            .filter_map(|(&role, &id)| {
                if !self.surface_descends_from(id, root) {
                    return None;
                }
                let parent = *self.surface_parent.get(&id)?;
                let surface = self.surfaces.get(&id);
                // Use the retained committed buffer reference, even after the client
                // destroys its wl_buffer object. Pending attachments are not commits.
                let buffer = surface.and_then(|s| s.buffer_ref.as_ref());
                Some(GuiSubsurfaceInfo {
                    subsurface_id: role,
                    surface_id: id,
                    parent_surface_id: parent,
                    position: self.surface_position.get(&id).copied().unwrap_or_default(),
                    synchronized: self.effectively_synchronized(id),
                    has_committed_buffer: surface.is_some_and(|s| s.has_committed_buffer),
                    buffer_id: surface.and_then(|s| s.buffer_id),
                    buffer_kind: surface.and_then(|s| s.buffer_kind.map(str::to_string)),
                    buffer_width: buffer.map(|b| b.width),
                    buffer_height: buffer.map(|b| b.height),
                    commit_serial: surface.map_or(0, |s| s.commit_serial),
                    capture_details: buffer.map(|b| b.capture_details()),
                    capture_error: surface.and_then(|s| s.capture_error.clone()),
                })
            })
            .collect::<Vec<_>>();
        result.sort_by_key(|s| s.surface_id);
        result
    }

    fn list_windows(&self) -> Vec<GuiWindowInfo> {
        let mut windows = self
            .windows
            .values()
            .filter(|window| window.xdg_surface_id.is_some())
            .map(|window| {
                let surface = self.surfaces.get(&window.wl_surface_id);
                let buffer = surface
                    .and_then(|surface| surface.buffer_id)
                    .and_then(|buffer_id| self.buffers.get(&buffer_id));
                let subsurfaces = self.subsurfaces_for_window(&window.window_id);
                let capture_error = self.scene_capture_error(&window.window_id);
                let capturable = capture_error.is_none();
                let capture_output_count =
                    usize::from(window.mapped && window.commit_serial > 0 && capturable);
                GuiWindowInfo {
                    window_id: window.window_id.clone(),
                    title: window.title.clone(),
                    app_id: window.app_id.clone(),
                    width: window.width.max(1),
                    height: window.height.max(1),
                    mapped: window.mapped,
                    commit_serial: window.commit_serial,
                    on_capture_output: capture_output_count > 0,
                    capture_output_count,
                    on_backend_output: window.output_count > 0,
                    backend_output_count: window.output_count,
                    buffer_kind: surface
                        .and_then(|surface| surface.buffer_kind.map(str::to_string)),
                    subsurface_count: subsurfaces.len(),
                    subsurfaces,
                    sync_state: surface.and_then(TrackedSurface::sync_state),
                    capturable,
                    capture_error,
                    render_surface_id: window.wl_surface_id,
                    input_surface_id: window.input_surface_id,
                    capture_details: buffer.map(|buffer| buffer.capture_details()),
                }
            })
            .collect::<Vec<_>>();
        windows.sort_by(|left, right| {
            right
                .commit_serial
                .cmp(&left.commit_serial)
                .then_with(|| left.window_id.cmp(&right.window_id))
        });
        windows
    }

    fn list_capturable_windows(&self) -> Vec<GuiWindowInfo> {
        self.list_windows()
            .into_iter()
            .filter(|window| {
                window.mapped
                    && window.commit_serial > 0
                    && self
                        .surface_for_window(&window.window_id)
                        .map(|surface| !surface.rgba.is_empty())
                        .unwrap_or(false)
            })
            .collect()
    }

    fn has_visible_dmabuf_window(&self) -> bool {
        self.windows.values().any(|window| {
            window.mapped
                && window.output_count > 0
                && self
                    .surfaces
                    .get(&window.wl_surface_id)
                    .map(|surface| matches!(surface.buffer_kind, Some("dmabuf")))
                    .unwrap_or(false)
        })
    }

    fn latest_commit_serial(&self) -> u64 {
        self.latest_surface()
            .map(|surface| surface.commit_serial)
            .unwrap_or(0)
    }

    fn surface_mut(&mut self, surface_id: u32) -> &mut TrackedSurface {
        self.surfaces
            .entry(surface_id)
            .or_insert_with(|| TrackedSurface {
                buffer_ref: None,
                id: surface_id,
                buffer_id: None,
                width: 1,
                height: 1,
                rgba: Vec::new(),
                linear_rgba: Arc::new(Vec::new()),
                color: None,
                output_ids: BTreeSet::new(),
                buffer_kind: None,
                capture_error: None,
                pending_acquire: None,
                pending_release: None,
                last_acquire: None,
                last_release: None,
                damage: Vec::new(),
                commit_serial: 0,
                pixel_commit_serial: 0,
                has_committed_buffer: false,
                attach_pending: false,
                xdg_configure_seen: false,
                xdg_configure_acked: false,
                viewport_destination: None,
                window_geometry: None,
                window_geometry_offset: (0, 0),
                buffer_scale: 1,
                buffer_transform: 0,
                viewport_source: None,
                offset: (0, 0),
                input_region: None,
            })
    }

    fn note_xdg_surface_created(&mut self, xdg_surface_id: u32, wl_surface_id: u32) {
        self.xdg_surface_to_surface
            .insert(xdg_surface_id, wl_surface_id);
    }

    fn note_surface_destroyed(&mut self, surface_id: u32) {
        self.prepared_gpu.remove(&surface_id);
        self.popup_parent.remove(&surface_id);
        self.popup_objects
            .retain(|_, surface| *surface != surface_id);
        self.popup_order.retain(|surface| *surface != surface_id);
        self.popup_positions.remove(&surface_id);
        self.pending_popup_positions.remove(&surface_id);
        self.dismissed_popups.remove(&surface_id);
        self.surfaces.remove(&surface_id);
        self.pending_surfaces.remove(&surface_id);
        self.cached_surfaces.remove(&surface_id);
        self.synchronized.remove(&surface_id);
        self.pending_positions.remove(&surface_id);
        if let Some(window_id) = self.surface_to_window.remove(&surface_id)
            && self
                .windows
                .get(&window_id)
                .is_some_and(|window| window.input_surface_id == surface_id)
        {
            self.windows.remove(&window_id);
            self.xdg_toplevel_to_window.retain(|_, id| id != &window_id);
            self.surface_to_window.retain(|_, id| id != &window_id);
        }
        self.xdg_surface_to_surface
            .retain(|_, id| *id != surface_id);
        self.subsurface_to_surface.retain(|_, id| *id != surface_id);
        self.surface_parent.remove(&surface_id);
        self.surface_position.remove(&surface_id);
        for order in self
            .stacking
            .values_mut()
            .chain(self.pending_stacking.values_mut())
        {
            order.retain(|id| *id != surface_id);
        }
        self.stacking.remove(&surface_id);
        self.pending_stacking.remove(&surface_id);
        self.viewport_to_surface.retain(|_, id| *id != surface_id);
        self.syncobj_surface_to_surface
            .retain(|_, id| *id != surface_id);
    }

    fn note_popup_created(&mut self, xdg: u32, popup: u32, parent_xdg: u32) -> Result<(), String> {
        let surface = *self
            .xdg_surface_to_surface
            .get(&xdg)
            .ok_or("unknown popup xdg surface")?;
        let parent = *self
            .xdg_surface_to_surface
            .get(&parent_xdg)
            .ok_or("unknown popup parent")?;
        let window = self.ensure_window_for_surface(parent);
        if let Some(old) = self.surface_to_window.insert(surface, window.clone())
            && old != window
        {
            self.windows.remove(&old);
        }
        self.popup_parent.insert(surface, parent);
        self.popup_objects.insert(popup, surface);
        self.popup_order.push(surface);
        self.dismissed_popups.remove(&surface);
        Ok(())
    }

    fn note_subsurface_created(
        &mut self,
        subsurface_id: u32,
        surface_id: u32,
        parent_surface_id: u32,
    ) {
        self.subsurface_to_surface.insert(subsurface_id, surface_id);
        self.surface_parent.insert(surface_id, parent_surface_id);
        self.synchronized.insert(surface_id);
        self.stacking
            .entry(parent_surface_id)
            .or_insert_with(|| vec![parent_surface_id])
            .push(surface_id);
        self.surface_position.entry(surface_id).or_insert((0, 0));

        let parent_window_id = self.ensure_window_for_surface(parent_surface_id);
        let child_window_id = self.surface_to_window.get(&surface_id).cloned();
        self.surface_to_window
            .insert(surface_id, parent_window_id.clone());
        if let Some(child_window_id) = child_window_id
            && child_window_id != parent_window_id
        {
            self.windows.remove(&child_window_id);
        }
        self.sync_window_from_surface(surface_id);
    }

    fn note_subsurface_destroyed(&mut self, subsurface_id: u32) {
        if let Some(surface_id) = self.subsurface_to_surface.remove(&subsurface_id) {
            self.surface_parent.remove(&surface_id);
            self.synchronized.remove(&surface_id);
            self.cached_surfaces.remove(&surface_id);
            self.surface_position.remove(&surface_id);
        }
    }

    fn note_subsurface_position(&mut self, subsurface_id: u32, x: i32, y: i32) {
        if let Some(surface_id) = self.subsurface_to_surface.get(&subsurface_id).copied() {
            self.pending_positions.insert(surface_id, (x, y));
        }
    }

    fn note_subsurface_stacking(
        &mut self,
        object: u32,
        sibling: u32,
        above: bool,
    ) -> Result<(), String> {
        let child = *self
            .subsurface_to_surface
            .get(&object)
            .ok_or("unknown subsurface")?;
        let parent = *self
            .surface_parent
            .get(&child)
            .ok_or("subsurface has no parent")?;
        if sibling == child
            || (sibling != parent && self.surface_parent.get(&sibling) != Some(&parent))
        {
            return Err("subsurface stacking target must be parent or sibling".into());
        }
        let order = self.pending_stacking.entry(parent).or_insert_with(|| {
            self.stacking
                .get(&parent)
                .cloned()
                .unwrap_or_else(|| vec![parent])
        });
        order.retain(|id| *id != child);
        let index = order
            .iter()
            .position(|id| *id == sibling)
            .ok_or("stacking target is not live")?;
        order.insert(index + usize::from(above), child);
        Ok(())
    }
    fn surface_descends_from(&self, mut surface_id: u32, ancestor_id: u32) -> bool {
        let mut visited = HashSet::new();
        while visited.insert(surface_id) {
            let Some(parent_id) = self.surface_parent.get(&surface_id).copied() else {
                return false;
            };
            if parent_id == ancestor_id {
                return true;
            }
            surface_id = parent_id;
        }
        false
    }

    fn note_viewport_created(&mut self, viewport_id: u32, wl_surface_id: u32) {
        self.viewport_to_surface.insert(viewport_id, wl_surface_id);
        self.ensure_window_for_surface(wl_surface_id);
    }

    fn note_viewport_destroyed(&mut self, viewport_id: u32) {
        if let Some(wl_surface_id) = self.viewport_to_surface.remove(&viewport_id) {
            self.pending_surface_mut(wl_surface_id).viewport_destination = None;
            self.pending_surface_mut(wl_surface_id).viewport_source = None;
            self.sync_window_from_surface(wl_surface_id);
        }
    }

    fn note_viewport_destination(
        &mut self,
        viewport_id: u32,
        width: i32,
        height: i32,
    ) -> Result<(), String> {
        let Some(wl_surface_id) = self.viewport_to_surface.get(&viewport_id).copied() else {
            return Err(format!("missing wl_surface for wp_viewport {viewport_id}"));
        };
        let destination = if width == -1 && height == -1 {
            None
        } else {
            Some((clamp_dimension(width), clamp_dimension(height)))
        };
        self.pending_surface_mut(wl_surface_id).viewport_destination = destination;
        self.sync_window_from_surface(wl_surface_id);
        Ok(())
    }

    fn note_xdg_toplevel_created(
        &mut self,
        xdg_surface_id: u32,
        xdg_toplevel_id: u32,
    ) -> Result<String, String> {
        let Some(&wl_surface_id) = self.xdg_surface_to_surface.get(&xdg_surface_id) else {
            return Err(format!(
                "missing wl_surface for xdg_surface {xdg_surface_id}"
            ));
        };
        let window_id = self.ensure_window_for_surface(wl_surface_id);
        self.xdg_toplevel_to_window
            .insert(xdg_toplevel_id, window_id.clone());
        let surface = self.surface_mut(wl_surface_id).clone();
        let window = self
            .windows
            .entry(window_id.clone())
            .or_insert_with(|| TrackedWindow {
                window_id: window_id.clone(),
                wl_surface_id,
                input_surface_id: wl_surface_id,
                xdg_surface_id: Some(xdg_surface_id),
                xdg_toplevel_id: Some(xdg_toplevel_id),
                title: None,
                app_id: None,
                width: surface.width,
                height: surface.height,
                mapped: false,
                commit_serial: surface.commit_serial,
                output_count: surface.output_ids.len(),
            });
        window.wl_surface_id = wl_surface_id;
        window.input_surface_id = wl_surface_id;
        window.xdg_surface_id = Some(xdg_surface_id);
        window.xdg_toplevel_id = Some(xdg_toplevel_id);
        window.width = surface.width.max(1);
        window.height = surface.height.max(1);
        window.commit_serial = surface.commit_serial;
        Ok(window_id)
    }

    fn note_xdg_toplevel_title(&mut self, xdg_toplevel_id: u32, title: Option<String>) {
        if let Some(window) = self.window_mut_for_toplevel(xdg_toplevel_id) {
            window.title = title;
        }
    }

    fn note_xdg_toplevel_app_id(&mut self, xdg_toplevel_id: u32, app_id: Option<String>) {
        if let Some(window) = self.window_mut_for_toplevel(xdg_toplevel_id) {
            window.app_id = app_id;
        }
    }

    fn note_window_geometry(
        &mut self,
        xdg_surface_id: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let Some(&wl_surface_id) = self.xdg_surface_to_surface.get(&xdg_surface_id) else {
            return Err(format!(
                "missing wl_surface for xdg_surface {xdg_surface_id}"
            ));
        };
        self.pending_surface_mut(wl_surface_id).window_geometry =
            Some((width.max(1), height.max(1)));
        if let Some(window_id) = self.surface_to_window.get(&wl_surface_id).cloned()
            && let Some(window) = self.windows.get_mut(&window_id)
        {
            window.width = width.max(1);
            window.height = height.max(1);
        }
        Ok(())
    }

    fn note_surface_buffer_scale(&mut self, surface_id: u32, scale: i32) {
        self.pending_surface_mut(surface_id).buffer_scale = scale.max(1) as u32;
    }

    fn note_xdg_surface_configure(&mut self, xdg_surface_id: u32) {
        if let Some(wl_surface_id) = self.xdg_surface_to_surface.get(&xdg_surface_id).copied() {
            self.surface_mut(wl_surface_id).xdg_configure_seen = true;
            self.sync_window_from_surface(wl_surface_id);
        }
    }

    fn note_xdg_surface_ack_configure(&mut self, xdg_surface_id: u32) {
        if let Some(wl_surface_id) = self.xdg_surface_to_surface.get(&xdg_surface_id).copied() {
            self.surface_mut(wl_surface_id).xdg_configure_acked = true;
            self.sync_window_from_surface(wl_surface_id);
        }
    }

    fn surface_for_window(&self, window_id: &str) -> Option<&TrackedSurface> {
        let window = self.windows.get(window_id)?;
        self.surfaces.get(&window.wl_surface_id)
    }

    fn click_target_for_window(
        &self,
        window_id: &str,
        x: i64,
        y: i64,
    ) -> Result<Option<PointerClickTarget>, String> {
        self.scene_pointer_target(window_id, x, y)
    }
    fn capture_window_rgba(&self, window_id: &str) -> Option<Result<CapturedRgbaFrame, String>> {
        self.windows.get(window_id)?;
        Some(self.capture_scene(window_id))
    }
    fn capture_buffer_rgba(&self, window_id: &str) -> Option<Result<CapturedRgbaFrame, String>> {
        let window = self.windows.get(window_id)?;
        let surface = self.surfaces.get(&window.wl_surface_id)?;
        Some(self.capture_surface_rgba(surface))
    }

    fn surface_for_capture(
        &self,
        window_id: &str,
        surface_id: u32,
    ) -> Result<&TrackedSurface, String> {
        let window = self.windows.get(window_id).ok_or("unknown windowId")?;
        if !window.mapped {
            return Err("capture window is not mapped".into());
        }
        if surface_id != window.input_surface_id
            && !self.surface_descends_from(surface_id, window.input_surface_id)
        {
            return Err("surfaceId does not belong to the window's live subsurface tree".into());
        }
        self.surfaces
            .get(&surface_id)
            .ok_or_else(|| "surfaceId has no committed state".to_string())
    }

    fn capture_surface_rgba(&self, surface: &TrackedSurface) -> Result<CapturedRgbaFrame, String> {
        let pixels = surface.width as usize * surface.height as usize;
        let hdr = surface
            .color
            .as_ref()
            .is_some_and(crate::gui_color::ColorDescription::is_hdr);
        let rgba = if surface.linear_rgba.len() == pixels {
            surface
                .linear_rgba
                .iter()
                .flat_map(|pixel| crate::gui_color::encode_preview(*pixel, hdr))
                .collect()
        } else if surface.rgba.len() == pixels * 4 {
            surface.rgba.clone()
        } else {
            return Err(surface.capture_error.clone().unwrap_or_else(|| {
                "snapshot_unavailable: render buffer requires a fresh producer commit".into()
            }));
        };
        Ok(CapturedRgbaFrame {
            width: surface.width,
            height: surface.height,
            rgba,
            color: Some(
                serde_json::json!({"coordinate_space":"buffer","surface_id":surface.id,"commit_serial":surface.commit_serial,"tone_mapped":hdr,
                "source":surface.color.as_ref().map(|color|color.metadata())}),
            ),
        })
    }

    fn wait_sync_point(&self, point: TrackedSyncPoint) -> Result<(), String> {
        let Some(timeline_id) = point.timeline_id else {
            return Ok(());
        };
        let timeline = self
            .syncobj_timelines
            .get(&timeline_id)
            .ok_or_else(|| format!("missing syncobj timeline {timeline_id}"))?;
        wait_drm_syncobj_timeline(&timeline.fd, point.point)
    }

    fn ensure_window_for_surface(&mut self, surface_id: u32) -> String {
        if let Some(window_id) = self.surface_to_window.get(&surface_id).cloned() {
            return window_id;
        }
        self.next_window_id = self.next_window_id.saturating_add(1);
        let window_id = format!("{}-window-{}", self.window_id_prefix, self.next_window_id);
        let surface = self.surface_mut(surface_id).clone();
        self.surface_to_window.insert(surface_id, window_id.clone());
        self.windows.insert(
            window_id.clone(),
            TrackedWindow {
                window_id: window_id.clone(),
                wl_surface_id: surface_id,
                input_surface_id: surface_id,
                xdg_surface_id: None,
                xdg_toplevel_id: None,
                title: None,
                app_id: None,
                width: surface.width.max(1),
                height: surface.height.max(1),
                mapped: surface.has_committed_buffer,
                commit_serial: surface.commit_serial,
                output_count: surface.output_ids.len(),
            },
        );
        window_id
    }

    fn sync_window_from_surface(&mut self, surface_id: u32) {
        let window_id = self.ensure_window_for_surface(surface_id);
        let root = self.windows[&window_id].input_surface_id;
        let Some(surface) = self.surfaces.get(&root).cloned() else {
            return;
        };
        let serial = self
            .surfaces
            .iter()
            .filter(|(id, _)| self.surface_to_window.get(id) == Some(&window_id))
            .map(|(_, s)| s.commit_serial)
            .max()
            .unwrap_or(0);
        let geometry = self
            .observation_scene(&window_id)
            .ok()
            .map(|scene| scene.size);
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.wl_surface_id = root;
            (window.width, window.height) =
                geometry.unwrap_or((surface.width.max(1), surface.height.max(1)));
            window.commit_serial = serial;
            window.mapped = surface.has_committed_buffer;
            window.output_count = surface.output_ids.len();
        }
    }

    fn window_mut_for_toplevel(&mut self, xdg_toplevel_id: u32) -> Option<&mut TrackedWindow> {
        let window_id = self.xdg_toplevel_to_window.get(&xdg_toplevel_id)?.clone();
        self.windows.get_mut(&window_id)
    }

    fn note_surface_enter(&mut self, surface_id: u32, output_id: u32) {
        self.surface_mut(surface_id).output_ids.insert(output_id);
        self.sync_window_from_surface(surface_id);
    }

    fn note_surface_leave(&mut self, surface_id: u32, output_id: u32) {
        self.surface_mut(surface_id).output_ids.remove(&output_id);
        self.sync_window_from_surface(surface_id);
    }

    fn note_syncobj_surface_created(
        &mut self,
        syncobj_surface_id: u32,
        wl_surface_id: Option<u32>,
    ) {
        if let Some(wl_surface_id) = wl_surface_id {
            self.syncobj_surface_to_surface
                .insert(syncobj_surface_id, wl_surface_id);
            self.ensure_window_for_surface(wl_surface_id);
        }
    }

    fn note_syncobj_surface_destroyed(&mut self, syncobj_surface_id: u32) {
        self.syncobj_surface_to_surface.remove(&syncobj_surface_id);
    }

    fn note_syncobj_timeline_imported(&mut self, timeline_id: u32, fd: OwnedFd) {
        self.syncobj_timelines
            .insert(timeline_id, TrackedSyncobjTimeline { fd });
    }

    fn note_syncobj_timeline_destroyed(&mut self, timeline_id: u32) {
        self.syncobj_timelines.remove(&timeline_id);
    }

    fn note_syncobj_acquire_point(
        &mut self,
        syncobj_surface_id: u32,
        timeline_id: Option<u32>,
        point: u64,
    ) -> Result<(), String> {
        let surface_id = self.surface_id_for_syncobj_surface(syncobj_surface_id)?;
        if let Some(timeline_id) = timeline_id {
            self.ensure_syncobj_timeline(timeline_id)?;
        }
        self.pending_surface_mut(surface_id).pending_acquire =
            Some(TrackedSyncPoint { timeline_id, point });
        self.sync_window_from_surface(surface_id);
        Ok(())
    }

    fn note_syncobj_release_point(
        &mut self,
        syncobj_surface_id: u32,
        timeline_id: Option<u32>,
        point: u64,
    ) -> Result<(), String> {
        let surface_id = self.surface_id_for_syncobj_surface(syncobj_surface_id)?;
        if let Some(timeline_id) = timeline_id {
            self.ensure_syncobj_timeline(timeline_id)?;
        }
        self.pending_surface_mut(surface_id).pending_release =
            Some(TrackedSyncPoint { timeline_id, point });
        self.sync_window_from_surface(surface_id);
        Ok(())
    }

    fn surface_id_for_syncobj_surface(&self, syncobj_surface_id: u32) -> Result<u32, String> {
        self.syncobj_surface_to_surface
            .get(&syncobj_surface_id)
            .copied()
            .ok_or_else(|| format!("missing wl_surface for syncobj surface {syncobj_surface_id}"))
    }

    fn ensure_syncobj_timeline(&self, timeline_id: u32) -> Result<(), String> {
        self.syncobj_timelines
            .get(&timeline_id)
            .map(|timeline| {
                let _ = timeline.fd.as_raw_fd();
            })
            .ok_or_else(|| format!("missing syncobj timeline {timeline_id}"))
    }
}

impl TrackedBuffer {
    fn capture_details(&self) -> String {
        match &self.source {
            TrackedBufferSource::Unknown => "unknown buffer backing".to_string(),
            TrackedBufferSource::Shm(buffer) => format!(
                "wl_shm format=0x{:08x} stride={} offset={}",
                buffer.format, buffer.stride, buffer.offset
            ),
            TrackedBufferSource::Dmabuf(buffer) => format!(
                "Vulkan DMA-BUF format=0x{:08x} planes={} modifiers=[{}]",
                buffer.format,
                buffer.planes.len(),
                buffer
                    .planes
                    .iter()
                    .map(|plane| format!("0x{:016x}", plane.modifier))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    fn kind_name(&self) -> &'static str {
        match self.source {
            TrackedBufferSource::Unknown => "unknown",
            TrackedBufferSource::Dmabuf(_) => "dmabuf",
            TrackedBufferSource::Shm(_) => "shm",
        }
    }

    fn capture_error(&self) -> Option<String> {
        match &self.source {
            TrackedBufferSource::Dmabuf(buffer) => buffer.capture_error(),
            TrackedBufferSource::Shm(buffer) => buffer.capture_error(),
            TrackedBufferSource::Unknown if self.rgba.is_none() => {
                Some("window buffer has no readable pixel backing yet".to_string())
            }
            _ => None,
        }
    }

    fn has_readback(&self) -> bool {
        match &self.source {
            TrackedBufferSource::Dmabuf(buffer) => buffer.capture_error().is_none(),
            TrackedBufferSource::Shm(buffer) => buffer.capture_error().is_none(),
            TrackedBufferSource::Unknown => self.rgba.is_some(),
        }
    }

    fn read_rgba(
        &self,
        surface: &TrackedSurface,
        color: Option<&crate::gui_color::ColorDescription>,
    ) -> Result<CapturedRgbaFrame, String> {
        match &self.source {
            TrackedBufferSource::Dmabuf(buffer) => {
                let rgba = read_vulkan_dmabuf_rgba(buffer, color)?;
                Ok(CapturedRgbaFrame {
                    width: surface.width,
                    height: surface.height,
                    rgba,
                    color: color.map(crate::gui_color::ColorDescription::metadata),
                })
            }
            TrackedBufferSource::Shm(buffer) => {
                let mut frame = buffer.read_rgba()?;
                convert_rgba8_colors(&mut frame.rgba, color);
                frame.color = color.map(crate::gui_color::ColorDescription::metadata);
                Ok(frame)
            }
            TrackedBufferSource::Unknown => {
                Err("window buffer has no readable pixel backing yet".to_string())
            }
        }
    }
}

#[derive(Debug)]
struct TrackedBuffer {
    width: u32,
    height: u32,
    rgba: Option<Vec<u8>>,
    source: TrackedBufferSource,
}

#[derive(Debug)]
enum TrackedBufferSource {
    Unknown,
    Dmabuf(TrackedDmabufBuffer),
    Shm(TrackedShmBuffer),
}

#[derive(Debug)]
struct TrackedShmPool {
    fd: OwnedFd,
    size: usize,
}

#[derive(Debug)]
struct TrackedShmBuffer {
    fd: OwnedFd,
    offset: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

impl TrackedShmBuffer {
    // Core wl_shm ARGB8888/XRGB8888 values. The bytes are native-endian
    // 0xAARRGGBB/0x00RRGGBB, hence BGRA/BGRX on little-endian hosts.
    const ARGB8888: u32 = 0;
    const XRGB8888: u32 = 1;

    fn capture_error(&self) -> Option<String> {
        if !cfg!(target_endian = "little") {
            return Some("wl_shm capture currently requires a little-endian host".to_string());
        }
        if !matches!(self.format, Self::ARGB8888 | Self::XRGB8888) {
            return Some(format!(
                "wl_shm format 0x{:08x} is not supported for capture and was not advertised by this MCP",
                self.format
            ));
        }
        if self.stride < self.width.saturating_mul(4) {
            return Some(format!(
                "wl_shm stride {} is too small for width {}",
                self.stride, self.width
            ));
        }
        None
    }

    fn read_rgba(&self) -> Result<CapturedRgbaFrame, String> {
        if let Some(err) = self.capture_error() {
            return Err(err);
        }
        let row_bytes = usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| "wl_shm row byte count overflow".to_string())?;
        let output_len = row_bytes
            .checked_mul(self.height as usize)
            .ok_or_else(|| "wl_shm output byte count overflow".to_string())?;
        if output_len > 64 * 1024 * 1024 {
            return Err("SHM snapshot exceeds 64 MiB".into());
        }
        let mut rgba = vec![0_u8; output_len];
        let file = std::fs::File::from(duplicate_fd(&self.fd)?);
        let mut source = vec![0_u8; row_bytes];
        for row in 0..self.height as usize {
            let row_offset = u64::from(self.offset)
                .checked_add((row as u64).saturating_mul(u64::from(self.stride)))
                .ok_or_else(|| "wl_shm row offset overflow".to_string())?;
            file.read_exact_at(&mut source, row_offset).map_err(|err| {
                format!("failed to read wl_shm row {row} at offset {row_offset}: {err}")
            })?;
            let output = &mut rgba[row * row_bytes..(row + 1) * row_bytes];
            let (source_pixels, source_remainder) = source.as_chunks::<4>();
            let (output_pixels, output_remainder) = output.as_chunks_mut::<4>();
            debug_assert!(source_remainder.is_empty());
            debug_assert!(output_remainder.is_empty());
            for (src, dst) in source_pixels.iter().zip(output_pixels) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = if self.format == Self::ARGB8888 {
                    src[3]
                } else {
                    255
                };
            }
        }
        Ok(CapturedRgbaFrame {
            width: self.width,
            height: self.height,
            rgba,
            color: None,
        })
    }
}

#[derive(Debug)]
struct TrackedDmabufBuffer {
    width: u32,
    height: u32,
    format: u32,
    flags: u32,
    planes: Vec<TrackedDmabufPlane>,
}

impl TrackedDmabufBuffer {
    fn capture_error(&self) -> Option<String> {
        crate::gui_vulkan_dmabuf::screenshot_support_error(self.format)
    }
}

#[derive(Debug)]
struct TrackedDmabufPlane {
    fd: OwnedFd,
    plane_idx: u32,
    offset: u32,
    stride: u32,
    modifier: u64,
}

#[derive(Debug)]
struct CapturedRgbaFrame {
    color: Option<serde_json::Value>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Debug)]
struct TrackedSyncobjTimeline {
    fd: OwnedFd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackedSyncPoint {
    timeline_id: Option<u32>,
    point: u64,
}

#[derive(Debug)]
struct PendingDmabufParams {
    planes: Vec<TrackedDmabufPlane>,
    pending_create: Option<PendingDmabufCreate>,
}

#[derive(Debug, Clone, Copy)]
struct PendingDmabufCreate {
    width: u32,
    height: u32,
    format: u32,
    flags: u32,
}

#[derive(Debug, Clone)]
struct TrackedSurface {
    buffer_ref: Option<Arc<TrackedBuffer>>,
    id: u32,
    buffer_id: Option<u32>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    linear_rgba: Arc<Vec<[f32; 4]>>,
    color: Option<crate::gui_color::ColorDescription>,
    output_ids: BTreeSet<u32>,
    buffer_kind: Option<&'static str>,
    capture_error: Option<String>,
    pending_acquire: Option<TrackedSyncPoint>,
    pending_release: Option<TrackedSyncPoint>,
    last_acquire: Option<TrackedSyncPoint>,
    last_release: Option<TrackedSyncPoint>,
    damage: Vec<DamageRect>,
    commit_serial: u64,
    // Last commit that changed pixels; empty commits keep the owned GPU copy.
    pixel_commit_serial: u64,
    has_committed_buffer: bool,
    attach_pending: bool,
    xdg_configure_seen: bool,
    xdg_configure_acked: bool,
    viewport_destination: Option<(u32, u32)>,
    window_geometry: Option<(u32, u32)>,
    window_geometry_offset: (i32, i32),
    buffer_scale: u32,
    buffer_transform: u32,
    viewport_source: Option<[i32; 4]>,
    offset: (i32, i32),
    input_region: Option<Vec<DamageRect>>,
}

impl TrackedSurface {
    fn retained_snapshot_pixels(
        &self,
        snapshot: &crate::visual_events::SnapshotPixels,
    ) -> Result<Vec<[f32; 4]>, String> {
        if snapshot.serial != self.pixel_commit_serial
            || snapshot.width != self.width
            || snapshot.height != self.height
            || self.buffer_ref.as_ref().is_none_or(|buffer| {
                !matches!(&buffer.source, TrackedBufferSource::Dmabuf(dmabuf)
                    if dmabuf.format == snapshot.format)
            })
        {
            return Err("snapshot_unavailable: retained GPU frame is stale".into());
        }
        crate::gui_vulkan_dmabuf::copied_dmabuf_pixels_to_linear(
            &snapshot.raw,
            snapshot.width,
            snapshot.height,
            snapshot.format,
            self.color.as_ref(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PointerClickTarget {
    fixed_coords: Option<(i32, i32)>,
    window_id: String,
    surface_id: u32,
    screenshot_x: i64,
    screenshot_y: i64,
    surface_x: i64,
    surface_y: i64,
}

impl PointerClickTarget {
    fn wire_x(&self) -> i32 {
        self.fixed_coords
            .map(|p| p.0)
            .unwrap_or_else(|| fixed_from_i64(self.surface_x))
    }
    fn wire_y(&self) -> i32 {
        self.fixed_coords
            .map(|p| p.1)
            .unwrap_or_else(|| fixed_from_i64(self.surface_y))
    }
}

impl TrackedSurface {
    fn sync_state(&self) -> Option<String> {
        let acquire = self
            .last_acquire
            .or(self.pending_acquire)
            .map(|point| format_sync_point("acquire", point));
        let release = self
            .last_release
            .or(self.pending_release)
            .map(|point| format_sync_point("release", point));
        match (acquire, release) {
            (Some(acquire), Some(release)) => Some(format!("{acquire}, {release}")),
            (Some(acquire), None) => Some(acquire),
            (None, Some(release)) => Some(release),
            (None, None) => None,
        }
    }
}

fn format_sync_point(label: &str, point: TrackedSyncPoint) -> String {
    match point.timeline_id {
        Some(timeline_id) => format!("{label}:timeline={timeline_id}:point={}", point.point),
        None => format!("{label}:none:point={}", point.point),
    }
}

fn fixed_from_i64(value: i64) -> i32 {
    value
        .saturating_mul(256)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn wayland_timestamp_ms_u32() -> u32 {
    // Match the compositor's monotonic input clock. Mixing Unix epoch time
    // with forwarded compositor events gives clients discontinuous timestamps.
    let mut timestamp = std::mem::MaybeUninit::<libc::timespec>::uninit();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, timestamp.as_mut_ptr()) } != 0 {
        return 0;
    }
    let timestamp = unsafe { timestamp.assume_init() };
    (timestamp.tv_sec as u64 * 1000 + timestamp.tv_nsec as u64 / 1_000_000) as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedWindow {
    window_id: String,
    /// Surface carrying the pixels selected for capture.
    wl_surface_id: u32,
    /// Role-bearing surface which must receive pointer focus/input.
    input_surface_id: u32,
    xdg_surface_id: Option<u32>,
    xdg_toplevel_id: Option<u32>,
    title: Option<String>,
    app_id: Option<String>,
    width: u32,
    height: u32,
    mapped: bool,
    commit_serial: u64,
    output_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DamageRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn create_proxy_runtime_dir() -> Result<TempDir, String> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("wayland-mcp-");
    let runtime_dir = builder
        .tempdir_in(env::temp_dir())
        .map_err(|err| format!("failed to create Wayland proxy runtime dir: {err}"))?;
    std::fs::set_permissions(runtime_dir.path(), std::fs::Permissions::from_mode(0o700)).map_err(
        |err| {
            format!(
                "failed to set Wayland proxy runtime dir permissions on {}: {err}",
                runtime_dir.path().display()
            )
        },
    )?;
    Ok(runtime_dir)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn run_proxy_accept_loop(
    listener: UnixListener,
    state: Arc<WaylandProxyState>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = listener.accept() => {
                let (stream, _addr) = match result {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        state.note_accept_error(format!(
                            "Wayland proxy accept loop stopped: {error}"
                        )).await;
                        break;
                    }
                };
                let state = Arc::clone(&state);
                tokio::task::spawn_blocking(move || {
                    let runtime = tokio::runtime::Handle::current();
                    let std_stream = match stream.into_std() {
                        Ok(stream) => stream,
                        Err(err) => {
                            runtime.block_on(state.note_accept_error(format!(
                                "accepted Wayland client could not be initialized: {err}"
                            )));
                            return;
                        }
                    };
                    if let Err(err) = handle_proxy_client_blocking(std_stream, Arc::clone(&state))
                        && !is_normal_client_disconnect(&err)
                    {
                        runtime.block_on(state.note_runtime_error(format!(
                            "Wayland proxy client exited: {err}"
                        )));
                    }
                });
            }
        }
    }
}

fn is_normal_client_disconnect(error: &str) -> bool {
    error.contains("Connection reset by peer")
        || error.contains("Broken pipe")
        || error.contains("Wayland stream reached EOF")
}

fn handle_proxy_client_blocking(
    stream: StdUnixStream,
    state: Arc<WaylandProxyState>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Handle::current();
    stream
        .set_nonblocking(false)
        .map_err(|err| format!("failed to make proxy client stream blocking: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|err| format!("failed to set proxy client read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(200)))
        .map_err(|err| format!("failed to set proxy client write timeout: {err}"))?;

    let client_id = runtime.block_on(state.register_live_client_session());
    runtime.block_on(state.track_object_interface(client_id, 1, "wl_display"))?;
    let backend_reader = runtime.block_on(state.backend_reader_stream(client_id))?;
    let raw_forward_only = runtime.block_on(state.raw_forward_only_flag(client_id))?;
    let relay_should_stop = Arc::new(AtomicBool::new(false));
    let client_writer = runtime.block_on(state.set_client_event_writer(
        client_id,
        stream.try_clone().map_err(|err| err.to_string())?,
    ))?;
    let mut backend_shutdown = None;
    let mut backend_relay = if let Some(backend_reader) = backend_reader {
        backend_shutdown =
            Some(backend_reader.try_clone().map_err(|err| {
                format!("failed to clone Wayland backend shutdown stream: {err}")
            })?);
        let relay_client_writer = client_writer.clone();
        backend_reader
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(|err| format!("failed to set backend reader timeout: {err}"))?;
        let relay_runtime = runtime.clone();
        let state = Arc::clone(&state);
        let relay_should_stop = Arc::clone(&relay_should_stop);
        let relay_raw_forward_only = Arc::clone(&raw_forward_only);
        Some(std::thread::spawn(move || {
            relay_backend_events_blocking(
                client_id,
                backend_reader,
                relay_client_writer,
                state,
                relay_runtime,
                relay_should_stop,
                relay_raw_forward_only,
            )
        }))
    } else {
        None
    };

    let mut close_detail = "client closed its Wayland connection".to_string();
    let result = 'client_loop: loop {
        match read_wayland_wire_message_blocking(&stream, 16) {
            Ok(message) => {
                let ingested = match runtime.block_on(state.ingest_request(client_id, message)) {
                    Ok(ingested) => ingested,
                    Err(err) => {
                        runtime.block_on(state.note_runtime_error(format!(
                            "client {} request ingest failed: {err}",
                            client_id.0
                        )));
                        break 'client_loop Err(err);
                    }
                };
                for event in ingested.backend_events {
                    if let Err(err) = client_writer.send_origin(
                        &event.encoded.bytes,
                        &event.encoded.fds,
                        Origin::Local,
                    ) {
                        break 'client_loop Err(err);
                    }
                }
                if let Err(error) = runtime.block_on(state.pulse_frame_callbacks(client_id)) {
                    break 'client_loop Err(error);
                }
            }
            Err(err) if is_timeout_error(&err) => {
                if let Err(error) = runtime.block_on(state.pulse_frame_callbacks(client_id)) {
                    break 'client_loop Err(error);
                }
                continue;
            }
            Err(err) if is_normal_client_disconnect(&err) => {
                close_detail = err;
                break Ok(());
            }
            Err(err) => {
                runtime.block_on(
                    state.note_runtime_error(format!("client {} read failed: {err}", client_id.0)),
                );
                break 'client_loop Err(err);
            }
        }
    };

    relay_should_stop.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(Shutdown::Both);
    if let Some(backend_shutdown) = backend_shutdown {
        let _ = backend_shutdown.shutdown(Shutdown::Both);
    }
    if let Some(backend_relay) = backend_relay.take()
        && backend_relay.join().is_err()
    {
        runtime.block_on(state.note_runtime_error(format!(
            "client {} backend relay thread panicked during shutdown",
            client_id.0
        )));
    }
    if let Err(err) = &result {
        close_detail = format!("session error: {err}");
    }
    runtime.block_on(state.finish_client_session(client_id, close_detail));
    result
}

fn relay_backend_events_blocking(
    client_id: WaylandClientId,
    backend_reader: StdUnixStream,
    client_writer: ClientWriter,
    state: Arc<WaylandProxyState>,
    runtime: tokio::runtime::Handle,
    should_stop: Arc<AtomicBool>,
    raw_forward_only: Arc<AtomicBool>,
) {
    loop {
        if should_stop.load(Ordering::Relaxed) {
            break;
        }
        let message = match read_wayland_wire_message(&backend_reader, 16) {
            Ok(message) => message,
            Err(err) if is_timeout_error(&err) && should_stop.load(Ordering::Relaxed) => break,
            Err(err) if is_timeout_error(&err) => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(err) if err.contains("backend Wayland stream reached EOF") => break,
            Err(err) => {
                runtime.block_on(state.note_runtime_error(format!(
                    "client {} backend read failed: {err}",
                    client_id.0
                )));
                break;
            }
        };
        let event = match runtime.block_on(state.prepare_backend_event(client_id, message)) {
            Ok(event) => event,
            Err(err) => {
                runtime.block_on(state.note_runtime_error(format!(
                    "client {} backend event tracking failed: {err}",
                    client_id.0
                )));
                break;
            }
        };
        if event.suppressed
            || event
                .decoded
                .as_ref()
                .is_some_and(is_suppressed_registry_global_event)
        {
            continue;
        }
        if let Err(err) =
            client_writer.send_origin(&event.encoded.bytes, &event.encoded.fds, Origin::Human)
        {
            if !is_normal_client_disconnect(&err) {
                runtime.block_on(state.note_runtime_error(format!(
                    "client {} backend event forward failed: {err}",
                    client_id.0
                )));
            }
            break;
        }
        if raw_forward_only.load(Ordering::Relaxed) {
            continue;
        }
    }
    // A stopped event relay cannot leave an apparently live client hanging.
    // Wake the request reader so normal session cleanup removes stale windows.
    client_writer.shutdown();
}

fn apply_object_tracking(
    session: &mut WaylandClientSession,
    registry: &WaylandProtocolRegistry,
    request: &DecodedWaylandRequest,
) -> Result<(), String> {
    match request.implemented_request.as_ref() {
        Some(GeneratedImplementedRequest::ZwpRelativePointerManagerV1GetRelativePointer {
            id,
            pointer,
        }) => {
            if let Some(seat) = pointer
                .as_ref()
                .and_then(|pointer| session.input_seats.get(pointer))
                .copied()
            {
                session.input_seats.insert(*id, seat);
            }
        }
        Some(GeneratedImplementedRequest::ZwpPointerConstraintsV1LockPointer {
            id,
            surface,
            pointer,
            lifetime,
            region,
            ..
        })
        | Some(GeneratedImplementedRequest::ZwpPointerConstraintsV1ConfinePointer {
            id,
            surface,
            pointer,
            lifetime,
            region,
            ..
        }) => {
            if ![1, 2].contains(lifetime) {
                return Err("invalid pointer constraint lifetime".into());
            }
            session.constraint_lifetimes.insert(*id, *lifetime);
            let value = region
                .map(|id| {
                    session
                        .frame_tracker
                        .regions
                        .get(&id)
                        .cloned()
                        .ok_or("unknown pointer constraint region")
                })
                .transpose()?;
            session.constraint_regions.insert(*id, value);
            if let Some(surface) = surface {
                session.input_surfaces.insert(*id, *surface);
            }
            if let Some(seat) = pointer
                .as_ref()
                .and_then(|pointer| session.input_seats.get(pointer))
                .copied()
            {
                session.input_seats.insert(*id, seat);
            }
        }
        Some(GeneratedImplementedRequest::ZwpLockedPointerV1SetRegion { region })
        | Some(GeneratedImplementedRequest::ZwpConfinedPointerV1SetRegion { region }) => {
            let value = region
                .map(|id| {
                    session
                        .frame_tracker
                        .regions
                        .get(&id)
                        .cloned()
                        .ok_or("unknown pointer constraint region")
                })
                .transpose()?;
            session
                .pending_constraint_regions
                .insert(request.object_id, value);
        }
        Some(GeneratedImplementedRequest::WlSurfaceCommit) => {
            if let Some(surface) = session
                .frame_tracker
                .pending_surfaces
                .get(&request.object_id)
                .or_else(|| session.frame_tracker.surfaces.get(&request.object_id))
                && surface.attach_pending
                && let Some(buffer) = surface.buffer_id
            {
                session.released_buffers.remove(&buffer);
            }

            let ids = session
                .input_surfaces
                .iter()
                .filter_map(|(id, s)| (*s == request.object_id).then_some(*id))
                .collect::<Vec<_>>();
            for id in ids {
                if let Some(region) = session.pending_constraint_regions.remove(&id) {
                    session.constraint_regions.insert(id, region);
                }
            }
        }
        _ => {}
    }
    if matches!(
        request.request_name.as_str(),
        "wl_seat.get_pointer" | "wl_seat.get_keyboard" | "wl_seat.get_touch"
    ) && let Some(DecodedWaylandArg::NewId(id)) = request.args.first()
    {
        session.input_seats.insert(*id, request.object_id);
    }
    if let Some(GeneratedTrackedRequest::WlRegistryBind {
        id_interface: Some(interface),
        id_version,
        id,
        name,
    }) = request.tracked_request.as_ref()
    {
        if interface == "wl_seat" {
            session.seat_globals.insert(*id, *name);
        }
        session.track_object_interface_version(*id, interface, *id_version);
        return Ok(());
    }

    if let Some((object_id, interface)) = request
        .implemented_request
        .as_ref()
        .and_then(implemented_request_created_interface)
    {
        let version = session
            .object_versions
            .get(&request.object_id)
            .copied()
            .unwrap_or(1);
        session.track_object_interface_version(object_id, interface, version);
        return Ok(());
    }

    let Some((_, generated_request)) = registry.request_by_id(request.request_id) else {
        return Ok(());
    };

    let version = session
        .object_versions
        .get(&request.object_id)
        .copied()
        .unwrap_or(1);
    apply_object_tracking_from_specs(session, generated_request.args, &request.args, version);
    if let Some(
        GeneratedImplementedRequest::ZwpPointerConstraintsV1LockPointer { id, lifetime, .. }
        | GeneratedImplementedRequest::ZwpPointerConstraintsV1ConfinePointer { id, lifetime, .. },
    ) = &request.implemented_request
    {
        session.constraint_lifetimes.insert(*id, *lifetime);
        if let Some(surface) = session.input_surfaces.get(id).copied()
            && session
                .delivered_focus
                .iter()
                .any(|((_, pointer, human), s)| *pointer && !*human && *s == surface)
        {
            session.update_model_constraints(surface, true)?;
        }
    }

    Ok(())
}

fn apply_object_tracking_from_specs(
    session: &mut WaylandClientSession,
    arg_specs: &[GeneratedArgSpec],
    args: &[DecodedWaylandArg],
    version: u32,
) {
    for (arg_spec, arg) in arg_specs.iter().zip(args) {
        if arg_spec.kind != GeneratedArgKind::NewId {
            continue;
        }
        let Some(interface) = arg_spec.interface else {
            continue;
        };
        let DecodedWaylandArg::NewId(object_id) = arg else {
            continue;
        };
        session.track_object_interface_version(*object_id, interface, version);
    }
}

fn register_core_protocols(registry: &mut WaylandProtocolRegistry) {
    let _ = registry;
}

fn register_frame_tracking_intercepts(registry: &mut WaylandProtocolRegistry) {
    const FRAME_TRACKING_INTERCEPTS: &[(&str, WaylandInterceptKind)] = &[
        ("wl_surface.attach", WaylandInterceptKind::SurfaceAttach),
        ("wl_surface.damage", WaylandInterceptKind::SurfaceDamage),
        (
            "wl_surface.damage_buffer",
            WaylandInterceptKind::SurfaceDamage,
        ),
        ("wl_surface.commit", WaylandInterceptKind::SurfaceCommit),
        (
            "wp_linux_drm_syncobj_manager_v1.import_timeline",
            WaylandInterceptKind::TimelineImport,
        ),
        (
            "wp_linux_drm_syncobj_surface_v1.set_acquire_point",
            WaylandInterceptKind::AcquirePoint,
        ),
        (
            "wp_linux_drm_syncobj_surface_v1.set_release_point",
            WaylandInterceptKind::ReleasePoint,
        ),
    ];

    for (request_name, kind) in FRAME_TRACKING_INTERCEPTS {
        let request_id = registry
            .hook_request_id(request_name)
            .unwrap_or_else(|| panic!("missing generated hook request {request_name}"));
        registry.register_intercept(request_id, WaylandInterceptHook { kind: *kind });
    }
}

fn decode_wayland_request(
    session: &WaylandClientSession,
    bytes: &[u8],
) -> Result<DecodedWaylandRequest, String> {
    let header = decode_wayland_header(bytes)?;
    let interface = session
        .interface_for_object(header.object_id)
        .ok_or_else(|| format!("unknown interface for object {}", header.object_id))?;
    let (_, request) =
        find_generated_request_by_opcode(interface, header.opcode).ok_or_else(|| {
            format!(
                "unknown request opcode {} for interface {}",
                header.opcode, interface
            )
        })?;
    let version = session
        .object_versions
        .get(&header.object_id)
        .copied()
        .unwrap_or(1);
    if request.since > version {
        return Err(format!(
            "{}.{} requires version {}, bound version is {version}",
            interface, request.name, request.since
        ));
    }
    if session.object_interfaces.len() >= 65_536
        && request
            .args
            .iter()
            .any(|s| s.kind == GeneratedArgKind::NewId)
    {
        return Err("client resource limit exceeded".into());
    }
    let args = decode_wayland_args(bytes, header.size, request.args)?;
    for (spec, arg) in request.args.iter().zip(&args) {
        match arg {
            DecodedWaylandArg::NewId(id)
                if *id == 0 || *id >= 0xff00_0000 || session.object_interfaces.contains_key(id) =>
            {
                return Err(format!("invalid or live new_id {id}"));
            }
            DecodedWaylandArg::Object(Some(id)) => {
                let actual = session
                    .object_interfaces
                    .get(id)
                    .ok_or_else(|| format!("unknown object argument {id}"))?;
                if spec.interface.is_some_and(|expected| expected != actual) {
                    return Err(format!("wrong interface for object argument {id}"));
                }
            }
            _ => {}
        }
    }
    let request_name = format!("{}.{}", interface, request.name);
    let request_id = find_generated_request_id_by_opcode(interface, header.opcode)
        .ok_or_else(|| format!("missing generated request id for {request_name}"))?;

    Ok(DecodedWaylandRequest {
        object_id: header.object_id,
        size: header.size,
        opcode: header.opcode,
        interface: interface.to_string(),
        request_name,
        request_id,
        implemented_request: {
            let generated_args = args
                .iter()
                .map(GeneratedDecodedArg::from_decoded)
                .collect::<Vec<_>>();
            decode_generated_implemented_request_by_id(request_id, &generated_args)
        },
        hook_request: {
            let generated_args = args
                .iter()
                .map(GeneratedDecodedArg::from_decoded)
                .collect::<Vec<_>>();
            decode_generated_hook_request_by_id(request_id, &generated_args)
        },
        tracked_request: {
            let generated_args = args
                .iter()
                .map(GeneratedDecodedArg::from_decoded)
                .collect::<Vec<_>>();
            decode_generated_tracked_request_by_id(request_id, &generated_args)
        },
        args,
    })
}

fn decode_wayland_event(
    object_interfaces: &HashMap<u32, String>,
    bytes: &[u8],
) -> Result<DecodedWaylandEvent, String> {
    let header = decode_wayland_header(bytes)?;
    let interface = object_interfaces
        .get(&header.object_id)
        .map(String::as_str)
        .ok_or_else(|| format!("unknown backend interface for object {}", header.object_id))?;
    let (_, event) = find_generated_event_by_opcode(interface, header.opcode).ok_or_else(|| {
        format!(
            "unknown event opcode {} for interface {}",
            header.opcode, interface
        )
    })?;
    let args = decode_wayland_args(bytes, header.size, event.args)?;
    let generated_args = args
        .iter()
        .map(GeneratedDecodedArg::from_decoded)
        .collect::<Vec<_>>();
    let generated_event =
        decode_generated_event_by_opcode(interface, header.opcode, &generated_args).ok_or_else(
            || {
                format!(
                    "generated event decoder could not decode {}.{}",
                    interface, event.name
                )
            },
        )?;
    Ok(DecodedWaylandEvent {
        object_id: header.object_id,
        size: header.size,
        opcode: header.opcode,
        interface: interface.to_string(),
        event_name: format!("{}.{}", interface, event.name),
        arg_specs: event.args,
        generated_event,
        args,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaylandMessageHeader {
    object_id: u32,
    size: u16,
    opcode: u16,
}

fn decode_wayland_header(bytes: &[u8]) -> Result<WaylandMessageHeader, String> {
    if bytes.len() < 8 {
        return Err("Wayland message shorter than header".to_string());
    }
    let object_id = u32::from_ne_bytes(
        bytes[0..4]
            .try_into()
            .map_err(|_| "invalid Wayland header object id".to_string())?,
    );
    let word_1 = u32::from_ne_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| "invalid Wayland header word 1".to_string())?,
    );
    let size = (word_1 >> 16) as u16;
    let opcode = (word_1 & 0xffff) as u16;
    if size < 8 || !size.is_multiple_of(4) {
        return Err(format!("invalid Wayland message size {size}"));
    }
    if size as usize > bytes.len() {
        return Err(format!(
            "Wayland message size {} exceeds provided bytes {}",
            size,
            bytes.len()
        ));
    }
    Ok(WaylandMessageHeader {
        object_id,
        size,
        opcode,
    })
}

fn decode_wayland_args(
    bytes: &[u8],
    size: u16,
    args_spec: &[crate::gui_wayland_generated::GeneratedArgSpec],
) -> Result<Vec<DecodedWaylandArg>, String> {
    let mut offset = 8usize;
    let mut args = Vec::with_capacity(args_spec.len());
    for arg in args_spec {
        args.push(match arg.kind {
            GeneratedArgKind::Int => {
                DecodedWaylandArg::Int(read_u32_arg(bytes, size, &mut offset)? as i32)
            }
            GeneratedArgKind::Uint => {
                DecodedWaylandArg::Uint(read_u32_arg(bytes, size, &mut offset)?)
            }
            GeneratedArgKind::Fixed => {
                DecodedWaylandArg::Fixed(read_u32_arg(bytes, size, &mut offset)? as i32)
            }
            GeneratedArgKind::Object => {
                let value = read_u32_arg(bytes, size, &mut offset)?;
                DecodedWaylandArg::Object((value != 0).then_some(value))
            }
            GeneratedArgKind::NewId => {
                DecodedWaylandArg::NewId(read_u32_arg(bytes, size, &mut offset)?)
            }
            GeneratedArgKind::String => {
                DecodedWaylandArg::String(read_string_arg(bytes, size, &mut offset)?)
            }
            GeneratedArgKind::Array => {
                DecodedWaylandArg::Array(read_array_arg(bytes, size, &mut offset)?)
            }
            GeneratedArgKind::Fd => DecodedWaylandArg::Fd,
        });
    }
    for (spec, value) in args_spec.iter().zip(&args) {
        if !spec.allow_null
            && matches!(
                value,
                DecodedWaylandArg::Object(None) | DecodedWaylandArg::String(None)
            )
        {
            return Err(format!("null argument {} is forbidden", spec.name));
        }
    }
    if offset != usize::from(size) {
        return Err("unexpected trailing Wayland argument bytes".into());
    }
    Ok(args)
}

fn read_wayland_message<R: Read>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut header = [0u8; 8];
    reader
        .read_exact(&mut header)
        .map_err(|err| format!("failed to read Wayland header: {err}"))?;

    let word_1 = u32::from_ne_bytes(
        header[4..8]
            .try_into()
            .map_err(|_| "invalid Wayland header word 1".to_string())?,
    );
    let size = (word_1 >> 16) as usize;
    if size < 8 || !size.is_multiple_of(4) {
        return Err(format!("invalid Wayland message size {size}"));
    }

    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&header);
    if size > 8 {
        let mut payload = vec![0u8; size - 8];
        reader
            .read_exact(&mut payload)
            .map_err(|err| format!("failed to read Wayland payload: {err}"))?;
        bytes.extend_from_slice(&payload);
    }
    Ok(bytes)
}

#[derive(Debug)]
struct WaylandWireMessage {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
}

// The proxy can inject pointer input even when the host seat has no physical
// pointer. Advertise that capability so new clients request wl_pointer.
fn rewrite_pointer_seat_capabilities(
    event: &DecodedWaylandEvent,
    message: &mut WaylandWireMessage,
) -> Result<(), String> {
    let GeneratedEvent::WlSeatCapabilities { capabilities } = &event.generated_event else {
        return Ok(());
    };
    if message.bytes.len() != 12 {
        return Err("wl_seat.capabilities event has an invalid wire length".to_string());
    }
    let advertised = *capabilities | 5; // WL_SEAT_CAPABILITY_POINTER | TOUCH
    message.bytes[8..12].copy_from_slice(&advertised.to_ne_bytes());
    Ok(())
}

fn send_wayland_wire_message(
    stream: &impl WireSink,
    bytes: &[u8],
    fds: &[OwnedFd],
) -> Result<(), String> {
    stream.send_wire(bytes, fds)
}

pub(crate) fn send_wayland_wire_message_to_fd(
    fd: libc::c_int,
    bytes: &[u8],
    fds: &[OwnedFd],
) -> Result<(), String> {
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iovlen = 1;

    let raw_fds = fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
    let mut control = if raw_fds.is_empty() {
        Vec::new()
    } else {
        vec![0u8; cmsg_space_for_fds(raw_fds.len())]
    };
    if !raw_fds.is_empty() {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = control.len();
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            if cmsg.is_null() {
                return Err("failed to prepare SCM_RIGHTS control message".to_string());
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = cmsg_len_for_fds(raw_fds.len());
            std::ptr::copy_nonoverlapping(
                raw_fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                raw_fds.len() * size_of::<libc::c_int>(),
            );
        }
    }

    // Socket backpressure is temporary, not a protocol failure. No bytes or
    // SCM_RIGHTS have been delivered on EAGAIN/EINTR, so retry the same message.
    // Bound the wait so an unresponsive client cannot stall this worker forever.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut offset = 0;
    while offset < bytes.len() {
        let mut iov = libc::iovec {
            iov_base: bytes[offset..].as_ptr().cast_mut().cast(),
            iov_len: bytes.len() - offset,
        };
        hdr.msg_iov = std::ptr::addr_of_mut!(iov);
        let sent = unsafe { libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
        if sent > 0 {
            offset += sent as usize;
            hdr.msg_control = std::ptr::null_mut();
            hdr.msg_controllen = 0;
            continue;
        }
        if sent == 0 {
            return Err("Wayland socket stopped accepting bytes".to_string());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock
            && error.kind() != std::io::ErrorKind::Interrupted
        {
            return Err(format!(
                "failed to send Wayland wire message with fds: {error}"
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out sending Wayland wire message after 5 seconds".to_string());
        }
        let mut ready = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut ready, 1, remaining.as_millis().max(1) as i32) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(format!(
                "failed to wait for Wayland socket: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn recv_wayland_wire_message(
    stream: &StdUnixStream,
    expected_size: usize,
    max_fds: usize,
) -> Result<WaylandWireMessage, String> {
    recv_wayland_wire_message_from_fd(stream.as_raw_fd(), expected_size, max_fds)
}

fn read_wayland_wire_message(
    stream: &StdUnixStream,
    max_fds: usize,
) -> Result<WaylandWireMessage, String> {
    read_wayland_wire_message_from_fd(stream.as_raw_fd(), max_fds, libc::MSG_DONTWAIT)
}

fn recv_wayland_wire_message_from_fd(
    fd: libc::c_int,
    expected_size: usize,
    max_fds: usize,
) -> Result<WaylandWireMessage, String> {
    recv_wayland_wire_message_from_fd_with_flags(fd, expected_size, max_fds, libc::MSG_DONTWAIT)
}

fn recv_wayland_wire_message_from_fd_with_flags(
    fd: libc::c_int,
    expected_size: usize,
    max_fds: usize,
    flags: libc::c_int,
) -> Result<WaylandWireMessage, String> {
    let mut bytes = vec![0u8; expected_size];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = if max_fds == 0 {
        Vec::new()
    } else {
        vec![0u8; cmsg_space_for_fds(max_fds)]
    };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::addr_of_mut!(iov);
    hdr.msg_iovlen = 1;
    if !control.is_empty() {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = control.len();
    }

    let received = unsafe { libc::recvmsg(fd, &mut hdr, flags | libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(format!(
            "failed to receive Wayland wire message with fds: {}",
            std::io::Error::last_os_error()
        ));
    }
    bytes.truncate(received as usize);

    let mut fds = Vec::new();
    unsafe {
        let mut current = libc::CMSG_FIRSTHDR(&hdr);
        while !current.is_null() {
            if (*current).cmsg_level == libc::SOL_SOCKET && (*current).cmsg_type == libc::SCM_RIGHTS
            {
                let data_len = (*current).cmsg_len.saturating_sub(cmsg_len_for_fds(0));
                let fd_count = data_len / size_of::<libc::c_int>();
                let data = libc::CMSG_DATA(current).cast::<libc::c_int>();
                for index in 0..fd_count {
                    let raw_fd = *data.add(index);
                    fds.push(OwnedFd::from_raw_fd(raw_fd));
                }
            }
            current = libc::CMSG_NXTHDR(&hdr, current);
        }
    }

    if hdr.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err("truncated Wayland ancillary descriptors".into());
    }
    Ok(WaylandWireMessage { bytes, fds })
}

fn recv_wayland_wire_message_from_fd_blocking(
    fd: libc::c_int,
    expected_size: usize,
    max_fds: usize,
) -> Result<WaylandWireMessage, String> {
    recv_wayland_wire_message_from_fd_with_flags(fd, expected_size, max_fds, 0)
}

fn read_wayland_wire_message_blocking(
    stream: &StdUnixStream,
    max_fds: usize,
) -> Result<WaylandWireMessage, String> {
    read_wayland_wire_message_from_fd(stream.as_raw_fd(), max_fds, 0)
}

fn read_wayland_wire_message_from_fd(
    fd: libc::c_int,
    max_fds: usize,
    flags: libc::c_int,
) -> Result<WaylandWireMessage, String> {
    let mut message = recv_wayland_wire_message_from_fd_with_flags(fd, 8, max_fds, flags)?;
    if message.bytes.is_empty() {
        return Err("backend Wayland stream reached EOF".into());
    }
    complete_wire_bytes(fd, &mut message, 8, max_fds)?;
    let word = u32::from_ne_bytes(message.bytes[4..8].try_into().unwrap());
    let size = (word >> 16) as usize;
    if size < 8 || !size.is_multiple_of(4) {
        return Err(format!("invalid Wayland message size {size}"));
    }
    complete_wire_bytes(fd, &mut message, size, max_fds)?;
    Ok(message)
}

fn complete_wire_bytes(
    fd: libc::c_int,
    message: &mut WaylandWireMessage,
    size: usize,
    max_fds: usize,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while message.bytes.len() < size {
        match recv_wayland_wire_message_from_fd_with_flags(
            fd,
            size - message.bytes.len(),
            max_fds,
            libc::MSG_DONTWAIT,
        ) {
            Ok(mut part) => {
                if part.bytes.is_empty() {
                    return Err("EOF inside Wayland message".into());
                }
                message.bytes.append(&mut part.bytes);
                message.fds.append(&mut part.fds);
                if message.fds.len() > max_fds {
                    return Err("Wayland message descriptor limit exceeded".into());
                }
            }
            Err(error) if is_timeout_error(&error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("incomplete Wayland message stalled for 5 seconds".into());
                }
                let mut ready = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result =
                    unsafe { libc::poll(&mut ready, 1, remaining.as_millis().max(1) as i32) };
                if result < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return Err("Wayland read poll failed".into());
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn write_wayland_bytes_async(stream: &UnixStream, bytes: &[u8]) -> Result<(), String> {
    stream
        .writable()
        .await
        .map_err(|err| format!("Wayland proxy stream not writable: {err}"))?;
    loop {
        match stream.try_write(bytes) {
            Ok(written) if written == bytes.len() => return Ok(()),
            Ok(written) => {
                return Err(format!(
                    "short write while relaying Wayland backend event: wrote {} of {} bytes",
                    written,
                    bytes.len()
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                stream
                    .writable()
                    .await
                    .map_err(|io_err| format!("Wayland proxy stream not writable: {io_err}"))?;
            }
            Err(err) => {
                return Err(format!(
                    "failed to relay Wayland backend event to client: {err}"
                ));
            }
        }
    }
}

async fn write_wayland_message_async(
    stream: &UnixStream,
    message: &WaylandWireMessage,
) -> Result<(), String> {
    if message.fds.is_empty() {
        return write_wayland_bytes_async(stream, &message.bytes).await;
    }

    stream
        .writable()
        .await
        .map_err(|err| format!("Wayland proxy stream not writable: {err}"))?;
    loop {
        match stream.try_io(tokio::io::Interest::WRITABLE, || {
            send_wayland_wire_message_to_fd(stream.as_raw_fd(), &message.bytes, &message.fds)
                .map_err(string_error_to_io)
        }) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                stream
                    .writable()
                    .await
                    .map_err(|io_err| format!("Wayland proxy stream not writable: {io_err}"))?;
            }
            Err(err) => {
                return Err(format!(
                    "failed to relay Wayland backend event to client: {err}"
                ));
            }
        }
    }
}

async fn read_wayland_wire_message_async(
    stream: &UnixStream,
    max_fds: usize,
) -> Result<Option<WaylandWireMessage>, String> {
    let mut message = WaylandWireMessage {
        bytes: Vec::new(),
        fds: Vec::new(),
    };
    while message.bytes.len() < 8 {
        stream.readable().await.map_err(|e| e.to_string())?;
        match stream.try_io(tokio::io::Interest::READABLE, || {
            recv_wayland_wire_message_from_fd(stream.as_raw_fd(), 8 - message.bytes.len(), max_fds)
                .map_err(string_error_to_io)
        }) {
            Ok(mut part) => {
                if part.bytes.is_empty() {
                    if message.bytes.is_empty() {
                        return Ok(None);
                    }
                    return Err("EOF inside Wayland header".into());
                }
                message.bytes.append(&mut part.bytes);
                message.fds.append(&mut part.fds);
                if message.fds.len() > max_fds {
                    return Err("Wayland descriptor limit exceeded".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.to_string()),
        }
    }

    let word_1 = u32::from_ne_bytes(
        message.bytes[4..8]
            .try_into()
            .map_err(|_| "invalid Wayland header word 1".to_string())?,
    );
    let size = (word_1 >> 16) as usize;
    if size < message.bytes.len() || !size.is_multiple_of(4) {
        return Err(format!("invalid Wayland message size {size}"));
    }
    while message.bytes.len() < size {
        stream
            .readable()
            .await
            .map_err(|err| format!("Wayland proxy stream not readable: {err}"))?;
        let remaining = size - message.bytes.len();
        match stream.try_io(tokio::io::Interest::READABLE, || {
            recv_wayland_wire_message_from_fd(stream.as_raw_fd(), remaining, max_fds)
                .map_err(string_error_to_io)
        }) {
            Ok(payload) if payload.bytes.is_empty() => {
                return Err("backend Wayland stream reached EOF".to_string());
            }
            Ok(mut payload) => {
                message.bytes.append(&mut payload.bytes);
                message.fds.append(&mut payload.fds);
                if message.fds.len() > max_fds {
                    return Err("Wayland descriptor limit exceeded".into());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(format!("failed to read Wayland payload: {err}")),
        }
    }
    Ok(Some(message))
}

fn is_timeout_error(err: &str) -> bool {
    err.contains("timed out")
        || err.contains("WouldBlock")
        || err.contains("Resource temporarily unavailable")
}

fn string_error_to_io(err: String) -> std::io::Error {
    if is_timeout_error(&err) {
        std::io::Error::from(std::io::ErrorKind::WouldBlock)
    } else {
        std::io::Error::other(err)
    }
}

fn trace_wayland_proxy(args: std::fmt::Arguments<'_>) {
    if env::var_os("WAYLAND_MCP_TRACE").is_some() {
        tracing::trace!(target: "wayland_mcp_proxy", "{args}");
    }
}

fn trace_undecoded_wayland_message(
    direction: &str,
    message: &WaylandWireMessage,
    decode_error: Option<&String>,
) {
    match decode_wayland_header(&message.bytes) {
        Ok(header) => trace_wayland_proxy(format_args!(
            "{direction} -> undecoded object={} opcode={} size={} bytes={} fds={} error={}",
            header.object_id,
            header.opcode,
            header.size,
            message.bytes.len(),
            message.fds.len(),
            decode_error.map(String::as_str).unwrap_or("unknown")
        )),
        Err(_) => trace_wayland_proxy(format_args!(
            "{direction} -> undecoded {} bytes fds={} error={}",
            message.bytes.len(),
            message.fds.len(),
            decode_error.map(String::as_str).unwrap_or("invalid header")
        )),
    }
}

fn read_u32_arg(bytes: &[u8], size: u16, offset: &mut usize) -> Result<u32, String> {
    if *offset + 4 > size as usize || *offset + 4 > bytes.len() {
        return Err("Wayland message ended while decoding u32 argument".to_string());
    }
    let value = u32::from_ne_bytes(
        bytes[*offset..*offset + 4]
            .try_into()
            .map_err(|_| "invalid Wayland u32 argument".to_string())?,
    );
    *offset += 4;
    Ok(value)
}

fn read_string_arg(bytes: &[u8], size: u16, offset: &mut usize) -> Result<Option<String>, String> {
    let len = read_u32_arg(bytes, size, offset)? as usize;
    if len == 0 {
        return Ok(None);
    }
    if *offset + len > size as usize || *offset + len > bytes.len() {
        return Err("Wayland message ended while decoding string argument".to_string());
    }
    let raw = &bytes[*offset..*offset + len];
    let content = if raw.last() == Some(&0) {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    let value = std::str::from_utf8(content)
        .map_err(|err| format!("invalid utf-8 string argument: {err}"))?
        .to_string();
    *offset += pad_to_4(len);
    Ok(Some(value))
}

fn read_array_arg(bytes: &[u8], size: u16, offset: &mut usize) -> Result<Vec<u8>, String> {
    let len = read_u32_arg(bytes, size, offset)? as usize;
    if *offset + len > size as usize || *offset + len > bytes.len() {
        return Err("Wayland message ended while decoding array argument".to_string());
    }
    let value = bytes[*offset..*offset + len].to_vec();
    *offset += pad_to_4(len);
    Ok(value)
}

fn pad_to_4(len: usize) -> usize {
    (len + 3) & !3
}

fn cmsg_space_for_fds(fd_count: usize) -> usize {
    unsafe { libc::CMSG_SPACE((fd_count * size_of::<libc::c_int>()) as u32) as usize }
}

fn cmsg_len_for_fds(fd_count: usize) -> usize {
    unsafe { libc::CMSG_LEN((fd_count * size_of::<libc::c_int>()) as u32) as usize }
}

fn encode_u32_message(object_id: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
    let size = 8 + args.len() * 4;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&object_id.to_ne_bytes());
    bytes.extend_from_slice(&(((size as u32) << 16) | opcode as u32).to_ne_bytes());
    for arg in args {
        bytes.extend_from_slice(&arg.to_ne_bytes());
    }
    bytes
}

fn backend_socket_path(config: &WaylandProxyConfig) -> Result<PathBuf, String> {
    if config.backend_socket.is_empty() {
        return Err("WAYLAND_DISPLAY is not set for Wayland backend connection".to_string());
    }
    let backend = PathBuf::from(&config.backend_socket);
    if backend.is_absolute() {
        return Ok(backend);
    }
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set for Wayland backend connection".to_string())?;
    Ok(runtime_dir.join(backend))
}

fn apply_backend_object_tracking(
    object_interfaces: &mut HashMap<u32, String>,
    generated: &WaylandGeneratedProtocolRegistry,
    request: &DecodedWaylandRequest,
) {
    if request.request_id == GeneratedRequestId::WlRegistryBind {
        let [
            DecodedWaylandArg::Uint(_name),
            DecodedWaylandArg::String(Some(interface)),
            DecodedWaylandArg::Uint(_version),
            DecodedWaylandArg::NewId(id),
        ] = request.args.as_slice()
        else {
            return;
        };
        object_interfaces.insert(*id, interface.clone());
        trace_wayland_proxy(format_args!(
            "backend object track {} -> {} via {} args={:?}",
            id, interface, request.request_name, request.args
        ));
        return;
    }

    if let Some((object_id, interface)) = request
        .implemented_request
        .as_ref()
        .and_then(implemented_request_created_interface)
    {
        object_interfaces.insert(object_id, interface.to_string());
        trace_wayland_proxy(format_args!(
            "backend object track {object_id} -> {interface} via {} args={:?}",
            request.request_name, request.args
        ));
        return;
    }

    let Some((_, generated_request)) = generated.request_by_id(request.request_id) else {
        return;
    };
    for (arg_spec, arg) in generated_request.args.iter().zip(&request.args) {
        if arg_spec.kind != GeneratedArgKind::NewId {
            continue;
        }
        let Some(interface) = arg_spec.interface else {
            continue;
        };
        let DecodedWaylandArg::NewId(object_id) = arg else {
            continue;
        };
        object_interfaces.insert(*object_id, interface.to_string());
        trace_wayland_proxy(format_args!(
            "backend object track {} -> {} via {} args={:?}",
            object_id, interface, request.request_name, request.args
        ));
    }
}

fn implemented_request_created_interface(
    request: &GeneratedImplementedRequest,
) -> Option<(u32, &'static str)> {
    match request {
        GeneratedImplementedRequest::WlDisplaySync { callback } => Some((*callback, "wl_callback")),
        GeneratedImplementedRequest::WlDisplayGetRegistry { registry } => {
            Some((*registry, "wl_registry"))
        }
        GeneratedImplementedRequest::WlCompositorCreateSurface { id } => Some((*id, "wl_surface")),
        GeneratedImplementedRequest::WlCompositorCreateRegion { id } => Some((*id, "wl_region")),
        GeneratedImplementedRequest::WlDataDeviceManagerCreateDataSource { id } => {
            Some((*id, "wl_data_source"))
        }
        GeneratedImplementedRequest::WlDataDeviceManagerGetDataDevice { id, .. } => {
            Some((*id, "wl_data_device"))
        }
        GeneratedImplementedRequest::WlShellGetShellSurface { id, .. } => {
            Some((*id, "wl_shell_surface"))
        }
        GeneratedImplementedRequest::WlSurfaceFrame { callback } => {
            Some((*callback, "wl_callback"))
        }
        GeneratedImplementedRequest::WlSurfaceGetRelease { callback } => {
            Some((*callback, "wp_linux_buffer_release_v1"))
        }
        GeneratedImplementedRequest::WlSeatGetPointer { id } => Some((*id, "wl_pointer")),
        GeneratedImplementedRequest::WlSeatGetKeyboard { id } => Some((*id, "wl_keyboard")),
        GeneratedImplementedRequest::WlSeatGetTouch { id } => Some((*id, "wl_touch")),
        GeneratedImplementedRequest::WlSubcompositorGetSubsurface { id, .. } => {
            Some((*id, "wl_subsurface"))
        }
        GeneratedImplementedRequest::XdgWmBaseCreatePositioner { id } => {
            Some((*id, "xdg_positioner"))
        }
        GeneratedImplementedRequest::XdgWmBaseGetXdgSurface { id, .. } => {
            Some((*id, "xdg_surface"))
        }
        GeneratedImplementedRequest::XdgSurfaceGetToplevel { id } => Some((*id, "xdg_toplevel")),
        GeneratedImplementedRequest::XdgSurfaceGetPopup { id, .. } => Some((*id, "xdg_popup")),
        GeneratedImplementedRequest::ZwpLinuxDmabufV1CreateParams { params_id } => {
            Some((*params_id, "zwp_linux_buffer_params_v1"))
        }
        GeneratedImplementedRequest::ZwpLinuxDmabufV1GetDefaultFeedback { id } => {
            Some((*id, "zwp_linux_dmabuf_feedback_v1"))
        }
        GeneratedImplementedRequest::ZwpLinuxDmabufV1GetSurfaceFeedback { id, .. } => {
            Some((*id, "zwp_linux_dmabuf_feedback_v1"))
        }
        GeneratedImplementedRequest::ZwpLinuxBufferParamsV1CreateImmed { buffer_id, .. } => {
            Some((*buffer_id, "wl_buffer"))
        }
        GeneratedImplementedRequest::WpLinuxDrmSyncobjManagerV1GetSurface { id, .. } => {
            Some((*id, "wp_linux_drm_syncobj_surface_v1"))
        }
        GeneratedImplementedRequest::WpLinuxDrmSyncobjManagerV1ImportTimeline { id, .. } => {
            Some((*id, "wp_linux_drm_syncobj_timeline_v1"))
        }
        GeneratedImplementedRequest::WpTearingControlManagerV1GetTearingControl { id, .. } => {
            Some((*id, "wp_tearing_control_v1"))
        }
        GeneratedImplementedRequest::WpPresentationFeedback { callback, .. } => {
            Some((*callback, "wp_presentation_feedback"))
        }
        GeneratedImplementedRequest::WpColorManagerV1GetOutput { id, .. } => {
            Some((*id, "wp_color_management_output_v1"))
        }
        GeneratedImplementedRequest::WpColorManagerV1GetSurface { id, .. } => {
            Some((*id, "wp_color_management_surface_v1"))
        }
        GeneratedImplementedRequest::WpColorManagerV1GetSurfaceFeedback { id, .. } => {
            Some((*id, "wp_color_management_surface_feedback_v1"))
        }
        GeneratedImplementedRequest::WpColorManagerV1CreateIccCreator { obj } => {
            Some((*obj, "wp_image_description_creator_icc_v1"))
        }
        GeneratedImplementedRequest::WpColorManagerV1CreateParametricCreator { obj } => {
            Some((*obj, "wp_image_description_creator_params_v1"))
        }
        GeneratedImplementedRequest::WpColorManagerV1CreateWindowsScrgb { image_description }
        | GeneratedImplementedRequest::WpColorManagerV1CreateWindowsBt2100 { image_description }
        | GeneratedImplementedRequest::WpColorManagerV1GetImageDescription {
            image_description,
            ..
        }
        | GeneratedImplementedRequest::WpColorManagementOutputV1GetImageDescription {
            image_description,
        }
        | GeneratedImplementedRequest::WpColorManagementSurfaceFeedbackV1GetPreferred {
            image_description,
        }
        | GeneratedImplementedRequest::WpColorManagementSurfaceFeedbackV1GetPreferredParametric {
            image_description,
        }
        | GeneratedImplementedRequest::WpImageDescriptionCreatorIccV1Create { image_description }
        | GeneratedImplementedRequest::WpImageDescriptionCreatorParamsV1Create {
            image_description,
        } => Some((*image_description, "wp_image_description_v1")),
        GeneratedImplementedRequest::WpImageDescriptionV1GetInformation { information } => {
            Some((*information, "wp_image_description_info_v1"))
        }
        GeneratedImplementedRequest::WpFifoManagerV1GetFifo { id, .. } => Some((*id, "wp_fifo_v1")),
        GeneratedImplementedRequest::WpFractionalScaleManagerV1GetFractionalScale {
            id, ..
        } => Some((*id, "wp_fractional_scale_v1")),
        GeneratedImplementedRequest::WpViewporterGetViewport { id, .. } => {
            Some((*id, "wp_viewport"))
        }
        _ => None,
    }
}

fn apply_object_interfaces_from_event(
    object_interfaces: &mut HashMap<u32, String>,
    event: &DecodedWaylandEvent,
) {
    for (arg_spec, arg) in event.arg_specs.iter().zip(&event.args) {
        if arg_spec.kind != GeneratedArgKind::NewId {
            continue;
        }
        let Some(interface) = arg_spec.interface else {
            continue;
        };
        let DecodedWaylandArg::NewId(object_id) = arg else {
            continue;
        };
        object_interfaces.insert(*object_id, interface.to_string());
    }
}

fn apply_dmabuf_tracking(
    session: &mut WaylandClientSession,
    request: &DecodedWaylandRequest,
    fds: &[OwnedFd],
) -> Result<(), String> {
    match request.tracked_request.as_ref() {
        Some(GeneratedTrackedRequest::ZwpLinuxDmabufV1CreateParams { params_id }) => {
            session.frame_tracker.note_dmabuf_params_created(*params_id);
        }
        Some(GeneratedTrackedRequest::ZwpLinuxBufferParamsV1Destroy) => {
            session
                .frame_tracker
                .destroy_dmabuf_params(request.object_id);
        }
        Some(GeneratedTrackedRequest::ZwpLinuxBufferParamsV1Add {
            fd: _,
            plane_idx,
            offset,
            stride,
            modifier_hi,
            modifier_lo,
        }) => {
            let Some(fd) = fds.first() else {
                return Err("zwp_linux_buffer_params_v1.add was missing its fd".to_string());
            };
            session.frame_tracker.note_dmabuf_plane(
                request.object_id,
                TrackedDmabufPlane {
                    fd: duplicate_fd(fd)?,
                    plane_idx: *plane_idx,
                    offset: *offset,
                    stride: *stride,
                    modifier: (u64::from(*modifier_hi) << 32) | u64::from(*modifier_lo),
                },
            )?;
        }
        Some(GeneratedTrackedRequest::ZwpLinuxBufferParamsV1Create {
            width,
            height,
            format,
            flags,
        }) => {
            session.frame_tracker.note_dmabuf_create(
                request.object_id,
                clamp_dimension(*width),
                clamp_dimension(*height),
                *format,
                *flags,
            )?;
        }
        Some(GeneratedTrackedRequest::ZwpLinuxBufferParamsV1CreateImmed {
            buffer_id,
            width,
            height,
            format,
            flags,
        }) => {
            session.frame_tracker.note_dmabuf_create_immed(
                request.object_id,
                *buffer_id,
                clamp_dimension(*width),
                clamp_dimension(*height),
                *format,
                *flags,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn apply_syncobj_tracking(
    session: &mut WaylandClientSession,
    request: &DecodedWaylandRequest,
    fds: &[OwnedFd],
) -> Result<(), String> {
    match request.implemented_request.as_ref() {
        Some(GeneratedImplementedRequest::WpLinuxDrmSyncobjManagerV1GetSurface { id, surface }) => {
            session
                .frame_tracker
                .note_syncobj_surface_created(*id, *surface);
        }
        Some(GeneratedImplementedRequest::WpLinuxDrmSyncobjManagerV1ImportTimeline {
            id,
            fd: _,
        }) => {
            let Some(fd) = fds.first() else {
                return Err(
                    "wp_linux_drm_syncobj_manager_v1.import_timeline was missing its fd"
                        .to_string(),
                );
            };
            session
                .frame_tracker
                .note_syncobj_timeline_imported(*id, duplicate_fd(fd)?);
        }
        Some(GeneratedImplementedRequest::WpLinuxDrmSyncobjTimelineV1Destroy) => {
            session
                .frame_tracker
                .note_syncobj_timeline_destroyed(request.object_id);
        }
        Some(GeneratedImplementedRequest::WpLinuxDrmSyncobjSurfaceV1Destroy) => {
            session
                .frame_tracker
                .note_syncobj_surface_destroyed(request.object_id);
        }
        Some(GeneratedImplementedRequest::WpLinuxDrmSyncobjSurfaceV1SetAcquirePoint {
            timeline,
            point_hi,
            point_lo,
        }) => {
            session.frame_tracker.note_syncobj_acquire_point(
                request.object_id,
                *timeline,
                sync_point_from_words(*point_hi, *point_lo),
            )?;
        }
        Some(GeneratedImplementedRequest::WpLinuxDrmSyncobjSurfaceV1SetReleasePoint {
            timeline,
            point_hi,
            point_lo,
        }) => {
            session.frame_tracker.note_syncobj_release_point(
                request.object_id,
                *timeline,
                sync_point_from_words(*point_hi, *point_lo),
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn apply_window_tracking(
    session: &mut WaylandClientSession,
    request: &DecodedWaylandRequest,
) -> Result<(), String> {
    match request.hook_request.as_ref() {
        Some(GeneratedHookRequest::XdgSurfaceGetPopup {
            id,
            parent: Some(parent),
            ..
        }) => {
            session
                .frame_tracker
                .note_popup_created(request.object_id, *id, *parent)?;
        }
        Some(GeneratedHookRequest::XdgPopupDestroy) => {
            if let Some(surface) = session
                .frame_tracker
                .popup_objects
                .remove(&request.object_id)
            {
                session.frame_tracker.dismissed_popups.insert(surface);
            }
        }
        Some(GeneratedHookRequest::XdgSurfaceSetWindowGeometry { x, y, .. }) => {
            if let Some(surface) = session
                .frame_tracker
                .xdg_surface_to_surface
                .get(&request.object_id)
                .copied()
            {
                session
                    .frame_tracker
                    .pending_surface_mut(surface)
                    .window_geometry_offset = (*x, *y);
            }
        }
        Some(GeneratedHookRequest::WlSubsurfaceSetSync) => session
            .frame_tracker
            .note_subsurface_sync(request.object_id, true),
        Some(GeneratedHookRequest::WlSubsurfaceSetDesync) => session
            .frame_tracker
            .note_subsurface_sync(request.object_id, false),
        Some(GeneratedHookRequest::WlSubsurfacePlaceAbove {
            sibling: Some(sibling),
        }) => session
            .frame_tracker
            .note_subsurface_stacking(request.object_id, *sibling, true)?,
        Some(GeneratedHookRequest::WlSubsurfacePlaceBelow {
            sibling: Some(sibling),
        }) => session
            .frame_tracker
            .note_subsurface_stacking(request.object_id, *sibling, false)?,
        Some(GeneratedHookRequest::WlSurfaceSetBufferTransform { transform }) => {
            if *transform < 0 || *transform > 7 {
                return Err("invalid buffer transform".into());
            }
            session
                .frame_tracker
                .pending_surface_mut(request.object_id)
                .buffer_transform = *transform as u32;
        }
        Some(GeneratedHookRequest::WlSurfaceOffset { x, y }) => {
            session
                .frame_tracker
                .pending_surface_mut(request.object_id)
                .offset = (*x, *y)
        }
        Some(GeneratedHookRequest::WpViewportSetSource {
            x,
            y,
            width,
            height,
        }) => {
            let surface = *session
                .frame_tracker
                .viewport_to_surface
                .get(&request.object_id)
                .ok_or("unknown viewport")?;
            let value = if [*x, *y, *width, *height] == [-256; 4] {
                None
            } else {
                if *x < 0 || *y < 0 || *width <= 0 || *height <= 0 {
                    return Err("invalid viewport source".into());
                }
                Some([*x, *y, *width, *height])
            };
            session
                .frame_tracker
                .pending_surface_mut(surface)
                .viewport_source = value;
        }
        Some(GeneratedHookRequest::WlCompositorCreateRegion { id }) => {
            session.frame_tracker.regions.insert(*id, vec![]);
        }
        Some(GeneratedHookRequest::WlRegionDestroy) => {
            session.frame_tracker.regions.remove(&request.object_id);
        }
        Some(GeneratedHookRequest::WlRegionAdd {
            x,
            y,
            width,
            height,
        }) => {
            if *width < 0 || *height < 0 {
                return Err("invalid region rectangle".into());
            }
            let region = session
                .frame_tracker
                .regions
                .get_mut(&request.object_id)
                .ok_or("unknown region")?;
            if region.len() >= 1024 {
                return Err("region rectangle limit exceeded".into());
            }
            region.push(DamageRect {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            });
        }
        Some(GeneratedHookRequest::WlRegionSubtract {
            x,
            y,
            width,
            height,
        }) => {
            let region = session
                .frame_tracker
                .regions
                .get_mut(&request.object_id)
                .ok_or("unknown region")?;
            let cut = DamageRect {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            };
            *region = region
                .iter()
                .flat_map(|rect| subtract_rectangle(*rect, cut))
                .collect();
            if region.len() > 1024 {
                return Err("region rectangle limit exceeded".into());
            }
        }
        Some(GeneratedHookRequest::WlSurfaceSetInputRegion { region }) => {
            let value = region
                .map(|id| {
                    session
                        .frame_tracker
                        .regions
                        .get(&id)
                        .cloned()
                        .ok_or("unknown input region")
                })
                .transpose()?;
            session
                .frame_tracker
                .pending_surface_mut(request.object_id)
                .input_region = value;
        }
        _ => {}
    }
    match request.tracked_request.as_ref() {
        Some(GeneratedTrackedRequest::WlSurfaceDestroy) => {
            session
                .frame_tracker
                .note_surface_destroyed(request.object_id);
            session.remove_object(request.object_id);
            if let Some(backend) = session.backend.as_mut() {
                backend.object_interfaces.remove(&request.object_id);
            }
        }
        Some(GeneratedTrackedRequest::WlSurfaceSetBufferScale { scale }) => {
            session
                .frame_tracker
                .note_surface_buffer_scale(request.object_id, *scale);
        }
        Some(GeneratedTrackedRequest::WlSubcompositorGetSubsurface {
            id,
            surface: Some(surface_id),
            parent: Some(parent_surface_id),
        }) => {
            session
                .frame_tracker
                .note_subsurface_created(*id, *surface_id, *parent_surface_id);
        }
        Some(GeneratedTrackedRequest::WlSubsurfaceDestroy) => {
            session
                .frame_tracker
                .note_subsurface_destroyed(request.object_id);
        }
        Some(GeneratedTrackedRequest::WlSubsurfaceSetPosition { x, y }) => {
            session
                .frame_tracker
                .note_subsurface_position(request.object_id, *x, *y);
        }
        Some(GeneratedTrackedRequest::XdgWmBaseGetXdgSurface {
            id,
            surface: Some(surface_id),
        }) => {
            session
                .frame_tracker
                .note_xdg_surface_created(*id, *surface_id);
        }
        Some(GeneratedTrackedRequest::XdgSurfaceGetToplevel { id }) => {
            session
                .frame_tracker
                .note_xdg_toplevel_created(request.object_id, *id)?;
        }
        Some(GeneratedTrackedRequest::XdgSurfaceSetWindowGeometry { width, height, .. }) => {
            session.frame_tracker.note_window_geometry(
                request.object_id,
                clamp_dimension(*width),
                clamp_dimension(*height),
            )?;
        }
        Some(GeneratedTrackedRequest::XdgToplevelSetTitle { title }) => {
            session
                .frame_tracker
                .note_xdg_toplevel_title(request.object_id, title.clone());
        }
        Some(GeneratedTrackedRequest::XdgToplevelSetAppId { app_id }) => {
            session
                .frame_tracker
                .note_xdg_toplevel_app_id(request.object_id, app_id.clone());
        }
        Some(GeneratedTrackedRequest::WpViewporterGetViewport {
            id,
            surface: Some(surface_id),
        }) => {
            session
                .frame_tracker
                .note_viewport_created(*id, *surface_id);
        }
        Some(GeneratedTrackedRequest::WpViewportDestroy) => {
            session
                .frame_tracker
                .note_viewport_destroyed(request.object_id);
        }
        Some(GeneratedTrackedRequest::WpViewportSetDestination { width, height }) => {
            session
                .frame_tracker
                .note_viewport_destination(request.object_id, *width, *height)?;
        }
        _ => {}
    }
    if matches!(
        request.implemented_request.as_ref(),
        Some(GeneratedImplementedRequest::XdgSurfaceAckConfigure { .. })
    ) {
        session
            .frame_tracker
            .note_xdg_surface_ack_configure(request.object_id);
    }
    Ok(())
}

fn apply_backend_event_tracking(
    session: &mut WaylandClientSession,
    event: &DecodedWaylandEvent,
) -> Result<(), String> {
    match &event.generated_event {
        GeneratedEvent::WlSurfaceEnter {
            output: Some(output_id),
        } => {
            session
                .frame_tracker
                .note_surface_enter(event.object_id, *output_id);
        }
        GeneratedEvent::WlSurfaceLeave {
            output: Some(output_id),
        } => {
            session
                .frame_tracker
                .note_surface_leave(event.object_id, *output_id);
        }
        GeneratedEvent::XdgPopupConfigure { x, y, .. } => {
            if let Some(surface) = session
                .frame_tracker
                .popup_objects
                .get(&event.object_id)
                .copied()
            {
                session
                    .frame_tracker
                    .pending_popup_positions
                    .insert(surface, (*x, *y));
            }
        }
        GeneratedEvent::XdgPopupPopupDone => {
            if let Some(surface) = session
                .frame_tracker
                .popup_objects
                .get(&event.object_id)
                .copied()
            {
                session.frame_tracker.dismissed_popups.insert(surface);
                session.frame_tracker.sync_window_from_surface(surface);
            }
        }
        GeneratedEvent::XdgSurfaceConfigure { .. } => {
            session
                .frame_tracker
                .note_xdg_surface_configure(event.object_id);
        }
        GeneratedEvent::ZwpLinuxBufferParamsV1Created { buffer } => {
            session
                .frame_tracker
                .note_dmabuf_created_from_event(event.object_id, *buffer)?;
        }
        GeneratedEvent::ZwpLinuxBufferParamsV1Failed => {
            session.frame_tracker.destroy_dmabuf_params(event.object_id);
        }
        GeneratedEvent::WlKeyboardModifiers {
            mods_depressed,
            mods_latched,
            mods_locked,
            group,
            ..
        } => {
            session.keyboard_mods_depressed = *mods_depressed;
            session.keyboard_mods_latched = *mods_latched;
            session.keyboard_mods_locked = *mods_locked;
            session.keyboard_layout_group = *group;
        }
        _ => {}
    }
    Ok(())
}

fn select_window_for_screenshot<'a>(
    windows: &'a [GuiWindowInfo],
    requested_window_id: Option<&str>,
) -> Result<Option<&'a GuiWindowInfo>, String> {
    if windows.is_empty() {
        return Ok(None);
    }
    if let Some(window_id) = requested_window_id {
        return windows
            .iter()
            .find(|window| window.window_id == window_id)
            .ok_or_else(|| {
                format!(
                    "unknown windowId `{window_id}`; call wayland.windows() to inspect available windows"
                )
            })
            .map(Some);
    }
    if windows.len() == 1 {
        return Ok(windows.first());
    }
    let available = windows
        .iter()
        .map(|window| {
            let name = window
                .title
                .as_deref()
                .or(window.app_id.as_deref())
                .unwrap_or("unnamed");
            format!(
                "{} ({name}, {}x{})",
                window.window_id, window.width, window.height
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "multiple Wayland windows are available; call wayland.windows() and provide windowId to wayland.screenshot(). Available windows: {available}"
    ))
}

fn clamp_dimension(value: i32) -> u32 {
    u32::try_from(value.max(1)).unwrap_or(1)
}

fn sync_point_from_words(point_hi: u32, point_lo: u32) -> u64 {
    (u64::from(point_hi) << 32) | u64::from(point_lo)
}

fn duplicate_fd(fd: &OwnedFd) -> Result<OwnedFd, String> {
    let duplicated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(format!(
            "failed to duplicate fd: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn duplicate_fds(fds: &[OwnedFd]) -> Result<Vec<OwnedFd>, String> {
    fds.iter().map(duplicate_fd).collect()
}

#[repr(C)]
struct DrmSyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
    point: u64,
}

#[repr(C)]
struct DrmSyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline_nsec: u64,
}

#[repr(C)]
struct DrmSyncobjDestroy {
    handle: u32,
    pad: u32,
}

const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE: u32 = 1 << 1;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
const DRM_IOCTL_SYNCOBJ_DESTROY: libc::c_ulong = drm_iowr::<DrmSyncobjDestroy>(0xC0);
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: libc::c_ulong = drm_iowr::<DrmSyncobjHandle>(0xC2);
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: libc::c_ulong = drm_iowr::<DrmSyncobjTimelineWait>(0xCA);

const fn drm_iowr<T>(nr: u8) -> libc::c_ulong {
    const IOC_WRITE: libc::c_ulong = 1;
    const IOC_READ: libc::c_ulong = 2;
    const IOC_NRBITS: libc::c_ulong = 8;
    const IOC_TYPEBITS: libc::c_ulong = 8;
    const IOC_SIZEBITS: libc::c_ulong = 14;
    const IOC_NRSHIFT: libc::c_ulong = 0;
    const IOC_TYPESHIFT: libc::c_ulong = IOC_NRSHIFT + IOC_NRBITS;
    const IOC_SIZESHIFT: libc::c_ulong = IOC_TYPESHIFT + IOC_TYPEBITS;
    const IOC_DIRSHIFT: libc::c_ulong = IOC_SIZESHIFT + IOC_SIZEBITS;
    const DRM_IOCTL_BASE: libc::c_ulong = b'd' as libc::c_ulong;

    ((IOC_READ | IOC_WRITE) << IOC_DIRSHIFT)
        | (DRM_IOCTL_BASE << IOC_TYPESHIFT)
        | ((nr as libc::c_ulong) << IOC_NRSHIFT)
        | ((size_of::<T>() as libc::c_ulong) << IOC_SIZESHIFT)
}

fn wait_drm_syncobj_timeline(timeline_fd: &OwnedFd, point: u64) -> Result<(), String> {
    let timeout_nsec = monotonic_timeout_nsec(Duration::from_secs(5))?;
    let mut last_error = "no DRM render node accepted syncobj timeline fd".to_string();
    for index in 128..=143 {
        let path = format!("/dev/dri/renderD{index}");
        let Ok(render_node) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        match wait_drm_syncobj_timeline_on_node(&render_node, timeline_fd, point, timeout_nsec) {
            Ok(()) => return Ok(()),
            Err(err) => last_error = format!("{path}: {err}"),
        }
    }
    Err(format!(
        "failed to wait for explicit Wayland acquire sync point {point}: {last_error}"
    ))
}

fn wait_drm_syncobj_timeline_on_node(
    render_node: &std::fs::File,
    timeline_fd: &OwnedFd,
    point: u64,
    timeout_nsec: i64,
) -> Result<(), String> {
    let imported_fd = duplicate_fd(timeline_fd)?;
    let mut handle = DrmSyncobjHandle {
        handle: 0,
        flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE,
        fd: imported_fd.as_raw_fd(),
        pad: 0,
        point: 0,
    };
    let import_result = unsafe {
        libc::ioctl(
            render_node.as_raw_fd(),
            DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
            &mut handle,
        )
    };
    drop(imported_fd);
    if import_result < 0 {
        return Err(format!(
            "DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let wait_result = wait_drm_syncobj_handle(render_node, handle.handle, point, timeout_nsec);
    let mut destroy = DrmSyncobjDestroy {
        handle: handle.handle,
        pad: 0,
    };
    let destroy_result = unsafe {
        libc::ioctl(
            render_node.as_raw_fd(),
            DRM_IOCTL_SYNCOBJ_DESTROY,
            &mut destroy,
        )
    };
    if destroy_result < 0 && wait_result.is_ok() {
        return Err(format!(
            "DRM_IOCTL_SYNCOBJ_DESTROY failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    wait_result
}

fn wait_drm_syncobj_handle(
    render_node: &std::fs::File,
    handle: u32,
    point: u64,
    timeout_nsec: i64,
) -> Result<(), String> {
    let mut handle_value = handle;
    let mut point_value = point;
    let mut wait = DrmSyncobjTimelineWait {
        handles: (&mut handle_value as *mut u32) as u64,
        points: (&mut point_value as *mut u64) as u64,
        timeout_nsec,
        count_handles: 1,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
        first_signaled: 0,
        pad: 0,
        deadline_nsec: 0,
    };
    let result = unsafe {
        libc::ioctl(
            render_node.as_raw_fd(),
            DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
            &mut wait,
        )
    };
    if result < 0 {
        return Err(format!(
            "DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn monotonic_timeout_nsec(timeout: Duration) -> Result<i64, String> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    if result < 0 {
        return Err(format!(
            "clock_gettime(CLOCK_MONOTONIC) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let now_nsec = i128::from(now.tv_sec) * 1_000_000_000 + i128::from(now.tv_nsec);
    let timeout_nsec = i128::try_from(timeout.as_nanos())
        .map_err(|_| "syncobj timeout is too large".to_string())?;
    i64::try_from(now_nsec + timeout_nsec).map_err(|_| "syncobj timeout overflowed i64".to_string())
}

fn convert_rgba8_colors(rgba: &mut [u8], color: Option<&crate::gui_color::ColorDescription>) {
    if let Some(color) = color {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            let input = [pixel[0], pixel[1], pixel[2], pixel[3]].map(|v| v as f32 / 255.0);
            pixel.copy_from_slice(&color.rgba8(input));
        }
    }
}

fn read_vulkan_dmabuf_rgba(
    buffer: &TrackedDmabufBuffer,
    color: Option<&crate::gui_color::ColorDescription>,
) -> Result<Vec<u8>, String> {
    let planes = buffer
        .planes
        .iter()
        .map(|plane| crate::gui_vulkan_dmabuf::DmabufPlane {
            fd: plane.fd.as_raw_fd(),
            offset: plane.offset,
            stride: plane.stride,
            modifier: plane.modifier,
        })
        .collect::<Vec<_>>();
    crate::gui_vulkan_dmabuf::read_dmabuf_rgba(crate::gui_vulkan_dmabuf::DmabufImage {
        width: buffer.width,
        height: buffer.height,
        format: buffer.format,
        planes: &planes,
        color,
    })
}

fn read_vulkan_dmabuf_linear(
    visual: &crate::visual_events::VisualHub,
    affinity: Option<(u32, u32)>,
    buffer: &TrackedDmabufBuffer,
    color: Option<&crate::gui_color::ColorDescription>,
) -> Result<Vec<[f32; 4]>, String> {
    let planes = buffer
        .planes
        .iter()
        .map(|plane| crate::visual_events::Plane {
            fd: plane.fd.as_raw_fd(),
            offset: plane.offset,
            stride: plane.stride,
            modifier: plane.modifier,
        })
        .collect::<Vec<_>>();
    let raw = visual.screenshot_raw(
        affinity.ok_or("screenshot DRM affinity unavailable")?,
        buffer.width,
        buffer.height,
        buffer.format,
        &planes,
    )?;
    crate::gui_vulkan_dmabuf::copied_dmabuf_pixels_to_linear(
        &raw,
        buffer.width,
        buffer.height,
        buffer.format,
        color,
    )
}

pub(crate) fn encode_rgba_png(
    width: u32,
    height: u32,
    rgba: &[u8],
    color: Option<&serde_json::Value>,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        if let Some(color) = color {
            encoder
                .add_text_chunk("wayland-mcp-color".to_string(), color.to_string())
                .map_err(|err| err.to_string())?;
        }
        let mut writer = encoder.write_header().map_err(|err| err.to_string())?;
        writer
            .write_image_data(rgba)
            .map_err(|err| err.to_string())?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_clock_fixture()
    -> Result<(WaylandProxyServer, WaylandClientId, StdUnixStream, String), String> {
        let client = WaylandClientId(1);
        let (host, upstream) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        host.set_read_timeout(Some(Duration::from_millis(10)))
            .map_err(|e| e.to_string())?;
        let backend = WaylandBackendSession {
            stream: upstream,
            globals: vec![],
            object_interfaces: HashMap::from([(1, "wl_display".into()), (10, "wl_surface".into())]),
            pending_backend_fds: VecDeque::new(),
        };
        let mut session = WaylandClientSession::new(client, vec![], Some(backend));
        session.track_object_interface_version(1, "wl_display", 1);
        session.track_object_interface_version(10, "wl_surface", 4);
        session.frame_tracker.surface_mut(10);
        let window = session.frame_tracker.ensure_window_for_surface(10);
        session
            .frame_tracker
            .windows
            .get_mut(&window)
            .unwrap()
            .mapped = true;
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test".into(),
            backend_socket: "test".into(),
        });
        register_frame_tracking_intercepts(&mut server.registry);
        server.sessions.insert(client, session);
        Ok((server, client, host, window))
    }

    fn frame_clock_request(
        server: &mut WaylandProxyServer,
        client: WaylandClientId,
        opcode: u16,
        args: &[u32],
    ) -> Result<(), String> {
        server.ingest_request(
            client,
            WaylandWireMessage {
                bytes: encode_u32_message(10, opcode, args),
                fds: vec![],
            },
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn visual_observer_ends_on_shm_or_detached_buffer() -> Result<(), String> {
        let Ok(minor) = std::env::var("WAYLAND_MCP_TEST_DRM_MINOR") else {
            return Ok(());
        };
        for detached in [false, true] {
            let (mut server, client, _host, window) = frame_clock_fixture()?;
            let tracker = &mut server.sessions.get_mut(&client).unwrap().frame_tracker;
            let file = tempfile::tempfile().map_err(|e| e.to_string())?;
            file.set_len(256).map_err(|e| e.to_string())?;
            tracker.note_dmabuf_params_created(20);
            tracker.note_dmabuf_plane(
                20,
                TrackedDmabufPlane {
                    fd: duplicate_file_fd(&file)?,
                    plane_idx: 0,
                    offset: 0,
                    stride: 32,
                    modifier: 0,
                },
            )?;
            tracker.note_dmabuf_create_immed(20, 21, 8, 8, 0x34324241, 0)?;
            tracker.set_surface_buffer(10, Some(21));
            tracker.commit_surface(10);
            tracker.note_shm_pool_created(40, duplicate_file_fd(&file)?, 256);
            tracker.note_shm_buffer_created(ShmBufferSpec {
                pool_id: 40,
                buffer_id: 41,
                offset: 0,
                width: 8,
                height: 8,
                stride: 32,
                format: 1,
            })?;
            tracker.set_surface_buffer(10, if detached { None } else { Some(41) });
            let state = WaylandProxyState {
                inner: Arc::new(Mutex::new(server)),
                input: Arc::new(crate::input_events::InputHub::default()),
                visual: Arc::new(crate::visual_events::VisualHub::default()),
            };
            let subscription = state.visual.subscribe(&serde_json::json!({"windowId":window,"rules":[{"id":"signal","rect":[0,0,8,8],"kind":"luminance"}]}), (226, minor.parse().map_err(|e| format!("{e}"))?))?;
            assert!(state.visual.interested(&window));
            state
                .ingest_request(
                    client,
                    WaylandWireMessage {
                        bytes: encode_u32_message(10, 6, &[]),
                        fds: vec![],
                    },
                )
                .await?;
            assert!(!state.visual.interested(&window));
            let end = subscription.end.borrow();
            let reason = end.as_ref().unwrap()["reason"].as_str().unwrap();
            assert!(
                reason.contains(if detached {
                    "detached"
                } else {
                    "CPU/SHM fallback is forbidden"
                }),
                "{reason}"
            );
            assert!(
                !state.inner.lock().await.sessions[&client]
                    .frame_tracker
                    .surfaces[&10]
                    .attach_pending
            );
        }
        Ok(())
    }

    #[test]
    fn observed_frame_clock_completes_committed_callbacks_without_host_events() -> Result<(), String>
    {
        let (mut server, client, host, window) = frame_clock_fixture()?;
        let lease = server.begin_observation(Some(window), Duration::from_secs(1))?;
        frame_clock_request(&mut server, client, 3, &[20])?;
        assert!(read_wayland_wire_message_from_fd(host.as_raw_fd(), 16, 0).is_err());
        assert!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .is_empty()
        );
        frame_clock_request(&mut server, client, 6, &[])?;
        assert_eq!(
            read_wayland_wire_message_from_fd(host.as_raw_fd(), 16, 0)?.bytes,
            encode_u32_message(10, 6, &[])
        );
        // Once owned locally, callbacks must finish even if the lease ends.
        server.observations.remove(&lease);
        let events =
            server.observed_frame_events(client, Instant::now() + Duration::from_secs(1))?;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].decoded.as_ref().unwrap().generated_event,
            GeneratedEvent::WlCallbackDone { .. }
        ));
        assert!(matches!(
            events[1].decoded.as_ref().unwrap().generated_event,
            GeneratedEvent::WlDisplayDeleteId { id: 20 }
        ));
        assert!(!server.sessions[&client].object_interfaces.contains_key(&20));
        assert!(
            !server.sessions[&client]
                .backend
                .as_ref()
                .unwrap()
                .object_interfaces
                .contains_key(&20)
        );
        assert!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .is_empty()
        );
        // A released local ID can safely be reused in a subsequent request.
        server.begin_observation(None, Duration::from_secs(1))?;
        frame_clock_request(&mut server, client, 3, &[20])?;
        frame_clock_request(&mut server, client, 6, &[])?;
        assert_eq!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn observed_frame_clock_recovers_forwarded_callback_and_suppresses_late_host_done()
    -> Result<(), String> {
        let (mut server, client, host, window) = frame_clock_fixture()?;
        frame_clock_request(&mut server, client, 3, &[20])?;
        frame_clock_request(&mut server, client, 6, &[])?;
        for opcode in [3, 6] {
            assert_eq!(
                read_wayland_wire_message_from_fd(host.as_raw_fd(), 16, 0)?.bytes,
                encode_u32_message(10, opcode, if opcode == 3 { &[20] } else { &[] })
            );
        }
        assert!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .is_empty()
        );
        server.begin_observation(Some(window), Duration::from_secs(1))?;
        let events =
            server.observed_frame_events(client, Instant::now() + Duration::from_secs(1))?;
        assert_eq!(events.len(), 1);
        assert!(server.sessions[&client].object_interfaces.contains_key(&20));
        let done = server.prepare_backend_event(
            client,
            WaylandWireMessage {
                bytes: encode_generated_event(
                    20,
                    &GeneratedEvent::WlCallbackDone { callback_data: 42 },
                )?,
                fds: vec![],
            },
        )?;
        assert!(done.suppressed);
        let deleted = server.prepare_backend_event(
            client,
            WaylandWireMessage {
                bytes: encode_generated_event(1, &GeneratedEvent::WlDisplayDeleteId { id: 20 })?,
                fds: vec![],
            },
        )?;
        assert!(!deleted.suppressed);
        assert!(!server.sessions[&client].synthetic_frame_done.contains(&20));
        assert!(!server.sessions[&client].object_interfaces.contains_key(&20));
        Ok(())
    }

    #[test]
    fn input_render_clock_is_scoped_to_the_explicit_window() -> Result<(), String> {
        let (mut server, client, host, window) = frame_clock_fixture()?;
        server.input_render_deadlines.insert(
            "another-window".into(),
            Instant::now() + Duration::from_secs(1),
        );
        assert!(!server.observing_surface(client, 10));
        server.mark_model_origin(&window)?;
        assert!(server.observing_surface(client, 10));
        frame_clock_request(&mut server, client, 3, &[20])?;
        frame_clock_request(&mut server, client, 6, &[])?;
        assert_eq!(
            read_wayland_wire_message_from_fd(host.as_raw_fd(), 16, 0)?.bytes,
            encode_u32_message(10, 6, &[])
        );
        assert_eq!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .len(),
            2
        );
        server
            .input_render_deadlines
            .insert(window, Instant::now() - Duration::from_secs(1));
        assert!(!server.observing_surface(client, 10));
        Ok(())
    }

    #[test]
    fn observed_frame_clock_preserves_request_order_with_reused_ids() -> Result<(), String> {
        let (mut server, client, _host, window) = frame_clock_fixture()?;
        server.begin_observation(Some(window), Duration::from_secs(1))?;
        frame_clock_request(&mut server, client, 3, &[20])?;
        frame_clock_request(&mut server, client, 3, &[19])?;
        frame_clock_request(&mut server, client, 6, &[])?;
        let events =
            server.observed_frame_events(client, Instant::now() + Duration::from_secs(1))?;
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].decoded.as_ref().unwrap().object_id, 20);
        assert_eq!(events[2].decoded.as_ref().unwrap().object_id, 19);
        Ok(())
    }

    #[test]
    fn observed_frame_clock_releases_local_callbacks_when_surface_is_destroyed()
    -> Result<(), String> {
        let (mut server, client, host, window) = frame_clock_fixture()?;
        server.begin_observation(Some(window), Duration::from_secs(1))?;
        frame_clock_request(&mut server, client, 3, &[20])?;
        let ingested = server.ingest_request(
            client,
            WaylandWireMessage {
                bytes: encode_u32_message(10, 0, &[]),
                fds: vec![],
            },
        )?;
        assert_eq!(
            read_wayland_wire_message_from_fd(host.as_raw_fd(), 16, 0)?.bytes,
            encode_u32_message(10, 0, &[])
        );
        assert_eq!(ingested.backend_events.len(), 1);
        assert!(matches!(
            ingested.backend_events[0]
                .decoded
                .as_ref()
                .unwrap()
                .generated_event,
            GeneratedEvent::WlDisplayDeleteId { id: 20 }
        ));
        assert!(!server.sessions[&client].object_interfaces.contains_key(&20));
        assert!(server.sessions[&client].frame_callbacks.is_empty());
        assert!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn visual_observation_drives_frames_without_enabling_cpu_pixel_capture() -> Result<(), String> {
        let (mut server, client, _host, window) = frame_clock_fixture()?;
        server.visual_windows.insert("another-window".into());
        assert!(!server.observing_surface(client, 10));
        server.visual_windows.insert(window.clone());
        assert!(server.observing_surface(client, 10));
        assert!(!server.capture_tracking_active());
        frame_clock_request(&mut server, client, 3, &[20])?;
        frame_clock_request(&mut server, client, 6, &[])?;
        assert_eq!(
            server
                .observed_frame_events(client, Instant::now() + Duration::from_secs(1))?
                .len(),
            2
        );
        server.visual_windows.remove(&window);
        assert!(!server.observing_surface(client, 10));
        Ok(())
    }

    #[test]
    fn targeted_input_enables_capture_tracking_until_its_render_lease_expires() -> Result<(), String>
    {
        let (mut server, _client, _host, window) = frame_clock_fixture()?;
        assert!(!server.capture_tracking_active());
        server.mark_model_origin(&window)?;
        assert!(server.capture_tracking_active());
        server
            .input_render_deadlines
            .insert(window, Instant::now() - Duration::from_secs(1));
        assert!(!server.capture_tracking_active());
        assert!(server.input_render_deadlines.is_empty());
        Ok(())
    }

    #[test]
    fn synthetic_input_timestamp_uses_the_compositor_monotonic_clock() {
        fn monotonic_ms() -> u32 {
            let mut timestamp = std::mem::MaybeUninit::<libc::timespec>::uninit();
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, timestamp.as_mut_ptr()) },
                0
            );
            let timestamp = unsafe { timestamp.assume_init() };
            (timestamp.tv_sec as u64 * 1000 + timestamp.tv_nsec as u64 / 1_000_000) as u32
        }
        let before = monotonic_ms();
        let actual = wayland_timestamp_ms_u32();
        let after = monotonic_ms();
        assert!(actual.wrapping_sub(before) <= after.wrapping_sub(before));
    }

    #[test]
    fn shader_bootstrap_screenshot_uses_buffer_pixels_without_viewport_mapping() {
        let mut tracker = WaylandFrameTracker::new("bootstrap".into());
        let surface = tracker.surface_mut(31);
        surface.width = 2;
        surface.height = 1;
        surface.has_committed_buffer = true;
        surface.rgba = vec![20, 30, 40, 255, 200, 210, 220, 255];
        surface.viewport_destination = Some((800, 420));
        surface.window_geometry_offset = (16, 10);
        surface.buffer_scale = 2;
        let window = tracker.ensure_window_for_surface(31);
        let frame = tracker.capture_buffer_rgba(&window).unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.rgba, [20, 30, 40, 255, 200, 210, 220, 255]);
        assert_eq!(frame.color.unwrap()["coordinate_space"], "buffer");
    }
    use pretty_assertions::assert_eq;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    #[test]
    fn broker_fd_duplicates_are_close_on_exec() -> Result<(), String> {
        let file = tempfile::tempfile().map_err(|e| e.to_string())?;
        let original: OwnedFd = file.into();
        let duplicated = duplicate_fd(&original)?;
        assert_ne!(
            unsafe { libc::fcntl(duplicated.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        Ok(())
    }

    #[test]
    fn local_and_host_server_ids_share_one_downstream_namespace() -> Result<(), String> {
        let client = WaylandClientId(1);
        let (host, upstream) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        let backend = WaylandBackendSession {
            stream: upstream,
            globals: vec![],
            object_interfaces: HashMap::from([(9, "zwp_linux_buffer_params_v1".into())]),
            pending_backend_fds: VecDeque::new(),
        };
        let mut session = WaylandClientSession::new(client, vec![], Some(backend));
        session.track_object_interface_version(9, "zwp_linux_buffer_params_v1", 3);
        session.frame_tracker.note_dmabuf_params_created(9);
        session
            .frame_tracker
            .note_dmabuf_create(9, 1, 1, u32::from_le_bytes(*b"AR24"), 0)?;
        let local = session.resource_map.allocate_server_id(None)?;
        session.track_object_interface_version(local, "wl_data_offer", 3);
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test".into(),
            backend_socket: "test".into(),
        });
        server.sessions.insert(client, session);
        let upstream_id = 0xff000000;
        let mut created = server.prepare_backend_event(
            client,
            WaylandWireMessage {
                bytes: encode_generated_event(
                    9,
                    &GeneratedEvent::ZwpLinuxBufferParamsV1Created {
                        buffer: upstream_id,
                    },
                )?,
                fds: vec![],
            },
        )?;
        let downstream = u32::from_ne_bytes(created.encoded.bytes[8..12].try_into().unwrap());
        assert_eq!(downstream, local + 1);
        assert_eq!(
            server.sessions[&client]
                .resource_map
                .upstream_id(downstream)?,
            upstream_id
        );
        // Nullable object arguments remain zero; actual objects are translated.
        rewrite_wire_object_ids(
            &mut created.encoded.bytes,
            created.decoded.as_ref().unwrap().arg_specs,
            false,
            |id, _| server.sessions[&client].resource_map.upstream_id(id),
        )?;
        assert_eq!(
            u32::from_ne_bytes(created.encoded.bytes[8..12].try_into().unwrap()),
            upstream_id
        );
        server.ingest_request(
            client,
            WaylandWireMessage {
                bytes: encode_u32_message(downstream, 0, &[]),
                fds: vec![],
            },
        )?;
        assert_eq!(
            read_wayland_wire_message_from_fd(host.as_raw_fd(), 16, 0)?.bytes,
            encode_u32_message(upstream_id, 0, &[])
        );
        assert!(
            !server.sessions[&client]
                .backend
                .as_ref()
                .unwrap()
                .object_interfaces
                .contains_key(&upstream_id)
        );
        assert!(
            server.sessions[&client]
                .resource_map
                .upstream_id(downstream)
                .is_err()
        );
        assert!(
            server.sessions[&client]
                .resource_map
                .upstream_id(local)
                .is_err()
        );
        assert_eq!(server.sessions[&client].resource_map.upstream_id(0)?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn delivered_input_has_trusted_origin_exact_coordinates_and_generations()
    -> Result<(), String> {
        let client = WaylandClientId(1);
        let mut session = WaylandClientSession::new(client, Vec::new(), None);
        session.track_object_interface(10, "wl_surface");
        session.track_object_interface_version(20, "wl_pointer", 9);
        session.track_object_interface_version(21, "wl_pointer", 9);
        session.input_seats.insert(20, 5);
        session.input_seats.insert(21, 5);
        session.frame_tracker.note_xdg_surface_created(30, 10);
        let window = session.frame_tracker.note_xdg_toplevel_created(30, 31)?;
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test".into(),
            backend_socket: "".into(),
        });
        server.sessions.insert(client, session);
        let state = Arc::new(WaylandProxyState {
            inner: Arc::new(Mutex::new(server)),
            input: Arc::new(crate::input_events::InputHub::default()),
            visual: Arc::new(crate::visual_events::VisualHub::default()),
        });
        let mut subscription = state
            .input
            .subscribe(&serde_json::json!({"windowId":window,"origin":"all"}))?;
        let (socket, reader) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        let writer = state.set_client_event_writer(client, socket).await?;
        for resource in [20, 21] {
            let bytes = encode_generated_event(
                resource,
                &GeneratedEvent::WlPointerEnter {
                    serial: 7,
                    surface: Some(10),
                    surface_x: 257,
                    surface_y: -1,
                },
            )?;
            writer.send_origin(&bytes, &[], Origin::Human)?;
        }
        let bytes = encode_generated_event(
            20,
            &GeneratedEvent::WlPointerMotion {
                time: 99,
                surface_x: 511,
                surface_y: -257,
            },
        )?;
        writer.send_origin(&bytes, &[], Origin::Human)?;
        let bytes = encode_generated_event(
            20,
            &GeneratedEvent::WlPointerMotion {
                time: 100,
                surface_x: 257,
                surface_y: -511,
            },
        )?;
        writer.send_scoped(&bytes, &[], Origin::Model, Some(10))?;
        let bytes = encode_generated_event(
            20,
            &GeneratedEvent::WlPointerEnter {
                serial: 8,
                surface: Some(10),
                surface_x: 3,
                surface_y: 4,
            },
        )?;
        writer.send_origin(&bytes, &[], Origin::Model)?;
        writer.wait(writer.sequence()).await?;
        let enter = subscription.events.recv().await.unwrap();
        let motion = subscription.events.recv().await.unwrap();
        let unfocused_model = subscription.events.recv().await.unwrap();
        assert_eq!(unfocused_model["event"]["type"], "motion");
        assert_eq!(unfocused_model["event"]["y"], -511);
        assert_eq!(unfocused_model["surfaceId"], enter["surfaceId"]);
        let model = subscription.events.recv().await.unwrap();
        assert_eq!(enter["origin"], "human");
        assert_eq!(enter["event"]["x"], 257);
        assert_eq!(motion["event"]["y"], -257);
        assert_eq!(model["origin"], "model");
        assert!(
            subscription.events.try_recv().is_err(),
            "duplicate resources must not duplicate samples"
        );
        assert_eq!(enter["surfaceId"], motion["surfaceId"]);
        let generation = state.inner.lock().await.sessions[&client].object_generations[&10];
        let mut locked = state.inner.lock().await;
        let session = locked.sessions.get_mut(&client).unwrap();
        session.remove_object(10);
        session.track_object_interface(10, "wl_surface");
        assert_ne!(session.object_generations[&10], generation);
        drop(locked);
        for _ in 0..5 {
            read_wayland_wire_message(&reader, 16)?;
        }
        state.input.stop(subscription.id, "test_done")?;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_writer_messages_and_descriptors_arrive_in_delivery_order()
    -> Result<(), String> {
        let (socket, reader) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        let writer = ClientWriter::new(socket);
        let delivered = Arc::new(StdMutex::new(Vec::new()));
        let actual = delivered.clone();
        writer.set_hook(Arc::new(move |bytes, _, _| {
            actual.lock().unwrap().push(bytes.to_vec())
        }));
        let receive = std::thread::spawn(move || {
            let mut packets = Vec::new();
            for _ in 0..24 {
                let packet = read_wayland_wire_message_from_fd(reader.as_raw_fd(), 16, 0)?;
                if packet.fds.len() != 1 {
                    return Err(format!(
                        "received {} descriptors instead of one",
                        packet.fds.len()
                    ));
                }
                packets.push(packet.bytes);
            }
            Ok::<_, String>(packets)
        });
        let mut producers = Vec::new();
        for producer in 0..3u32 {
            let writer = writer.clone();
            producers.push(std::thread::spawn(move || {
                let file = tempfile::tempfile().map_err(|e| e.to_string())?;
                for index in 0..8u32 {
                    let payload = vec![producer * 8 + index; 8000];
                    writer.send_origin(
                        &encode_u32_message(1, 0, &payload),
                        &[duplicate_file_fd(&file)?],
                        Origin::Local,
                    )?;
                }
                Ok::<_, String>(())
            }));
        }
        for producer in producers {
            producer.join().unwrap()?;
        }
        writer.wait(writer.sequence()).await?;
        assert_eq!(receive.join().unwrap()?, *delivered.lock().unwrap());
        Ok(())
    }

    #[test]
    fn clipboard_receive_decodes_mime_and_fd() -> Result<(), String> {
        // Both a human-triggered paste and a malicious eager read send precisely
        // the same MIME/FD request. Neither carries the input serial or actor.
        let interface = HashMap::from([(42, "wl_data_offer".into())]);
        let mut session = WaylandClientSession::new(WaylandClientId(1), Vec::new(), None);
        session.object_interfaces = interface;
        // Receive opcode 1: 11-byte NUL-terminated MIME, 12-byte padded
        // string. FD is carried out of band, not encoded as a serial.
        let mut mime = b"text/plain\0".to_vec();
        mime.resize(12, 0);
        let mut payload = vec![11];
        payload.extend(
            mime.as_chunks::<4>()
                .0
                .iter()
                .map(|b| u32::from_ne_bytes(*b)),
        );
        let human = encode_u32_message(42, 1, &payload);
        let request = decode_wayland_request(&session, &human)?;
        assert_eq!(
            request.args,
            vec![
                DecodedWaylandArg::String(Some("text/plain".into())),
                DecodedWaylandArg::Fd
            ]
        );
        Ok(())
    }
    #[test]
    fn keyboard_only_seat_still_advertises_synthetic_pointer() -> Result<(), String> {
        let event = DecodedWaylandEvent {
            object_id: 6,
            size: 12,
            opcode: 0,
            interface: "wl_seat".to_string(),
            event_name: "capabilities".to_string(),
            arg_specs: &[],
            generated_event: GeneratedEvent::WlSeatCapabilities { capabilities: 2 },
            args: vec![DecodedWaylandArg::Uint(2)],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&6u32.to_ne_bytes());
        bytes.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        bytes.extend_from_slice(&2u32.to_ne_bytes());
        let mut message = WaylandWireMessage {
            bytes,
            fds: Vec::new(),
        };
        rewrite_pointer_seat_capabilities(&event, &mut message)?;
        assert_eq!(
            u32::from_ne_bytes(message.bytes[8..12].try_into().unwrap()),
            7
        );
        Ok(())
    }

    #[test]
    fn polled_seat_event_advertises_synthetic_pointer_to_second_client() -> Result<(), String> {
        let (mut writer, reader) = StdUnixStream::pair().map_err(|err| err.to_string())?;
        reader
            .set_read_timeout(Some(Duration::from_millis(10)))
            .map_err(|err| err.to_string())?;
        let backend = WaylandBackendSession {
            stream: reader,
            globals: Vec::new(),
            object_interfaces: HashMap::from([(6, "wl_seat".to_string())]),
            pending_backend_fds: VecDeque::new(),
        };
        let client = WaylandClientId(2);
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test".to_string(),
            backend_socket: "test".to_string(),
        });
        server.sessions.insert(
            client,
            WaylandClientSession::new(client, Vec::new(), Some(backend)),
        );
        writer
            .write_all(&encode_u32_message(6, 0, &[2]))
            .map_err(|err| err.to_string())?;
        let events = server.poll_backend_events(client)?;
        assert_eq!(events.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(events[0].encoded.bytes[8..12].try_into().unwrap()),
            7
        );
        Ok(())
    }

    #[test]
    fn destroyed_surface_removes_pointer_target_window() -> Result<(), String> {
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());
        tracker.note_xdg_surface_created(30, 10);
        let window_id = tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.windows.get_mut(&window_id).unwrap().mapped = true;
        assert!(
            tracker
                .list_windows()
                .iter()
                .any(|window| window.window_id == window_id)
        );

        tracker.note_surface_destroyed(10);
        assert!(
            !tracker
                .list_windows()
                .iter()
                .any(|window| window.window_id == window_id)
        );
        assert!(!tracker.surface_to_window.contains_key(&10));
        assert!(!tracker.xdg_surface_to_surface.contains_key(&30));
        assert!(!tracker.xdg_toplevel_to_window.contains_key(&31));
        Ok(())
    }

    #[test]
    fn resize_sends_an_xdg_configure_and_tracks_its_local_ack() -> Result<(), String> {
        let (writer, mut reader) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|e| e.to_string())?;
        let mut session = WaylandClientSession::new(WaylandClientId(1), Vec::new(), None);
        session.client_event_writer = Some(ClientWriter::new(writer));
        session.frame_tracker.note_xdg_surface_created(30, 10);
        let window_id = session.frame_tracker.note_xdg_toplevel_created(30, 31)?;
        session
            .frame_tracker
            .windows
            .get_mut(&window_id)
            .unwrap()
            .mapped = true;

        session.resize_window(&window_id, 1000, 650)?;
        let serial = 0x8000_0001;
        assert!(session.synthetic_configures.contains(&(30, serial)));
        let expected = [
            encode_generated_event(
                31,
                &GeneratedEvent::XdgToplevelConfigure {
                    width: 1000,
                    height: 650,
                    states: Vec::new(),
                },
            )?,
            encode_generated_event(30, &GeneratedEvent::XdgSurfaceConfigure { serial })?,
        ]
        .concat();
        let mut actual = vec![0; expected.len()];
        reader.read_exact(&mut actual).map_err(|e| e.to_string())?;
        assert_eq!(actual, expected);
        Ok(())
    }

    fn duplicate_file_fd(file: &File) -> Result<OwnedFd, String> {
        let duplicated = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            return Err(format!(
                "failed to duplicate fixture fd: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }

    #[test]
    fn wire_send_retries_backpressure_and_delivers_fd_once() {
        let (sender, mut receiver) = StdUnixStream::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        let block = vec![0u8; 4096];
        let mut filled = 0;
        loop {
            match (&sender).write(&block) {
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected fill result: {other:?}"),
            }
        }
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            receiver.read_exact(&mut vec![0; filled]).unwrap();
            // Draining the socket wakes the writer but does not guarantee it
            // has sent yet. Wait for its payload rather than racing MSG_DONTWAIT.
            receiver
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            recv_wayland_wire_message_from_fd_blocking(receiver.as_raw_fd(), 8, 2).unwrap()
        });
        let file = tempfile::tempfile().unwrap();
        file.write_at(b"fd survived backpressure", 0).unwrap();
        send_wayland_wire_message(&sender, b"12345678", &[duplicate_file_fd(&file).unwrap()])
            .unwrap();
        let message = reader.join().unwrap();
        assert_eq!(message.bytes, b"12345678");
        assert_eq!(message.fds.len(), 1);
        let mut received_file = File::from(message.fds.into_iter().next().unwrap());
        let mut contents = String::new();
        received_file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "fd survived backpressure");
    }

    #[test]
    fn dmabuf_advertisement_preserves_feedback_version_for_filtered_tables() {
        let globals = filtered_backend_globals(&[WaylandGlobalInfo {
            name: 7,
            interface: "zwp_linux_dmabuf_v1".to_string(),
            version: 5,
        }]);
        assert_eq!(globals[0].version, 5);
    }

    #[test]
    fn ab4h_is_advertised_only_after_screenshot_support_is_enabled() {
        const DRM_FORMAT_ABGR16161616F: u32 = 0x4834_4241;
        for generated_event in [
            GeneratedEvent::ZwpLinuxDmabufV1Format {
                format: DRM_FORMAT_ABGR16161616F,
            },
            GeneratedEvent::ZwpLinuxDmabufV1Modifier {
                format: DRM_FORMAT_ABGR16161616F,
                modifier_hi: 0,
                modifier_lo: 0,
            },
        ] {
            let event = DecodedWaylandEvent {
                object_id: 7,
                size: 12,
                opcode: 0,
                interface: "zwp_linux_dmabuf_v1".to_string(),
                event_name: "fixture".to_string(),
                arg_specs: &[],
                generated_event,
                args: Vec::new(),
            };
            assert!(!is_unsupported_dmabuf_advertisement(&event));
        }
    }

    #[test]
    fn unsupported_dmabuf_format_is_filtered_and_not_capturable() -> Result<(), String> {
        const DRM_FORMAT_NV12: u32 = 0x3231_564e;
        let event = DecodedWaylandEvent {
            object_id: 7,
            size: 12,
            opcode: 0,
            interface: "zwp_linux_dmabuf_v1".to_string(),
            event_name: "format".to_string(),
            arg_specs: &[],
            generated_event: GeneratedEvent::ZwpLinuxDmabufV1Format {
                format: DRM_FORMAT_NV12,
            },
            args: Vec::new(),
        };
        assert!(is_unsupported_dmabuf_advertisement(&event));

        let dmabuf_file =
            tempfile::tempfile().map_err(|err| format!("failed to create dmabuf fd: {err}"))?;
        let buffer = TrackedBuffer {
            width: 8,
            height: 4,
            rgba: None,
            source: TrackedBufferSource::Dmabuf(TrackedDmabufBuffer {
                width: 8,
                height: 4,
                format: DRM_FORMAT_NV12,
                flags: 0,
                planes: vec![TrackedDmabufPlane {
                    fd: duplicate_file_fd(&dmabuf_file)?,
                    plane_idx: 0,
                    offset: 0,
                    stride: 8,
                    modifier: 0,
                }],
            }),
        };
        assert!(!buffer.has_readback());
        assert_eq!(
            buffer.capture_error().as_deref(),
            Some("DMA-BUF DRM format 0x3231564E (NV12) is unsupported for capture by this MCP")
        );
        Ok(())
    }

    #[test]
    fn backend_globals_without_generated_protocol_metadata_are_not_advertised() {
        let globals = filtered_backend_globals(&[
            WaylandGlobalInfo {
                name: 7,
                interface: "wl_compositor".to_string(),
                version: 6,
            },
            WaylandGlobalInfo {
                name: 8,
                interface: "ext_data_control_manager_v1".to_string(),
                version: 1,
            },
            WaylandGlobalInfo {
                name: 9,
                interface: "wl_shm".to_string(),
                version: 2,
            },
            WaylandGlobalInfo {
                name: 10,
                interface: "wl_subcompositor".to_string(),
                version: 1,
            },
            WaylandGlobalInfo {
                name: 11,
                interface: "wl_data_device_manager".to_string(),
                version: 3,
            },
            WaylandGlobalInfo {
                name: 12,
                interface: "wp_color_manager_v1".to_string(),
                version: 2,
            },
            WaylandGlobalInfo {
                name: 13,
                interface: "wp_color_representation_manager_v1".to_string(),
                version: 1,
            },
        ]);

        assert_eq!(globals.len(), 5);
        assert_eq!(globals[0].interface, "wl_compositor");
        assert_eq!(globals[1].interface, "wl_shm");
        assert_eq!(globals[2].interface, "wl_subcompositor");
        assert_eq!(globals[3].interface, "wl_data_device_manager");
        assert_eq!(globals[4].interface, "wp_color_manager_v1");
        assert_eq!(globals[4].version, 2);
    }

    #[test]
    fn repeated_registry_enumeration_does_not_duplicate_backend_globals() {
        let mut globals = vec![WaylandGlobalInfo {
            name: 4,
            interface: "wl_shm".to_string(),
            version: 1,
        }];
        upsert_backend_global(&mut globals, 4, "wl_shm", 2);

        assert_eq!(globals.len(), 1);
        assert_eq!(globals[0].version, 2);
    }

    #[test]
    fn dmabuf_feedback_table_and_tranche_indices_are_filtered_together() -> Result<(), String> {
        let mut source = tempfile::tempfile().map_err(|err| err.to_string())?;
        for (format, modifier) in [
            (0x3432_4241u32, 0x10u64),
            (0x4834_4241u32, 0u64),
            (0x3231_564eu32, 0u64),
        ] {
            source
                .write_all(&format.to_ne_bytes())
                .map_err(|err| err.to_string())?;
            source
                .write_all(&0u32.to_ne_bytes())
                .map_err(|err| err.to_string())?;
            source
                .write_all(&modifier.to_ne_bytes())
                .map_err(|err| err.to_string())?;
        }
        let mut session = WaylandClientSession::new(WaylandClientId(1), Vec::new(), None);
        let table_event = DecodedWaylandEvent {
            object_id: 50,
            size: 12,
            opcode: 1,
            interface: "zwp_linux_dmabuf_feedback_v1".to_string(),
            event_name: "format_table".to_string(),
            arg_specs: &[],
            generated_event: GeneratedEvent::ZwpLinuxDmabufFeedbackV1FormatTable {
                fd: true,
                size: 48,
            },
            args: vec![DecodedWaylandArg::Fd, DecodedWaylandArg::Uint(48)],
        };
        let mut table_message = WaylandWireMessage {
            bytes: encode_generated_event(50, &table_event.generated_event)?,
            fds: vec![duplicate_file_fd(&source)?],
        };
        rewrite_dmabuf_feedback_event(&mut session, &table_event, &mut table_message)?;
        assert_eq!(
            u32::from_ne_bytes(table_message.bytes[8..12].try_into().unwrap()),
            32
        );
        assert_eq!(
            session.dmabuf_feedback_index_maps[&50],
            vec![Some(0), Some(1), None]
        );

        let tranche_event = DecodedWaylandEvent {
            object_id: 50,
            size: 16,
            opcode: 5,
            interface: "zwp_linux_dmabuf_feedback_v1".to_string(),
            event_name: "tranche_formats".to_string(),
            arg_specs: &[],
            generated_event: GeneratedEvent::ZwpLinuxDmabufFeedbackV1TrancheFormats {
                indices: [0u16.to_ne_bytes(), 1u16.to_ne_bytes(), 2u16.to_ne_bytes()].concat(),
            },
            args: Vec::new(),
        };
        let mut tranche_message = WaylandWireMessage {
            bytes: encode_generated_event(50, &tranche_event.generated_event)?,
            fds: Vec::new(),
        };
        rewrite_dmabuf_feedback_event(&mut session, &tranche_event, &mut tranche_message)?;
        let mut offset = 8;
        assert_eq!(
            read_array_arg(&tranche_message.bytes, 16, &mut offset)?,
            [0u16.to_ne_bytes(), 1u16.to_ne_bytes()].concat()
        );
        Ok(())
    }

    fn screenshot_refresh_fixture() -> Result<(Arc<WaylandProxyState>, String), String> {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig::from_env());
        let client = WaylandClientId(1);
        let mut session = WaylandClientSession::new(client, Vec::new(), None);
        let tracker = &mut session.frame_tracker;
        tracker.note_xdg_surface_created(30, 10);
        let window = tracker.note_xdg_toplevel_created(30, 31)?;
        let surface = tracker.surface_mut(10);
        surface.has_committed_buffer = true;
        surface.rgba = vec![255, 0, 0, 255];
        tracker.commit_surface(10);
        server.sessions.insert(client, session);
        Ok((
            Arc::new(WaylandProxyState {
                inner: Arc::new(Mutex::new(server)),
                input: Arc::new(crate::input_events::InputHub::default()),
                visual: Arc::new(crate::visual_events::VisualHub::default()),
            }),
            window,
        ))
    }

    #[tokio::test]
    async fn explicit_subsurface_capture_waits_for_child_and_excludes_parent() -> Result<(), String>
    {
        let (state, window) = screenshot_refresh_fixture()?;
        {
            let mut inner = state.inner.lock().await;
            let tracker = &mut inner
                .sessions
                .get_mut(&WaylandClientId(1))
                .unwrap()
                .frame_tracker;
            tracker.note_subsurface_created(40, 11, 10);
            tracker.note_subsurface_sync(40, false);
            let child = tracker.surface_mut(11);
            child.has_committed_buffer = true;
            child.buffer_kind = Some("dmabuf");
            child.rgba = vec![0, 0, 255, 255];
            tracker.commit_surface(11);
        }
        let backend = WaylandGuiBackend {
            state: state.clone(),
            transport: Arc::new(StdMutex::new(None)),
            transport_error: Arc::new(StdMutex::new(None)),
        };
        assert!(
            backend
                .screenshot_surface(window.clone(), 99)
                .await
                .unwrap_err()
                .contains("live subsurface tree")
        );
        assert!(state.inner.lock().await.observations.is_empty());
        let producer = state.clone();
        let update = tokio::spawn(async move {
            loop {
                if !producer.inner.lock().await.observations.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let mut inner = producer.inner.lock().await;
            let tracker = &mut inner
                .sessions
                .get_mut(&WaylandClientId(1))
                .unwrap()
                .frame_tracker;
            tracker.surface_mut(11).rgba = vec![0, 255, 0, 255];
            tracker.commit_surface(11);
        });
        let png = backend.screenshot_surface(window.clone(), 11).await?;
        update.await.map_err(|e| e.to_string())?;
        let image = image::load_from_memory(&png)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.as_raw(), &[0, 255, 0, 255]);
        // The root remains selected for ordinary window APIs and input.
        let inner = state.inner.lock().await;
        assert_eq!(
            inner.sessions[&WaylandClientId(1)].frame_tracker.windows[&window].wl_surface_id,
            10
        );
        Ok(())
    }

    #[test]
    fn explicit_surface_capture_rejects_destroyed_and_other_window_children() -> Result<(), String>
    {
        let mut tracker = WaylandFrameTracker::new("test-client".into());
        tracker.note_xdg_surface_created(100, 10);
        let window = tracker.note_xdg_toplevel_created(100, 101)?;
        tracker.surface_mut(10).has_committed_buffer = true;
        tracker.commit_surface(10);
        tracker.note_xdg_surface_created(200, 20);
        let other = tracker.note_xdg_toplevel_created(200, 201)?;
        tracker.surface_mut(20).has_committed_buffer = true;
        tracker.commit_surface(20);
        tracker.note_subsurface_created(40, 11, 10);
        tracker.surface_mut(11).has_committed_buffer = true;
        tracker.commit_surface(11);
        tracker.commit_surface(10);
        assert!(tracker.surface_for_capture(&window, 11).is_ok());
        assert!(tracker.surface_for_capture(&other, 11).is_err());
        tracker.note_subsurface_destroyed(40);
        assert!(tracker.surface_for_capture(&window, 11).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn screenshot_refresh_waits_for_pending_pixels() -> Result<(), String> {
        let (state, window) = screenshot_refresh_fixture()?;
        let producer = state.clone();
        let update = tokio::spawn(async move {
            // Wait until the screenshot has requested observation, then publish
            // a commit whose pixels become readable a little later.
            loop {
                if !producer.inner.lock().await.observations.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            {
                let mut inner = producer.inner.lock().await;
                let tracker = &mut inner
                    .sessions
                    .get_mut(&WaylandClientId(1))
                    .unwrap()
                    .frame_tracker;
                tracker.surface_mut(10).rgba.clear();
                tracker.commit_surface(10);
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            producer
                .inner
                .lock()
                .await
                .sessions
                .get_mut(&WaylandClientId(1))
                .unwrap()
                .frame_tracker
                .surface_mut(10)
                .rgba = vec![0, 255, 0, 255];
        });
        for buffer_coordinates in [false, true] {
            let png = state
                .screenshot_png(
                    GuiScreenshotRequest {
                        window_id: Some(window.clone()),
                    },
                    buffer_coordinates,
                )
                .await?
                .ok_or("missing screenshot")?;
            let pixels = image::load_from_memory(&png)
                .map_err(|e| e.to_string())?
                .to_rgba8();
            assert_eq!(pixels.get_pixel(0, 0).0, [0, 255, 0, 255]);
        }
        update.await.map_err(|e| e.to_string())?;
        Ok(())
    }

    #[tokio::test]
    async fn screenshot_refresh_returns_idle_pixels_after_deadline() -> Result<(), String> {
        let (state, window) = screenshot_refresh_fixture()?;
        let started = tokio::time::Instant::now();
        let png = state
            .screenshot_png(
                GuiScreenshotRequest {
                    window_id: Some(window),
                },
                false,
            )
            .await?
            .ok_or("missing screenshot")?;
        assert!(started.elapsed() >= Duration::from_millis(250));
        let pixels = image::load_from_memory(&png)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        assert_eq!(pixels.get_pixel(0, 0).0, [255, 0, 0, 255]);
        Ok(())
    }

    #[test]
    fn retained_gpu_snapshot_survives_empty_commits_but_rejects_changed_pixels()
    -> Result<(), String> {
        let file = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut tracker = WaylandFrameTracker::new("retained-test".into());
        tracker.note_dmabuf_params_created(20);
        tracker.note_dmabuf_plane(
            20,
            TrackedDmabufPlane {
                fd: duplicate_file_fd(&file)?,
                plane_idx: 0,
                offset: 0,
                stride: 32,
                modifier: 0,
            },
        )?;
        tracker.note_dmabuf_create_immed(20, 21, 8, 4, 0x34324241, 0)?;
        tracker.note_xdg_surface_created(30, 10);
        let window = tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.set_surface_buffer(10, Some(21));
        tracker.commit_surface(10);
        let mut snapshot = crate::visual_events::SnapshotPixels {
            raw: [255, 0, 0, 255].repeat(32),
            width: 8,
            height: 4,
            format: 0x34324241,
            serial: 1,
        };
        // Explicit-sync buffers may be released already; no producer fd read is
        // needed to recover the retained copy and compose a window screenshot.
        tracker.commit_surface(10);
        let surface = tracker.surfaces.get_mut(&10).unwrap();
        assert_eq!(surface.commit_serial, 2);
        let pixels = surface.retained_snapshot_pixels(&snapshot)?;
        assert_eq!(pixels.len(), 32);
        surface.linear_rgba = Arc::new(pixels);
        surface.capture_error = None;
        assert!(tracker.capture_window_rgba(&window).unwrap().is_ok());

        tracker.add_damage(
            10,
            DamageRect {
                x: 0,
                y: 0,
                width: 8,
                height: 4,
            },
        );
        tracker.commit_surface(10);
        let surface = &tracker.surfaces[&10];
        assert!(
            surface
                .retained_snapshot_pixels(&snapshot)
                .unwrap_err()
                .contains("stale")
        );
        snapshot.serial = 3;
        assert!(surface.retained_snapshot_pixels(&snapshot).is_ok());
        snapshot.width = 4;
        assert!(
            surface
                .retained_snapshot_pixels(&snapshot)
                .unwrap_err()
                .contains("stale")
        );
        snapshot.width = 8;
        snapshot.format = 0x34325258;
        assert!(
            surface
                .retained_snapshot_pixels(&snapshot)
                .unwrap_err()
                .contains("stale")
        );
        Ok(())
    }

    #[test]
    fn format_support_does_not_certify_snapshot_availability() -> Result<(), String> {
        const DRM_FORMAT_ABGR16161616F: u32 = 0x4834_4241;
        let dmabuf_file =
            tempfile::tempfile().map_err(|err| format!("failed to create dmabuf fd: {err}"))?;
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());

        tracker.note_dmabuf_params_created(20);
        tracker.note_dmabuf_plane(
            20,
            TrackedDmabufPlane {
                fd: duplicate_file_fd(&dmabuf_file)?,
                plane_idx: 0,
                offset: 0,
                stride: 64,
                modifier: 0,
            },
        )?;
        tracker.note_dmabuf_create_immed(20, 21, 8, 4, DRM_FORMAT_ABGR16161616F, 0)?;
        tracker.note_xdg_surface_created(30, 10);
        let window_id = tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.set_surface_buffer(10, Some(21));
        tracker.commit_surface(10);

        let windows = tracker.list_windows();
        assert_eq!(windows.len(), 1);
        assert!(tracker.buffers[&21].has_readback());
        assert!(!windows[0].capturable);
        assert!(
            windows[0]
                .capture_error
                .as_deref()
                .unwrap()
                .contains("snapshot_unavailable")
        );
        assert_eq!(windows[0].window_id, window_id);
        Ok(())
    }

    #[test]
    fn dmabuf_window_tracks_syncobj_and_reports_vulkan_capture_path() -> Result<(), String> {
        let dmabuf_file =
            tempfile::tempfile().map_err(|err| format!("failed to create dmabuf fd: {err}"))?;
        let timeline_file =
            tempfile::tempfile().map_err(|err| format!("failed to create timeline fd: {err}"))?;
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());

        tracker.note_dmabuf_params_created(20);
        tracker.note_dmabuf_plane(
            20,
            TrackedDmabufPlane {
                fd: duplicate_file_fd(&dmabuf_file)?,
                plane_idx: 0,
                offset: 0,
                stride: 32,
                modifier: 0,
            },
        )?;
        tracker.note_dmabuf_create_immed(20, 21, 8, 4, 875_713_112, 0)?;
        tracker.note_xdg_surface_created(30, 10);
        tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.note_xdg_toplevel_title(31, Some("DMABUF fixture".to_string()));
        tracker.set_surface_buffer(10, Some(21));
        tracker.note_syncobj_surface_created(40, Some(10));
        tracker.note_syncobj_timeline_imported(41, duplicate_file_fd(&timeline_file)?);
        tracker.note_syncobj_acquire_point(40, Some(41), 7)?;
        tracker.note_syncobj_release_point(40, Some(41), 9)?;
        tracker.commit_surface(10);

        assert_eq!(
            tracker.list_windows(),
            vec![GuiWindowInfo {
                window_id: "test-client-window-1".to_string(),
                title: Some("DMABUF fixture".to_string()),
                app_id: None,
                width: 8,
                height: 4,
                mapped: true,
                commit_serial: 1,
                on_capture_output: false,
                capture_output_count: 0,
                on_backend_output: false,
                backend_output_count: 0,
                buffer_kind: Some("dmabuf".to_string()),
                subsurface_count: 0,
                subsurfaces: Vec::new(),
                sync_state: Some(
                    "acquire:timeline=41:point=7, release:timeline=41:point=9".to_string(),
                ),
                capturable: false,
                capture_error: Some(
                    "snapshot_unavailable: enable capture before the next GPU commit".into()
                ),
                render_surface_id: 10,
                input_surface_id: 10,
                capture_details: Some(
                    "Vulkan DMA-BUF format=0x34325258 planes=1 modifiers=[0x0000000000000000]"
                        .to_string(),
                ),
            }]
        );

        tracker.note_surface_enter(10, 55);
        let window = tracker.list_windows().remove(0);
        assert!(!window.on_capture_output);
        assert_eq!(window.capture_output_count, 0);
        assert!(window.on_backend_output);
        assert_eq!(window.backend_output_count, 1);
        Ok(())
    }

    #[test]
    fn click_target_maps_screenshot_pixels_to_viewport_destination() -> Result<(), String> {
        let dmabuf_file =
            tempfile::tempfile().map_err(|err| format!("failed to create dmabuf fd: {err}"))?;
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());

        tracker.note_dmabuf_params_created(20);
        tracker.note_dmabuf_plane(
            20,
            TrackedDmabufPlane {
                fd: duplicate_file_fd(&dmabuf_file)?,
                plane_idx: 0,
                offset: 0,
                stride: 6400,
                modifier: 0,
            },
        )?;
        tracker.note_dmabuf_create_immed(20, 21, 1600, 1200, 875_713_112, 0)?;
        tracker.note_xdg_surface_created(30, 10);
        let window_id = tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.note_viewport_created(40, 10);
        tracker.note_viewport_destination(40, 1280, 960)?;
        tracker.set_surface_buffer(10, Some(21));
        tracker.commit_surface(10);

        assert_eq!(
            tracker.click_target_for_window(&window_id, 520, 80)?,
            Some(PointerClickTarget {
                fixed_coords: None,
                window_id,
                surface_id: 10,
                screenshot_x: 520,
                screenshot_y: 80,
                surface_x: 416,
                surface_y: 64,
            })
        );
        Ok(())
    }

    #[test]
    fn click_target_maps_scaled_screenshot_to_logical_surface() -> Result<(), String> {
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());
        tracker.note_xdg_surface_created(30, 10);
        let window_id = tracker.note_xdg_toplevel_created(30, 31)?;
        let surface = tracker.surface_mut(10);
        surface.width = 2304;
        surface.height = 1440;

        surface.has_committed_buffer = true;
        // Fractional scaling uses a viewport; window geometry only describes
        // content bounds and must not rescale the entire surface.
        tracker.note_viewport_created(40, 10);
        tracker.note_viewport_destination(40, 1440, 900)?;
        tracker.commit_surface(10);
        let target = tracker
            .click_target_for_window(&window_id, 985, 1042)?
            .unwrap();
        assert_eq!((target.surface_x, target.surface_y), (615, 651));

        // Without geometry or a viewport, wl_surface buffer scale still applies.
        tracker.note_viewport_destination(40, -1, -1)?;
        tracker.note_surface_buffer_scale(10, 2);
        tracker.commit_surface(10);
        let target = tracker
            .click_target_for_window(&window_id, 1000, 800)?
            .unwrap();
        assert_eq!((target.surface_x, target.surface_y), (500, 400));
        Ok(())
    }

    #[test]
    fn click_rejects_coordinates_outside_the_returned_screenshot() -> Result<(), String> {
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());
        let window_id = tracker.ensure_window_for_surface(10);
        let surface = tracker.surface_mut(10);
        surface.width = 100;
        surface.height = 50;
        surface.rgba = vec![0; 100 * 50 * 4];
        surface.has_committed_buffer = true;
        tracker.sync_window_from_surface(10);

        let error = tracker
            .click_target_for_window(&window_id, 100, 49)
            .expect_err("x == width must be rejected rather than clamped");
        assert!(error.contains("outside screenshot bounds 100x50"));
        Ok(())
    }

    #[test]
    fn shm_argb8888_buffer_is_captured_as_rgba() -> Result<(), String> {
        let mut file =
            tempfile::tempfile().map_err(|err| format!("failed to create shm fixture: {err}"))?;
        // Two native-endian ARGB8888 pixels: opaque red, half-alpha green.
        file.write_all(&[0, 0, 255, 255, 0, 255, 0, 128])
            .map_err(|err| format!("failed to write shm fixture: {err}"))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|err| format!("failed to rewind shm fixture: {err}"))?;
        let buffer = TrackedShmBuffer {
            fd: duplicate_file_fd(&file)?,
            offset: 0,
            width: 2,
            height: 1,
            stride: 8,
            format: TrackedShmBuffer::ARGB8888,
        };

        assert_eq!(
            buffer.read_rgba()?.rgba,
            vec![255, 0, 0, 255, 0, 255, 0, 128]
        );
        Ok(())
    }

    #[test]
    fn fd_attached_to_unknown_message_is_queued_for_following_request() -> Result<(), String> {
        let file =
            tempfile::tempfile().map_err(|err| format!("failed to create fd fixture: {err}"))?;
        let mut session = WaylandClientSession::new(WaylandClientId(1), Vec::new(), None);
        let mut message = WaylandWireMessage {
            bytes: encode_u32_message(999, 0, &[]),
            fds: vec![duplicate_file_fd(&file)?],
        };

        apply_undecoded_client_tracking(&mut session, &mut message)?;

        assert!(message.fds.is_empty());
        assert_eq!(session.pending_client_fds.len(), 1);
        Ok(())
    }

    #[test]
    fn fd_on_shm_pool_resize_is_reserved_for_following_create_pool() -> Result<(), String> {
        let file = tempfile::tempfile().map_err(|err| err.to_string())?;
        let mut session = WaylandClientSession::new(WaylandClientId(1), Vec::new(), None);
        session.track_object_interface(7, "wl_shm");
        session.track_object_interface(31, "wl_shm_pool");
        session
            .frame_tracker
            .note_shm_pool_created(31, duplicate_file_fd(&file)?, 1024);
        let mut resize = WaylandWireMessage {
            bytes: encode_u32_message(31, 2, &[2048]),
            fds: vec![duplicate_file_fd(&file)?],
        };
        apply_undecoded_client_tracking(&mut session, &mut resize)?;
        assert!(resize.fds.is_empty());
        assert_eq!(session.pending_client_fds.len(), 1);

        let mut create = WaylandWireMessage {
            bytes: encode_u32_message(7, 0, &[32, 2048]),
            fds: Vec::new(),
        };
        apply_undecoded_client_tracking(&mut session, &mut create)?;
        assert_eq!(create.fds.len(), 1);
        assert!(session.pending_client_fds.is_empty());
        assert!(session.frame_tracker.shm_pools.contains_key(&32));
        Ok(())
    }

    #[tokio::test]
    async fn async_reader_preserves_fd_attached_to_message_payload() -> Result<(), String> {
        let (mut writer, reader) = StdUnixStream::pair().map_err(|err| err.to_string())?;
        reader
            .set_nonblocking(true)
            .map_err(|err| err.to_string())?;
        let reader = UnixStream::from_std(reader).map_err(|err| err.to_string())?;
        let bytes = encode_u32_message(5, 0, &[42]);
        let fd_file = tempfile::tempfile().map_err(|err| err.to_string())?;
        writer
            .write_all(&bytes[..8])
            .map_err(|err| err.to_string())?;
        send_wayland_wire_message(&writer, &bytes[8..], &[duplicate_file_fd(&fd_file)?])?;

        let message = read_wayland_wire_message_async(&reader, 4)
            .await?
            .ok_or_else(|| "message disappeared".to_string())?;
        assert_eq!(message.bytes, bytes);
        assert_eq!(message.fds.len(), 1);
        Ok(())
    }

    #[test]
    fn fd_batched_with_manual_wl_shm_event_is_reserved_for_following_backend_event()
    -> Result<(), String> {
        let file =
            tempfile::tempfile().map_err(|err| format!("failed to create fd fixture: {err}"))?;
        let (stream, _peer) = StdUnixStream::pair().map_err(|err| err.to_string())?;
        let mut backend = WaylandBackendSession {
            stream,
            globals: Vec::new(),
            object_interfaces: HashMap::from([(22, "wl_shm".to_string())]),
            pending_backend_fds: VecDeque::new(),
        };
        let bytes = encode_u32_message(22, 0, &[0]);
        let mut fds = vec![duplicate_file_fd(&file)?];

        reassociate_undecoded_backend_fds(&mut backend, &bytes, &mut fds)?;
        assert!(fds.is_empty());
        assert_eq!(backend.pending_backend_fds.len(), 1);

        let mut following_fds = Vec::new();
        reassociate_fds(
            "backend-test",
            &mut backend.pending_backend_fds,
            &[DecodedWaylandArg::Fd],
            &mut following_fds,
        );
        assert_eq!(following_fds.len(), 1);
        assert!(backend.pending_backend_fds.is_empty());
        Ok(())
    }

    #[test]
    fn window_subsurfaces_report_nested_unbuffered_and_destroyed_roles() -> Result<(), String> {
        let mut tracker = WaylandFrameTracker::new("test-client".into());
        tracker.note_xdg_surface_created(100, 10);
        let window = tracker.note_xdg_toplevel_created(100, 101)?;
        tracker.note_xdg_surface_created(200, 20);
        let other = tracker.note_xdg_toplevel_created(200, 201)?;
        assert_eq!(tracker.list_windows()[0].subsurface_count, 0);
        // A nested role is counted even before either child has a buffer.
        tracker.note_subsurface_created(110, 11, 10);
        tracker.note_subsurface_created(120, 12, 11);
        tracker.note_subsurface_created(210, 21, 20);
        tracker.note_subsurface_position(110, 7, 9);
        tracker.commit_surface(10);
        let children = tracker.subsurfaces_for_window(&window);
        assert_eq!(children.len(), 2);
        assert_eq!(
            (
                children[0].subsurface_id,
                children[0].surface_id,
                children[0].parent_surface_id
            ),
            (110, 11, 10)
        );
        assert_eq!(children[0].position, (7, 9));
        assert!(children[0].synchronized);
        assert_eq!(children[1].parent_surface_id, 11);
        assert!(!children[1].has_committed_buffer);
        assert_eq!(children[1].buffer_kind, None);
        assert_eq!(tracker.subsurfaces_for_window(&other).len(), 1);
        let info = tracker
            .list_windows()
            .into_iter()
            .find(|w| w.window_id == window)
            .unwrap();
        assert_eq!(info.subsurface_count, 2);
        assert_eq!(info.subsurfaces, children);
        tracker.note_subsurface_destroyed(120);
        assert_eq!(tracker.subsurfaces_for_window(&window).len(), 1);
        tracker.note_surface_destroyed(11);
        assert!(tracker.subsurfaces_for_window(&window).is_empty());
        assert_eq!(tracker.subsurfaces_for_window(&other).len(), 1);
        Ok(())
    }

    #[test]
    fn window_subsurfaces_distinguish_shm_parent_and_committed_dmabuf_child() -> Result<(), String>
    {
        let mut tracker = WaylandFrameTracker::new("test-client".into());
        tracker.note_xdg_surface_created(100, 10);
        let window = tracker.note_xdg_toplevel_created(100, 101)?;
        let shm = tempfile::tempfile().map_err(|e| e.to_string())?;
        shm.set_len(128).map_err(|e| e.to_string())?;
        tracker.note_shm_pool_created(30, duplicate_file_fd(&shm)?, 128);
        tracker.note_shm_buffer_created(ShmBufferSpec {
            pool_id: 30,
            buffer_id: 31,
            offset: 0,
            width: 8,
            height: 4,
            stride: 32,
            format: 1,
        })?;
        tracker.set_surface_buffer(10, Some(31));
        tracker.commit_surface(10);
        tracker.note_subsurface_created(110, 11, 10);
        let dma = tempfile::tempfile().map_err(|e| e.to_string())?;
        tracker.note_dmabuf_params_created(40);
        tracker.note_dmabuf_plane(
            40,
            TrackedDmabufPlane {
                fd: duplicate_file_fd(&dma)?,
                plane_idx: 0,
                offset: 0,
                stride: 32,
                modifier: 0,
            },
        )?;
        tracker.note_dmabuf_create_immed(40, 41, 8, 4, 0x34325258, 0)?;
        tracker.set_surface_buffer(11, Some(41));
        // Pending attaches must not masquerade as committed child pixels.
        assert_eq!(tracker.subsurfaces_for_window(&window)[0].buffer_kind, None);
        tracker.commit_surface(11);
        assert_eq!(tracker.subsurfaces_for_window(&window)[0].buffer_kind, None);
        tracker.commit_surface(10);
        tracker.destroy_buffer(41);
        let info = tracker
            .list_windows()
            .into_iter()
            .find(|w| w.window_id == window)
            .unwrap();
        assert_eq!(info.buffer_kind.as_deref(), Some("shm"));
        assert_eq!(info.subsurface_count, 1);
        let child = &info.subsurfaces[0];
        assert_eq!(child.buffer_kind.as_deref(), Some("dmabuf"));
        assert_eq!(child.buffer_id, Some(41));
        assert_eq!(
            (child.buffer_width, child.buffer_height),
            (Some(8), Some(4))
        );
        assert!(child.capture_details.as_ref().unwrap().contains("DMA-BUF"));
        assert!(child.commit_serial > 0);
        assert!(child.has_committed_buffer);
        let json = serde_json::to_value(&info).map_err(|e| e.to_string())?;
        assert_eq!(json["subsurface_count"], 1);
        assert_eq!(json["subsurfaces"][0]["buffer_kind"], "dmabuf");
        Ok(())
    }

    #[test]
    fn firefox_style_render_subsurface_inherits_toplevel_identity() -> Result<(), String> {
        let mut tracker = WaylandFrameTracker::new("test-client".to_string());
        tracker.note_xdg_surface_created(30, 10);
        let window_id = tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.note_xdg_toplevel_title(31, Some("Firefox fixture".to_string()));
        tracker.note_xdg_toplevel_app_id(31, Some("firefox".to_string()));

        {
            let parent = tracker.surface_mut(10);
            parent.width = 1600;
            parent.height = 1200;
            parent.rgba = vec![0; 1600 * 1200 * 4];
            parent.has_committed_buffer = true;
        }
        tracker.commit_surface(10);

        {
            let child = tracker.surface_mut(11);
            // The page buffer can be smaller than its shell parent because of
            // output scaling; ancestry must outrank raw pixel area.
            child.width = 1200;
            child.height = 900;
            child.rgba = vec![0xff; 1200 * 900 * 4];
            child.has_committed_buffer = true;
        }
        tracker.commit_surface(11);
        tracker.note_subsurface_created(40, 11, 10);
        tracker.note_subsurface_position(40, 17, 23);
        tracker.commit_surface(10);

        let windows = tracker.list_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].window_id, window_id);
        assert_eq!(windows[0].title.as_deref(), Some("Firefox fixture"));
        assert_eq!(windows[0].app_id.as_deref(), Some("firefox"));
        assert_eq!((windows[0].width, windows[0].height), (1600, 1200));
        assert!(windows[0].capturable);
        assert_eq!(
            tracker
                .surface_for_window(&window_id)
                .map(|surface| surface.id),
            Some(10)
        );
        assert_eq!(
            tracker.click_target_for_window(&window_id, 600, 450)?,
            Some(PointerClickTarget {
                fixed_coords: None,
                window_id: window_id.clone(),
                surface_id: 11,
                screenshot_x: 600,
                screenshot_y: 450,
                surface_x: 583,
                surface_y: 427,
            })
        );
        Ok(())
    }

    #[test]
    fn capture_tracking_temporarily_disables_raw_forwarding() {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test-wayland".to_string(),
            backend_socket: "test-backend".to_string(),
        });
        let client_id = WaylandClientId(1);
        let session = WaylandClientSession::new(client_id, Vec::new(), None);
        session.raw_forward_only.store(true, Ordering::Relaxed);
        server.sessions.insert(client_id, session);

        server.enable_capture_tracking_until(Instant::now() + Duration::from_secs(1));

        assert!(server.capture_tracking_active());
        assert!(
            !server
                .sessions
                .get(&client_id)
                .expect("session")
                .raw_forward_only
                .load(Ordering::Relaxed)
        );
    }

    #[test]
    fn expired_capture_tracking_is_cleared() {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test-wayland".to_string(),
            backend_socket: "test-backend".to_string(),
        });
        server.capture_tracking_deadline = Some(Instant::now() - Duration::from_secs(1));

        assert!(!server.capture_tracking_active());
        assert_eq!(server.capture_tracking_deadline, None);
    }

    #[test]
    fn disconnect_preserves_the_causal_runtime_error() {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test-wayland".to_string(),
            backend_socket: "test-backend".to_string(),
        });
        let client_id = WaylandClientId(7);
        server.sessions.insert(
            client_id,
            WaylandClientSession::new(client_id, Vec::new(), None),
        );
        server.note_runtime_error("client 7 request ingest failed: fixture failure");

        server.remove_client_session(client_id);

        assert_eq!(server.sessions.len(), 0);
        assert_eq!(
            server.last_runtime_error.as_deref(),
            Some("client 7 request ingest failed: fixture failure")
        );
    }

    #[test]
    fn selecting_a_display_name_uses_the_backend_runtime_directory() {
        assert_eq!(
            resolve_backend_choice("wayland-1", "/run/user/1000/wayland-0").unwrap(),
            PathBuf::from("/run/user/1000/wayland-1")
        );
        assert_eq!(
            resolve_backend_choice("/other/session/wayland-2", "/run/user/1000/wayland-0").unwrap(),
            PathBuf::from("/other/session/wayland-2")
        );
        assert!(resolve_backend_choice("../wayland-1", "/run/user/1000/wayland-0").is_err());
    }

    #[tokio::test]
    async fn diagnostics_report_the_proxy_running_without_a_client() {
        let backend = WaylandGuiBackend::new();
        assert!(backend.transport_available());

        let snapshot = backend.snapshot().await;

        assert!(snapshot.running);
        assert_eq!(snapshot.session_count, 0);
    }

    #[tokio::test]
    async fn environment_returns_an_existing_connectable_socket() {
        let backend = WaylandGuiBackend::new();

        let environment = backend
            .launch_environment()
            .expect("live launch environment");
        let socket_path = PathBuf::from(&environment.socket_path);

        assert!(
            std::fs::metadata(&socket_path)
                .expect("proxy socket metadata")
                .file_type()
                .is_socket()
        );
        assert_eq!(environment.wayland_display, DEFAULT_PROXY_SOCKET);
        assert_eq!(environment.launch_preflight.endpoint_state, "ready");
        assert!(environment.launch_preflight.endpoint_exists);
        assert!(environment.launch_preflight.endpoint_is_unix_socket);
        assert!(environment.launch_preflight.accept_loop_running);
        assert_eq!(
            environment.launch_preflight.caller_namespace_access,
            "not_tested"
        );
        assert_eq!(
            environment.launch_preflight.render_node_access,
            "not_tested"
        );
        assert!(backend.snapshot().await.running);
        backend.list_windows().await.expect("window inventory");
        StdUnixStream::connect(&socket_path).expect("connect after intervening console operations");
        StdUnixStream::connect(&socket_path).expect("reconnect to returned proxy endpoint");
    }

    #[tokio::test]
    async fn environment_replaces_a_missing_proxy_endpoint() {
        let backend = WaylandGuiBackend::new();
        let first_environment = backend.sandbox_env().expect("initial environment");
        let first_socket = PathBuf::from(&first_environment["XDG_RUNTIME_DIR"])
            .join(&first_environment["WAYLAND_DISPLAY"]);
        std::fs::remove_file(&first_socket).expect("remove fixture proxy socket");

        let stopped = backend.snapshot().await;
        assert!(!stopped.running);
        assert!(
            stopped
                .last_runtime_error
                .as_deref()
                .is_some_and(|error| error.contains("endpoint") && error.contains("unavailable"))
        );

        let replacement_environment = backend.sandbox_env().expect("replacement environment");
        let replacement_socket = PathBuf::from(&replacement_environment["XDG_RUNTIME_DIR"])
            .join(&replacement_environment["WAYLAND_DISPLAY"]);
        assert_ne!(replacement_socket, first_socket);
        assert!(
            std::fs::metadata(&replacement_socket)
                .expect("replacement socket metadata")
                .file_type()
                .is_socket()
        );
        StdUnixStream::connect(&replacement_socket).expect("connect to replacement endpoint");
        assert!(backend.snapshot().await.running);
    }

    #[tokio::test]
    async fn environment_restarts_a_stopped_accept_loop() {
        let backend = WaylandGuiBackend::new();
        let first_transport = backend.current_transport().expect("initial transport");
        let first_socket = first_transport.socket_path();
        first_transport.accept_task.abort();
        tokio::task::yield_now().await;

        let replacement_environment = backend.sandbox_env().expect("replacement environment");
        let replacement_socket = PathBuf::from(&replacement_environment["XDG_RUNTIME_DIR"])
            .join(&replacement_environment["WAYLAND_DISPLAY"]);

        assert_ne!(replacement_socket, first_socket);
        StdUnixStream::connect(&replacement_socket).expect("connect to replacement endpoint");
        assert!(backend.snapshot().await.running);
    }

    #[test]
    fn color_only_commit_invalidates_cached_pixels() -> Result<(), String> {
        use crate::gui_wayland_generated::GeneratedHookRequest as R;
        let file = tempfile::tempfile().map_err(|e| e.to_string())?;
        let mut tracker = WaylandFrameTracker::new("review".into());
        tracker.note_dmabuf_params_created(20);
        tracker.note_dmabuf_plane(
            20,
            TrackedDmabufPlane {
                fd: duplicate_file_fd(&file)?,
                plane_idx: 0,
                offset: 0,
                stride: 4,
                modifier: 0,
            },
        )?;
        tracker.note_dmabuf_create_immed(20, 21, 1, 1, 0x34324241, 0)?;
        tracker.note_xdg_surface_created(30, 10);
        tracker.note_xdg_toplevel_created(30, 31)?;
        tracker.set_surface_buffer(10, Some(21));
        tracker.commit_surface(10);
        let raw = crate::visual_events::SnapshotPixels {
            raw: vec![128, 128, 128, 255],
            width: 1,
            height: 1,
            format: 0x34324241,
            serial: 1,
        };
        let old = tracker.surfaces[&10].retained_snapshot_pixels(&raw)?;
        tracker.surfaces.get_mut(&10).unwrap().linear_rgba = Arc::new(old.clone());
        tracker.surfaces.get_mut(&10).unwrap().capture_error = None;
        tracker.colors.request(
            1,
            &R::WpColorManagerV1CreateWindowsScrgb {
                image_description: 90,
            },
        );
        tracker.colors.request(
            1,
            &R::WpColorManagerV1GetSurface {
                id: 91,
                surface: Some(10),
            },
        );
        tracker.colors.request(
            91,
            &R::WpColorManagementSurfaceV1SetImageDescription {
                image_description: Some(90),
                render_intent: 0,
            },
        );
        tracker.colors.request(10, &R::WlSurfaceCommit);
        tracker.commit_surface(10);
        let surface = &tracker.surfaces[&10];
        let new = surface.retained_snapshot_pixels(&raw)?;
        assert_ne!(old, new);
        assert!(
            surface.linear_rgba.is_empty() || *surface.linear_rgba == new,
            "color changed but cached pixels retain old interpretation"
        );
        Ok(())
    }
}
