//! The tick clock.
//!
//! OrbitNet runs a fixed-rate simulation tick that is decoupled from the render frame. This module
//! owns the conversion between ticks and seconds, and the accumulator that decides how many ticks a
//! given frame should run.
//!
//! The important behaviour here is **catch-up bounding**. When a frame runs long, the accumulator
//! holds more than one tick's worth of time. Running all of it unbounded is what turns a single
//! hitch into a spiral: the frame runs longer because it ran more ticks, which leaves more backlog,
//! which runs more ticks. [`TickAccumulator::advance`] therefore caps the ticks per frame and
//! *discards* the backlog it refuses to run, reporting that it did so via [`TickStep::clamped`].
//! Re-aligning with the server after a discard is the clock's job (see [`crate::clock`]), not the
//! accumulator's.

/// Lowest tick rate OrbitNet will run at.
pub const MIN_TICKRATE_HZ: u32 = 1;

/// Highest tick rate OrbitNet will run at.
///
/// Matches the clamp the Spaceman console has always applied to `net.tickrate`.
pub const MAX_TICKRATE_HZ: u32 = 240;

/// Default ceiling on how many simulation ticks a single frame may run.
pub const DEFAULT_MAX_TICKS_PER_FRAME: u32 = 8;

/// A validated simulation tick rate in hertz.
///
/// Construction clamps into `MIN_TICKRATE_HZ..=MAX_TICKRATE_HZ`, so a `TickRate` can never carry a
/// zero rate and [`TickRate::dt`] can never divide by zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TickRate(u32);

impl TickRate {
    /// Build a tick rate, clamping into the supported range.
    #[must_use]
    pub fn new(hz: u32) -> Self {
        Self(hz.clamp(MIN_TICKRATE_HZ, MAX_TICKRATE_HZ))
    }

    /// The rate in hertz.
    #[must_use]
    pub fn hz(self) -> u32 {
        self.0
    }

    /// Seconds per tick.
    #[must_use]
    pub fn dt(self) -> f64 {
        1.0 / f64::from(self.0)
    }

    /// Convert a tick count to seconds.
    #[must_use]
    pub fn ticks_to_seconds(self, ticks: i64) -> f64 {
        ticks as f64 * self.dt()
    }

    /// Convert seconds to a whole number of ticks, rounding toward zero.
    #[must_use]
    pub fn seconds_to_ticks(self, seconds: f64) -> i64 {
        if !seconds.is_finite() {
            return 0;
        }
        (seconds * f64::from(self.0)) as i64
    }
}

impl Default for TickRate {
    fn default() -> Self {
        Self::new(60)
    }
}

/// What a single [`TickAccumulator::advance`] call decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickStep {
    /// How many simulation ticks the caller should run this frame.
    pub ticks: u32,
    /// Whether the tick count was capped, meaning backlog was discarded.
    pub clamped: bool,
}

/// Turns wall-clock deltas into a whole number of fixed-rate simulation ticks.
#[derive(Debug, Clone)]
pub struct TickAccumulator {
    rate: TickRate,
    max_ticks_per_frame: u32,
    accumulator: f64,
    tick: u64,
}

impl TickAccumulator {
    /// Start an accumulator at tick 0 with the default per-frame cap.
    #[must_use]
    pub fn new(rate: TickRate) -> Self {
        Self {
            rate,
            max_ticks_per_frame: DEFAULT_MAX_TICKS_PER_FRAME,
            accumulator: 0.0,
            tick: 0,
        }
    }

    /// The current tick rate.
    #[must_use]
    pub fn rate(&self) -> TickRate {
        self.rate
    }

    /// Change the tick rate, preserving the current tick index.
    ///
    /// The pending sub-tick remainder is dropped rather than rescaled: it was measured against the
    /// old tick length and carrying it over would produce one mistimed tick at the switch.
    pub fn set_rate(&mut self, rate: TickRate) {
        if rate != self.rate {
            self.rate = rate;
            self.accumulator = 0.0;
        }
    }

    /// The ceiling on ticks run per frame.
    #[must_use]
    pub fn max_ticks_per_frame(&self) -> u32 {
        self.max_ticks_per_frame
    }

    /// Set the ceiling on ticks run per frame. Values below 1 are raised to 1.
    pub fn set_max_ticks_per_frame(&mut self, max: u32) {
        self.max_ticks_per_frame = max.max(1);
    }

    /// The tick the simulation has reached.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Force the tick index, clearing any sub-tick remainder.
    ///
    /// Used when the clock hard-resyncs after a panic-level offset.
    pub fn seek(&mut self, tick: u64) {
        self.tick = tick;
        self.accumulator = 0.0;
    }

    /// How far the clock is between the last tick and the next, in `0.0..1.0`.
    ///
    /// This is the interpolation weight presentation code uses when the net tick is slower than the
    /// render frame.
    #[must_use]
    pub fn tick_factor(&self) -> f64 {
        (self.accumulator / self.rate.dt()).clamp(0.0, 1.0)
    }

    /// Feed a frame delta (already scaled by any clock stretch) and get the ticks to run.
    ///
    /// Non-finite and non-positive deltas contribute nothing, so a paused or glitched frame cannot
    /// poison the accumulator with `NaN`.
    pub fn advance(&mut self, delta: f64) -> TickStep {
        if delta.is_finite() && delta > 0.0 {
            self.accumulator += delta;
        }

        let dt = self.rate.dt();
        if self.accumulator < dt {
            return TickStep::default();
        }

        let pending = (self.accumulator / dt) as u64;
        let cap = u64::from(self.max_ticks_per_frame);
        let clamped = pending > cap;
        let ticks = if clamped { cap } else { pending };

        self.accumulator -= ticks as f64 * dt;
        if clamped {
            // Discard the backlog we refused to run. Keeping it would guarantee the cap is hit
            // again next frame, which is exactly the catch-up spiral this cap exists to stop.
            self.accumulator = self.accumulator.min(dt);
        }
        self.tick += ticks;

        TickStep {
            ticks: ticks as u32,
            clamped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickrate_clamps_into_supported_range() {
        assert_eq!(TickRate::new(0).hz(), MIN_TICKRATE_HZ);
        assert_eq!(TickRate::new(10_000).hz(), MAX_TICKRATE_HZ);
        assert_eq!(TickRate::new(120).hz(), 120);
    }

    #[test]
    fn tickrate_converts_between_ticks_and_seconds() {
        let rate = TickRate::new(60);
        assert!((rate.dt() - 1.0 / 60.0).abs() < f64::EPSILON);
        assert!((rate.ticks_to_seconds(60) - 1.0).abs() < 1e-12);
        assert_eq!(rate.seconds_to_ticks(1.0), 60);
        assert_eq!(rate.seconds_to_ticks(f64::NAN), 0);
    }

    #[test]
    fn accumulator_steps_one_tick_per_dt() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        let step = acc.advance(rate.dt());
        assert_eq!(step.ticks, 1);
        assert!(!step.clamped);
        assert_eq!(acc.tick(), 1);
    }

    #[test]
    fn accumulator_holds_partial_ticks() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        assert_eq!(acc.advance(rate.dt() * 0.5).ticks, 0);
        assert_eq!(acc.tick(), 0);
        assert!((acc.tick_factor() - 0.5).abs() < 1e-9);
        // The second half completes the tick.
        assert_eq!(acc.advance(rate.dt() * 0.5).ticks, 1);
        assert_eq!(acc.tick(), 1);
    }

    #[test]
    fn accumulator_caps_ticks_per_frame_and_reports_it() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        // A one-second stall is 60 ticks of backlog at 60Hz.
        let step = acc.advance(1.0);
        assert_eq!(step.ticks, 8);
        assert!(step.clamped);
        assert_eq!(acc.tick(), 8);
    }

    #[test]
    fn clamped_frame_discards_backlog_instead_of_spiralling() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        acc.advance(1.0);
        // The next normal frame must run a normal number of ticks, not another capped burst.
        let step = acc.advance(rate.dt());
        assert!(!step.clamped, "backlog leaked into the following frame");
        assert!(step.ticks <= 2, "expected ~1 tick, got {}", step.ticks);
    }

    #[test]
    fn accumulator_ignores_nonfinite_and_negative_deltas() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        assert_eq!(acc.advance(f64::NAN).ticks, 0);
        assert_eq!(acc.advance(f64::INFINITY).ticks, 0);
        assert_eq!(acc.advance(-1.0).ticks, 0);
        assert_eq!(acc.tick(), 0);
        assert!(acc.tick_factor().is_finite());
        // Still healthy afterwards.
        assert_eq!(acc.advance(rate.dt()).ticks, 1);
    }

    #[test]
    fn max_ticks_per_frame_never_drops_below_one() {
        let mut acc = TickAccumulator::new(TickRate::new(60));
        acc.set_max_ticks_per_frame(0);
        assert_eq!(acc.max_ticks_per_frame(), 1);
    }

    #[test]
    fn changing_rate_drops_stale_remainder_but_keeps_tick() {
        let mut acc = TickAccumulator::new(TickRate::new(60));
        acc.advance(TickRate::new(60).dt() * 0.75);
        acc.set_rate(TickRate::new(120));
        assert_eq!(acc.rate().hz(), 120);
        assert_eq!(acc.tick_factor(), 0.0);
        assert_eq!(acc.tick(), 0);
    }

    #[test]
    fn seek_resets_subtick_remainder() {
        let mut acc = TickAccumulator::new(TickRate::new(60));
        acc.advance(TickRate::new(60).dt() * 0.5);
        acc.seek(1000);
        assert_eq!(acc.tick(), 1000);
        assert_eq!(acc.tick_factor(), 0.0);
    }
}
