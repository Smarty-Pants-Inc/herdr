//! Ordered device controls and callback-to-worker confirmations. No callback sends events.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::peer::PeerInitialState;

pub(super) const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
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
    /// Publish protection before constructing callbacks or opening either stream. Zero
    /// applied tokens deliberately mean no real encoder/outer-render observation yet.
    pub(super) fn with_initial(initial: PeerInitialState) -> Self {
        Self {
            input_request: AtomicU64::new(2 | u64::from(initial.input_muted)),
            output_request: AtomicU64::new(2 | u64::from(initial.output_suppressed)),
            ..Self::default()
        }
    }

    /// Terminal-only fail-stop. Never manufacture callback evidence or resume controls
    /// after this operation; no generation allocation may prevent protective teardown.
    pub(super) fn force_protective_gates(&self) {
        self.input_request.fetch_or(1, Ordering::AcqRel);
        self.output_request.fetch_or(1, Ordering::AcqRel);
        self.levels.store(false, Ordering::Release);
        self.input_peak.store(0, Ordering::Relaxed);
        self.output_peak.store(0, Ordering::Relaxed);
    }

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
    input: u64,
    output: u64,
    deadline: Instant,
}

pub(super) enum ControlOutcome {
    Applied(Confirmation),
    Failed { control_id: Option<u64> },
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
            // Snapshot current requests, not cached combined observations. Omitted
            // axes can still be unobserved after an earlier command timed out.
            input: self.audio.input_token(),
            output: self.audio.output_token(),
            deadline: now + CONTROL_TIMEOUT,
        });
        Ok(())
    }

    pub(super) fn poll(&mut self, now: Instant) -> Option<ControlOutcome> {
        let input = self.audio.input_token();
        let output = self.audio.output_token();
        let input_applied = self.audio.input_applied.load(Ordering::Acquire);
        let output_applied = self.audio.output_applied.load(Ordering::Acquire);
        // Legacy State reports only independently observed input, including when
        // output is withheld or input is first discovered at the failure deadline.
        if input_applied == input {
            self.muted = enabled(input);
        }
        if output_applied == output {
            self.suppressed = enabled(output);
        }
        let pending = self.pending.as_ref()?;
        if now >= pending.deadline {
            let pending = self.pending.take()?;
            return Some(ControlOutcome::Failed {
                control_id: pending.control_id,
            });
        }
        if input_applied != pending.input
            || (pending.control_id.is_some() && output_applied != pending.output)
        {
            return None;
        }
        let pending = self.pending.take()?;
        Some(ControlOutcome::Applied(Confirmation {
            control_id: pending.control_id,
            muted: enabled(pending.input),
            suppressed: self.suppressed,
        }))
    }

    /// Compatibility for read-only speaker unit fixtures, never a production bypass.
    #[cfg(test)]
    pub(super) fn confirmed(&mut self) -> Option<Confirmation> {
        match self.poll(Instant::now())? {
            ControlOutcome::Applied(confirmation) => Some(confirmation),
            ControlOutcome::Failed { .. } => None,
        }
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

    // Keep: #316 device-data safety and deadline failure paths are deterministic
    // here; the native integration tests separately exercise real callback evidence.
    #[test]
    fn initial_gates_are_unobserved_and_terminal_gates_never_fake_evidence() {
        let audio = AudioState::with_initial(PeerInitialState {
            input_muted: true,
            output_suppressed: false,
        });
        assert_eq!((audio.input_token(), audio.output_token()), (3, 2));
        assert_eq!(audio.input_applied.load(Ordering::Acquire), 0);
        assert_eq!(audio.output_drained.load(Ordering::Acquire), 0);
        assert_eq!(audio.output_applied.load(Ordering::Acquire), 0);
        audio.force_protective_gates();
        assert_eq!((audio.input_token(), audio.output_token()), (3, 3));
        assert!(!audio.levels_enabled());
        assert_eq!(audio.input_applied.load(Ordering::Acquire), 0);
        assert_eq!(audio.output_applied.load(Ordering::Acquire), 0);
    }

    #[test]
    fn timeout_is_take_once_and_omitted_axes_wait_for_current_observation() {
        let now = Instant::now();
        let audio = Arc::new(AudioState::with_initial(PeerInitialState::default()));
        let mut controls = Controls::new(Arc::clone(&audio));
        controls
            .begin(Some(1), Some(true), Some(true), None, now)
            .unwrap();
        audio
            .input_applied
            .store(audio.input_token(), Ordering::Release);
        assert!(controls
            .poll(now + CONTROL_TIMEOUT - Duration::from_nanos(1))
            .is_none());
        assert!(matches!(
            controls.poll(now + CONTROL_TIMEOUT),
            Some(ControlOutcome::Failed {
                control_id: Some(1)
            })
        ));
        assert!(controls.poll(now + CONTROL_TIMEOUT).is_none());
        assert!(controls.muted);
        assert!(!controls.suppressed);
        controls
            .begin(Some(2), None, None, Some(true), now + CONTROL_TIMEOUT)
            .unwrap();
        assert!(controls.poll(now + CONTROL_TIMEOUT).is_none());
        audio
            .output_applied
            .store(audio.output_token(), Ordering::Release);
        match controls.poll(now + CONTROL_TIMEOUT) {
            Some(ControlOutcome::Applied(confirmation)) => {
                assert_eq!(confirmation.control_id, Some(2));
                assert!(confirmation.muted && confirmation.suppressed);
            }
            _ => panic!("current omitted output still needs its real observation"),
        }
    }

    #[test]
    fn legacy_is_input_only_and_deadline_reports_last_observed_input() {
        let now = Instant::now();
        let audio = Arc::new(AudioState::with_initial(PeerInitialState::default()));
        let mut controls = Controls::new(Arc::clone(&audio));
        controls.begin(None, Some(true), None, None, now).unwrap();
        assert!(matches!(
            controls.poll(now + CONTROL_TIMEOUT),
            Some(ControlOutcome::Failed { control_id: None })
        ));
        assert!(!controls.muted, "a request is not an observed State");
        controls
            .begin(None, Some(true), None, None, now + CONTROL_TIMEOUT)
            .unwrap();
        audio
            .input_applied
            .store(audio.input_token(), Ordering::Release);
        assert!(matches!(
            controls.poll(now + CONTROL_TIMEOUT),
            Some(ControlOutcome::Applied(_))
        ));
        assert!(controls.muted);
        assert_eq!(audio.output_applied.load(Ordering::Acquire), 0);
    }

    #[test]
    fn first_observation_at_deadline_fails_without_rolling_back_truthful_axes() {
        let now = Instant::now();
        let audio = Arc::new(AudioState::with_initial(PeerInitialState::default()));
        let mut controls = Controls::new(Arc::clone(&audio));
        controls
            .begin(Some(1), Some(true), Some(true), None, now)
            .unwrap();
        audio
            .input_applied
            .store(audio.input_token(), Ordering::Release);
        audio
            .output_applied
            .store(audio.output_token(), Ordering::Release);
        assert!(matches!(
            controls.poll(now + CONTROL_TIMEOUT),
            Some(ControlOutcome::Failed {
                control_id: Some(1)
            })
        ));
        assert!(controls.muted && controls.suppressed);
        assert!(controls.poll(now + CONTROL_TIMEOUT).is_none());
        assert!(enabled(audio.input_token()) && enabled(audio.output_token()));
    }

    #[test]
    fn partial_output_timeout_and_late_input_cannot_confirm_explicit_false_successor() {
        let now = Instant::now();
        let audio = Arc::new(AudioState::with_initial(PeerInitialState::default()));
        let mut controls = Controls::new(Arc::clone(&audio));
        controls
            .begin(Some(1), Some(true), Some(true), None, now)
            .unwrap();
        let stale_input = audio.input_token();
        audio
            .output_applied
            .store(audio.output_token(), Ordering::Release);
        assert!(matches!(
            controls.poll(now + CONTROL_TIMEOUT),
            Some(ControlOutcome::Failed {
                control_id: Some(1)
            })
        ));
        assert!(!controls.muted && controls.suppressed);
        controls
            .begin(Some(2), Some(false), None, None, now + CONTROL_TIMEOUT)
            .unwrap();
        audio.input_applied.store(stale_input, Ordering::Release);
        assert!(controls.poll(now + CONTROL_TIMEOUT).is_none());
        audio
            .input_applied
            .store(audio.input_token(), Ordering::Release);
        match controls.poll(now + CONTROL_TIMEOUT) {
            Some(ControlOutcome::Applied(confirmation)) => {
                assert_eq!(confirmation.control_id, Some(2));
                assert!(!confirmation.muted && confirmation.suppressed);
            }
            _ => panic!("only fresh input may confirm the explicit-false successor"),
        }
        assert!(controls.poll(now + CONTROL_TIMEOUT).is_none());
    }

    #[test]
    fn exhausted_generation_publishes_neither_axis_and_repeated_values_advance() {
        let audio = Arc::new(AudioState::with_initial(PeerInitialState::default()));
        let mut controls = Controls::new(Arc::clone(&audio));
        audio.output_request.store(u64::MAX, Ordering::Release);
        assert!(controls
            .begin(Some(1), Some(true), Some(false), None, Instant::now())
            .is_err());
        assert_eq!(audio.input_token(), 2);
        assert_eq!(audio.output_token(), u64::MAX);
        audio.output_request.store(2, Ordering::Release);
        controls
            .begin(None, Some(false), None, None, Instant::now())
            .unwrap();
        assert_eq!(audio.input_token(), 4);
        audio.input_applied.store(4, Ordering::Release);
        assert!(controls.confirmed().is_some());
        controls
            .begin(None, Some(false), None, None, Instant::now())
            .unwrap();
        assert_eq!(audio.input_token(), 6);
        audio.input_applied.store(4, Ordering::Release);
        assert!(controls.confirmed().is_none());
    }

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
