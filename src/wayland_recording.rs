use super::*;
use crate::ssh_video::RecordingEncoder;
use serde::Deserialize;
use tokio::sync::watch;

const START_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DURATION: Duration = Duration::from_secs(8 * 60 * 60);
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const FREE_SPACE_RESERVE: u64 = 256 * 1024 * 1024;
// The VA-API recorder uses four display pictures between anchors and starts
// a closed GOP after 120 inter pictures.
const ANCHOR_INTERVAL: u32 = 4;
const IDR_PERIOD: u32 = 121;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Request {
    window_id: String,
    max_frames: u32,
    #[serde(default = "default_frame_rate")]
    frame_rate: u32,
}

fn default_frame_rate() -> u32 {
    30
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    recording_id: u64,
    path: Option<PathBuf>,
    max_frames: u32,
    frame_rate: u32,
    frames_written: u32,
    bytes_written: u64,
    width: Option<u32>,
    height: Option<u32>,
    stop_reason: Option<&'static str>,
    error: Option<String>,
}

#[derive(Clone)]
pub(super) struct Recording {
    window: String,
    deadline: Instant,
    stop: CancellationToken,
    status: watch::Receiver<Status>,
}

impl Recording {
    pub(super) fn is_observing(&self) -> bool {
        self.status.borrow().stop_reason.is_none()
            && self.status.has_changed().is_ok()
            && !self.stop.is_cancelled()
            && Instant::now() < self.deadline
    }

    pub(super) fn observes(&self, window: Option<&String>) -> bool {
        window == Some(&self.window) && self.is_observing()
    }

    pub(super) fn cancel(&self) {
        self.stop.cancel();
    }

    pub(super) async fn wait(&mut self) {
        // If the worker panicked, its sender is dropped and wait_for returns.
        let _ = self
            .status
            .wait_for(|status| status.stop_reason.is_some())
            .await;
    }

    fn report(&self) -> Result<serde_json::Value, String> {
        let status = self.status.borrow();
        if status.stop_reason.is_none() && self.status.has_changed().is_err() {
            return Err("recording worker exited unexpectedly".into());
        }
        serde_json::to_value(&*status).map_err(|error| error.to_string())
    }
}

impl WaylandGuiBackend {
    pub(crate) async fn start_recording(
        &self,
        args: serde_json::Value,
        artifacts: &crate::ArtifactStore,
    ) -> Result<serde_json::Value, String> {
        let request: Request =
            serde_json::from_value(args).map_err(|err| format!("startRecording options: {err}"))?;
        if !(1..=60).contains(&request.frame_rate) {
            return Err("startRecording frameRate must be in 1..60".into());
        }
        let cadence = Duration::from_secs(1) / request.frame_rate;
        if request.max_frames == 0
            || u64::from(request.max_frames)
                > MAX_DURATION.as_secs() * u64::from(request.frame_rate)
        {
            return Err(
                "startRecording maxFrames must be positive and at most 8 hours at frameRate".into(),
            );
        }
        let duration = cadence * request.max_frames;
        let mut inner = self.state.inner.lock().await;
        let id = match &inner.recording {
            Some(recording) => {
                let status = recording.status.borrow();
                if status.stop_reason.is_none() && recording.status.has_changed().is_ok() {
                    return Err("a recording is already running".into());
                }
                status
                    .recording_id
                    .checked_add(1)
                    .ok_or("recording IDs exhausted")?
            }
            None => 1,
        };
        if inner.sessions.values().all(|session| {
            !session
                .frame_tracker
                .windows
                .get(&request.window_id)
                .is_some_and(|window| window.mapped)
        }) {
            return Err("recording target is not mapped".into());
        }
        artifacts
            .ensure_directory()
            .map_err(|err| format!("create recording directory: {err}"))?;
        let directory = artifacts
            .directory
            .canonicalize()
            .map_err(|err| err.to_string())?;
        let file = tempfile::Builder::new()
            .prefix("recording-")
            .suffix(".265")
            .tempfile_in(directory)
            .map_err(|err| format!("create H.265 recording: {err}"))?;
        let initial = Status {
            recording_id: id,
            path: Some(file.path().into()),
            frame_rate: request.frame_rate,
            max_frames: request.max_frames,
            frames_written: 0,
            bytes_written: 0,
            width: None,
            height: None,
            stop_reason: None,
            error: None,
        };
        let (updates, status) = watch::channel(initial.clone());
        let recording = Recording {
            window: request.window_id,
            deadline: Instant::now() + START_TIMEOUT + duration,
            stop: CancellationToken::new(),
            status,
        };
        let result = recording.report()?;
        inner.recording = Some(recording.clone());
        let state = self.state.clone();
        tokio::spawn(async move {
            run(state, recording, file, initial, updates, cadence, duration).await;
        });
        Ok(result)
    }

    pub(crate) async fn recording_status(
        &self,
        id: u64,
        end: bool,
    ) -> Result<serde_json::Value, String> {
        let mut recording = self
            .state
            .inner
            .lock()
            .await
            .recording
            .as_ref()
            .filter(|recording| recording.status.borrow().recording_id == id)
            .cloned()
            .ok_or("unknown recordingId")?;
        if end {
            recording.cancel();
            recording.wait().await;
        }
        recording.report()
    }
}

async fn capture_scene(
    state: &WaylandProxyState,
    window: &str,
    recover_shm: bool,
) -> Result<Option<scene::Scene<'static>>, String> {
    let mut inner = state.inner.lock().await;
    let Some(session) = inner.sessions.values_mut().find(|session| {
        session
            .frame_tracker
            .windows
            .get(window)
            .is_some_and(|window| window.mapped)
    }) else {
        return Ok(None);
    };
    if recover_shm {
        session.retain_current_shm(window);
    }
    session
        .frame_tracker
        .scene_for_capture(window)
        .map(|scene| Some(scene.into_owned()))
}

async fn initial_scene(
    state: &WaylandProxyState,
    window: &str,
) -> Result<Option<scene::Scene<'static>>, String> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        match capture_scene(state, window, true).await {
            Err(error) if error.starts_with("snapshot_unavailable:") => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::timeout_at(deadline, state.snapshot_current_pixels(Some(window)))
                    .await
                    .map_err(|_| error)??;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            result => return result,
        }
    }
}

async fn run(
    state: Arc<WaylandProxyState>,
    recording: Recording,
    file: tempfile::NamedTempFile,
    mut status: Status,
    updates: watch::Sender<Status>,
    cadence: Duration,
    duration: Duration,
) {
    let mut output = Output { file, bytes: 0 };
    let result = record(
        &state,
        &recording,
        &mut output,
        &mut status,
        &updates,
        cadence,
        duration,
    )
    .await;
    match result {
        Ok(reason) => status.stop_reason = Some(reason),
        Err(error) => {
            status.stop_reason = Some("error");
            status.error = Some(error);
        }
    }
    if status.frames_written == 0 {
        drop(output);
        status.path = None;
    } else if let Err(error) = output.file.keep() {
        status.path = None;
        status.error = Some(format!("retain recording file: {error}"));
        status.stop_reason = Some("error");
    }
    updates.send_replace(status);
}

async fn record(
    state: &WaylandProxyState,
    recording: &Recording,
    output: &mut Output,
    status: &mut Status,
    updates: &watch::Sender<Status>,
    cadence: Duration,
    duration: Duration,
) -> Result<&'static str, String> {
    let window = &recording.window;
    let first = tokio::select! {
        biased;
        _ = recording.stop.cancelled() => return Ok("stopped"),
        scene = initial_scene(state, window) => scene?,
    };
    let Some(first) = first else {
        return Ok("window_closed");
    };
    let size = (
        first.size.0.next_multiple_of(8),
        first.size.1.next_multiple_of(8),
    );
    let frame_rate = status.frame_rate;
    let (mut encoder, mut previous) = tokio::task::spawn_blocking(move || {
        let encoder = RecordingEncoder::new(size.0, size.1, frame_rate)?;
        let packed = pack_scene(&first, size);
        Ok::<_, String>((encoder, (first, Arc::new(packed))))
    })
    .await
    .map_err(|err| format!("H.265 recording setup worker: {err}"))??;
    status.width = Some(size.0);
    status.height = Some(size.1);
    let now = tokio::time::Instant::now();
    let deadline = (now + duration).min(recording.deadline.into());
    let mut ticks = tokio::time::interval_at(now + cadence, cadence);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frames = vec![previous.1.clone()];
    let mut missing_since = None;
    let mut reason = None;
    loop {
        let count = if status.frames_written.is_multiple_of(IDR_PERIOD) {
            1
        } else {
            ANCHOR_INTERVAL.min(status.max_frames - status.frames_written)
        };
        if frames.len() == count as usize || reason.is_some() {
            if !frames.is_empty() {
                let frame_count = frames.len() as u32;
                let group = std::mem::take(&mut frames);
                let (returned, encoded) = tokio::task::spawn_blocking(move || {
                    let encoded = encoder.encode_group(&group);
                    (encoder, encoded)
                })
                .await
                .map_err(|err| format!("H.265 recording encode worker: {err}"))?;
                encoder = returned;
                if let Some(limit) = output.append(&encoded?)? {
                    return Ok(limit);
                }
                status.frames_written += frame_count;
                status.bytes_written = output.bytes;
                updates.send_replace(status.clone());
            }
            if status.frames_written == status.max_frames {
                return Ok("frame_limit");
            }
            if let Some(reason) = reason {
                return Ok(reason);
            }
        }
        tokio::select! {
            biased;
            _ = recording.stop.cancelled() => reason = Some("stopped"),
            _ = tokio::time::sleep_until(deadline) => reason = Some("duration_limit"),
            _ = ticks.tick() => {
                match capture_scene(state, window, false).await {
                    Ok(Some(scene)) => {
                        missing_since = None;
                        if !previous.0.same_pixels(&scene) {
                            previous = tokio::task::spawn_blocking(move || {
                                let packed = pack_scene(&scene, size);
                                (scene, Arc::new(packed))
                            }).await.map_err(|err| format!("recording composition worker: {err}"))?;
                        }
                        frames.push(previous.1.clone());
                    }
                    Ok(None) => reason = Some("window_closed"),
                    Err(error) if error.starts_with("snapshot_unavailable:") => {
                        if missing_since.get_or_insert_with(Instant::now).elapsed() >= START_TIMEOUT {
                            status.error = Some(error);
                            reason = Some("capture_unavailable");
                        }
                        // Keep the last complete composition until all layers are readable.
                        frames.push(previous.1.clone());
                    }
                    Err(error) => {
                        status.error = Some(error);
                        reason = Some("capture_unavailable");
                    }
                }
            }
        }
    }
}

struct Output {
    file: tempfile::NamedTempFile,
    bytes: u64,
}

impl Output {
    fn append(&mut self, group: &[u8]) -> Result<Option<&'static str>, String> {
        let end = self.bytes + group.len() as u64;
        if end > MAX_BYTES {
            return Ok(Some("size_limit"));
        }
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(self.file.as_file().as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(format!(
                "check recording free space: {}",
                std::io::Error::last_os_error()
            ));
        }
        let stat = unsafe { stat.assume_init() };
        if stat.f_bavail.saturating_mul(stat.f_frsize) < FREE_SPACE_RESERVE + group.len() as u64 {
            return Ok(Some("disk_space"));
        }
        if let Err(error) = self.file.as_file().write_all_at(group, self.bytes) {
            // A short write must not leave a half-written access unit or pyramid.
            self.file
                .as_file()
                .set_len(self.bytes)
                .map_err(|rollback| {
                    format!("write recording: {error}; truncate incomplete group: {rollback}")
                })?;
            return Err(format!("write recording: {error}"));
        }
        self.bytes = end;
        Ok(None)
    }
}

fn pack_scene(scene: &scene::Scene<'static>, (width, height): (u32, u32)) -> Vec<u32> {
    let frame = scene.capture(true);
    let fit = (f64::from(width) / f64::from(frame.width))
        .min(f64::from(height) / f64::from(frame.height))
        .min(1.0);
    let fit_width = (f64::from(frame.width) * fit).round().max(1.0) as u32;
    let fit_height = (f64::from(frame.height) * fit).round().max(1.0) as u32;
    let mut packed = vec![0; width as usize * height as usize];
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut cb = 0.0;
            let mut cr = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = x + dx;
                    let py = y + dy;
                    let mut luma = 0.0;
                    if px < fit_width && py < fit_height {
                        let sx = px * frame.width / fit_width;
                        let sy = py * frame.height / fit_height;
                        let offset = ((sy * frame.width + sx) * 4) as usize;
                        let [r, g, b] = [
                            frame.rgba[offset],
                            frame.rgba[offset + 1],
                            frame.rgba[offset + 2],
                        ]
                        .map(|component| f32::from(component) / 255.0);
                        luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                        cb += (b - luma) / (2.0 * (1.0 - 0.0722));
                        cr += (r - luma) / (2.0 * (1.0 - 0.2126));
                    }
                    packed[(py * width + px) as usize] =
                        ((16.0 + 219.0 * luma).round() as u32) << 10;
                }
            }
            // The VA upload reads centered chroma from the upper-left word of the block.
            let cb = (128.0 + 56.0 * cb).round() as u32;
            let cr = (128.0 + 56.0 * cr).round() as u32;
            packed[(y * width + x) as usize] |= cb | (cr << 20);
        }
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_rejects_a_whole_group_before_crossing_the_byte_limit() -> Result<(), String> {
        let file = tempfile::NamedTempFile::new().map_err(|err| err.to_string())?;
        let mut output = Output {
            file,
            bytes: MAX_BYTES - 1,
        };
        assert_eq!(output.append(&[0, 0, 1])?, Some("size_limit"));
        assert_eq!(output.bytes, MAX_BYTES - 1);
        assert_eq!(output.file.as_file().metadata().unwrap().len(), 0);
        Ok(())
    }
}
