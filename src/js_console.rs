use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::ArtifactStore;
use crate::gui_backend::{
    GuiBackendHandle, GuiCaptureNextFrameRequest, GuiKeyboardTextPlanRequest,
    GuiResizeWindowRequest, GuiScreenshotRequest, GuiWaylandKeyboardEvent,
    GuiWaylandKeyboardEventRequest, GuiWaylandPointerEvent, GuiWaylandPointerEventRequest,
};

const NODE_RUNTIME: &str = include_str!("wayland_console_runtime.mjs");
const MAX_EVAL_CODE_BYTES: usize = 1_048_576;
const DEFAULT_EVAL_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_EVAL_TIMEOUT_MS: u64 = 1_000;
const MAX_EVAL_TIMEOUT_MS: u64 = 900_000;

#[derive(Clone)]
pub(crate) struct JsConsole {
    sender: mpsc::Sender<ActorCommand>,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug)]
pub(crate) struct JsEvalOutput {
    pub(crate) value: Value,
    pub(crate) logs: Vec<String>,
    pub(crate) images: Vec<JsConsoleImage>,
}

#[derive(Debug)]
pub(crate) struct JsConsoleImage {
    pub(crate) color: Option<Value>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) path: String,
}

enum ActorCommand {
    Eval {
        id: u64,
        code: String,
        response: oneshot::Sender<Result<JsEvalOutput, String>>,
    },
}

struct PendingEval {
    response: oneshot::Sender<Result<JsEvalOutput, String>>,
    images: Vec<JsConsoleImage>,
}

impl JsConsole {
    pub(crate) fn new(backend: GuiBackendHandle, artifacts: Arc<ArtifactStore>) -> Self {
        Self::new_with_timeout(backend, artifacts, evaluation_timeout_from_env())
    }

    fn new_with_timeout(
        backend: GuiBackendHandle,
        artifacts: Arc<ArtifactStore>,
        evaluation_timeout: Duration,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(run_actor(receiver, backend, artifacts, evaluation_timeout));
        Self {
            sender,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub(crate) async fn eval(&self, code: String) -> Result<JsEvalOutput, String> {
        if code.len() > MAX_EVAL_CODE_BYTES {
            return Err(format!(
                "JavaScript evaluation is {} bytes; the limit is {MAX_EVAL_CODE_BYTES} bytes",
                code.len()
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response, receive) = oneshot::channel();
        self.sender
            .send(ActorCommand::Eval { id, code, response })
            .await
            .map_err(|_| "JavaScript console process is unavailable".to_string())?;
        receive
            .await
            .map_err(|_| "JavaScript console evaluation was interrupted".to_string())?
    }
}

fn evaluation_timeout_from_env() -> Duration {
    std::env::var("WAYLAND_MCP_EVAL_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|milliseconds| {
            Duration::from_millis(milliseconds.clamp(MIN_EVAL_TIMEOUT_MS, MAX_EVAL_TIMEOUT_MS))
        })
        .unwrap_or(DEFAULT_EVAL_TIMEOUT)
}

enum SessionOutcome {
    CommandsClosed,
    Restart,
}

async fn run_actor(
    mut commands: mpsc::Receiver<ActorCommand>,
    backend: GuiBackendHandle,
    artifacts: Arc<ArtifactStore>,
    evaluation_timeout: Duration,
) {
    loop {
        match run_node_session(&mut commands, &backend, &artifacts, evaluation_timeout).await {
            SessionOutcome::CommandsClosed => return,
            SessionOutcome::Restart => {
                artifacts.record(
                    "console_javascript_runtime_restarted",
                    json!({"reason":"process exit, protocol failure, or evaluation timeout"}),
                );
            }
        }
    }
}

async fn run_node_session(
    commands: &mut mpsc::Receiver<ActorCommand>,
    backend: &GuiBackendHandle,
    artifacts: &ArtifactStore,
    evaluation_timeout: Duration,
) -> SessionOutcome {
    let mut child = match Command::new("node")
        .args(["--input-type=module", "--eval", NODE_RUNTIME])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let Some(ActorCommand::Eval { response, .. }) = commands.recv().await else {
                return SessionOutcome::CommandsClosed;
            };
            let _ = response.send(Err(format!(
                "failed to start persistent Node.js console: {err}"
            )));
            return SessionOutcome::Restart;
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill().await;
        return SessionOutcome::Restart;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        return SessionOutcome::Restart;
    };
    let mut lines = BufReader::new(stdout).lines();
    let mut pending = HashMap::<u64, PendingEval>::new();
    let mut deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(ActorCommand::Eval { id, code, response }) = command else {
                    let _ = child.kill().await;
                    return SessionOutcome::CommandsClosed;
                };
                if !pending.is_empty() {
                    let _ = response.send(Err("the JavaScript console is already evaluating code".to_string()));
                    continue;
                }
                pending.insert(id, PendingEval { response, images: Vec::new() });
                deadline = Some(Box::pin(tokio::time::sleep(evaluation_timeout)));
                if let Err(err) = write_message(&mut stdin, &json!({"type":"eval", "id":id, "code":code})).await {
                    fail_pending(
                        &mut pending,
                        format!("{err}; the JavaScript runtime was restarted and its state was cleared"),
                    );
                    let _ = child.kill().await;
                    return SessionOutcome::Restart;
                }
            }
            _ = async {
                if let Some(deadline) = deadline.as_mut() {
                    deadline.as_mut().await;
                }
            }, if deadline.is_some() => {
                fail_pending(
                    &mut pending,
                    format!(
                        "JavaScript evaluation exceeded {} ms; the runtime was restarted and its state was cleared",
                        evaluation_timeout.as_millis()
                    ),
                );
                let _ = child.kill().await;
                return SessionOutcome::Restart;
            }
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        fail_pending(
                            &mut pending,
                            "JavaScript console process exited; the runtime was restarted and its state was cleared".to_string(),
                        );
                        let _ = child.kill().await;
                        return SessionOutcome::Restart;
                    }
                    Err(err) => {
                        fail_pending(
                            &mut pending,
                            format!(
                                "failed to read JavaScript console: {err}; the runtime was restarted and its state was cleared"
                            ),
                        );
                        let _ = child.kill().await;
                        return SessionOutcome::Restart;
                    }
                };
                let message: Value = match serde_json::from_str(&line) {
                    Ok(message) => message,
                    Err(err) => {
                        fail_pending(
                            &mut pending,
                            format!(
                                "invalid JavaScript console response: {err}; the runtime was restarted and its state was cleared"
                            ),
                        );
                        let _ = child.kill().await;
                        return SessionOutcome::Restart;
                    }
                };
                match message.get("type").and_then(Value::as_str) {
                    Some("native_call") => {
                        let call_id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let eval_id = message.get("evalId").and_then(Value::as_u64);
                        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                        let args = message.get("args").cloned().unwrap_or_else(|| json!({}));
                        let result = handle_native_call(method, args, backend, artifacts).await;
                        let reply = match result {
                            Ok((value, image)) => {
                                if let (Some(eval_id), Some(image)) = (eval_id, image)
                                    && let Some(evaluation) = pending.get_mut(&eval_id)
                                {
                                    evaluation.images.push(image);
                                }
                                json!({"type":"native_result", "id":call_id, "ok":true, "value":value})
                            }
                            Err(error) => json!({"type":"native_result", "id":call_id, "ok":false, "error":error}),
                        };
                        if let Err(err) = write_message(&mut stdin, &reply).await {
                            fail_pending(
                                &mut pending,
                                format!("{err}; the JavaScript runtime was restarted and its state was cleared"),
                            );
                            let _ = child.kill().await;
                            return SessionOutcome::Restart;
                        }
                    }
                    Some("eval_result") => {
                        let id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let Some(evaluation) = pending.remove(&id) else {
                            continue;
                        };
                        deadline = None;
                        let logs = message.get("logs").and_then(Value::as_array)
                            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                            .unwrap_or_default();
                        let result = if message.get("ok").and_then(Value::as_bool) == Some(true) {
                            Ok(JsEvalOutput {
                                value: message.get("value").cloned().unwrap_or(Value::Null),
                                logs,
                                images: evaluation.images,
                            })
                        } else {
                            Err(message.get("error").and_then(Value::as_str).unwrap_or("JavaScript evaluation failed").to_string())
                        };
                        let _ = evaluation.response.send(result);
                    }
                    Some("runtime_error") => {
                        fail_pending(
                            &mut pending,
                            message.get("error").and_then(Value::as_str)
                                .unwrap_or("JavaScript runtime error")
                                .to_string(),
                        );
                        let _ = child.kill().await;
                        return SessionOutcome::Restart;
                    }
                    _ => {}
                }
            }
        }
    }
}
async fn handle_native_call(
    method: &str,
    args: Value,
    backend: &GuiBackendHandle,
    artifacts: &ArtifactStore,
) -> Result<(Value, Option<JsConsoleImage>), String> {
    match method {
        "environment" => {
            let environment = match backend {
                GuiBackendHandle::Wayland(backend) => {
                    serde_json::to_value(backend.launch_environment()?)
                        .map_err(|err| err.to_string())?
                }
                GuiBackendHandle::Command(_) => json!({}),
                #[cfg(test)]
                GuiBackendHandle::Test(_) => json!({}),
            };
            Ok((environment, None))
        }
        "diagnostics" => {
            let diagnostics = match backend {
                GuiBackendHandle::Wayland(backend) => {
                    serde_json::to_value(backend.snapshot().await).map_err(|err| err.to_string())?
                }
                GuiBackendHandle::Command(_) => json!({
                    "running": false,
                    "last_runtime_error": "Wayland proxy backend is not active"
                }),
                #[cfg(test)]
                GuiBackendHandle::Test(_) => json!({
                    "running": true,
                    "last_runtime_error": null
                }),
            };
            Ok((diagnostics, None))
        }
        "windows" => Ok((
            serde_json::to_value(backend.list_windows().await?).map_err(|err| err.to_string())?,
            None,
        )),
        "resize_window" => {
            let window_id = args
                .get("windowId")
                .and_then(Value::as_str)
                .ok_or_else(|| "resizeWindow requires windowId".to_string())?
                .to_string();
            let width = args
                .get("width")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| "resizeWindow requires a positive integer width".to_string())?;
            let height = args
                .get("height")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| "resizeWindow requires a positive integer height".to_string())?;
            let detail = backend
                .resize_window(GuiResizeWindowRequest {
                    window_id: window_id.clone(),
                    width,
                    height,
                })
                .await?;
            artifacts.record(
                "console_window_resize_requested",
                json!({"window_id":window_id,"width":width,"height":height,"detail":detail}),
            );
            Ok((json!({"delivered":true,"detail":detail}), None))
        }
        "screenshot" => {
            let window_id = optional_string(&args, "windowId");
            let bytes = backend
                .screenshot(GuiScreenshotRequest {
                    window_id: window_id.clone(),
                })
                .await?;
            retained_image("console-screenshot", window_id.as_deref(), bytes, artifacts)
        }
        "capture_next_frame" => {
            let window_id = optional_string(&args, "windowId");
            let bytes = backend
                .capture_next_frame(GuiCaptureNextFrameRequest {
                    window_id: window_id.clone(),
                    after_commit_serial: args.get("afterCommitSerial").and_then(Value::as_u64),
                    timeout_ms: args.get("timeoutMs").and_then(Value::as_u64),
                })
                .await?;
            retained_image("console-next-frame", window_id.as_deref(), bytes, artifacts)
        }
        "pointer_event" => {
            let window_id = optional_string(&args, "windowId");
            let event: GuiWaylandPointerEvent = serde_json::from_value(
                args.get("event")
                    .cloned()
                    .ok_or_else(|| "pointerEvent requires event".to_string())?,
            )
            .map_err(|err| format!("invalid wl_pointer event: {err}"))?;
            let retained_event = serde_json::to_value(&event).map_err(|err| err.to_string())?;
            let result = backend
                .emit_wayland_pointer_event(GuiWaylandPointerEventRequest {
                    window_id: window_id.clone(),
                    event,
                })
                .await?;
            artifacts.record(
                "console_wayland_pointer_event_emitted",
                json!({"window_id":window_id, "event":retained_event, "result":result}),
            );
            Ok((json!({"delivered": true, "detail": result}), None))
        }
        "keyboard_event" => {
            let window_id = optional_string(&args, "windowId");
            let event: GuiWaylandKeyboardEvent = serde_json::from_value(
                args.get("event")
                    .cloned()
                    .ok_or_else(|| "keyboardEvent requires event".to_string())?,
            )
            .map_err(|err| format!("invalid wl_keyboard event: {err}"))?;
            let retained_event = serde_json::to_value(&event).map_err(|err| err.to_string())?;
            let result = backend
                .emit_wayland_keyboard_event(GuiWaylandKeyboardEventRequest {
                    window_id: window_id.clone(),
                    event,
                })
                .await?;
            artifacts.record(
                "console_wayland_keyboard_event_emitted",
                json!({"window_id":window_id, "event":retained_event, "result":result}),
            );
            Ok((json!({"delivered": true, "detail": result}), None))
        }
        "keyboard_text_plan" => {
            let window_id = optional_string(&args, "windowId");
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "typeText requires text".to_string())?
                .to_string();
            let plan = backend
                .keyboard_text_plan(GuiKeyboardTextPlanRequest { window_id, text })
                .await?;
            Ok((
                serde_json::to_value(plan).map_err(|err| err.to_string())?,
                None,
            ))
        }
        other => Err(format!("unknown JavaScript native call `{other}`")),
    }
}

fn retained_image(
    operation: &str,
    window_id: Option<&str>,
    bytes: Vec<u8>,
    artifacts: &ArtifactStore,
) -> Result<(Value, Option<JsConsoleImage>), String> {
    let path = artifacts.save_png(operation, window_id, &bytes)?;
    let dimensions = image::load_from_memory(&bytes)
        .map(|image| (image.width(), image.height()))
        .map_err(|err| format!("captured invalid PNG: {err}"))?;
    let path = path.display().to_string();
    let reader = png::Decoder::new(std::io::Cursor::new(&bytes))
        .read_info()
        .map_err(|err| err.to_string())?;
    let color = reader
        .info()
        .uncompressed_latin1_text
        .iter()
        .find(|chunk| chunk.keyword == "wayland-mcp-color")
        .map(|chunk| serde_json::from_str::<Value>(&chunk.text))
        .transpose()
        .map_err(|err| err.to_string())?;
    Ok((
        json!({"path":path, "width":dimensions.0, "height":dimensions.1, "color": color}),
        Some(JsConsoleImage { bytes, path, color }),
    ))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

async fn write_message(
    stdin: &mut tokio::process::ChildStdin,
    message: &Value,
) -> Result<(), String> {
    let mut encoded = serde_json::to_vec(message).map_err(|err| err.to_string())?;
    encoded.push(b'\n');
    stdin
        .write_all(&encoded)
        .await
        .map_err(|err| format!("failed to write JavaScript console: {err}"))?;
    stdin
        .flush()
        .await
        .map_err(|err| format!("failed to flush JavaScript console: {err}"))
}

fn fail_pending(pending: &mut HashMap<u64, PendingEval>, error: String) {
    for (_, evaluation) in pending.drain() {
        let _ = evaluation.response.send(Err(error.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_backend::CommandGuiBackend;
    use crate::gui_backend::GuiBackend;
    use crate::gui_backend::GuiKeyboardTextPlan;
    use crate::gui_backend::GuiKeyboardTextStroke;
    use crate::gui_backend::GuiWindowInfo;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct RecordingBackend {
        pointer_events: StdMutex<Vec<GuiWaylandPointerEventRequest>>,
        keyboard_events: StdMutex<Vec<GuiWaylandKeyboardEventRequest>>,
        fail_pointer_event_once_at: StdMutex<Option<usize>>,
        fail_keyboard_event_once_at: StdMutex<Option<usize>>,
        pointer_event_attempts: AtomicUsize,
        keyboard_event_attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl GuiBackend for RecordingBackend {
        async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
            Ok(Vec::new())
        }

        async fn screenshot(&self, _request: GuiScreenshotRequest) -> Result<Vec<u8>, String> {
            Err("screenshot is not used by this test backend".to_string())
        }

        async fn capture_next_frame(
            &self,
            _request: GuiCaptureNextFrameRequest,
        ) -> Result<Vec<u8>, String> {
            Err("capture is not used by this test backend".to_string())
        }

        async fn emit_wayland_pointer_event(
            &self,
            request: GuiWaylandPointerEventRequest,
        ) -> Result<String, String> {
            let attempt = self.pointer_event_attempts.fetch_add(1, Ordering::Relaxed);
            let mut fail_once = self
                .fail_pointer_event_once_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *fail_once == Some(attempt) {
                *fail_once = None;
                return Err("injected pointer event failure".to_string());
            }
            drop(fail_once);
            self.pointer_events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            Ok("recorded pointer event".to_string())
        }

        async fn emit_wayland_keyboard_event(
            &self,
            request: GuiWaylandKeyboardEventRequest,
        ) -> Result<String, String> {
            let attempt = self.keyboard_event_attempts.fetch_add(1, Ordering::Relaxed);
            let mut fail_once = self
                .fail_keyboard_event_once_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *fail_once == Some(attempt) {
                *fail_once = None;
                return Err("injected keyboard event failure".to_string());
            }
            drop(fail_once);
            self.keyboard_events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            Ok("recorded keyboard event".to_string())
        }

        async fn keyboard_text_plan(
            &self,
            request: GuiKeyboardTextPlanRequest,
        ) -> Result<GuiKeyboardTextPlan, String> {
            let strokes = match request.text.as_str() {
                "Az 0!?" => vec![
                    GuiKeyboardTextStroke {
                        key: 30,
                        modifiers: 1,
                    },
                    GuiKeyboardTextStroke {
                        key: 21,
                        modifiers: 0,
                    },
                    GuiKeyboardTextStroke {
                        key: 57,
                        modifiers: 0,
                    },
                    GuiKeyboardTextStroke {
                        key: 11,
                        modifiers: 0,
                    },
                    GuiKeyboardTextStroke {
                        key: 2,
                        modifiers: 1,
                    },
                    GuiKeyboardTextStroke {
                        key: 12,
                        modifiers: 1,
                    },
                ],
                "z" => vec![GuiKeyboardTextStroke {
                    key: 21,
                    modifiers: 0,
                }],
                "w" => vec![GuiKeyboardTextStroke {
                    key: 17,
                    modifiers: 0,
                }],
                text if text.chars().all(|c| "u0123456789abcdef".contains(c)) => text
                    .chars()
                    .map(|c| GuiKeyboardTextStroke {
                        key: match c {
                            'u' => 22,
                            '0' => 11,
                            '1'..='9' => 2 + c as u32 - '1' as u32,
                            'a' => 30,
                            'b' => 48,
                            'c' => 46,
                            'd' => 32,
                            'e' => 18,
                            'f' => 33,
                            _ => unreachable!(),
                        },
                        modifiers: 0,
                    })
                    .collect(),
                unexpected => return Err(format!("unexpected text fixture: {unexpected:?}")),
            };
            Ok(GuiKeyboardTextPlan {
                layout_group: 0,
                restore_mods_depressed: 0,
                restore_mods_latched: 0,
                restore_mods_locked: 0,
                shift_modifier: Some(1),
                control_modifier: Some(4),
                alt_modifier: Some(8),
                logo_modifier: Some(64),
                // These are inverse mappings from a German XKB keymap,
                // notably Z on evdev 21 and ? on evdev 12.
                strokes,
            })
        }
    }

    #[tokio::test]
    async fn javascript_definitions_persist_across_console_calls() {
        let backend = GuiBackendHandle::Command(Arc::new(CommandGuiBackend));
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        let help = console
            .eval("return wayland.help".to_string())
            .await
            .unwrap();
        assert!(help.value.as_str().unwrap().contains("wayland.click"));
        assert!(help.value.as_str().unwrap().contains("pressShortcut"));
        assert!(help.value.as_str().unwrap().contains("typeText"));
        assert!(help.value.as_str().unwrap().contains("actAndCapture"));
        assert!(help.value.as_str().unwrap().contains("wayland.keyNames"));

        console
            .eval("globalThis.twice = async function(value) { await wayland.sleep(1); return value * 2; }; return 'defined';".to_string())
            .await
            .unwrap();
        let called = console
            .eval("return await twice(21);".to_string())
            .await
            .unwrap();
        assert_eq!(called.value, json!(42));
    }

    #[test]
    fn captured_color_notice_survives_png_and_retention() {
        let color =
            json!({"tone_mapped":true, "notice":"SDR sRGB preview, not the original HDR window"});
        let png =
            crate::gui_backend_wayland::encode_rgba_png(1, 1, &[128, 128, 128, 255], Some(&color))
                .unwrap();
        let reader = png::Decoder::new(std::io::Cursor::new(&png))
            .read_info()
            .unwrap();
        assert_eq!(
            reader.info().srgb,
            Some(png::SrgbRenderingIntent::Perceptual)
        );
        let (value, image) =
            retained_image("color-test", None, png, &ArtifactStore::new()).unwrap();
        assert_eq!(value["color"], color);
        assert_eq!(image.unwrap().color, Some(color));
    }

    #[tokio::test]
    async fn type_text_emits_the_keymap_derived_keyboard_sequence() {
        let recording = Arc::new(RecordingBackend::default());
        let backend = GuiBackendHandle::Test(recording.clone());
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        let output = console
            .eval("return await wayland.typeText({windowId:'fixture', text:'Az 0!?'});".to_string())
            .await
            .expect("representative keymap-derived text should be emitted");

        assert_eq!(output.value["delivered"], json!(true));
        assert_eq!(output.value["keymapDriven"], json!(true));
        let recorded = recording
            .keyboard_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            recorded
                .iter()
                .all(|request| request.window_id.as_deref() == Some("fixture"))
        );
        let actual = recorded
            .into_iter()
            .map(|request| request.event)
            .collect::<Vec<_>>();
        let key = |key, state| GuiWaylandKeyboardEvent::Key {
            key,
            state,
            serial: None,
            time: None,
        };
        let modifiers = |mods_depressed| GuiWaylandKeyboardEvent::Modifiers {
            mods_depressed,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
            serial: None,
        };
        assert_eq!(
            actual,
            vec![
                GuiWaylandKeyboardEvent::Enter {
                    serial: None,
                    keys: Vec::new(),
                },
                modifiers(1),
                key(30, 1),
                key(30, 0),
                modifiers(0),
                key(21, 1),
                key(21, 0),
                key(57, 1),
                key(57, 0),
                key(11, 1),
                key(11, 0),
                modifiers(1),
                key(2, 1),
                key(2, 0),
                key(12, 1),
                key(12, 0),
                modifiers(0),
            ]
        );
    }

    #[tokio::test]
    async fn unicode_hex_emits_a_full_supplementary_codepoint() {
        let recording = Arc::new(RecordingBackend::default());
        let console = JsConsole::new(
            GuiBackendHandle::Test(recording.clone()),
            Arc::new(ArtifactStore::new()),
        );
        let output = console.eval("return await wayland.typeText({windowId:'fixture',text:'🌘',inputMethod:'unicode-hex'});".to_string()).await.unwrap();
        assert_eq!(output.value["delivered"], true);
        let events = recording.keyboard_events.lock().unwrap();
        let down = events
            .iter()
            .filter_map(|request| match request.event {
                GuiWaylandKeyboardEvent::Key { key, state: 1, .. } => Some(key),
                _ => None,
            })
            .collect::<Vec<_>>();
        // Ctrl+Shift+U, 1f318, Enter; the surrogate pair is one codepoint.
        assert_eq!(down, [22, 2, 33, 4, 2, 9, 28]);
        assert!(events.iter().any(|request| matches!(
            request.event,
            GuiWaylandKeyboardEvent::Modifiers {
                mods_depressed: 5,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn raw_keyboard_event_invalidates_helper_focus() {
        let recording = Arc::new(RecordingBackend::default());
        let console = JsConsole::new(
            GuiBackendHandle::Test(recording.clone()),
            Arc::new(ArtifactStore::new()),
        );
        console.eval("await wayland.pressKey({windowId:'fixture',key:'ENTER'}); await wayland.keyboardEvent({windowId:'fixture',event:{type:'leave'}}); return await wayland.pressKey({windowId:'fixture',key:'ENTER'});".to_string()).await.unwrap();
        let events = recording.keyboard_events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|request| matches!(request.event, GuiWaylandKeyboardEvent::Enter { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn shortcut_character_and_modifier_are_keymap_derived() {
        let recording = Arc::new(RecordingBackend::default());
        let backend = GuiBackendHandle::Test(recording.clone());
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        let output = console
            .eval(
                "return await wayland.pressShortcut({windowId:'fixture', keys:['CTRL','Z'], holdMs:0});"
                    .to_string(),
            )
            .await
            .expect("keymap-derived shortcut");

        assert_eq!(output.value["keyCode"], json!(21));
        assert_eq!(output.value["modifierMask"], json!(4));
        let actual = recording
            .keyboard_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|request| request.event.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                GuiWaylandKeyboardEvent::Enter {
                    serial: None,
                    keys: Vec::new(),
                },
                GuiWaylandKeyboardEvent::Modifiers {
                    mods_depressed: 4,
                    mods_latched: 0,
                    mods_locked: 0,
                    group: 0,
                    serial: None,
                },
                GuiWaylandKeyboardEvent::Key {
                    key: 21,
                    state: 1,
                    serial: None,
                    time: None,
                },
                GuiWaylandKeyboardEvent::Key {
                    key: 21,
                    state: 0,
                    serial: None,
                    time: None,
                },
                GuiWaylandKeyboardEvent::Modifiers {
                    mods_depressed: 0,
                    mods_latched: 0,
                    mods_locked: 0,
                    group: 0,
                    serial: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn shortcut_releases_key_and_restores_modifiers_after_event_failure() {
        let recording = Arc::new(RecordingBackend::default());
        *recording
            .fail_keyboard_event_once_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(3);
        let backend = GuiBackendHandle::Test(recording.clone());
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        let error = console
            .eval(
                "return await wayland.pressShortcut({windowId:'fixture', keys:['CTRL','Z'], holdMs:0});"
                    .to_string(),
            )
            .await
            .expect_err("the injected key-up failure should reject the shortcut");
        assert!(error.contains("injected keyboard event failure"));

        let actual = recording
            .keyboard_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|request| request.event.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            actual.as_slice(),
            [
                GuiWaylandKeyboardEvent::Enter { .. },
                GuiWaylandKeyboardEvent::Modifiers {
                    mods_depressed: 4,
                    ..
                },
                GuiWaylandKeyboardEvent::Key {
                    key: 21,
                    state: 1,
                    ..
                },
                GuiWaylandKeyboardEvent::Key {
                    key: 21,
                    state: 0,
                    ..
                },
                GuiWaylandKeyboardEvent::Modifiers {
                    mods_depressed: 0,
                    ..
                },
            ]
        ));
    }

    #[tokio::test]
    async fn click_releases_button_after_event_failure() {
        let recording = Arc::new(RecordingBackend::default());
        *recording
            .fail_pointer_event_once_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(5);
        let backend = GuiBackendHandle::Test(recording.clone());
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        let error = console
            .eval("return await wayland.click({windowId:'fixture', x:10, y:20});".to_string())
            .await
            .expect_err("the injected button-up failure should reject the click");
        assert!(error.contains("injected pointer event failure"));

        let actual = recording
            .pointer_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|request| request.event.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            actual.as_slice(),
            [
                GuiWaylandPointerEvent::Enter { .. },
                GuiWaylandPointerEvent::Motion { .. },
                GuiWaylandPointerEvent::Frame,
                GuiWaylandPointerEvent::Button { state: 1, .. },
                GuiWaylandPointerEvent::Frame,
                GuiWaylandPointerEvent::Button { state: 0, .. },
                GuiWaylandPointerEvent::Frame,
            ]
        ));
    }

    #[tokio::test]
    async fn press_key_accepts_uppercase_single_character_names() {
        let recording = Arc::new(RecordingBackend::default());
        let backend = GuiBackendHandle::Test(recording.clone());
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        let output = console
            .eval(
                "return await wayland.pressKey({windowId:'fixture', key:'W', holdMs:0});"
                    .to_string(),
            )
            .await
            .expect("uppercase character key");

        assert_eq!(output.value["keyCode"], json!(17));
        assert_eq!(output.value["keymapDriven"], json!(true));
        let actual = recording
            .keyboard_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|request| request.event.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            actual.as_slice(),
            [
                GuiWaylandKeyboardEvent::Enter { .. },
                GuiWaylandKeyboardEvent::Modifiers {
                    mods_depressed: 0,
                    ..
                },
                GuiWaylandKeyboardEvent::Key {
                    key: 17,
                    state: 1,
                    ..
                },
                GuiWaylandKeyboardEvent::Key {
                    key: 17,
                    state: 0,
                    ..
                },
                GuiWaylandKeyboardEvent::Modifiers {
                    mods_depressed: 0,
                    ..
                },
            ]
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_javascript_before_sending_it_to_node() {
        let backend = GuiBackendHandle::Command(Arc::new(CommandGuiBackend));
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);
        let error = console
            .eval("x".repeat(MAX_EVAL_CODE_BYTES + 1))
            .await
            .expect_err("oversized source must be rejected");
        assert!(error.contains("the limit is 1048576 bytes"));
    }

    #[tokio::test]
    async fn interrupts_synchronous_infinite_loops() {
        let backend = GuiBackendHandle::Command(Arc::new(CommandGuiBackend));
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);
        let error = console
            .eval("while (true) {}".to_string())
            .await
            .expect_err("synchronous loop must time out");
        assert!(error.contains("Script execution timed out"));
    }

    #[tokio::test]
    async fn background_timer_logs_are_not_attributed_to_a_later_evaluation() {
        let backend = GuiBackendHandle::Command(Arc::new(CommandGuiBackend));
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);
        console
            .eval(
                "setTimeout(() => console.log('old evaluation'), 5); return 'scheduled';"
                    .to_string(),
            )
            .await
            .unwrap();
        let output = console
            .eval(
                "await wayland.sleep(20); console.log('current evaluation'); return 'done';"
                    .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(output.logs, vec!["current evaluation"]);
    }

    #[tokio::test]
    async fn restarts_after_an_asynchronous_evaluation_timeout() {
        let backend = GuiBackendHandle::Command(Arc::new(CommandGuiBackend));
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new_with_timeout(backend, artifacts, Duration::from_millis(500));
        let error = console
            .eval("await new Promise(() => {});".to_string())
            .await
            .expect_err("never-settling promise must time out");
        assert!(error.contains("runtime was restarted"));

        let recovered = console
            .eval("return 6 * 7;".to_string())
            .await
            .expect("console should accept evaluations after a restart");
        assert_eq!(recovered.value, json!(42));
    }
}
