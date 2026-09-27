//! Scoped, bounded input streams. Recording and replay remain ordinary JS programs.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{mpsc, watch};

pub(crate) struct InputHub {
    start: Instant,
    inner: Mutex<State>,
}
#[derive(Default)]
struct State {
    next_id: u64,
    sequence: u64,
    streams: HashMap<u64, Stream>,
    snapshots: HashMap<String, Value>,
}
struct Stream {
    window: String,
    origin: String,
    devices: Vec<String>,
    events: mpsc::Sender<QueuedInput>,
    bytes: Arc<AtomicUsize>,
    end: watch::Sender<Option<Value>>,
}
pub(crate) struct Subscription {
    pub id: u64,
    pub initial: Value,
    pub events: mpsc::Receiver<QueuedInput>,
    pub bytes: Arc<AtomicUsize>,
    pub end: watch::Receiver<Option<Value>>,
}
pub(crate) struct QueuedInput {
    pub value: Value,
    pub bytes: usize,
}
impl std::ops::Deref for QueuedInput {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}
impl Default for InputHub {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            inner: Mutex::new(State::default()),
        }
    }
}
impl InputHub {
    pub(crate) fn subscribe(&self, args: &Value) -> Result<Subscription, String> {
        let window = args
            .get("windowId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or("onInput requires an explicit windowId")?;
        let origin = args
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or("human");
        if !["human", "model", "all"].contains(&origin) {
            return Err("origin must be human, model or all".into());
        }
        let devices = args
            .get("devices")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["pointer".into(), "keyboard".into()]);
        if devices.len() > 5
            || args.get("devices").is_some_and(|d| {
                !d.is_array() || d.as_array().unwrap().iter().any(|v| !v.is_string())
            })
            || devices.is_empty()
            || devices.iter().any(|d| {
                ![
                    "pointer",
                    "keyboard",
                    "touch",
                    "relative_pointer",
                    "pointer_constraints",
                ]
                .contains(&d.as_str())
            })
        {
            return Err("devices must contain pointer, keyboard, touch, relative_pointer or pointer_constraints".into());
        }
        let mut state = self.inner.lock().unwrap();
        if state.streams.len() >= 32 {
            return Err("input subscription limit reached".into());
        }
        state.next_id += 1;
        let id = state.next_id;
        let (sender, events) = mpsc::channel(256);
        let (terminal, end) = watch::channel(None);
        let bytes = Arc::new(AtomicUsize::new(0));
        let initial = state
            .snapshots
            .get(&format!("{window}/{origin}/pointer"))
            .cloned()
            .unwrap_or_else(|| json!({"known":false}));
        let keyboard = state
            .snapshots
            .get(&format!("{window}/{origin}/keyboard"))
            .cloned()
            .unwrap_or_else(|| json!({"known":false}));
        let touch = state
            .snapshots
            .get(&format!("{window}/{origin}/touch"))
            .cloned()
            .unwrap_or_else(|| json!({"known":false,"contacts":{}}));
        state.streams.insert(
            id,
            Stream {
                window: window.into(),
                origin: origin.into(),
                devices,
                events: sender,
                bytes: bytes.clone(),
                end: terminal,
            },
        );
        Ok(Subscription {
            id,
            initial: json!({"sequence":state.sequence,"pointer":initial,"keyboard":keyboard,"touch":touch}),
            events,
            bytes,
            end,
        })
    }
    pub(crate) fn stop(&self, id: u64, reason: &str) -> Result<Value, String> {
        let mut state = self.inner.lock().unwrap();
        let Some(stream) = state.streams.remove(&id) else {
            return Ok(json!({"sequence":state.sequence,"reason":"already_stopped"}));
        };
        let end = json!({"sequence":state.sequence,"reason":reason});
        stream.end.send_replace(Some(end.clone()));
        Ok(end)
    }
    pub(crate) fn close_window(&self, window: &str, reason: &str) {
        let mut state = self.inner.lock().unwrap();
        let sequence = state.sequence;
        state.streams.retain(|_, stream| {
            if stream.window != window {
                return true;
            }
            stream
                .end
                .send_replace(Some(json!({"sequence":sequence,"reason":reason})));
            false
        });
        state
            .snapshots
            .retain(|key, _| !key.starts_with(&format!("{window}/")));
    }
    pub(crate) fn publish(&self, mut event: Value) {
        let mut state = self.inner.lock().unwrap();
        state.sequence += 1;
        event["sequence"] = json!(state.sequence);
        event["timestampMs"] = json!(self.start.elapsed().as_secs_f64() * 1000.0);
        let window = event["windowId"].as_str().unwrap_or("").to_string();
        let origin = event["origin"].as_str().unwrap_or("").to_string();
        let device = event["device"].as_str().unwrap_or("").to_string();
        if device == "pointer" {
            let snapshot = state
                .snapshots
                .entry(format!("{window}/{origin}/pointer"))
                .or_insert_with(|| json!({"known":false,"buttons":[]}));
            match event["event"]["type"].as_str() {
                Some("enter" | "motion") => {
                    snapshot["known"] = json!(true);
                    snapshot["surfaceId"] = event["surfaceId"].clone();
                    snapshot["x"] = event["event"]["x"].clone();
                    snapshot["y"] = event["event"]["y"].clone();
                }
                Some("leave") => {
                    snapshot["surfaceId"] = Value::Null;
                }
                Some("button") => {
                    let button = event["event"]["button"].clone();
                    let buttons = snapshot["buttons"].as_array_mut().unwrap();
                    buttons.retain(|b| *b != button);
                    if event["event"]["state"] == 1 {
                        buttons.push(button);
                    }
                }
                _ => {}
            }
        }
        if device == "keyboard" {
            let snapshot = state
                .snapshots
                .entry(format!("{window}/{origin}/keyboard"))
                .or_insert_with(|| json!({"known":false,"keys":[]}));
            match event["event"]["type"].as_str() {
                Some("enter") => {
                    snapshot["known"] = json!(true);
                    snapshot["surfaceId"] = event["surfaceId"].clone();
                    snapshot["keys"] = event["event"]["keys"].clone();
                    snapshot["keymapId"] = event["keymapId"].clone();
                }
                Some("leave") => {
                    snapshot["surfaceId"] = Value::Null;
                }
                Some("key") => {
                    let key = event["event"]["key"].clone();
                    let keys = snapshot["keys"].as_array_mut().unwrap();
                    keys.retain(|k| *k != key);
                    if event["event"]["state"] == 1 {
                        keys.push(key);
                    }
                }
                Some("modifiers") => {
                    for name in ["mods_depressed", "mods_latched", "mods_locked", "group"] {
                        snapshot[name] = event["event"][name].clone();
                    }
                }
                _ => {}
            }
        }
        if device == "touch" {
            let snapshot = state
                .snapshots
                .entry(format!("{window}/{origin}/touch"))
                .or_insert_with(|| json!({"known":false,"contacts":{}}));
            snapshot["known"] = json!(true);
            if let Some(id) = event["event"]["id"].as_i64() {
                let id = id.to_string();
                match event["event"]["type"].as_str() {
                    Some("down" | "motion") => {
                        snapshot["contacts"][&id] = json!({"surfaceId":event["surfaceId"],"x":event["event"]["x"],"y":event["event"]["y"]});
                    }
                    Some("up") => {
                        snapshot["contacts"].as_object_mut().unwrap().remove(&id);
                    }
                    _ => {}
                }
            }
            if event["event"]["type"] == "cancel" {
                snapshot["contacts"] = json!({});
            }
        }
        let event_bytes = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
        let mut stopped = Vec::new();
        for (id, stream) in &state.streams {
            if stream.window != window
                || (stream.origin != "all" && stream.origin != origin)
                || !stream.devices.contains(&device)
            {
                continue;
            }
            let over_limit = event_bytes > 4 * 1024 * 1024
                || stream
                    .bytes
                    .load(Ordering::Relaxed)
                    .saturating_add(event_bytes)
                    > 4 * 1024 * 1024;
            let unavailable = if over_limit {
                true
            } else {
                stream.bytes.fetch_add(event_bytes, Ordering::Relaxed);
                if stream
                    .events
                    .try_send(QueuedInput {
                        value: event.clone(),
                        bytes: event_bytes,
                    })
                    .is_err()
                {
                    stream.bytes.fetch_sub(event_bytes, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            };
            if unavailable {
                stream.end.send_replace(Some(json!({"sequence":state.sequence,"reason":"overflow","error":"input stream queue overflow; recording has a gap"})));
                stopped.push(*id);
            }
        }
        for id in stopped {
            state.streams.remove(&id);
        }
    }
}
