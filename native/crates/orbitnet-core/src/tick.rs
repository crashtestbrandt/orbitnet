//! The tick clock.
//!
//! OrbitNet runs a fixed-rate simulation tick that is decoupled from the render frame. This module
//! owns the conversion between ticks and seconds, and the accumulator that decides how many ticks a
//! given frame should run.
//!
//! The important behavior here is **catch-up bounding**. When a frame runs long, the accumulator
//! holds more than one tick's worth of time. Running all of it in one frame is what turns a single
//! hitch into a spiral: the frame runs longer because it ran more ticks, which leaves more backlog,
//! which runs more ticks. [`TickAccumulator::advance`] therefore caps the ticks per frame — and it
//! **retains** the backlog the cap refused, up to [`MAX_RETAINED_BACKLOG_SECONDS`], draining it a
//! couple of ticks per frame ([`CATCHUP_TICKS_PER_FRAME`]) on top of what each frame's own delta
//! brings. A hitch inside that bound costs nothing but a dozen imperceptibly-heavier frames: the
//! timeline loses no time, so the peer at the other end of the wire never sees this one's clock
//! move. The earlier design discarded the refused backlog instead, which
//! tore this peer's tick timeline away from wall clock by the length of every render hitch — and
//! a rendered client's clock stretch is capped a few percent from 1.0, so each tear was walked
//! off over seconds and any tear past the panic threshold was a visible hard resync. Measured on
//! a LAN listen host with two rendered clients: a steady-state hard resync every 13–25 s, on a
//! 15 ms link with zero loss.
//!
//! Only a stall past the retention bound *discards*, reported via [`TickStep::clamped`] — a
//! machine that far behind (a world build, a debugger pause) is not going to catch up by
//! simulating harder. Re-aligning with the server after a genuine discard is the clock's job
//! (see [`crate::clock`]), not the accumulator's.

/// Lowest tick rate OrbitNet will run at.
pub const MIN_TICKRATE_HZ: u32 = 1;

/// Highest tick rate OrbitNet will run at.
///
/// A sane range for a network tick; the facade clamps identically so a runtime knob round-trips.
pub const MAX_TICKRATE_HZ: u32 = 240;

/// Default ceiling on how many simulation ticks a single frame may run.
pub const DEFAULT_MAX_TICKS_PER_FRAME: u32 = 8;

/// How many ticks of retained backlog one frame may drain on top of the ticks its own delta
/// brought.
///
/// Two is deliberate: a healthy 60 fps frame at a 60 Hz tick runs one tick, so the worst paced
/// frame runs three — a bounded, invisible cost — while a machine that cannot even sustain
/// `rate + 2` ticks per frame was never going to catch up by simulating harder, and reaches the
/// retention bound's discard instead. Draining faster than this is what turned a render hitch
/// into back-to-back max-cost frames on rendered peers (see [`TickAccumulator::advance`]).
pub const CATCHUP_TICKS_PER_FRAME: u64 = 2;

/// How much refused backlog [`TickAccumulator::advance`] retains for later frames, in seconds.
///
/// Backlog under this bound is a recoverable hitch: the following frames drain it at the
/// per-frame cap and the timeline loses nothing. Backlog past it is a genuine stall, where
/// fast-forwarding a second of simulation buys nothing — the clock's hard resync is about to
/// reseek the timeline anyway — so everything above the bound is discarded.
pub const MAX_RETAINED_BACKLOG_SECONDS: f64 = 1.0;

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
    /// Whether backlog past [`MAX_RETAINED_BACKLOG_SECONDS`] was discarded — the timeline just
    /// tore, so every clock sample measured against it before this frame is now meaningless.
    /// A merely capped frame (backlog retained for later frames) does NOT set this.
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
    ///
    /// THE DRAIN IS PACED, NOT BURSTED. A frame runs the ticks its own delta brought, plus at most
    /// [`CATCHUP_TICKS_PER_FRAME`] more from the retained backlog. Draining at the full per-frame
    /// cap instead demanded max-cost frames back to back until the backlog cleared, and on a
    /// RENDERED peer a tick is not free — the send path on a host, the sim and its effects on a
    /// firing client — so the burst frames themselves ran long, accrued more backlog than they
    /// drained, and pinned the process at the cap: a client froze mid-firefight and a listen host
    /// starved its renderer until the platform killed it. Headless peers never showed it, because
    /// a headless tick costs next to nothing — which is exactly why the burst design survived
    /// loopback validation. Paced, the worst frame is its own ticks plus two, and a hitch drains
    /// over a dozen frames the clock never sees (`timeline_seconds` counts retained backlog).
    pub fn advance(&mut self, delta: f64) -> TickStep {
        let fed = if delta.is_finite() && delta > 0.0 {
            self.accumulator += delta;
            delta
        } else {
            0.0
        };

        let dt = self.rate.dt();
        if self.accumulator < dt {
            return TickStep::default();
        }

        let pending = (self.accumulator / dt) as u64;
        let cap = u64::from(self.max_ticks_per_frame);
        // What this frame's own delta is worth in whole ticks, rounded up: real time never waits
        // on the pacing, only the backlog does.
        let fresh = (fed / dt).ceil() as u64;
        let paced = fresh.saturating_add(CATCHUP_TICKS_PER_FRAME);
        let ticks = pending.min(cap).min(paced);
        self.accumulator -= ticks as f64 * dt;
        // What the pacing refused is RETAINED for the following frames. Only a stall past the
        // retention bound discards, because that timeline is torn either way and fast-forwarding
        // a second of old simulation would only delay the reseek.
        let clamped = self.accumulator > MAX_RETAINED_BACKLOG_SECONDS;
        if clamped {
            self.accumulator = self.accumulator.min(dt);
        }
        self.tick += ticks;

        TickStep {
            ticks: ticks as u32,
            clamped,
        }
    }

    /// The peer's position on its own tick timeline, in seconds: completed ticks plus every
    /// second `advance` has accepted but not yet run.
    ///
    /// The clock discipline measures THIS, not presentation time. Retained catch-up backlog is
    /// time that has already arrived, so a peer mid-drain after a hitch is exactly where its
    /// wall clock says it is — measuring `tick * dt` instead would report the whole hitch as a
    /// clock offset and yank every peer's controller for a displacement that is about to drain
    /// away on its own.
    #[must_use]
    pub fn timeline_seconds(&self) -> f64 {
        self.tick as f64 * self.rate.dt() + self.accumulator
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
    fn accumulator_caps_ticks_per_frame_but_retains_the_backlog() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        // A half-second hitch is 30 ticks of backlog at 60 Hz: run the cap, keep the rest.
        let step = acc.advance(0.5);
        assert_eq!(step.ticks, 8);
        assert!(!step.clamped, "a recoverable hitch is not a discard");
        assert_eq!(acc.tick(), 8);
    }

    /// THE HITCH MUST NOT TEAR THE TIMELINE: a render stall inside the retention bound is caught
    /// up over the following frames, losing no time — so the peer on the other end of the wire
    /// never sees this clock move, and nothing reaches the hard-resync path. (The discard this
    /// replaces cost a LAN session a rubber-band hard resync every 13–25 s.)
    #[test]
    fn a_hitch_inside_the_bound_is_caught_up_without_losing_time() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        let mut wall = 0.0;
        // One second of normal 60 fps frames...
        for _ in 0..60 {
            acc.advance(rate.dt());
            wall += rate.dt();
        }
        // ...then a 400 ms hitch arrives as one big delta...
        let step = acc.advance(0.4);
        wall += 0.4;
        assert_eq!(step.ticks, 8);
        assert!(!step.clamped);
        // ...and the drain runs at most the cap per frame until the backlog is gone.
        for _ in 0..10 {
            let drain = acc.advance(rate.dt());
            wall += rate.dt();
            assert!(
                drain.ticks <= 8,
                "drain burst past the cap: {}",
                drain.ticks
            );
            assert!(!drain.clamped, "the drain is not a discard");
        }
        // Zero time lost: the timeline (ticks run plus what little backlog remains) matches the
        // wall clock to float error, and the backlog itself has drained to under one tick.
        assert!(
            (acc.timeline_seconds() - wall).abs() < 1e-9,
            "the timeline lost time through a recoverable hitch: {} vs {}",
            acc.timeline_seconds(),
            wall
        );
        assert!(
            acc.timeline_seconds() - acc.tick() as f64 * rate.dt() < rate.dt(),
            "the backlog never drained"
        );
    }

    /// THE DRAIN MUST NOT BURST: draining at the full per-frame cap ran max-cost frames back to
    /// back, and on a rendered peer those frames were themselves slow enough to accrue more
    /// backlog than they drained — a client froze mid-firefight and a listen host starved its
    /// renderer until the platform killed it. Each drain frame may run only its own delta's worth
    /// of ticks plus the catch-up allowance.
    #[test]
    fn the_drain_is_paced_at_a_frames_own_ticks_plus_the_allowance() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        // A 400 ms hitch: the hitch frame itself may burst to the cap (it always could — the
        // engine's old clamp handed it ~a cap's worth of delta in one piece)...
        let step = acc.advance(0.4);
        assert_eq!(step.ticks, 8);
        // ...but every DRAIN frame is bounded by its own delta (one tick at 60 fps) plus the
        // allowance, however much backlog remains.
        let mut drained = 0u64;
        for _ in 0..30 {
            let drain = acc.advance(rate.dt());
            assert!(
                u64::from(drain.ticks) <= 1 + CATCHUP_TICKS_PER_FRAME,
                "a drain frame bursted: {} ticks",
                drain.ticks
            );
            drained += u64::from(drain.ticks);
        }
        // And the pacing still conserves the time: hitch + 30 frames = 24 + 30 ticks in total.
        assert_eq!(drained + 8, 54, "the paced drain lost or invented ticks");
        assert!(
            acc.timeline_seconds() - acc.tick() as f64 * rate.dt() < rate.dt(),
            "the backlog never finished draining"
        );
    }

    #[test]
    fn a_stall_past_the_bound_discards_and_reports_it() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        // A three-second stall (a world build) is past the retention bound.
        let step = acc.advance(3.0);
        assert_eq!(step.ticks, 8);
        assert!(step.clamped, "a genuine stall must report the discard");
        // The next normal frame runs a normal number of ticks: the discard kept no drain.
        let step = acc.advance(rate.dt());
        assert!(!step.clamped);
        assert!(step.ticks <= 2, "expected ~1 tick, got {}", step.ticks);
    }

    /// The clock measures the timeline INCLUDING retained backlog, so a mid-drain peer reports
    /// the position its wall clock implies rather than the tick its simulation has reached.
    #[test]
    fn timeline_seconds_is_continuous_through_a_retained_hitch() {
        let rate = TickRate::new(60);
        let mut acc = TickAccumulator::new(rate);
        acc.set_max_ticks_per_frame(8);
        let mut wall = 0.0;
        for _ in 0..30 {
            acc.advance(rate.dt());
            wall += rate.dt();
        }
        assert!((acc.timeline_seconds() - wall).abs() < 1e-9);
        // The hitch lands: the simulation is 8 ticks in, but the timeline holds all 0.4 s.
        acc.advance(0.4);
        wall += 0.4;
        assert!(
            (acc.timeline_seconds() - wall).abs() < 1e-9,
            "retained backlog fell out of the timeline: {} vs {}",
            acc.timeline_seconds(),
            wall
        );
        // And it stays continuous through the drain.
        acc.advance(rate.dt());
        wall += rate.dt();
        assert!((acc.timeline_seconds() - wall).abs() < 1e-9);
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
        // Still healthy afterward.
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
