//! Remote clock discipline.
//!
//! Peers agree on a tick index, not on wall-clock time, so every client needs an estimate of how
//! far its own clock sits from the server's. That estimate is noisy: a sample's apparent offset is
//! corrupted by however long the round trip happened to queue for.
//!
//! [`ClockEstimator`] keeps a small window of `(rtt, offset)` samples and applies the standard
//! trick of trusting the **lowest-RTT half** — a sample that came back fast spent the least time
//! queued, so its offset reading is the least polluted. Correction is then applied as a bounded
//! *time stretch* rather than a jump, so the simulation speeds up or slows down by a few percent
//! instead of teleporting. Only a genuinely large offset earns a hard reseek.
//!
//! **Every stored sample is moved by the correction applied after it was taken**
//! ([`ClockEstimator::apply_local_correction`]).
//!
//! - A sample is the offset at the moment its pong arrived. A stretch of `s` held for `t` wall
//!   seconds moves the local clock `(s - 1) * t` further than the remote one, so the offset every
//!   older sample recorded is out of date by exactly that much.
//! - Without the shift, the window reports the offset as it stood up to a window's length ago
//!   (eight samples at the four-per-second ping rate is two seconds). The stretch keeps correcting an
//!   error it has already removed, overshoots, and the sign flips. The loop then cycles between the
//!   two stretch bounds.
//! - Measured on a loaded two-core host with a 15 ms link: the offset swung about 90 ms peak to peak
//!   with a period of a few seconds and the stretch sat at each bound in turn.
//! - The shift adds no lag, and the lowest-RTT half still rejects queued samples as before.

/// Number of `(rtt, offset)` samples kept in the estimation window.
pub const DEFAULT_SAMPLE_CAPACITY: usize = 8;

/// Seconds over which the decoupled client's stretch closes the error it measures.
///
/// The stretch is `1 + error / window`, clamped to the configured bound. At `0.5` and the default
/// `1.05` bound the stretch is linear for an error inside 25 ms and saturates beyond it.
pub const STRETCH_CORRECTION_WINDOW_SECONDS: f64 = 0.5;

/// A single clock observation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    rtt: f64,
    offset: f64,
}

/// Rolling estimate of round-trip time, jitter, and local-vs-remote clock offset.
#[derive(Debug, Clone)]
pub struct ClockEstimator {
    samples: Vec<Sample>,
    capacity: usize,
    next: usize,
}

impl ClockEstimator {
    /// Create an estimator holding `capacity` samples (minimum 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: Vec::with_capacity(capacity),
            capacity,
            next: 0,
        }
    }

    /// How many samples the window currently holds.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Whether the window holds enough samples to be trusted.
    #[must_use]
    pub fn is_ready(&self, min_samples: usize) -> bool {
        self.samples.len() >= min_samples.max(1)
    }

    /// Drop every sample. Used on disconnect so a new session never inherits a stale estimate.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.next = 0;
    }

    /// Record one observation.
    ///
    /// `rtt` is the measured round trip in seconds and `offset` is `remote_time - local_time` in
    /// seconds, so a positive offset means the local clock is running behind. Non-finite values and
    /// negative round trips are rejected rather than stored, since a single `NaN` would otherwise
    /// poison every statistic derived from the window.
    pub fn push_sample(&mut self, rtt: f64, offset: f64) -> bool {
        if !rtt.is_finite() || !offset.is_finite() || rtt < 0.0 {
            return false;
        }
        let sample = Sample { rtt, offset };
        if self.samples.len() < self.capacity {
            self.samples.push(sample);
        } else {
            self.samples[self.next] = sample;
        }
        self.next = (self.next + 1) % self.capacity;
        true
    }

    /// Account for a correction the caller applied to its own clock after the stored samples were
    /// taken.
    ///
    /// `seconds` is how much further the local clock advanced than wall time because the caller
    /// chose to move it: `(stretch - 1) * wall_seconds` for a stretched frame, positive when the
    /// local clock was sped up. Each stored offset is `remote - local`, so each shrinks by
    /// `seconds`, and [`Self::offset`] then describes the clocks as they stand now. See the module
    /// header for what the unshifted window did.
    ///
    /// Only a DELIBERATE correction belongs here. A local clock that fell behind wall time on its
    /// own, such as a stall, is what the samples measure. A non-finite value is ignored.
    pub fn apply_local_correction(&mut self, seconds: f64) {
        if !seconds.is_finite() {
            return;
        }
        for sample in &mut self.samples {
            sample.offset -= seconds;
        }
    }

    /// Mean round-trip time in seconds, or 0 when no samples have arrived.
    #[must_use]
    pub fn rtt(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().map(|s| s.rtt).sum::<f64>() / self.samples.len() as f64
    }

    /// Mean absolute deviation of round-trip time in seconds.
    ///
    /// Mean absolute deviation rather than standard deviation: it is what the netbench gates
    /// already report, and it does not over-weight the single worst sample in a small window.
    #[must_use]
    pub fn jitter(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let mean = self.rtt();
        self.samples
            .iter()
            .map(|s| (s.rtt - mean).abs())
            .sum::<f64>()
            / self.samples.len() as f64
    }

    /// Filtered clock offset in seconds (`remote - local`).
    ///
    /// Averages the offsets of the lowest-RTT half of the window.
    #[must_use]
    pub fn offset(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut ordered: Vec<Sample> = self.samples.clone();
        // Every stored sample is finite, so this comparison is total.
        ordered.sort_by(|a, b| a.rtt.partial_cmp(&b.rtt).expect("samples are finite"));
        let keep = ordered.len().div_ceil(2);
        ordered.iter().take(keep).map(|s| s.offset).sum::<f64>() / keep as f64
    }

    /// The multiplier to apply to local time so the offset closes over `correction_window`.
    ///
    /// Returned in `1.0 / max_stretch ..= max_stretch`. Greater than 1 means the local clock is
    /// behind and should run faster.
    #[must_use]
    pub fn stretch(&self, max_stretch: f64, correction_window: f64) -> f64 {
        if !max_stretch.is_finite() || max_stretch <= 1.0 {
            return 1.0;
        }
        if !correction_window.is_finite() || correction_window <= 0.0 {
            return 1.0;
        }
        let raw = 1.0 + self.offset() / correction_window;
        raw.clamp(1.0 / max_stretch, max_stretch)
    }

    /// [`Self::stretch`] with an extra offset (seconds) added to the measured one.
    ///
    /// The adaptive-lead loop chases `measured offset + lead bias`: the bias deliberately holds
    /// the local clock ahead of the server so input arrives with margin.
    #[must_use]
    pub fn stretch_with(&self, extra_offset: f64, max_stretch: f64, correction_window: f64) -> f64 {
        if !max_stretch.is_finite() || max_stretch <= 1.0 {
            return 1.0;
        }
        if !correction_window.is_finite() || correction_window <= 0.0 {
            return 1.0;
        }
        if !extra_offset.is_finite() {
            return self.stretch(max_stretch, correction_window);
        }
        let raw = 1.0 + (self.offset() + extra_offset) / correction_window;
        raw.clamp(1.0 / max_stretch, max_stretch)
    }

    /// Whether the offset is too large to walk off with a stretch and needs a hard reseek.
    ///
    /// **Prefer [`Self::needs_hard_resync_with_lead`].** This form compares the RAW offset, which is only
    /// the control error for a peer that wants zero offset. A client does not: it must run AHEAD of the
    /// server so its input arrives before the tick that consumes it, so its offset settles at minus the
    /// lead it has dialed in, and testing the raw value fires the panic path on a perfectly healthy client.
    #[must_use]
    pub fn needs_hard_resync(&self, panic_threshold: f64) -> bool {
        self.needs_hard_resync_with_lead(panic_threshold, 0.0)
    }

    /// Whether the RESIDUAL — the error the caller's controller is actually driving to zero — is too large
    /// to walk off with a stretch.
    ///
    /// `lead_seconds` is how far ahead of the server the caller intends to run, so the residual is
    /// `offset + lead_seconds` and a client holding exactly its intended lead reports zero however large
    /// that lead is. Comparing the raw offset instead made the panic path self-sustaining: it fired on a
    /// correctly-leading client, the reseek that followed targeted zero offset and discarded the lead, the
    /// controller drove straight back to it, and it fired again — measured at about thirty hard resyncs per
    /// minute on a rendered client over a LAN, each one reseeking the tick and forcing a full snapshot.
    /// **A PANIC PATH MAY NOT FIRE ON THE ABSENCE OF A MEASUREMENT.** With no samples `offset()` reports
    /// `0.0` — which is not "the clocks agree", it is "nobody has looked" — and the residual then reads as
    /// the whole intended lead. That is not hypothetical: the reseek this test guards calls
    /// [`Self::clear`] itself, so the very next tick evaluates a residual derived from nothing. At 60 Hz the
    /// clamped 8-tick lead is 133 ms and stays under the 250 ms threshold by luck; at the 30 Hz decoupled
    /// tick the 100-player target runs on it is 267 ms, and the reseek re-armed itself every tick, restoring
    /// exactly the storm the lead term was added to stop — at the one rate nothing measures.
    #[must_use]
    pub fn needs_hard_resync_with_lead(&self, panic_threshold: f64, lead_seconds: f64) -> bool {
        if !panic_threshold.is_finite() || panic_threshold <= 0.0 || self.samples.is_empty() {
            return false;
        }
        let lead = if lead_seconds.is_finite() {
            lead_seconds
        } else {
            0.0
        };
        (self.offset() + lead).abs() > panic_threshold
    }
}

impl Default for ClockEstimator {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_SAMPLE_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client holding exactly the lead it intends is NOT in trouble, however big the lead is.
    #[test]
    fn a_client_holding_its_intended_lead_does_not_panic() {
        let mut clock = ClockEstimator::default();
        // 8 ticks of lead at 60 Hz is 133 ms, which is what `lead_bias_ticks` clamps to.
        let lead = 8.0 / 60.0;
        for _ in 0..4 {
            clock.push_sample(0.02, -lead);
        }
        assert!(
            !clock.needs_hard_resync_with_lead(0.25, lead),
            "offset == -lead is zero residual: the controller is exactly where it wants to be"
        );
    }

    /// ...and the loop this replaced: a lead large enough to trip the raw threshold on its own.
    #[test]
    fn the_raw_form_fires_on_a_healthy_client_and_the_lead_aware_form_does_not() {
        let mut clock = ClockEstimator::default();
        let lead = 0.30; // deliberately past the 0.25 panic threshold
        for _ in 0..4 {
            clock.push_sample(0.02, -lead);
        }
        assert!(
            clock.needs_hard_resync(0.25),
            "the raw offset alone trips the threshold -- this is the bug"
        );
        assert!(
            !clock.needs_hard_resync_with_lead(0.25, lead),
            "but the residual is zero, so nothing is wrong and nothing should reseek"
        );
    }

    /// Genuine trouble must still be caught, or the panic path stops doing its job.
    #[test]
    fn a_real_excursion_still_reseeks() {
        let mut clock = ClockEstimator::default();
        let lead = 8.0 / 60.0;
        for _ in 0..4 {
            clock.push_sample(0.02, -lead - 0.5);
        }
        assert!(
            clock.needs_hard_resync_with_lead(0.25, lead),
            "half a second of residual is exactly what the reseek exists for"
        );
        let mut ahead = ClockEstimator::default();
        for _ in 0..4 {
            ahead.push_sample(0.02, 0.9);
        }
        assert!(
            ahead.needs_hard_resync_with_lead(0.25, lead),
            "and it is symmetric -- a client far BEHIND is in trouble too"
        );
    }

    /// THE RESEEK MUST NOT RE-ARM ITSELF, and the rate where it did is the one nothing measures.
    ///
    /// `maybe_hard_resync` clears the estimator as part of reseeking, so the tick immediately after a reseek
    /// asks this question with an EMPTY window. `offset()` answers `0.0` there -- meaning "nobody has looked",
    /// not "the clocks agree" -- and the residual then reads as the whole intended lead. At 60 Hz the clamped
    /// 8-tick lead is 133 ms and squeaks under the 250 ms threshold; at the 30 Hz decoupled tick the 100-player
    /// target runs on it is 267 ms, so the reseek fired again, and again, every tick.
    #[test]
    fn a_reseek_that_cleared_the_window_does_not_immediately_re_arm() {
        let mut clock = ClockEstimator::default();
        for _ in 0..4 {
            clock.push_sample(0.02, -1.0);
        }
        let lead_30hz = 8.0 / 30.0; // 267 ms -- past the panic threshold on its own
        assert!(
            clock.needs_hard_resync_with_lead(0.25, lead_30hz),
            "a one-second offset is genuine trouble and must reseek"
        );
        clock.clear(); // ...which is what the reseek itself does
        assert!(
            !clock.needs_hard_resync_with_lead(0.25, lead_30hz),
            "with no samples there is no measurement to panic about -- the lead alone is not a residual"
        );
        // ...and once real samples arrive at the post-reseek steady state, it stays quiet.
        for _ in 0..4 {
            clock.push_sample(0.02, -lead_30hz);
        }
        assert!(
            !clock.needs_hard_resync_with_lead(0.25, lead_30hz),
            "a client holding its 30 Hz lead is exactly where the controller wants it"
        );
    }

    #[test]
    fn empty_estimator_is_neutral() {
        let clock = ClockEstimator::default();
        assert_eq!(clock.rtt(), 0.0);
        assert_eq!(clock.jitter(), 0.0);
        assert_eq!(clock.offset(), 0.0);
        assert_eq!(clock.stretch(1.05, 1.0), 1.0);
        assert!(!clock.needs_hard_resync(0.5));
        assert!(!clock.is_ready(1));
    }

    #[test]
    fn rejects_poison_samples() {
        let mut clock = ClockEstimator::default();
        assert!(!clock.push_sample(f64::NAN, 0.0));
        assert!(!clock.push_sample(0.1, f64::INFINITY));
        assert!(!clock.push_sample(-0.1, 0.0));
        assert_eq!(clock.sample_count(), 0);
        assert!(clock.rtt().is_finite());
    }

    #[test]
    fn averages_rtt_and_reports_jitter() {
        let mut clock = ClockEstimator::default();
        clock.push_sample(0.10, 0.0);
        clock.push_sample(0.20, 0.0);
        assert!((clock.rtt() - 0.15).abs() < 1e-12);
        assert!((clock.jitter() - 0.05).abs() < 1e-12);
    }

    #[test]
    fn offset_trusts_the_fastest_samples() {
        let mut clock = ClockEstimator::with_capacity(4);
        // The two fast samples agree on +0.10s; the slow ones are badly queued and read high.
        clock.push_sample(0.02, 0.10);
        clock.push_sample(0.02, 0.10);
        clock.push_sample(0.90, 0.80);
        clock.push_sample(0.95, 0.90);
        assert!(
            (clock.offset() - 0.10).abs() < 1e-9,
            "queued samples polluted the offset: {}",
            clock.offset()
        );
    }

    #[test]
    fn window_is_bounded_and_evicts_oldest() {
        let mut clock = ClockEstimator::with_capacity(2);
        clock.push_sample(1.0, 1.0);
        clock.push_sample(1.0, 1.0);
        clock.push_sample(0.0, 0.0);
        clock.push_sample(0.0, 0.0);
        assert_eq!(clock.sample_count(), 2);
        assert_eq!(clock.rtt(), 0.0);
        assert_eq!(clock.offset(), 0.0);
    }

    #[test]
    fn stretch_speeds_up_when_behind_and_slows_when_ahead() {
        let mut behind = ClockEstimator::default();
        behind.push_sample(0.01, 0.05);
        assert!(behind.stretch(1.05, 1.0) > 1.0);

        let mut ahead = ClockEstimator::default();
        ahead.push_sample(0.01, -0.05);
        assert!(ahead.stretch(1.05, 1.0) < 1.0);
    }

    #[test]
    fn stretch_with_folds_the_lead_bias_into_the_offset() {
        // Measured offset zero, positive bias: the clock must still speed up (chasing the lead).
        let mut clock = ClockEstimator::default();
        clock.push_sample(0.01, 0.0);
        assert!(clock.stretch_with(0.05, 1.05, 1.0) > 1.0);
        assert!(clock.stretch_with(-0.05, 1.05, 1.0) < 1.0);
        // A zero bias is exactly stretch(); a poison bias falls back to it too.
        assert_eq!(clock.stretch_with(0.0, 1.05, 1.0), clock.stretch(1.05, 1.0));
        assert_eq!(
            clock.stretch_with(f64::NAN, 1.05, 1.0),
            clock.stretch(1.05, 1.0)
        );
    }

    #[test]
    fn stretch_respects_the_bound() {
        let mut clock = ClockEstimator::default();
        clock.push_sample(0.01, 100.0);
        let s = clock.stretch(1.05, 1.0);
        assert!((s - 1.05).abs() < 1e-12, "stretch escaped its bound: {s}");

        clock.clear();
        clock.push_sample(0.01, -100.0);
        let s = clock.stretch(1.05, 1.0);
        assert!(
            (s - 1.0 / 1.05).abs() < 1e-12,
            "stretch escaped its bound: {s}"
        );
    }

    #[test]
    fn degenerate_stretch_parameters_are_neutral() {
        let mut clock = ClockEstimator::default();
        clock.push_sample(0.01, 5.0);
        assert_eq!(clock.stretch(1.0, 1.0), 1.0);
        assert_eq!(clock.stretch(f64::NAN, 1.0), 1.0);
        assert_eq!(clock.stretch(1.05, 0.0), 1.0);
        assert_eq!(clock.stretch(1.05, f64::NAN), 1.0);
    }

    #[test]
    fn hard_resync_only_past_the_threshold() {
        let mut clock = ClockEstimator::default();
        clock.push_sample(0.01, 0.20);
        assert!(!clock.needs_hard_resync(0.5));
        clock.clear();
        clock.push_sample(0.01, 0.90);
        assert!(clock.needs_hard_resync(0.5));
    }

    #[test]
    fn clear_resets_the_window() {
        let mut clock = ClockEstimator::with_capacity(2);
        clock.push_sample(0.5, 0.5);
        clock.clear();
        assert_eq!(clock.sample_count(), 0);
        assert_eq!(clock.offset(), 0.0);
        // The ring cursor reset too, so the next samples fill from the start.
        clock.push_sample(0.1, 0.1);
        assert_eq!(clock.sample_count(), 1);
    }

    #[test]
    fn a_local_correction_moves_the_stored_offsets_and_not_later_ones() {
        let mut clock = ClockEstimator::with_capacity(4);
        clock.push_sample(0.02, 0.10);
        // Sped up by 40 ms: the local clock is 40 ms closer to the remote one than the sample says.
        clock.apply_local_correction(0.04);
        assert!(
            (clock.offset() - 0.06).abs() < 1e-12,
            "offset {}",
            clock.offset()
        );
        // A sample taken after the correction already measured it, so it is not moved. It is the
        // faster of the two, so it is the one the lowest-RTT half keeps.
        clock.push_sample(0.01, 0.10);
        assert!(
            (clock.offset() - 0.10).abs() < 1e-12,
            "offset {}",
            clock.offset()
        );
        // Slowed down by 10 ms: the gap widens again.
        clock.apply_local_correction(-0.01);
        assert!(
            (clock.offset() - 0.11).abs() < 1e-12,
            "offset {}",
            clock.offset()
        );
        // The round trip is not a clock reading and does not move.
        assert!((clock.rtt() - 0.015).abs() < 1e-12);
    }

    #[test]
    fn a_poison_correction_is_ignored_and_an_empty_window_stays_empty() {
        let mut clock = ClockEstimator::default();
        clock.apply_local_correction(0.5);
        assert_eq!(clock.sample_count(), 0);
        assert_eq!(clock.offset(), 0.0);
        clock.push_sample(0.02, 0.10);
        clock.apply_local_correction(f64::NAN);
        clock.apply_local_correction(f64::INFINITY);
        assert!((clock.offset() - 0.10).abs() < 1e-12);
    }

    /// splitmix64. The closed-loop model's only source of noise, so it needs no dependency and
    /// replays exactly.
    struct Noise(u64);

    impl Noise {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Uniform in `[0, 1)`.
        fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        fn range(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * self.unit()
        }
    }

    /// One frame of a headless process sharing a two-core host with another: 0.5 to 2 ms of work,
    /// and about one frame in twenty preempted for 4 to 10 ms.
    fn shared_core_frame(noise: &mut Noise) -> f64 {
        if noise.unit() < 0.05 {
            noise.range(0.004, 0.010)
        } else {
            noise.range(0.0005, 0.002)
        }
    }

    /// 60 Hz, the rate the hunt was reported at.
    const MODEL_DT: f64 = 1.0 / 60.0;

    /// What one run of [`run_loop`] observed after its settling time.
    struct LoopTrace {
        /// The true lead-adjusted error, `(remote - local) / dt + lead`, once per wall second.
        true_error_ticks: Vec<f64>,
        /// `(wall, error)` for the error the controller acted on, `offset() / dt + lead`, every
        /// frame.
        seen_error_ticks: Vec<(f64, f64)>,
        /// Frames whose stretch sat on one of its bounds.
        frames_at_bound: usize,
    }

    impl LoopTrace {
        fn true_peak_to_peak(&self) -> f64 {
            let max = self
                .true_error_ticks
                .iter()
                .copied()
                .fold(f64::MIN, f64::max);
            let min = self
                .true_error_ticks
                .iter()
                .copied()
                .fold(f64::MAX, f64::min);
            max - min
        }

        fn worst_seen(&self) -> f64 {
            self.seen_error_ticks
                .iter()
                .map(|&(_, e)| e.abs())
                .fold(0.0, f64::max)
        }

        /// How far the seen error crossed zero after a lead step, worst over every step.
        ///
        /// A step of the lead moves the error by one tick, and the controller then drives it back
        /// to zero. A loop that reaches zero and keeps going is overshooting.
        fn worst_overshoot(&self, step_every: f64) -> f64 {
            let mut worst = 0.0_f64;
            let mut segment = Vec::new();
            let mut current = None;
            for &(wall, error) in &self.seen_error_ticks {
                let index = (wall / step_every) as u64;
                if current != Some(index) {
                    worst = worst.max(Self::segment_overshoot(&segment));
                    segment.clear();
                    current = Some(index);
                }
                segment.push(error);
            }
            worst.max(Self::segment_overshoot(&segment))
        }

        fn segment_overshoot(segment: &[f64]) -> f64 {
            let Some(&first) = segment.first() else {
                return 0.0;
            };
            let sign = first.signum();
            let deepest = segment.iter().map(|e| sign * e).fold(f64::MAX, f64::min);
            (-deepest).max(0.0)
        }
    }

    /// The decoupled client clock loop end to end, over a modelled 15 ms link between two headless
    /// processes on one loaded host.
    ///
    /// - The remote clock is wall time, advanced once per server frame. A pong carries it as of the
    ///   frame before the one that answered, which is what a server that drains before it steps
    ///   stamps.
    /// - The client runs the loop in the order the backend does: drain the pongs that arrived, take
    ///   the stretch from the window, advance the local clock by `wall * stretch`, then, with
    ///   `compensate`, shift the window by the correction just applied.
    /// - A ping every 0.25 s of wall, sent on a frame boundary, 7.5 ms each way plus up to 0.25 ms.
    /// - The default 1.05 bound, and a start 50 ms behind the remote with an empty window: the state
    ///   a hard resync leaves. `lead_ticks` is the lead bias at a wall time.
    fn run_loop(compensate: bool, seed: u64, lead_ticks: fn(f64) -> f64) -> LoopTrace {
        const SECONDS: f64 = 60.0;
        const MAX_STRETCH: f64 = 1.05;
        const ONE_WAY: f64 = 0.0075;
        const LINK_NOISE: f64 = 0.000_25;
        const SETTLE: f64 = 8.0;

        let mut server_noise = Noise(seed ^ 0x5E4E_E4F4);
        let mut client_noise = Noise(seed ^ 0xC11E_4700);
        let mut link_noise = Noise(seed ^ 0x0071_4C00);
        let mut server_frames = vec![0.0];
        while server_frames[server_frames.len() - 1] < SECONDS + 1.0 {
            let last = server_frames[server_frames.len() - 1];
            server_frames.push(last + shared_core_frame(&mut server_noise));
        }

        let mut clock = ClockEstimator::default();
        let mut wall = 0.0_f64;
        let mut local = -0.05_f64;
        let mut ping_timer = 0.0_f64;
        // (arrives at, sent at, remote clock the server stamped)
        let mut in_flight: Vec<(f64, f64, f64)> = Vec::new();
        let mut next_report = SETTLE;
        let mut trace = LoopTrace {
            true_error_ticks: Vec::new(),
            seen_error_ticks: Vec::new(),
            frames_at_bound: 0,
        };

        while wall < SECONDS {
            let frame = shared_core_frame(&mut client_noise);
            wall += frame;
            in_flight.retain(|&(arrives, sent, stamp)| {
                if arrives > wall {
                    return true;
                }
                let rtt = wall - sent;
                clock.push_sample(rtt, stamp + rtt * 0.5 - local);
                false
            });
            let lead = lead_ticks(wall);
            let stretch = clock.stretch_with(
                lead * MODEL_DT,
                MAX_STRETCH,
                STRETCH_CORRECTION_WINDOW_SECONDS,
            );
            if wall >= SETTLE {
                if (stretch - MAX_STRETCH).abs() < 1e-12
                    || (stretch - 1.0 / MAX_STRETCH).abs() < 1e-12
                {
                    trace.frames_at_bound += 1;
                }
                trace
                    .seen_error_ticks
                    .push((wall, clock.offset() / MODEL_DT + lead));
            }
            local += frame * stretch;
            if compensate {
                clock.apply_local_correction((stretch - 1.0) * frame);
            }
            if wall >= next_report {
                next_report += 1.0;
                trace
                    .true_error_ticks
                    .push((wall - local) / MODEL_DT + lead);
            }
            ping_timer += frame;
            if ping_timer >= 0.25 {
                ping_timer -= 0.25;
                let reaches_server = wall + ONE_WAY + link_noise.range(0.0, LINK_NOISE);
                let answered = server_frames.partition_point(|&f| f < reaches_server);
                let stamp = server_frames[answered - 1];
                let back = server_frames[answered] + ONE_WAY + link_noise.range(0.0, LINK_NOISE);
                in_flight.push((back, wall, stamp));
            }
        }
        trace
    }

    fn steady_lead(_wall: f64) -> f64 {
        1.0
    }

    /// The lead bias walking 0 to 4 ticks and back, one tick every two seconds, as the reported
    /// trace's did.
    fn walking_lead(wall: f64) -> f64 {
        const WALK: [f64; 8] = [0.0, 1.0, 2.0, 3.0, 4.0, 3.0, 2.0, 1.0];
        WALK[(wall / LEAD_STEP_SECONDS) as usize % WALK.len()]
    }

    const LEAD_STEP_SECONDS: f64 = 2.0;

    /// THE CLOCK SETTLES once the window is shifted by the correction applied after each sample,
    /// and the same model without the shift keeps swinging.
    ///
    /// The unshifted run is the negative control. It asserts the model reproduces the hunt, so the
    /// shifted run passing is evidence the shift removed it. Thresholds are in 60 Hz ticks.
    #[test]
    fn the_shifted_window_settles_where_the_unshifted_one_hunts() {
        for seed in 1_u64..=5 {
            let shifted = run_loop(true, seed, steady_lead);
            let unshifted = run_loop(false, seed, steady_lead);
            eprintln!(
                "seed {seed}: shifted p2p {:.2} ticks, at bound {} | unshifted p2p {:.2} ticks, at bound {}",
                shifted.true_peak_to_peak(),
                shifted.frames_at_bound,
                unshifted.true_peak_to_peak(),
                unshifted.frames_at_bound,
            );
            assert!(
                unshifted.true_peak_to_peak() > 1.25,
                "seed {seed}: the model no longer reproduces the hunt ({:.2} ticks peak to peak)",
                unshifted.true_peak_to_peak()
            );
            assert!(
                shifted.true_peak_to_peak() < 0.5,
                "seed {seed}: the shifted loop still swings {:.2} ticks peak to peak",
                shifted.true_peak_to_peak()
            );
            assert_eq!(
                shifted.frames_at_bound, 0,
                "seed {seed}: a settled loop has no reason to sit on a stretch bound"
            );
        }
    }

    /// A STEP OF THE LEAD IS MET WITHOUT OVERSHOOT, so a walking lead bias does not set the clock
    /// swinging.
    ///
    /// The reported trace had the lead walking 0 to 4 ticks while the clock hunted. Each step moves
    /// the error the controller chases by one tick. The shifted loop walks it back to zero and stops
    /// there, so the error it acts on stays inside the two-tick band a readiness rule would test.
    /// The unshifted loop carries on past zero.
    #[test]
    fn a_lead_step_is_met_without_overshoot() {
        for seed in 1_u64..=5 {
            let shifted = run_loop(true, seed, walking_lead);
            let unshifted = run_loop(false, seed, walking_lead);
            eprintln!(
                "seed {seed}: shifted overshoot {:.2} ticks, worst seen {:.2} | unshifted overshoot {:.2} ticks, worst seen {:.2}",
                shifted.worst_overshoot(LEAD_STEP_SECONDS),
                shifted.worst_seen(),
                unshifted.worst_overshoot(LEAD_STEP_SECONDS),
                unshifted.worst_seen(),
            );
            assert!(
                unshifted.worst_overshoot(LEAD_STEP_SECONDS) > 0.5,
                "seed {seed}: the model no longer overshoots without the shift ({:.2} ticks)",
                unshifted.worst_overshoot(LEAD_STEP_SECONDS)
            );
            assert!(
                shifted.worst_overshoot(LEAD_STEP_SECONDS) < 0.3,
                "seed {seed}: the shifted loop overshot a lead step by {:.2} ticks",
                shifted.worst_overshoot(LEAD_STEP_SECONDS)
            );
            assert!(
                shifted.worst_seen() < 1.5,
                "seed {seed}: the controller saw a {:.2}-tick error",
                shifted.worst_seen()
            );
        }
    }
}
