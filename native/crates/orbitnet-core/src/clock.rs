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

/// Number of `(rtt, offset)` samples kept in the estimation window.
pub const DEFAULT_SAMPLE_CAPACITY: usize = 8;

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
    /// lead it has dialled in, and testing the raw value fires the panic path on a perfectly healthy client.
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
}
