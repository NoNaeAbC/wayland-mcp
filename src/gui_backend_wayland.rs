#![allow(dead_code)]

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
use crate::gui_backend::GuiWaylandKeyboardEvent;
use crate::gui_backend::GuiWaylandKeyboardEventRequest;
use crate::gui_backend::GuiWaylandPointerEvent;
use crate::gui_backend::GuiWaylandPointerEventRequest;
use crate::gui_backend::GuiWindowInfo;
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

const DEFAULT_PROXY_SOCKET: &str = "wayland-mcp-0";
const DEFAULT_BACKEND_SOCKET: &str = "wayland-0";
const SOCKET_ENV: &str = "WAYLAND_MCP_SOCKET";
const BACKEND_SOCKET_ENV: &str = "WAYLAND_MCP_BACKEND_SOCKET";
const BACKEND_BOOTSTRAP_REGISTRY_ID: u32 = 2;
const BACKEND_BOOTSTRAP_CALLBACK_ID: u32 = 3;
const MAX_BACKEND_EVENTS_PER_DRAIN: usize = 1024;
const MAX_CONNECTION_HISTORY: usize = 32;
// These generated interfaces are understood by the proxy. A compositor global
// which is not present in GENERATED_PROTOCOLS must not be advertised: raw
// forwarding cannot correctly associate SCM_RIGHTS file descriptors without
// the request signature. Advertising such a global caused
// ext_data_control_offer_v1.receive to reach the compositor without its fd.
//
// Do not use GeneratedInterfaceSpec::is_global as this capability test. That
// flag only describes the roots selected by the protocol generator and is
// false for supported core globals such as wl_subcompositor and
// wl_data_device_manager. The host registry already tells us whether an
// interface is a global; here we only need to know whether we have its wire
// metadata.
const SUPPRESSED_BACKEND_GLOBALS: &[&str] = &[
    "org_kde_kwin_server_decoration_manager",
    "wp_color_representation_manager_v1",
    "xdg_activation_v1",
];
// wl_shm and wl_shm_pool are decoded/tracked by
// apply_undecoded_client_tracking because they are deliberately absent from
// the generated protocol table. Keep this list explicit: membership means the
// proxy has a compile-time request/FD implementation outside the generator.
const MANUALLY_SUPPORTED_BACKEND_GLOBALS: &[&str] = &["wl_shm"];

#[derive(Clone)]
pub(crate) struct WaylandGuiBackend {
    state: Arc<WaylandProxyState>,
    transport: Arc<StdMutex<Option<Arc<WaylandProxyTransport>>>>,
    transport_error: Arc<StdMutex<Option<String>>>,
}

impl WaylandGuiBackend {
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
            inner: Mutex::new(server),
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

#[async_trait::async_trait]
impl GuiBackend for WaylandGuiBackend {
    async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
        self.state.list_windows().await
    }

    async fn screenshot(&self, request: GuiScreenshotRequest) -> Result<Vec<u8>, String> {
        if let Some(frame) = self.state.screenshot_png(request).await? {
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

struct WaylandProxyState {
    inner: Mutex<WaylandProxyServer>,
}

impl WaylandProxyState {
    async fn resize_window(&self, request: GuiResizeWindowRequest) -> Result<String, String> {
        let mut inner = self.inner.lock().await;
        inner.resize_window(request)
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

    async fn screenshot_png(
        &self,
        request: GuiScreenshotRequest,
    ) -> Result<Option<Vec<u8>>, String> {
        // Sessions switch to raw forwarding after startup to avoid proxy
        // overhead. Temporarily re-enable tracking before a screenshot so the
        // result is not a stale bootstrap frame (notably Firefox's logo).
        let refresh_for = Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + refresh_for;
        let baseline_serial = {
            let mut inner = self.inner.lock().await;
            inner.enable_capture_tracking_until(Instant::now() + refresh_for);
            let windows = inner
                .sessions
                .values()
                .flat_map(|session| session.frame_tracker.list_windows())
                .filter(|window| window.mapped)
                .collect::<Vec<_>>();
            select_window_for_screenshot(&windows, request.window_id.as_deref())?
                .map(|window| window.commit_serial)
                .unwrap_or(0)
        };

        let result = loop {
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
                        session.frame_tracker.capture_window_rgba(&window.window_id)
                    })
                });
                (selected, maybe_frame)
            };
            let refreshed = selected
                .as_ref()
                .map(|window| window.commit_serial > baseline_serial)
                .unwrap_or(false);
            if refreshed || tokio::time::Instant::now() >= deadline {
                if let Some(window) = selected
                    && !window.capturable
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
        self.inner.lock().await.disable_capture_tracking();
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

        let result = loop {
            let (selected_window, selected_frame) = {
                let mut inner = self.inner.lock().await;
                inner.enable_capture_tracking_until(Instant::now() + timeout);
                let windows = inner
                    .sessions
                    .values()
                    .flat_map(|session| session.frame_tracker.list_windows())
                    .filter(|window| window.mapped)
                    .collect::<Vec<_>>();
                let selected_window =
                    select_window_for_screenshot(&windows, request.window_id.as_deref())?.cloned();
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
        {
            let mut inner = self.inner.lock().await;
            inner.disable_capture_tracking();
        }
        result
    }

    async fn click(&self, request: GuiClickRequest) -> Result<String, String> {
        let mut inner = self.inner.lock().await;
        inner.inject_click(request)
    }

    async fn move_pointer(&self, request: GuiPointerMoveRequest) -> Result<String, String> {
        let mut inner = self.inner.lock().await;
        inner.inject_pointer_motion(request)
    }

    async fn emit_wayland_pointer_event(
        &self,
        request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String> {
        let mut inner = self.inner.lock().await;
        inner.emit_wayland_pointer_event(request)
    }

    async fn emit_wayland_keyboard_event(
        &self,
        request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String> {
        let mut inner = self.inner.lock().await;
        inner.emit_wayland_keyboard_event(request)
    }

    async fn keyboard_text_plan(
        &self,
        request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String> {
        let inner = self.inner.lock().await;
        inner.keyboard_text_plan(request)
    }

    async fn register_live_client_session(&self) -> WaylandClientId {
        let mut inner = self.inner.lock().await;
        let globals = inner.synthetic_backend_globals();
        inner.register_client_session(globals)
    }

    async fn finish_client_session(&self, client_id: WaylandClientId, detail: String) {
        let mut inner = self.inner.lock().await;
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
        let mut inner = self.inner.lock().await;
        inner.ingest_request(client_id, message)
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
        &self,
        client_id: WaylandClientId,
        writer: StdUnixStream,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.set_client_event_writer(client_id, writer)
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
    socket_name: String,
    backend_socket: String,
}

impl WaylandProxyConfig {
    fn from_env() -> Self {
        Self {
            socket_name: env::var(SOCKET_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_PROXY_SOCKET.to_string()),
            backend_socket: env::var(BACKEND_SOCKET_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BACKEND_SOCKET.to_string()),
        }
    }
}

struct WaylandProxyServer {
    config: WaylandProxyConfig,
    running: bool,
    next_client_id: u64,
    last_runtime_error: Option<String>,
    connection_history: VecDeque<WaylandConnectionHistoryEntry>,
    capture_tracking_deadline: Option<Instant>,
    sessions: HashMap<WaylandClientId, WaylandClientSession>,
    registry: WaylandProtocolRegistry,
}

impl WaylandProxyServer {
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
            sessions: HashMap::new(),
            registry: WaylandProtocolRegistry::default(),
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
        self.last_runtime_error = Some(error.into());
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
        let Some(deadline) = self.capture_tracking_deadline else {
            return false;
        };
        if Instant::now() <= deadline {
            return true;
        }
        self.disable_capture_tracking();
        false
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
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        session.client_event_writer = Some(writer);
        Ok(())
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

        for session in self.sessions.values_mut() {
            let Some(target) = session.frame_tracker.click_target_for_window(
                &selected_window_id,
                request.x,
                request.y,
            )?
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

        for session in self.sessions.values_mut() {
            let Some(target) = session.frame_tracker.click_target_for_window(
                &selected_window_id,
                request.x,
                request.y,
            )?
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

        for session in self.sessions.values_mut() {
            let target = match &request.event {
                GuiWaylandPointerEvent::Enter { x, y, .. }
                | GuiWaylandPointerEvent::Motion { x, y, .. } => session
                    .frame_tracker
                    .click_target_for_window(&selected_window_id, *x, *y)?,
                GuiWaylandPointerEvent::Button { .. }
                | GuiWaylandPointerEvent::Axis { .. }
                | GuiWaylandPointerEvent::AxisSource { .. }
                | GuiWaylandPointerEvent::AxisStop { .. }
                | GuiWaylandPointerEvent::AxisDiscrete { .. }
                | GuiWaylandPointerEvent::AxisValue120 { .. }
                | GuiWaylandPointerEvent::AxisRelativeDirection { .. }
                | GuiWaylandPointerEvent::Frame => session
                    .frame_tracker
                    .list_windows()
                    .iter()
                    .any(|window| window.window_id == selected_window_id)
                    .then(|| PointerClickTarget {
                        window_id: selected_window_id.clone(),
                        screenshot_x: 0,
                        screenshot_y: 0,
                        surface_id: selected.input_surface_id,
                        surface_x: 0,
                        surface_y: 0,
                    }),
            };
            let Some(target) = target else {
                continue;
            };
            return session.emit_wayland_pointer_event(target, request.event);
        }

        Err(format!(
            "window `{selected_window_id}` disappeared before the raw Wayland pointer event could be emitted"
        ))
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

        for session in self.sessions.values_mut() {
            if session
                .frame_tracker
                .list_windows()
                .iter()
                .any(|window| window.window_id == selected_window_id)
            {
                return session.emit_wayland_keyboard_event(
                    &selected_window_id,
                    selected.input_surface_id,
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
                trace_undecoded_wayland_message("client", &message, Some(&err));
                None
            }
        };
        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        if let Some(request) = request.as_ref() {
            reassociate_client_fds(session, request, &mut message.fds);
        } else {
            apply_undecoded_client_tracking(session, &mut message)?;
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
        let backend_events = if raw_forward_only {
            Vec::new()
        } else if let Some(request) = request.as_ref() {
            local_protocol_response_events(session, request)?
        } else {
            Vec::new()
        };
        let mut backend_globals = session.backend_globals.clone();
        if backend_events.is_empty()
            && !synthetic_ack
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
            backend.forward_raw_request(&message)?;
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
        for event in &backend_events {
            if let Some(decoded) = event.decoded.as_ref() {
                apply_client_event_tracking(session, decoded)?;
            }
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

        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        for event in &backend_events {
            if let Some(decoded) = event.decoded.as_ref() {
                apply_client_event_tracking(session, decoded)?;
            }
        }
        session.backend_globals = backend_globals;
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

        let session = self
            .sessions
            .get_mut(&client_id)
            .ok_or_else(|| format!("missing Wayland client session {}", client_id.0))?;
        if let Some(decoded) = decoded.as_ref() {
            track_keyboard_keymap(session, decoded, &message.fds)?;
            rewrite_dmabuf_feedback_event(session, decoded, &mut message)?;
            apply_client_event_tracking(session, decoded)?;
        }
        session.backend_globals = backend_globals;
        Ok(WaylandBackendEvent {
            encoded: message,
            decoded,
        })
    }
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
    let Some(interface) = session.interface_for_object(header.object_id) else {
        // The recvmsg chunk can attach an FD needed by a later message even
        // when this first message's object/interface is unknown to us.
        reassociate_undecoded_client_fds(session, &mut message.fds, 0);
        return Ok(());
    };
    match (interface, header.opcode) {
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
            // SCM_RIGHTS belongs to the byte stream, not necessarily the first
            // Wayland message decoded from the recvmsg chunk. Preserve FDs seen
            // on an undecoded request so the next request with a known FD
            // signature (for example wl_shm.create_pool) can claim them.
            reassociate_undecoded_client_fds(session, &mut message.fds, 0);
        }
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

struct WaylandClientSession {
    client_id: WaylandClientId,
    backend_globals: Vec<WaylandGlobalInfo>,
    backend: Option<WaylandBackendSession>,
    client_event_writer: Option<StdUnixStream>,
    resource_map: WaylandResourceMap,
    object_interfaces: HashMap<u32, String>,
    object_versions: HashMap<u32, u32>,
    local_registry_ids: HashSet<u32>,
    local_callback_ids: HashSet<u32>,
    pending_client_fds: VecDeque<OwnedFd>,
    dmabuf_feedback_index_maps: HashMap<u32, Vec<Option<u16>>>,
    frame_tracker: WaylandFrameTracker,
    raw_forward_only: Arc<AtomicBool>,
    next_synthetic_serial: u32,
    synthetic_configures: HashSet<(u32, u32)>,
    keyboard_keymap_text: Option<String>,
    keyboard_layout_group: u32,
    keyboard_mods_depressed: u32,
    keyboard_mods_latched: u32,
    keyboard_mods_locked: u32,
}

impl WaylandClientSession {
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
            local_registry_ids: HashSet::new(),
            local_callback_ids: HashSet::new(),
            pending_client_fds: VecDeque::new(),
            dmabuf_feedback_index_maps: HashMap::new(),
            frame_tracker: WaylandFrameTracker::new(format!("wayland-client-{}", client_id.0)),
            raw_forward_only: Arc::new(AtomicBool::new(false)),
            next_synthetic_serial: 1,
            synthetic_configures: HashSet::new(),
            keyboard_keymap_text: None,
            keyboard_layout_group: 0,
            keyboard_mods_depressed: 0,
            keyboard_mods_latched: 0,
            keyboard_mods_locked: 0,
        }
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
        self.object_interfaces.remove(&object_id);
        self.object_versions.remove(&object_id);
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
                surface_x: fixed_from_i64(target.surface_x),
                surface_y: fixed_from_i64(target.surface_y),
            },
            GeneratedEvent::WlPointerMotion {
                time,
                surface_x: fixed_from_i64(target.surface_x),
                surface_y: fixed_from_i64(target.surface_y),
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
                surface_x: fixed_from_i64(target.surface_x),
                surface_y: fixed_from_i64(target.surface_y),
            },
            GeneratedEvent::WlPointerMotion {
                time,
                surface_x: fixed_from_i64(target.surface_x),
                surface_y: fixed_from_i64(target.surface_y),
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
    ) -> Result<String, String> {
        let required_version = match &event {
            GuiWaylandPointerEvent::Enter { .. }
            | GuiWaylandPointerEvent::Motion { .. }
            | GuiWaylandPointerEvent::Button { .. }
            | GuiWaylandPointerEvent::Axis { .. } => 1,
            GuiWaylandPointerEvent::Frame
            | GuiWaylandPointerEvent::AxisSource { .. }
            | GuiWaylandPointerEvent::AxisStop { .. }
            | GuiWaylandPointerEvent::AxisDiscrete { .. } => 5,
            GuiWaylandPointerEvent::AxisValue120 { .. } => 8,
            GuiWaylandPointerEvent::AxisRelativeDirection { .. } => 9,
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
        };
        let generated = match event {
            GuiWaylandPointerEvent::Enter { serial, .. } => GeneratedEvent::WlPointerEnter {
                serial: serial.unwrap_or_else(|| self.next_synthetic_serial()),
                surface: Some(target.surface_id),
                surface_x: fixed_from_i64(target.surface_x),
                surface_y: fixed_from_i64(target.surface_y),
            },
            GuiWaylandPointerEvent::Motion { time, .. } => GeneratedEvent::WlPointerMotion {
                time: time.unwrap_or_else(wayland_timestamp_ms_u32),
                surface_x: fixed_from_i64(target.surface_x),
                surface_y: fixed_from_i64(target.surface_y),
            },
            GuiWaylandPointerEvent::Button {
                button,
                state,
                serial,
                time,
            } => GeneratedEvent::WlPointerButton {
                serial: serial.unwrap_or_else(|| self.next_synthetic_serial()),
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
        };
        let writer = self.client_event_writer.as_ref().ok_or_else(|| {
            "raw Wayland event cannot be emitted because the client stream is unavailable"
                .to_string()
        })?;
        for pointer_id in &pointer_ids {
            let bytes = encode_generated_event(*pointer_id, &generated)?;
            send_wayland_wire_message(writer, &bytes, &[])?;
        }
        Ok(format!(
            "emitted {event_name} through {} wl_pointer resource(s) for window `{}`",
            pointer_ids.len(),
            target.window_id
        ))
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
                    serial: serial.unwrap_or_else(|| self.next_synthetic_serial()),
                    surface: Some(surface_id),
                    keys: key_bytes,
                }
            }
            GuiWaylandKeyboardEvent::Leave { serial } => GeneratedEvent::WlKeyboardLeave {
                serial: serial.unwrap_or_else(|| self.next_synthetic_serial()),
                surface: Some(surface_id),
            },
            GuiWaylandKeyboardEvent::Key {
                key,
                state,
                serial,
                time,
            } => GeneratedEvent::WlKeyboardKey {
                serial: serial.unwrap_or_else(|| self.next_synthetic_serial()),
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
                serial: serial.unwrap_or_else(|| self.next_synthetic_serial()),
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
            send_wayland_wire_message(writer, &bytes, &[])?;
        }
        Ok(format!(
            "emitted {event_name} through {} wl_keyboard resource(s) for window `{window_id}`",
            keyboard_ids.len()
        ))
    }

    fn next_synthetic_serial(&mut self) -> u32 {
        let serial = self.next_synthetic_serial;
        self.next_synthetic_serial = self.next_synthetic_serial.wrapping_add(1).max(1);
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
        .cloned()
        .collect()
}

fn is_suppressed_backend_global(interface: &str) -> bool {
    !is_supported_backend_global(interface)
}

fn is_supported_backend_global(interface: &str) -> bool {
    !SUPPRESSED_BACKEND_GLOBALS.contains(&interface)
        && (MANUALLY_SUPPORTED_BACKEND_GLOBALS.contains(&interface)
            || GENERATED_PROTOCOLS.iter().any(|protocol| {
                protocol
                    .interfaces
                    .iter()
                    .any(|candidate| candidate.name == interface)
            }))
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
    client_to_backend: HashMap<u32, u32>,
    backend_to_client: HashMap<u32, u32>,
}

impl WaylandResourceMap {
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
    next_window_id: u64,
    dmabuf_params: HashMap<u32, PendingDmabufParams>,
    shm_pools: HashMap<u32, TrackedShmPool>,
    buffers: HashMap<u32, TrackedBuffer>,
    surfaces: HashMap<u32, TrackedSurface>,
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
            next_window_id: 0,
            dmabuf_params: HashMap::new(),
            shm_pools: HashMap::new(),
            buffers: HashMap::new(),
            surfaces: HashMap::new(),
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
            TrackedBuffer {
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
            },
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
            TrackedBuffer {
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
            },
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
            TrackedBuffer {
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
            },
        );
        Ok(())
    }

    fn set_surface_buffer(&mut self, surface_id: u32, buffer_id: Option<u32>) {
        self.ensure_window_for_surface(surface_id);
        let buffer_state = buffer_id.and_then(|id| {
            self.buffers.get(&id).map(|buffer| {
                (
                    buffer.width,
                    buffer.height,
                    buffer.rgba.clone().unwrap_or_default(),
                    buffer.kind_name(),
                    buffer.capture_error(),
                )
            })
        });
        let surface = self.surface_mut(surface_id);
        surface.buffer_id = buffer_id;
        if let Some((width, height, rgba, kind_name, capture_error)) = buffer_state {
            surface.width = width.max(1);
            surface.height = height.max(1);
            surface.rgba = rgba;
            surface.buffer_kind = Some(kind_name);
            surface.capture_error = capture_error;
        } else if buffer_id.is_none() {
            surface.width = 1;
            surface.height = 1;
            surface.rgba = vec![0, 0, 0, 0];
            surface.buffer_kind = None;
            surface.capture_error = None;
        }
        self.sync_window_from_surface(surface_id);
    }

    fn destroy_buffer(&mut self, buffer_id: u32) {
        self.buffers.remove(&buffer_id);
        for surface in self.surfaces.values_mut() {
            if surface.buffer_id == Some(buffer_id) {
                surface.buffer_id = None;
            }
        }
    }

    fn add_damage(&mut self, surface_id: u32, rect: DamageRect) {
        self.surface_mut(surface_id).damage.push(rect);
    }

    fn commit_surface(&mut self, surface_id: u32) {
        self.ensure_window_for_surface(surface_id);
        self.next_commit_serial = self.next_commit_serial.saturating_add(1);
        let commit_serial = self.next_commit_serial;
        let surface = self.surface_mut(surface_id);
        surface.commit_serial = commit_serial;
        if surface.buffer_id.is_some() {
            surface.has_committed_buffer = true;
        }
        surface.last_acquire = surface.pending_acquire.take();
        surface.last_release = surface.pending_release.take();
        surface.damage.clear();
        self.sync_window_from_surface(surface_id);
    }

    fn latest_surface(&self) -> Option<&TrackedSurface> {
        self.surfaces
            .values()
            .max_by_key(|surface| surface.commit_serial)
    }

    fn list_windows(&self) -> Vec<GuiWindowInfo> {
        let mut windows = self
            .windows
            .values()
            .map(|window| {
                let surface = self.surfaces.get(&window.wl_surface_id);
                let buffer = surface
                    .and_then(|surface| surface.buffer_id)
                    .and_then(|buffer_id| self.buffers.get(&buffer_id));
                let capturable = surface
                    .map(|surface| {
                        !surface.rgba.is_empty()
                            || buffer.map(TrackedBuffer::has_readback).unwrap_or(false)
                    })
                    .unwrap_or(false);
                let capture_output_count =
                    usize::from(window.mapped && window.commit_serial > 0 && capturable);
                GuiWindowInfo {
                    window_id: window.window_id.clone(),
                    title: window.title.clone(),
                    app_id: window.app_id.clone(),
                    width: window.width.max(1),
                    height: window.height.max(1),
                    mapped: window.mapped,
                    focused: window.focused,
                    commit_serial: window.commit_serial,
                    on_capture_output: capture_output_count > 0,
                    capture_output_count,
                    on_backend_output: window.output_count > 0,
                    backend_output_count: window.output_count,
                    buffer_kind: surface
                        .and_then(|surface| surface.buffer_kind.map(str::to_string)),
                    sync_state: surface.and_then(TrackedSurface::sync_state),
                    capturable,
                    capture_error: surface.and_then(|surface| surface.capture_error.clone()),
                    render_surface_id: window.wl_surface_id,
                    input_surface_id: window.input_surface_id,
                    capture_details: buffer.map(TrackedBuffer::capture_details),
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
                id: surface_id,
                buffer_id: None,
                width: 1,
                height: 1,
                rgba: Vec::new(),
                output_ids: BTreeSet::new(),
                buffer_kind: None,
                capture_error: None,
                pending_acquire: None,
                pending_release: None,
                last_acquire: None,
                last_release: None,
                damage: Vec::new(),
                commit_serial: 0,
                has_committed_buffer: false,
                xdg_configure_seen: false,
                xdg_configure_acked: false,
                viewport_destination: None,
            })
    }

    fn note_xdg_surface_created(&mut self, xdg_surface_id: u32, wl_surface_id: u32) {
        self.xdg_surface_to_surface
            .insert(xdg_surface_id, wl_surface_id);
    }

    fn note_subsurface_created(
        &mut self,
        subsurface_id: u32,
        surface_id: u32,
        parent_surface_id: u32,
    ) {
        self.subsurface_to_surface.insert(subsurface_id, surface_id);
        self.surface_parent.insert(surface_id, parent_surface_id);
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
        self.promote_render_surface(&parent_window_id, surface_id);
        self.sync_window_from_surface(surface_id);
    }

    fn note_subsurface_destroyed(&mut self, subsurface_id: u32) {
        if let Some(surface_id) = self.subsurface_to_surface.remove(&subsurface_id) {
            self.surface_parent.remove(&surface_id);
            self.surface_position.remove(&surface_id);
        }
    }

    fn note_subsurface_position(&mut self, subsurface_id: u32, x: i32, y: i32) {
        if let Some(surface_id) = self.subsurface_to_surface.get(&subsurface_id).copied() {
            self.surface_position.insert(surface_id, (x, y));
        }
    }

    fn promote_render_surface(&mut self, window_id: &str, candidate_id: u32) {
        let Some(candidate) = self.surfaces.get(&candidate_id) else {
            return;
        };
        let candidate_score = (
            candidate.has_committed_buffer,
            u64::from(candidate.width) * u64::from(candidate.height),
            candidate.commit_serial,
        );
        let Some(current_id) = self
            .windows
            .get(window_id)
            .map(|window| window.wl_surface_id)
        else {
            return;
        };
        // A committed child subsurface is composited into its ancestor and is
        // often the actual application render target. Firefox, for example,
        // uses a larger transparent/black SHM parent around a scaled DMA-BUF
        // child. Pixel area alone therefore selects the wrong surface.
        let candidate_descends_from_current = self.surface_descends_from(candidate_id, current_id);
        let current_descends_from_candidate = self.surface_descends_from(current_id, candidate_id);
        let current_score = self
            .surfaces
            .get(&current_id)
            .map(|surface| {
                (
                    surface.has_committed_buffer,
                    u64::from(surface.width) * u64::from(surface.height),
                    surface.commit_serial,
                )
            })
            .unwrap_or((false, 0, 0));
        if ((candidate_descends_from_current && candidate.has_committed_buffer)
            || (!current_descends_from_candidate && candidate_score > current_score))
            && let Some(window) = self.windows.get_mut(window_id)
        {
            window.wl_surface_id = candidate_id;
        }
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
            self.surface_mut(wl_surface_id).viewport_destination = None;
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
        self.surface_mut(wl_surface_id).viewport_destination = destination;
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
                focused: false,
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
        if let Some(window_id) = self.surface_to_window.get(&wl_surface_id).cloned()
            && let Some(window) = self.windows.get_mut(&window_id)
        {
            window.width = width.max(1);
            window.height = height.max(1);
        }
        Ok(())
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
        let Some(window) = self.windows.get(window_id) else {
            return Ok(None);
        };
        let surface = self.surfaces.get(&window.wl_surface_id).ok_or_else(|| {
            format!(
                "missing wl_surface {} for window `{window_id}`",
                window.wl_surface_id
            )
        })?;
        if x < 0 || y < 0 || x >= i64::from(surface.width) || y >= i64::from(surface.height) {
            return Err(format!(
                "click coordinate ({x}, {y}) is outside screenshot bounds {}x{} for window `{window_id}`",
                surface.width, surface.height
            ));
        }
        let screenshot_x = x;
        let screenshot_y = y;
        let (logical_width, logical_height) = surface
            .viewport_destination
            .unwrap_or((surface.width.max(1), surface.height.max(1)));
        let mut surface_x = screenshot_x.saturating_mul(i64::from(logical_width.max(1)))
            / i64::from(surface.width.max(1));
        let mut surface_y = screenshot_y.saturating_mul(i64::from(logical_height.max(1)))
            / i64::from(surface.height.max(1));
        let mut current_surface_id = window.wl_surface_id;
        let mut visited = HashSet::new();
        while current_surface_id != window.input_surface_id && visited.insert(current_surface_id) {
            let Some(parent_id) = self.surface_parent.get(&current_surface_id).copied() else {
                break;
            };
            let (offset_x, offset_y) = self
                .surface_position
                .get(&current_surface_id)
                .copied()
                .unwrap_or((0, 0));
            surface_x = surface_x.saturating_add(i64::from(offset_x));
            surface_y = surface_y.saturating_add(i64::from(offset_y));
            current_surface_id = parent_id;
        }
        if current_surface_id != window.input_surface_id {
            return Err(format!(
                "cannot translate render surface {} coordinates to input surface {} for window `{window_id}`",
                window.wl_surface_id, window.input_surface_id
            ));
        }
        Ok(Some(PointerClickTarget {
            window_id: window_id.to_string(),
            surface_id: window.input_surface_id,
            screenshot_x,
            screenshot_y,
            surface_x,
            surface_y,
        }))
    }

    fn capture_window_rgba(&self, window_id: &str) -> Option<Result<CapturedRgbaFrame, String>> {
        let surface = self.surface_for_window(window_id)?;
        let color = self.colors.get(surface.id);
        if let Some(color) = color
            && let Err(error) = color.validate()
        {
            return Some(Err(error));
        }
        if !surface.rgba.is_empty() {
            let mut rgba = surface.rgba.clone();
            convert_rgba8_colors(&mut rgba, color);
            return Some(Ok(CapturedRgbaFrame {
                width: surface.width,
                height: surface.height,
                rgba,
                color: color.map(crate::gui_color::ColorDescription::metadata),
            }));
        }
        let buffer_id = surface.buffer_id?;
        let buffer = self.buffers.get(&buffer_id)?;
        if let Some(acquire) = surface.last_acquire
            && let Err(err) = self.wait_sync_point(acquire)
        {
            return Some(Err(err));
        }
        Some(buffer.read_rgba(surface, color))
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
                focused: false,
                commit_serial: surface.commit_serial,
                output_count: surface.output_ids.len(),
            },
        );
        window_id
    }

    fn sync_window_from_surface(&mut self, surface_id: u32) {
        let window_id = self.ensure_window_for_surface(surface_id);
        self.promote_render_surface(&window_id, surface_id);
        let Some(surface) = self.surfaces.get(&surface_id).cloned() else {
            return;
        };
        if let Some(window) = self.windows.get_mut(&window_id) {
            if window.wl_surface_id != surface_id {
                return;
            }
            window.width = surface.width.max(1);
            window.height = surface.height.max(1);
            window.commit_serial = surface.commit_serial;
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
        self.surface_mut(surface_id).pending_acquire =
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
        self.surface_mut(surface_id).pending_release =
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedSurface {
    id: u32,
    buffer_id: Option<u32>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    output_ids: BTreeSet<u32>,
    buffer_kind: Option<&'static str>,
    capture_error: Option<String>,
    pending_acquire: Option<TrackedSyncPoint>,
    pending_release: Option<TrackedSyncPoint>,
    last_acquire: Option<TrackedSyncPoint>,
    last_release: Option<TrackedSyncPoint>,
    damage: Vec<DamageRect>,
    commit_serial: u64,
    has_committed_buffer: bool,
    xdg_configure_seen: bool,
    xdg_configure_acked: bool,
    viewport_destination: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PointerClickTarget {
    window_id: String,
    surface_id: u32,
    screenshot_x: i64,
    screenshot_y: i64,
    surface_x: i64,
    surface_y: i64,
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u32)
        .unwrap_or(0)
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
    focused: bool,
    commit_serial: u64,
    output_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    let mut backend_shutdown = None;
    let mut backend_relay = if let Some(backend_reader) = backend_reader {
        backend_shutdown =
            Some(backend_reader.try_clone().map_err(|err| {
                format!("failed to clone Wayland backend shutdown stream: {err}")
            })?);
        let client_writer = stream
            .try_clone()
            .map_err(|err| format!("failed to clone Wayland client stream: {err}"))?;
        let client_event_writer = client_writer
            .try_clone()
            .map_err(|err| format!("failed to clone Wayland client event stream: {err}"))?;
        runtime.block_on(state.set_client_event_writer(client_id, client_event_writer))?;
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
                client_writer,
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
                    if let Err(err) =
                        send_wayland_wire_message(&stream, &event.encoded.bytes, &event.encoded.fds)
                    {
                        break 'client_loop Err(err);
                    }
                }
            }
            Err(err) if is_timeout_error(&err) => {
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
    client_writer: StdUnixStream,
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
        if event.decoded.as_ref().is_some_and(|event| {
            is_suppressed_registry_global_event(event) || is_unsupported_dmabuf_advertisement(event)
        }) {
            continue;
        }
        if let Err(err) =
            send_wayland_wire_message(&client_writer, &event.encoded.bytes, &event.encoded.fds)
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
}

fn apply_object_tracking(
    session: &mut WaylandClientSession,
    registry: &WaylandProtocolRegistry,
    request: &DecodedWaylandRequest,
) -> Result<(), String> {
    if let Some(GeneratedTrackedRequest::WlRegistryBind {
        id_interface: Some(interface),
        id_version,
        id,
        ..
    }) = request.tracked_request.as_ref()
    {
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
    let args = decode_wayland_args(bytes, header.size, request.args)?;
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
    if size < 8 {
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
    if size < 8 {
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

fn send_wayland_wire_message(
    stream: &StdUnixStream,
    bytes: &[u8],
    fds: &[OwnedFd],
) -> Result<(), String> {
    send_wayland_wire_message_to_fd(stream.as_raw_fd(), bytes, fds)
}

fn send_wayland_wire_message_to_fd(
    fd: libc::c_int,
    bytes: &[u8],
    fds: &[OwnedFd],
) -> Result<(), String> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::addr_of_mut!(iov);
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

    let sent = unsafe { libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(format!(
            "failed to send Wayland wire message with fds: {}",
            std::io::Error::last_os_error()
        ));
    }
    if sent as usize != bytes.len() {
        return Err(format!(
            "short send when writing Wayland wire message: sent {} of {} bytes",
            sent,
            bytes.len()
        ));
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

    let received = unsafe { libc::recvmsg(fd, &mut hdr, flags) };
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
    let mut header = recv_wayland_wire_message_from_fd_with_flags(fd, 8, max_fds, flags)?;
    if header.bytes.is_empty() {
        return Err("backend Wayland stream reached EOF".to_string());
    }
    if header.bytes.len() != 8 {
        return Err(format!(
            "failed to read complete Wayland header: received {} bytes",
            header.bytes.len()
        ));
    }
    let word_1 = u32::from_ne_bytes(
        header.bytes[4..8]
            .try_into()
            .map_err(|_| "invalid Wayland header word 1".to_string())?,
    );
    let size = (word_1 >> 16) as usize;
    if size < header.bytes.len() {
        return Err(format!("invalid Wayland message size {size}"));
    }
    if size > header.bytes.len() {
        let mut payload = recv_wayland_wire_message_from_fd_with_flags(
            fd,
            size - header.bytes.len(),
            max_fds,
            flags,
        )?;
        if payload.bytes.len() + header.bytes.len() != size {
            return Err(format!(
                "failed to read complete Wayland payload: received {} of {} bytes",
                payload.bytes.len(),
                size - header.bytes.len()
            ));
        }
        header.bytes.append(&mut payload.bytes);
        header.fds.append(&mut payload.fds);
    }
    Ok(header)
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
    let mut message = loop {
        stream
            .readable()
            .await
            .map_err(|err| format!("Wayland proxy stream not readable: {err}"))?;
        match stream.try_io(tokio::io::Interest::READABLE, || {
            recv_wayland_wire_message_from_fd(stream.as_raw_fd(), 8, max_fds)
                .map_err(string_error_to_io)
        }) {
            Ok(message) => {
                if message.bytes.is_empty() {
                    return Ok(None);
                }
                if message.bytes.len() != 8 {
                    return Err(format!(
                        "failed to read complete Wayland header: received {} bytes",
                        message.bytes.len()
                    ));
                }
                break message;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(err) => return Err(format!("failed to read Wayland header: {err}")),
        }
    };

    let word_1 = u32::from_ne_bytes(
        message.bytes[4..8]
            .try_into()
            .map_err(|_| "invalid Wayland header word 1".to_string())?,
    );
    let size = (word_1 >> 16) as usize;
    if size < message.bytes.len() {
        return Err(format!("invalid Wayland message size {size}"));
    }
    while message.bytes.len() < size {
        stream
            .readable()
            .await
            .map_err(|err| format!("Wayland proxy stream not readable: {err}"))?;
        let remaining = size - message.bytes.len();
        let mut payload = vec![0u8; remaining];
        match stream.try_read(&mut payload) {
            Ok(0) => return Err("backend Wayland stream reached EOF".to_string()),
            Ok(read) => {
                payload.truncate(read);
                message.bytes.extend_from_slice(&payload);
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
    match request.tracked_request.as_ref() {
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
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
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
    use pretty_assertions::assert_eq;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    #[test]
    fn resize_sends_an_xdg_configure_and_tracks_its_local_ack() -> Result<(), String> {
        let (writer, mut reader) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|e| e.to_string())?;
        let mut session = WaylandClientSession::new(WaylandClientId(1), Vec::new(), None);
        session.client_event_writer = Some(writer);
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
        let duplicated = unsafe { libc::dup(file.as_raw_fd()) };
        if duplicated < 0 {
            return Err(format!(
                "failed to duplicate fixture fd: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
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
            Some(
                "DMA-BUF DRM format 0x3231564E (NV12) is not supported for screenshots and is not advertised by this MCP"
            )
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

    #[test]
    fn supported_ab4h_buffer_is_reported_capturable() -> Result<(), String> {
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
        assert!(windows[0].capturable);
        assert_eq!(windows[0].capture_error, None);
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
                focused: false,
                commit_serial: 1,
                on_capture_output: true,
                capture_output_count: 1,
                on_backend_output: false,
                backend_output_count: 0,
                buffer_kind: Some("dmabuf".to_string()),
                sync_state: Some(
                    "acquire:timeline=41:point=7, release:timeline=41:point=9".to_string(),
                ),
                capturable: true,
                capture_error: None,
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
        assert!(window.on_capture_output);
        assert_eq!(window.capture_output_count, 1);
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

        let windows = tracker.list_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].window_id, window_id);
        assert_eq!(windows[0].title.as_deref(), Some("Firefox fixture"));
        assert_eq!(windows[0].app_id.as_deref(), Some("firefox"));
        assert_eq!((windows[0].width, windows[0].height), (1200, 900));
        assert!(windows[0].capturable);
        assert_eq!(
            tracker
                .surface_for_window(&window_id)
                .map(|surface| surface.id),
            Some(11)
        );
        assert_eq!(
            tracker.click_target_for_window(&window_id, 600, 450)?,
            Some(PointerClickTarget {
                window_id: window_id.clone(),
                surface_id: 10,
                screenshot_x: 600,
                screenshot_y: 450,
                surface_x: 617,
                surface_y: 473,
            })
        );
        Ok(())
    }

    #[test]
    fn capture_tracking_temporarily_disables_raw_forwarding() {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
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
}
