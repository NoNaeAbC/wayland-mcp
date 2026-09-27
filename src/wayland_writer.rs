//! One bounded message queue per downstream connection; no socket I/O under proxy locks.
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Human,
    Model,
    Local,
}

type DeliveryHook = Arc<dyn Fn(&[u8], Origin, Option<u32>) + Send + Sync>;
struct Packet {
    sequence: u64,
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    origin: Origin,
    surface: Option<u32>,
}
struct Shared {
    socket: UnixStream,
    enqueue_lock: Mutex<()>,
    sequence: AtomicU64,
    bytes: AtomicUsize,
    error: Mutex<Option<String>>,
    delivered: watch::Sender<u64>,
    hook: Mutex<Option<DeliveryHook>>,
}

#[derive(Clone)]
pub(crate) struct ClientWriter {
    sender: mpsc::SyncSender<Packet>,
    shared: Arc<Shared>,
}

pub(crate) trait WireSink {
    fn send_wire(&self, bytes: &[u8], fds: &[OwnedFd]) -> Result<(), String>;
}
impl WireSink for UnixStream {
    fn send_wire(&self, bytes: &[u8], fds: &[OwnedFd]) -> Result<(), String> {
        crate::gui_backend_wayland::send_wayland_wire_message_to_fd(self.as_raw_fd(), bytes, fds)
    }
}
impl WireSink for ClientWriter {
    fn send_wire(&self, bytes: &[u8], fds: &[OwnedFd]) -> Result<(), String> {
        self.send_origin(bytes, fds, Origin::Model)
    }
}
impl ClientWriter {
    pub(crate) fn new(socket: UnixStream) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Packet>(1024);
        let (delivered, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            socket,
            enqueue_lock: Mutex::new(()),
            sequence: AtomicU64::new(0),
            bytes: AtomicUsize::new(0),
            error: Mutex::new(None),
            delivered,
            hook: Mutex::new(None),
        });
        let worker = shared.clone();
        std::thread::spawn(move || {
            while let Ok(packet) = receiver.recv() {
                let result = worker.socket.send_wire(&packet.bytes, &packet.fds);
                worker
                    .bytes
                    .fetch_sub(packet.bytes.len(), Ordering::Relaxed);
                if let Err(error) = result {
                    fail(&worker, error);
                    break;
                }
                let hook = worker.hook.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook(&packet.bytes, packet.origin, packet.surface);
                }
                worker.delivered.send_replace(packet.sequence);
            }
        });
        Self { sender, shared }
    }

    pub(crate) fn set_hook(&self, hook: DeliveryHook) {
        *self.shared.hook.lock().unwrap() = Some(hook);
    }
    pub(crate) fn sequence(&self) -> u64 {
        self.shared.sequence.load(Ordering::Acquire)
    }
    pub(crate) fn send_origin(
        &self,
        bytes: &[u8],
        fds: &[OwnedFd],
        origin: Origin,
    ) -> Result<(), String> {
        self.send_scoped(bytes, fds, origin, None)
    }
    pub(crate) fn send_scoped(
        &self,
        bytes: &[u8],
        fds: &[OwnedFd],
        origin: Origin,
        surface: Option<u32>,
    ) -> Result<(), String> {
        let _guard = self.shared.enqueue_lock.lock().unwrap();
        if let Some(error) = self.shared.error.lock().unwrap().clone() {
            return Err(error);
        }
        if bytes.len() > 65535
            || self.shared.bytes.load(Ordering::Relaxed) + bytes.len() > 4 * 1024 * 1024
        {
            let error = "downstream Wayland queue exceeded its byte limit".to_string();
            fail(&self.shared, error.clone());
            return Err(error);
        }
        let owned_fds = fds
            .iter()
            .map(|fd| fd.try_clone().map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let sequence = self.shared.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.bytes.fetch_add(bytes.len(), Ordering::Relaxed);
        if let Err(error) = self.sender.try_send(Packet {
            sequence,
            bytes: bytes.to_vec(),
            fds: owned_fds,
            origin,
            surface,
        }) {
            self.shared.bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
            let error = format!("downstream Wayland queue unavailable: {error}");
            fail(&self.shared, error.clone());
            return Err(error);
        }
        Ok(())
    }
    pub(crate) async fn wait(&self, sequence: u64) -> Result<(), String> {
        let mut delivered = self.shared.delivered.subscribe();
        loop {
            if let Some(error) = self.shared.error.lock().unwrap().clone() {
                return Err(error);
            }
            if *delivered.borrow_and_update() >= sequence {
                return Ok(());
            }
            delivered
                .changed()
                .await
                .map_err(|_| "Wayland writer stopped".to_string())?;
        }
    }
    pub(crate) fn shutdown(&self) {
        let _ = self.shared.socket.shutdown(std::net::Shutdown::Both);
    }
}
fn fail(shared: &Shared, error: String) {
    *shared.error.lock().unwrap() = Some(error);
    let _ = shared.socket.shutdown(std::net::Shutdown::Both);
    shared.delivered.send_replace(u64::MAX);
}
