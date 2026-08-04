//! Tick pacing: coupled-mode slewing and input-lead tracking.
//!
//! When the net tick is *coupled* to the physics tick (both 120 Hz), pacing the tick loop off a
//! stretched wall clock is exactly wrong: any stretch != 1.0 slides tick boundaries across physics
//! frames, so some frames run zero net ticks and others run two — visible judder, inherited from
//! stretching the clock under `sync_to_physics`. The coupled-mode rule is the opposite: pin
//! stretch to 1.0, run exactly one tick per physics frame, and absorb clock error as a rare,
//! deliberate *slew* — a single frame that runs zero or two ticks. [`CoupledSlew`] decides when:
//! only once the offset exceeds a threshold comfortably past the 0.5-tick rounding boundary (so
//! jitter can never trigger it), and never within a cooldown of the previous slew (so corrections
//! are isolated events instead of a continuous drift).
//!
//! The second brain closes the input-lead loop. The server reports, in every snapshot header, how
//! early or late that peer's newest input arrived (`margin_ticks`, positive = early).
//! [`LeadTracker`] aggregates those samples in a bounded window and steers off the **worst**
//! (minimum) margin — a mean would hide the occasional late packet, and it is the late packets
//! that actually cost mispredictions. Suggestions only fire on a full window and clear it, so lead
//! changes are paced by window refill rather than twitching on every sample. Applying a suggestion
//! is the caller's job; this type only measures.

/// Default slew threshold in ticks.
///
/// Well past the 0.5-tick rounding boundary, so ordinary estimator jitter around a tick edge can
/// never trigger a slew — only a genuine accumulated drift can.
pub const DEFAULT_SLEW_THRESHOLD_TICKS: f64 = 0.75;

/// Default slew cooldown in physics frames.
///
/// About one second at the 120 Hz coupled rate, so corrections are rare, isolated events rather
/// than an oscillation between catch-up and give-back.
pub const DEFAULT_SLEW_COOLDOWN_FRAMES: u32 = 120;

/// Default number of `margin_ticks` samples a [`LeadTracker`] window holds.
pub const DEFAULT_LEAD_WINDOW_CAPACITY: usize = 32;

/// How many net ticks a coupled physics frame should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlewDecision {
    /// Skip this frame's tick: the local clock is ahead and must give a tick back.
    Run0,
    /// The steady state: exactly one net tick per physics frame.
    Run1,
    /// Run an extra tick: the local clock is behind and must catch up.
    Run2,
}

impl SlewDecision {
    /// The tick count this decision stands for.
    #[must_use]
    pub fn ticks(self) -> u32 {
        match self {
            Self::Run0 => 0,
            Self::Run1 => 1,
            Self::Run2 => 2,
        }
    }
}

/// Decides the per-physics-frame tick count in coupled mode.
///
/// Call [`CoupledSlew::decide`] once per physics frame with the current clock offset. The answer
/// is [`SlewDecision::Run1`] almost always; a slew is only issued when the offset has drifted past
/// the threshold *and* the cooldown since the previous slew has elapsed.
#[derive(Debug, Clone)]
pub struct CoupledSlew {
    threshold_ticks: f64,
    cooldown_frames: u32,
    cooldown_remaining: u32,
}

impl CoupledSlew {
    /// Build a slew brain with the default threshold and cooldown.
    #[must_use]
    pub fn new() -> Self {
        Self::with_params(DEFAULT_SLEW_THRESHOLD_TICKS, DEFAULT_SLEW_COOLDOWN_FRAMES)
    }

    /// Build a slew brain with an explicit threshold (in ticks) and cooldown (in frames).
    ///
    /// A non-finite or non-positive threshold falls back to [`DEFAULT_SLEW_THRESHOLD_TICKS`],
    /// since a zero threshold would slew on every frame of ordinary jitter. A cooldown of 0 is
    /// allowed and means back-to-back slews are permitted (useful in tests, never in production).
    #[must_use]
    pub fn with_params(threshold_ticks: f64, cooldown_frames: u32) -> Self {
        let threshold_ticks = if threshold_ticks.is_finite() && threshold_ticks > 0.0 {
            threshold_ticks
        } else {
            DEFAULT_SLEW_THRESHOLD_TICKS
        };
        Self {
            threshold_ticks,
            cooldown_frames,
            cooldown_remaining: 0,
        }
    }

    /// The slew threshold in ticks.
    #[must_use]
    pub fn threshold_ticks(&self) -> f64 {
        self.threshold_ticks
    }

    /// The cooldown between slews in frames.
    #[must_use]
    pub fn cooldown_frames(&self) -> u32 {
        self.cooldown_frames
    }

    /// Decide how many net ticks this physics frame should run.
    ///
    /// `clock_offset_ticks` is `server_clock - local_clock` expressed in ticks: positive means the
    /// local clock is behind and needs to catch up ([`SlewDecision::Run2`]), negative means it is
    /// ahead and must give a tick back ([`SlewDecision::Run0`]). Anything inside the threshold —
    /// and any non-finite offset — is the steady state, [`SlewDecision::Run1`]. Every call counts
    /// as one frame for cooldown purposes, and issuing a slew re-arms the full cooldown.
    pub fn decide(&mut self, clock_offset_ticks: f64) -> SlewDecision {
        let cooling = self.cooldown_remaining > 0;
        if cooling {
            self.cooldown_remaining -= 1;
        }
        if !clock_offset_ticks.is_finite() {
            return SlewDecision::Run1;
        }
        if cooling || clock_offset_ticks.abs() <= self.threshold_ticks {
            return SlewDecision::Run1;
        }
        self.cooldown_remaining = self.cooldown_frames;
        if clock_offset_ticks > 0.0 {
            SlewDecision::Run2
        } else {
            SlewDecision::Run0
        }
    }

    /// Forget any pending cooldown. Used on session teardown so a new session starts armed.
    pub fn reset(&mut self) {
        self.cooldown_remaining = 0;
    }
}

impl Default for CoupledSlew {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded window of server-reported input-arrival margins, in ticks.
///
/// Each sample is the `margin_ticks` byte from one snapshot header: how early (positive) or late
/// (negative) that peer's newest input arrived at the server. The steering signal is
/// [`LeadTracker::min_margin`] — steer the input stamp lead so the *worst* sample stays slightly
/// positive. [`LeadTracker::suggest_adjustment`] wraps that rule with hysteresis; the caller
/// applies the returned adjustment to its lead, this struct only measures.
#[derive(Debug, Clone)]
pub struct LeadTracker {
    samples: Vec<i8>,
    capacity: usize,
    next: usize,
}

impl LeadTracker {
    /// Build a tracker holding [`DEFAULT_LEAD_WINDOW_CAPACITY`] samples.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_LEAD_WINDOW_CAPACITY)
    }

    /// Build a tracker holding `capacity` samples (minimum 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: Vec::with_capacity(capacity),
            capacity,
            next: 0,
        }
    }

    /// Record one `margin_ticks` sample from a snapshot header (positive = input arrived early).
    pub fn push(&mut self, margin_ticks: i8) {
        if self.samples.len() < self.capacity {
            self.samples.push(margin_ticks);
        } else {
            self.samples[self.next] = margin_ticks;
        }
        self.next = (self.next + 1) % self.capacity;
    }

    /// How many samples the window currently holds.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// The worst (most late) margin in the window, or `None` when empty.
    ///
    /// This is the lead-steering signal: steer so the minimum stays slightly positive.
    #[must_use]
    pub fn min_margin(&self) -> Option<i8> {
        self.samples.iter().copied().min()
    }

    /// Mean margin over the window, or 0.0 when empty.
    #[must_use]
    pub fn mean_margin(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().map(|&m| f64::from(m)).sum::<f64>() / self.samples.len() as f64
    }

    /// Suggest a lead adjustment in ticks: `+1` to increase the lead, `-1` to decrease, `0` to
    /// hold.
    ///
    /// Only a **full** window earns a suggestion — partial windows return 0. `+1` when the
    /// window's worst margin fell below `target_min` (inputs are arriving too late); `-1` when the
    /// worst margin exceeds `target_min + 2` (a sustained surplus is pure wasted latency); the
    /// band in between is hysteresis and holds. A nonzero suggestion **clears the window**, so the
    /// next suggestion waits for a full refill — adjustments are paced by the window, not issued
    /// per sample. The caller applies the adjustment to its lead; this struct only measures.
    pub fn suggest_adjustment(&mut self, target_min: i8) -> i32 {
        if self.samples.len() < self.capacity {
            return 0;
        }
        // i32 arithmetic so `target_min + 2` cannot overflow at the top of the i8 range.
        let min = i32::from(self.min_margin().expect("window is full"));
        let target = i32::from(target_min);
        let adjustment = if min < target {
            1
        } else if min > target + 2 {
            -1
        } else {
            0
        };
        if adjustment != 0 {
            self.clear();
        }
        adjustment
    }

    /// Drop every sample. Used on disconnect so a new session never inherits stale margins.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.next = 0;
    }
}

impl Default for LeadTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slew_decision_maps_to_tick_counts() {
        assert_eq!(SlewDecision::Run0.ticks(), 0);
        assert_eq!(SlewDecision::Run1.ticks(), 1);
        assert_eq!(SlewDecision::Run2.ticks(), 2);
    }

    #[test]
    fn coupled_slew_runs_one_tick_in_the_deadband() {
        let mut slew = CoupledSlew::new();
        for offset in [0.0, 0.4, -0.4, 0.75, -0.75] {
            assert_eq!(
                slew.decide(offset),
                SlewDecision::Run1,
                "offset {offset} should sit in the deadband"
            );
        }
    }

    #[test]
    fn slews_two_ticks_when_local_clock_is_behind() {
        let mut slew = CoupledSlew::new();
        assert_eq!(slew.decide(0.76), SlewDecision::Run2);
    }

    #[test]
    fn slews_zero_ticks_when_local_clock_is_ahead() {
        let mut slew = CoupledSlew::new();
        assert_eq!(slew.decide(-0.76), SlewDecision::Run0);
    }

    #[test]
    fn cooldown_suppresses_back_to_back_slews_then_rearms() {
        let mut slew = CoupledSlew::with_params(0.75, 3);
        assert_eq!(slew.decide(2.0), SlewDecision::Run2);
        // The next `cooldown_frames` frames are suppressed even though the offset still demands it.
        for frame in 0..3 {
            assert_eq!(
                slew.decide(2.0),
                SlewDecision::Run1,
                "frame {frame} slewed inside the cooldown"
            );
        }
        // Cooldown expired: the persistent offset earns another slew.
        assert_eq!(slew.decide(2.0), SlewDecision::Run2);
    }

    #[test]
    fn deadband_frames_still_advance_the_cooldown() {
        let mut slew = CoupledSlew::with_params(0.75, 2);
        assert_eq!(slew.decide(2.0), SlewDecision::Run2);
        // Two in-band frames burn the whole cooldown...
        assert_eq!(slew.decide(0.0), SlewDecision::Run1);
        assert_eq!(slew.decide(0.0), SlewDecision::Run1);
        // ...so a fresh drift may slew immediately.
        assert_eq!(slew.decide(-2.0), SlewDecision::Run0);
    }

    #[test]
    fn nonfinite_offsets_are_neutral() {
        let mut slew = CoupledSlew::new();
        assert_eq!(slew.decide(f64::NAN), SlewDecision::Run1);
        assert_eq!(slew.decide(f64::INFINITY), SlewDecision::Run1);
        assert_eq!(slew.decide(f64::NEG_INFINITY), SlewDecision::Run1);
        // A poison sample never armed the cooldown, so a real drift still slews.
        assert_eq!(slew.decide(1.0), SlewDecision::Run2);
    }

    #[test]
    fn reset_forgets_the_cooldown() {
        let mut slew = CoupledSlew::with_params(0.75, 1000);
        assert_eq!(slew.decide(2.0), SlewDecision::Run2);
        assert_eq!(slew.decide(2.0), SlewDecision::Run1);
        slew.reset();
        assert_eq!(slew.decide(2.0), SlewDecision::Run2);
    }

    #[test]
    fn degenerate_threshold_falls_back_to_default() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let slew = CoupledSlew::with_params(bad, 120);
            assert_eq!(slew.threshold_ticks(), DEFAULT_SLEW_THRESHOLD_TICKS);
        }
    }

    #[test]
    fn empty_lead_tracker_is_neutral() {
        let mut lead = LeadTracker::new();
        assert_eq!(lead.sample_count(), 0);
        assert_eq!(lead.min_margin(), None);
        assert_eq!(lead.mean_margin(), 0.0);
        assert_eq!(lead.suggest_adjustment(1), 0);
    }

    #[test]
    fn lead_window_is_bounded_and_evicts_oldest() {
        let mut lead = LeadTracker::with_capacity(2);
        lead.push(-5);
        lead.push(3);
        lead.push(4);
        // The -5 rolled out of the window.
        assert_eq!(lead.sample_count(), 2);
        assert_eq!(lead.min_margin(), Some(3));
        assert!((lead.mean_margin() - 3.5).abs() < 1e-12);
    }

    #[test]
    fn min_margin_reports_the_worst_sample() {
        let mut lead = LeadTracker::with_capacity(4);
        lead.push(5);
        lead.push(-2);
        lead.push(7);
        assert_eq!(lead.min_margin(), Some(-2));
    }

    #[test]
    fn capacity_never_drops_below_one() {
        let mut lead = LeadTracker::with_capacity(0);
        lead.push(1);
        lead.push(2);
        assert_eq!(lead.sample_count(), 1);
        assert_eq!(lead.min_margin(), Some(2));
    }

    #[test]
    fn partial_window_never_suggests() {
        let mut lead = LeadTracker::with_capacity(4);
        lead.push(-10);
        lead.push(-10);
        assert_eq!(lead.suggest_adjustment(1), 0);
        // The samples survive: no suggestion was issued.
        assert_eq!(lead.sample_count(), 2);
    }

    #[test]
    fn suggests_more_lead_when_inputs_arrive_late_and_clears() {
        let mut lead = LeadTracker::with_capacity(3);
        lead.push(2);
        lead.push(0);
        lead.push(2);
        assert_eq!(lead.suggest_adjustment(1), 1);
        assert_eq!(lead.sample_count(), 0, "nonzero suggestion must clear");
        // Paced by refill: an immediate re-ask holds.
        assert_eq!(lead.suggest_adjustment(1), 0);
    }

    #[test]
    fn suggests_less_lead_on_sustained_surplus_and_clears() {
        let mut lead = LeadTracker::with_capacity(3);
        lead.push(8);
        lead.push(4);
        lead.push(9);
        assert_eq!(lead.suggest_adjustment(1), -1);
        assert_eq!(lead.sample_count(), 0, "nonzero suggestion must clear");
    }

    #[test]
    fn hysteresis_band_holds_and_keeps_the_window() {
        let mut lead = LeadTracker::with_capacity(3);
        // min = 2 with target 1: inside (target ..= target + 2), so hold.
        lead.push(2);
        lead.push(5);
        lead.push(3);
        assert_eq!(lead.suggest_adjustment(1), 0);
        assert_eq!(lead.sample_count(), 3, "a hold must not clear the window");
    }

    #[test]
    fn extreme_margins_do_not_overflow() {
        let mut lead = LeadTracker::with_capacity(1);
        lead.push(i8::MAX);
        // target_min + 2 would overflow i8; the comparison must survive it and hold.
        assert_eq!(lead.suggest_adjustment(i8::MAX), 0);
        lead.clear();
        lead.push(i8::MIN);
        assert_eq!(lead.min_margin(), Some(i8::MIN));
        assert_eq!(lead.suggest_adjustment(i8::MAX), 1);
    }

    #[test]
    fn clear_empties_the_window() {
        let mut lead = LeadTracker::with_capacity(2);
        lead.push(1);
        lead.push(2);
        lead.clear();
        assert_eq!(lead.sample_count(), 0);
        assert_eq!(lead.min_margin(), None);
        // The ring cursor reset too, so the next samples fill from the start.
        lead.push(7);
        assert_eq!(lead.sample_count(), 1);
        assert_eq!(lead.min_margin(), Some(7));
    }
}
