//! Human selection is shared with the host; model selection belongs to this proxy.
//! Each receive uses the latest trusted input origin of its client and seat.
use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncReadExt, unix::AsyncFd};

#[path = "wayland_drag.rs"]
mod drag;
use drag::{Drag, Offer};

const CONSOLE_CLIPBOARD_BYTES: usize = 1024 * 1024;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);

type Object = (WaylandClientId, u32);
enum Selection {
    App(Object),
    Console(BTreeMap<String, Arc<[u8]>>),
}
impl Selection {
    fn app(&self) -> Option<Object> {
        match self {
            Self::App(owner) => Some(*owner),
            Self::Console(_) => None,
        }
    }
}
struct Source {
    mimes: Vec<String>,
    published: bool,
    actions: Option<u32>,
}
#[derive(Clone)]
struct Device {
    seat: u32,
    selected_host: Option<u32>,
    ready: bool,
}
#[derive(Default)]
pub(super) struct Clipboard {
    sources: HashMap<Object, Source>,
    devices: HashMap<Object, Device>,
    offers: HashMap<Object, u32>,
    host_mimes: HashMap<Object, Vec<String>>,
    private: HashMap<u32, Selection>,
    origins: HashMap<Object, Origin>,
    transfers: Arc<AtomicUsize>,
    drags: HashMap<u32, Drag>,
    drag_offers: HashMap<Object, Offer>,
    drag_positions: HashMap<u32, (WaylandClientId, u32, i32, i32)>,
    host_drag_actions: HashMap<Object, (u32, u32)>,
    host_drag_devices: HashMap<Object, Option<u32>>,
}
impl Clipboard {
    fn reserve_transfer(&self) -> Result<TransferCount, String> {
        if self.transfers.fetch_add(1, Ordering::AcqRel) >= 32 {
            self.transfers.fetch_sub(1, Ordering::AcqRel);
            return Err("data transfer limit exceeded".into());
        }
        Ok(TransferCount(self.transfers.clone()))
    }
    fn origin(&self, client: WaylandClientId, seat: u32) -> Origin {
        self.origins
            .get(&(client, seat))
            .copied()
            .unwrap_or(Origin::Model)
    }
    pub(super) fn switch(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        seat: u32,
        origin: Origin,
    ) -> Result<(), String> {
        let changed = self.origin(client, seat) != origin;
        self.origins.insert((client, seat), origin);
        let devices = self
            .devices
            .iter()
            .filter(|((owner, _), device)| {
                *owner == client && device.seat == seat && (changed || !device.ready)
            })
            .map(|(object, _)| *object)
            .collect::<Vec<_>>();
        for device in devices {
            self.devices.get_mut(&device).unwrap().ready = true;
            self.notify(sessions, device)?;
        }
        Ok(())
    }
    pub(super) fn host_focus(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        seat: u32,
    ) -> Result<(), String> {
        // Mapping and desktop focus changes can occur between injected input
        // and the client's selection request. Seed focus without changing the
        // actor that performed the last input.
        let origin = self
            .origins
            .get(&(client, seat))
            .copied()
            .unwrap_or(Origin::Human);
        self.switch(sessions, client, seat, origin)
    }
    fn emit(
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        object: Object,
        event: GeneratedEvent,
        fds: &[OwnedFd],
    ) -> Result<(), String> {
        let session = sessions
            .get_mut(&object.0)
            .ok_or("clipboard client disconnected")?;
        let encoded = encode_local_event(session, object.1, &event)?;
        if let Some(decoded) = &encoded.decoded {
            apply_client_event_tracking(session, decoded)?;
        }
        session
            .client_event_writer
            .as_ref()
            .ok_or("clipboard event stream unavailable")?
            .send_origin(&encoded.encoded.bytes, fds, Origin::Local)
    }
    fn upstream(
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        bytes: Vec<u8>,
        fds: &[OwnedFd],
    ) -> Result<(), String> {
        let backend = sessions
            .get_mut(&client)
            .and_then(|session| session.backend.as_mut())
            .ok_or("human clipboard requires a host compositor")?;
        backend.forward_raw_request(&WaylandWireMessage {
            bytes,
            fds: duplicate_fds(fds)?,
        })
    }
    fn publish_host_source(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        source: Object,
    ) -> Result<(), String> {
        let info = self.sources.get_mut(&source).ok_or("unknown data source")?;
        if !info.published {
            for mime in &info.mimes {
                Self::upstream(sessions, source.0, string_request(source.1, 0, mime), &[])?;
            }
            if let Some(actions) = info.actions {
                Self::upstream(
                    sessions,
                    source.0,
                    encode_u32_message(source.1, 2, &[actions]),
                    &[],
                )?;
            }
            info.published = true;
        }
        Ok(())
    }
    fn notify(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        device: Object,
    ) -> Result<(), String> {
        let info = self
            .devices
            .get(&device)
            .ok_or("clipboard device disappeared")?
            .clone();
        // Binding a data device is not keyboard/input focus. In particular, Qt
        // installs its listener before QGuiApplication's platform integration
        // exists; unsolicited selection events during that roundtrip crash it.
        if !info.ready {
            return Ok(());
        }
        let mimes = if self.origin(device.0, info.seat) == Origin::Human {
            info.selected_host
                .and_then(|offer| self.host_mimes.get(&(device.0, offer)))
                .cloned()
        } else {
            self.private_mimes(info.seat)
        };
        let Some(mimes) = mimes else {
            return Self::emit(
                sessions,
                device,
                GeneratedEvent::WlDataDeviceSelection { id: None },
                &[],
            );
        };
        if self
            .offers
            .keys()
            .filter(|(client, _)| *client == device.0)
            .count()
            >= 1024
        {
            return Err("clipboard offer limit exceeded".into());
        }
        let offer = sessions
            .get_mut(&device.0)
            .ok_or("clipboard client disappeared")?
            .resource_map
            .allocate_server_id(None)?;
        self.offers.insert((device.0, offer), info.seat);
        Self::emit(
            sessions,
            device,
            GeneratedEvent::WlDataDeviceDataOffer { id: offer },
            &[],
        )?;
        for mime in mimes {
            Self::emit(
                sessions,
                (device.0, offer),
                GeneratedEvent::WlDataOfferOffer {
                    mime_type: Some(mime),
                },
                &[],
            )?;
        }
        Self::emit(
            sessions,
            device,
            GeneratedEvent::WlDataDeviceSelection { id: Some(offer) },
            &[],
        )
    }
    fn notify_private(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        seat: u32,
    ) -> Result<(), String> {
        let devices = self
            .devices
            .iter()
            .filter(|(object, device)| {
                device.seat == seat && self.origin(object.0, seat) != Origin::Human
            })
            .map(|(object, _)| *object)
            .collect::<Vec<_>>();
        let mut result = Ok(());
        for device in devices {
            result = result.and(self.notify(sessions, device));
        }
        result
    }
    fn set_private(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        seat: u32,
        selection: Option<Selection>,
    ) -> Result<(), String> {
        let next_app = selection.as_ref().and_then(Selection::app);
        let old = match selection {
            Some(selection) => self.private.insert(seat, selection),
            None => self.private.remove(&seat),
        };
        let cancelled = if let Some(Selection::App(owner)) = old
            && Some(owner) != next_app
            && sessions.contains_key(&owner.0)
        {
            Self::emit(sessions, owner, GeneratedEvent::WlDataSourceCancelled, &[])
        } else {
            Ok(())
        };
        cancelled.and(self.notify_private(sessions, seat))
    }
    pub(super) fn private_mimes(&self, seat: u32) -> Option<Vec<String>> {
        match self.private.get(&seat)? {
            Selection::App(owner) => self.sources.get(owner).map(|source| source.mimes.clone()),
            Selection::Console(formats) => Some(formats.keys().cloned().collect()),
        }
    }
    pub(super) fn write_private(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        seat: u32,
        formats: BTreeMap<String, Vec<u8>>,
    ) -> Result<(), String> {
        if formats.len() > 64 {
            return Err("clipboard MIME limit exceeded".into());
        }
        if formats.keys().any(|mime| !valid_mime(mime)) {
            return Err("invalid clipboard MIME type".into());
        }
        if formats.values().map(Vec::len).sum::<usize>() > CONSOLE_CLIPBOARD_BYTES {
            return Err("console clipboard payload exceeds 1 MiB".into());
        }
        let selection = (!formats.is_empty()).then(|| {
            Selection::Console(
                formats
                    .into_iter()
                    .map(|(mime, bytes)| (mime, bytes.into()))
                    .collect(),
            )
        });
        self.set_private(sessions, seat, selection)
    }
    pub(super) fn open_private(
        &self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        seat: u32,
        mime: &str,
    ) -> Result<Option<ClipboardRead>, String> {
        match self.private.get(&seat) {
            Some(Selection::Console(formats)) => {
                Ok(formats.get(mime).cloned().map(ClipboardRead::Bytes))
            }
            Some(Selection::App(owner))
                if self
                    .sources
                    .get(owner)
                    .is_some_and(|source| source.mimes.iter().any(|offered| offered == mime)) =>
            {
                let permit = self.reserve_transfer()?;
                let (reader, writer) = clipboard_pipe()?;
                Self::emit(
                    sessions,
                    *owner,
                    GeneratedEvent::WlDataSourceSend {
                        mime_type: Some(mime.into()),
                        fd: true,
                    },
                    &[writer],
                )?;
                Ok(Some(ClipboardRead::Pipe(reader, permit)))
            }
            _ => Ok(None),
        }
    }
    pub(super) fn request(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        request: &DecodedWaylandRequest,
        message: &mut WaylandWireMessage,
    ) -> Result<bool, String> {
        if let Some(consumed) = self.drag_request(sessions, client, request, message)? {
            return Ok(consumed);
        }
        let object = (client, request.object_id);
        match request.hook_request.as_ref() {
            Some(GeneratedHookRequest::WlDataDeviceManagerCreateDataSource { id }) => {
                if self.sources.len() >= 4096 {
                    return Err("clipboard source limit exceeded".into());
                }
                self.sources.insert(
                    (client, *id),
                    Source {
                        mimes: Vec::new(),
                        published: false,
                        actions: None,
                    },
                );
                // Both connections use the client's object IDs. Allocate the
                // empty source upstream so later new IDs do not leave a gap.
                Ok(false)
            }
            Some(GeneratedHookRequest::WlDataDeviceManagerGetDataDevice {
                id,
                seat: Some(seat),
            }) => {
                if self
                    .devices
                    .keys()
                    .filter(|(owner, _)| *owner == client)
                    .count()
                    >= 128
                {
                    return Err("clipboard device limit exceeded".into());
                }
                let session = sessions
                    .get_mut(&client)
                    .ok_or("clipboard client disappeared")?;
                let global = *session
                    .seat_globals
                    .get(seat)
                    .ok_or("clipboard device requires a bound seat")?;
                let version = session
                    .object_versions
                    .get(&request.object_id)
                    .copied()
                    .unwrap_or(1);
                session.track_object_interface_version(*id, "wl_data_device", version);
                self.devices.insert(
                    (client, *id),
                    Device {
                        seat: global,
                        selected_host: None,
                        ready: false,
                    },
                );
                Ok(false)
            }
            Some(GeneratedHookRequest::WlDataSourceOffer {
                mime_type: Some(mime),
            }) => {
                if !valid_mime(mime) {
                    return Err("invalid clipboard MIME type".into());
                }
                let source = self
                    .sources
                    .get_mut(&object)
                    .ok_or("unknown clipboard source")?;
                if !source.mimes.contains(mime) {
                    if source.mimes.len() >= 64 {
                        return Err("clipboard MIME limit exceeded".into());
                    }
                    source.mimes.push(mime.clone());
                }
                // A source's MIME list is published to the host when a human
                // selection is claimed; model metadata never updates it there.
                Ok(true)
            }
            Some(GeneratedHookRequest::WlDataSourceDestroy) => {
                self.sources.remove(&object);
                let seats = self
                    .private
                    .iter()
                    .filter(|(_, selection)| selection.app() == Some(object))
                    .map(|(seat, _)| *seat)
                    .collect::<Vec<_>>();
                for seat in seats {
                    self.private.remove(&seat);
                    self.notify_private(sessions, seat)?;
                }
                Ok(false)
            }
            Some(GeneratedHookRequest::WlDataDeviceSetSelection { source, .. }) => {
                let device = self
                    .devices
                    .get(&object)
                    .ok_or("unknown clipboard device")?
                    .clone();
                let origin = self.origin(client, device.seat);
                if origin == Origin::Human {
                    if let Some(id) = source {
                        self.publish_host_source(sessions, (client, *id))?;
                    }
                    Self::upstream(sessions, client, message.bytes.clone(), &[])?;
                } else {
                    let owner = source.map(|id| (client, id));
                    if owner.is_some_and(|owner| !self.sources.contains_key(&owner)) {
                        return Err("unknown private selection source".into());
                    }
                    self.set_private(sessions, device.seat, owner.map(Selection::App))?;
                }
                Ok(true)
            }
            // Selection offers are local objects. Accept only affects drag
            // negotiation, which has already been handled by drag_request.
            Some(GeneratedHookRequest::WlDataOfferAccept { .. })
                if self.offers.contains_key(&object) =>
            {
                Ok(true)
            }
            Some(GeneratedHookRequest::WlDataOfferReceive {
                mime_type: Some(mime),
                ..
            }) => {
                let seat = *self.offers.get(&object).ok_or("unknown clipboard offer")?;
                let destination = message
                    .fds
                    .pop()
                    .ok_or("clipboard receive missing its FD")?;
                if self.origin(client, seat) == Origin::Human {
                    let offer = self
                        .devices
                        .iter()
                        .filter(|(object, device)| object.0 == client && device.seat == seat)
                        .find_map(|(_, device)| device.selected_host);
                    if let Some(offer) = offer
                        && self
                            .host_mimes
                            .get(&(client, offer))
                            .is_some_and(|mimes| mimes.contains(mime))
                    {
                        Self::upstream(
                            sessions,
                            client,
                            string_request(offer, 1, mime),
                            &[destination],
                        )?;
                    }
                } else if let Some(transfer) = self.open_private(sessions, seat, mime)? {
                    let permit = match transfer {
                        ClipboardRead::Bytes(_) => Some(self.reserve_transfer()?),
                        ClipboardRead::Pipe(..) => None,
                    };
                    tokio::runtime::Handle::try_current()
                        .map_err(|e| e.to_string())?
                        .spawn(async move {
                            let _permit = permit;
                            let _ =
                                tokio::time::timeout(TRANSFER_TIMEOUT, transfer.send(destination))
                                    .await;
                        });
                }
                Ok(true)
            }
            Some(GeneratedHookRequest::WlDataOfferDestroy) => {
                self.offers.remove(&object);
                Ok(true)
            }
            Some(GeneratedHookRequest::WlDataDeviceRelease) => {
                self.devices.remove(&object);
                Ok(false)
            }
            _ => Ok(false),
        }
    }
    pub(super) fn host_event(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        event: &DecodedWaylandEvent,
    ) -> Result<bool, String> {
        if let Some(consumed) = self.drag_host_event(sessions, client, event)? {
            return Ok(consumed);
        }
        match &event.generated_event {
            GeneratedEvent::WlDataDeviceDataOffer { id } => {
                if self.host_mimes.len() >= 4096 {
                    return Err("host clipboard offer limit exceeded".into());
                }
                self.host_mimes.insert((client, *id), Vec::new());
                Ok(true)
            }
            GeneratedEvent::WlDataOfferOffer {
                mime_type: Some(mime),
            } => {
                let mimes = self
                    .host_mimes
                    .get_mut(&(client, event.object_id))
                    .ok_or("host offered MIME before creating an offer")?;
                if mimes.len() >= 64 || mime.len() > 512 {
                    return Err("host clipboard MIME limit exceeded".into());
                }
                mimes.push(mime.clone());
                Ok(true)
            }
            GeneratedEvent::WlDataDeviceSelection { id } => {
                let object = (client, event.object_id);
                if let Some(device) = self.devices.get_mut(&object) {
                    let old = device.selected_host;
                    device.selected_host = *id;
                    let seat = device.seat;
                    if let Some(old) = old
                        && Some(old) != *id
                    {
                        self.host_mimes.remove(&(client, old));
                        Self::upstream(sessions, client, encode_u32_message(old, 2, &[]), &[])?;
                        if let Some(backend) =
                            sessions.get_mut(&client).and_then(|s| s.backend.as_mut())
                        {
                            backend.object_interfaces.remove(&old);
                        }
                    }
                    if self.origin(client, seat) == Origin::Human {
                        self.notify(sessions, object)?;
                    }
                }
                Ok(true)
            }
            _ => Ok(matches!(
                event.interface.as_str(),
                "wl_data_offer" | "wl_data_device"
            )),
        }
    }
    pub(super) fn disconnect(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
    ) {
        let seats = self
            .drags
            .iter()
            .filter(|(_, d)| {
                d.source.is_some_and(|s| s.0 == client) || d.target.is_some_and(|s| s.0 == client)
            })
            .map(|(seat, _)| *seat)
            .collect::<Vec<_>>();
        for seat in seats {
            let _ = self.cancel_drag(sessions, seat);
        }
        self.drag_offers.retain(|(owner, _), _| *owner != client);
        self.host_drag_devices
            .retain(|(owner, _), _| *owner != client);
        self.host_drag_actions
            .retain(|(owner, _), _| *owner != client);
        self.drag_positions
            .retain(|_, (owner, _, _, _)| *owner != client);
        self.sources.retain(|(owner, _), _| *owner != client);
        self.devices.retain(|(owner, _), _| *owner != client);
        self.offers.retain(|(owner, _), _| *owner != client);
        self.host_mimes.retain(|(owner, _), _| *owner != client);
        self.origins.retain(|(owner, _), _| *owner != client);
        let seats = self
            .private
            .iter()
            .filter(|(_, selection)| selection.app().is_some_and(|owner| owner.0 == client))
            .map(|(seat, _)| *seat)
            .collect::<Vec<_>>();
        for seat in seats {
            self.private.remove(&seat);
            let _ = self.notify_private(sessions, seat);
        }
    }
}
pub(super) struct TransferCount(Arc<AtomicUsize>);
impl Drop for TransferCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(super) enum ClipboardRead {
    Bytes(Arc<[u8]>),
    Pipe(OwnedFd, TransferCount),
}
impl ClipboardRead {
    pub(super) async fn read(self) -> Result<Arc<[u8]>, String> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            Self::Pipe(reader, _permit) => {
                let reader = tokio::net::unix::pipe::Receiver::from_owned_fd(reader)
                    .map_err(|error| error.to_string())?;
                let mut bytes = Vec::new();
                tokio::time::timeout(
                    TRANSFER_TIMEOUT,
                    reader
                        .take(CONSOLE_CLIPBOARD_BYTES as u64 + 1)
                        .read_to_end(&mut bytes),
                )
                .await
                .map_err(|_| "clipboard read timed out")?
                .map_err(|error| error.to_string())?;
                if bytes.len() > CONSOLE_CLIPBOARD_BYTES {
                    return Err("console clipboard payload exceeds 1 MiB".into());
                }
                Ok(bytes.into())
            }
        }
    }
    async fn send(self, destination: OwnedFd) -> Result<(), String> {
        match self {
            Self::Bytes(bytes) => write_clipboard(&clipboard_async_fd(destination)?, &bytes).await,
            Self::Pipe(reader, _permit) => pump_clipboard(reader, destination).await,
        }
    }
}
fn valid_mime(mime: &str) -> bool {
    !mime.is_empty() && mime.len() <= 512 && !mime.contains('\0')
}
fn string_request(object: u32, opcode: u16, value: &str) -> Vec<u8> {
    let mut payload = vec![(value.len() + 1) as u32];
    let mut string = value.as_bytes().to_vec();
    string.push(0);
    string.resize(pad_to_4(string.len()), 0);
    payload.extend(
        string
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_ne_bytes(*bytes)),
    );
    encode_u32_message(object, opcode, &payload)
}
fn clipboard_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
fn clipboard_async_fd(fd: OwnedFd) -> Result<AsyncFd<OwnedFd>, String> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err("clipboard FD setup failed".into());
    }
    // OwnedFd keeps the same descriptor open until AsyncFd deregisters it.
    unsafe { AsyncFd::register(fd) }.map_err(|e| e.to_string())
}
async fn pump_clipboard(reader: OwnedFd, writer: OwnedFd) -> Result<(), String> {
    let reader = clipboard_async_fd(reader)?;
    let writer = clipboard_async_fd(writer)?;
    let mut buffer = [0u8; 16384];
    let mut total = 0usize;
    loop {
        let n = loop {
            let mut ready = reader.readable().await.map_err(|e| e.to_string())?;
            match ready.try_io(|fd| {
                let n =
                    unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(n) => break n.map_err(|e| e.to_string())?,
                Err(_) => continue,
            }
        };
        if n == 0 {
            return Ok(());
        }
        total += n;
        if total > 64 * 1024 * 1024 {
            return Err("clipboard transfer exceeded 64 MiB".into());
        }
        write_clipboard(&writer, &buffer[..n]).await?;
    }
}
async fn write_clipboard(writer: &AsyncFd<OwnedFd>, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        let mut ready = writer.writable().await.map_err(|e| e.to_string())?;
        if let Ok(sent) = ready.try_io(|fd| {
            let sent = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if sent < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(sent as usize)
            }
        }) {
            let sent = sent.map_err(|e| e.to_string())?;
            if sent == 0 {
                return Err("clipboard destination stopped accepting data".into());
            }
            bytes = &bytes[sent..];
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn fixture(
        server: &mut WaylandProxyServer,
        client: WaylandClientId,
    ) -> (StdUnixStream, StdUnixStream) {
        let (host, upstream) = StdUnixStream::pair().unwrap();
        let (events, reader) = StdUnixStream::pair().unwrap();
        for stream in [&host, &reader] {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
        }
        let interfaces = HashMap::from([
            (1, "wl_display".into()),
            (2, "wl_data_device_manager".into()),
            (3, "wl_seat".into()),
        ]);
        let backend = WaylandBackendSession {
            stream: upstream,
            globals: vec![],
            object_interfaces: interfaces.clone(),
            pending_backend_fds: VecDeque::new(),
            retired_client_objects: HashSet::new(),
        };
        let mut session = WaylandClientSession::new(client, vec![], Some(backend));
        for (id, interface) in interfaces {
            session.track_object_interface_version(id, &interface, 3);
        }
        session.seat_globals.insert(3, 7);
        session.client_event_writer = Some(ClientWriter::new(events));
        server.sessions.insert(client, session);
        (host, reader)
    }
    fn request(
        server: &mut WaylandProxyServer,
        client: WaylandClientId,
        bytes: Vec<u8>,
        fds: Vec<OwnedFd>,
    ) -> Result<(), String> {
        server.ingest_request(client, WaylandWireMessage { bytes, fds })?;
        Ok(())
    }
    fn next(stream: &StdUnixStream) -> WaylandWireMessage {
        read_wayland_wire_message_from_fd(stream.as_raw_fd(), 16, 0).unwrap()
    }
    fn selection(stream: &StdUnixStream, mimes: &[&str]) -> u32 {
        let offer = next(stream);
        assert_eq!(decode_wayland_header(&offer.bytes).unwrap().opcode, 0);
        let id = u32::from_ne_bytes(offer.bytes[8..12].try_into().unwrap());
        for mime in mimes {
            assert_eq!(next(stream).bytes, string_request(id, 0, mime));
        }
        let selection = next(stream);
        assert_eq!(
            u32::from_ne_bytes(selection.bytes[8..12].try_into().unwrap()),
            id
        );
        id
    }
    fn host_event(
        server: &mut WaylandProxyServer,
        client: WaylandClientId,
        object: u32,
        event: GeneratedEvent,
    ) -> Result<(), String> {
        let event = server.prepare_backend_event(
            client,
            WaylandWireMessage {
                bytes: encode_generated_event(object, &event)?,
                fds: vec![],
            },
        )?;
        assert!(event.suppressed);
        Ok(())
    }

    fn keyboard_event(
        server: &mut WaylandProxyServer,
        client: WaylandClientId,
        event: &GeneratedEvent,
    ) -> Result<(), String> {
        let event = server.prepare_backend_event(
            client,
            WaylandWireMessage {
                bytes: encode_generated_event(19, event)?,
                fds: vec![],
            },
        )?;
        assert!(!event.suppressed);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_clipboards_route_real_fd_transfers_by_latest_source() -> Result<(), String> {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test".into(),
            backend_socket: "test".into(),
        });
        let owner = WaylandClientId(1);
        let receiver = WaylandClientId(2);
        let (host_owner, owner_events) = fixture(&mut server, owner);
        let (host_receiver, receiver_events) = fixture(&mut server, receiver);
        for (client, host, events) in [
            (owner, &host_owner, &owner_events),
            (receiver, &host_receiver, &receiver_events),
        ] {
            request(
                &mut server,
                client,
                encode_u32_message(2, 1, &[4, 3]),
                vec![],
            )?;
            assert_eq!(next(host).bytes, encode_u32_message(2, 1, &[4, 3]));
            let mut byte = 0u8;
            assert_eq!(
                unsafe {
                    libc::recv(
                        events.as_raw_fd(),
                        (&mut byte as *mut u8).cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                },
                -1,
                "no selection event before trusted focus/input"
            );
            server
                .clipboard
                .switch(&mut server.sessions, client, 7, Origin::Model)?;
            assert_eq!(next(events).bytes, encode_u32_message(4, 5, &[0]));
        }
        request(&mut server, owner, encode_u32_message(2, 0, &[5]), vec![])?;
        assert_eq!(next(&host_owner).bytes, encode_u32_message(2, 0, &[5]));
        request(
            &mut server,
            owner,
            string_request(5, 0, "text/plain"),
            vec![],
        )?;
        request(
            &mut server,
            owner,
            encode_u32_message(4, 1, &[5, 123]),
            vec![],
        )?;
        let _owner_offer = selection(&owner_events, &["text/plain"]);
        let private_offer = selection(&receiver_events, &["text/plain"]);
        let host_id = 0xff000000;
        host_event(
            &mut server,
            receiver,
            4,
            GeneratedEvent::WlDataDeviceDataOffer { id: host_id },
        )?;
        host_event(
            &mut server,
            receiver,
            host_id,
            GeneratedEvent::WlDataOfferOffer {
                mime_type: Some("text/plain".into()),
            },
        )?;
        host_event(
            &mut server,
            receiver,
            4,
            GeneratedEvent::WlDataDeviceSelection { id: Some(host_id) },
        )?;
        assert_eq!(
            server.sessions[&receiver].resource_map.next_server_id,
            private_offer + 1
        );
        let payload = vec![0x5a; 256 * 1024];
        let expected = payload.clone();
        let (read, write) = clipboard_pipe()?;
        request(
            &mut server,
            receiver,
            encode_u32_message(private_offer, 0, &[123, 0]),
            vec![],
        )?;
        request(
            &mut server,
            receiver,
            string_request(private_offer, 1, "text/plain"),
            vec![write],
        )?;
        let mut send = next(&owner_events);
        assert_eq!(decode_wayland_header(&send.bytes)?.object_id, 5);
        assert_eq!(send.fds.len(), 1);
        let source_fd = send.fds.pop().unwrap();
        let producer =
            tokio::task::spawn_blocking(move || File::from(source_fd).write_all(&payload));
        let bytes = tokio::task::spawn_blocking(move || {
            let mut bytes = vec![];
            File::from(read).read_to_end(&mut bytes).unwrap();
            bytes
        })
        .await
        .unwrap();
        producer.await.unwrap().unwrap();
        assert_eq!(bytes, expected);
        server
            .clipboard
            .switch(&mut server.sessions, receiver, 7, Origin::Human)?;
        let human_offer = selection(&receiver_events, &["text/plain"]);
        assert!(human_offer > private_offer);
        let (read, write) = clipboard_pipe()?;
        // An older offer follows the current source too.
        request(
            &mut server,
            receiver,
            string_request(private_offer, 1, "text/plain"),
            vec![write],
        )?;
        let mut receive = next(&host_receiver);
        assert_eq!(receive.bytes, string_request(host_id, 1, "text/plain"));
        File::from(receive.fds.pop().unwrap())
            .write_all(b"host sentinel")
            .unwrap();
        let mut value = vec![];
        File::from(read).read_to_end(&mut value).unwrap();
        assert_eq!(value, b"host sentinel");
        let transfer = server
            .clipboard
            .open_private(&mut server.sessions, 7, "text/plain")?
            .unwrap();
        let mut send = next(&owner_events);
        assert_eq!(decode_wayland_header(&send.bytes)?.object_id, 5);
        File::from(send.fds.pop().unwrap())
            .write_all(b"app copied text")
            .unwrap();
        assert_eq!(&*transfer.read().await?, b"app copied text");
        server
            .clipboard
            .switch(&mut server.sessions, receiver, 7, Origin::Model)?;
        let model_offer = selection(&receiver_events, &["text/plain"]);
        assert!(model_offer > human_offer);
        assert_eq!(
            server.clipboard.private.get(&7).and_then(Selection::app),
            Some((owner, 5))
        );
        assert_eq!(
            server.clipboard.devices[&(receiver, 4)].selected_host,
            Some(host_id)
        );
        server.remove_client_session(owner);
        assert_eq!(next(&receiver_events).bytes, encode_u32_message(4, 5, &[0]));
        assert!(server.clipboard.private_mimes(7).is_none());

        let html = b"<b>private HTML</b>";
        server.clipboard.write_private(
            &mut server.sessions,
            7,
            BTreeMap::from([
                ("text/html".into(), html.to_vec()),
                ("text/plain".into(), b"private HTML".to_vec()),
            ]),
        )?;
        let offer = selection(&receiver_events, &["text/html", "text/plain"]);
        let (read, write) = clipboard_pipe()?;
        request(
            &mut server,
            receiver,
            string_request(offer, 1, "text/html"),
            vec![write],
        )?;
        // A transfer already requested by the app retains the old selection.
        server
            .clipboard
            .write_private(&mut server.sessions, 7, BTreeMap::new())?;
        assert_eq!(next(&receiver_events).bytes, encode_u32_message(4, 5, &[0]));
        let mut bytes = Vec::new();
        tokio::net::unix::pipe::Receiver::from_owned_fd(read)
            .unwrap()
            .read_to_end(&mut bytes)
            .await
            .unwrap();
        assert_eq!(bytes, html);
        assert!(
            server
                .clipboard
                .open_private(&mut server.sessions, 7, "text/html")?
                .is_none()
        );
        assert_eq!(
            server.clipboard.devices[&(receiver, 4)].selected_host,
            Some(host_id)
        );
        Ok(())
    }

    #[tokio::test]
    async fn private_source_keeps_ids_in_sync_without_publishing_selection() -> Result<(), String> {
        let mut server = WaylandProxyServer::new(WaylandProxyConfig {
            dmabuf_transparent: false,
            socket_name: "test".into(),
            backend_socket: "test".into(),
        });
        let client = WaylandClientId(1);
        let (host, events) = fixture(&mut server, client);
        request(
            &mut server,
            client,
            encode_u32_message(2, 1, &[4, 3]),
            vec![],
        )?;
        let _ = next(&host);
        let session = server.sessions.get_mut(&client).unwrap();
        for (id, interface) in [(19, "wl_keyboard"), (20, "wl_surface")] {
            session.track_object_interface_version(id, interface, 3);
            session
                .backend
                .as_mut()
                .unwrap()
                .object_interfaces
                .insert(id, interface.into());
        }
        session.input_seats.insert(19, 3);
        let focus = GeneratedEvent::WlKeyboardEnter {
            serial: 120,
            surface: Some(20),
            keys: vec![],
        };
        keyboard_event(&mut server, client, &focus)?;
        assert_eq!(server.clipboard.origin(client, 7), Origin::Human);
        let _ = next(&events);
        server
            .clipboard
            .switch(&mut server.sessions, client, 7, Origin::Model)?;
        let _ = next(&events);
        keyboard_event(&mut server, client, &focus)?;
        assert_eq!(server.clipboard.origin(client, 7), Origin::Model);
        request(&mut server, client, encode_u32_message(2, 0, &[5]), vec![])?;
        request(
            &mut server,
            client,
            string_request(5, 0, "text/plain"),
            vec![],
        )?;
        request(
            &mut server,
            client,
            encode_u32_message(4, 1, &[5, 123]),
            vec![],
        )?;
        let _ = selection(&events, &["text/plain"]);
        request(&mut server, client, encode_u32_message(1, 0, &[6]), vec![])?;
        assert_eq!(next(&host).bytes, encode_u32_message(2, 0, &[5]));
        assert_eq!(next(&host).bytes, encode_u32_message(1, 0, &[6]));
        keyboard_event(
            &mut server,
            client,
            &GeneratedEvent::WlKeyboardKey {
                serial: 121,
                time: 10,
                key: 30,
                state: 1,
            },
        )?;
        assert_eq!(server.clipboard.origin(client, 7), Origin::Human);
        let _ = next(&events);
        request(&mut server, client, encode_u32_message(2, 0, &[7]), vec![])?;
        request(
            &mut server,
            client,
            string_request(7, 0, "text/plain"),
            vec![],
        )?;
        request(
            &mut server,
            client,
            encode_u32_message(4, 1, &[7, 456]),
            vec![],
        )?;
        assert_eq!(next(&host).bytes, encode_u32_message(2, 0, &[7]));
        assert_eq!(next(&host).bytes, string_request(7, 0, "text/plain"));
        assert_eq!(next(&host).bytes, encode_u32_message(4, 1, &[7, 456]));
        assert_eq!(
            server.clipboard.private.get(&7).and_then(Selection::app),
            Some((client, 5))
        );
        request(&mut server, client, encode_u32_message(5, 1, &[]), vec![])?;
        assert_eq!(next(&host).bytes, encode_u32_message(5, 1, &[]));
        let deletion = encode_u32_message(1, 1, &[5]);
        let event = server.prepare_backend_event(
            client,
            WaylandWireMessage {
                bytes: deletion.clone(),
                fds: vec![],
            },
        )?;
        assert!(!event.suppressed);
        assert_eq!(event.encoded.bytes, deletion);
        assert!(!server.sessions[&client].object_interfaces.contains_key(&5));
        Ok(())
    }
}
