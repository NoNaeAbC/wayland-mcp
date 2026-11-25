use super::{End, io_error};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const SAMPLE_LIFETIME_NS: u64 = 8 * NANOS_PER_SECOND;
const HW_CLOCK: u32 = 0x2;
const ZERO_COPY: u32 = 0x8;

// Leave room for the renderer, capture, encode, decode and compositor to
// overlap. Outstanding callbacks are still retired by the real compositor.
const FRAME_LEAD: usize = 5;
const RECENT_COMPOSITOR_CALLBACK: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(super) struct Frames {
    surfaces: HashMap<u32, SurfaceFrames>,
    callbacks: HashMap<u32, FrameCallback>,
}

#[derive(Default)]
struct SurfaceFrames {
    toplevel: bool,
    pending: Vec<u32>,
    in_flight: usize,
    deferred: VecDeque<u32>,
    last_compositor_callback: Option<Instant>,
}

struct FrameCallback {
    surface: Option<u32>,
    sent_early: bool,
    can_send_early: bool,
}

impl Frames {
    pub(super) fn toplevel(&mut self, surface: u32) {
        self.surfaces.entry(surface).or_default().toplevel = true;
    }

    pub(super) fn request(&mut self, surface: u32, callback: u32) {
        self.surfaces
            .entry(surface)
            .or_default()
            .pending
            .push(callback);
    }

    pub(super) fn commit(&mut self, id: u32, has_buffer: bool, now: Instant) -> Vec<u32> {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return Vec::new();
        };
        let callbacks = std::mem::take(&mut surface.pending);
        surface.in_flight += callbacks.len();
        let can_send_early = has_buffer && surface.toplevel;
        let sent_early = can_send_early
            && surface.deferred.is_empty()
            && surface.in_flight < FRAME_LEAD
            && surface.last_compositor_callback.is_some_and(|last| {
                now.saturating_duration_since(last) < RECENT_COMPOSITOR_CALLBACK
            });
        for &callback in &callbacks {
            self.callbacks.insert(
                callback,
                FrameCallback {
                    surface: Some(id),
                    sent_early,
                    can_send_early,
                },
            );
        }
        if sent_early {
            callbacks
        } else {
            surface.deferred.extend(callbacks);
            Vec::new()
        }
    }

    // A canceled callback releases credit without establishing visibility.
    pub(super) fn finish(
        &mut self,
        id: u32,
        compositor_callback: Option<Instant>,
    ) -> Option<(bool, Vec<u32>)> {
        let callback = self.callbacks.remove(&id)?;
        let mut ready = Vec::new();
        if let Some(surface) = callback.surface.and_then(|id| self.surfaces.get_mut(&id)) {
            surface.in_flight -= 1;
            if !callback.sent_early {
                surface.deferred.retain(|&deferred| deferred != id);
            }
            if let Some(now) = compositor_callback {
                surface.last_compositor_callback = Some(now);
                // The client already received this acknowledgment. Use its freed
                // slot for the oldest waiting callback so the pipeline stays full.
                if callback.sent_early && surface.in_flight < FRAME_LEAD {
                    while let Some(&next) = surface.deferred.front() {
                        let waiting = self.callbacks.get_mut(&next).unwrap();
                        if !waiting.can_send_early {
                            break;
                        }
                        waiting.sent_early = true;
                        surface.deferred.pop_front();
                        ready.push(next);
                    }
                }
            }
        }
        Some((callback.sent_early, ready))
    }

    pub(super) fn destroy(&mut self, surface: u32) {
        self.surfaces.remove(&surface);
        for callback in self.callbacks.values_mut() {
            if callback.surface == Some(surface) {
                callback.surface = None;
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Clock {
    source: Option<libc::clockid_t>,
    last_probe: Option<u64>,
    samples: VecDeque<Sample>,
    translations: VecDeque<(i128, i128)>,
}

struct Sample {
    received: u64,
    delay: u64,
    offset: i128,
}

impl Clock {
    pub(super) fn set_source(&mut self, clock: libc::clockid_t) -> Result<(), String> {
        if !matches!(
            clock,
            libc::CLOCK_MONOTONIC
                | libc::CLOCK_MONOTONIC_RAW
                | libc::CLOCK_MONOTONIC_COARSE
                | libc::CLOCK_BOOTTIME
                | libc::CLOCK_REALTIME
                | libc::CLOCK_REALTIME_COARSE
                | libc::CLOCK_TAI
        ) {
            return Err(format!("unsupported compositor presentation clock {clock}"));
        }
        if *self.source.get_or_insert(clock) != clock {
            return Err("compositor changed its presentation clock".into());
        }
        Ok(())
    }

    pub(super) fn probe(&mut self) -> Result<Option<u64>, String> {
        let now = monotonic()?;
        if self
            .last_probe
            .is_some_and(|last| now - last < NANOS_PER_SECOND)
        {
            return Ok(None);
        }
        self.last_probe = Some(now);
        Ok(Some(now))
    }

    pub(super) fn sample(
        &mut self,
        [sent, received, replied]: [u64; 3],
        now: u64,
    ) -> Result<(), String> {
        let local_elapsed = now.checked_sub(sent).ok_or("invalid clock probe time")?;
        let remote_elapsed = replied
            .checked_sub(received)
            .ok_or("invalid clock reply time")?;
        // Use the midpoints of the two measurements to estimate the offset.
        // The remote processing time is excluded from the round-trip delay.
        let offset = i128::from(sent.midpoint(now)) - i128::from(received.midpoint(replied));
        self.samples
            .retain(|sample| now - sample.received < SAMPLE_LIFETIME_NS);
        if self.samples.len() == 8 {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            received: now,
            delay: local_elapsed.saturating_sub(remote_elapsed),
            offset,
        });
        Ok(())
    }

    pub(super) fn presented(
        &mut self,
        end: End,
        shared_clock: bool,
        bytes: &mut [u8],
        seconds: u64,
        nanos: u32,
        flags: u32,
    ) -> Result<(), String> {
        if u64::from(nanos) >= NANOS_PER_SECOND {
            return Err("invalid compositor presentation timestamp".into());
        }
        let flags = flags & !ZERO_COPY;
        if shared_clock {
            bytes[32..36].copy_from_slice(&flags.to_ne_bytes());
            return Ok(());
        }

        let timestamp = i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(nanos);
        let translated = self.translate(end, timestamp)?.max(0);
        let seconds = u64::try_from(translated / i128::from(NANOS_PER_SECOND))
            .map_err(|_| "translated presentation timestamp overflow")?;
        let nanos = (translated % i128::from(NANOS_PER_SECOND)) as u32;
        bytes[8..12].copy_from_slice(&((seconds >> 32) as u32).to_ne_bytes());
        bytes[12..16].copy_from_slice(&(seconds as u32).to_ne_bytes());
        bytes[16..20].copy_from_slice(&nanos.to_ne_bytes());
        bytes[32..36].copy_from_slice(&(flags & !HW_CLOCK).to_ne_bytes());
        Ok(())
    }

    fn translate(&mut self, end: End, timestamp: i128) -> Result<i128, String> {
        // Repeated feedback for a commit must report the same time. An offset
        // sample may arrive between two events for the same presentation.
        if let Some((_, translated)) = self
            .translations
            .iter()
            .rev()
            .find(|(t, _)| *t == timestamp)
        {
            return Ok(*translated);
        }
        let offset = match end {
            End::Compositor => {
                let source = self
                    .source
                    .ok_or("missing compositor presentation clock ID")?;
                if source == libc::CLOCK_MONOTONIC {
                    return Ok(timestamp);
                }
                let before = monotonic()?;
                let clock = clock_time(source)?;
                let after = monotonic()?;
                i128::from(before.midpoint(after)) - i128::from(clock)
            }
            End::Application => {
                self.samples
                    .iter()
                    .rev()
                    .min_by_key(|sample| sample.delay)
                    .ok_or("missing SSH Wayland presentation clock sample")?
                    .offset
            }
        };
        let translated = timestamp + offset;
        if self.translations.len() == 64 {
            self.translations.pop_front();
        }
        self.translations.push_back((timestamp, translated));
        Ok(translated)
    }
}

pub(super) fn monotonic() -> Result<u64, String> {
    clock_time(libc::CLOCK_MONOTONIC)
}

fn clock_time(clock: libc::clockid_t) -> Result<u64, String> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(NANOS_PER_SECOND))
        .and_then(|seconds| seconds.checked_add(time.tv_nsec as u64))
        .ok_or_else(|| "presentation clock out of range".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_callbacks_pipeline_with_bounded_compositor_credit() {
        let mut frames = Frames::default();
        let now = Instant::now();
        let surface = 2;
        frames.toplevel(surface);
        frames.request(surface, 10);
        assert!(frames.commit(surface, true, now).is_empty());
        assert_eq!(frames.finish(10, Some(now)), Some((false, vec![])));

        for callback in 11..10 + FRAME_LEAD as u32 {
            frames.request(surface, callback);
            assert_eq!(frames.commit(surface, true, now), [callback]);
        }
        frames.request(surface, 20);
        assert!(frames.commit(surface, true, now).is_empty());
        assert_eq!(frames.finish(11, Some(now)), Some((true, vec![20])));
        for callback in 12..10 + FRAME_LEAD as u32 {
            assert_eq!(frames.finish(callback, Some(now)), Some((true, vec![])));
        }
        assert_eq!(frames.finish(20, Some(now)), Some((true, vec![])));

        frames.request(surface, 22);
        assert!(frames.commit(surface, false, now).is_empty());
        assert_eq!(frames.finish(22, None), Some((false, vec![])));
        frames.request(surface, 23);
        assert!(
            frames
                .commit(surface, true, now + Duration::from_secs(1))
                .is_empty()
        );
    }

    #[test]
    fn translates_distinct_clock_epochs_using_the_lowest_network_delay() -> Result<(), String> {
        let mut clock = Clock::default();
        clock.sample([1_000, 100_700, 100_900], 1_400)?;
        clock.sample([2_000, 102_000, 102_020], 2_600)?;
        assert_eq!(clock.translate(End::Application, 101_000)?, 1_400);

        let sent = SAMPLE_LIFETIME_NS + 3_000;
        clock.sample([sent, sent + 100_900, sent + 101_000], sent + 300)?;
        assert_eq!(
            clock.translate(End::Application, i128::from(sent) + 101_100)?,
            i128::from(sent) + 300
        );
        assert_eq!(clock.translate(End::Application, 101_000)?, 1_400);
        Ok(())
    }
}
