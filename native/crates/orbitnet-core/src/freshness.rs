//! Input-novelty freshness — the #67 fix.
//!
//! Netfox's `is_fresh` means "this `(node, tick)` pair has not been visited before" — a visitation
//! high-water mark. When the server predicts tick `T` with no input and *then* receives the
//! client's real input for `T`, it resimulates with `is_fresh = false` even though that resim is
//! the **first time the real input was seen**. Game code that must fire an effect exactly once per
//! real input (weapon shots, one-shot interactions) ends up carrying its own workarounds, like
//! `weapon_authority.gd`'s `_resolved_shot_seq` high-water marks and its non-replicated 256-tick
//! `_held_cat_log`.
//!
//! [`FreshnessLedger`] keys freshness on input **novelty** instead. Each tick carries a
//! [`Confidence`]: `Predicted` (no input at all), `Extrapolated` (input carried forward from an
//! older tick), or `Authoritative` (real received — or locally authored — input stamped for this
//! tick). Confidence only ever upgrades, and [`FreshnessLedger::begin_sim`] answers `is_fresh`:
//! true exactly once per tick, on the first simulation pass whose input is `Authoritative`. A
//! predicted or extrapolated pass returns false *without consuming*, so however many speculative
//! passes precede the packet, the resim that first carries the real input still reads fresh.
//!
//! [`MemoRing`] is the companion `commit_once`/tick-memo primitive: a tick-indexed ring of small
//! key→value pair lists that replaces the hand-rolled `_held_cat_log`. A body memos one to three
//! keys per tick, so lookups are small-N linear scans rather than a `HashMap` — and eviction
//! reuses each slot's `Vec` via `clear()`, keeping the steady state allocation-free.

/// How much the simulating peer actually knows about a tick's input.
///
/// Ordered so "upgrades only" is expressible: `Predicted < Extrapolated < Authoritative`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// No input at all for this tick.
    Predicted,
    /// Input repeated or carried forward from an older tick.
    Extrapolated,
    /// Real received (or locally authored) input stamped for this tick.
    Authoritative,
}

/// One resident tick's freshness record.
#[derive(Debug, Clone, Copy)]
struct LedgerSlot {
    tick: u64,
    confidence: Confidence,
    fresh_consumed: bool,
}

/// Fixed-capacity, tick-indexed ledger of per-tick input confidence.
///
/// Slot addressing follows [`crate::history::TickRing`]: `slot = tick % capacity`, and a slot
/// whose stored tick differs from the queried tick reads as empty — [`Confidence::Predicted`],
/// freshness unconsumed.
#[derive(Debug, Clone)]
pub struct FreshnessLedger {
    slots: Vec<Option<LedgerSlot>>,
}

impl FreshnessLedger {
    /// Create a ledger covering `capacity` ticks (minimum 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || None);
        Self { slots }
    }

    /// Record the confidence for `tick`, upgrades only.
    ///
    /// A level at or below the stored one is a no-op — confidence never downgrades, so a late
    /// duplicate or an extrapolation pass cannot erase the fact that real input arrived. Writing
    /// to a slot holding an *older* tick evicts it (freshness resets unconsumed); writing a tick
    /// older than the slot's resident tick is refused as stale, like `TickRing::set`.
    pub fn set_confidence(&mut self, tick: u64, confidence: Confidence) {
        let index = (tick % self.slots.len() as u64) as usize;
        match &mut self.slots[index] {
            Some(slot) if slot.tick == tick => {
                if confidence > slot.confidence {
                    slot.confidence = confidence;
                }
            }
            // Stale: the slot already holds a newer tick — refuse rather than corrupt it.
            Some(slot) if slot.tick > tick => {}
            slot => {
                *slot = Some(LedgerSlot {
                    tick,
                    confidence,
                    fresh_consumed: false,
                });
            }
        }
    }

    /// The recorded confidence for `tick` — [`Confidence::Predicted`] when absent.
    #[must_use]
    pub fn confidence(&self, tick: u64) -> Confidence {
        let index = (tick % self.slots.len() as u64) as usize;
        match &self.slots[index] {
            Some(slot) if slot.tick == tick => slot.confidence,
            _ => Confidence::Predicted,
        }
    }

    /// Begin a simulation pass over `tick` — this is the `is_fresh` call.
    ///
    /// Returns true iff the tick's input is [`Confidence::Authoritative`] and its freshness has
    /// not already been consumed; consumes it when returning true. A pass at `Predicted` or
    /// `Extrapolated` returns false and does **not** consume, so the later authoritative resim of
    /// the same tick still reads fresh exactly once (the #67 scenario).
    pub fn begin_sim(&mut self, tick: u64) -> bool {
        let index = (tick % self.slots.len() as u64) as usize;
        match &mut self.slots[index] {
            Some(slot)
                if slot.tick == tick
                    && slot.confidence == Confidence::Authoritative
                    && !slot.fresh_consumed =>
            {
                slot.fresh_consumed = true;
                true
            }
            _ => false,
        }
    }

    /// Forget every recorded tick.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = None;
        }
    }
}

/// One resident tick's memo pairs. The `Vec` outlives eviction — cleared, never dropped — so the
/// ring stops allocating once each slot has grown to its working size.
#[derive(Debug, Clone, Default)]
struct MemoSlot {
    tick: Option<u64>,
    pairs: Vec<(i64, i64)>,
}

/// Fixed-capacity, tick-indexed memo storage — the `commit_once` primitive's ledger.
///
/// Each resident tick holds a small key→value pair list (a body memos one to three keys, so
/// lookups are linear scans — no `HashMap`, no per-tick allocation churn). Slot addressing and
/// stale-write refusal follow [`FreshnessLedger`].
#[derive(Debug, Clone)]
pub struct MemoRing {
    slots: Vec<MemoSlot>,
}

impl MemoRing {
    /// Create a ring covering `capacity` ticks (minimum 1).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, MemoSlot::default);
        Self { slots }
    }

    /// Store (or overwrite) `key`'s value for `tick`.
    ///
    /// Writing to a slot holding an *older* tick evicts it — the pair list is cleared first, so a
    /// new tick never inherits a stale memo. Returns `false` — storing nothing — when the slot
    /// holds a newer tick, which would otherwise be silently corrupted.
    pub fn set(&mut self, tick: u64, key: i64, value: i64) -> bool {
        let index = (tick % self.slots.len() as u64) as usize;
        let slot = &mut self.slots[index];
        match slot.tick {
            Some(stored) if stored > tick => return false,
            Some(stored) if stored < tick => {
                slot.pairs.clear();
                slot.tick = Some(tick);
            }
            None => slot.tick = Some(tick),
            _ => {}
        }
        for pair in &mut slot.pairs {
            if pair.0 == key {
                pair.1 = value;
                return true;
            }
        }
        slot.pairs.push((key, value));
        true
    }

    /// The value memoed for `key` at `tick`, if that tick is still resident and holds the key.
    #[must_use]
    pub fn get(&self, tick: u64, key: i64) -> Option<i64> {
        let index = (tick % self.slots.len() as u64) as usize;
        let slot = &self.slots[index];
        if slot.tick != Some(tick) {
            return None;
        }
        slot.pairs
            .iter()
            .find(|pair| pair.0 == key)
            .map(|pair| pair.1)
    }

    /// Forget every memo. Slot `Vec`s keep their grown capacity for reuse.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.tick = None;
            slot.pairs.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_ordering_expresses_upgrade_only() {
        assert!(Confidence::Predicted < Confidence::Extrapolated);
        assert!(Confidence::Extrapolated < Confidence::Authoritative);
    }

    #[test]
    fn absent_tick_reads_predicted_and_never_fresh() {
        let mut ledger = FreshnessLedger::with_capacity(8);
        assert_eq!(ledger.confidence(5), Confidence::Predicted);
        assert!(!ledger.begin_sim(5));
        // A failed begin_sim must not have materialised a slot.
        assert_eq!(ledger.confidence(5), Confidence::Predicted);
    }

    #[test]
    fn confidence_upgrades_but_never_downgrades() {
        let mut ledger = FreshnessLedger::with_capacity(8);
        ledger.set_confidence(3, Confidence::Extrapolated);
        assert_eq!(ledger.confidence(3), Confidence::Extrapolated);
        // Downgrade attempts are no-ops.
        ledger.set_confidence(3, Confidence::Predicted);
        assert_eq!(ledger.confidence(3), Confidence::Extrapolated);
        ledger.set_confidence(3, Confidence::Authoritative);
        assert_eq!(ledger.confidence(3), Confidence::Authoritative);
        ledger.set_confidence(3, Confidence::Extrapolated);
        assert_eq!(ledger.confidence(3), Confidence::Authoritative);
    }

    #[test]
    fn begin_sim_consumes_freshness_exactly_once() {
        let mut ledger = FreshnessLedger::with_capacity(8);
        ledger.set_confidence(7, Confidence::Authoritative);
        assert!(ledger.begin_sim(7));
        assert!(!ledger.begin_sim(7), "freshness must be consumed once");
    }

    /// The #67 scenario: predicted passes must not spend the tick's freshness, so the resim that
    /// first carries the real input still reads fresh — exactly once.
    #[test]
    fn predicted_and_extrapolated_passes_do_not_consume_freshness() {
        let mut ledger = FreshnessLedger::with_capacity(8);
        // Server simulates tick T with no input at all.
        assert!(!ledger.begin_sim(42));
        // A later pass carries input repeated from an older tick.
        ledger.set_confidence(42, Confidence::Extrapolated);
        assert!(!ledger.begin_sim(42));
        // The client's real input for T finally lands: the resim is the fresh pass.
        ledger.set_confidence(42, Confidence::Authoritative);
        assert!(ledger.begin_sim(42));
        // And only that one.
        assert!(!ledger.begin_sim(42));
    }

    #[test]
    fn re_stamping_authoritative_does_not_rearm_freshness() {
        let mut ledger = FreshnessLedger::with_capacity(8);
        ledger.set_confidence(9, Confidence::Authoritative);
        assert!(ledger.begin_sim(9));
        // A duplicate packet re-stamps the same level; that is not new novelty.
        ledger.set_confidence(9, Confidence::Authoritative);
        assert!(!ledger.begin_sim(9));
    }

    #[test]
    fn ledger_eviction_resets_freshness() {
        let mut ledger = FreshnessLedger::with_capacity(4);
        ledger.set_confidence(0, Confidence::Authoritative);
        assert!(ledger.begin_sim(0));
        // Tick 4 lands in tick 0's slot: the old record is gone, freshness starts unconsumed.
        ledger.set_confidence(4, Confidence::Authoritative);
        assert_eq!(ledger.confidence(0), Confidence::Predicted);
        assert!(ledger.begin_sim(4));
        assert!(!ledger.begin_sim(4));
    }

    #[test]
    fn ledger_refuses_stale_writes() {
        let mut ledger = FreshnessLedger::with_capacity(4);
        ledger.set_confidence(100, Confidence::Authoritative);
        // 96 maps to the same slot as 100 and would clobber a live, newer record.
        ledger.set_confidence(96, Confidence::Authoritative);
        assert_eq!(ledger.confidence(96), Confidence::Predicted);
        assert_eq!(ledger.confidence(100), Confidence::Authoritative);
        assert!(ledger.begin_sim(100));
    }

    /// Tick indices can originate from a decoded frame, so nothing may overflow on an absurd one.
    #[test]
    fn ledger_handles_extreme_tick_indices() {
        let mut ledger = FreshnessLedger::with_capacity(4);
        ledger.set_confidence(u64::MAX, Confidence::Authoritative);
        assert_eq!(ledger.confidence(u64::MAX), Confidence::Authoritative);
        assert!(ledger.begin_sim(u64::MAX));
        // An older tick in the same slot is refused rather than evicting the newer record.
        ledger.set_confidence(u64::MAX - 4, Confidence::Authoritative);
        assert_eq!(ledger.confidence(u64::MAX - 4), Confidence::Predicted);
        assert!(!ledger.begin_sim(u64::MAX - 4));
    }

    #[test]
    fn ledger_clear_forgets_everything() {
        let mut ledger = FreshnessLedger::with_capacity(4);
        ledger.set_confidence(2, Confidence::Authoritative);
        assert!(ledger.begin_sim(2));
        ledger.clear();
        assert_eq!(ledger.confidence(2), Confidence::Predicted);
        assert!(!ledger.begin_sim(2));
        // A fresh session can re-stamp the same tick from scratch.
        ledger.set_confidence(2, Confidence::Authoritative);
        assert!(ledger.begin_sim(2));
    }

    #[test]
    fn capacity_is_floored_at_one() {
        let mut ledger = FreshnessLedger::with_capacity(0);
        ledger.set_confidence(5, Confidence::Authoritative);
        assert!(ledger.begin_sim(5));
        // One slot total: the next tick evicts the previous one.
        ledger.set_confidence(6, Confidence::Authoritative);
        assert_eq!(ledger.confidence(5), Confidence::Predicted);
        assert!(ledger.begin_sim(6));

        let mut memo = MemoRing::with_capacity(0);
        assert!(memo.set(5, 1, 10));
        assert!(memo.set(6, 1, 11));
        assert_eq!(memo.get(5, 1), None);
        assert_eq!(memo.get(6, 1), Some(11));
    }

    #[test]
    fn memo_stores_and_reads_back() {
        let mut memo = MemoRing::with_capacity(8);
        assert!(memo.set(10, 1, 100));
        assert!(memo.set(10, 2, 200));
        assert_eq!(memo.get(10, 1), Some(100));
        assert_eq!(memo.get(10, 2), Some(200));
        assert_eq!(memo.get(10, 3), None, "unknown key");
        assert_eq!(memo.get(11, 1), None, "unknown tick");
    }

    #[test]
    fn memo_overwrites_a_key_within_a_tick() {
        let mut memo = MemoRing::with_capacity(8);
        assert!(memo.set(10, 1, 100));
        assert!(memo.set(10, 1, 999));
        assert_eq!(memo.get(10, 1), Some(999));
        // The neighbouring key is untouched.
        assert!(memo.set(10, 2, 200));
        assert!(memo.set(10, 1, 111));
        assert_eq!(memo.get(10, 1), Some(111));
        assert_eq!(memo.get(10, 2), Some(200));
    }

    #[test]
    fn memo_survives_until_eviction() {
        let mut memo = MemoRing::with_capacity(4);
        assert!(memo.set(1, 7, 70));
        // Other ticks inside the window leave it resident.
        assert!(memo.set(2, 7, 71));
        assert!(memo.set(3, 7, 72));
        assert_eq!(memo.get(1, 7), Some(70));
        // Tick 5 lands in tick 1's slot and must not inherit its pairs.
        assert!(memo.set(5, 8, 80));
        assert_eq!(memo.get(1, 7), None);
        assert_eq!(memo.get(5, 7), None, "evicted pairs must not leak");
        assert_eq!(memo.get(5, 8), Some(80));
    }

    #[test]
    fn memo_refuses_stale_writes() {
        let mut memo = MemoRing::with_capacity(4);
        assert!(memo.set(100, 1, 1));
        // 96 maps to the same slot as 100: refused, nothing stored, resident tick intact.
        assert!(!memo.set(96, 2, 2));
        assert_eq!(memo.get(96, 2), None);
        assert_eq!(memo.get(100, 1), Some(1));
    }

    #[test]
    fn memo_handles_extreme_tick_indices() {
        let mut memo = MemoRing::with_capacity(4);
        assert!(memo.set(u64::MAX, 1, 1));
        assert_eq!(memo.get(u64::MAX, 1), Some(1));
        assert!(
            !memo.set(u64::MAX - 4, 1, 2),
            "older same-slot tick refused"
        );
        assert_eq!(memo.get(u64::MAX, 1), Some(1));
    }

    #[test]
    fn memo_clear_forgets_everything() {
        let mut memo = MemoRing::with_capacity(4);
        memo.set(1, 1, 10);
        memo.set(2, 2, 20);
        memo.clear();
        assert_eq!(memo.get(1, 1), None);
        assert_eq!(memo.get(2, 2), None);
        // Reusable after clearing.
        assert!(memo.set(1, 1, 30));
        assert_eq!(memo.get(1, 1), Some(30));
    }
}
