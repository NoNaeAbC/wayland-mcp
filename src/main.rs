mod gui_backend;
mod gui_backend_wayland;
mod gui_vulkan_dmabuf;
mod gui_wayland_generated;
mod gui_xkb;
mod js_console;
mod wayland_protocol_registry;

use std::borrow::Cow;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::GenericImageView;
use js_console::JsConsole;
use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
struct WaylandMcp {
    tools: Arc<Vec<Tool>>,
    artifacts: Arc<ArtifactStore>,
    js_console: JsConsole,
}

pub(crate) struct ArtifactStore {
    directory: PathBuf,
    secure_directory: bool,
    next_id: AtomicU64,
    event_log_lock: Mutex<()>,
}

static NEXT_ARTIFACT_STORE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize)]
struct ConsoleArgs {
    code: String,
}

impl WaylandMcp {
    pub(crate) fn new() -> Self {
        gui_backend::ensure_default_gui_backend_initialized();
        let tools = Arc::new(tool_inventory());
        let artifacts = Arc::new(ArtifactStore::new());
        artifacts.record(
            "server_start",
            json!({
                "pid": std::process::id(),
                "tools": tools.iter().map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })).collect::<Vec<_>>()
            }),
        );
        let backend = gui_backend::default_gui_backend();
        let js_console = JsConsole::new(backend.clone(), Arc::clone(&artifacts));
        Self {
            tools,
            artifacts,
            js_console,
        }
    }

    async fn call_console(&self, arguments: serde_json::Value) -> Result<CallToolResult, McpError> {
        let args: ConsoleArgs = parse_args(arguments)?;
        match self.js_console.eval(args.code).await {
            Ok(output) => {
                self.artifacts.record(
                    "console_javascript_evaluated",
                    json!({
                        "value": &output.value,
                        "logs": &output.logs,
                        "image_count": output.images.len(),
                    }),
                );
                let mut image_content = Vec::new();
                let mut image_metadata = Vec::new();
                for image in output.images {
                    let (returned, metadata) = match bounded_png_preview(&image.bytes) {
                        Some((bytes, full_width, full_height, preview_width, preview_height)) => (
                            bytes,
                            json!({
                                "path": image.path,
                                "full": {"width":full_width, "height":full_height},
                                "embedded_preview": {"width":preview_width, "height":preview_height},
                                "preview_to_full_scale": {
                                    "x": full_width as f64 / preview_width as f64,
                                    "y": full_height as f64 / preview_height as f64,
                                },
                                "coordinate_space": "All Wayland input x/y values use full screenshot pixels. Multiply preview coordinates by preview_to_full_scale.",
                            }),
                        ),
                        None => {
                            let dimensions = image::load_from_memory(&image.bytes)
                                .map(|image| (image.width(), image.height()))
                                .unwrap_or((0, 0));
                            (
                                image.bytes.clone(),
                                json!({
                                    "path": image.path,
                                    "full": {"width":dimensions.0, "height":dimensions.1},
                                    "embedded_preview": {"width":dimensions.0, "height":dimensions.1},
                                    "preview_to_full_scale": {"x":1.0, "y":1.0},
                                    "coordinate_space": "All Wayland input x/y values use full screenshot pixels.",
                                }),
                            )
                        }
                    };
                    image_metadata.push(metadata);
                    image_content.push(Content::image(
                        BASE64_STANDARD.encode(returned),
                        "image/png",
                    ));
                }
                let structured = json!({
                    "value": output.value,
                    "logs": output.logs,
                    "images": image_metadata,
                });
                let mut content = vec![Content::text(
                    serde_json::to_string_pretty(&structured)
                        .map_err(|err| McpError::internal_error(err.to_string(), None))?,
                )];
                content.extend(image_content);
                let mut result = CallToolResult::success(content);
                result.structured_content = Some(structured);
                Ok(result)
            }
            Err(err) => Ok(tool_error(err)),
        }
    }
}
impl ArtifactStore {
    pub(crate) fn new() -> Self {
        let configured_directory = std::env::var_os("WAYLAND_MCP_ARTIFACT_DIR");
        let secure_directory = configured_directory.is_none();
        let directory = configured_directory
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let epoch_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or(0);
                let instance = NEXT_ARTIFACT_STORE_ID.fetch_add(1, Ordering::Relaxed);
                std::env::temp_dir().join(format!(
                    "wayland-mcp-artifacts-{}-{epoch_ms}-{instance}",
                    std::process::id(),
                ))
            });
        let store = Self {
            directory,
            secure_directory,
            next_id: AtomicU64::new(1),
            event_log_lock: Mutex::new(()),
        };
        if let Err(err) = store.ensure_directory() {
            eprintln!(
                "wayland-mcp: failed to create artifact directory {}: {err}",
                store.directory.display()
            );
        }
        store
    }

    fn ensure_directory(&self) -> std::io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        if self.secure_directory {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.directory)?;
        #[cfg(unix)]
        if self.secure_directory {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub(crate) fn record(&self, event: &str, data: serde_json::Value) {
        let entry = json!({
            "timestamp_unix_ms": unix_time_ms(),
            "event": event,
            "data": data,
        });
        let path = self.directory.join("events.jsonl");
        let _guard = match self.event_log_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = self.ensure_directory() {
            eprintln!(
                "wayland-mcp: failed to prepare artifact directory {}: {err}",
                self.directory.display()
            );
            return;
        }
        let mut encoded = match serde_json::to_vec(&entry) {
            Ok(encoded) => encoded,
            Err(err) => {
                eprintln!("wayland-mcp: failed to encode event log entry: {err}");
                return;
            }
        };
        encoded.push(b'\n');
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let result = options
            .open(&path)
            .and_then(|mut file| file.write_all(&encoded));
        if let Err(err) = result {
            eprintln!("wayland-mcp: failed to append {}: {err}", path.display());
        }
    }

    pub(crate) fn save_png(
        &self,
        operation: &str,
        window_id: Option<&str>,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let window = sanitize_filename(window_id.unwrap_or("auto"));
        let path = self
            .directory
            .join(format!("{id:06}-{operation}-{window}.png"));
        self.ensure_directory().map_err(|err| {
            format!(
                "failed to prepare artifact directory {}: {err}",
                self.directory.display()
            )
        })?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options
            .open(&path)
            .and_then(|mut file| file.write_all(bytes))
            .map_err(|err| format!("failed to retain screenshot {}: {err}", path.display()))?;
        self.record(
            "frame_saved",
            json!({
                "operation": operation,
                "window_id": window_id,
                "path": path,
                "byte_length": bytes.len(),
            }),
        );
        Ok(path)
    }
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

impl ServerHandler for WaylandMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "This server intentionally advertises one persistent graphics tool: gui_console. Evaluate JavaScript with the required code field; variables and globalThis function definitions survive calls. Begin with `return wayland.help`, which documents supported click/drag/scroll/key/shortcut/text/wait helpers and the raw wl_pointer/wl_keyboard event shapes. Use wayland.environment/diagnostics/windows/screenshot/captureNextFrame for observation. Complete PNGs and JSONL traces are retained at returned paths. A delivered event is not proof the application accepted it. Launch the GUI as a live foreground process session when the caller reaps detached `&`/nohup jobs. This MCP cannot configure the caller sandbox; native-GPU applications may require caller-granted access to the returned Unix socket and /dev/dri/renderD*. Request the caller's supported permission if launch diagnostics show those resources are blocked; do not silently switch to software rendering. This MCP is for graphics/input diagnostics, not DOM automation."
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.artifacts.record(
            "tools_listed",
            json!({"tool_names": self.tools.iter().map(|tool| tool.name.as_ref()).collect::<Vec<_>>() }),
        );
        Ok(ListToolsResult {
            tools: (*self.tools).clone(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
        self.artifacts.record(
            "tool_called",
            json!({"name": request.name.as_ref(), "arguments": &arguments}),
        );
        if request.name.as_ref() == "gui_console" {
            return self.call_console(arguments).await;
        }
        Err(McpError::invalid_params(
            format!("unknown tool: {}", request.name),
            None,
        ))
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Result<T, McpError> {
    serde_json::from_value(value).map_err(|err| McpError::invalid_params(err.to_string(), None))
}

fn bounded_png_preview(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    const MAX_PREVIEW_WIDTH: u32 = 1600;
    const MAX_PREVIEW_HEIGHT: u32 = 1000;

    let image = image::load_from_memory(bytes).ok()?;
    let (full_width, full_height) = image.dimensions();
    if full_width <= MAX_PREVIEW_WIDTH && full_height <= MAX_PREVIEW_HEIGHT {
        return None;
    }
    let preview = image.thumbnail(MAX_PREVIEW_WIDTH, MAX_PREVIEW_HEIGHT);
    let (preview_width, preview_height) = preview.dimensions();
    let mut encoded = std::io::Cursor::new(Vec::new());
    preview
        .write_to(&mut encoded, image::ImageFormat::Png)
        .ok()?;
    Some((
        encoded.into_inner(),
        full_width,
        full_height,
        preview_width,
        preview_height,
    ))
}

fn tool_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message)])
}

fn tool_inventory() -> Vec<Tool> {
    vec![tool(
        "gui_console",
        "Persistent JavaScript Wayland graphics console. Pass code; state and globalThis function definitions survive calls. First evaluate `return wayland.help` for the exact API. Supported helpers cover click, drag, scroll, named keys, keymap-aware shortcuts and text, window discovery, and action/commit waits; raw pointerEvent and keyboardEvent calls remain available. Coordinates come from returned screenshots and are never silently clamped. Native GPU apps need caller-granted access to the returned Unix socket and /dev/dri/renderD*; request caller-supported permission if blocked, never silently substitute software rendering.",
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript evaluated in the persistent console. Use `return wayland.help` first; define persistent helpers with globalThis.name = async function (...) { ... }."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    )]
}

fn tool(name: &'static str, description: &'static str, schema: serde_json::Value) -> Tool {
    let schema: JsonObject = serde_json::from_value(schema).expect("static tool schema is valid");
    Tool::new(
        Cow::Borrowed(name),
        Cow::Borrowed(description),
        Arc::new(schema),
    )
}

fn stdio() -> (tokio::io::Stdin, tokio::io::Stdout) {
    (tokio::io::stdin(), tokio::io::stdout())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("WAYLAND_MCP_TRACE").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter("wayland_mcp_proxy=trace")
            .with_writer(std::io::stderr)
            .init();
    }
    let running = WaylandMcp::new().serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_exactly_one_persistent_console() {
        let tools = tool_inventory();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), "gui_console");
        assert_eq!(
            tools[0]
                .input_schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .cloned(),
            Some(vec![json!("code")])
        );
        assert!(
            tools[0]
                .input_schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|properties| !properties.contains_key("action"))
        );
    }

    #[test]
    fn artifact_store_recreates_missing_directory_for_events_and_pngs() {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let directory = temporary.path().join("artifacts");
        let store = ArtifactStore {
            directory: directory.clone(),
            secure_directory: true,
            next_id: AtomicU64::new(1),
            event_log_lock: Mutex::new(()),
        };

        store.ensure_directory().expect("initialize artifact store");
        std::fs::remove_dir_all(&directory).expect("remove artifact directory");
        store.record("directory_recreated", json!({"write": "event"}));
        let events = std::fs::read_to_string(directory.join("events.jsonl"))
            .expect("event log should be recreated");
        assert!(events.contains("directory_recreated"));

        std::fs::remove_dir_all(&directory).expect("remove artifact directory again");
        let png = store
            .save_png("test", Some("window/1"), b"png bytes")
            .expect("PNG write should recreate the artifact directory");
        assert_eq!(
            std::fs::read(&png).expect("read retained PNG"),
            b"png bytes"
        );
        assert!(directory.join("events.jsonl").is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&directory)
                .expect("artifact directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
