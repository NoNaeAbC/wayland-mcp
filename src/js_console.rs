use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

#[derive(Debug, Clone)]
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

const MAX_WIRE_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_NATIVE_CALLS: usize = 64;

struct NativeJob {
    call_id: u64,
    eval_id: u64,
    subscription_id: Option<u64>,
    method: String,
    args: Value,
    cancellation: tokio_util::sync::CancellationToken,
}
struct NativeCompletion {
    call_id: u64,
    eval_id: u64,
    subscription_id: Option<u64>,
    result: Result<(Value, Option<JsConsoleImage>), String>,
}

async fn run_node_session(
    commands: &mut mpsc::Receiver<ActorCommand>,
    backend: &GuiBackendHandle,
    artifacts: &Arc<ArtifactStore>,
    evaluation_timeout: Duration,
) -> SessionOutcome {
    let child = crate::console_runtime::command(NODE_RUNTIME)
        .and_then(|mut command| command.spawn().map_err(|error| error.to_string()));
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let Some(ActorCommand::Eval { response, .. }) = commands.recv().await else {
                return SessionOutcome::CommandsClosed;
            };
            let _ = response.send(Err(format!("isolated console launch failed: {error}")));
            return SessionOutcome::Restart;
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_text = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let stderr_buffer = stderr_text.clone();
    let stderr_task = tokio::spawn(async move {
        let mut buffer = [0; 1024];
        while let Ok(count) = stderr.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            let mut saved = stderr_buffer.lock().unwrap();
            let retain = count.min(8192usize.saturating_sub(saved.len()));
            saved.extend_from_slice(&buffer[..retain]);
        }
    });
    let (outgoing, mut output) = mpsc::channel::<Value>(64);
    let mut writer = tokio::spawn(async move {
        while let Some(message) = output.recv().await {
            write_message(&mut stdin, &message).await?;
        }
        Ok::<(), String>(())
    });
    let (incoming, mut input) = mpsc::channel::<Result<Value, String>>(64);
    let reader = tokio::spawn(async move {
        let mut stdout = BufReader::new(stdout);
        loop {
            let message = read_bounded_json(&mut stdout).await;
            let stop = message.is_err();
            if incoming.send(message).await.is_err() || stop {
                break;
            }
        }
    });
    let (job_sender, mut jobs) = mpsc::channel::<NativeJob>(MAX_NATIVE_CALLS);
    let (completion_sender, mut completions) = mpsc::channel::<NativeCompletion>(MAX_NATIVE_CALLS);
    let worker_backend = backend.clone();
    let worker_artifacts = artifacts.clone();
    let completion_queue = completion_sender.clone();
    let worker = tokio::spawn(async move {
        while let Some(job) = jobs.recv().await {
            if job.cancellation.is_cancelled() {
                continue;
            }
            let result = tokio::select! {
                _ = job.cancellation.cancelled() => continue,
                result = handle_native_call(&job.method, job.args, &worker_backend, &worker_artifacts) => result,
            };
            if completion_queue
                .send(NativeCompletion {
                    call_id: job.call_id,
                    eval_id: job.eval_id,
                    subscription_id: job.subscription_id,
                    result,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut observations = tokio::task::JoinSet::new();
    let mut subscriptions = HashMap::<u64, tokio_util::sync::CancellationToken>::new();
    let mut streams = tokio::task::JoinSet::new();
    let hub = backend.input_hub();
    let mut retained_images = HashMap::<u64, JsConsoleImage>::new();
    let mut retained_bytes = 0usize;
    let mut next_image = 0u64;
    let mut pending = HashMap::<u64, PendingEval>::new();
    let mut cancellation = tokio_util::sync::CancellationToken::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut last_heartbeat = tokio::time::Instant::now();
    let mut health = tokio::time::interval(Duration::from_millis(250));
    let mut active_calls = HashMap::<u64, Option<u64>>::new();
    let mut closed = false;
    let failure = loop {
        tokio::select! {
            _ = observations.join_next(), if !observations.is_empty() => {}
            _ = streams.join_next(), if !streams.is_empty() => {}
            command = commands.recv() => {
                let Some(ActorCommand::Eval { id, code, response }) = command else { closed = true; break "console closed".to_string(); };
                if !pending.is_empty() {
                    let _ = response.send(Err("the JavaScript console is already evaluating code".to_string()));
                    continue;
                }
                cancellation = tokio_util::sync::CancellationToken::new();
                pending.insert(id, PendingEval { response, images: Vec::new() });
                deadline = Some(tokio::time::Instant::now() + evaluation_timeout);
                if outgoing.try_send(json!({"type":"eval", "id":id, "code":code})).is_err() {
                    break "console transport output queue is full".to_string();
                }
            }
            _ = health.tick() => {
                let now = tokio::time::Instant::now();
                if deadline.is_some_and(|deadline| now >= deadline) {
                    break format!("JavaScript evaluation exceeded {} ms", evaluation_timeout.as_millis());
                }
                if now.duration_since(last_heartbeat) > Duration::from_secs(5) {
                    break "JavaScript runtime heartbeat stopped (including background callbacks)".to_string();
                }
            }
            result = &mut writer => {
                break match result {
                    Ok(Err(error)) => error,
                    other => format!("console input writer stopped: {other:?}"),
                };
            }
            completion = completions.recv() => {
                if let Some(completion) = completion {
                    active_calls.remove(&completion.call_id);
                    if !pending.contains_key(&completion.eval_id) && !completion.subscription_id.is_some_and(|id| subscriptions.get(&id).is_some_and(|t| !t.is_cancelled())) { continue; }
                    let reply = match completion.result {
                        Ok((mut value, image)) => {
                            if let Some(image) = image {
                                if completion.subscription_id.is_none() && let Some(eval) = pending.get_mut(&completion.eval_id) {
                                    if eval.images.len() >= 16 || eval.images.iter().map(|image| image.bytes.len()).sum::<usize>() + image.bytes.len() > 32*1024*1024 {
                                        if outgoing.try_send(json!({"type":"native_result","id":completion.call_id,"ok":false,"error":"evaluation image budget exceeded"})).is_err() { break "console output queue is full".into(); } continue;
                                    }
                                    eval.images.push(image);
                                } else {
                                    if retained_images.len() >= 16 || retained_bytes + image.bytes.len() > 32*1024*1024 {
                                        if outgoing.try_send(json!({"type":"native_result","id":completion.call_id,"ok":false,"error":"retained image budget exceeded; dispose old handles"})).is_err() { break "console output queue is full".into(); } continue;
                                    }
                                    next_image += 1; value["imageHandle"] = json!(next_image); retained_bytes += image.bytes.len(); retained_images.insert(next_image,image);
                                }
                            }
                            json!({"type":"native_result", "id":completion.call_id, "ok":true, "value":value})
                        }
                        Err(error) => json!({"type":"native_result", "id":completion.call_id, "ok":false, "error":error}),
                    };
                    if outgoing.try_send(reply).is_err() { break "console transport output queue is full".to_string(); }
                }
            }
            message = input.recv() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => break error,
                    None => break "JavaScript console process exited".to_string(),
                };
                match message.get("type").and_then(Value::as_str) {
                    Some("heartbeat") | Some("ready") => { last_heartbeat = tokio::time::Instant::now(); }
                    Some("native_call") => {
                        let call_id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let eval_id = message.get("evalId").and_then(Value::as_u64).unwrap_or(0);
                        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                        let subscription_id = message.get("subscriptionId").and_then(Value::as_u64);
                        let context_token = subscription_id.and_then(|id| subscriptions.get(&id)).filter(|t| !t.is_cancelled()).cloned();
                        if call_id == 0 || (!pending.contains_key(&eval_id) && context_token.is_none()) || active_calls.len() >= MAX_NATIVE_CALLS || active_calls.insert(call_id, subscription_id).is_some() {
                            if outgoing.try_send(json!({"type":"native_result","id":call_id,"ok":false,"error":"invalid, stale, duplicate or over-limit native call"})).is_err() {
                                break "console transport output queue is full".to_string();
                            }
                            continue;
                        }
                        let args = message.get("args").cloned().unwrap_or_else(|| json!({}));
                        if method == "subscribe_input" {
                            let result = if subscriptions.len() >= 32 { Err("console subscription limit reached".into()) } else {
                                match &hub { Some(hub) => hub.subscribe(&args), None => Err("input subscriptions require the Wayland proxy".into()) }
                            };
                            let reply = match result {
                                Ok(mut subscription) => {
                                    let id = subscription.id; subscriptions.insert(id, tokio_util::sync::CancellationToken::new());
                                    let queue = outgoing.clone();
                                    // Queue the acknowledgement before any event, including already buffered input.
                                    if outgoing.try_send(json!({"type":"native_result","id":call_id,"ok":true,"value":{"id":id,"initialState":subscription.initial}})).is_err() { break "console output queue is full".into(); }
                                    streams.spawn(async move {
                                        while let Some(event) = subscription.events.recv().await {
                                            subscription.bytes.fetch_sub(event.bytes, std::sync::atomic::Ordering::Relaxed);
                                            if queue.send(json!({"type":"input_event","subscriptionId":id,"event":event.value})).await.is_err() { return; }
                                        }
                                        let end = subscription.end.borrow_and_update().clone().unwrap_or_else(|| json!({"reason":"closed"}));
                                        let _ = queue.send(json!({"type":"input_end","subscriptionId":id,"end":end})).await;
                                    });
                                    active_calls.remove(&call_id); continue;
                                }
                                Err(error) => json!({"type":"native_result","id":call_id,"ok":false,"error":error}),
                            };
                            active_calls.remove(&call_id);
                            if outgoing.try_send(reply).is_err() { break "console output queue is full".into(); } continue;
                        }
                        if method == "finish_input" {
                            let id = args.get("id").and_then(Value::as_u64).unwrap_or(0);
                            if let Some(token) = subscriptions.remove(&id) { token.cancel(); }
                            let revoked = active_calls.iter().filter_map(|(call,subscription)| (*subscription == Some(id) && *call != call_id).then_some(*call)).collect::<Vec<_>>();
                            for call in revoked {
                                active_calls.remove(&call);
                                if outgoing.try_send(json!({"type":"native_result","id":call,"ok":false,"error":"input subscription was revoked"})).is_err() { break; }
                            }
                            active_calls.remove(&call_id); let _ = outgoing.try_send(json!({"type":"native_result","id":call_id,"ok":true,"value":null})); continue;
                        }
                        if method == "unsubscribe_input" {
                            let id = args.get("id").and_then(Value::as_u64).unwrap_or(0);
                            let result = match (&hub, subscriptions.get(&id)) { (Some(hub), Some(_)) => hub.stop(id,"unsubscribed"), _ => Err("unknown subscription".into()) };
                            let reply = match result { Ok(value) => json!({"type":"native_result","id":call_id,"ok":true,"value":value}), Err(error) => json!({"type":"native_result","id":call_id,"ok":false,"error":error}) };
                            active_calls.remove(&call_id); if outgoing.try_send(reply).is_err() { break "console output queue is full".into(); } continue;
                        }
                        if matches!(method.as_str(), "present_image" | "dispose_image") {
                            let id = args.get("imageHandle").and_then(Value::as_u64).unwrap_or(0);
                            let result = if method == "dispose_image" {
                                if let Some(image) = retained_images.remove(&id) { retained_bytes -= image.bytes.len(); Ok(json!({"disposed":true})) } else { Err("unknown or disposed image handle") }
                            } else if subscription_id.is_some() { Err("presentImage requires a foreground evaluation") }
                            else if let (Some(image),Some(eval)) = (retained_images.get(&id),pending.get_mut(&eval_id)) {
                                if eval.images.len() >= 16 || eval.images.iter().map(|image| image.bytes.len()).sum::<usize>() + image.bytes.len() > 32*1024*1024 { Err("evaluation image budget exceeded") }
                                else { eval.images.push(image.clone()); Ok(json!({"presented":true,"imageHandle":id})) }
                            } else { Err("unknown or disposed image handle") };
                            let reply = match result { Ok(value) => json!({"type":"native_result","id":call_id,"ok":true,"value":value}),Err(error) => json!({"type":"native_result","id":call_id,"ok":false,"error":error}) };
                            active_calls.remove(&call_id); if outgoing.try_send(reply).is_err() { break "console output queue is full".into(); } continue;
                        }
                        let job = NativeJob { call_id, eval_id, subscription_id, method, args, cancellation: context_token.unwrap_or_else(|| cancellation.clone()) };
                        if matches!(job.method.as_str(), "capture_next_frame" | "screenshot" | "windows" | "diagnostics") {
                            let backend = backend.clone(); let artifacts = artifacts.clone(); let completions = completion_sender.clone();
                            observations.spawn(async move {
                                let result = tokio::select! {
                                    _ = job.cancellation.cancelled() => return,
                                    result = handle_native_call(&job.method, job.args, &backend, &artifacts) => result,
                                };
                                let _ = completions.send(NativeCompletion { call_id: job.call_id, eval_id: job.eval_id, subscription_id: job.subscription_id, result }).await;
                            });
                        } else if job_sender.try_send(job).is_err() {
                            break "native mutation queue exceeded its limit".to_string();
                        }
                    }
                    Some("eval_result") => {
                        let id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let Some(evaluation) = pending.remove(&id) else { break "stale evaluation result".to_string(); };
                        cancellation.cancel(); deadline = None; active_calls.retain(|_, subscription| subscription.is_some());
                        let logs = message.get("logs").and_then(Value::as_array)
                            .map(|items| items.iter().take(501).filter_map(Value::as_str).map(|s| s.chars().take(4096).collect()).collect()).unwrap_or_default();
                        let result = if message.get("ok").and_then(Value::as_bool) == Some(true) {
                            Ok(JsEvalOutput { value: message.get("value").cloned().unwrap_or(Value::Null), logs, images: evaluation.images })
                        } else { Err(message.get("error").and_then(Value::as_str).unwrap_or("JavaScript evaluation failed").to_string()) };
                        let _ = evaluation.response.send(result);
                    }
                    _ => break "unexpected console transport message".to_string(),
                }
            }
        }
    };
    cancellation.cancel();
    for (id, token) in subscriptions {
        token.cancel();
        if let Some(hub) = &hub {
            let _ = hub.stop(id, "runtime_closed");
        }
    }
    streams.abort_all();
    observations.abort_all();
    worker.abort();
    writer.abort();
    reader.abort();
    let _ = child.kill().await;
    match tokio::time::timeout(Duration::from_secs(2), backend.cleanup_model_input()).await {
        Ok(Ok(())) => {}
        result => {
            artifacts.record(
                "console_synthetic_input_cleanup_failed",
                json!({"detail":format!("{result:?}")}),
            );
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr_task).await;
    let stderr = String::from_utf8_lossy(&stderr_text.lock().unwrap()).into_owned();
    let error = format!("{failure}; {stderr}; the runtime was restarted and its state was cleared");
    if pending.is_empty() && !closed {
        if let Some(ActorCommand::Eval { response, .. }) = commands.recv().await {
            let _ = response.send(Err(error.clone()));
        } else {
            closed = true;
        }
    }
    fail_pending(&mut pending, error);
    if closed {
        SessionOutcome::CommandsClosed
    } else {
        SessionOutcome::Restart
    }
}

async fn read_bounded_json<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Value, String> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await.map_err(|error| error.to_string())?;
        if buffer.is_empty() {
            return Err("JavaScript console process exited".to_string());
        }
        let count = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if line.len() + count > MAX_WIRE_FRAME_BYTES {
            return Err("console JSON frame exceeded 2 MiB".to_string());
        }
        let complete = buffer[count - 1] == b'\n';
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if complete {
            return serde_json::from_slice(&line)
                .map_err(|error| format!("invalid console JSON: {error}"));
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
            let mut diagnostics = diagnostics;
            if let Some(object) = diagnostics.as_object_mut() {
                object.remove("backend_socket");
                object.remove("sessions");
                object.remove("connection_history");
                object.remove("last_runtime_error");
                object.insert("console_isolation".to_string(), json!({"enforcement":"node-permissions", "filesystem":"denied", "network":"denied", "subprocess":"denied", "heap_limit_mib":128}));
            }
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
                    surface_id: optional_string(&args, "surfaceId"),
                    surface_fixed: args.get("coordinateSpace").and_then(Value::as_str)
                        == Some("surface-fixed"),
                    event,
                })
                .await?;
            artifacts.record(
                "console_wayland_pointer_event_emitted",
                json!({"window_id":window_id, "event":retained_event, "result":result}),
            );
            Ok((json!({"delivered": true, "detail": result}), None))
        }
        "begin_observation" => Ok((
            backend
                .begin_observation(
                    optional_string(&args, "windowId")
                        .ok_or("beginObservation requires windowId")?,
                    args.get("durationMs")
                        .and_then(Value::as_u64)
                        .unwrap_or(5000),
                )
                .await?,
            None,
        )),
        "end_observation" => Ok((
            backend
                .end_observation(
                    args.get("id")
                        .and_then(Value::as_u64)
                        .ok_or("endObservation requires id")?,
                )
                .await?,
            None,
        )),
        "input_capabilities" => Ok((
            backend
                .input_capabilities(
                    &optional_string(&args, "windowId")
                        .ok_or("inputCapabilities requires windowId")?,
                )
                .await?,
            None,
        )),
        "touch_event" => {
            let event = serde_json::from_value(
                args.get("event")
                    .cloned()
                    .ok_or("touchEvent requires event")?,
            )
            .map_err(|e| format!("invalid touch event: {e}"))?;
            let result = backend
                .emit_wayland_touch_event(crate::gui_backend::GuiWaylandTouchEventRequest {
                    window_id: optional_string(&args, "windowId")
                        .ok_or("touchEvent requires windowId")?,
                    surface_id: optional_string(&args, "surfaceId"),
                    event,
                })
                .await?;
            Ok((json!({"delivered":true,"detail":result}), None))
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
                    surface_id: optional_string(&args, "surfaceId"),
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
        input: Arc<crate::input_events::InputHub>,
        image: Option<Vec<u8>>,
        capture_gate: Option<Arc<tokio::sync::Notify>>,
        pointer_events: StdMutex<Vec<GuiWaylandPointerEventRequest>>,
        keyboard_events: StdMutex<Vec<GuiWaylandKeyboardEventRequest>>,
        fail_pointer_event_once_at: StdMutex<Option<usize>>,
        fail_keyboard_event_once_at: StdMutex<Option<usize>>,
        pointer_event_attempts: AtomicUsize,
        keyboard_event_attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl GuiBackend for RecordingBackend {
        fn input_hub(&self) -> Option<Arc<crate::input_events::InputHub>> {
            Some(self.input.clone())
        }
        async fn list_windows(&self) -> Result<Vec<GuiWindowInfo>, String> {
            Ok(Vec::new())
        }

        async fn screenshot(&self, _request: GuiScreenshotRequest) -> Result<Vec<u8>, String> {
            self.image
                .clone()
                .ok_or_else(|| "screenshot is not used by this test backend".to_string())
        }

        async fn capture_next_frame(
            &self,
            _request: GuiCaptureNextFrameRequest,
        ) -> Result<Vec<u8>, String> {
            if let Some(gate) = &self.capture_gate {
                gate.notified().await;
            }
            self.image
                .clone()
                .ok_or_else(|| "capture is not used by this test backend".to_string())
        }

        async fn emit_wayland_pointer_event(
            &self,
            request: GuiWaylandPointerEventRequest,
        ) -> Result<String, String> {
            if let Some(gate) = &self.capture_gate {
                gate.notify_one();
            }
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
    async fn node_permissions_block_io_after_constructor_escape() {
        let console = JsConsole::new(
            GuiBackendHandle::Command(Arc::new(CommandGuiBackend)),
            Arc::new(ArtifactStore::new()),
        );
        let sentinel = tempfile::NamedTempFile::new().unwrap();
        let path = serde_json::to_string(&sentinel.path().to_string_lossy()).unwrap();
        let source = format!(
            r#"
          const process = wayland.windows.constructor('return process')();
          const denied = [];
          for (const attempt of [
            () => process.getBuiltinModule('fs').readFileSync({path}),
            () => process.getBuiltinModule('fs').writeFileSync({path}, 'bad'),
            () => process.getBuiltinModule('child_process').spawnSync('/bin/true'),
            () => new (process.getBuiltinModule('worker_threads').Worker)('0', {{eval:true}}),
            () => process.getBuiltinModule('inspector').open(),
          ]) {{ try {{ attempt(); denied.push(false); }} catch(e) {{ denied.push(e.code === 'ERR_ACCESS_DENIED'); }} }}
          denied.splice(4,0,await new Promise(resolve=>{{ const socket=process.getBuiltinModule('net').connect({{path:'/tmp/no-such-console-socket'}}); socket.on('error',e=>resolve(e.code==='ERR_ACCESS_DENIED')); socket.on('connect',()=>{{socket.destroy();resolve(false);}}); }}));
          return {{denied,environment:Object.keys(process.env)}};
        "#
        );
        let output = console.eval(source).await.unwrap();
        assert_eq!(
            output.value["denied"],
            json!([true, true, true, true, true, true])
        );
        assert_eq!(output.value["environment"], json!([]));
    }

    #[tokio::test]
    async fn host_descriptor_is_not_inherited_by_node() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let sentinel = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(sentinel.path(), b"host descriptor sentinel").unwrap();
        let raw = unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 256) };
        assert!(raw >= 256);
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        let console = JsConsole::new(
            GuiBackendHandle::Command(Arc::new(CommandGuiBackend)),
            Arc::new(ArtifactStore::new()),
        );
        let output=console.eval(format!("const p=wayland.windows.constructor('return process')();try{{return {{leaked:p.getBuiltinModule('fs').readFileSync({raw},'utf8')}};}}catch(e){{return {{error:e.code}};}}")).await.unwrap();
        assert_eq!(output.value["error"], "EBADF");
        assert!(output.value.get("leaked").is_none());
        drop(owned);
    }

    #[tokio::test]
    async fn agent_written_recording_survives_idle_and_replays_twice() {
        let recording = Arc::new(RecordingBackend::default());
        let console = JsConsole::new_with_timeout(
            GuiBackendHandle::Test(recording.clone()),
            Arc::new(ArtifactStore::new()),
            Duration::from_millis(500),
        );
        console.eval("globalThis.samples=[]; globalThis.sub=await wayland.onInput({windowId:'fixture',devices:['pointer']},async e=>{ await wayland.sleep(5); samples.push(e); }); return sub.initialState;".into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(650)).await;
        // The foreground call has returned. Simulate the user reproducing a bug.
        for x in [128, 257, 511] {
            recording.input.publish(json!({"windowId":"fixture","surfaceId":"surface:1:2:3","device":"pointer","origin":"human","event":{"type":"motion","x":x,"y":-64}}));
        }
        let output = console.eval("const end=await sub.unsubscribe(); globalThis.replay=async()=>{for(const e of samples) await wayland.pointerEvent({windowId:e.windowId,surfaceId:e.surfaceId,coordinateSpace:'surface-fixed',event:{type:e.event.type,x:e.event.x,y:e.event.y}});}; await replay(); await replay(); return {count:samples.length,end,origins:samples.map(e=>e.origin)};".into()).await.unwrap();
        assert_eq!(output.value["count"], 3);
        assert_eq!(output.value["origins"], json!(["human", "human", "human"]));
        let events = recording.pointer_events.lock().unwrap();
        assert_eq!(events.len(), 6);
        assert_eq!(&events[..3], &events[3..]);
        assert!(events.iter().all(|e| e.surface_fixed));
    }

    #[tokio::test]
    async fn idle_callback_can_inject_and_self_draining_is_rejected() {
        let recording = Arc::new(RecordingBackend::default());
        let console = JsConsole::new(
            GuiBackendHandle::Test(recording.clone()),
            Arc::new(ArtifactStore::new()),
        );
        console.eval("globalThis.error=null; globalThis.sub=await wayland.onInput({windowId:'fixture'},async e=>{try {await sub.unsubscribe();} catch(e) {globalThis.error=e.message;} await wayland.pointerEvent({windowId:'fixture',event:{type:'motion',x:1,y:2}});});".into()).await.unwrap();
        recording.input.publish(json!({"windowId":"fixture","device":"pointer","origin":"human","event":{"type":"motion","x":1,"y":2}}));
        let output = console
            .eval("await sub.unsubscribe(); return globalThis.error;".into())
            .await
            .unwrap();
        assert!(output.value.as_str().unwrap().contains("own callback"));
        assert_eq!(recording.pointer_events.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pending_capture_does_not_block_ordered_input() {
        let backend = Arc::new(RecordingBackend {
            image: Some(
                crate::gui_backend_wayland::encode_rgba_png(1, 1, &[1, 2, 3, 255], None).unwrap(),
            ),
            capture_gate: Some(Arc::new(tokio::sync::Notify::new())),
            ..Default::default()
        });
        let console = JsConsole::new_with_timeout(
            GuiBackendHandle::Test(backend),
            Arc::new(ArtifactStore::new()),
            Duration::from_millis(1000),
        );
        let output = console.eval("return await Promise.all([wayland.captureNextFrame({windowId:'fixture'}),wayland.pointerEvent({windowId:'fixture',event:{type:'motion',x:0,y:0}})]);".into()).await.unwrap();
        assert_eq!(output.images.len(), 1);
        assert_eq!(output.value[1]["delivered"], true);
    }

    #[tokio::test]
    async fn background_images_require_explicit_foreground_presentation() {
        let backend = Arc::new(RecordingBackend {
            image: Some(
                crate::gui_backend_wayland::encode_rgba_png(1, 1, &[1, 2, 3, 255], None).unwrap(),
            ),
            ..Default::default()
        });
        let console = JsConsole::new(
            GuiBackendHandle::Test(backend.clone()),
            Arc::new(ArtifactStore::new()),
        );
        console.eval("globalThis.image=null; globalThis.sub=await wayland.onInput({windowId:'fixture'},async()=>{globalThis.image=await wayland.screenshot({windowId:'fixture'});});".into()).await.unwrap();
        backend.input.publish(json!({"windowId":"fixture","origin":"human","device":"pointer","event":{"type":"frame"}}));
        let output = console
            .eval("await sub.unsubscribe(); return image;".into())
            .await
            .unwrap();
        assert!(output.images.is_empty());
        assert!(output.value["imageHandle"].is_u64());
        let presented = console
            .eval("return await wayland.presentImage(image.imageHandle);".into())
            .await
            .unwrap();
        assert_eq!(presented.images.len(), 1);
        let output = console.eval("await wayland.disposeImage(image.imageHandle); try {await wayland.presentImage(image.imageHandle);} catch(e) {return e.message;}".into()).await.unwrap();
        assert!(output.value.as_str().unwrap().contains("disposed"));
    }

    #[tokio::test]
    async fn abandoned_callback_terminates_subscription_and_drain() {
        let recording = Arc::new(RecordingBackend::default());
        let console = JsConsole::new(
            GuiBackendHandle::Test(recording.clone()),
            Arc::new(ArtifactStore::new()),
        );
        console.eval("globalThis.sub=await wayland.onInput({windowId:'fixture',callbackTimeoutMs:25},()=>new Promise(()=>{}));return true;".into()).await.unwrap();
        recording.input.publish(json!({"windowId":"fixture","surfaceId":"surface:1:2:3","device":"pointer","origin":"human","event":{"type":"motion","x":1,"y":2}}));
        tokio::time::sleep(Duration::from_millis(75)).await;
        let output = console
            .eval("return {status:sub.status(),end:await sub.unsubscribe(),alive:42};".into())
            .await
            .unwrap();
        assert_eq!(output.value["alive"], 42);
        assert!(
            output.value["status"]["error"]
                .as_str()
                .unwrap()
                .contains("exceeded its time limit")
        );
        assert_eq!(output.value["status"]["active"], false);
    }

    #[tokio::test]
    async fn callback_failure_is_reported_without_losing_console_state() {
        let backend = Arc::new(RecordingBackend::default());
        let console = JsConsole::new(
            GuiBackendHandle::Test(backend.clone()),
            Arc::new(ArtifactStore::new()),
        );
        console.eval("globalThis.sub=await wayland.onInput({windowId:'fixture'},()=>{throw new Error('fixture callback failed');});".into()).await.unwrap();
        backend.input.publish(json!({"windowId":"fixture","origin":"human","device":"pointer","event":{"type":"frame"}}));
        let output = console
            .eval("await sub.unsubscribe(); return sub.status();".into())
            .await
            .unwrap();
        assert!(
            output.value["error"]
                .as_str()
                .unwrap()
                .contains("fixture callback failed")
        );
        assert_eq!(output.value["active"], false);
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
    async fn pointer_move_leaves_previous_window_before_entering_next() {
        let recording = Arc::new(RecordingBackend::default());
        let backend = GuiBackendHandle::Test(recording.clone());
        let artifacts = Arc::new(ArtifactStore::new());
        let console = JsConsole::new(backend, artifacts);

        console
            .eval("await wayland.move({windowId:'first', x:10, y:20}); return await wayland.move({windowId:'second', x:30, y:40});".to_string())
            .await
            .expect("move between windows");

        let actual = recording
            .pointer_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|request| (request.window_id.clone(), request.event.clone()))
            .collect::<Vec<_>>();
        assert!(matches!(
            actual.as_slice(),
            [
                (Some(first), GuiWaylandPointerEvent::Enter { .. }),
                (Some(_), GuiWaylandPointerEvent::Motion { .. }),
                (Some(_), GuiWaylandPointerEvent::Frame),
                (Some(leaving), GuiWaylandPointerEvent::Leave { .. }),
                (Some(_), GuiWaylandPointerEvent::Frame),
                (Some(second), GuiWaylandPointerEvent::Enter { .. }),
                (Some(_), GuiWaylandPointerEvent::Motion { .. }),
                (Some(_), GuiWaylandPointerEvent::Frame),
            ] if first == "first" && leaving == "first" && second == "second"
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
