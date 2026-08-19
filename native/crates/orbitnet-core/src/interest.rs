//! Interest management (area-of-interest).
//!
//! At the 100-player target, sending every entity to every peer is quadratic in both bandwidth and
//! encode cost, and almost all of it describes entities a peer is too far from to interact with.
//! The fix is classic AOI (see docs/architecture.md): each peer replicates only the
//! entities near it. The two failure modes AOI must avoid are *boundary flicker* — an entity
//! oscillating across the cut-off radius spawns and despawns on the wire every few ticks — and
//! *send spikes* — every entity refreshing its full state on the same tick.
//!
//! [`InterestGrid`] is a uniform grid over the **XZ plane**, rebuilt from scratch each net tick
//! from the position column already in native memory. Rebuilding beats incremental maintenance at
//! this scale: a few hundred inserts into pooled buckets is cheaper and simpler than tracking cell
//! transitions, and it can never drift out of sync with the simulation. Y is ignored for cell
//! assignment because cislunar arenas are broad, not tall — stations, forts and the l_system
//! cluster spread horizontally, so vertical binning would only fragment cells — but queries still
//! measure true 3D euclidean distance, so a body far overhead is correctly out of range.
//!
//! [`PeerInterest`] layers hysteresis on top: an entity *enters* a peer's set inside
//! `enter_radius` but only *leaves* past `enter_radius * exit_factor`. Inside the band between the
//! two radii, current members stay and newcomers are refused, so a body drifting along the
//! boundary changes membership once, not every tick. [`send_phase`] handles the spike half:
//! full-state refreshes are phase-offset by entity id so each tick of an interval carries its own
//! slice of the refresh traffic instead of tick zero carrying all of it.
//!
//! ## Grid or scan
//!
//! [`PeerInterest`] has two update paths and the backend ships the *linear* one. A uniform grid can
//! only beat a flat scan when the query radius covers a small fraction of the occupied space;
//! otherwise [`InterestGrid::query_within`]'s own guard finds the scan rectangle larger than the
//! occupancy and iterates every bucket — which **is** the linear scan, plus a rebuild. Measured
//! (`tests/interest_bench.rs`, 240 ticks, radius 256 m, release):
//!
//! | arena extent | mean set | scan µs/tick | grid µs/tick |
//! |--------------|----------|--------------|--------------|
//! | ±300 m       | 335      | 519          | 1628         |
//! | ±600 m       | 101      | 311          | 483          |
//! | ±1200 m      | 26       | 190          | 113          |
//! | ±5000 m      | 1        | 85           | 85           |
//!
//! The grid wins only in a band no shipped arena occupies (2fort's forts sit at ±74 m, the
//! container cube is 60 m on a side), and by ~80 µs/tick when it does. So [`PeerInterest::update`]
//! and the grid are retained, tested and unchanged — they are the right answer if arenas ever grow
//! an order of magnitude — while [`PeerInterest::update_linear_into`] is what `orbit_net.rs` calls.
//! `net.perf`'s `interest_ms` is the live number that would justify revisiting.

use std::collections::{BTreeMap, HashMap};

use crate::history::BodyId;

/// One grid cell's occupants: `(id, position)` pairs, position retained for the distance check.
type CellBucket = Vec<(BodyId, [f32; 3])>;

/// Tuning for the AOI grid and the per-peer hysteresis band.
///
/// Degenerate values are corrected at **use** time, not construction, so a config decoded from a
/// console cvar or the wire can be stored as-is: a non-finite or non-positive `cell_size` behaves
/// as the default `32.0`, and an `exit_factor` below `1.0` (including `NaN`) behaves as `1.0`
/// (no hysteresis band).
#[derive(Debug, Clone, Copy)]
pub struct AoiConfig {
    /// Edge length of one grid cell in metres. Values that are non-finite or `<= 0` behave as the
    /// default `32.0`.
    pub cell_size: f32,
    /// Radius in metres at which an entity enters a peer's interest set.
    ///
    /// The default of `256.0` covers the demo arena with margin: the 2fort CTF forts sit at
    /// ±74 m, so a peer in one fort holds interest over the bridged midfield *and* the far fort.
    pub enter_radius: f32,
    /// An entity leaves the set only past `enter_radius * exit_factor`. Values below `1.0`
    /// (including `NaN`) behave as `1.0`, which collapses the hysteresis band.
    pub exit_factor: f32,
    /// Hard cap on a peer's set size; the nearest N win. `0` means uncapped.
    pub max_entities: usize,
}

impl Default for AoiConfig {
    fn default() -> Self {
        Self {
            cell_size: 32.0,
            enter_radius: 256.0,
            exit_factor: 1.25,
            max_entities: 0,
        }
    }
}

impl AoiConfig {
    /// The cell size actually used, with degenerate values replaced by the default.
    fn effective_cell_size(&self) -> f32 {
        if self.cell_size.is_finite() && self.cell_size > 0.0 {
            self.cell_size
        } else {
            32.0
        }
    }

    /// The exit factor actually used, floored at `1.0` (a band narrower than the enter radius
    /// would make entities leave the set while still eligible to re-enter it — flicker by
    /// construction).
    fn effective_exit_factor(&self) -> f32 {
        if self.exit_factor.is_finite() && self.exit_factor >= 1.0 {
            self.exit_factor
        } else {
            1.0
        }
    }
}

/// The grid cell a coordinate falls in along one axis.
///
/// The `as` cast saturates, so an absurd but finite coordinate lands in the outermost cell
/// instead of invoking undefined behaviour.
fn cell_coord(value: f32, cell_size: f32) -> i32 {
    (value / cell_size).floor() as i32
}

/// A uniform spatial grid over the XZ plane, rebuilt from entity positions each net tick.
///
/// Y is deliberately **not** part of the cell key (see the module header); it still participates
/// in every distance computed by [`InterestGrid::query_within`]. Bucket `Vec`s are pooled across
/// rebuilds, so after the first few ticks a rebuild allocates nothing.
///
/// Rebuild and query must use the same [`AoiConfig`] (or at least the same effective cell size) —
/// the query derives its cell scan from the size the entities were binned under.
#[derive(Debug, Clone, Default)]
pub struct InterestGrid {
    cells: HashMap<(i32, i32), CellBucket>,
    pool: Vec<CellBucket>,
}

impl InterestGrid {
    /// An empty grid.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the grid's contents with `entities`, binning by XZ cell.
    ///
    /// Entities with any non-finite position component are **skipped**, not clamped: a `NaN`
    /// cannot be meaningfully binned, and clamping would teleport the body into some arbitrary
    /// boundary cell where it would match queries it should not. A skipped entity simply falls
    /// out of every peer's set, which is the same thing that happens when it despawns.
    pub fn rebuild(&mut self, cfg: &AoiConfig, entities: &[(BodyId, [f32; 3])]) {
        let cell_size = cfg.effective_cell_size();
        let pool = &mut self.pool;
        let cells = &mut self.cells;
        for (_, mut bucket) in cells.drain() {
            bucket.clear();
            pool.push(bucket);
        }
        for &(id, pos) in entities {
            if !(pos[0].is_finite() && pos[1].is_finite() && pos[2].is_finite()) {
                continue;
            }
            let key = (cell_coord(pos[0], cell_size), cell_coord(pos[2], cell_size));
            cells
                .entry(key)
                .or_insert_with(|| pool.pop().unwrap_or_default())
                .push((id, pos));
        }
    }

    /// Append every entity within true 3D euclidean distance `<= radius` of `center` to `out` as
    /// `(id, distance_squared)`, scanning only the grid cells the radius overlaps.
    ///
    /// `out` is cleared first. A non-finite `radius` or `center`, or a negative `radius`, yields
    /// an empty result. When the scan rectangle would cover more cells than are actually occupied
    /// (an enormous radius), the occupied cells are scanned instead, so the cost is bounded by the
    /// entity count either way. Append **order is unspecified** — sort by id if a deterministic
    /// order matters downstream ([`PeerInterest`] does not depend on it).
    pub fn query_within(
        &self,
        cfg: &AoiConfig,
        center: [f32; 3],
        radius: f32,
        out: &mut Vec<(BodyId, f32)>,
    ) {
        out.clear();
        if !radius.is_finite()
            || radius < 0.0
            || !(center[0].is_finite() && center[1].is_finite() && center[2].is_finite())
        {
            return;
        }
        let cell_size = cfg.effective_cell_size();
        let radius_sq = radius * radius;
        let min_x = cell_coord(center[0] - radius, cell_size);
        let max_x = cell_coord(center[0] + radius, cell_size);
        let min_z = cell_coord(center[2] - radius, cell_size);
        let max_z = cell_coord(center[2] + radius, cell_size);
        // Widened arithmetic: the rectangle can span the whole i32 range, and 2^32 * 2^32
        // overflows u64 exactly at the corner case.
        let span_x = (i64::from(max_x) - i64::from(min_x) + 1) as u128;
        let span_z = (i64::from(max_z) - i64::from(min_z) + 1) as u128;
        if span_x * span_z > self.cells.len() as u128 {
            for bucket in self.cells.values() {
                Self::append_within(bucket, center, radius_sq, out);
            }
        } else {
            for cx in min_x..=max_x {
                for cz in min_z..=max_z {
                    if let Some(bucket) = self.cells.get(&(cx, cz)) {
                        Self::append_within(bucket, center, radius_sq, out);
                    }
                }
            }
        }
    }

    /// Distance-check one bucket's occupants against `center`, appending the hits.
    fn append_within(
        bucket: &[(BodyId, [f32; 3])],
        center: [f32; 3],
        radius_sq: f32,
        out: &mut Vec<(BodyId, f32)>,
    ) {
        for &(id, pos) in bucket {
            let dx = pos[0] - center[0];
            let dy = pos[1] - center[1];
            let dz = pos[2] - center[2];
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if dist_sq <= radius_sq {
                out.push((id, dist_sq));
            }
        }
    }
}

/// One entity offered to a peer's interest filter for a tick.
///
/// `always` is the fail-open flag and carries three separate facts that all mean "never cull this":
/// the peer's own body, an entity whose synchronizer declares no anchor at all, and an entity whose
/// anchor could not be resolved this tick. Keeping them one flag is deliberate — the filter has no
/// business distinguishing them, and every one of them must survive the cap as well as the radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InterestCandidate {
    /// The entity id.
    pub id: BodyId,
    /// World-space anchor for the distance test. Ignored entirely when `always` is set.
    pub pos: [f32; 3],
    /// Unconditionally relevant: never culled by radius, never evicted by `max_entities`.
    pub always: bool,
}

impl InterestCandidate {
    /// A candidate culled by distance from `pos`.
    #[must_use]
    pub fn anchored(id: BodyId, pos: [f32; 3]) -> Self {
        Self {
            id,
            pos,
            always: false,
        }
    }

    /// A candidate that is always in interest.
    #[must_use]
    pub fn always(id: BodyId) -> Self {
        Self {
            id,
            pos: [0.0; 3],
            always: true,
        }
    }
}

/// One peer's hysteretic interest set.
///
/// Members are stored in a [`BTreeMap`] keyed by id (value: the distance squared observed on the
/// last update), so [`PeerInterest::iter`] walks in ascending id order for free — the wire order
/// must not vary run to run.
#[derive(Debug, Clone, Default)]
pub struct PeerInterest {
    members: BTreeMap<BodyId, f32>,
}

impl PeerInterest {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recompute the set from the grid for a peer observing from `center`.
    ///
    /// Entities within `enter_radius` join; current members are retained until they exceed
    /// `enter_radius * exit_factor`; members not found within the exit radius (moved away,
    /// despawned, or position went non-finite) are removed — nothing leaks. When
    /// `cfg.max_entities > 0`, only the nearest N survive, ordered by distance, then **current
    /// members before newcomers** on a distance tie (so the set stays stable when a newcomer
    /// merely matches a member's range), then ascending id (so the result is deterministic
    /// regardless of grid iteration order). An entity evicted by the cap is a real leave: it must
    /// re-enter through `enter_radius` like any newcomer.
    ///
    /// `scratch` is caller-provided working storage so a per-peer, per-tick update allocates
    /// nothing once warm; its contents on return are unspecified. A non-finite `enter_radius` or
    /// `center` empties the set (the query it depends on returns nothing).
    ///
    /// **NOT THE SHIPPED PATH, AND IT CANNOT BE ADOPTED AS IT STANDS.** The binding calls
    /// [`Self::update_linear_into`], which reports the entities that LEFT the set — and the send path needs
    /// that list, not as a nicety but for correctness: a leave has to clear `last_sent` and `acked_base` for
    /// that entity, or a re-entering body is answered with a delta against a base its peer dropped, which
    /// raises the per-peer all-entity `WANT_FULL` flag. This grid form returns nothing. Whoever adopts it for
    /// the entity counts a grid finally pays for has to give it the same leave diff first; swapping the call
    /// site alone would trade a linear scan for a full-state storm.
    pub fn update(
        &mut self,
        grid: &InterestGrid,
        cfg: &AoiConfig,
        center: [f32; 3],
        scratch: &mut Vec<(BodyId, f32)>,
    ) {
        let enter_radius = cfg.enter_radius;
        // `.min(f32::MAX)` keeps an overflowing product finite so the query still runs.
        let exit_radius = (enter_radius * cfg.effective_exit_factor()).min(f32::MAX);
        grid.query_within(cfg, center, exit_radius, scratch);

        // One query at the exit radius serves both bands: everything returned is inside the exit
        // radius (members retained), and the enter check below filters the newcomers.
        let enter_sq = enter_radius * enter_radius;
        let members = &self.members;
        scratch.retain(|&(id, dist_sq)| members.contains_key(&id) || dist_sq <= enter_sq);

        if cfg.max_entities > 0 && scratch.len() > cfg.max_entities {
            scratch.sort_by(|a, b| {
                a.1.total_cmp(&b.1)
                    .then_with(|| members.contains_key(&b.0).cmp(&members.contains_key(&a.0)))
                    .then_with(|| a.0.cmp(&b.0))
            });
            scratch.truncate(cfg.max_entities);
        }

        self.members.clear();
        for &(id, dist_sq) in scratch.iter() {
            self.members.insert(id, dist_sq);
        }
    }

    /// Recompute the set from a flat candidate slice, reporting every id that **left**.
    ///
    /// This is the path `orbit_net.rs` runs (see the module header for why the grid is not). The
    /// hysteresis, cap and tie-breaking rules are identical to [`Self::update`]; what differs is
    /// only how candidates are found, plus two things [`Self::update`] cannot express:
    ///
    /// * [`InterestCandidate::always`] entities bypass both the radius and the cap. The cap bounds
    ///   the *cullable* set; an unconditionally-relevant entity is never evicted by it.
    /// * `leaves` receives every id that was a member and is not one now — radius exits **and**
    ///   cap evictions alike, because a cap eviction is a real leave that must re-enter through
    ///   `enter_radius` like any newcomer. The caller uses this to clear its per-peer delta
    ///   bookkeeping, so a re-entrant entity gets a full block rather than a delta against a base
    ///   the peer stopped tracking.
    ///
    /// A candidate whose position (or `center`) is non-finite is treated as `always` rather than
    /// dropped: an unbinnable body is a body the filter cannot reason about, and failing open
    /// wastes bandwidth where failing closed would silently delete it from someone's world.
    ///
    /// `leaves` and `scratch` are both cleared on entry; `scratch` is caller-owned working storage
    /// so a warm per-peer update allocates nothing.
    pub fn update_linear_into(
        &mut self,
        cfg: &AoiConfig,
        center: [f32; 3],
        candidates: &[InterestCandidate],
        scratch: &mut Vec<(BodyId, f32)>,
        leaves: &mut Vec<BodyId>,
    ) {
        scratch.clear();
        leaves.clear();
        let center_ok = center[0].is_finite() && center[1].is_finite() && center[2].is_finite();
        let enter_radius = cfg.enter_radius;
        let enter_sq = enter_radius * enter_radius;
        let exit_radius = (enter_radius * cfg.effective_exit_factor()).min(f32::MAX);
        let exit_sq = exit_radius * exit_radius;

        let mut cullable = 0usize;
        for candidate in candidates {
            let pos = candidate.pos;
            let binnable =
                center_ok && pos[0].is_finite() && pos[1].is_finite() && pos[2].is_finite();
            if candidate.always || !binnable {
                // Sorted below `0.0` is impossible, so an always-entity can never be reordered
                // ahead of a genuinely closer one by the cap sort — it is excluded from it.
                scratch.push((candidate.id, f32::NEG_INFINITY));
                continue;
            }
            let dx = pos[0] - center[0];
            let dy = pos[1] - center[1];
            let dz = pos[2] - center[2];
            let dist_sq = dx * dx + dy * dy + dz * dz;
            let member = self.members.contains_key(&candidate.id);
            let keep = if member {
                dist_sq <= exit_sq
            } else {
                dist_sq <= enter_sq
            };
            if keep {
                scratch.push((candidate.id, dist_sq));
                cullable += 1;
            }
        }

        if cfg.max_entities > 0 && cullable > cfg.max_entities {
            let members = &self.members;
            // `NEG_INFINITY` sorts the always-entities to the front, so truncating to
            // `max_entities + always_count` keeps every one of them plus the nearest N cullable.
            scratch.sort_by(|a, b| {
                a.1.total_cmp(&b.1)
                    .then_with(|| members.contains_key(&b.0).cmp(&members.contains_key(&a.0)))
                    .then_with(|| a.0.cmp(&b.0))
            });
            let always_count = scratch.len() - cullable;
            scratch.truncate(always_count + cfg.max_entities);
        }

        // Diff before overwrite: everything the old set held that the new one does not is a leave.
        // `self.members` is already ascending by id, so sorting `scratch` the same way turns the
        // diff into one linear merge — a nested scan here would be O(K^2) per peer per tick, which
        // at the interest-set sizes this exists to serve is worse than the filter it replaced.
        scratch.sort_unstable_by_key(|&(id, _)| id);
        let mut fresh = scratch.iter().peekable();
        for &id in self.members.keys() {
            while fresh.peek().is_some_and(|&&(candidate, _)| candidate < id) {
                fresh.next();
            }
            if fresh.peek().is_none_or(|&&(candidate, _)| candidate != id) {
                leaves.push(id);
            }
        }
        self.members.clear();
        for &(id, dist_sq) in scratch.iter() {
            self.members
                .insert(id, if dist_sq.is_finite() { dist_sq } else { 0.0 });
        }
    }

    /// Whether `id` is currently in the set.
    #[must_use]
    pub fn contains(&self, id: BodyId) -> bool {
        self.members.contains_key(&id)
    }

    /// The distance squared recorded for `id` at the last update (`0.0` for always-relevant
    /// entities, which have no meaningful distance).
    #[must_use]
    pub fn dist_sq(&self, id: BodyId) -> Option<f32> {
        self.members.get(&id).copied()
    }

    /// The members with their recorded distance squared, in ascending id order.
    pub fn iter_with_distance(&self) -> impl Iterator<Item = (BodyId, f32)> + '_ {
        self.members.iter().map(|(&id, &dist_sq)| (id, dist_sq))
    }

    /// The member ids in ascending order — the deterministic wire order.
    pub fn iter(&self) -> impl Iterator<Item = BodyId> + '_ {
        self.members.keys().copied()
    }

    /// How many entities the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the set holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Drop `id` from the set immediately — for despawns, which must not wait for the next
    /// update to fall out of the grid.
    pub fn remove(&mut self, id: BodyId) {
        self.members.remove(&id);
    }
}

/// Whether `tick` is `id`'s phase slot within `interval`.
///
/// Full-state refreshes gated on this spread across the interval by entity id instead of all
/// firing on the same tick: over any `interval` consecutive ticks, each id fires exactly once.
/// An `interval` of `0` or `1` means every tick (the modulo is guarded — `interval` may come off
/// the wire or a console cvar, and a zero must not panic the process).
#[must_use]
pub fn send_phase(id: BodyId, tick: u64, interval: u64) -> bool {
    if interval <= 1 {
        return true;
    }
    tick % interval == id % interval
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cell_size: f32, enter_radius: f32, exit_factor: f32, max_entities: usize) -> AoiConfig {
        AoiConfig {
            cell_size,
            enter_radius,
            exit_factor,
            max_entities,
        }
    }

    /// The same deterministic LCG the codec's hostile-input sweep uses — reproducible, no crates.
    fn lcg(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *state
    }

    /// A coordinate in `[-200, 200)` from the LCG's high bits.
    fn lcg_coord(state: &mut u32) -> f32 {
        (lcg(state) >> 8) as f32 / 16_777_216.0 * 400.0 - 200.0
    }

    /// Reference implementation: scan everything, keep what is inside, sort by id.
    fn brute_force(
        entities: &[(BodyId, [f32; 3])],
        center: [f32; 3],
        radius: f32,
    ) -> Vec<(BodyId, f32)> {
        let radius_sq = radius * radius;
        let mut hits: Vec<(BodyId, f32)> = Vec::new();
        for &(id, pos) in entities {
            if !(pos[0].is_finite() && pos[1].is_finite() && pos[2].is_finite()) {
                continue;
            }
            let dx = pos[0] - center[0];
            let dy = pos[1] - center[1];
            let dz = pos[2] - center[2];
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if dist_sq <= radius_sq {
                hits.push((id, dist_sq));
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits
    }

    fn sorted_by_id(mut hits: Vec<(BodyId, f32)>) -> Vec<(BodyId, f32)> {
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits
    }

    #[test]
    fn default_config_covers_the_demo_arena_with_margin() {
        let cfg = AoiConfig::default();
        assert_eq!(cfg.cell_size, 32.0);
        assert_eq!(cfg.enter_radius, 256.0);
        assert_eq!(cfg.exit_factor, 1.25);
        assert_eq!(cfg.max_entities, 0);
        // The 2fort forts sit at ±74 m; a peer in one must hold interest over the other.
        assert!(cfg.enter_radius > 2.0 * 74.0);
    }

    #[test]
    fn grid_query_matches_brute_force_on_a_pseudo_random_layout() {
        let mut state = 0x1234_5678u32;
        let mut entities: Vec<(BodyId, [f32; 3])> = Vec::new();
        for id in 0..256u64 {
            let pos = [
                lcg_coord(&mut state),
                lcg_coord(&mut state),
                lcg_coord(&mut state),
            ];
            entities.push((id, pos));
        }
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &entities);

        let centers = [
            [0.0, 0.0, 0.0],
            [150.0, -40.0, -150.0],
            [-31.9, 5.0, 32.1], // straddles a cell boundary
        ];
        let radii = [0.0, 12.5, 64.0, 250.0, 1000.0];
        let mut out: Vec<(BodyId, f32)> = Vec::new();
        for center in centers {
            for radius in radii {
                grid.query_within(&cfg, center, radius, &mut out);
                assert_eq!(
                    sorted_by_id(out.clone()),
                    brute_force(&entities, center, radius),
                    "mismatch at center {center:?} radius {radius}"
                );
            }
        }
    }

    #[test]
    fn empty_grid_returns_nothing() {
        let cfg = AoiConfig::default();
        let grid = InterestGrid::new();
        let mut out = vec![(1, 0.0)]; // must be cleared even when nothing matches
        grid.query_within(&cfg, [0.0; 3], 100.0, &mut out);
        assert!(out.is_empty());

        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(peer.is_empty());
        assert_eq!(peer.len(), 0);
        assert_eq!(peer.iter().count(), 0);
    }

    #[test]
    fn y_offset_counts_for_distance_but_not_for_cell_assignment() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        // Both share the origin's XZ cell; one hangs 300 m overhead.
        grid.rebuild(&cfg, &[(1, [0.0, 300.0, 0.0]), (2, [0.0, 10.0, 0.0])]);
        let mut out = Vec::new();
        grid.query_within(&cfg, [0.0; 3], 50.0, &mut out);
        assert_eq!(sorted_by_id(out.clone()), vec![(2, 100.0)]);
        // A radius covering the true 3D distance finds the overhead body too, which also proves
        // it was binned by XZ alone — the scan rectangle only covers cells around the origin.
        grid.query_within(&cfg, [0.0; 3], 301.0, &mut out);
        assert_eq!(sorted_by_id(out.clone()).len(), 2);
    }

    #[test]
    fn non_finite_positions_are_skipped_on_rebuild() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &[
                (1, [f32::NAN, 0.0, 0.0]),
                (2, [0.0, f32::INFINITY, 0.0]),
                (3, [0.0, 0.0, f32::NEG_INFINITY]),
                (4, [1.0, 2.0, 3.0]),
            ],
        );
        let mut out = Vec::new();
        grid.query_within(&cfg, [0.0; 3], f32::MAX.sqrt(), &mut out);
        let ids: Vec<BodyId> = sorted_by_id(out).iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, vec![4]);
    }

    #[test]
    fn degenerate_cell_sizes_fall_back_to_the_default() {
        let entities: Vec<(BodyId, [f32; 3])> = vec![
            (1, [5.0, 0.0, 5.0]),
            (2, [-40.0, 0.0, 33.0]),
            (3, [90.0, 12.0, -90.0]),
        ];
        let mut out = Vec::new();
        for bad in [0.0, -3.0, f32::NAN, f32::INFINITY] {
            let cfg = cfg(bad, 256.0, 1.25, 0);
            let mut grid = InterestGrid::new();
            grid.rebuild(&cfg, &entities);
            grid.query_within(&cfg, [0.0; 3], 200.0, &mut out);
            assert_eq!(
                sorted_by_id(out.clone()),
                brute_force(&entities, [0.0; 3], 200.0),
                "cell_size {bad} misbehaved"
            );
        }
    }

    #[test]
    fn query_rejects_non_finite_centers_and_radii() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &[(1, [0.0, 0.0, 0.0])]);
        let mut out = Vec::new();
        for (center, radius) in [
            ([f32::NAN, 0.0, 0.0], 100.0),
            ([0.0, f32::INFINITY, 0.0], 100.0),
            ([0.0; 3], f32::NAN),
            ([0.0; 3], f32::INFINITY),
            ([0.0; 3], -1.0),
        ] {
            grid.query_within(&cfg, center, radius, &mut out);
            assert!(out.is_empty(), "center {center:?} radius {radius} matched");
        }
    }

    #[test]
    fn rebuild_replaces_previous_contents() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &[(1, [0.0; 3]), (2, [10.0, 0.0, 0.0])]);
        grid.rebuild(&cfg, &[(3, [0.0; 3])]);
        let mut out = Vec::new();
        grid.query_within(&cfg, [0.0; 3], 500.0, &mut out);
        assert_eq!(sorted_by_id(out), vec![(3, 0.0)]);
    }

    #[test]
    fn enormous_radius_scans_occupied_cells_not_the_rectangle() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &[(1, [0.0; 3]), (2, [5000.0, 0.0, -5000.0])]);
        let mut out = Vec::new();
        // Finite but so large the XZ scan rectangle would span the whole i32 cell range; the
        // occupied-cell fallback must return everything without iterating billions of cells.
        grid.query_within(&cfg, [0.0; 3], 1.0e30, &mut out);
        let ids: Vec<BodyId> = sorted_by_id(out).iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn hysteresis_enters_below_enter_radius_and_holds_through_the_band() {
        let cfg = cfg(32.0, 100.0, 1.25, 0); // exit at 125
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();

        grid.rebuild(&cfg, &[(1, [90.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(peer.contains(1), "90 m is inside the enter radius");

        for held in [110.0f32, 124.0] {
            grid.rebuild(&cfg, &[(1, [held, 0.0, 0.0])]);
            peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
            assert!(peer.contains(1), "{held} m is inside the hysteresis band");
        }

        grid.rebuild(&cfg, &[(1, [126.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(!peer.contains(1), "126 m is past the exit radius");
    }

    #[test]
    fn a_target_at_the_longest_shot_in_the_game_stays_in_interest() {
        // THE PLAYER-FACING GUARANTEE BEHIND THE SHIPPED RADIUS, stated as behaviour rather
        // than as a number. `aoi_weapon_range_test.gd` checks that the shipped radius is >= the
        // longest projectile range; this checks that a body at that range is actually still in the
        // set, which is the thing a scoped sniper depends on.
        //
        // A culled entity is not despawned -- nothing carries a spatial visibility filter -- so it
        // keeps its node on the peer and freezes at the last pose that arrived. Cull inside a
        // weapon's range and the shooter is aiming at a stale ghost they cannot hit.
        let sniper_range = 2000.0f32;
        let cfg = cfg(32.0, sniper_range, 1.25, 0);
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();

        for shot in [1.0f32, 600.0, 700.0, 1999.0, sniper_range] {
            grid.rebuild(&cfg, &[(1, [shot, 0.0, 0.0])]);
            peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
            assert!(
                peer.contains(1),
                "a target {shot} m away is within the sniper's reach and must still be replicated"
            );
        }

        // And the radius is what bounds it: past the hysteresis band the body does leave, which is
        // the behaviour an arena larger than the sniper's reach is meant to get.
        grid.rebuild(&cfg, &[(1, [sniper_range * 1.25 + 1.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(!peer.contains(1), "past the exit radius it leaves");
    }

    #[test]
    fn hysteresis_band_does_not_admit_newcomers() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &[(1, [110.0, 0.0, 0.0])]);
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(
            !peer.contains(1),
            "110 m is in the band, and the band only retains — it never admits"
        );
        assert!(peer.is_empty());
    }

    #[test]
    fn members_past_the_exit_radius_are_dropped_without_leaking() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();
        grid.rebuild(&cfg, &[(1, [50.0, 0.0, 0.0]), (2, [60.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(peer.len(), 2);

        // Body 1 teleports far away, body 2 despawns from the grid entirely.
        grid.rebuild(&cfg, &[(1, [10_000.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(!peer.contains(1));
        assert!(!peer.contains(2));
        assert!(peer.is_empty(), "stale members must not accumulate");
    }

    #[test]
    fn exit_factor_below_one_collapses_the_band() {
        let cfg = cfg(32.0, 100.0, 0.5, 0); // effective factor 1.0: no band
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();
        grid.rebuild(&cfg, &[(1, [50.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(peer.contains(1));
        grid.rebuild(&cfg, &[(1, [101.0, 0.0, 0.0])]);
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert!(!peer.contains(1), "with no band, past enter means out");
    }

    #[test]
    fn max_entities_keeps_the_nearest_and_members_win_ties() {
        let cfg = cfg(32.0, 100.0, 1.25, 2);
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();

        grid.rebuild(
            &cfg,
            &[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [30.0, 0.0, 0.0]),
            ],
        );
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // Body 3 closes to an exact distance tie with member 2: the member wins, so the set does
        // not churn on a mere tie.
        grid.rebuild(
            &cfg,
            &[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [0.0, 0.0, 20.0]),
            ],
        );
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // Strictly closer beats membership: 3 displaces 2, even though 2 is still in range.
        grid.rebuild(
            &cfg,
            &[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [15.0, 0.0, 0.0]),
            ],
        );
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn max_entities_tie_between_newcomers_is_broken_by_id() {
        let cfg = cfg(32.0, 100.0, 1.25, 2);
        let mut grid = InterestGrid::new();
        // Fresh peer, so everyone is a newcomer; 2 and 3 tie at 20 m.
        grid.rebuild(
            &cfg,
            &[
                (3, [0.0, 0.0, 20.0]),
                (2, [20.0, 0.0, 0.0]),
                (1, [10.0, 0.0, 0.0]),
            ],
        );
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(
            peer.iter().collect::<Vec<_>>(),
            vec![1, 2],
            "the lower id must win a newcomer tie deterministically"
        );
    }

    #[test]
    fn remove_handles_despawns_and_iter_is_ascending() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        let entities: Vec<(BodyId, [f32; 3])> = vec![
            (42, [1.0, 0.0, 0.0]),
            (7, [2.0, 0.0, 0.0]),
            (99, [3.0, 0.0, 0.0]),
        ];
        grid.rebuild(&cfg, &entities);
        let mut peer = PeerInterest::new();
        let mut scratch = Vec::new();
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![7, 42, 99]);

        peer.remove(42);
        assert!(!peer.contains(42));
        assert_eq!(peer.len(), 2);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![7, 99]);

        // Still present in the grid and inside the enter radius, so it re-enters next update.
        peer.update(&grid, &cfg, [0.0; 3], &mut scratch);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![7, 42, 99]);
    }

    // ------------------------------------------------------------------
    // The linear path — the one the backend actually runs.
    // ------------------------------------------------------------------

    fn anchored(entities: &[(BodyId, [f32; 3])]) -> Vec<InterestCandidate> {
        entities
            .iter()
            .map(|&(id, pos)| InterestCandidate::anchored(id, pos))
            .collect()
    }

    #[test]
    fn linear_agrees_with_the_grid_over_a_pseudo_random_walk() {
        // The two paths are separate implementations of one rule; if they ever disagree, the
        // measurement that chose between them was comparing different work.
        let cfg = cfg(64.0, 100.0, 1.25, 0);
        let mut state = 0x0bad_f00du32;
        let mut entities: Vec<(BodyId, [f32; 3])> = (0..64u64)
            .map(|id| {
                (
                    id + 1,
                    [
                        lcg_coord(&mut state),
                        lcg_coord(&mut state),
                        lcg_coord(&mut state),
                    ],
                )
            })
            .collect();

        let mut grid = InterestGrid::new();
        let mut via_grid = PeerInterest::new();
        let mut via_linear = PeerInterest::new();
        let mut scratch = Vec::new();
        let mut leaves = Vec::new();

        for step in 0..40 {
            for entry in entities.iter_mut() {
                entry.1[0] += lcg_coord(&mut state) * 0.05;
                entry.1[2] += lcg_coord(&mut state) * 0.05;
            }
            let center = [
                lcg_coord(&mut state) * 0.2,
                0.0,
                lcg_coord(&mut state) * 0.2,
            ];
            grid.rebuild(&cfg, &entities);
            via_grid.update(&grid, &cfg, center, &mut scratch);
            via_linear.update_linear_into(
                &cfg,
                center,
                &anchored(&entities),
                &mut scratch,
                &mut leaves,
            );
            assert_eq!(
                via_grid.iter().collect::<Vec<_>>(),
                via_linear.iter().collect::<Vec<_>>(),
                "grid and linear diverged at step {step}"
            );
        }
    }

    #[test]
    fn linear_holds_through_the_band_and_reports_the_exit_as_a_leave() {
        let cfg = cfg(32.0, 100.0, 1.25, 0); // exit at 125
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());

        let step = |peer: &mut PeerInterest,
                    x: f32,
                    scratch: &mut Vec<(BodyId, f32)>,
                    leaves: &mut Vec<BodyId>| {
            peer.update_linear_into(
                &cfg,
                [0.0; 3],
                &[InterestCandidate::anchored(1, [x, 0.0, 0.0])],
                scratch,
                leaves,
            );
        };

        step(&mut peer, 90.0, &mut scratch, &mut leaves);
        assert!(peer.contains(1));
        assert!(leaves.is_empty());

        step(&mut peer, 124.0, &mut scratch, &mut leaves);
        assert!(peer.contains(1), "the band retains");
        assert!(leaves.is_empty(), "retention is not a leave");

        step(&mut peer, 126.0, &mut scratch, &mut leaves);
        assert!(!peer.contains(1));
        assert_eq!(leaves, vec![1], "past the exit radius is a leave");

        // Re-entry must clear the leave list, or a caller would re-run the leave bookkeeping.
        step(&mut peer, 50.0, &mut scratch, &mut leaves);
        assert!(peer.contains(1));
        assert!(leaves.is_empty(), "leaves is per-call, not cumulative");
    }

    #[test]
    fn linear_band_admits_nobody_new() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            &[InterestCandidate::anchored(1, [110.0, 0.0, 0.0])],
            &mut scratch,
            &mut leaves,
        );
        assert!(peer.is_empty(), "the band retains but never admits");
        assert!(leaves.is_empty());
    }

    #[test]
    fn linear_counts_a_cap_eviction_as_a_leave() {
        let cfg = cfg(32.0, 100.0, 1.25, 2);
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            &anchored(&[(1, [10.0, 0.0, 0.0]), (2, [20.0, 0.0, 0.0])]),
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // 3 arrives strictly closer than 2 and displaces it. That is a real leave: 2 must re-enter
        // through `enter_radius` like a newcomer, and the caller must drop its delta base.
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            &anchored(&[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [15.0, 0.0, 0.0]),
            ]),
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(
            leaves,
            vec![2],
            "a cap eviction is a leave, not a silent drop"
        );
    }

    #[test]
    fn linear_always_candidates_bypass_both_the_radius_and_the_cap() {
        let cfg = cfg(32.0, 100.0, 1.25, 1);
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            &[
                InterestCandidate::always(7),
                InterestCandidate::always(8),
                InterestCandidate::anchored(1, [10.0, 0.0, 0.0]),
                InterestCandidate::anchored(2, [20.0, 0.0, 0.0]),
                // Far outside the exit radius: the radius does not reach it either way.
                InterestCandidate::anchored(3, [9_000.0, 0.0, 0.0]),
            ],
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(
            peer.iter().collect::<Vec<_>>(),
            vec![1, 7, 8],
            "the cap of 1 bounds the CULLABLE set only; both always-entities survive it"
        );
        assert_eq!(
            peer.dist_sq(7),
            Some(0.0),
            "an always-entity has no distance"
        );
        assert!(leaves.is_empty());
    }

    #[test]
    fn linear_fails_open_on_non_finite_positions() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            &[
                InterestCandidate::anchored(1, [f32::NAN, 0.0, 0.0]),
                InterestCandidate::anchored(2, [0.0, f32::INFINITY, 0.0]),
                InterestCandidate::anchored(3, [5_000.0, 0.0, 0.0]),
            ],
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(
            peer.iter().collect::<Vec<_>>(),
            vec![1, 2],
            "an unbinnable body stays replicated; only a binnable, distant one is culled"
        );

        // A non-finite CENTRE (a peer whose own body has gone bad) must not blank the world.
        let mut wide = PeerInterest::new();
        wide.update_linear_into(
            &cfg,
            [f32::NAN, 0.0, 0.0],
            &anchored(&[(1, [0.0; 3]), (2, [9_000.0, 0.0, 0.0])]),
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(wide.iter().collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn linear_iteration_carries_distance_in_ascending_id_order() {
        let cfg = cfg(32.0, 200.0, 1.25, 0);
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            &anchored(&[(9, [3.0, 0.0, 4.0]), (4, [10.0, 0.0, 0.0])]),
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(
            peer.iter_with_distance().collect::<Vec<_>>(),
            vec![(4, 100.0), (9, 25.0)]
        );
    }

    #[test]
    fn send_phase_fires_each_id_exactly_once_per_interval() {
        let interval = 8u64;
        let base = 160u64;
        for id in [0u64, 1, 2, 3, 5, 7, 8, 9, 255, u64::MAX] {
            let fires = (base..base + interval)
                .filter(|&tick| send_phase(id, tick, interval))
                .count();
            assert_eq!(fires, 1, "id {id} fired {fires} times in one interval");
        }
        // Two ids one interval apart share a slot — the spread is by residue, deterministically.
        assert_eq!(
            send_phase(3, base + 3, interval),
            send_phase(11, base + 3, interval)
        );
    }

    #[test]
    fn send_phase_guards_degenerate_intervals_and_extreme_ticks() {
        // Interval 0 must not panic on the modulo; 0 and 1 both mean "every tick".
        for interval in [0u64, 1] {
            assert!(send_phase(0, 0, interval));
            assert!(send_phase(u64::MAX, u64::MAX, interval));
            assert!(send_phase(12_345, 67_890, interval));
        }
        // Extreme tick indices still fire exactly once per window.
        let interval = 7u64;
        let fires = (u64::MAX - (interval - 1)..=u64::MAX)
            .filter(|&tick| send_phase(u64::MAX, tick, interval))
            .count();
        assert_eq!(fires, 1);
    }
}
