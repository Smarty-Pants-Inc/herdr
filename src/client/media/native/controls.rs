//! Ordered device controls and callback-to-worker confirmations. No callback sends events.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const LEVELS_INTERVAL: Duration = Duration::from_millis(100);

/// A request token combines a monotonically increasing generation and its boolean value.
/// Only the control worker writes requests; the encoder and outer speaker callback confirm.
#[derive(Default)]
pub(crate) struct AudioState {
    pub(super) input_request: AtomicU64,
    pub(super) input_applied: AtomicU64,
    pub(super) output_request: AtomicU64,
    pub(super) output_drained: AtomicU64,
    pub(super) output_applied: AtomicU64,
    pub(super) device_changed: AtomicBool,
    levels: AtomicBool,
    input_peak: AtomicU32,
    output_peak: AtomicU32,
}

impl AudioState {
    pub(super) fn input_token(&self) -> u64 {
        self.input_request.load(Ordering::Acquire)
    }

    pub(super) fn output_token(&self) -> u64 {
        self.output_request.load(Ordering::Acquire)
    }

    pub(super) fn levels_enabled(&self) -> bool {
        self.levels.load(Ordering::Relaxed)
    }

    pub(super) fn input_peak(&self, samples: &[f32]) {
        if self.levels_enabled() {
            record_peak(&self.input_peak, samples.iter().copied().fold(0.0, peak));
        }
    }

    pub(super) fn output_peak(&self, value: f32) {
        if self.levels_enabled() {
            record_peak(&self.output_peak, value);
        }
    }
}

pub(super) fn enabled(token: u64) -> bool {
    token & 1 != 0
}

pub(super) fn peak(previous: f32, sample: f32) -> f32 {
    if sample.is_finite() {
        previous.max(sample.abs().min(1.0))
    } else {
        previous
    }
}

fn record_peak(atomic: &AtomicU32, value: f32) {
    // Positive finite f32 bit patterns have the same ordering as their values.
    atomic.fetch_max(peak(0.0, value).to_bits(), Ordering::Relaxed);
}

struct Pending {
    control_id: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
    muted: bool,
    suppressed: bool,
}

pub(super) struct Confirmation {
    /// None denotes the legacy media.mute.v1 command, confirmed with a State event.
    pub(super) control_id: Option<u64>,
    pub(super) muted: bool,
    pub(super) suppressed: bool,
}

/// One in-flight control. The bounded command receiver is not polled until it confirms,
/// so subsequent controls cannot overwrite requests the audio workers have not seen.
pub(super) struct Controls {
    pub(super) audio: Arc<AudioState>,
    pub(super) muted: bool,
    pub(super) suppressed: bool,
    pending: Option<Pending>,
    last_levels: Option<Instant>,
}

impl Controls {
    pub(super) fn new(audio: Arc<AudioState>) -> Self {
        Self {
            audio,
            muted: false,
            suppressed: false,
            pending: None,
            last_levels: None,
        }
    }

    pub(super) fn idle(&self) -> bool {
        self.pending.is_none()
    }

    pub(super) fn observing(&self) -> bool {
        !self.idle() || self.audio.levels_enabled()
    }

    pub(super) fn begin(
        &mut self,
        control_id: Option<u64>,
        input_muted: Option<bool>,
        output_suppressed: Option<bool>,
        levels: Option<bool>,
        now: Instant,
    ) -> Result<(), String> {
        if !self.idle() {
            return Err("a media control is still awaiting the audio devices".to_owned());
        }
        let next = |token: u64, value: bool| {
            (token & !1)
                .checked_add(2)
                .map(|generation| generation | u64::from(value))
                .ok_or_else(|| "media control generation exhausted".to_owned())
        };
        let input = input_muted
            .map(|value| next(self.audio.input_token(), value))
            .transpose()?;
        let output = output_suppressed
            .map(|value| next(self.audio.output_token(), value))
            .transpose()?;
        if let Some(token) = input {
            // Mute discards the pre-mute meter window; unmute starts a fresh one.
            // Clear before publishing so a new-generation peak cannot be erased.
            // An old encoder may still record after this clear, so take_levels also
            // masks requested mute, even before the encoder confirms it.
            self.audio.input_peak.store(0, Ordering::Relaxed);
            self.audio.input_request.store(token, Ordering::Release);
        }
        if let Some(token) = output {
            self.audio.output_request.store(token, Ordering::Release);
        }
        if let Some(levels) = levels {
            if levels != self.audio.levels_enabled() {
                self.audio.levels.store(false, Ordering::Relaxed);
                self.audio.input_peak.store(0, Ordering::Relaxed);
                self.audio.output_peak.store(0, Ordering::Relaxed);
                self.last_levels = levels.then_some(now);
                self.audio.levels.store(levels, Ordering::Relaxed);
            }
        }
        self.pending = Some(Pending {
            control_id,
            input,
            output,
            muted: input_muted.unwrap_or(self.muted),
            suppressed: output_suppressed.unwrap_or(self.suppressed),
        });
        Ok(())
    }

    pub(super) fn confirmed(&mut self) -> Option<Confirmation> {
        let pending = self.pending.as_ref()?;
        if pending
            .input
            .is_some_and(|token| self.audio.input_applied.load(Ordering::Acquire) != token)
            || pending
                .output
                .is_some_and(|token| self.audio.output_applied.load(Ordering::Acquire) != token)
        {
            return None;
        }
        let pending = self.pending.take()?;
        self.muted = pending.muted;
        self.suppressed = pending.suppressed;
        Some(Confirmation {
            control_id: pending.control_id,
            muted: self.muted,
            suppressed: self.suppressed,
        })
    }

    pub(super) fn take_levels(&mut self, now: Instant) -> Option<(f32, f32)> {
        if !self.audio.levels_enabled()
            || now.saturating_duration_since(self.last_levels?) < LEVELS_INTERVAL
        {
            return None;
        }
        self.last_levels = Some(now);
        let input_peak = f32::from_bits(self.audio.input_peak.swap(0, Ordering::Relaxed));
        Some((
            if enabled(self.audio.input_token()) {
                // Always drain, but never emit even a late pre-mute peak. Requests
                // and emission share this worker; confirmed state can still lag.
                0.0
            } else {
                input_peak
            },
            f32::from_bits(self.audio.output_peak.swap(0, Ordering::Relaxed)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_opt_in_finite_clamped_and_throttled() {
        let now = Instant::now();
        let audio = Arc::new(AudioState::default());
        let mut controls = Controls::new(Arc::clone(&audio));
        audio.input_peak(&[0.8]);
        audio.output_peak(0.5);
        assert_eq!(controls.take_levels(now + LEVELS_INTERVAL), None);
        controls
            .begin(Some(1), None, None, Some(true), now)
            .unwrap();
        assert!(controls.confirmed().is_some());
        audio.input_peak(&[f32::NAN, f32::INFINITY, -0.75, 0.2]);
        audio.output_peak(-2.0);
        assert_eq!(controls.take_levels(now + Duration::from_millis(99)), None);
        assert_eq!(
            controls.take_levels(now + LEVELS_INTERVAL),
            Some((0.75, 1.0))
        );
        assert_eq!(controls.take_levels(now + LEVELS_INTERVAL), None);
        audio.input_peak(&[f32::NEG_INFINITY, f32::NAN]);
        assert_eq!(
            controls.take_levels(now + LEVELS_INTERVAL * 2),
            Some((0.0, 0.0))
        );
        controls
            .begin(Some(2), None, None, Some(false), now + LEVELS_INTERVAL * 2)
            .unwrap();
        audio.input_peak(&[0.9]);
        audio.output_peak(0.8);
        assert_eq!(controls.take_levels(now + LEVELS_INTERVAL * 10), None);
    }

    #[test]
    fn mute_discards_stale_input_levels_before_confirmation_and_unmute_starts_fresh() {
        // Both acknowledged controls and legacy mute use this same request boundary.
        for control_id in [Some(2), None] {
            let now = Instant::now();
            let audio = Arc::new(AudioState::default());
            let mut controls = Controls::new(Arc::clone(&audio));
            controls
                .begin(Some(1), None, None, Some(true), now)
                .unwrap();
            assert!(controls.confirmed().is_some());
            audio.input_peak(&[0.9]);
            audio.output_peak(0.5);
            assert_eq!(
                f32::from_bits(audio.input_peak.load(Ordering::Relaxed)),
                0.9
            );

            controls
                .begin(control_id, Some(true), None, Some(true), now)
                .unwrap();
            assert_eq!(audio.input_peak.load(Ordering::Relaxed), 0);
            assert!(controls.confirmed().is_none());
            assert!(!controls.muted, "the confirmation must still be pending");
            // Deterministically model an old encoder recording after the request's
            // clear. Masking must use the request, not the last confirmed state.
            record_peak(&audio.input_peak, 0.8);
            assert_eq!(
                controls.take_levels(now + LEVELS_INTERVAL),
                Some((0.0, 0.5)),
                "mute must not suppress independent speaker telemetry"
            );
            assert_eq!(audio.input_peak.load(Ordering::Relaxed), 0);

            record_peak(&audio.input_peak, 0.7);
            audio
                .input_applied
                .store(audio.input_token(), Ordering::Release);
            let confirmation = controls.confirmed().unwrap();
            assert_eq!(confirmation.control_id, control_id);
            assert!(confirmation.muted);
            controls
                .begin(Some(3), Some(false), None, None, now + LEVELS_INTERVAL)
                .unwrap();
            assert_eq!(audio.input_peak.load(Ordering::Relaxed), 0);
            assert_eq!(
                controls.take_levels(now + LEVELS_INTERVAL * 2),
                Some((0.0, 0.0)),
                "unmute must not resurrect an unconsumed pre-mute peak"
            );
            audio.input_peak(&[-0.4]);
            audio.output_peak(0.3);
            assert_eq!(
                controls.take_levels(now + LEVELS_INTERVAL * 3),
                Some((0.4, 0.3))
            );
        }
    }

    #[test]
    fn later_controls_cannot_overwrite_unconfirmed_requests() {
        let audio = Arc::new(AudioState::default());
        let mut controls = Controls::new(Arc::clone(&audio));
        controls
            .begin(Some(10), Some(true), Some(true), None, Instant::now())
            .unwrap();
        let input = audio.input_token();
        let output = audio.output_token();
        assert!(controls
            .begin(Some(11), Some(false), Some(false), None, Instant::now())
            .is_err());
        assert_eq!((audio.input_token(), audio.output_token()), (input, output));
        audio.input_applied.store(input, Ordering::Release);
        assert!(controls.confirmed().is_none());
        audio.output_applied.store(output, Ordering::Release);
        assert_eq!(controls.confirmed().unwrap().control_id, Some(10));
        controls
            .begin(Some(11), Some(false), Some(false), None, Instant::now())
            .unwrap();
        assert!(controls.confirmed().is_none());
    }
}
