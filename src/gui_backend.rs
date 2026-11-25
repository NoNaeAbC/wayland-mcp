#![cfg_attr(test, allow(dead_code))]

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::sync::OnceLock;

#[cfg(unix)]
use crate::gui_backend_wayland::WaylandGuiBackend;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum DesktopSession {
    Wayland,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiClickRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) button: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiPointerMoveRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) x: i64,
    pub(crate) y: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiWaylandPointerEventRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) surface_id: Option<String>,
    pub(crate) surface_fixed: bool,
    pub(crate) event: GuiWaylandPointerEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiWaylandTouchEventRequest {
    pub(crate) window_id: String,
    pub(crate) surface_id: Option<String>,
    pub(crate) event: GuiWaylandTouchEvent,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum GuiWaylandTouchEvent {
    Down { id: i32, x: i32, y: i32 },
    Up { id: i32 },
    Motion { id: i32, x: i32, y: i32 },
    Frame,
    Cancel,
    Shape { id: i32, major: i32, minor: i32 },
    Orientation { id: i32, orientation: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiWaylandKeyboardEventRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) surface_id: Option<String>,
    pub(crate) event: GuiWaylandKeyboardEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiKeyboardTextPlanRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) text: String,
    pub(crate) allow_compose: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GuiKeyboardTextStroke {
    pub(crate) key: u32,
    pub(crate) modifiers: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GuiKeyboardTextPlan {
    pub(crate) layout_group: u32,
    pub(crate) restore_mods_depressed: u32,
    pub(crate) restore_mods_latched: u32,
    pub(crate) restore_mods_locked: u32,
    pub(crate) shift_modifier: Option<u32>,
    pub(crate) control_modifier: Option<u32>,
    pub(crate) alt_modifier: Option<u32>,
    pub(crate) logo_modifier: Option<u32>,
    pub(crate) strokes: Vec<GuiKeyboardTextStroke>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum GuiWaylandPointerEvent {
    Enter {
        x: i64,
        y: i64,
        #[serde(default)]
        serial: Option<u32>,
    },
    Leave {
        #[serde(default)]
        serial: Option<u32>,
    },
    Motion {
        x: i64,
        y: i64,
        #[serde(default)]
        time: Option<u32>,
    },
    Button {
        button: u32,
        state: u32,
        #[serde(default)]
        serial: Option<u32>,
        #[serde(default)]
        time: Option<u32>,
    },
    Axis {
        axis: u32,
        value: i32,
        #[serde(default)]
        time: Option<u32>,
    },
    AxisSource {
        axis_source: u32,
    },
    AxisStop {
        axis: u32,
        #[serde(default)]
        time: Option<u32>,
    },
    AxisDiscrete {
        axis: u32,
        discrete: i32,
    },
    AxisValue120 {
        axis: u32,
        value120: i32,
    },
    RelativeMotion {
        utime_hi: u32,
        utime_lo: u32,
        dx: i32,
        dy: i32,
        dx_unaccel: i32,
        dy_unaccel: i32,
    },
    AxisRelativeDirection {
        axis: u32,
        direction: u32,
    },
    Frame,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum GuiWaylandKeyboardEvent {
    Enter {
        #[serde(default)]
        serial: Option<u32>,
        #[serde(default)]
        keys: Vec<u32>,
    },
    Leave {
        #[serde(default)]
        serial: Option<u32>,
    },
    Key {
        key: u32,
        state: u32,
        #[serde(default)]
        serial: Option<u32>,
        #[serde(default)]
        time: Option<u32>,
    },
    Modifiers {
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
        #[serde(default)]
        serial: Option<u32>,
    },
    RepeatInfo {
        rate: i32,
        delay: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GuiScreenshotRequest {
    pub(crate) window_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GuiCaptureNextFrameRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) after_commit_serial: Option<u64>,
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiResizeWindowRequest {
    pub(crate) window_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GuiSubsurfaceInfo {
    pub(crate) subsurface_id: u32,
    pub(crate) surface_id: u32,
    pub(crate) parent_surface_id: u32,
    pub(crate) position: (i32, i32),
    /// Effective synchronization includes synchronized ancestors.
    pub(crate) synchronized: bool,
    pub(crate) has_committed_buffer: bool,
    pub(crate) buffer_id: Option<u32>,
    pub(crate) buffer_kind: Option<String>,
    pub(crate) buffer_width: Option<u32>,
    pub(crate) buffer_height: Option<u32>,
    pub(crate) commit_serial: u64,
    pub(crate) capture_details: Option<String>,
    pub(crate) capture_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GuiWindowInfo {
    pub(crate) window_id: String,
    pub(crate) title: Option<String>,
    pub(crate) app_id: Option<String>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) mapped: bool,
    pub(crate) commit_serial: u64,
    /// Membership on the MCP's virtual capture output. A mapped window with a
    /// readable committed buffer belongs to exactly one capture output.
    pub(crate) on_capture_output: bool,
    pub(crate) capture_output_count: usize,
    /// Membership reported by wl_surface.enter/leave from the host compositor.
    pub(crate) on_backend_output: bool,
    pub(crate) backend_output_count: usize,
    /// Buffer metadata below describes render_surface_id (currently the root),
    /// not every buffer in the window's subsurface tree.
    pub(crate) buffer_kind: Option<String>,
    /// All live wl_subsurface descendants, including nested and unbuffered ones.
    pub(crate) subsurface_count: usize,
    pub(crate) subsurfaces: Vec<GuiSubsurfaceInfo>,
    pub(crate) sync_state: Option<String>,
    pub(crate) capturable: bool,
    pub(crate) capture_error: Option<String>,
    /// Protocol object carrying the pixels returned by screenshot operations.
    pub(crate) render_surface_id: u32,
    /// Protocol object receiving synthetic pointer focus and input.
    pub(crate) input_surface_id: u32,
    /// Concrete buffer import/readback path, format, and modifier information.
    pub(crate) capture_details: Option<String>,
}

#[async_trait]
pub(crate) trait GuiBackend: Send + Sync {
    #[cfg(test)]
    async fn begin_observation(
        &self,
        _window: String,
        _duration_ms: u64,
    ) -> Result<serde_json::Value, String> {
        Err("observation is not implemented by this test backend".into())
    }
    #[cfg(test)]
    async fn end_observation(&self, _id: u64) -> Result<serde_json::Value, String> {
        Err("observation is not implemented by this test backend".into())
    }
    async fn cleanup_model_input(&self) -> Result<(), String> {
        Ok(())
    }
    fn input_hub(&self) -> Option<Arc<crate::input_events::InputHub>> {
        None
    }
    async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String>;

    async fn screenshot(&self, request: GuiScreenshotRequest) -> Result<Vec<u8>, String>;

    async fn capture_next_frame(
        &self,
        request: GuiCaptureNextFrameRequest,
    ) -> Result<Vec<u8>, String>;

    async fn emit_wayland_pointer_event(
        &self,
        request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String>;

    async fn emit_wayland_keyboard_event(
        &self,
        request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String>;

    async fn keyboard_text_plan(
        &self,
        request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String>;
}

#[derive(Clone)]
pub(crate) enum GuiBackendHandle {
    Command(Arc<CommandGuiBackend>),
    #[cfg(unix)]
    Wayland(Arc<WaylandGuiBackend>),
    #[cfg(test)]
    Test(Arc<dyn GuiBackend>),
}

impl GuiBackendHandle {
    pub(crate) async fn cleanup_model_input(&self) -> Result<(), String> {
        match self {
            Self::Wayland(backend) => backend.cleanup_model_input().await,
            #[cfg(test)]
            Self::Test(backend) => backend.cleanup_model_input().await,
            _ => Ok(()),
        }
    }
    pub(crate) fn input_hub(&self) -> Option<Arc<crate::input_events::InputHub>> {
        match self {
            Self::Wayland(backend) => backend.input_hub(),
            #[cfg(test)]
            Self::Test(backend) => backend.input_hub(),
            _ => None,
        }
    }
    pub(crate) async fn resize_window(
        &self,
        request: GuiResizeWindowRequest,
    ) -> Result<String, String> {
        match self {
            #[cfg(unix)]
            Self::Wayland(backend) => backend.resize_window(request).await,
            _ => Err("window resizing requires the Wayland proxy backend".to_string()),
        }
    }
    pub(crate) async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
        match self {
            Self::Command(backend) => backend.list_windows().await,
            #[cfg(unix)]
            Self::Wayland(backend) => backend.list_windows().await,
            #[cfg(test)]
            Self::Test(backend) => backend.list_windows().await,
        }
    }

    pub(crate) async fn screenshot(
        &self,
        request: GuiScreenshotRequest,
    ) -> Result<Vec<u8>, String> {
        match self {
            Self::Command(backend) => backend.screenshot(request).await,
            #[cfg(unix)]
            Self::Wayland(backend) => backend.screenshot(request).await,
            #[cfg(test)]
            Self::Test(backend) => backend.screenshot(request).await,
        }
    }

    pub(crate) async fn capture_next_frame(
        &self,
        request: GuiCaptureNextFrameRequest,
    ) -> Result<Vec<u8>, String> {
        match self {
            Self::Command(backend) => backend.capture_next_frame(request).await,
            #[cfg(unix)]
            Self::Wayland(backend) => backend.capture_next_frame(request).await,
            #[cfg(test)]
            Self::Test(backend) => backend.capture_next_frame(request).await,
        }
    }

    pub(crate) async fn emit_wayland_pointer_event(
        &self,
        request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String> {
        match self {
            Self::Command(backend) => backend.emit_wayland_pointer_event(request).await,
            #[cfg(unix)]
            Self::Wayland(backend) => backend.emit_wayland_pointer_event(request).await,
            #[cfg(test)]
            Self::Test(backend) => backend.emit_wayland_pointer_event(request).await,
        }
    }

    pub(crate) async fn begin_observation(
        &self,
        window: String,
        duration_ms: u64,
    ) -> Result<serde_json::Value, String> {
        match self {
            #[cfg(unix)]
            Self::Wayland(backend) => backend.begin_observation(window, duration_ms).await,
            #[cfg(test)]
            Self::Test(backend) => backend.begin_observation(window, duration_ms).await,
            _ => Err("observation requires the Wayland backend".into()),
        }
    }
    pub(crate) async fn end_observation(&self, id: u64) -> Result<serde_json::Value, String> {
        match self {
            #[cfg(unix)]
            Self::Wayland(backend) => backend.end_observation(id).await,
            #[cfg(test)]
            Self::Test(backend) => backend.end_observation(id).await,
            _ => Err("observation requires the Wayland backend".into()),
        }
    }
    pub(crate) async fn input_capabilities(
        &self,
        window: &str,
    ) -> Result<serde_json::Value, String> {
        match self {
            #[cfg(unix)]
            Self::Wayland(backend) => backend.input_capabilities(window).await,
            _ => Err("input capabilities require the Wayland proxy backend".into()),
        }
    }
    pub(crate) async fn emit_wayland_touch_event(
        &self,
        request: GuiWaylandTouchEventRequest,
    ) -> Result<String, String> {
        match self {
            #[cfg(unix)]
            Self::Wayland(backend) => backend.emit_wayland_touch_event(request).await,
            _ => Err("touch input requires the Wayland proxy backend".into()),
        }
    }

    pub(crate) async fn emit_wayland_keyboard_event(
        &self,
        request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String> {
        match self {
            Self::Command(backend) => backend.emit_wayland_keyboard_event(request).await,
            #[cfg(unix)]
            Self::Wayland(backend) => backend.emit_wayland_keyboard_event(request).await,
            #[cfg(test)]
            Self::Test(backend) => backend.emit_wayland_keyboard_event(request).await,
        }
    }

    pub(crate) async fn keyboard_text_plan(
        &self,
        request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String> {
        match self {
            Self::Command(backend) => backend.keyboard_text_plan(request).await,
            #[cfg(unix)]
            Self::Wayland(backend) => backend.keyboard_text_plan(request).await,
            #[cfg(test)]
            Self::Test(backend) => backend.keyboard_text_plan(request).await,
        }
    }
}

pub(crate) fn desktop_session() -> Option<DesktopSession> {
    if cfg!(unix)
        && (std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_MCP_BACKEND_SOCKET").is_some())
    {
        Some(DesktopSession::Wayland)
    } else {
        None
    }
}

pub(crate) fn default_gui_backend() -> GuiBackendHandle {
    match desktop_session() {
        #[cfg(unix)]
        Some(DesktopSession::Wayland) => GuiBackendHandle::Wayland(default_wayland_backend()),
        #[cfg(not(unix))]
        Some(DesktopSession::Wayland) => GuiBackendHandle::Command(default_command_backend()),
        None => GuiBackendHandle::Command(default_command_backend()),
    }
}

pub(crate) fn ensure_default_gui_backend_initialized() {
    // The host environment belongs to this process. Applications receive the
    // proxy environment explicitly through wayland.environment().
    let _ = default_gui_backend();
}

fn default_command_backend() -> Arc<CommandGuiBackend> {
    static COMMAND_BACKEND: OnceLock<Arc<CommandGuiBackend>> = OnceLock::new();
    COMMAND_BACKEND
        .get_or_init(|| Arc::new(CommandGuiBackend))
        .clone()
}

#[cfg(unix)]
fn default_wayland_backend() -> Arc<WaylandGuiBackend> {
    static WAYLAND_BACKEND: OnceLock<Arc<WaylandGuiBackend>> = OnceLock::new();
    WAYLAND_BACKEND
        .get_or_init(|| Arc::new(WaylandGuiBackend::new()))
        .clone()
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CommandGuiBackend;

#[async_trait]
impl GuiBackend for CommandGuiBackend {
    async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
        Ok(Vec::new())
    }

    async fn screenshot(&self, request: GuiScreenshotRequest) -> Result<Vec<u8>, String> {
        if let Some(window_id) = request.window_id.as_deref()
            && window_id != "desktop"
        {
            return Err(format!(
                "window_id `{window_id}` is not available because GUI capture only supports windows connected through the Wayland MCP proxy"
            ));
        }
        Err(
            "wayland.screenshot is only available for windows connected through the Wayland proxy"
                .to_string(),
        )
    }

    async fn capture_next_frame(
        &self,
        request: GuiCaptureNextFrameRequest,
    ) -> Result<Vec<u8>, String> {
        let screenshot_request = GuiScreenshotRequest {
            window_id: request.window_id,
        };
        self.screenshot(screenshot_request).await
    }

    async fn emit_wayland_pointer_event(
        &self,
        _request: GuiWaylandPointerEventRequest,
    ) -> Result<String, String> {
        Err("raw Wayland pointer events are unavailable on the command backend".to_string())
    }

    async fn emit_wayland_keyboard_event(
        &self,
        _request: GuiWaylandKeyboardEventRequest,
    ) -> Result<String, String> {
        Err("raw Wayland keyboard events are unavailable on the command backend".to_string())
    }

    async fn keyboard_text_plan(
        &self,
        _request: GuiKeyboardTextPlanRequest,
    ) -> Result<GuiKeyboardTextPlan, String> {
        Err("text entry is unavailable on the command backend".to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    use std::ffi::OsString;

    struct EnvRestore {
        key: &'static str,
        value: Option<OsString>,
    }

    impl EnvRestore {
        fn save(key: &'static str) -> Self {
            Self {
                key,
                value: env::var_os(key),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match &self.value {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn wayland_proxy_initialization_preserves_the_host_environment() {
        let host_runtime = tempfile::tempdir().expect("host runtime");
        let _restore_backend = EnvRestore::save("WAYLAND_MCP_BACKEND_SOCKET");
        let _restore_runtime = EnvRestore::save("XDG_RUNTIME_DIR");
        let _restore_wayland = EnvRestore::save("WAYLAND_DISPLAY");
        let _restore_display = EnvRestore::save("DISPLAY");
        let _restore_xauthority = EnvRestore::save("XAUTHORITY");
        let host_display = "host-session-socket";
        let xauthority = host_runtime.path().join("xauthority");
        unsafe {
            env::remove_var("WAYLAND_MCP_BACKEND_SOCKET");
            env::set_var("XDG_RUNTIME_DIR", host_runtime.path());
            env::set_var("WAYLAND_DISPLAY", host_display);
            env::set_var("DISPLAY", "inherited-x-display");
            env::set_var("XAUTHORITY", &xauthority);
        }
        ensure_default_gui_backend_initialized();
        assert_eq!(env::var_os("WAYLAND_DISPLAY"), Some(host_display.into()));
        assert_eq!(
            env::var_os("XDG_RUNTIME_DIR"),
            Some(host_runtime.path().into())
        );
        assert_eq!(env::var_os("DISPLAY"), Some("inherited-x-display".into()));
        assert_eq!(env::var_os("XAUTHORITY"), Some(xauthority.into_os_string()));
        assert_eq!(env::var_os("WAYLAND_MCP_BACKEND_SOCKET"), None);
        let GuiBackendHandle::Wayland(backend) = default_gui_backend() else {
            panic!("host Wayland environment was not selected");
        };
        if let Ok(proxy) = backend.launch_environment() {
            assert_ne!(proxy.xdg_runtime_dir, host_runtime.path().to_string_lossy());
            assert_ne!(proxy.wayland_display, host_display);
        }
    }

    #[test]
    #[serial]
    fn explicit_backend_configuration_does_not_require_a_display_name() {
        let _restore_backend = EnvRestore::save("WAYLAND_MCP_BACKEND_SOCKET");
        let _restore_wayland = EnvRestore::save("WAYLAND_DISPLAY");
        let directory = tempfile::tempdir().unwrap();
        unsafe {
            env::remove_var("WAYLAND_DISPLAY");
            env::set_var("WAYLAND_MCP_BACKEND_SOCKET", directory.path().join("host"));
        }
        assert_eq!(desktop_session(), Some(DesktopSession::Wayland));
    }
}
