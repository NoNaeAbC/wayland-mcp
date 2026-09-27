//! Drag transfers share the bounded clipboard pump, but never selection ownership.
use super::*;
#[derive(Clone)]
pub(super) enum Route {
    Private(Object),
    Host(u32),
}
#[derive(Clone)]
pub(super) struct Offer {
    pub seat: u32,
    pub route: Route,
    pub mime: Option<String>,
    pub actions: u32,
    pub dropped: bool,
}
#[derive(Clone)]
pub(super) struct Drag {
    pub source: Option<Object>,
    pub target: Option<Object>,
    pub offer: Option<u32>,
    pub dropped: bool,
}
impl Clipboard {
    fn source_actions(
        &self,
        sessions: &HashMap<WaylandClientId, WaylandClientSession>,
        source: Object,
    ) -> u32 {
        if sessions
            .get(&source.0)
            .and_then(|s| s.object_versions.get(&source.1))
            .copied()
            .unwrap_or(1)
            < 3
        {
            1
        } else {
            self.sources
                .get(&source)
                .and_then(|s| s.actions)
                .unwrap_or(0)
        }
    }
    fn source_event(
        &self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        source: Object,
        event: GeneratedEvent,
    ) -> Result<(), String> {
        if sessions
            .get(&source.0)
            .and_then(|s| s.object_versions.get(&source.1))
            .copied()
            .unwrap_or(1)
            >= 3
        {
            Self::emit(sessions, source, event, &[])?;
        }
        Ok(())
    }
    fn new_drag_offer(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        device: Object,
        seat: u32,
        route: Route,
        mimes: Vec<String>,
        actions: u32,
    ) -> Result<u32, String> {
        if self
            .offers
            .keys()
            .filter(|(client, _)| *client == device.0)
            .count()
            >= 1024
        {
            return Err("data offer limit exceeded".into());
        }
        let id = sessions
            .get_mut(&device.0)
            .ok_or("drag target disappeared")?
            .resource_map
            .allocate_server_id(None)?;
        self.offers.insert((device.0, id), seat);
        self.drag_offers.insert(
            (device.0, id),
            Offer {
                seat,
                route,
                mime: None,
                actions: 0,
                dropped: false,
            },
        );
        Self::emit(
            sessions,
            device,
            GeneratedEvent::WlDataDeviceDataOffer { id },
            &[],
        )?;
        for mime in mimes {
            Self::emit(
                sessions,
                (device.0, id),
                GeneratedEvent::WlDataOfferOffer {
                    mime_type: Some(mime),
                },
                &[],
            )?;
        }
        if sessions
            .get(&device.0)
            .and_then(|s| s.object_versions.get(&device.1))
            .copied()
            .unwrap_or(1)
            >= 3
        {
            Self::emit(
                sessions,
                (device.0, id),
                GeneratedEvent::WlDataOfferSourceActions {
                    source_actions: actions,
                },
                &[],
            )?;
        }
        Ok(id)
    }
    fn ensure_host_source(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        source: Object,
    ) -> Result<(), String> {
        let info = self
            .sources
            .get(&source)
            .ok_or("unknown drag source")?
            .clone();
        if !info.upstream {
            Self::upstream(
                sessions,
                source.0,
                encode_u32_message(info.manager, 0, &[source.1]),
                &[],
            )?;
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
            sessions
                .get_mut(&source.0)
                .and_then(|s| s.backend.as_mut())
                .ok_or("host compositor unavailable")?
                .object_interfaces
                .insert(source.1, "wl_data_source".into());
            self.sources.get_mut(&source).unwrap().upstream = true;
        }
        Ok(())
    }
    pub(super) fn cancel_drag(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        seat: u32,
    ) -> Result<(), String> {
        if let Some(drag) = self.drags.remove(&seat) {
            if let Some(target) = drag.target
                && sessions.contains_key(&target.0)
            {
                Self::emit(sessions, target, GeneratedEvent::WlDataDeviceLeave, &[])?;
            }
            if let Some(source) = drag.source
                && self.sources.contains_key(&source)
                && sessions.contains_key(&source.0)
            {
                Self::emit(sessions, source, GeneratedEvent::WlDataSourceCancelled, &[])?;
            }
        }
        self.drag_offers
            .retain(|_, offer| offer.seat != seat || matches!(offer.route, Route::Host(_)));
        Ok(())
    }
    pub(in crate::gui_backend_wayland) fn model_input(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        seat: u32,
        surface: u32,
        event: &serde_json::Value,
    ) -> Result<(), String> {
        let kind = event["type"].as_str().unwrap_or("");
        if matches!(kind, "enter" | "motion") {
            let x = event["x"].as_i64().unwrap_or(0) as i32;
            let y = event["y"].as_i64().unwrap_or(0) as i32;
            self.drag_positions.insert(seat, (client, surface, x, y));
            let Some(mut drag) = self.drags.get(&seat).cloned() else {
                return Ok(());
            };
            if drag.dropped {
                return Ok(());
            }
            let device = self
                .devices
                .iter()
                .filter(|((owner, _), d)| *owner == client && d.seat == seat)
                .map(|(object, _)| *object)
                .min_by_key(|(_, id)| *id);
            if device != drag.target {
                if let Some(old) = drag.target {
                    Self::emit(sessions, old, GeneratedEvent::WlDataDeviceLeave, &[])?;
                }
                drag.offer = None;
                drag.target = device;
                if let Some(device) = device {
                    if let Some(source) = drag.source {
                        let mimes = self
                            .sources
                            .get(&source)
                            .ok_or("drag source disappeared")?
                            .mimes
                            .clone();
                        let actions = self.source_actions(sessions, source);
                        drag.offer = Some(self.new_drag_offer(
                            sessions,
                            device,
                            seat,
                            Route::Private(source),
                            mimes,
                            actions,
                        )?);
                    }
                    let serial = sessions
                        .get_mut(&client)
                        .ok_or("drag target disappeared")?
                        .next_synthetic_serial();
                    Self::emit(
                        sessions,
                        device,
                        GeneratedEvent::WlDataDeviceEnter {
                            serial,
                            surface: Some(surface),
                            x,
                            y,
                            id: drag.offer,
                        },
                        &[],
                    )?;
                }
            }
            if let Some(device) = device {
                Self::emit(
                    sessions,
                    device,
                    GeneratedEvent::WlDataDeviceMotion {
                        time: event["time"]
                            .as_u64()
                            .unwrap_or(u64::from(wayland_timestamp_ms_u32()))
                            as u32,
                        x,
                        y,
                    },
                    &[],
                )?;
            }
            self.drags.insert(seat, drag);
        } else if kind == "button" && event["state"] == 0 {
            let Some(mut drag) = self.drags.get(&seat).cloned() else {
                return Ok(());
            };
            if drag.dropped {
                return Ok(());
            }
            let accepted = drag
                .target
                .zip(drag.offer)
                .and_then(|(target, id)| self.drag_offers.get(&(target.0, id)))
                .is_some_and(|o| o.mime.is_some() && o.actions != 0);
            if !accepted && drag.source.is_some() {
                return self.cancel_drag(sessions, seat);
            }
            if let Some(target) = drag.target {
                if let Some(source) = drag.source {
                    self.source_event(
                        sessions,
                        source,
                        GeneratedEvent::WlDataSourceDndDropPerformed,
                    )?;
                }
                trace_wayland_proxy(format_args!(
                    "private drag dropped: source={:?} target={}",
                    drag.source, target.0.0
                ));
                Self::emit(sessions, target, GeneratedEvent::WlDataDeviceDrop, &[])?;
                if let Some(id) = drag.offer
                    && let Some(offer) = self.drag_offers.get_mut(&(target.0, id))
                {
                    offer.dropped = true;
                }
                drag.dropped = true;
                self.drags.insert(seat, drag);
            } else {
                self.cancel_drag(sessions, seat)?;
            }
        }
        Ok(())
    }
    pub(super) fn drag_request(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        request: &DecodedWaylandRequest,
        message: &mut WaylandWireMessage,
    ) -> Result<Option<bool>, String> {
        let object = (client, request.object_id);
        match request.hook_request.as_ref() {
            Some(GeneratedHookRequest::WlDataSourceSetActions { dnd_actions }) => {
                if *dnd_actions & !7 != 0 {
                    return Err("invalid drag source actions".into());
                }
                let source = self.sources.get_mut(&object).ok_or("unknown drag source")?;
                if source.actions.replace(*dnd_actions).is_some() {
                    return Err("drag source actions already set".into());
                }
                if source.upstream {
                    Self::upstream(sessions, client, message.bytes.clone(), &[])?;
                }
                return Ok(Some(true));
            }
            Some(GeneratedHookRequest::WlDataDeviceStartDrag { source, origin, .. }) => {
                let seat = self.devices.get(&object).ok_or("unknown drag device")?.seat;
                if self.origin(client, seat) == Origin::Human {
                    if let Some(source) = source {
                        self.ensure_host_source(sessions, (client, *source))?;
                    }
                    Self::upstream(sessions, client, message.bytes.clone(), &[])?;
                } else {
                    let source = source.map(|id| (client, id));
                    if source.is_some_and(|source| !self.sources.contains_key(&source)) {
                        return Err("unknown private drag source".into());
                    }
                    if origin.is_none_or(|surface| {
                        sessions
                            .get(&client)
                            .and_then(|s| s.object_interfaces.get(&surface))
                            .map(String::as_str)
                            != Some("wl_surface")
                    }) {
                        return Err("invalid drag origin surface".into());
                    }
                    self.cancel_drag(sessions, seat)?;
                    trace_wayland_proxy(format_args!(
                        "private drag started: client={} seat={seat}",
                        client.0
                    ));
                    self.drags.insert(
                        seat,
                        Drag {
                            source,
                            target: None,
                            offer: None,
                            dropped: false,
                        },
                    );
                    if let Some((client, surface, x, y)) = self.drag_positions.get(&seat).copied() {
                        self.model_input(
                            sessions,
                            client,
                            seat,
                            surface,
                            &serde_json::json!({"type":"motion","x":x,"y":y}),
                        )?;
                    }
                }
                return Ok(Some(true));
            }
            Some(GeneratedHookRequest::WlDataSourceDestroy) => {
                let seats = self
                    .drags
                    .iter()
                    .filter(|(_, d)| d.source == Some(object))
                    .map(|(seat, _)| *seat)
                    .collect::<Vec<_>>();
                for seat in seats {
                    self.cancel_drag(sessions, seat)?;
                }
            }
            _ => {}
        }
        let Some(mut offer) = self.drag_offers.get(&object).cloned() else {
            return Ok(None);
        };
        if let Route::Host(up) = offer.route {
            if self.origin(client, offer.seat) != Origin::Human {
                return Ok(Some(true));
            }
            if matches!(
                request.hook_request,
                Some(
                    GeneratedHookRequest::WlDataOfferDestroy
                        | GeneratedHookRequest::WlDataOfferAccept { .. }
                        | GeneratedHookRequest::WlDataOfferSetActions { .. }
                        | GeneratedHookRequest::WlDataOfferFinish
                        | GeneratedHookRequest::WlDataOfferReceive { .. }
                )
            ) {
                let mut bytes = message.bytes.clone();
                bytes[..4].copy_from_slice(&up.to_ne_bytes());
                Self::upstream(sessions, client, bytes, &message.fds)?;
                if matches!(
                    request.hook_request,
                    Some(GeneratedHookRequest::WlDataOfferDestroy)
                ) {
                    self.drag_offers.remove(&object);
                    self.offers.remove(&object);
                    self.host_mimes.remove(&(client, up));
                }
                return Ok(Some(true));
            }
            return Ok(None);
        }
        let Route::Private(source) = offer.route else {
            unreachable!()
        };
        match request.hook_request.as_ref() {
            Some(GeneratedHookRequest::WlDataOfferAccept { mime_type, .. }) => {
                if mime_type.as_ref().is_some_and(|m| {
                    !self
                        .sources
                        .get(&source)
                        .is_some_and(|s| s.mimes.contains(m))
                }) {
                    return Err("drag MIME was not offered".into());
                }
                if offer.mime != *mime_type {
                    offer.mime = mime_type.clone();
                    Self::emit(
                        sessions,
                        source,
                        GeneratedEvent::WlDataSourceTarget {
                            mime_type: mime_type.clone(),
                        },
                        &[],
                    )?;
                }
                if sessions
                    .get(&client)
                    .and_then(|s| s.object_versions.get(&object.1))
                    .copied()
                    .unwrap_or(1)
                    < 3
                {
                    offer.actions = 1;
                }
            }
            Some(GeneratedHookRequest::WlDataOfferSetActions {
                dnd_actions,
                preferred_action,
            }) => {
                if *dnd_actions & !7 != 0
                    || (*preferred_action != 0
                        && (!preferred_action.is_power_of_two()
                            || *preferred_action & *dnd_actions == 0))
                {
                    return Err("invalid target drag actions".into());
                }
                let available = self.source_actions(sessions, source) & *dnd_actions;
                let chosen = if available & *preferred_action != 0 {
                    *preferred_action
                } else {
                    available & available.wrapping_neg()
                };
                if chosen != offer.actions {
                    offer.actions = chosen;
                    Self::emit(
                        sessions,
                        object,
                        GeneratedEvent::WlDataOfferAction { dnd_action: chosen },
                        &[],
                    )?;
                    self.source_event(
                        sessions,
                        source,
                        GeneratedEvent::WlDataSourceAction { dnd_action: chosen },
                    )?;
                }
            }
            Some(GeneratedHookRequest::WlDataOfferReceive {
                mime_type: Some(mime),
                ..
            }) => {
                if self.origin(client, offer.seat) != Origin::Model {
                    return Ok(Some(true));
                }
                if !self
                    .sources
                    .get(&source)
                    .is_some_and(|s| s.mimes.contains(mime))
                {
                    return Err("drag MIME was not offered".into());
                }
                let destination = message.fds.pop().ok_or("drag receive missing FD")?;
                let count = self.reserve_transfer()?;
                let (read, write) = clipboard_pipe()?;
                Self::emit(
                    sessions,
                    source,
                    GeneratedEvent::WlDataSourceSend {
                        mime_type: Some(mime.clone()),
                        fd: true,
                    },
                    &[write],
                )?;
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(10),
                        pump_clipboard(read, destination),
                    )
                    .await;
                    drop(count);
                });
            }
            Some(GeneratedHookRequest::WlDataOfferFinish) => {
                if !offer.dropped || offer.mime.is_none() || offer.actions == 0 {
                    return Err("cannot finish an unaccepted drag".into());
                }
                self.source_event(sessions, source, GeneratedEvent::WlDataSourceDndFinished)?;
                self.drags.remove(&offer.seat);
            }
            Some(GeneratedHookRequest::WlDataOfferDestroy) => {
                self.drag_offers.remove(&object);
                self.offers.remove(&object);
                if !offer.dropped {
                    Self::emit(
                        sessions,
                        source,
                        GeneratedEvent::WlDataSourceTarget { mime_type: None },
                        &[],
                    )?;
                }
                return Ok(Some(true));
            }
            _ => return Ok(None),
        }
        self.drag_offers.insert(object, offer);
        Ok(Some(true))
    }
    pub(super) fn drag_host_event(
        &mut self,
        sessions: &mut HashMap<WaylandClientId, WaylandClientSession>,
        client: WaylandClientId,
        event: &DecodedWaylandEvent,
    ) -> Result<Option<bool>, String> {
        match &event.generated_event {
            GeneratedEvent::WlDataOfferSourceActions { source_actions } => {
                self.host_drag_actions
                    .entry((client, event.object_id))
                    .or_default()
                    .0 = *source_actions;
                return Ok(Some(true));
            }
            GeneratedEvent::WlDataOfferAction { dnd_action } => {
                self.host_drag_actions
                    .entry((client, event.object_id))
                    .or_default()
                    .1 = *dnd_action;
                let ids = self
                    .drag_offers
                    .iter()
                    .filter_map(|((owner, id), o)| {
                        (*owner == client
                            && matches!(o.route,Route::Host(up) if up==event.object_id))
                        .then_some(*id)
                    })
                    .collect::<Vec<_>>();
                for id in ids {
                    Self::emit(
                        sessions,
                        (client, id),
                        GeneratedEvent::WlDataOfferAction {
                            dnd_action: *dnd_action,
                        },
                        &[],
                    )?;
                }
                return Ok(Some(true));
            }
            GeneratedEvent::WlDataDeviceEnter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                let device = (client, event.object_id);
                let seat = self
                    .devices
                    .get(&device)
                    .ok_or("host drag device is unknown")?
                    .seat;
                if self.origin(client, seat) != Origin::Human {
                    return Ok(Some(true));
                }
                let offer = if let Some(up) = id {
                    let mimes = self
                        .host_mimes
                        .get(&(client, *up))
                        .cloned()
                        .unwrap_or_default();
                    let actions = self
                        .host_drag_actions
                        .get(&(client, *up))
                        .copied()
                        .unwrap_or_default();
                    let local = self.new_drag_offer(
                        sessions,
                        device,
                        seat,
                        Route::Host(*up),
                        mimes,
                        actions.0,
                    )?;
                    if sessions
                        .get(&client)
                        .and_then(|s| s.object_versions.get(&event.object_id))
                        .copied()
                        .unwrap_or(1)
                        >= 3
                    {
                        Self::emit(
                            sessions,
                            (client, local),
                            GeneratedEvent::WlDataOfferAction {
                                dnd_action: actions.1,
                            },
                            &[],
                        )?;
                    }
                    Some(local)
                } else {
                    None
                };
                self.host_drag_devices.insert(device, offer);
                let surface = surface
                    .map(|id| {
                        sessions
                            .get(&client)
                            .ok_or("drag client disconnected")?
                            .resource_map
                            .downstream_id(id)
                    })
                    .transpose()?;
                Self::emit(
                    sessions,
                    device,
                    GeneratedEvent::WlDataDeviceEnter {
                        serial: *serial,
                        surface,
                        x: *x,
                        y: *y,
                        id: offer,
                    },
                    &[],
                )?;
                return Ok(Some(true));
            }
            GeneratedEvent::WlDataDeviceMotion { .. }
            | GeneratedEvent::WlDataDeviceDrop
            | GeneratedEvent::WlDataDeviceLeave => {
                let device = (client, event.object_id);
                let Some(info) = self.devices.get(&device) else {
                    return Ok(Some(true));
                };
                if self.origin(client, info.seat) == Origin::Human
                    && self.host_drag_devices.contains_key(&device)
                {
                    Self::emit(sessions, device, event.generated_event.clone(), &[])?;
                }
                if matches!(event.generated_event, GeneratedEvent::WlDataDeviceLeave) {
                    self.host_drag_devices.remove(&device);
                }
                return Ok(Some(true));
            }
            _ => {}
        }
        Ok(None)
    }
}
