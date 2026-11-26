use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::fd::AsFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt;
use ordered_stream::{OrderedStreamExt, join as join_streams};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use zbus::address::{Address, Transport};
use zbus::connection::{Builder, Connection};
use zbus::match_rule::PathSpec;
use zbus::message::{Flags, Message, Type};
use zbus::names::BusName;
use zbus::wire::{Array, Dict, ObjectPath, Signature, Structure, StructureBuilder, Variant};

use crate::dbus_endpoint::{Direction, Endpoint};
use crate::dbus_fd::{Descriptors, Received};

const MAX_BRIDGES: usize = 8;
const MAX_CONNECTIONS: usize = 64;
const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_EXPORTS: usize = 256;
const MAX_PENDING: usize = 64;
const MAX_NOTIFICATION_CALLBACKS: usize = 64;
const MAX_MESSAGE_BYTES: usize = 512 * 1024;
const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) struct Console {
    next_id: AtomicU64,
    handles: Mutex<HashMap<u64, Handle>>,
    bridges: Mutex<HashMap<u64, Bridge>>,
    bridge_slots: Arc<Semaphore>,
    exports: Mutex<HashMap<u64, Export>>,
    subscriptions: Mutex<HashMap<u64, (u64, CancellationToken)>>,
    pending: Mutex<HashMap<u64, Pending>>,
    notifications: Mutex<HashMap<u64, u64>>,
    descriptors: Descriptors,
    output: mpsc::Sender<Value>,
}

#[derive(Clone)]
struct Handle {
    connection: Connection,
    scope: Option<u64>,
    endpoint: Option<Arc<Tap>>,
    address: Option<Address>,
    cancellation: CancellationToken,
}

struct Bridge {
    endpoints: Vec<Endpoint>,
    _slot: OwnedSemaphorePermit,
}

struct Tap {
    observers: Mutex<HashMap<u64, (u64, Observation)>>,
    console: Weak<Console>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Options {
    session: bool,
    accessibility: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            session: true,
            accessibility: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Observation {
    #[serde(rename = "type")]
    message_type: Option<MessageType>,
    direction: Option<Direction>,
    connection: Option<u64>,
    sender: Option<String>,
    destination: Option<String>,
    path: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    serial: Option<u32>,
    reply_serial: Option<u32>,
    error_name: Option<String>,
    signature: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum MessageType {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

impl From<Type> for MessageType {
    fn from(value: Type) -> Self {
        match value {
            Type::MethodCall => Self::MethodCall,
            Type::MethodReturn => Self::MethodReturn,
            Type::Error => Self::Error,
            Type::Signal => Self::Signal,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    #[serde(skip)]
    service: u64,
    path: String,
    interface: String,
    methods: HashMap<String, Method>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Method {
    #[serde(rename = "in", default)]
    input: String,
    #[serde(rename = "out", default)]
    output: String,
}

struct Pending {
    service: u64,
    call: Option<Message>,
    signature: String,
    done: CancellationToken,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.done.cancel();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Call {
    destination: String,
    path: String,
    interface: Option<String>,
    member: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    body: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Signal {
    destination: Option<String>,
    path: String,
    interface: String,
    member: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    body: Vec<Value>,
}

impl Drop for Console {
    fn drop(&mut self) {
        if let Ok(handles) = self.handles.get_mut() {
            for handle in handles.values() {
                handle.cancellation.cancel();
            }
        }
    }
}

impl Console {
    pub(crate) fn new(output: mpsc::Sender<Value>) -> Self {
        Self {
            next_id: AtomicU64::new(1 << 50),
            handles: Mutex::new(HashMap::new()),
            bridges: Mutex::new(HashMap::new()),
            bridge_slots: Arc::new(Semaphore::new(MAX_BRIDGES)),
            exports: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            notifications: Mutex::new(HashMap::new()),
            descriptors: Descriptors::default(),
            output,
        }
    }

    fn id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn context(&self, id: u64) -> Option<CancellationToken> {
        self.handles
            .lock()
            .ok()?
            .get(&id)
            .map(|entry| entry.cancellation.clone())
    }

    pub(crate) fn notification_done(&self, event: u64) {
        if let Ok(mut notifications) = self.notifications.lock() {
            notifications.remove(&event);
        }
    }

    fn event_slot(&self) -> Option<mpsc::Permit<'_, Value>> {
        let slot = self.output.try_reserve().ok()?;
        // Keep space for native replies when the app produces events faster
        // than the console can consume them.
        (self.output.capacity() >= 8).then_some(slot)
    }

    fn notify(&self, owner: u64, build: impl FnOnce(u64) -> Result<Value, String>) -> bool {
        let Some(slot) = self.event_slot() else {
            return false;
        };
        let Ok(mut notifications) = self.notifications.lock() else {
            return false;
        };
        if notifications.len() >= MAX_NOTIFICATION_CALLBACKS {
            return false;
        }
        let id = self.id();
        match build(id) {
            Ok(event) => {
                notifications.insert(id, owner);
                slot.send(event);
                true
            }
            Err(error) => {
                tracing::debug!(%error, "skipping D-Bus notification");
                false
            }
        }
    }

    pub(crate) async fn request(
        self: &Arc<Self>,
        method: &str,
        args: Value,
    ) -> Result<Value, String> {
        let id = || {
            args["id"]
                .as_u64()
                .ok_or_else(|| "missing D-Bus handle".to_string())
        };
        match method {
            "dbus_open" => {
                return self
                    .open(serde_json::from_value(args).map_err(display)?)
                    .await;
            }
            "dbus_shutdown" => return self.shutdown(id()?).await,
            "dbus_connect" => {
                let address: Address = args["address"]
                    .as_str()
                    .ok_or("missing D-Bus address")?
                    .parse()
                    .map_err(display)?;
                if !matches!(address.transport(), Transport::Unix(_)) {
                    return Err("D-Bus connections require a local Unix socket address".into());
                }
                return self.insert(Handle {
                    connection: connect(address).await?,
                    scope: None,
                    endpoint: None,
                    address: None,
                    cancellation: CancellationToken::new(),
                });
            }
            "dbus_close" => return self.close(id()?).await,
            "dbus_reply" => return self.reply(id()?, &args).await,
            "dbus_fd_read" | "dbus_fd_write" | "dbus_fd_close" => {
                return self.descriptors.io(id()?, method, &args).await;
            }
            _ => {}
        }
        let id = id()?;
        let handle = match self.handle(id) {
            Ok(handle) => handle,
            Err(_)
                if matches!(
                    method,
                    "dbus_unsubscribe" | "dbus_unobserve" | "dbus_unexport"
                ) =>
            {
                return Ok(Value::Null);
            }
            Err(error) => return Err(error),
        };
        let handler = || args["handler"].as_u64().ok_or("missing D-Bus handler");
        match method {
            "dbus_call" => {
                tokio::select! {
                    _ = handle.cancellation.cancelled() => Err("D-Bus connection is closed".into()),
                    result = tokio::time::timeout(CALL_TIMEOUT, self.call(id, &handle, args["message"].clone())) =>
                        result.map_err(|_| "D-Bus call timed out")?,
                }
            }
            "dbus_pipe" => self.descriptors.pipe(id, &handle.cancellation),
            "dbus_subscribe" => {
                let rule = args["rule"].as_str().ok_or("missing D-Bus match rule")?;
                self.subscribe(id, handle, handler()?, rule).await
            }
            "dbus_unsubscribe" => {
                let handler = handler()?;
                let mut subscriptions = self.subscriptions.lock().map_err(display)?;
                if subscriptions
                    .get(&handler)
                    .is_some_and(|(owner, _)| *owner == id)
                    && let Some((_, token)) = subscriptions.remove(&handler)
                {
                    token.cancel();
                }
                Ok(Value::Null)
            }
            "dbus_serve" => {
                self.serve(handle, args["name"].as_str().ok_or("missing D-Bus name")?)
                    .await
            }
            "dbus_export" => {
                if handle.scope.is_none() || handle.address.is_some() {
                    return Err("object export requires a service handle".into());
                }
                let handler = handler()?;
                let mut exports = self.exports.lock().map_err(display)?;
                if handle.cancellation.is_cancelled() {
                    return Err("D-Bus connection is closed".into());
                }
                if exports.len() >= MAX_EXPORTS {
                    return Err("console D-Bus export limit reached".into());
                }
                let mut export: Export =
                    serde_json::from_value(args["definition"].clone()).map_err(display)?;
                export.service = id;
                ObjectPath::try_from(export.path.as_str()).map_err(display)?;
                zbus::names::InterfaceName::try_from(export.interface.as_str()).map_err(display)?;
                if export.methods.is_empty() || export.methods.len() > 64 {
                    return Err("a D-Bus export needs between 1 and 64 methods".into());
                }
                for (name, method) in &export.methods {
                    zbus::names::MemberName::try_from(name.as_str()).map_err(display)?;
                    signature_fields(&method.input)?;
                    signature_fields(&method.output)?;
                }
                if exports.values().any(|old| {
                    old.service == id
                        && old.path == export.path
                        && old.interface == export.interface
                }) {
                    return Err("D-Bus interface is already exported".into());
                }
                exports.insert(handler, export);
                Ok(Value::Null)
            }
            "dbus_unexport" => {
                let handler = handler()?;
                let mut exports = self.exports.lock().map_err(display)?;
                if exports
                    .get(&handler)
                    .is_some_and(|entry| entry.service == id)
                {
                    exports.remove(&handler);
                }
                Ok(Value::Null)
            }
            "dbus_signal" => {
                signal(
                    &handle.connection,
                    args["message"].clone(),
                    &self.descriptors,
                )
                .await
            }
            "dbus_observe" | "dbus_unobserve" => {
                let tap = handle
                    .endpoint
                    .ok_or("observation requires an app-facing bridge")?;
                let mut observers = tap.observers.lock().map_err(display)?;
                let handler = handler()?;
                if method == "dbus_observe" {
                    if handle.cancellation.is_cancelled() {
                        return Err("D-Bus connection is closed".into());
                    }
                    if observers.len() >= 32 {
                        return Err("D-Bus observation limit reached".into());
                    }
                    let rule = serde_json::from_value(args["match"].clone()).map_err(display)?;
                    observers.insert(handler, (id, rule));
                } else {
                    observers.remove(&handler);
                }
                Ok(Value::Null)
            }
            _ => Err("unknown D-Bus operation".into()),
        }
    }

    fn insert(self: &Arc<Self>, handle: Handle) -> Result<Value, String> {
        let bridges = self.bridges.lock().map_err(display)?;
        if handle
            .scope
            .is_some_and(|scope| !bridges.contains_key(&scope))
        {
            return Err("D-Bus bridge is closed".into());
        }
        let mut handles = self.handles.lock().map_err(display)?;
        if handles.len() >= MAX_CONNECTIONS {
            return Err("console D-Bus connection limit reached".into());
        }
        Ok(self.register_handle(&mut handles, handle))
    }

    fn register_handle(
        self: &Arc<Self>,
        handles: &mut HashMap<u64, Handle>,
        handle: Handle,
    ) -> Value {
        let id = self.id();
        let name = handle
            .connection
            .unique_name()
            .map(|name| name.as_str().to_owned());
        let connection = handle.connection.clone();
        let cancellation = handle.cancellation.clone();
        handles.insert(id, handle);
        let console = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                _ = connection.closed() => {}
            }
            if let Some(console) = console.upgrade() {
                let _ = console.close(id).await;
                let _ = console
                    .output
                    .send(json!({"type":"dbus_closed","scope":id}))
                    .await;
            }
        });
        json!({"id":id,"uniqueName":name})
    }

    fn handle(&self, id: u64) -> Result<Handle, String> {
        self.handles
            .lock()
            .map_err(display)?
            .get(&id)
            .cloned()
            .ok_or_else(|| "D-Bus connection is closed".into())
    }

    async fn open(self: &Arc<Self>, options: Options) -> Result<Value, String> {
        let slot = self
            .bridge_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "console D-Bus bridge limit reached")?;
        let scope = self.id();
        let mut endpoints = Vec::new();
        let mut environment = json!({});
        let mut returned = json!({"id":scope});
        let mut prepared = Vec::new();
        for (kind, variable, enabled) in [
            ("session", "DBUS_SESSION_BUS_ADDRESS", options.session),
            ("accessibility", "AT_SPI_BUS_ADDRESS", options.accessibility),
        ] {
            if !enabled {
                continue;
            }
            let tap = Arc::new(Tap {
                console: Arc::downgrade(self),
                observers: Mutex::default(),
            });
            let capture = tap.clone();
            let endpoint = Endpoint::start(Arc::new(move |id, direction, message| {
                capture.deliver(id, direction, message)
            }))
            .await?;
            let handle = Handle {
                connection: connect(endpoint.peer_address.clone()).await?,
                scope: Some(scope),
                endpoint: Some(tap),
                address: Some(endpoint.peer_address.clone()),
                cancellation: CancellationToken::new(),
            };
            environment[variable] = json!(endpoint.address);
            prepared.push((kind, handle));
            endpoints.push(endpoint);
        }
        if endpoints.is_empty() {
            return Err("enable a D-Bus bridge endpoint".into());
        }
        let mut bridges = self.bridges.lock().map_err(display)?;
        let mut handles = self.handles.lock().map_err(display)?;
        if handles.len() + prepared.len() > MAX_CONNECTIONS {
            return Err("console D-Bus connection limit reached".into());
        }
        for (kind, handle) in prepared {
            returned[kind] = self.register_handle(&mut handles, handle);
        }
        returned["environment"] = environment;
        bridges.insert(
            scope,
            Bridge {
                endpoints,
                _slot: slot,
            },
        );
        Ok(returned)
    }

    async fn shutdown(&self, scope: u64) -> Result<Value, String> {
        let bridge = self.bridges.lock().map_err(display)?.remove(&scope);
        if let Some(bridge) = bridge {
            for endpoint in bridge.endpoints {
                endpoint.close().await;
            }
        }
        let handles: Vec<_> = self
            .handles
            .lock()
            .map_err(display)?
            .iter()
            .filter_map(|(id, handle)| (handle.scope == Some(scope)).then_some(*id))
            .collect();
        for id in handles {
            if let Err(error) = self.close(id).await {
                tracing::debug!(%error, "closing D-Bus connection");
            }
        }
        Ok(Value::Null)
    }

    async fn close(&self, id: u64) -> Result<Value, String> {
        let handle = self.handles.lock().map_err(display)?.remove(&id);
        if let Some(handle) = &handle {
            handle.cancellation.cancel();
            if let Some(tap) = &handle.endpoint {
                tap.observers.lock().map_err(display)?.clear();
            }
        }
        self.descriptors.close_owner(id);
        self.notifications
            .lock()
            .map_err(display)?
            .retain(|_, owner| *owner != id);
        self.subscriptions
            .lock()
            .map_err(display)?
            .retain(|_, (owner, token)| {
                if *owner != id {
                    return true;
                }
                token.cancel();
                false
            });
        self.exports
            .lock()
            .map_err(display)?
            .retain(|_, export| export.service != id);
        self.pending.lock().map_err(display)?.retain(|_, pending| {
            if pending.service == id {
                pending.done.cancel();
                false
            } else {
                true
            }
        });
        if let Some(handle) = handle
            && !handle.connection.is_closed()
        {
            tokio::time::timeout(CALL_TIMEOUT, handle.connection.close())
                .await
                .map_err(display)?
                .map_err(display)?;
        }
        Ok(Value::Null)
    }

    async fn subscribe(
        self: &Arc<Self>,
        id: u64,
        handle: Handle,
        handler: u64,
        expression: &str,
    ) -> Result<Value, String> {
        let rule = zbus::MatchRule::try_from(expression).map_err(display)?;
        if rule.msg_type() != Some(Type::Signal) {
            return Err("a D-Bus signal subscription needs type='signal'".into());
        }
        let connection = &handle.connection;
        let mut owner = None;
        let mut snapshot = None;
        let owner_changes = if let Some(BusName::WellKnown(name)) = rule.sender() {
            let changes = zbus::MatchRule::builder()
                .msg_type(Type::Signal)
                .sender("org.freedesktop.DBus")
                .path("/org/freedesktop/DBus")
                .interface("org.freedesktop.DBus")
                .member("NameOwnerChanged")
                .add_arg(name.as_str())
                .build()
                .map_err(display)?;
            let changes = zbus::MessageStream::for_match_rule(changes, connection, None)
                .await
                .map_err(display)?;
            let query = Message::method_call("/org/freedesktop/DBus", "GetNameOwner")
                .destination("org.freedesktop.DBus")
                .interface("org.freedesktop.DBus")
                .build(&(name.as_str(),))
                .map_err(display)?;
            let reply = tokio::time::timeout(CALL_TIMEOUT, call_message(connection, &query))
                .await
                .map_err(display)??;
            let header = reply.header();
            if reply.message_type() == Type::MethodReturn {
                owner = Some(reply.body().deserialize::<String>().map_err(display)?);
            } else if !header
                .error_name()
                .is_some_and(|name| name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner")
            {
                return Err(format!("GetNameOwner failed: {:?}", header.error_name()));
            }
            snapshot = Some(reply.recv_position());
            Some(OrderedStreamExt::map(changes, |item| (true, item)))
        } else {
            None
        };
        let rule = rule.to_owned();
        let signals = zbus::MessageStream::for_match_rule(rule.clone(), connection, None)
            .await
            .map_err(display)?;
        // zbus cannot test well-known names against signal headers. The bus
        // sends the union of this connection's matches, so track the owner
        // alongside the signals in their original receive order.
        let mut stream = join_streams(
            OrderedStreamExt::map(signals, |item| (false, item)),
            owner_changes,
        )
        .into_stream();
        let cancellation = handle.cancellation.child_token();
        {
            let mut subscriptions = self.subscriptions.lock().map_err(display)?;
            if cancellation.is_cancelled() {
                return Err("D-Bus connection is closed".into());
            }
            if subscriptions.len() >= MAX_SUBSCRIPTIONS {
                return Err("console D-Bus subscription limit reached".into());
            }
            subscriptions.insert(handler, (id, cancellation.clone()));
        }
        let console = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let item = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    item = StreamExt::next(&mut stream) => item,
                };
                let Some((owner_changed, Ok(message))) = item else {
                    break;
                };
                if owner_changed {
                    if snapshot.is_some_and(|sequence| message.recv_position() <= sequence) {
                        continue;
                    }
                    if let Ok((_, _, name)) =
                        message.body().deserialize::<(String, String, String)>()
                    {
                        owner = (!name.is_empty()).then_some(name);
                    }
                    continue;
                }
                if !matches_signal(&rule, &message, snapshot.is_some(), owner.as_deref()) {
                    continue;
                }
                let Some(console) = console.upgrade() else {
                    break;
                };
                // Keep delivery ordered with unsubscribe. No event can be queued after its acknowledgement.
                let Ok(subscriptions) = console.subscriptions.lock() else {
                    break;
                };
                if cancellation.is_cancelled() || !subscriptions.contains_key(&handler) {
                    break;
                }
                let mut received = console.descriptors.receive(id, &handle.cancellation);
                if console.notify(id, |event| {
                    let message = unpack(&message, &mut Some(&mut received))?;
                    bounded_json(json!({"type":"dbus_event","scope":id,"event":event,
                        "handler":handler,"message":message}))
                }) {
                    received.commit();
                }
            }
            if let Some(console) = console.upgrade()
                && let Ok(mut subscriptions) = console.subscriptions.lock()
            {
                subscriptions.remove(&handler);
            }
        });
        Ok(Value::Null)
    }

    async fn serve(self: &Arc<Self>, handle: Handle, name: &str) -> Result<Value, String> {
        let scope = handle
            .scope
            .ok_or("service exports require a private app-facing bridge")?;
        let address = handle
            .address
            .ok_or("service requires an app-facing bus handle")?;
        let connection = tokio::select! {
            _ = handle.cancellation.cancelled() => return Err("D-Bus connection is closed".into()),
            result = connect(address) => result?,
        };
        let mut stream = zbus::MessageStream::from(&connection);
        connection.request_name(name).await.map_err(display)?;
        if handle.cancellation.is_cancelled() {
            return Err("D-Bus connection is closed".into());
        }
        let service = Handle {
            connection,
            scope: Some(scope),
            endpoint: None,
            address: None,
            cancellation: CancellationToken::new(),
        };
        let result = self.insert(service.clone())?;
        let id = result["id"].as_u64().expect("D-Bus handle has an ID");
        let console = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let item = tokio::select! {
                    biased;
                    _ = service.cancellation.cancelled() => break,
                    item = StreamExt::next(&mut stream) => item,
                };
                let Some(Ok(message)) = item else {
                    break;
                };
                if message.message_type() != Type::MethodCall {
                    continue;
                }
                let Some(console) = console.upgrade() else {
                    break;
                };
                let reply = match console.dispatch(id, &service, &message) {
                    Ok(Some(reply)) => send_message(&service.connection, &reply).await,
                    Ok(None) => Ok(()),
                    Err((name, error)) => {
                        send_error(&service.connection, &message, name, &error).await
                    }
                };
                if let Err(error) = reply {
                    tracing::debug!(%error, "sending D-Bus service reply");
                }
            }
        });
        Ok(result)
    }

    fn dispatch(
        self: &Arc<Self>,
        service: u64,
        handle: &Handle,
        message: &Message,
    ) -> Result<Option<Message>, (&'static str, String)> {
        let failed = |error| ("org.freedesktop.DBus.Error.Failed", error);
        let limits = |error: &str| {
            (
                "org.freedesktop.DBus.Error.LimitsExceeded",
                error.to_owned(),
            )
        };
        let header = message.header();
        let path = header.path().map_or("", |value| value.as_str());
        let interface = header.interface().map(|value| value.as_str());
        let member = header.member().map_or("", |value| value.as_str());
        let exports = self
            .exports
            .lock()
            .map_err(|error| failed(display(error)))?;
        let signature = wire_signature(message).map_err(failed)?;
        let mut methods = exports.iter().filter_map(|(handler, export)| {
            (export.service == service
                && export.path == path
                && interface.is_none_or(|name| name == export.interface))
            .then(|| export.methods.get(member).map(|method| (*handler, method)))
            .flatten()
        });
        let method = methods.next();
        if methods.next().is_some() {
            return Err((
                "org.freedesktop.DBus.Error.UnknownMethod",
                "specify an interface for this member".into(),
            ));
        }
        if method.is_none()
            && member == "Introspect"
            && interface.is_none_or(|name| name == "org.freedesktop.DBus.Introspectable")
        {
            if !signature.is_empty() {
                return Err((
                    "org.freedesktop.DBus.Error.InvalidArgs",
                    "Introspect takes no arguments".into(),
                ));
            }
            if header.primary().flags().contains(Flags::NO_REPLY_EXPECTED) {
                return Ok(None);
            }
            let xml = introspection(
                exports.values().filter(|export| export.service == service),
                path,
            )
            .ok_or((
                "org.freedesktop.DBus.Error.UnknownObject",
                "object is not exported".into(),
            ))?;
            return Message::method_return(&header)
                .build(&(xml,))
                .map(Some)
                .map_err(|error| failed(display(error)));
        }
        let Some((handler, method)) = method else {
            return Err((
                "org.freedesktop.DBus.Error.UnknownMethod",
                "method is not exported".into(),
            ));
        };
        if signature != method.input {
            return Err((
                "org.freedesktop.DBus.Error.InvalidArgs",
                "incorrect method signature".into(),
            ));
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|error| failed(display(error)))?;
        if handle.cancellation.is_cancelled() {
            return Err(failed("D-Bus connection is closed".into()));
        }
        if pending.len() >= MAX_PENDING {
            return Err(limits("D-Bus service callback limit reached"));
        }
        let slot = self
            .event_slot()
            .ok_or_else(|| limits("console event queue is full"))?;
        let mut received = self.descriptors.receive(service, &handle.cancellation);
        let body = unpack(message, &mut Some(&mut received)).map_err(failed)?;
        let id = self.id();
        let event = bounded_json(
            json!({"type":"dbus_event","scope":service,"handler":handler,"call":id,"message":body}),
        )
        .map_err(failed)?;
        let done = CancellationToken::new();
        pending.insert(
            id,
            Pending {
                service,
                call: Some(message.clone()),
                signature: method.output.clone(),
                done: done.clone(),
            },
        );
        slot.send(event);
        received.commit();
        let console = Arc::downgrade(self);
        let connection = handle.connection.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(SERVICE_TIMEOUT, done.cancelled())
                .await
                .is_err()
                && let Some(console) = console.upgrade()
            {
                // Keep the slot until JS settles. Timed out handlers may still be running.
                let call = console
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.get_mut(&id)?.call.take());
                if let Some(message) = call {
                    let _ = send_error(
                        &connection,
                        &message,
                        "org.freedesktop.DBus.Error.Timeout",
                        "service callback timed out",
                    )
                    .await;
                }
            }
        });
        Ok(None)
    }

    async fn reply(&self, id: u64, args: &Value) -> Result<Value, String> {
        let pending = self
            .pending
            .lock()
            .map_err(display)?
            .remove(&id)
            .ok_or("D-Bus method call ended")?;
        pending.done.cancel();
        let connection = self.handle(pending.service)?.connection;
        let call = pending.call.as_ref().ok_or("D-Bus method call timed out")?;
        let header = call.header();
        if header.primary().flags().contains(Flags::NO_REPLY_EXPECTED) {
            return Ok(Value::Null);
        }
        let answer = &args["answer"];
        let reply = (|| {
            let fields = answer
                .as_object()
                .ok_or("D-Bus method handler must return a reply object")?;
            if fields.contains_key("errorName") {
                let name = answer["errorName"]
                    .as_str()
                    .ok_or("D-Bus error name must be a string")?;
                let detail = answer["errorMessage"].as_str().unwrap_or("");
                if detail.contains('\0') {
                    return Err("D-Bus strings cannot contain a NUL byte".into());
                }
                if fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "errorName" | "errorMessage"))
                {
                    return Err("D-Bus error reply accepts errorName and errorMessage".into());
                }
                Message::error(&header, name)
                    .build(&(detail,))
                    .map_err(display)
            } else {
                if fields.keys().any(|key| key != "body") {
                    return Err("D-Bus successful reply accepts a body array".into());
                }
                let body = if let Some(body) = fields.get("body") {
                    body.as_array()
                        .ok_or("D-Bus reply body must be an array")?
                        .as_slice()
                } else {
                    &[]
                };
                build_body(
                    Message::method_return(&header),
                    &pending.signature,
                    body,
                    &self.descriptors,
                )
            }
        })();
        match reply {
            Ok(reply) => send_message(&connection, &reply).await?,
            Err(error) => {
                send_error(
                    &connection,
                    call,
                    "org.freedesktop.DBus.Error.Failed",
                    &error,
                )
                .await?;
                return Err(error);
            }
        }
        Ok(Value::Null)
    }

    async fn call(&self, id: u64, handle: &Handle, args: Value) -> Result<Value, String> {
        let call: Call = serde_json::from_value(args).map_err(display)?;
        let connection = &handle.connection;
        let mut builder = Message::method_call(call.path.as_str(), call.member.as_str())
            .destination(call.destination.as_str());
        if let Some(interface) = &call.interface {
            builder = builder.interface(interface.as_str());
        }
        if let Some(sender) = connection.unique_name() {
            builder = builder.sender(sender);
        }
        let request = build_body(builder, &call.signature, &call.body, &self.descriptors)?;
        let reply = call_message(connection, &request).await?;
        let mut received = self.descriptors.receive(id, &handle.cancellation);
        let value = bounded_json(unpack(&reply, &mut Some(&mut received))?)?;
        received.commit();
        Ok(value)
    }
}

fn matches_signal(
    rule: &zbus::MatchRule<'_>,
    message: &Message,
    tracks_owner: bool,
    owner: Option<&str>,
) -> bool {
    let header = message.header();
    (!tracks_owner
        || owner.is_some_and(|owner| header.sender().is_some_and(|name| name.as_str() == owner)))
        && rule.destination().is_none_or(|expected| {
            header
                .destination()
                .is_some_and(|name| name.as_str() == expected.as_str())
        })
        && match rule.path_spec() {
            Some(PathSpec::PathNamespace(namespace)) => header.path().is_some_and(|path| {
                path.as_str() == namespace.as_str()
                    || path
                        .as_str()
                        .strip_prefix(namespace.as_str())
                        .is_some_and(|suffix| namespace.as_str() == "/" || suffix.starts_with('/'))
            }),
            _ => true,
        }
}

impl Observation {
    fn matches(&self, connection: u64, direction: Direction, message: &Message) -> bool {
        let header = message.header();
        macro_rules! field {
            ($name:ident) => {
                self.$name
                    .as_deref()
                    .is_none_or(|value| header.$name().is_some_and(|field| field.as_str() == value))
            };
        }
        self.connection.is_none_or(|id| id == connection)
            && self.direction.is_none_or(|value| value == direction)
            && self
                .message_type
                .is_none_or(|value| value == MessageType::from(message.message_type()))
            && field!(sender)
            && field!(destination)
            && field!(path)
            && field!(interface)
            && field!(member)
            && field!(error_name)
            && self
                .serial
                .is_none_or(|serial| header.primary().serial_num().get() == serial)
            && self.reply_serial.is_none_or(|serial| {
                header
                    .reply_serial()
                    .is_some_and(|value| value.get() == serial)
            })
            && self.signature.as_deref().is_none_or(|signature| {
                wire_signature(message).is_ok_and(|actual| actual == signature)
            })
    }
}

impl Tap {
    fn deliver(&self, connection: u64, direction: Direction, message: &Message) {
        let Some(console) = self.console.upgrade() else {
            return;
        };
        let Ok(observers) = self.observers.lock() else {
            return;
        };
        let mut body = None;
        for (handler, (owner, rule)) in observers.iter() {
            if !rule.matches(connection, direction, message) {
                continue;
            }
            if !console.notify(*owner, |event| {
                let value = body
                    .get_or_insert_with(|| unpack(message, &mut None).and_then(bounded_json))
                    .as_ref()
                    .map_err(Clone::clone)?;
                Ok(json!({"type":"dbus_event","scope":owner,"event":event,
                    "handler":handler,"direction":direction,"connection":connection,"message":value}))
            }) {
                break;
            }
        }
    }
}

fn introspection<'a>(exports: impl Iterator<Item = &'a Export>, path: &str) -> Option<String> {
    use std::fmt::Write;

    let mut interfaces = BTreeMap::new();
    let mut children = BTreeSet::new();
    for export in exports {
        if export.path == path {
            interfaces.insert(&export.interface, &export.methods);
        } else if let Some(child) = export
            .path
            .strip_prefix(path.trim_end_matches('/'))
            .and_then(|child| child.strip_prefix('/'))
        {
            children.insert(child.split('/').next().unwrap_or(child));
        }
    }
    if interfaces.is_empty() && children.is_empty() {
        return None;
    }
    let mut xml = String::from(
        "<node>\n  <interface name=\"org.freedesktop.DBus.Introspectable\">\n    <method name=\"Introspect\"><arg type=\"s\" direction=\"out\"/></method>\n  </interface>\n",
    );
    for (interface, methods) in interfaces {
        let _ = writeln!(xml, "  <interface name=\"{interface}\">");
        for (name, method) in methods.iter().collect::<BTreeMap<_, _>>() {
            let _ = writeln!(xml, "    <method name=\"{name}\">");
            for (direction, signature) in [("in", &method.input), ("out", &method.output)] {
                if let Ok(Signature::Structure(fields)) = signature_fields(signature) {
                    for field in fields.iter() {
                        let _ = writeln!(
                            xml,
                            "      <arg type=\"{field}\" direction=\"{direction}\"/>"
                        );
                    }
                }
            }
            xml.push_str("    </method>\n");
        }
        xml.push_str("  </interface>\n");
    }
    for child in children {
        let _ = writeln!(xml, "  <node name=\"{child}\"/>");
    }
    xml.push_str("</node>");
    Some(xml)
}

async fn call_message(connection: &Connection, request: &Message) -> Result<Message, String> {
    let serial = request.primary_header().serial_num();
    // The high-level call API converts standard D-Bus errors into Rust
    // errors and discards their wire headers and body. Keep the raw reply.
    let mut replies = zbus::MessageStream::from(connection);
    send_message(connection, request).await?;
    while let Some(reply) = StreamExt::next(&mut replies).await {
        let reply = reply.map_err(display)?;
        if matches!(reply.message_type(), Type::MethodReturn | Type::Error)
            && reply.header().reply_serial() == Some(serial)
        {
            return Ok(reply);
        }
    }
    Err("D-Bus connection closed before the reply".into())
}

async fn connect(address: Address) -> Result<Connection, String> {
    tokio::time::timeout(
        CONNECTION_TIMEOUT,
        Builder::address(address)
            .method_timeout(CALL_TIMEOUT)
            .build(),
    )
    .await
    .map_err(display)?
    .map_err(display)
}

async fn send_error(
    connection: &Connection,
    call: &Message,
    name: &str,
    error: &str,
) -> Result<(), String> {
    let header = call.header();
    if header.primary().flags().contains(Flags::NO_REPLY_EXPECTED) {
        return Ok(());
    }
    let reply = Message::error(&header, name)
        .build(&(error,))
        .map_err(display)?;
    send_message(connection, &reply).await
}

async fn send_message(connection: &Connection, message: &Message) -> Result<(), String> {
    tokio::time::timeout(CALL_TIMEOUT, connection.send(message))
        .await
        .map_err(|_| "D-Bus send timed out")?
        .map_err(display)
}

async fn signal(
    connection: &Connection,
    message: Value,
    descriptors: &Descriptors,
) -> Result<Value, String> {
    let signal: Signal = serde_json::from_value(message).map_err(display)?;
    let mut builder = Message::signal(
        signal.path.as_str(),
        signal.interface.as_str(),
        signal.member.as_str(),
    );
    if let Some(destination) = &signal.destination {
        builder = builder.destination(destination.as_str());
    }
    if let Some(sender) = connection.unique_name() {
        builder = builder.sender(sender);
    }
    let outgoing = build_body(builder, &signal.signature, &signal.body, descriptors)?;
    send_message(connection, &outgoing).await?;
    Ok(Value::Null)
}

fn build_body(
    builder: zbus::message::Builder<'_>,
    signature: &str,
    body: &[Value],
    descriptors: &Descriptors,
) -> Result<Message, String> {
    if signature.is_empty() && body.is_empty() {
        builder.build(&()).map_err(display)
    } else {
        builder
            .build(&pack_body(signature, body, descriptors)?)
            .map_err(display)
    }
}

fn signature_fields(signature: &str) -> Result<Signature, String> {
    if signature.len() > 255 {
        return Err("D-Bus signature exceeds 255 bytes".into());
    }
    if signature.is_empty() {
        Ok(Signature::Unit)
    } else {
        let parsed: Signature = format!("({signature})").parse().map_err(display)?;
        if parsed.to_string_no_parens() != signature {
            return Err("invalid D-Bus body signature".into());
        }
        Ok(parsed)
    }
}

fn bounded_json(value: Value) -> Result<Value, String> {
    if value.to_string().len() > MAX_EVENT_BYTES {
        return Err("D-Bus message exceeds the 2 MiB console limit".into());
    }
    Ok(value)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn pack_body<'a>(
    signature: &str,
    body: &'a [Value],
    descriptors: &Descriptors,
) -> Result<Structure<'a>, String> {
    let Signature::Structure(fields) = signature_fields(signature)? else {
        return Err("D-Bus signature and argument count differ".into());
    };
    pack_fields(fields.iter(), body, descriptors)
}

fn pack_fields<'a, 's>(
    signatures: impl Iterator<Item = &'s Signature>,
    body: &'a [Value],
    descriptors: &Descriptors,
) -> Result<Structure<'a>, String> {
    let signatures: Vec<_> = signatures.collect();
    if signatures.len() != body.len() {
        return Err("D-Bus signature and argument count differ".into());
    }
    let mut builder = StructureBuilder::new();
    for (signature, value) in signatures.into_iter().zip(body) {
        builder.push_value(pack(signature, value, descriptors)?);
    }
    builder.build().map_err(display)
}

fn pack<'a>(
    signature: &Signature,
    value: &'a Value,
    descriptors: &Descriptors,
) -> Result<Variant<'a>, String> {
    let invalid = || format!("invalid D-Bus argument for signature {signature}");
    let text = || value.as_str().ok_or_else(invalid);
    let integer = || value.as_i64().ok_or_else(invalid);
    let list = || value.as_array().ok_or_else(invalid);
    Ok(match signature {
        Signature::U8 => Variant::U8(integer()?.try_into().map_err(display)?),
        Signature::Bool => Variant::Bool(value.as_bool().ok_or_else(invalid)?),
        Signature::I16 => Variant::I16(integer()?.try_into().map_err(display)?),
        Signature::U16 => Variant::U16(integer()?.try_into().map_err(display)?),
        Signature::I32 => Variant::I32(integer()?.try_into().map_err(display)?),
        Signature::U32 => Variant::U32(integer()?.try_into().map_err(display)?),
        Signature::I64 => Variant::I64(text()?.parse().map_err(display)?),
        Signature::U64 => Variant::U64(text()?.parse().map_err(display)?),
        Signature::F64 => Variant::F64(if let Some(value) = value.as_f64() {
            value
        } else {
            match value["double"].as_str() {
                Some("NaN") => f64::NAN,
                Some("Infinity") => f64::INFINITY,
                Some("-Infinity") => f64::NEG_INFINITY,
                Some("-0") => -0.0,
                _ => return Err(invalid()),
            }
        }),
        Signature::Str => {
            let value = text()?;
            if value.contains('\0') {
                return Err("D-Bus strings cannot contain a NUL byte".into());
            }
            Variant::Str(value.into())
        }
        Signature::Signature => {
            let input = text()?;
            signature_fields(input)?;
            let value: Signature = input.parse().map_err(display)?;
            if value.to_string() != input {
                return Err(
                    "D-Bus signature values with multiple top-level types are unsupported".into(),
                );
            }
            Variant::Signature(value)
        }
        Signature::ObjectPath => {
            Variant::ObjectPath(ObjectPath::try_from(text()?).map_err(display)?)
        }
        Signature::Variant => {
            let signature = value["signature"].as_str().ok_or_else(invalid)?;
            let Signature::Structure(fields) = signature_fields(signature)? else {
                return Err("a D-Bus variant needs one complete type".into());
            };
            let mut fields = fields.iter();
            let inner = fields.next().ok_or_else(invalid)?;
            if fields.next().is_some() {
                return Err("a D-Bus variant needs one complete type".into());
            }
            Variant::Variant(Box::new(pack(inner, &value["value"], descriptors)?))
        }
        Signature::Array(element) => {
            let mut array = Array::new(element.signature());
            if element.signature() == &Signature::U8
                && let Some(bytes) = value["bytes"].as_str()
            {
                let bytes = BASE64.decode(bytes).map_err(display)?;
                if bytes.len() > MAX_MESSAGE_BYTES {
                    return Err("D-Bus byte array exceeds 512 KiB".into());
                }
                for byte in bytes {
                    array.append(Variant::U8(byte)).map_err(display)?;
                }
            } else {
                for item in list()? {
                    array
                        .append(pack(element.signature(), item, descriptors)?)
                        .map_err(display)?;
                }
            }
            Variant::Array(array)
        }
        Signature::Dict {
            key,
            value: element,
        } => {
            let mut dict = Dict::new(key.signature(), element.signature());
            if let Some(entries) = value.as_object() {
                if key.signature() != &Signature::Str {
                    return Err(invalid());
                }
                for (name, item) in entries {
                    if name.contains('\0') {
                        return Err("D-Bus strings cannot contain a NUL byte".into());
                    }
                    dict.append(
                        Variant::Str(name.as_str().into()),
                        pack(element.signature(), item, descriptors)?,
                    )
                    .map_err(display)?;
                }
            } else {
                for entry in list()? {
                    let [name, item] = entry.as_array().map(Vec::as_slice).ok_or_else(invalid)?
                    else {
                        return Err(invalid());
                    };
                    dict.append(
                        pack(key.signature(), name, descriptors)?,
                        pack(element.signature(), item, descriptors)?,
                    )
                    .map_err(display)?;
                }
            }
            Variant::Dict(dict)
        }
        Signature::Structure(fields) => {
            Variant::Structure(pack_fields(fields.iter(), list()?, descriptors)?)
        }
        Signature::Fd => Variant::Fd(
            descriptors
                .copy(value["descriptor"].as_u64().ok_or_else(invalid)?)?
                .into(),
        ),
        _ => return Err(format!("unsupported D-Bus argument signature {signature}")),
    })
}

type ReceiveDescriptors<'a, 'store> = Option<&'a mut Received<'store>>;

fn unpack_variant(
    value: &Variant<'_>,
    descriptors: &mut ReceiveDescriptors<'_, '_>,
) -> Result<Value, String> {
    Ok(match value {
        Variant::U8(value) => json!(value),
        Variant::Bool(value) => json!(value),
        Variant::I16(value) => json!(value),
        Variant::U16(value) => json!(value),
        Variant::I32(value) => json!(value),
        Variant::U32(value) => json!(value),
        Variant::I64(value) => json!({"kind":"integer","value":value.to_string()}),
        Variant::U64(value) => json!({"kind":"integer","value":value.to_string()}),
        Variant::F64(value) if value.is_nan() => json!({"kind":"double","value":"NaN"}),
        Variant::F64(value) if *value == f64::INFINITY => {
            json!({"kind":"double","value":"Infinity"})
        }
        Variant::F64(value) if *value == f64::NEG_INFINITY => {
            json!({"kind":"double","value":"-Infinity"})
        }
        Variant::F64(value) if *value == 0.0 && value.is_sign_negative() => {
            json!({"kind":"double","value":"-0"})
        }
        Variant::F64(value) => json!(value),
        Variant::Str(value) => json!(value.as_str()),
        Variant::Signature(value) => json!(value.to_string()),
        Variant::ObjectPath(value) => json!(value.as_str()),
        Variant::Variant(value) => {
            json!({"kind":"variant","signature":value.value_signature().to_string(),
            "value":unpack_variant(value, descriptors)?})
        }
        Variant::Array(array) => {
            if array.element_signature() == &Signature::U8 {
                let bytes = array
                    .inner()
                    .iter()
                    .map(|item| match item {
                        Variant::U8(value) => Ok(*value),
                        _ => Err("invalid D-Bus byte array"),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                json!({"kind":"bytes","value":BASE64.encode(bytes)})
            } else {
                Value::Array(unpack_fields(array.inner(), descriptors)?)
            }
        }
        Variant::Dict(dict) => {
            let entries = dict
                .iter()
                .map(|(key, value)| {
                    Ok([
                        unpack_variant(key, descriptors)?,
                        unpack_variant(value, descriptors)?,
                    ])
                })
                .collect::<Result<Vec<_>, String>>()?;
            let string_keys = matches!(dict.signature(), Signature::Dict { key, .. } if key.signature() == &Signature::Str);
            json!({"kind":"dictionary","stringKeys":string_keys,"value":entries})
        }
        Variant::Structure(value) => Value::Array(unpack_fields(value.fields(), descriptors)?),
        Variant::Fd(fd) => match descriptors {
            Some(received) => received.add(fd.as_fd().try_clone_to_owned().map_err(display)?)?,
            None => json!({"kind":"fdMetadata"}),
        },
    })
}

fn unpack_fields(
    values: &[Variant<'_>],
    descriptors: &mut ReceiveDescriptors<'_, '_>,
) -> Result<Vec<Value>, String> {
    values
        .iter()
        .map(|value| unpack_variant(value, descriptors))
        .collect()
}

fn unpack(
    message: &Message,
    descriptors: &mut ReceiveDescriptors<'_, '_>,
) -> Result<Value, String> {
    if message.data().bytes().len() > MAX_MESSAGE_BYTES {
        return Err("D-Bus message exceeds the 512 KiB decoding limit".into());
    }
    let header = message.header();
    let body = message.body();
    let signature = wire_signature(message)?;
    let values = if body.is_empty() {
        Vec::new()
    } else {
        let (structure, count) = body
            .data()
            .deserialize_for_dynamic_signature::<_, Structure<'_>>(signature_fields(signature)?)
            .map_err(display)?;
        if count != body.len() {
            return Err("D-Bus body length differs from its signature".into());
        }
        unpack_fields(structure.fields(), descriptors)?
    };
    Ok(json!({
        "type":MessageType::from(message.message_type()),
        "sender":header.sender().map(|value| value.as_str()),
        "destination":header.destination().map(|value| value.as_str()),
        "path":header.path().map(|value| value.as_str()),
        "interface":header.interface().map(|value| value.as_str()),
        "member":header.member().map(|value| value.as_str()),
        "serial":header.primary().serial_num().get(),
        "replySerial":header.reply_serial().map(|value| value.get()),
        "noReplyExpected":header.primary().flags().contains(Flags::NO_REPLY_EXPECTED),
        "errorName":header.error_name().map(|value| value.as_str()),
        "unixFds":header.unix_fds().unwrap_or(0),
        "signature":signature,
        "body":values,
    }))
}

// zbus represents `so` and `(so)` with the same Signature when it parses a
// whole message. Keep the wire spelling so one struct argument stays one.
fn wire_signature(message: &Message) -> Result<&str, String> {
    let data = message.data();
    let bytes = data.bytes();
    let size = bytes
        .get(12..16)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("missing D-Bus header fields length")?;
    let end = 16
        + if bytes[0] == b'l' {
            u32::from_le_bytes(size)
        } else {
            u32::from_be_bytes(size)
        } as usize;
    if end > bytes.len() {
        return Err("invalid D-Bus header size".into());
    }
    let mut offset = 16;
    while offset < end {
        let field = bytes[offset];
        offset += 1;
        if field == 8 {
            let value = bytes
                .get(offset..end)
                .ok_or("missing D-Bus signature field")?;
            let [1, b'g', 0, len, ..] = value else {
                return Err("invalid D-Bus signature field".into());
            };
            return std::str::from_utf8(
                value
                    .get(4..4 + *len as usize)
                    .ok_or("truncated D-Bus signature")?,
            )
            .map_err(display);
        }
        let (_, length) = data
            .slice(offset..end)
            .deserialize_for_signature::<_, Variant<'_>>(Signature::Variant)
            .map_err(display)?;
        offset = (offset + length + 7) & !7;
    }
    Ok("")
}
