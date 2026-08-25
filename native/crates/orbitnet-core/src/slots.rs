//! Dense per-session entity indices — the name an entity goes by **on the wire**.
//!
//! Every hot-frame block used to open with the entity id itself: the 64-bit FNV-1a hash of the
//! synchronizer root's node path, written as an LEB128 varint. A hash output is spread across the
//! whole 64-bit range, so that varint costs **9.5 bytes on average** and LEB128 buys essentially
//! nothing. Against the demo's own state entity — 20 B of properties, 3 B of other framing — the
//! identifier was **29% of a full block and 46% of a delta**. At the default 1200 B budget, 30 Hz
//! and 100 peers that is 1.05 MB/s of server egress spent naming entities.
//!
//! A [`SlotTable`] replaces it with a **`u16` index, 2 bytes, fixed**. The id stays the identity
//! everywhere off the wire — registries, interest sets, the resim planner, `send_phase` — and the
//! slot is only ever the wire's shorthand for it.
//!
//! # What this costs, stated plainly
//!
//! The old scheme needed no distribution at all: every peer derived the same id from the same node
//! path, which is why a reconnecting client re-derived its ids with no handshake. A slot is
//! **assigned by the server**, so it has to be distributed and held. The entity manifest carries the
//! `(slot, id)` pairs, reliably, and a client that has not yet received a slot's binding skips that
//! block — which is exactly what it already did for an entity whose spawn was still in flight.
//!
//! **The manifest states a change rather than the whole table** ([`crate::codec::ManifestDelta`]),
//! so a receiver applies [`SlotTable::bind`] and [`SlotTable::unbind`] instead of clearing and
//! rebuilding. A rebuild was self-repairing and a delta is not, which is what the quarantine below
//! now also has to cover: a receiver that missed a removal keeps a binding the server has retired,
//! and the window is what stops that binding naming a *different* entity before the repair lands.
//!
//! # Reissuing a freed slot
//!
//! Ids are reused: a body respawning under its old node name reclaims the same id. Slots are reused
//! too, and reuse is the one way a slot can be *wrong* rather than merely unknown — an unreliable
//! snapshot naming slot `N` can overtake the reliable manifest that rebound `N` from entity A to
//! entity B, and the receiver would apply B's row to A.
//!
//! Two rules close that:
//!
//! - **A freed slot is quarantined for [`SLOT_QUARANTINE_TICKS`] before it may be reissued.** The
//!   window is far longer than a reliable round trip at any tick rate this backend runs, so the
//!   rebinding manifest has landed long before the slot names anything again.
//! - **The oldest expired slot is reissued first**, never the most recently freed one, so churn
//!   spends the whole free list rather than cycling one slot.
//!
//! [`SlotTable::alloc`] answers [`SlotError::Quarantined`] while every free slot is still cooling —
//! a transient refusal the caller retries next tick — and [`SlotError::Exhausted`] only when
//! [`MAX_SLOTS`] entities are live at once. **The cap is declared and refused, never wrapped**: a
//! wrapped index would alias two live entities onto one wire name.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Concurrent entities one session can name on the wire. The width of the wire field, exactly.
pub const MAX_SLOTS: usize = 1 << 16;

/// Ticks a freed slot must sit idle before it may name a different entity.
///
/// Chosen against the delivery it has to outlast, not against a wall clock: the manifest that
/// rebinds the slot goes out reliably, so the window only has to cover a reliable retransmit — a
/// few round trips. 256 ticks is ~4.3 s at 60 Hz and ~12.8 s at 20 Hz, orders of magnitude clear of
/// that, and it costs nothing except delaying reuse in a session that is churning entities faster
/// than it is creating them.
pub const SLOT_QUARANTINE_TICKS: u64 = 256;

/// Why a slot could not be issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// Every free slot is still inside its quarantine window. **Transient** — retry next tick.
    Quarantined,
    /// [`MAX_SLOTS`] entities are live at once, so the session has no wire name left to give.
    /// **Terminal** for this entity until something unregisters.
    Exhausted,
}

impl core::fmt::Display for SlotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SlotError::Quarantined => write!(
                f,
                "every free entity slot is still inside its reuse quarantine; retry next tick"
            ),
            SlotError::Exhausted => write!(
                f,
                "all {MAX_SLOTS} entity slots are in use; this session cannot name another entity \
                 on the wire"
            ),
        }
    }
}

impl std::error::Error for SlotError {}

/// What one [`SlotTable::reconcile`] pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reconciled {
    /// Slots returned to the free list because their entity is no longer registered.
    pub released: usize,
    /// Entities that were given a slot this pass.
    pub named: usize,
    /// Entities still without one. Non-zero means run again next tick.
    pub unnamed: usize,
    /// The lowest id refused because every slot is in use — [`SlotError::Exhausted`], the terminal
    /// case, which is worth telling an operator about. `None` when the only refusals were
    /// quarantine, which expires on its own.
    pub exhausted: Option<u64>,
}

/// The session's map between 64-bit entity ids and the dense `u16` indices the wire carries.
///
/// **One type, two roles.** The server *allocates* ([`SlotTable::alloc`] / [`SlotTable::release`])
/// and is the authority. A client *binds* ([`SlotTable::bind`]) what the manifest tells it and
/// allocates nothing. Sharing the type is what keeps `slot_of`/`id_of` reading the same on both
/// sides of the wire.
#[derive(Debug, Clone, Default)]
pub struct SlotTable {
    /// Slot to id, indexed by slot. `0` is vacant — id `0` already means "no entity" everywhere
    /// else in the backend (an unresolved synchronizer reports it, and every id-taking call refuses
    /// it), so the sentinel costs no representable value and halves the table against
    /// `Vec<Option<u64>>`.
    ids: Vec<u64>,
    /// Id to slot. A `BTreeMap` rather than a hash map so a debug dump reads in a stable order.
    slots: BTreeMap<u64, u16>,
    /// Freed slots with the tick each was freed on, oldest first. Ticks are monotonic, so the front
    /// is always the oldest and the quarantine check never has to scan.
    free: VecDeque<(u16, u64)>,
}

impl SlotTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Entities currently named.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether nothing is named.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Slots ever minted, which is where the table sits against [`MAX_SLOTS`].
    ///
    /// Reuse is preferred over minting, so this settles at peak concurrent entities **plus whatever
    /// was freed inside the last quarantine window** — a session that churns faster than the
    /// quarantine expires walks toward the cap until the churn slows.
    #[must_use]
    pub fn frontier(&self) -> usize {
        self.ids.len()
    }

    /// Forget everything. A table describes one session; carrying it into the next one would name
    /// a stranger.
    pub fn clear(&mut self) {
        self.ids.clear();
        self.slots.clear();
        self.free.clear();
    }

    /// The wire name for `id`, if it has one.
    #[must_use]
    pub fn slot_of(&self, id: u64) -> Option<u16> {
        self.slots.get(&id).copied()
    }

    /// Every named entity id, ascending. The order is stable, so a caller reconciling this table
    /// against its registries does the same work in the same order on every run.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots.keys().copied()
    }

    /// Every `(slot, id)` binding, ascending by id — what the manifest puts on the wire.
    pub fn bindings(&self) -> impl Iterator<Item = (u16, u64)> + '_ {
        self.slots.iter().map(|(&id, &slot)| (slot, id))
    }

    /// The entity `slot` names, if this table knows it.
    #[must_use]
    pub fn id_of(&self, slot: u16) -> Option<u64> {
        match self.ids.get(usize::from(slot)).copied() {
            Some(0) | None => None,
            Some(id) => Some(id),
        }
    }

    /// SERVER: name `id`, reusing an expired slot before minting a new one.
    ///
    /// Idempotent — an id that already holds a slot answers that slot and nothing moves, so the
    /// caller may run this over the whole registry every tick without bookkeeping of its own.
    ///
    /// Reuse is preferred over minting deliberately: it holds [`SlotTable::frontier`] near the
    /// session's peak concurrency instead of letting the index space climb with every spawn. It
    /// cannot hold it exactly there — a slot freed inside the quarantine window is not available to
    /// the next caller, so a fast-churning session does mint past its peak.
    pub fn alloc(&mut self, id: u64, tick: u64) -> Result<u16, SlotError> {
        if id == 0 {
            // Id 0 is the backend's "no entity". Naming it would make `id_of` answer a vacant slot.
            return Err(SlotError::Exhausted);
        }
        if let Some(&slot) = self.slots.get(&id) {
            return Ok(slot);
        }
        if let Some(&(slot, freed_tick)) = self.free.front() {
            if tick.saturating_sub(freed_tick) >= SLOT_QUARANTINE_TICKS {
                self.free.pop_front();
                self.bind_at(slot, id);
                return Ok(slot);
            }
        }
        if self.ids.len() < MAX_SLOTS {
            let slot = u16::try_from(self.ids.len()).map_err(|_| SlotError::Exhausted)?;
            self.ids.push(id);
            self.slots.insert(id, slot);
            return Ok(slot);
        }
        if self.free.is_empty() {
            Err(SlotError::Exhausted)
        } else {
            Err(SlotError::Quarantined)
        }
    }

    /// SERVER: give up `id`'s slot, starting its quarantine at `tick`. Answers the slot released.
    pub fn release(&mut self, id: u64, tick: u64) -> Option<u16> {
        let slot = self.slots.remove(&id)?;
        if let Some(entry) = self.ids.get_mut(usize::from(slot)) {
            *entry = 0;
        }
        self.free.push_back((slot, tick));
        Some(slot)
    }

    /// CLIENT: record what the manifest says, replacing any previous binding of either side.
    ///
    /// Both directions are replaced because a manifest is the authority on both: a slot that has
    /// been reissued must stop naming its old entity, and an entity that moved slots must stop
    /// answering its old one. Leaving either behind would let `slot_of`/`id_of` disagree.
    pub fn bind(&mut self, slot: u16, id: u64) {
        if id == 0 {
            return;
        }
        if let Some(previous) = self.id_of(slot) {
            self.slots.remove(&previous);
        }
        if let Some(previous_slot) = self.slots.get(&id).copied() {
            if let Some(entry) = self.ids.get_mut(usize::from(previous_slot)) {
                *entry = 0;
            }
        }
        self.bind_at(slot, id);
    }

    /// CLIENT: drop `slot`'s binding in both directions, answering the id it named.
    ///
    /// **This is not [`SlotTable::release`], and a client must never call that one.** `release` is
    /// the SERVER's record of a name it may hand out again: it pushes the slot onto the free list
    /// with the tick it was freed on, and that stamp is what
    /// [`SLOT_QUARANTINE_TICKS`] is measured from. A client issues no names, so a free list on its
    /// side would describe a decision it does not make — and a client that pushed onto one would
    /// start refusing its own [`SlotTable::alloc`] calls with [`SlotError::Quarantined`] if anything
    /// ever asked it for a slot.
    ///
    /// What this is for is the **removal half of an entity-manifest delta**
    /// ([`crate::codec::ManifestDelta`]). A complete manifest needed nothing like it: the receiver
    /// cleared its table and rebuilt, so a binding that had gone away simply did not come back. A
    /// delta names the slot instead, and the receiver has to retire exactly that binding and leave
    /// every other one alone.
    ///
    /// Answers `None` for a slot that named nothing, which is the ordinary case for a duplicate or
    /// re-ordered record rather than an error.
    pub fn unbind(&mut self, slot: u16) -> Option<u64> {
        let id = self.id_of(slot)?;
        if let Some(entry) = self.ids.get_mut(usize::from(slot)) {
            *entry = 0;
        }
        self.slots.remove(&id);
        Some(id)
    }

    /// SERVER: make this table name exactly the entities in `registered`, as of `tick`.
    ///
    /// **Releases before it allocates**, so a pass that swaps one entity for another returns the
    /// departing slot to the free list before the arrival asks for one. The arrival still does not
    /// get that slot — the quarantine refuses it — but the free list's order stays honest.
    ///
    /// Idempotent, and cheap when nothing changed: a pass over a table that already agrees with
    /// `registered` releases nothing, allocates nothing and reports `unnamed: 0`. A non-zero
    /// `unnamed` is the caller's signal to run again next tick, which is how an entity refused
    /// during its predecessor's quarantine eventually gets named instead of being stranded.
    pub fn reconcile(&mut self, registered: &BTreeSet<u64>, tick: u64) -> Reconciled {
        let mut outcome = Reconciled::default();

        let stale: Vec<u64> = self.ids().filter(|id| !registered.contains(id)).collect();
        outcome.released = stale.len();
        for id in stale {
            self.release(id, tick);
        }

        for &id in registered {
            if self.slot_of(id).is_some() {
                continue;
            }
            match self.alloc(id, tick) {
                Ok(_) => outcome.named += 1,
                Err(error) => {
                    outcome.unnamed += 1;
                    if error == SlotError::Exhausted && outcome.exhausted.is_none() {
                        outcome.exhausted = Some(id);
                    }
                }
            }
        }
        outcome
    }

    /// Write a binding both ways, growing the slot vector to reach `slot`.
    ///
    /// Growth is bounded by the wire field: `u16` caps the vector at [`MAX_SLOTS`] entries of 8
    /// bytes, so the worst a peer can make a receiver allocate here is 512 KiB.
    fn bind_at(&mut self, slot: u16, id: u64) {
        let index = usize::from(slot);
        if index >= self.ids.len() {
            self.ids.resize(index + 1, 0);
        }
        self.ids[index] = id;
        self.slots.insert(id, slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_table_mints_dense_ascending_slots() {
        let mut table = SlotTable::new();
        for expected in 0u16..8 {
            let id = u64::from(expected) + 100;
            assert_eq!(table.alloc(id, 0), Ok(expected));
        }
        assert_eq!(table.len(), 8);
        assert_eq!(table.frontier(), 8);
        assert_eq!(table.slot_of(103), Some(3));
        assert_eq!(table.id_of(3), Some(103));
    }

    #[test]
    fn allocating_a_named_id_answers_its_slot_and_moves_nothing() {
        let mut table = SlotTable::new();
        let first = table.alloc(7, 0).unwrap();
        for tick in 0..1_000 {
            assert_eq!(table.alloc(7, tick), Ok(first));
        }
        assert_eq!(table.len(), 1);
        assert_eq!(table.frontier(), 1);
    }

    #[test]
    fn id_zero_is_never_named() {
        let mut table = SlotTable::new();
        assert_eq!(table.alloc(0, 0), Err(SlotError::Exhausted));
        table.bind(4, 0);
        assert_eq!(table.id_of(4), None);
        assert!(table.is_empty());
    }

    #[test]
    fn an_unknown_slot_names_nothing() {
        let mut table = SlotTable::new();
        table.alloc(9, 0).unwrap();
        assert_eq!(table.id_of(1), None);
        assert_eq!(table.id_of(u16::MAX), None);
        assert_eq!(table.slot_of(10), None);
    }

    #[test]
    fn a_freed_slot_is_quarantined_before_it_is_reissued() {
        let mut table = SlotTable::new();
        let a = table.alloc(11, 0).unwrap();
        table.alloc(12, 0).unwrap();
        table.release(11, 10);
        assert_eq!(table.id_of(a), None, "a released slot names nothing");

        // Inside the window the frontier grows instead of reusing.
        let fresh = table.alloc(13, 10 + SLOT_QUARANTINE_TICKS - 1).unwrap();
        assert_ne!(fresh, a);
        assert_eq!(table.frontier(), 3);

        // On the boundary tick it is reissued, and the frontier does not move.
        let reused = table.alloc(14, 10 + SLOT_QUARANTINE_TICKS).unwrap();
        assert_eq!(reused, a);
        assert_eq!(table.frontier(), 3);
        assert_eq!(table.id_of(a), Some(14));
    }

    #[test]
    fn a_respawn_under_the_same_id_reclaims_a_slot_only_after_quarantine() {
        // The id is node-path-derived, so a body respawning under its old name asks for the same id.
        let mut table = SlotTable::new();
        let before = table.alloc(42, 0).unwrap();
        table.release(42, 5);
        let during = table.alloc(42, 6).unwrap();
        assert_ne!(
            during, before,
            "the same id respawning inside the window takes a fresh slot, not its old one"
        );
        assert_eq!(table.id_of(before), None);
        assert_eq!(table.id_of(during), Some(42));
    }

    #[test]
    fn the_oldest_expired_slot_is_reissued_first() {
        let mut table = SlotTable::new();
        for id in 1u64..=4 {
            table.alloc(id, 0).unwrap();
        }
        table.release(3, 10);
        table.release(1, 20);
        table.release(2, 30);

        let tick = 30 + SLOT_QUARANTINE_TICKS;
        assert_eq!(table.alloc(100, tick), Ok(2), "slot 2 was freed first");
        assert_eq!(table.alloc(101, tick), Ok(0), "then slot 0");
        assert_eq!(table.alloc(102, tick), Ok(1), "then slot 1");
        assert_eq!(table.frontier(), 4, "reuse never grew the frontier");
    }

    #[test]
    fn every_free_slot_still_cooling_is_a_transient_refusal() {
        let mut table = SlotTable::new();
        // Fill the index space, then free exactly one slot.
        for id in 1..=MAX_SLOTS as u64 {
            table.alloc(id, 0).unwrap();
        }
        assert_eq!(table.frontier(), MAX_SLOTS);
        assert_eq!(
            table.alloc(u64::MAX, 0),
            Err(SlotError::Exhausted),
            "a full table with nothing freed is terminal"
        );
        table.release(1, 0);
        assert_eq!(
            table.alloc(u64::MAX, 1),
            Err(SlotError::Quarantined),
            "a full table with a cooling slot is transient"
        );
        assert_eq!(table.alloc(u64::MAX, SLOT_QUARANTINE_TICKS), Ok(0));
    }

    #[test]
    fn binding_replaces_both_directions() {
        let mut table = SlotTable::new();
        table.bind(5, 500);
        table.bind(6, 600);
        assert_eq!(table.slot_of(500), Some(5));

        // The slot is reissued to another entity: its old id must stop answering.
        table.bind(5, 700);
        assert_eq!(table.id_of(5), Some(700));
        assert_eq!(table.slot_of(500), None);

        // An entity moves slots: its old slot must stop answering.
        table.bind(9, 600);
        assert_eq!(table.slot_of(600), Some(9));
        assert_eq!(table.id_of(6), None);
        assert_eq!(table.id_of(9), Some(600));
    }

    /// The removal half of a manifest delta: both directions cleared, and the free list untouched.
    ///
    /// A client holds no free list — it issues no names — and pushing onto one would make the
    /// client's own `alloc` start refusing with `Quarantined` for slots the server owns.
    #[test]
    fn unbinding_clears_both_directions_and_never_touches_the_free_list() {
        let mut table = SlotTable::new();
        table.bind(3, 300);
        table.bind(4, 400);

        assert_eq!(table.unbind(3), Some(300), "it answers the id it retired");
        assert_eq!(table.id_of(3), None);
        assert_eq!(
            table.slot_of(300),
            None,
            "and the reverse direction with it"
        );
        assert_eq!(
            table.id_of(4),
            Some(400),
            "every other binding is left alone"
        );
        assert_eq!(table.len(), 1);

        // Unbinding a slot that named nothing is the ordinary duplicate record, not an error.
        assert_eq!(table.unbind(3), None);
        assert_eq!(table.unbind(u16::MAX), None);
        assert_eq!(table.unbind(9), None);

        // THE FREE LIST IS UNTOUCHED. `release` would have pushed slot 3 onto it with a quarantine
        // stamp; an `alloc` right after this proves nothing was pushed, because a table holding a
        // freed slot reissues it once the window expires and this one mints instead.
        assert_eq!(
            table.alloc(500, SLOT_QUARANTINE_TICKS),
            Ok(5),
            "the frontier grew past the bound slots; nothing was reused"
        );
        assert_eq!(table.id_of(3), None, "and slot 3 was never handed out");
    }

    /// The other half of that: `release` IS a free-list push, so the two are not interchangeable.
    #[test]
    fn releasing_records_a_reuse_and_unbinding_does_not() {
        let mut released = SlotTable::new();
        released.alloc(1, 0).unwrap();
        released.release(1, 0);
        assert_eq!(
            released.alloc(2, SLOT_QUARANTINE_TICKS),
            Ok(0),
            "a released slot comes back once its quarantine expires"
        );

        let mut unbound = SlotTable::new();
        unbound.alloc(1, 0).unwrap();
        unbound.unbind(0);
        assert_eq!(
            unbound.alloc(2, SLOT_QUARANTINE_TICKS),
            Ok(1),
            "an unbound slot is never reissued, because nothing recorded it as free"
        );
    }

    #[test]
    fn a_bound_table_round_trips_every_slot_it_was_given() {
        let mut table = SlotTable::new();
        let pairs: Vec<(u16, u64)> = (1..64u16)
            .map(|n| {
                (
                    n.wrapping_mul(1_031),
                    u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15),
                )
            })
            .filter(|&(_, id)| id != 0)
            .collect();
        for &(slot, id) in &pairs {
            table.bind(slot, id);
        }
        for &(slot, id) in &pairs {
            assert_eq!(table.id_of(slot), Some(id));
            assert_eq!(table.slot_of(id), Some(slot));
        }
    }

    fn registry(ids: &[u64]) -> BTreeSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn reconciling_names_every_registered_entity_and_releases_the_rest() {
        let mut table = SlotTable::new();
        let outcome = table.reconcile(&registry(&[10, 20, 30]), 0);
        assert_eq!(outcome.named, 3);
        assert_eq!(outcome.released, 0);
        assert_eq!(outcome.unnamed, 0);
        assert_eq!(table.len(), 3);

        // 20 unregisters, 40 arrives.
        let outcome = table.reconcile(&registry(&[10, 30, 40]), 1);
        assert_eq!(outcome.released, 1, "20's slot went back to the free list");
        assert_eq!(outcome.named, 1, "40 was named");
        assert_eq!(table.slot_of(20), None);
        assert!(table.slot_of(40).is_some());
        assert_ne!(
            table.slot_of(40),
            Some(1),
            "40 did not take the slot 20 gave up in the same pass"
        );
    }

    #[test]
    fn reconciling_an_agreeing_table_changes_nothing() {
        let mut table = SlotTable::new();
        let registered = registry(&[1, 2, 3]);
        table.reconcile(&registered, 0);
        let before: Vec<(u16, u64)> = table.bindings().collect();
        for tick in 1..100 {
            assert_eq!(
                table.reconcile(&registered, tick),
                Reconciled::default(),
                "an agreeing table did work on tick {tick}"
            );
        }
        assert_eq!(table.bindings().collect::<Vec<_>>(), before);
    }

    #[test]
    fn a_quarantined_refusal_is_retried_and_eventually_named() {
        // One slot minted, freed, and a different entity asking for a name while it cools. The
        // frontier absorbs this one, so drive the table to the cap to force the refusal.
        let mut table = SlotTable::new();
        let full: BTreeSet<u64> = (1..=MAX_SLOTS as u64).collect();
        table.reconcile(&full, 0);

        // Swap one entity for another: the departing slot is the only one free, and it is cooling.
        let mut swapped = full.clone();
        swapped.remove(&1);
        swapped.insert(u64::MAX);
        let outcome = table.reconcile(&swapped, 10);
        assert_eq!(outcome.released, 1);
        assert_eq!(outcome.unnamed, 1);
        assert_eq!(outcome.exhausted, None, "quarantine is not exhaustion");
        assert_eq!(table.slot_of(u64::MAX), None);

        // Retried each tick, it is named the moment the quarantine expires.
        let outcome = table.reconcile(&swapped, 10 + SLOT_QUARANTINE_TICKS);
        assert_eq!(outcome.named, 1);
        assert_eq!(outcome.unnamed, 0);
        assert_eq!(table.slot_of(u64::MAX), Some(0));
    }

    #[test]
    fn exhaustion_names_the_entity_that_could_not_be_replicated() {
        let mut table = SlotTable::new();
        let mut registered: BTreeSet<u64> = (1..=MAX_SLOTS as u64).collect();
        table.reconcile(&registered, 0);
        registered.insert(u64::MAX - 1);
        registered.insert(u64::MAX);
        let outcome = table.reconcile(&registered, 0);
        assert_eq!(outcome.unnamed, 2);
        assert_eq!(
            outcome.exhausted,
            Some(u64::MAX - 1),
            "the lowest refused id is the one reported"
        );
    }

    #[test]
    fn clearing_forgets_the_session() {
        let mut table = SlotTable::new();
        table.alloc(1, 0).unwrap();
        table.release(1, 0);
        table.alloc(2, 0).unwrap();
        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.frontier(), 0);
        assert_eq!(table.id_of(0), None);
        assert_eq!(
            table.alloc(3, 0),
            Ok(0),
            "the next session starts at slot 0"
        );
    }
}
