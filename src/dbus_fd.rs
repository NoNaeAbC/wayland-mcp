use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use tokio::io::{Interest, unix::AsyncFd};
use tokio_util::sync::CancellationToken;

const MAX_DESCRIPTORS: usize = 256;

#[derive(Default)]
pub(crate) struct Descriptors {
    next: AtomicU64,
    handles: Mutex<HashMap<u64, Entry>>,
}

struct Entry {
    owner: u64,
    fd: OwnedFd,
    closed: CancellationToken,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

/// Reclaim every handle if decoding or delivering the message fails partway.
pub(crate) struct Received<'a> {
    store: &'a Descriptors,
    owner: u64,
    active: &'a CancellationToken,
    ids: Vec<u64>,
}

impl Received<'_> {
    pub(crate) fn add(&mut self, fd: OwnedFd) -> Result<Value, String> {
        let mut handles = self
            .store
            .handles
            .lock()
            .map_err(|error| error.to_string())?;
        if self.active.is_cancelled() {
            return Err("D-Bus connection is closed".into());
        }
        if handles.len() >= MAX_DESCRIPTORS {
            return Err("console D-Bus descriptor limit reached".into());
        }
        let id = self.store.next.fetch_add(1, Ordering::Relaxed) + 1;
        handles.insert(
            id,
            Entry {
                owner: self.owner,
                fd,
                closed: CancellationToken::new(),
            },
        );
        self.ids.push(id);
        Ok(json!({"kind":"fd","value":id}))
    }

    pub(crate) fn commit(mut self) {
        self.ids.clear();
    }
}

impl Drop for Received<'_> {
    fn drop(&mut self) {
        if !self.ids.is_empty()
            && let Ok(mut handles) = self.store.handles.lock()
        {
            for id in &self.ids {
                handles.remove(id);
            }
        }
    }
}

impl Descriptors {
    pub(crate) fn receive<'a>(&'a self, owner: u64, active: &'a CancellationToken) -> Received<'a> {
        Received {
            store: self,
            owner,
            active,
            ids: Vec::new(),
        }
    }

    pub(crate) fn copy(&self, id: u64) -> Result<OwnedFd, String> {
        self.handles
            .lock()
            .map_err(|error| error.to_string())?
            .get(&id)
            .ok_or("D-Bus descriptor is closed")?
            .fd
            .try_clone()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn close_owner(&self, owner: u64) {
        if let Ok(mut handles) = self.handles.lock() {
            handles.retain(|_, entry| entry.owner != owner);
        }
    }

    pub(crate) fn pipe(&self, owner: u64, active: &CancellationToken) -> Result<Value, String> {
        let mut raw = [-1; 2];
        if unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // Both descriptors belong to this process after pipe2 succeeds.
        let [read, write] = raw.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
        let mut received = self.receive(owner, active);
        let ends = json!({"readEnd":received.add(read)?,"writeEnd":received.add(write)?});
        received.commit();
        Ok(ends)
    }

    pub(crate) async fn io(&self, id: u64, operation: &str, args: &Value) -> Result<Value, String> {
        if operation == "dbus_fd_close" {
            self.handles
                .lock()
                .map_err(|error| error.to_string())?
                .remove(&id);
            return Ok(Value::Null);
        }
        let (fd, closed) = {
            let handles = self.handles.lock().map_err(|error| error.to_string())?;
            let entry = handles.get(&id).ok_or("D-Bus descriptor is closed")?;
            (
                entry.fd.try_clone().map_err(|error| error.to_string())?,
                entry.closed.clone(),
            )
        };
        let writing = operation == "dbus_fd_write";
        let mut bytes = if writing {
            let encoded = args["bytes"]
                .as_str()
                .ok_or("descriptor write needs bytes")?;
            if encoded.len() > 4 * 4096usize.div_ceil(3) {
                return Err("descriptor writes take up to 4096 bytes per call".into());
            }
            let bytes = BASE64.decode(encoded).map_err(|error| error.to_string())?;
            if bytes.len() > 4096 {
                return Err("descriptor writes take up to 4096 bytes per call".into());
            }
            bytes
        } else {
            let size = args["limit"]
                .as_u64()
                .filter(|size| (1..=65536).contains(size))
                .ok_or("descriptor read size must be between 1 and 65536")?;
            vec![0; size as usize]
        };
        let (fd, socket) = nonblocking_io(fd, writing).map_err(|error| error.to_string())?;
        // OwnedFd keeps the registered descriptor stable until deregistration.
        let fd = unsafe { AsyncFd::register(fd) }.map_err(|error| error.to_string())?;
        let interest = if writing {
            Interest::WRITABLE
        } else {
            Interest::READABLE
        };
        let transfer = fd.async_io(interest, |fd| {
            let fd = fd.as_raw_fd();
            loop {
                let count = unsafe {
                    match (socket, writing) {
                        (true, true) => libc::send(
                            fd,
                            bytes.as_ptr().cast(),
                            bytes.len(),
                            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                        ),
                        (true, false) => libc::recv(
                            fd,
                            bytes.as_mut_ptr().cast(),
                            bytes.len(),
                            libc::MSG_DONTWAIT,
                        ),
                        (false, true) => libc::write(fd, bytes.as_ptr().cast(), bytes.len()),
                        (false, false) => libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()),
                    }
                };
                if count >= 0 {
                    return Ok(count as usize);
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        });
        let count = tokio::select! {
            biased;
            _ = closed.cancelled() => return Err("D-Bus descriptor is closed".into()),
            result = tokio::time::timeout(Duration::from_secs(10), transfer) =>
                result.map_err(|_| "D-Bus descriptor operation timed out")?
                    .map_err(|error| error.to_string())?,
        };
        if writing {
            Ok(json!(count))
        } else {
            Ok(json!(BASE64.encode(&bytes[..count])))
        }
    }
}

fn nonblocking_io(fd: OwnedFd, writing: bool) -> io::Result<(OwnedFd, bool)> {
    let raw = fd.as_raw_fd();
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(raw, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    match unsafe { stat.assume_init() }.st_mode & libc::S_IFMT {
        libc::S_IFIFO => {
            let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let access = flags & libc::O_ACCMODE;
            if (writing && access == libc::O_RDONLY) || (!writing && access == libc::O_WRONLY) {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            // Reopen with independent status flags. F_SETFL would change the
            // peer's descriptor too, and a poll followed by blocking I/O races.
            let file = OpenOptions::new()
                .read(!writing)
                .write(writing)
                .custom_flags(libc::O_NONBLOCK)
                .open(format!("/proc/self/fd/{raw}"))?;
            Ok((file.into(), false))
        }
        libc::S_IFSOCK => {
            let mut kind: libc::c_int = 0;
            let mut len = std::mem::size_of_val(&kind) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    raw,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    (&mut kind as *mut libc::c_int).cast(),
                    &mut len,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if kind == libc::SOCK_STREAM {
                Ok((fd, true))
            } else {
                Err(io::Error::other("descriptor I/O requires a stream socket"))
            }
        }
        _ => Err(io::Error::other(
            "descriptor I/O requires a pipe or stream socket",
        )),
    }
}
