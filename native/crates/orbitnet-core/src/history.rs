//! Tick-indexed history and resimulation scheduling.
//!
//! [`TickRing`] is the storage: a fixed-capacity ring addressed by tick index, so recording a tick
//! never allocates and history trimming is implicit.
//!
//! [`ResimPlanner`] is the interesting part. The GDScript backend computed a **single global resim
//! window** — the earliest unconfirmed input across *every* body, replayed for *every* body, every
//! tick. That is why one late peer degraded the whole server: its arrival lag set the window depth
//! that all bodies then paid (issue #318).
//!
//! Here each body carries its own [`DirtyWindow`], so a late body replays deeply while its
//! well-behaved neighbours replay one tick. [`ResimPlanner::global_window`] computes the old
//! behaviour alongside, which is what lets the cost difference be asserted in tests and reported as
//! a live metric rather than merely claimed.

use std::collections::BTreeMap;

/// Stable identifier for a replicated body, assigned at spawn.
pub type BodyId = u64;

/// A half-open range of ticks to resimulate: `from` inclusive, `to` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResimRange {
    /// First tick to replay.
    pub from: u64,
    /// One past the last tick to replay.
    pub to: u64,
}

impl ResimRange {
    /// How many ticks this range replays.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.to.saturating_sub(self.from)
    }

    /// Whether the range replays nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One body's entry in a resimulation plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyResim {
    /// The body to replay.
    pub body: BodyId,
    /// The ticks to replay it over.
    pub range: ResimRange,
}

/// Tracks the earliest tick a single body must be replayed from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirtyWindow {
    earliest: Option<u64>,
}

impl DirtyWindow {
    /// A clean window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that state or input changed at `tick`, deepening the window if needed.
    pub fn mark(&mut self, tick: u64) {
        self.earliest = Some(match self.earliest {
            Some(current) => current.min(tick),
            None => tick,
        });
    }

    /// The earliest dirty tick, if any.
    #[must_use]
    pub fn earliest(&self) -> Option<u64> {
        self.earliest
    }

    /// Whether anything needs replaying.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.earliest.is_some()
    }

    /// Forget the pending window without replaying it.
    pub fn clear(&mut self) {
        self.earliest = None;
    }

    /// Consume the window, returning the range to replay up to (not including) `current_tick`.
    ///
    /// The range is floored at `current_tick - history_limit`, because history older than that has
    /// already been evicted from the ring and cannot be replayed from. Returns `None` when the body
    /// is clean or when the dirty tick is not actually in the past.
    pub fn take(&mut self, current_tick: u64, history_limit: u64) -> Option<ResimRange> {
        let earliest = self.earliest.take()?;
        let floor = current_tick.saturating_sub(history_limit);
        let from = earliest.max(floor);
        if from >= current_tick {
            return None;
        }
        Some(ResimRange {
            from,
            to: current_tick,
        })
    }
}

/// Per-body resimulation scheduling.
///
/// Bodies are kept in a [`BTreeMap`] so a plan is emitted in a stable, id-ordered sequence. Replay
/// order must not vary run to run: a consumer may gate on a bit-exact resim, and a
/// nondeterministic iteration order would show up there as a phantom desync.
#[derive(Debug, Clone, Default)]
pub struct ResimPlanner {
    bodies: BTreeMap<BodyId, DirtyWindow>,
}

impl ResimPlanner {
    /// An empty planner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `body` changed at `tick`.
    pub fn mark(&mut self, body: BodyId, tick: u64) {
        self.bodies.entry(body).or_default().mark(tick);
    }

    /// Stop tracking a body, e.g. when it despawns.
    pub fn remove(&mut self, body: BodyId) {
        self.bodies.remove(&body);
    }

    /// Whether any tracked body is dirty.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.bodies.values().any(DirtyWindow::is_dirty)
    }

    /// Drop every pending window.
    pub fn clear(&mut self) {
        self.bodies.clear();
    }

    /// The window the GDScript backend would have used: one range covering every dirty body.
    ///
    /// Computed without consuming the pending windows, so it can be reported as a diagnostic
    /// next to the plan that actually ran.
    #[must_use]
    pub fn global_window(&self, current_tick: u64, history_limit: u64) -> Option<ResimRange> {
        let earliest = self
            .bodies
            .values()
            .filter_map(DirtyWindow::earliest)
            .min()?;
        let floor = current_tick.saturating_sub(history_limit);
        let from = earliest.max(floor);
        if from >= current_tick {
            return None;
        }
        Some(ResimRange {
            from,
            to: current_tick,
        })
    }

    /// Consume every pending window into a per-body plan, in ascending body order.
    pub fn plan(&mut self, current_tick: u64, history_limit: u64) -> Vec<BodyResim> {
        let mut out = Vec::new();
        for (&body, window) in &mut self.bodies {
            if let Some(range) = window.take(current_tick, history_limit) {
                out.push(BodyResim { body, range });
            }
        }
        out
    }
}

/// Total body-ticks a plan will simulate — the quantity that actually costs frame time.
#[must_use]
pub fn plan_cost(plan: &[BodyResim]) -> u64 {
    plan.iter().map(|entry| entry.range.len()).sum()
}

/// Fixed-capacity, tick-addressed history.
#[derive(Debug, Clone)]
pub struct TickRing<T> {
    slots: Vec<Option<(u64, T)>>,
    latest: Option<u64>,
}

impl<T> TickRing<T> {
    /// Create a ring holding `capacity` ticks (minimum 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || None);
        Self {
            slots,
            latest: None,
        }
    }

    /// How many ticks the ring can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// How many ticks are currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// Whether the ring holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// The newest tick stored.
    #[must_use]
    pub fn latest_tick(&self) -> Option<u64> {
        self.latest
    }

    /// The oldest tick still addressable.
    #[must_use]
    pub fn earliest_tick(&self) -> Option<u64> {
        let latest = self.latest?;
        let span = self.capacity() as u64 - 1;
        let floor = latest.saturating_sub(span);
        (floor..=latest).find(|&tick| self.get(tick).is_some())
    }

    /// Store a value for `tick`.
    ///
    /// Returns `false` — storing nothing — when the tick has already fallen out of the window,
    /// which would otherwise land in a live slot and silently corrupt a newer tick's entry.
    pub fn set(&mut self, tick: u64, value: T) -> bool {
        if let Some(latest) = self.latest {
            let span = self.capacity() as u64;
            // Saturating: `tick` may originate from a decoded frame, and a near-u64::MAX value
            // would otherwise overflow this sum.
            if tick.saturating_add(span) <= latest {
                return false;
            }
        }
        let index = (tick % self.capacity() as u64) as usize;
        self.slots[index] = Some((tick, value));
        self.latest = Some(match self.latest {
            Some(latest) => latest.max(tick),
            None => tick,
        });
        true
    }

    /// Borrow the value stored for `tick`, if it is still resident.
    #[must_use]
    pub fn get(&self, tick: u64) -> Option<&T> {
        let index = (tick % self.capacity() as u64) as usize;
        match &self.slots[index] {
            Some((stored, value)) if *stored == tick => Some(value),
            _ => None,
        }
    }

    /// Mutably borrow the value stored for `tick`.
    pub fn get_mut(&mut self, tick: u64) -> Option<&mut T> {
        let index = (tick % self.capacity() as u64) as usize;
        match &mut self.slots[index] {
            Some((stored, value)) if *stored == tick => Some(value),
            _ => None,
        }
    }

    /// The newest stored tick at or before `tick`.
    ///
    /// This is the lookup a display-only body uses: it renders the most recent authoritative state
    /// it actually has, rather than nothing at all when the exact tick never arrived.
    #[must_use]
    pub fn closest_at_or_before(&self, tick: u64) -> Option<(u64, &T)> {
        let span = self.capacity() as u64;
        let floor = tick.saturating_sub(span - 1);
        let mut probe = tick;
        loop {
            if let Some(value) = self.get(probe) {
                return Some((probe, value));
            }
            if probe == floor {
                return None;
            }
            probe -= 1;
        }
    }

    /// Drop every stored tick.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = None;
        }
        self.latest = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_stores_and_reads_back() {
        let mut ring: TickRing<u32> = TickRing::with_capacity(4);
        assert!(ring.is_empty());
        assert!(ring.set(10, 100));
        assert_eq!(ring.get(10), Some(&100));
        assert_eq!(ring.latest_tick(), Some(10));
        assert_eq!(ring.len(), 1);
        assert!(!ring.is_empty());
    }

    #[test]
    fn ring_evicts_by_wrapping() {
        let mut ring: TickRing<u32> = TickRing::with_capacity(4);
        for tick in 0..4 {
            assert!(ring.set(tick, tick as u32));
        }
        assert_eq!(ring.get(0), Some(&0));
        // Tick 4 lands in tick 0's slot.
        assert!(ring.set(4, 40));
        assert_eq!(ring.get(0), None);
        assert_eq!(ring.get(4), Some(&40));
        assert_eq!(ring.earliest_tick(), Some(1));
    }

    #[test]
    fn ring_refuses_ticks_that_fell_out_of_the_window() {
        let mut ring: TickRing<u32> = TickRing::with_capacity(4);
        ring.set(100, 1);
        // 96 would map to the same slot as 100 and clobber a live entry.
        assert!(!ring.set(96, 2));
        assert_eq!(ring.get(100), Some(&1));
        // 97 is still inside the window.
        assert!(ring.set(97, 3));
        assert_eq!(ring.get(97), Some(&3));
    }

    /// Tick indices can originate from a decoded frame, so the staleness check must not overflow
    /// on an absurd value.
    #[test]
    fn ring_handles_extreme_tick_indices() {
        let mut ring: TickRing<u32> = TickRing::with_capacity(4);
        assert!(ring.set(u64::MAX, 1));
        assert_eq!(ring.get(u64::MAX), Some(&1));
        // A tick far in the past is refused rather than overflowing the staleness comparison.
        assert!(!ring.set(0, 2));
        assert!(ring.closest_at_or_before(0).is_none());
        assert_eq!(ring.closest_at_or_before(u64::MAX), Some((u64::MAX, &1)));
    }

    #[test]
    fn ring_finds_the_closest_earlier_tick() {
        let mut ring: TickRing<u32> = TickRing::with_capacity(8);
        ring.set(10, 10);
        ring.set(13, 13);
        assert_eq!(ring.closest_at_or_before(15), Some((13, &13)));
        assert_eq!(ring.closest_at_or_before(12), Some((10, &10)));
        assert_eq!(ring.closest_at_or_before(9), None);
    }

    #[test]
    fn ring_mutation_and_clear() {
        let mut ring: TickRing<u32> = TickRing::with_capacity(4);
        ring.set(1, 1);
        if let Some(v) = ring.get_mut(1) {
            *v = 99;
        }
        assert_eq!(ring.get(1), Some(&99));
        assert!(ring.get_mut(2).is_none());
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.latest_tick(), None);
        assert_eq!(ring.earliest_tick(), None);
    }

    #[test]
    fn dirty_window_keeps_the_earliest_mark() {
        let mut window = DirtyWindow::new();
        assert!(!window.is_dirty());
        window.mark(50);
        window.mark(30);
        window.mark(40);
        assert_eq!(window.earliest(), Some(30));
        let range = window.take(60, 128).expect("window should be dirty");
        assert_eq!(range, ResimRange { from: 30, to: 60 });
        assert_eq!(range.len(), 30);
        // Taking consumed it.
        assert!(!window.is_dirty());
        assert!(window.take(60, 128).is_none());
    }

    #[test]
    fn dirty_window_is_floored_by_history_limit() {
        let mut window = DirtyWindow::new();
        window.mark(1);
        let range = window.take(1000, 128).expect("dirty");
        assert_eq!(range.from, 872, "must not replay past evicted history");
        assert_eq!(range.len(), 128);
    }

    #[test]
    fn dirty_window_ignores_marks_that_are_not_in_the_past() {
        let mut window = DirtyWindow::new();
        window.mark(60);
        assert!(window.take(60, 128).is_none());
        let mut future = DirtyWindow::new();
        future.mark(80);
        assert!(future.take(60, 128).is_none());
    }

    #[test]
    fn planner_gives_each_body_its_own_window() {
        let mut planner = ResimPlanner::new();
        planner.mark(1, 90); // healthy body, one tick behind
        planner.mark(2, 20); // late peer, deep window
        let plan = planner.plan(91, 128);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].range, ResimRange { from: 90, to: 91 });
        assert_eq!(plan[1].range, ResimRange { from: 20, to: 91 });
    }

    #[test]
    fn plan_is_emitted_in_stable_body_order() {
        let mut planner = ResimPlanner::new();
        for body in [7, 3, 9, 1] {
            planner.mark(body, 10);
        }
        let plan = planner.plan(11, 128);
        let ids: Vec<BodyId> = plan.iter().map(|entry| entry.body).collect();
        assert_eq!(ids, vec![1, 3, 7, 9]);
    }

    /// The #318 claim, as an assertion: one late peer must not deepen everyone else's replay.
    #[test]
    fn per_body_windows_beat_a_global_window_when_one_peer_is_late() {
        let current = 200;
        let limit = 128;
        let mut planner = ResimPlanner::new();
        // Seven healthy bodies, one tick behind each.
        for body in 0..7 {
            planner.mark(body, current - 1);
        }
        // One straggler 100 ticks behind.
        planner.mark(7, current - 100);

        let global = planner
            .global_window(current, limit)
            .expect("something is dirty");
        assert_eq!(global.len(), 100);
        let global_cost = global.len() * 8; // every body pays the deepest window

        let plan = planner.plan(current, limit);
        let cost = plan_cost(&plan);
        assert_eq!(cost, 7 + 100);
        assert!(
            cost * 7 < global_cost,
            "expected a large saving, got {cost} vs {global_cost}"
        );
    }

    #[test]
    fn planner_tracks_removal_and_cleanliness() {
        let mut planner = ResimPlanner::new();
        planner.mark(1, 5);
        assert!(planner.is_dirty());
        planner.remove(1);
        assert!(!planner.is_dirty());
        assert!(planner.global_window(10, 128).is_none());
        assert!(planner.plan(10, 128).is_empty());

        planner.mark(2, 5);
        planner.clear();
        assert!(!planner.is_dirty());
    }

    #[test]
    fn planning_twice_without_new_marks_is_a_no_op() {
        let mut planner = ResimPlanner::new();
        planner.mark(1, 10);
        assert_eq!(planner.plan(20, 128).len(), 1);
        assert!(
            planner.plan(21, 128).is_empty(),
            "a consumed window must not replay again"
        );
    }
}
