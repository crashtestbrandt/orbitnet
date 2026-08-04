//! Interest management (area-of-interest).
//!
//! At the 100-player target, sending every entity to every peer is quadratic in both bandwidth and
//! encode cost, and almost all of it describes entities a peer is too far from to interact with.
//! The fix is classic AOI (see `docs/orbitnet-native.md` §5.3): each peer replicates only the
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

    /// Whether `id` is currently in the set.
    #[must_use]
    pub fn contains(&self, id: BodyId) -> bool {
        self.members.contains_key(&id)
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
