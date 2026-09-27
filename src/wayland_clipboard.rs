//! Human selection is shared with the host; model selection belongs to this proxy.
//! Each receive uses the latest trusted input origin of its client and seat.
use super::*;
use std::sync::atomic::AtomicUsize;

#[path = "wayland_drag.rs"]
mod drag;
use drag::{Drag, Offer};

type Object = (WaylandClientId, u32);
#[derive(Clone)]
struct Source {
    manager: u32,
    mimes: Vec<String>,
    upstream: bool,
    actions: Option<u32>,
}
#[derive(Clone)]
struct Device {
    seat: u32,
    selected_host: Option<u32>,
}
#[derive(Default)]
pub(super) struct Clipboard {
    sources: HashMap<Object, Source>,
    devices: HashMap<Object, Device>,
    offers: HashMap<Object, u32>,
    host_mimes: HashMap<Object, Vec<String>>,
    private: HashMap<u32, Object>,
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
        if self.origin(client, seat) == origin {
            return Ok(());
        }
        self.origins.insert((client, seat), origin);
        let devices = self
            .devices
            .iter()
            .filter(|((owner, _), device)| *owner == client && device.seat == seat)
            .map(|(object, _)| *object)
            .collect::<Vec<_>>();
        for device in devices {
            self.notify(sessions, device)?;
        }
        Ok(())
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
        let mimes = if self.origin(device.0, info.seat) == Origin::Human {
            info.selected_host
                .and_then(|offer| self.host_mimes.get(&(device.0, offer)))
                .cloned()
        } else {
            self.private
                .get(&info.seat)
                .and_then(|source| self.sources.get(source))
                .map(|source| source.mimes.clone())
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
        for device in devices {
            self.notify(sessions, device)?;
        }
        Ok(())
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
                        manager: request.object_id,
                        mimes: Vec::new(),
                        upstream: false,
                        actions: None,
                    },
                );
                Ok(true)
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
                    },
                );
                self.notify(sessions, (client, *id))?;
                Ok(false)
            }
            Some(GeneratedHookRequest::WlDataSourceOffer {
                mime_type: Some(mime),
            }) => {
                if mime.is_empty() || mime.len() > 512 {
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
                if let Some(source) = self.sources.remove(&object) {
                    if source.upstream {
                        Self::upstream(sessions, client, message.bytes.clone(), &[])?;
                    } else {
                        Self::emit(
                            sessions,
                            (client, 1),
                            GeneratedEvent::WlDisplayDeleteId {
                                id: request.object_id,
                            },
                            &[],
                        )?;
                    }
                }
                let seats = self
                    .private
                    .iter()
                    .filter(|(_, owner)| **owner == object)
                    .map(|(seat, _)| *seat)
                    .collect::<Vec<_>>();
                for seat in seats {
                    self.private.remove(&seat);
                    self.notify_private(sessions, seat)?;
                }
                Ok(true)
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
                        let source = self
                            .sources
                            .get_mut(&(client, *id))
                            .ok_or("unknown selection source")?;
                        if !source.upstream {
                            Self::upstream(
                                sessions,
                                client,
                                encode_u32_message(source.manager, 0, &[*id]),
                                &[],
                            )?;
                            for mime in &source.mimes {
                                Self::upstream(
                                    sessions,
                                    client,
                                    string_request(*id, 0, mime),
                                    &[],
                                )?;
                            }
                            sessions
                                .get_mut(&client)
                                .and_then(|s| s.backend.as_mut())
                                .ok_or("host compositor unavailable")?
                                .object_interfaces
                                .insert(*id, "wl_data_source".into());
                            source.upstream = true;
                        }
                    }
                    Self::upstream(sessions, client, message.bytes.clone(), &[])?;
                } else {
                    let old = self.private.remove(&device.seat);
                    if let Some(id) = source {
                        if !self.sources.contains_key(&(client, *id)) {
                            return Err("unknown private selection source".into());
                        }
                        self.private.insert(device.seat, (client, *id));
                    }
                    if let Some(old) = old
                        && Some(old) != source.map(|id| (client, id))
                        && sessions.contains_key(&old.0)
                    {
                        Self::emit(sessions, old, GeneratedEvent::WlDataSourceCancelled, &[])?;
                    }
                    self.notify_private(sessions, device.seat)?;
                }
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
                } else if let Some(owner) = self.private.get(&seat).copied()
                    && self
                        .sources
                        .get(&owner)
                        .is_some_and(|source| source.mimes.contains(mime))
                {
                    if self.transfers.fetch_add(1, Ordering::AcqRel) >= 32 {
                        self.transfers.fetch_sub(1, Ordering::AcqRel);
                        return Err("clipboard transfer limit exceeded".into());
                    }
                    let count = TransferCount(self.transfers.clone());
                    let (reader, writer) = clipboard_pipe()?;
                    Self::emit(
                        sessions,
                        owner,
                        GeneratedEvent::WlDataSourceSend {
                            mime_type: Some(mime.clone()),
                            fd: true,
                        },
                        &[writer],
                    )?;
                    tokio::runtime::Handle::try_current()
                        .map_err(|e| e.to_string())?
                        .spawn(async move {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(10),
                                pump_clipboard(reader, destination),
                            )
                            .await;
                            drop(count);
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
            .filter(|(_, owner)| owner.0 == client)
            .map(|(seat, _)| *seat)
            .collect::<Vec<_>>();
        for seat in seats {
            self.private.remove(&seat);
            let _ = self.notify_private(sessions, seat);
        }
    }
}
struct TransferCount(Arc<AtomicUsize>);
impl Drop for TransferCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
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
async fn pump_clipboard(reader: OwnedFd, writer: OwnedFd) -> Result<(), String> {
    use tokio::io::unix::AsyncFd;
    for fd in [&reader, &writer] {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err("clipboard FD setup failed".into());
        }
    }
    let reader = AsyncFd::new(reader).map_err(|e| e.to_string())?;
    let writer = AsyncFd::new(writer).map_err(|e| e.to_string())?;
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
        let mut offset = 0;
        while offset < n {
            let mut ready = writer.writable().await.map_err(|e| e.to_string())?;
            match ready.try_io(|fd| {
                let sent = unsafe {
                    libc::write(
                        fd.as_raw_fd(),
                        buffer[offset..n].as_ptr().cast(),
                        n - offset,
                    )
                };
                if sent < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(sent as usize)
                }
            }) {
                Ok(sent) => {
                    let sent = sent.map_err(|e| e.to_string())?;
                    if sent == 0 {
                        return Err("clipboard destination stopped accepting data".into());
                    }
                    offset += sent;
                }
                Err(_) => continue,
            }
        }
    }
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
    fn selection(stream: &StdUnixStream) -> u32 {
        let offer = next(stream);
        assert_eq!(decode_wayland_header(&offer.bytes).unwrap().opcode, 0);
        let id = u32::from_ne_bytes(offer.bytes[8..12].try_into().unwrap());
        let mime = next(stream);
        assert_eq!(decode_wayland_header(&mime.bytes).unwrap().object_id, id);
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
            assert_eq!(next(events).bytes, encode_u32_message(4, 5, &[0]));
        }
        request(&mut server, owner, encode_u32_message(2, 0, &[5]), vec![])?;
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
        let _owner_offer = selection(&owner_events);
        let private_offer = selection(&receiver_events);
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
        let human_offer = selection(&receiver_events);
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
        server
            .clipboard
            .switch(&mut server.sessions, receiver, 7, Origin::Model)?;
        let model_offer = selection(&receiver_events);
        assert!(model_offer > human_offer);
        assert_eq!(server.clipboard.private.get(&7), Some(&(owner, 5)));
        assert_eq!(
            server.clipboard.devices[&(receiver, 4)].selected_host,
            Some(host_id)
        );
        server.remove_client_session(owner);
        assert_eq!(next(&receiver_events).bytes, encode_u32_message(4, 5, &[0]));
        assert!(!server.clipboard.private.contains_key(&7));
        Ok(())
    }

    #[tokio::test]
    async fn human_copy_only_creates_host_source_when_human_is_current() -> Result<(), String> {
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
        let _ = next(&events);
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
        let _ = selection(&events);
        server
            .clipboard
            .switch(&mut server.sessions, client, 7, Origin::Human)?;
        let _ = next(&events);
        request(&mut server, client, encode_u32_message(2, 0, &[6]), vec![])?;
        request(
            &mut server,
            client,
            string_request(6, 0, "text/plain"),
            vec![],
        )?;
        request(
            &mut server,
            client,
            encode_u32_message(4, 1, &[6, 456]),
            vec![],
        )?;
        assert_eq!(next(&host).bytes, encode_u32_message(2, 0, &[6]));
        assert_eq!(next(&host).bytes, string_request(6, 0, "text/plain"));
        assert_eq!(next(&host).bytes, encode_u32_message(4, 1, &[6, 456]));
        assert_eq!(server.clipboard.private.get(&7), Some(&(client, 5)));
        request(&mut server, client, encode_u32_message(5, 1, &[]), vec![])?;
        assert_eq!(next(&events).bytes, encode_u32_message(1, 1, &[5]));
        assert!(!server.sessions[&client].object_interfaces.contains_key(&5));
        Ok(())
    }
}
