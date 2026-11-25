use super::super::{
    destroy_drm_syncobj, import_drm_syncobj, monotonic_timeout_nsec, signal_drm_syncobj_handle,
    wait_drm_syncobj_handle,
};
use super::{Arg, Backing, Buffer};
use crate::ssh_video::bridge_render_nodes;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

struct Timeline {
    device: Arc<File>,
    handle: u32,
}

impl Drop for Timeline {
    fn drop(&mut self) {
        let _ = destroy_drm_syncobj(&self.device, self.handle);
    }
}

struct Point {
    timeline: Arc<Timeline>,
    value: u64,
}

pub(super) struct CommitSync {
    acquire: Point,
    release: Point,
}

impl CommitSync {
    pub(super) fn wait(&self) -> Result<(), String> {
        wait_drm_syncobj_handle(
            &self.acquire.timeline.device,
            self.acquire.timeline.handle,
            self.acquire.value,
            monotonic_timeout_nsec(Duration::from_secs(5))?,
        )
    }

    pub(super) fn release(self) -> Result<(), String> {
        signal_drm_syncobj_handle(
            &self.release.timeline.device,
            self.release.timeline.handle,
            self.release.value,
        )
    }
}

#[derive(Default)]
struct Surface {
    acquire: Option<Point>,
    release: Option<Point>,
}

#[derive(Default)]
pub(super) struct SourceSync {
    devices: HashMap<String, Arc<File>>,
    timelines: HashMap<u32, Arc<Timeline>>,
    surface_objects: HashMap<u32, u32>,
    surfaces: HashMap<u32, Surface>,
}

impl SourceSync {
    fn import(&mut self, fd: &OwnedFd, preferred: Option<&str>) -> Result<Timeline, String> {
        let mut error = "no DRM render node available for explicit synchronization".to_owned();
        let nodes = preferred.into_iter().chain(
            bridge_render_nodes()
                .iter()
                .map(String::as_str)
                .filter(|node| Some(*node) != preferred),
        );
        for node in nodes {
            let device = match self.devices.get(node) {
                Some(device) => Arc::clone(device),
                None => match OpenOptions::new().read(true).write(true).open(node) {
                    Ok(file) => Arc::clone(
                        self.devices
                            .entry(node.to_owned())
                            .or_insert_with(|| Arc::new(file)),
                    ),
                    Err(reason) => {
                        error = format!("{node}: {reason}");
                        continue;
                    }
                },
            };
            match import_drm_syncobj(&device, fd) {
                Ok(handle) => return Ok(Timeline { device, handle }),
                Err(reason) => error = format!("{node}: {reason}"),
            }
        }
        Err(error)
    }

    pub(super) fn request(
        &mut self,
        request: (&str, &str),
        id: u32,
        args: &[Arg],
        fds: &mut Vec<OwnedFd>,
        gpu: Option<&str>,
    ) -> Result<(), String> {
        match (request.0, request.1, args) {
            (
                "wp_linux_drm_syncobj_manager_v1",
                "get_surface",
                [Arg::NewId(object), Arg::Object(Some(surface))],
            ) => {
                if self.surfaces.contains_key(surface) {
                    return Err("Wayland surface already has explicit synchronization".into());
                }
                self.surface_objects.insert(*object, *surface);
                self.surfaces.insert(*surface, Surface::default());
            }
            (
                "wp_linux_drm_syncobj_manager_v1",
                "import_timeline",
                [Arg::NewId(object), Arg::Fd],
            ) => {
                let fd = fds.pop().ok_or("missing Wayland syncobj timeline fd")?;
                let timeline = self.import(&fd, gpu)?;
                self.timelines.insert(*object, Arc::new(timeline));
            }
            ("wp_linux_drm_syncobj_manager_v1", "destroy", []) => {}
            ("wp_linux_drm_syncobj_timeline_v1", "destroy", []) => {
                self.timelines.remove(&id);
            }
            ("wp_linux_drm_syncobj_surface_v1", "destroy", []) => {
                if let Some(surface) = self.surface_objects.remove(&id) {
                    self.surfaces.remove(&surface);
                }
            }
            (
                "wp_linux_drm_syncobj_surface_v1",
                name @ ("set_acquire_point" | "set_release_point"),
                [Arg::Object(Some(timeline)), Arg::Uint(hi), Arg::Uint(lo)],
            ) => {
                let surface = self
                    .surface_objects
                    .get(&id)
                    .and_then(|id| self.surfaces.get_mut(id))
                    .ok_or("unknown explicit-sync Wayland surface")?;
                let timeline = self
                    .timelines
                    .get(timeline)
                    .ok_or("unknown Wayland syncobj timeline")?;
                let point = Some(Point {
                    timeline: Arc::clone(timeline),
                    value: (u64::from(*hi) << 32) | u64::from(*lo),
                });
                if name == "set_acquire_point" {
                    surface.acquire = point;
                } else {
                    surface.release = point;
                }
            }
            _ => {
                return Err(format!(
                    "unsupported explicit-sync request {}.{}",
                    request.0, request.1
                ));
            }
        }
        Ok(())
    }

    pub(super) fn destroy_surface(&mut self, id: u32) {
        self.surfaces.remove(&id);
        self.surface_objects.retain(|_, surface| *surface != id);
    }

    pub(super) fn commit(
        &mut self,
        id: u32,
        buffer: Option<&Buffer>,
    ) -> Result<Option<CommitSync>, String> {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return Ok(None);
        };
        match (buffer, surface.acquire.take(), surface.release.take()) {
            (None, None, None) => Ok(None),
            (Some(buffer), Some(acquire), Some(release))
                if matches!(buffer.backing, Backing::SourceDma(_)) =>
            {
                if Arc::ptr_eq(&acquire.timeline.device, &release.timeline.device)
                    && acquire.timeline.handle == release.timeline.handle
                    && acquire.value >= release.value
                {
                    return Err(
                        "Wayland release point must follow acquire on the same timeline".into(),
                    );
                }
                Ok(Some(CommitSync { acquire, release }))
            }
            (Some(_), _, _) => {
                Err("explicit Wayland buffer commit requires DMA-BUF and both sync points".into())
            }
            (None, _, _) => Err("explicit Wayland sync points require a buffer attachment".into()),
        }
    }
}
