use super::{
    Attachment, Backing, Buffer, Shm, WaylandWireMessage, XR24, display, display_format,
    find_generated_event_by_opcode, io_error, memfd, rewrite_wire_object_ids, wire,
};
use std::collections::HashMap;
use std::sync::{Arc, Condvar};

const SERVER_ID_START: u32 = 0xff00_0000;
const MAX_DISPLAY_SLOTS: usize = 5;

#[derive(Clone, Copy)]
enum Resource {
    Application(u32),
    Replica(u32),
    Private,
}

#[derive(Default)]
pub(super) struct Host {
    next: u32,
    free: Vec<u32>,
    outgoing: HashMap<u32, u32>,
    incoming: HashMap<u32, Resource>,
    registry: Option<u32>,
    factories: HashMap<&'static str, u32>,
    buffers: HashMap<u32, Vec<Slot>>,
    pub(super) slot_released: Arc<Condvar>,
}

pub(super) struct Slot {
    pub wire_id: u32,
    pub storage: Arc<Buffer>,
    in_use: bool,
}

impl Host {
    fn allocate(&mut self, resource: Resource) -> Result<u32, String> {
        let id = if let Some(id) = self.free.pop() {
            id
        } else {
            let id = self.next.max(2);
            if id >= SERVER_ID_START {
                return Err("SSH Wayland compositor object IDs exhausted".into());
            }
            self.next = id + 1;
            id
        };
        self.incoming.insert(id, resource);
        Ok(id)
    }

    pub(super) fn register(&mut self, application: u32) -> Result<u32, String> {
        let id = self.allocate(Resource::Application(application))?;
        self.outgoing.insert(application, id);
        Ok(id)
    }

    pub(super) fn wire_id(&self, id: u32) -> Result<u32, String> {
        if id == 1 || id >= SERVER_ID_START {
            Ok(id)
        } else {
            self.outgoing
                .get(&id)
                .copied()
                .ok_or_else(|| format!("Wayland object {id} has no compositor resource"))
        }
    }

    pub(super) fn set_registry(&mut self, id: u32) {
        self.registry = Some(id);
    }

    fn application_id(&self, id: u32) -> Result<Option<u32>, String> {
        if id == 1 || id >= SERVER_ID_START {
            return Ok(Some(id));
        }
        match self.incoming.get(&id) {
            Some(Resource::Application(id) | Resource::Replica(id)) => Ok(Some(*id)),
            Some(Resource::Private) => Ok(None),
            None => Err(format!("unknown compositor Wayland object {id}")),
        }
    }

    pub(super) fn route_event(
        &mut self,
        bytes: &mut [u8],
        id: u32,
        opcode: u16,
        interfaces: &HashMap<u32, String>,
    ) -> Result<bool, String> {
        if id == 1 && opcode == 1 && bytes.len() == 12 {
            let deleted = u32::from_ne_bytes(bytes[8..12].try_into().unwrap());
            let resource = self
                .incoming
                .remove(&deleted)
                .ok_or_else(|| format!("unexpected compositor delete_id for {deleted}"))?;
            self.free.push(deleted);
            let Resource::Application(application) = resource else {
                return Ok(false);
            };
            self.outgoing.remove(&application);
            bytes[8..12].copy_from_slice(&application.to_ne_bytes());
            return Ok(true);
        }

        let Some(application) = self.application_id(id)? else {
            return Ok(false);
        };
        let interface = interfaces
            .get(&application)
            .ok_or_else(|| format!("unknown Wayland event object {application}"))?;
        if interface == "wl_buffer" && opcode == 0 {
            let Some(slots) = self.buffers.get_mut(&application) else {
                return Ok(false);
            };
            let Some(slot) = slots
                .iter_mut()
                .find(|slot| slot.wire_id == id && slot.in_use)
            else {
                return Ok(false);
            };
            slot.in_use = false;
            self.slot_released.notify_all();
            // A decoded copy releases only its local slot. The application
            // buffer was released on the source host after the GPU read.
            return Ok(false);
        }
        if interface == "wl_shm" && opcode == 0 {
            bytes[..4].copy_from_slice(&application.to_ne_bytes());
        } else {
            let (_, event) = find_generated_event_by_opcode(interface, opcode)
                .ok_or_else(|| format!("unsupported compositor event {interface}.{opcode}"))?;
            rewrite_wire_object_ids(bytes, event.args, false, |id, _| {
                self.application_id(id)?
                    .ok_or("event references a private compositor object".into())
            })?;
        }
        Ok(true)
    }

    pub(super) fn can_commit(&self, buffer: u32) -> bool {
        self.buffers.get(&buffer).is_none_or(|slots| {
            slots.len() < MAX_DISPLAY_SLOTS || slots.iter().any(|slot| !slot.in_use)
        })
    }

    pub(super) fn commit(
        &mut self,
        attach: &Attachment,
        globals: &HashMap<u32, (String, u32)>,
        gpu: Option<&str>,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<&Slot, String> {
        let base_id = self.wire_id(attach.id)?;
        let slots = self.buffers.entry(attach.id).or_insert_with(|| {
            vec![Slot {
                wire_id: base_id,
                storage: Arc::clone(&attach.buffer),
                in_use: false,
            }]
        });
        let position = if let Some(index) = slots.iter().position(|slot| !slot.in_use) {
            index
        } else {
            let index = slots.len();
            if index == MAX_DISPLAY_SLOTS {
                return Err(
                    "the compositor still holds all five copies of this Wayland buffer".into(),
                );
            }
            let replacement = self.replicate(attach, globals, gpu, before)?;
            self.buffers.get_mut(&attach.id).unwrap().push(replacement);
            index
        };
        let slot = &mut self.buffers.get_mut(&attach.id).unwrap()[position];
        slot.in_use = true;
        Ok(slot)
    }

    pub(super) fn destroy_buffer(&mut self, buffer: u32, before: &mut Vec<WaylandWireMessage>) {
        if let Some(slots) = self.buffers.remove(&buffer) {
            let base = self.outgoing.get(&buffer).copied().unwrap_or(buffer);
            for slot in slots {
                self.retire_replica(slot.wire_id, base, before);
            }
        }
    }

    fn retire_replica(&mut self, id: u32, base: u32, before: &mut Vec<WaylandWireMessage>) {
        if id != base {
            self.incoming.insert(id, Resource::Private);
            before.push(wire(id, 0, &[], Vec::new()));
        }
    }

    fn replicate(
        &mut self,
        attach: &Attachment,
        globals: &HashMap<u32, (String, u32)>,
        gpu: Option<&str>,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<Slot, String> {
        let buffer = &attach.buffer;
        let (width, height) = (buffer.width, buffer.height);
        let (id, storage) = match &buffer.backing {
            Backing::Shm(_) => {
                let factory = self.factory("wl_shm", 1, globals, before)?;
                let pool = self.allocate(Resource::Private)?;
                let id = self.allocate(Resource::Replica(attach.id))?;
                let stride = width * 4;
                let size = stride * height;
                let file = memfd((size as usize).next_multiple_of(4096))?;
                let fd = file.try_clone().map_err(io_error)?.into();
                before.push(wire(factory, 0, &[pool, size], vec![fd]));
                before.push(wire(
                    pool,
                    0,
                    &[
                        id,
                        0,
                        width,
                        height,
                        stride,
                        u32::from(display_format(buffer.format) == XR24),
                    ],
                    Vec::new(),
                ));
                before.push(wire(pool, 1, &[], Vec::new()));
                let backing = Backing::Shm(Shm {
                    file,
                    offset: 0,
                    stride,
                });
                (id, Buffer::new(width, height, buffer.format, backing))
            }
            Backing::DisplayDma(_, flags) => {
                let allocation = display::DisplayBuffer::new(
                    gpu.ok_or("no render node for SSH Wayland DMA-BUFs")?,
                    width,
                    height,
                    display_format(buffer.format),
                )?;
                let storage = Buffer::new(
                    width,
                    height,
                    buffer.format,
                    Backing::DisplayDma(allocation, *flags),
                );
                let id = self.create_display_dma(
                    Resource::Replica(attach.id),
                    &storage,
                    globals,
                    before,
                )?;
                (id, storage)
            }
            Backing::SourceDma(_) => return Err("invalid compositor buffer backing".into()),
        };
        Ok(Slot {
            wire_id: id,
            storage,
            in_use: false,
        })
    }

    pub(super) fn register_display_dma(
        &mut self,
        application: u32,
        buffer: &Buffer,
        globals: &HashMap<u32, (String, u32)>,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<(), String> {
        self.create_display_dma(Resource::Application(application), buffer, globals, before)?;
        Ok(())
    }

    fn create_display_dma(
        &mut self,
        resource: Resource,
        buffer: &Buffer,
        globals: &HashMap<u32, (String, u32)>,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<u32, String> {
        let Backing::DisplayDma(allocation, flags) = &buffer.backing else {
            return Err("invalid display DMA-BUF backing".into());
        };
        let factory = self.factory("zwp_linux_dmabuf_v1", 2, globals, before)?;
        let params = self.allocate(Resource::Private)?;
        // Wayland IDs must be introduced in allocation order.
        let id = match resource {
            Resource::Application(app_id) => self.register(app_id)?,
            resource => self.allocate(resource)?,
        };
        before.push(wire(factory, 1, &[params], Vec::new()));
        before.push(allocation.plane_request(params)?);
        before.push(wire(
            params,
            3,
            &[
                id,
                buffer.width,
                buffer.height,
                display_format(buffer.format),
                *flags,
            ],
            Vec::new(),
        ));
        before.push(wire(params, 0, &[], Vec::new()));
        Ok(id)
    }

    fn factory(
        &mut self,
        interface: &'static str,
        version: u32,
        globals: &HashMap<u32, (String, u32)>,
        before: &mut Vec<WaylandWireMessage>,
    ) -> Result<u32, String> {
        if let Some(id) = self.factories.get(interface) {
            return Ok(*id);
        }
        let registry = self
            .registry
            .ok_or("no compositor registry for buffer allocation")?;
        let name = globals
            .iter()
            .find_map(|(&name, (available, max))| {
                (available == interface && *max >= version).then_some(name)
            })
            .ok_or_else(|| format!("compositor did not advertise {interface} version {version}"))?;
        let id = self.allocate(Resource::Private)?;
        before.push(bind(registry, name, interface, version, id));
        self.factories.insert(interface, id);
        Ok(id)
    }
}

pub(super) fn bind(
    registry: u32,
    name: u32,
    interface: &str,
    version: u32,
    id: u32,
) -> WaylandWireMessage {
    let mut words = vec![name, interface.len() as u32 + 1];
    let mut string = interface.as_bytes().to_vec();
    string.resize((string.len() + 1).next_multiple_of(4), 0);
    words.extend(
        string
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(u32::from_ne_bytes),
    );
    words.extend([version, id]);
    wire(registry, 0, &words, Vec::new())
}
