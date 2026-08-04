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
    #[must_use]
    pub fn needs_hard_resync(&self, panic_threshold: f64) -> bool {
        panic_threshold.is_finite()
            && panic_threshold > 0.0
            && self.offset().abs() > panic_threshold
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
