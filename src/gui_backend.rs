#![cfg_attr(test, allow(dead_code))]

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
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
    pub(crate) event: GuiWaylandPointerEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiWaylandKeyboardEventRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) event: GuiWaylandKeyboardEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuiKeyboardTextPlanRequest {
    pub(crate) window_id: Option<String>,
    pub(crate) text: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GuiWindowInfo {
    pub(crate) window_id: String,
    pub(crate) title: Option<String>,
    pub(crate) app_id: Option<String>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) mapped: bool,
    pub(crate) focused: bool,
    pub(crate) commit_serial: u64,
    pub(crate) on_output: bool,
    pub(crate) output_count: usize,
    pub(crate) buffer_kind: Option<String>,
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
    if cfg!(unix) && std::env::var_os("WAYLAND_DISPLAY").is_some() {
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
    #[cfg(unix)]
    {
        const BACKEND_SOCKET_ENV: &str = "WAYLAND_MCP_BACKEND_SOCKET";

        if std::env::var_os(BACKEND_SOCKET_ENV).is_none()
            && let (Some(runtime_dir), Some(display)) = (
                std::env::var_os("XDG_RUNTIME_DIR"),
                std::env::var_os("WAYLAND_DISPLAY"),
            )
        {
            let backend_socket = PathBuf::from(runtime_dir).join(display);
            unsafe {
                std::env::set_var(BACKEND_SOCKET_ENV, backend_socket);
            }
        }

        let backend = default_gui_backend();
        if wayland_backend_socket_exists(BACKEND_SOCKET_ENV)
            && let GuiBackendHandle::Wayland(backend) = &backend
            && backend.transport_available()
        {
            unsafe {
                std::env::remove_var("DISPLAY");
                std::env::remove_var("XAUTHORITY");
            }
            if let Ok(environment) = backend.sandbox_env() {
                for (key, value) in environment {
                    unsafe {
                        std::env::set_var(key, value);
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
fn wayland_backend_socket_exists(backend_socket_env: &str) -> bool {
    let Some(backend_socket) = std::env::var_os(backend_socket_env) else {
        return false;
    };
    let path = PathBuf::from(backend_socket);
    if path.is_absolute() {
        return path.exists();
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|runtime_dir| runtime_dir.join(path).exists())
        .unwrap_or(false)
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
    use std::fs::File;

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
    async fn wayland_proxy_initialization_removes_x11_display_env() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let _restore_backend = EnvRestore::save("WAYLAND_MCP_BACKEND_SOCKET");
        let _restore_runtime = EnvRestore::save("XDG_RUNTIME_DIR");
        let _restore_wayland = EnvRestore::save("WAYLAND_DISPLAY");
        let _restore_display = EnvRestore::save("DISPLAY");
        let _restore_xauthority = EnvRestore::save("XAUTHORITY");

        let host_display = "wayland-host-0";
        let host_socket = temp_dir.path().join(host_display);
        let xauthority = temp_dir.path().join("xauthority");
        File::create(&host_socket).expect("host socket placeholder");

        unsafe {
            env::remove_var("WAYLAND_MCP_BACKEND_SOCKET");
            env::set_var("XDG_RUNTIME_DIR", temp_dir.path());
            env::set_var("WAYLAND_DISPLAY", host_display);
            env::set_var("DISPLAY", ":1");
            env::set_var("XAUTHORITY", &xauthority);
        }

        ensure_default_gui_backend_initialized();

        let startup_error = match default_gui_backend() {
            GuiBackendHandle::Wayland(backend) => backend.transport_startup_error(),
            GuiBackendHandle::Command(_) => Some("command backend selected".to_string()),
            GuiBackendHandle::Test(_) => Some("test backend selected".to_string()),
        };
        if startup_error.is_some() {
            assert_eq!(env::var_os("DISPLAY"), Some(":1".into()));
            assert_eq!(env::var_os("XAUTHORITY"), Some(xauthority.into_os_string()));
            assert_eq!(env::var_os("XDG_RUNTIME_DIR"), Some(temp_dir.path().into()));
            return;
        }
        assert_eq!(env::var_os("DISPLAY"), None);
        assert_eq!(env::var_os("XAUTHORITY"), None);
        let proxy_runtime_dir = env::var_os("XDG_RUNTIME_DIR").expect("proxy runtime dir");
        assert!(
            !PathBuf::from(proxy_runtime_dir).starts_with(temp_dir.path()),
            "proxy runtime dir must not live under host compositor runtime dir {}; got {}; startup error: {:?}",
            temp_dir.path().display(),
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").unwrap()).display(),
            startup_error,
        );
    }
}
