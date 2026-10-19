//! The actual outer speaker renderer, shared by CPAL and hardware-free tests.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::controls::{enabled, peak, AudioState};
use super::{PlaybackFn, Resampler, FRAME_SAMPLES, SAMPLE_RATE};

pub(super) struct Speaker {
    playback: PlaybackFn,
    audio: Arc<AudioState>,
    resampler: Resampler,
    chunk: [f32; FRAME_SAMPLES / 2],
    pending: Vec<f32>,
    cursor: usize,
    token: u64,
}

impl Speaker {
    pub(super) fn new(rate: u32, playback: PlaybackFn, audio: Arc<AudioState>) -> Self {
        // Resample one fixed chunk at a time, never an entire device buffer. The extra
        // samples cover the interpolator's carried fractional position. No growth or
        // per-callback allocation is needed, including large device callbacks.
        let capacity = ((FRAME_SAMPLES / 2 + 2) as u64 * u64::from(rate.max(1)))
            .div_ceil(u64::from(SAMPLE_RATE)) as usize
            + 4;
        Self {
            playback,
            audio,
            resampler: Resampler::new(SAMPLE_RATE, rate),
            chunk: [0.0; FRAME_SAMPLES / 2],
            pending: Vec::with_capacity(capacity),
            cursor: 0,
            token: 0,
        }
    }

    /// `convert` returns both the device sample and its actual normalized value, so
    /// telemetry measures what is written (including integer sample quantization).
    pub(super) fn render<T: Copy>(
        &mut self,
        out: &mut [T],
        channels: usize,
        mut convert: impl FnMut(f32) -> (T, f32),
    ) {
        if out.is_empty() {
            return;
        }
        let token = self.audio.output_token();
        if token != self.token || enabled(token) {
            self.pending.clear();
            self.cursor = 0;
            self.resampler.reset();
            self.chunk.fill(0.0);
            self.token = token;
        }
        if enabled(token) {
            // The inner fill only drains the ring. It must NOT acknowledge: previously
            // resampled pending audio could still be waiting outside that callback.
            (self.playback)(&mut self.chunk);
            let (silence, _) = convert(0.0);
            out.fill(silence);
            // This release follows the actual device writes AND the ring drain AND
            // clearing outer pending/resampler history. The worker observes it later.
            if self.audio.output_drained.load(Ordering::Acquire) == token {
                self.audio.output_applied.store(token, Ordering::Release);
            }
            return;
        }
        let meter = self.audio.levels_enabled();
        let mut output_peak = 0.0;
        for frame in out.chunks_mut(channels.max(1)) {
            while self.cursor == self.pending.len() {
                (self.playback)(&mut self.chunk);
                self.pending.clear();
                self.cursor = 0;
                self.resampler.process(&self.chunk, &mut self.pending);
            }
            let sample = self.pending[self.cursor];
            self.cursor += 1;
            let sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let (sample, actual) = convert(sample);
            frame.fill(sample);
            if meter {
                output_peak = peak(output_peak, actual);
            }
        }
        if meter {
            self.audio.output_peak(output_peak);
        }
        self.audio.output_applied.store(token, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::super::controls::{Controls, LEVELS_INTERVAL};
    use super::super::{audio_io, push_playback, Playback, PLAYBACK_CAPACITY};
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Instant;
    use tokio::sync::mpsc;

    #[test]
    fn suppression_waits_for_ring_drain_and_actual_outer_silence_clears_pending_and_history() {
        let audio = Arc::new(AudioState::default());
        let playback: Playback = Arc::new(Mutex::new(VecDeque::with_capacity(PLAYBACK_CAPACITY)));
        let (frames, _frame_rx) = mpsc::channel(1);
        let (internal, _internal_rx) = mpsc::unbounded_channel();
        let io = audio_io(frames, Arc::clone(&playback), Arc::clone(&audio), &internal);
        let mut speaker = Speaker::new(44_100, io.playback, Arc::clone(&audio));
        let capacity = speaker.pending.capacity();
        let mut controls = Controls::new(Arc::clone(&audio));
        push_playback(&playback, &audio, audio.output_token(), &[0.8; 2_000]);
        let mut out = [0.0; 37];
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert!(out.iter().any(|&sample| sample > 0.7));
        assert!(
            speaker.cursor < speaker.pending.len(),
            "stage stale outer pending audio"
        );
        assert_eq!(
            speaker.resampler.last, 0.8,
            "stage stale interpolation history"
        );

        controls
            .begin(Some(1), None, Some(true), None, Instant::now())
            .unwrap();
        let token = audio.output_token();
        // The inner callback drains the ring, but has not performed ANY outer
        // device writes. Both outer buffers still carry the audible old tone.
        // This was the false-positive acknowledgment boundary.
        (speaker.playback)(&mut [1.0; 480]);
        assert_eq!(audio.output_drained.load(Ordering::Acquire), token);
        assert!(playback.lock().unwrap().is_empty());
        assert!(speaker.pending[speaker.cursor..]
            .iter()
            .any(|&sample| sample > 0.7));
        assert_eq!(speaker.resampler.last, 0.8);
        assert!(out.iter().any(|&sample| sample > 0.7));
        assert!(controls.confirmed().is_none());
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert!(speaker.pending.is_empty());
        assert_eq!(speaker.resampler.last, 0.0);
        assert_eq!(audio.output_applied.load(Ordering::Acquire), token);
        assert!(controls.confirmed().unwrap().suppressed);
        push_playback(&playback, &audio, audio.output_token(), &[0.9; 960]);
        assert!(
            playback.lock().unwrap().is_empty(),
            "suppressed remote packets stay discarded"
        );

        // Even a repeated suppression must wait for this generation's ring drain;
        // a busy ring cannot be acknowledged just because outer output is silent.
        controls
            .begin(Some(2), None, Some(true), None, Instant::now())
            .unwrap();
        let busy_ring = playback.lock().unwrap();
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert!(controls.confirmed().is_none());
        drop(busy_ring);
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert!(controls.confirmed().unwrap().suppressed);

        // Unsuppress without fresh remote audio: no old pending or interpolated
        // tone may leak. Fresh remote audio must subsequently become audible.
        controls
            .begin(Some(3), None, Some(false), None, Instant::now())
            .unwrap();
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert!(!controls.confirmed().unwrap().suppressed);
        push_playback(&playback, &audio, 0, &[0.8; 960]);
        assert!(
            playback.lock().unwrap().is_empty(),
            "a late producer from the pre-suppression generation stays discarded after unsuppress"
        );
        push_playback(&playback, &audio, audio.output_token(), &[0.4; 4_000]);
        let mut audible = [0.0; 4_096];
        speaker.render(&mut audible, 1, |sample| (sample, sample));
        assert!(audible.iter().any(|&sample| sample > 0.3));
        assert!(audible.iter().all(|&sample| sample <= 0.4));
        assert_eq!(
            speaker.pending.capacity(),
            capacity,
            "large outer callbacks must not grow pending storage"
        );
    }

    #[test]
    fn output_levels_measure_actual_written_samples_not_inner_prefetch() {
        let audio = Arc::new(AudioState::default());
        let now = Instant::now();
        let mut controls = Controls::new(Arc::clone(&audio));
        controls
            .begin(Some(1), None, None, Some(true), now)
            .unwrap();
        assert!(controls.confirmed().is_some());
        let mut speaker = Speaker::new(
            48_000,
            Box::new(|out| {
                out.fill(0.9);
                out[0] = 0.2;
            }),
            Arc::clone(&audio),
        );
        let mut out = [0.0];
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert_eq!(out, [0.2]);
        assert_eq!(
            controls.take_levels(now + LEVELS_INTERVAL),
            Some((0.0, 0.2)),
            "prefetched 0.9 was not yet written to the speaker"
        );
    }
}
