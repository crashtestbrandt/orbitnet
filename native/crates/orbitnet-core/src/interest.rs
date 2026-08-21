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
//! ## Membership
//!
//! Distance alone cannot separate **overlapping worlds**: several independent worlds inside one
//! session, each rebased near its own coordinate origin, put unrelated entities at the same
//! coordinates. Every candidate and every observer therefore carries a [`MembershipId`], and
//! [`membership_matches`] refuses a candidate whose membership differs from the observer's
//! whatever its distance says.
//!
//! [`MEMBERSHIP_GLOBAL`] (`0`) is the default on both sides and matches everything, so a game that
//! declares no memberships is filtered exactly as before. It is also the fail-open value: an
//! observer whose membership could not be resolved sees every world rather than none.
//!
//! Membership is a separate axis from the radius, which is what makes it usable by the channels
//! that need it most. A state channel that replicates no position — health, inventory, a door's
//! state — has no distance to be culled by, so its only previous setting was all-or-nothing: it
//! reached every peer in every world. Declaring it always-relevant *within one membership* bounds
//! it to its own world while leaving it uncullable inside it.
//!
//! ## Grid or scan
//!
//! [`PeerInterest`] has two update paths and the backend ships the *linear* one.
//! [`PeerInterest::update_grid_into`] and [`PeerInterest::update_linear_into`] apply the same rules
//! to the same [`InterestCandidate`]s and report the same leaves — the suite asserts both over a
//! randomised walk — so the choice between them is a cost decision, and it is the measurements
//! below rather than an argument.
//!
//! Each row times three variants over one session (`tests/interest_bench.rs`, 240 ticks, radius
//! 256 m, release, one unowned row in eight positionless):
//!
//! * **scan/peer** — what `orbit_net.rs` runs. A peer's own body is `always` to that peer alone, so
//!   the candidate list is rebuilt inside the per-peer loop: O(P·N) per tick on top of the filter.
//! * **scan/shared** — the same filter over one list per tick, that single row patched in and out
//!   around each call. Needs no grid.
//! * **grid** — one [`InterestGrid::rebuild`] per tick, the own body passed as the `also` override.
//!
//! **Arena extent** (64 peers, 800 entities). A uniform grid can only win when the query radius
//! covers a small fraction of the occupied space; otherwise [`InterestGrid::query_within`]'s own
//! guard finds the scan rectangle larger than the occupancy and iterates every bucket — which
//! **is** the linear scan, plus a rebuild.
//!
//! | arena extent | mean set | scan/peer | scan/shared | grid  |
//! |--------------|----------|-----------|-------------|-------|
//! | ±300 m       | 391      | 935       | 899         | 1129  |
//! | ±600 m       | 182      | 524       | 473         | 454   |
//! | ±1200 m      | 115      | 412       | 367         | 219   |
//! | ±2500 m      | 96       | 364       | 315         | 174   |
//! | ±5000 m      | 93       | 362       | 313         | 165   |
//! | ±25000 m     | 93       | 375       | 326         | 162   |
//!
//! The crossover is between ±300 m and ±600 m, and past ±1200 m the grid is about twice as fast as
//! either scan. No shipped arena is out there: 2fort's forts sit at ±74 m and the container cube is
//! 60 m on a side, which is the ±300 m row, and that row is the one the grid loses.
//!
//! **World count** (64 peers, 1200 entities total, each world rebased on its own origin at ±300 m).
//! Several worlds in one session was the case the grid was expected to win — a peer is entitled to
//! a shrinking share of a session that keeps its size.
//!
//! | worlds | mean set | scan/peer | scan/shared | grid |
//! |--------|----------|-----------|-------------|------|
//! | 1      | 587      | 1926      | 1739        | 1831 |
//! | 2      | 295      | 718       | 632         | 854  |
//! | 4      | 146      | 353       | 277         | 434  |
//! | 8      | 72       | 210       | 134         | 238  |
//! | 16     | 36       | 151       | 76          | 135  |
//! | 32     | 18       | 125       | 52          | 87   |
//!
//! **The two scan columns disagree, and the disagreement is the finding.** From 16 worlds up the
//! grid beats what ships — but it beats it by deleting the per-peer candidate rebuild, not by
//! indexing space, and `scan/shared` deletes the same rebuild without a grid and is then 1.7× ahead
//! of the grid. The multi-world win is a rebuild win wearing a grid's clothes. Refusing another
//! world costs a scan one integer comparison per candidate, which is already less than binning that
//! candidate costs the grid.
//!
//! So [`PeerInterest::update_linear_into`] is what `orbit_net.rs` calls, and the grid is retained
//! and tested rather than adopted. What changed is that it is now *adoptable*: it reports the leaves
//! the send path clears its delta bookkeeping from, it carries the always-set and the worlds, and
//! the only thing an adopting caller adds is one [`InterestGrid::rebuild`] per tick before the
//! per-peer loop. **An arena past about ±600 m of occupancy is what would justify making that
//! switch**, and `net.perf`'s `interest_ms` is the live number to watch. A high world count is not:
//! the cheaper answer there is to stop rebuilding the candidate list per peer.

use std::collections::{BTreeMap, HashMap};

use crate::history::BodyId;

/// One grid cell's occupants: `(id, position)` pairs, position retained for the distance check.
type CellBucket = Vec<(BodyId, [f32; 3])>;

/// One world's XZ cells. Each [`MembershipId`] gets its own map — see [`InterestGrid`].
type WorldCells = HashMap<(i32, i32), CellBucket>;

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
    /// Radius in metres at which an entity enters a peer's interest set. A negative radius
    /// behaves as `0.0`.
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

    /// The enter radius actually used, with a negative one floored at `0.0`.
    ///
    /// A negative radius reads as "cull everything", and it is the only reading available: the
    /// squared distance a filter compares against cannot carry the sign, so the raw value would
    /// admit everything within its magnitude instead. `NaN` and infinity pass through — both are
    /// meaningful to the comparisons downstream (nothing enters, and everything does).
    fn effective_enter_radius(&self) -> f32 {
        if self.enter_radius < 0.0 {
            0.0
        } else {
            self.enter_radius
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

/// A uniform spatial grid over the XZ plane, rebuilt from the tick's candidates.
///
/// Y is deliberately **not** part of the cell key (see the module header); it still participates
/// in every distance computed by [`InterestGrid::query_within`]. Bucket `Vec`s are pooled across
/// rebuilds, so after the first few ticks a rebuild allocates nothing.
///
/// Rebuild and query must use the same [`AoiConfig`] (or at least the same effective cell size) —
/// the query derives its cell scan from the size the entities were binned under.
///
/// **Each world is binned separately.** [`MembershipId`] keys the outer map rather than forming a
/// third component of the cell key, because [`InterestGrid::query_within`]'s occupancy guard
/// compares a scan rectangle against a cell count. Folding the worlds together would compare it
/// against every world's cells at once, and a query that reads one world would take the
/// scan-everything branch on the strength of occupancy it can never see.
///
/// **The candidates that bypass the distance test are held beside the cells**, not in them:
/// [`InterestCandidate::always`] entities, and any whose position has a non-finite component. They
/// have no cell to be found in — an `always` candidate carries no position at all, and a `NaN`
/// cannot be binned — and a query must return them whatever its radius, so they are a flat list
/// read through [`InterestGrid::uncullable_for`]. Failing open on an unbinnable position rather
/// than dropping it is the rule the linear path applies, and the reason is recorded on
/// [`PeerInterest::update_linear_into`].
#[derive(Debug, Clone, Default)]
pub struct InterestGrid {
    worlds: HashMap<MembershipId, WorldCells>,
    uncullable: Vec<(BodyId, MembershipId)>,
    cell_pool: Vec<CellBucket>,
    world_pool: Vec<WorldCells>,
}

impl InterestGrid {
    /// An empty grid.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the grid's contents with `candidates`, binning each by world and XZ cell.
    ///
    /// A candidate that is [`InterestCandidate::always`], or whose position has any non-finite
    /// component, is **not binned**: it joins the uncullable list and reaches every observer its
    /// membership admits, at any distance. Clamping a non-finite position instead would teleport
    /// the body into some arbitrary boundary cell where it would match queries it should not.
    pub fn rebuild(&mut self, cfg: &AoiConfig, candidates: &[InterestCandidate]) {
        let cell_size = cfg.effective_cell_size();
        let cell_pool = &mut self.cell_pool;
        let world_pool = &mut self.world_pool;
        let worlds = &mut self.worlds;
        for (_, mut world) in worlds.drain() {
            for (_, mut bucket) in world.drain() {
                bucket.clear();
                cell_pool.push(bucket);
            }
            world_pool.push(world);
        }
        self.uncullable.clear();
        for candidate in candidates {
            let pos = candidate.pos;
            if candidate.always || !(pos[0].is_finite() && pos[1].is_finite() && pos[2].is_finite())
            {
                self.uncullable.push((candidate.id, candidate.membership));
                continue;
            }
            let key = (cell_coord(pos[0], cell_size), cell_coord(pos[2], cell_size));
            worlds
                .entry(candidate.membership)
                .or_insert_with(|| world_pool.pop().unwrap_or_default())
                .entry(key)
                .or_insert_with(|| cell_pool.pop().unwrap_or_default())
                .push((candidate.id, pos));
        }
    }

    /// Append every entity `observer` may see within true 3D euclidean distance `<= radius` of
    /// `center` to `out` as `(id, distance_squared)`, scanning only the cells the radius overlaps
    /// in the worlds [`membership_matches`] admits.
    ///
    /// `out` is cleared first. A non-finite `radius` or `center`, or a negative `radius`, yields
    /// an empty result. When the scan rectangle would cover more cells than a world actually
    /// occupies (an enormous radius), that world's occupied cells are scanned instead, so the cost
    /// is bounded by the entity count either way. Append **order is unspecified** — sort by id if a
    /// deterministic order matters downstream ([`PeerInterest`] does not depend on it).
    ///
    /// This is the **distance** half alone. The uncullable candidates are never returned here at
    /// any radius, because they have no distance to be inside;
    /// [`PeerInterest::update_grid_into`] merges them in from [`Self::uncullable_for`].
    pub fn query_within(
        &self,
        cfg: &AoiConfig,
        observer: MembershipId,
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
        for (&membership, world) in &self.worlds {
            if !membership_matches(observer, membership) {
                continue;
            }
            if span_x * span_z > world.len() as u128 {
                for bucket in world.values() {
                    Self::append_within(bucket, center, radius_sq, out);
                }
            } else {
                for cx in min_x..=max_x {
                    for cz in min_z..=max_z {
                        if let Some(bucket) = world.get(&(cx, cz)) {
                            Self::append_within(bucket, center, radius_sq, out);
                        }
                    }
                }
            }
        }
    }

    /// Every entity `observer` may see that bypasses the distance test: the
    /// [`InterestCandidate::always`] candidates and the ones whose position could not be binned.
    /// Order is unspecified.
    pub fn uncullable_for(&self, observer: MembershipId) -> impl Iterator<Item = BodyId> + '_ {
        self.uncullable
            .iter()
            .filter(move |&&(_, membership)| membership_matches(observer, membership))
            .map(|&(id, _)| id)
    }

    /// Every entity `observer` may see at **any** distance: the binned ones plus
    /// [`Self::uncullable_for`]. Order is unspecified.
    ///
    /// This is what an observer with no usable centre sees. A non-finite centre is a measurement
    /// that failed, and the filter fails open on it rather than blanking that peer's world.
    pub fn visible_to(&self, observer: MembershipId) -> impl Iterator<Item = BodyId> + '_ {
        self.worlds
            .iter()
            .filter(move |(&membership, _)| membership_matches(observer, membership))
            .flat_map(|(_, world)| world.values().flat_map(|b| b.iter().map(|&(id, _)| id)))
            .chain(self.uncullable_for(observer))
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

/// Which of several independent worlds an entity or an observer belongs to.
///
/// Opaque to this crate: the filter only ever compares two of them for equality, so a game may key
/// it on a world index, an instance handle or a hash. `0` is [`MEMBERSHIP_GLOBAL`] and is the only
/// value with a meaning attached.
pub type MembershipId = u64;

/// The membership that matches every other one, and the default on both sides of the comparison.
///
/// Two roles, both wanted:
///
/// * **Session-global.** A candidate in `MEMBERSHIP_GLOBAL` is offered to observers in every world
///   — the setting for a channel that describes the session rather than a place in it.
/// * **Fail open.** An observer whose membership could not be resolved lands here and sees every
///   world. A misconfigured membership then costs bandwidth; the opposite default would delete
///   every body from that peer's world.
pub const MEMBERSHIP_GLOBAL: MembershipId = 0;

/// Whether a candidate in membership `candidate` is offered to an observer in membership
/// `observer`.
///
/// True when either side is [`MEMBERSHIP_GLOBAL`], or when the two are equal. The rule is
/// symmetric, so a game that declares no memberships at all leaves every comparison true and is
/// filtered on distance alone.
#[must_use]
pub fn membership_matches(observer: MembershipId, candidate: MembershipId) -> bool {
    observer == MEMBERSHIP_GLOBAL || candidate == MEMBERSHIP_GLOBAL || observer == candidate
}

/// One entity offered to a peer's interest filter for a tick.
///
/// Two independent axes, and the separation is the point:
///
/// * `always` suppresses the **distance** test — never culled by radius, never evicted by
///   `max_entities`. It is the fail-open flag and carries three facts that all mean "never cull
///   this": the peer's own body, an entity whose synchronizer declares no anchor at all, and an
///   entity whose anchor could not be resolved this tick. Keeping them one flag is deliberate —
///   the filter has no business distinguishing them.
/// * `membership` is checked **first and separately**, and `always` does not suppress it. An
///   entity that is always-relevant within its own world is `always` plus a membership; one that
///   is always-relevant in every world is `always` plus [`MEMBERSHIP_GLOBAL`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InterestCandidate {
    /// The entity id.
    pub id: BodyId,
    /// World-space anchor for the distance test. Ignored entirely when `always` is set.
    pub pos: [f32; 3],
    /// Unconditionally relevant: never culled by radius, never evicted by `max_entities`.
    ///
    /// Says nothing about `membership` — an `always` candidate in a world the observer is not in
    /// is still refused.
    pub always: bool,
    /// The world this entity belongs to, or [`MEMBERSHIP_GLOBAL`] for every world.
    pub membership: MembershipId,
}

impl InterestCandidate {
    /// A candidate in every world, culled by distance from `pos`.
    #[must_use]
    pub fn anchored(id: BodyId, pos: [f32; 3]) -> Self {
        Self::anchored_in(id, pos, MEMBERSHIP_GLOBAL)
    }

    /// A candidate in `membership`, culled by distance from `pos`.
    #[must_use]
    pub fn anchored_in(id: BodyId, pos: [f32; 3], membership: MembershipId) -> Self {
        Self {
            id,
            pos,
            always: false,
            membership,
        }
    }

    /// A candidate that is always in interest, in every world.
    #[must_use]
    pub fn always(id: BodyId) -> Self {
        Self::always_in(id, MEMBERSHIP_GLOBAL)
    }

    /// A candidate that is always in interest **within `membership`**, and refused outside it.
    ///
    /// The setting for a channel with no position to be culled by — health, inventory, a door's
    /// state — that still belongs to one world rather than to the session.
    #[must_use]
    pub fn always_in(id: BodyId, membership: MembershipId) -> Self {
        Self {
            id,
            pos: [0.0; 3],
            always: true,
            membership,
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

    /// Recompute the set from `grid` for a peer observing from `center`, reporting every id that
    /// **left**.
    ///
    /// These are [`Self::update_linear_into`]'s rules applied to the same candidates through a
    /// spatial index instead of a flat scan. The two paths are asserted to agree — members and
    /// leaves alike — over a randomised walk in this module's suite, so choosing between them is a
    /// cost decision and not a behaviour change. Part by part:
    ///
    /// * Entities within `enter_radius` join; current members are retained until they exceed
    ///   `enter_radius * exit_factor`; members the query no longer returns (moved away, despawned,
    ///   or position went non-finite) are removed — nothing leaks.
    /// * `observer` is the world the peer is in. [`InterestGrid::rebuild`] bins each world
    ///   separately and the query reads only the ones [`membership_matches`] admits, so an
    ///   overlapping world's entities never enter the set at any distance.
    /// * [`InterestCandidate::always`] entities, and any whose position could not be binned, arrive
    ///   from [`InterestGrid::uncullable_for`] and bypass both the radius and the cap.
    /// * When `cfg.max_entities > 0`, only the nearest N **cullable** entities survive, ordered by
    ///   distance, then current members before newcomers on a distance tie (so the set stays stable
    ///   when a newcomer merely matches a member's range), then ascending id (so the result is
    ///   deterministic regardless of grid iteration order). An entity evicted by the cap is a real
    ///   leave: it must re-enter through `enter_radius` like any newcomer.
    /// * `leaves` receives every id that was a member and is not one now — radius exits, membership
    ///   refusals and cap evictions alike. The caller clears its per-peer delta bookkeeping from
    ///   that list, so a re-entrant entity gets a full block rather than a delta against a base the
    ///   peer stopped tracking. Without it the peer NACKs, and a NACK is per-peer and all-entity:
    ///   one re-entering body costs a full-state burst for everything that peer holds.
    ///
    /// `also` holds candidates offered to **this peer alone**, filtered exactly as
    /// [`Self::update_linear_into`] filters its slice and merged in before the cap runs. It carries
    /// the facts a grid shared by every peer cannot, of which the send path has one: a peer's own
    /// body is always-relevant to that peer and to no other. An id named in `also` is answered by
    /// `also` alone and its binned entry is ignored, so the two can never both admit it. That check
    /// is a scan of `also` per grid hit, which is why `also` holds a peer's handful of overrides
    /// rather than a second candidate list.
    ///
    /// A non-finite `center` admits every entity `observer` may see, uncapped. An observer that
    /// cannot be located measures nothing, and failing open costs bandwidth where failing closed
    /// would blank that peer's world; [`Self::update_linear_into`] fails open the same way.
    ///
    /// `leaves` and `scratch` are both cleared on entry; `scratch` is caller-owned working storage
    /// so a warm per-peer update allocates nothing.
    // The grid, the config, the observer's centre and world, that peer's overrides and the two
    // caller-owned buffers. Bundling any pair of them into a struct would either allocate per peer
    // per tick or move the same arguments to a constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn update_grid_into(
        &mut self,
        grid: &InterestGrid,
        cfg: &AoiConfig,
        center: [f32; 3],
        observer: MembershipId,
        also: &[InterestCandidate],
        scratch: &mut Vec<(BodyId, f32)>,
        leaves: &mut Vec<BodyId>,
    ) {
        scratch.clear();
        leaves.clear();
        let center_ok = center[0].is_finite() && center[1].is_finite() && center[2].is_finite();
        let enter_radius = cfg.effective_enter_radius();
        let enter_sq = enter_radius * enter_radius;
        // `.min(f32::MAX)` keeps an overflowing product finite so the query still runs.
        let exit_radius = (enter_radius * cfg.effective_exit_factor()).min(f32::MAX);
        let exit_sq = exit_radius * exit_radius;
        // Cheap because `also` holds a peer's overrides, and free when it is empty — which it is
        // for every caller that has no per-peer fact to add.
        let overridden = |id: BodyId| !also.is_empty() && also.iter().any(|c| c.id == id);

        let mut cullable = 0usize;
        if center_ok {
            // One query at the exit radius serves both bands: everything returned is inside the
            // exit radius (members retained), and the enter check below filters the newcomers.
            grid.query_within(cfg, observer, center, exit_radius, scratch);
            let members = &self.members;
            scratch.retain(|&(id, dist_sq)| {
                !overridden(id) && (members.contains_key(&id) || dist_sq <= enter_sq)
            });
            cullable = scratch.len();
            for id in grid.uncullable_for(observer) {
                if !overridden(id) {
                    // Sorting below `0.0` is impossible, so an uncullable entity can never be
                    // reordered ahead of a genuinely closer one by the cap sort — it is excluded
                    // from that sort's population instead.
                    scratch.push((id, f32::NEG_INFINITY));
                }
            }
        } else {
            for id in grid.visible_to(observer) {
                if !overridden(id) {
                    scratch.push((id, f32::NEG_INFINITY));
                }
            }
        }
        for candidate in also {
            if let Some((dist_sq, is_cullable)) =
                self.classify(candidate, observer, center, center_ok, enter_sq, exit_sq)
            {
                scratch.push((candidate.id, dist_sq));
                cullable += usize::from(is_cullable);
            }
        }

        Self::apply_cap(cfg, &self.members, cullable, scratch);
        self.commit(scratch, leaves);
    }

    /// Recompute the set from a flat candidate slice, reporting every id that **left**.
    ///
    /// This is the path `orbit_net.rs` runs (see the module header for why the grid is not). The
    /// hysteresis, cap, tie-breaking and leave rules are [`Self::update_grid_into`]'s; what differs
    /// is only how candidates are found. Three of them are worth restating where the shipped caller
    /// will read them:
    ///
    /// * [`InterestCandidate::always`] entities bypass both the radius and the cap. The cap bounds
    ///   the *cullable* set; an unconditionally-relevant entity is never evicted by it.
    /// * `observer` is the world the peer is in. A candidate [`membership_matches`] refuses is
    ///   dropped **before** the radius and before `always` is consulted, so an overlapping world's
    ///   entities never enter the set at any distance, and an always-relevant channel is bounded
    ///   to its own world. [`MEMBERSHIP_GLOBAL`] on either side matches, which is why a game that
    ///   declares no memberships keeps the distance-only behaviour exactly.
    /// * `leaves` receives every id that was a member and is not one now — radius exits, membership
    ///   refusals **and** cap evictions alike, because each is a real leave that must re-enter
    ///   through `enter_radius` like any newcomer. The caller uses this to clear its per-peer delta
    ///   bookkeeping, so a re-entrant entity gets a full block rather than a delta against a base
    ///   the peer stopped tracking.
    ///
    /// A candidate whose position (or `center`) is non-finite is treated as `always` rather than
    /// dropped: an unbinnable body is a body the filter cannot reason about, and failing open
    /// wastes bandwidth where failing closed would silently delete it from someone's world. That
    /// fail-open covers the **distance** test only. An unbinnable candidate in another world is
    /// still refused: its membership is a declaration rather than a measurement, and it did not
    /// fail.
    ///
    /// `leaves` and `scratch` are both cleared on entry; `scratch` is caller-owned working storage
    /// so a warm per-peer update allocates nothing.
    pub fn update_linear_into(
        &mut self,
        cfg: &AoiConfig,
        center: [f32; 3],
        observer: MembershipId,
        candidates: &[InterestCandidate],
        scratch: &mut Vec<(BodyId, f32)>,
        leaves: &mut Vec<BodyId>,
    ) {
        scratch.clear();
        leaves.clear();
        let center_ok = center[0].is_finite() && center[1].is_finite() && center[2].is_finite();
        let enter_radius = cfg.effective_enter_radius();
        let enter_sq = enter_radius * enter_radius;
        // `.min(f32::MAX)` keeps an overflowing product finite so the comparison still means
        // something.
        let exit_radius = (enter_radius * cfg.effective_exit_factor()).min(f32::MAX);
        let exit_sq = exit_radius * exit_radius;

        let mut cullable = 0usize;
        for candidate in candidates {
            if let Some((dist_sq, is_cullable)) =
                self.classify(candidate, observer, center, center_ok, enter_sq, exit_sq)
            {
                scratch.push((candidate.id, dist_sq));
                cullable += usize::from(is_cullable);
            }
        }

        Self::apply_cap(cfg, &self.members, cullable, scratch);
        self.commit(scratch, leaves);
    }

    /// One candidate's verdict, shared by both update paths so their rules cannot drift apart.
    ///
    /// `None` refuses it. `Some((dist_sq, cullable))` keeps it, and `cullable` is `false` for an
    /// entity that bypassed the distance test — those carry [`f32::NEG_INFINITY`] and are excluded
    /// from the cap's population rather than merely sorted to the front of it.
    ///
    /// Membership is decided first. A candidate in a world the observer is not in is refused
    /// whatever its distance and whatever `always` says, so nothing below that line can readmit it.
    fn classify(
        &self,
        candidate: &InterestCandidate,
        observer: MembershipId,
        center: [f32; 3],
        center_ok: bool,
        enter_sq: f32,
        exit_sq: f32,
    ) -> Option<(f32, bool)> {
        if !membership_matches(observer, candidate.membership) {
            return None;
        }
        let pos = candidate.pos;
        let binnable = center_ok && pos[0].is_finite() && pos[1].is_finite() && pos[2].is_finite();
        if candidate.always || !binnable {
            return Some((f32::NEG_INFINITY, false));
        }
        let dx = pos[0] - center[0];
        let dy = pos[1] - center[1];
        let dz = pos[2] - center[2];
        let dist_sq = dx * dx + dy * dy + dz * dz;
        let keep = if self.members.contains_key(&candidate.id) {
            dist_sq <= exit_sq
        } else {
            dist_sq <= enter_sq
        };
        keep.then_some((dist_sq, true))
    }

    /// Keep only the nearest `cfg.max_entities` cullable entries, plus every uncullable one.
    ///
    /// `cullable` is how many of `scratch`'s entries were admitted on distance; the rest carry
    /// [`f32::NEG_INFINITY`] and sort to the front, so truncating to `uncullable + max_entities`
    /// keeps all of them and the nearest N of the others.
    fn apply_cap(
        cfg: &AoiConfig,
        members: &BTreeMap<BodyId, f32>,
        cullable: usize,
        scratch: &mut Vec<(BodyId, f32)>,
    ) {
        if cfg.max_entities == 0 || cullable <= cfg.max_entities {
            return;
        }
        scratch.sort_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then_with(|| members.contains_key(&b.0).cmp(&members.contains_key(&a.0)))
                .then_with(|| a.0.cmp(&b.0))
        });
        let uncullable = scratch.len() - cullable;
        scratch.truncate(uncullable + cfg.max_entities);
    }

    /// Diff `scratch` against the current members, push what left to `leaves`, then overwrite.
    ///
    /// Diff **before** overwrite: everything the old set held that the new one does not is a leave.
    /// `self.members` is already ascending by id, so sorting `scratch` the same way turns the diff
    /// into one linear merge — a nested scan here would be O(K^2) per peer per tick, which at the
    /// interest-set sizes this exists to serve is worse than the filter it replaced.
    fn commit(&mut self, scratch: &mut [(BodyId, f32)], leaves: &mut Vec<BodyId>) {
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

    /// Every entity as a plain anchored candidate in every world — the shape most of the grid
    /// suite feeds [`InterestGrid::rebuild`].
    fn anchored(entities: &[(BodyId, [f32; 3])]) -> Vec<InterestCandidate> {
        entities
            .iter()
            .map(|&(id, pos)| InterestCandidate::anchored(id, pos))
            .collect()
    }

    /// A grid update for a global observer with no per-peer overrides, discarding the leaves —
    /// for the tests about the distance rules rather than about the diff.
    fn update_grid(
        peer: &mut PeerInterest,
        grid: &InterestGrid,
        cfg: &AoiConfig,
        center: [f32; 3],
    ) {
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_grid_into(
            grid,
            cfg,
            center,
            MEMBERSHIP_GLOBAL,
            &[],
            &mut scratch,
            &mut leaves,
        );
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
        grid.rebuild(&cfg, &anchored(&entities));

        let centers = [
            [0.0, 0.0, 0.0],
            [150.0, -40.0, -150.0],
            [-31.9, 5.0, 32.1], // straddles a cell boundary
        ];
        let radii = [0.0, 12.5, 64.0, 250.0, 1000.0];
        let mut out: Vec<(BodyId, f32)> = Vec::new();
        for center in centers {
            for radius in radii {
                grid.query_within(&cfg, MEMBERSHIP_GLOBAL, center, radius, &mut out);
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
        grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], 100.0, &mut out);
        assert!(out.is_empty());
        assert_eq!(grid.uncullable_for(MEMBERSHIP_GLOBAL).count(), 0);
        assert_eq!(grid.visible_to(MEMBERSHIP_GLOBAL).count(), 0);

        let mut peer = PeerInterest::new();
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert!(peer.is_empty());
        assert_eq!(peer.len(), 0);
        assert_eq!(peer.iter().count(), 0);
    }

    #[test]
    fn y_offset_counts_for_distance_but_not_for_cell_assignment() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        // Both share the origin's XZ cell; one hangs 300 m overhead.
        grid.rebuild(
            &cfg,
            &anchored(&[(1, [0.0, 300.0, 0.0]), (2, [0.0, 10.0, 0.0])]),
        );
        let mut out = Vec::new();
        grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], 50.0, &mut out);
        assert_eq!(sorted_by_id(out.clone()), vec![(2, 100.0)]);
        // A radius covering the true 3D distance finds the overhead body too, which also proves
        // it was binned by XZ alone — the scan rectangle only covers cells around the origin.
        grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], 301.0, &mut out);
        assert_eq!(sorted_by_id(out.clone()).len(), 2);
    }

    #[test]
    fn non_finite_positions_are_held_beside_the_cells_and_fail_open() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &anchored(&[
                (1, [f32::NAN, 0.0, 0.0]),
                (2, [0.0, f32::INFINITY, 0.0]),
                (3, [0.0, 0.0, f32::NEG_INFINITY]),
                (4, [1.0, 2.0, 3.0]),
                (5, [10_000.0, 0.0, 0.0]),
            ]),
        );
        // None of the three unbinnable bodies is in a cell, so the distance query never sees them
        // whatever its radius.
        let mut out = Vec::new();
        grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], f32::MAX.sqrt(), &mut out);
        let ids: Vec<BodyId> = sorted_by_id(out).iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, vec![4, 5]);

        // They are uncullable instead — a body the filter cannot reason about is replicated
        // rather than silently deleted from the peer's world. Body 5 is genuinely out of range.
        let mut uncullable: Vec<BodyId> = grid.uncullable_for(MEMBERSHIP_GLOBAL).collect();
        uncullable.sort_unstable();
        assert_eq!(uncullable, vec![1, 2, 3]);

        let mut peer = PeerInterest::new();
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        // An uncullable member has no meaningful distance, and stores `0.0` rather than the
        // `NEG_INFINITY` it sorted under.
        assert_eq!(peer.dist_sq(1), Some(0.0));
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
            grid.rebuild(&cfg, &anchored(&entities));
            grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], 200.0, &mut out);
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
        grid.rebuild(&cfg, &anchored(&[(1, [0.0, 0.0, 0.0])]));
        let mut out = Vec::new();
        for (center, radius) in [
            ([f32::NAN, 0.0, 0.0], 100.0),
            ([0.0, f32::INFINITY, 0.0], 100.0),
            ([0.0; 3], f32::NAN),
            ([0.0; 3], f32::INFINITY),
            ([0.0; 3], -1.0),
        ] {
            grid.query_within(&cfg, MEMBERSHIP_GLOBAL, center, radius, &mut out);
            assert!(out.is_empty(), "center {center:?} radius {radius} matched");
        }
    }

    #[test]
    fn rebuild_replaces_previous_contents() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &[
                InterestCandidate::anchored(1, [0.0; 3]),
                InterestCandidate::anchored_in(2, [10.0, 0.0, 0.0], 7),
                InterestCandidate::always(9),
            ],
        );
        grid.rebuild(&cfg, &anchored(&[(3, [0.0; 3])]));
        let mut out = Vec::new();
        grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], 500.0, &mut out);
        assert_eq!(sorted_by_id(out), vec![(3, 0.0)]);
        // The emptied world and the uncullable list are replaced too, not merely shadowed.
        assert_eq!(grid.uncullable_for(MEMBERSHIP_GLOBAL).count(), 0);
        assert_eq!(grid.visible_to(7).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn enormous_radius_scans_occupied_cells_not_the_rectangle() {
        let cfg = AoiConfig::default();
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &anchored(&[(1, [0.0; 3]), (2, [5000.0, 0.0, -5000.0])]),
        );
        let mut out = Vec::new();
        // Finite but so large the XZ scan rectangle would span the whole i32 cell range; the
        // occupied-cell fallback must return everything without iterating billions of cells.
        grid.query_within(&cfg, MEMBERSHIP_GLOBAL, [0.0; 3], 1.0e30, &mut out);
        let ids: Vec<BodyId> = sorted_by_id(out).iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn hysteresis_enters_below_enter_radius_and_holds_through_the_band() {
        let cfg = cfg(32.0, 100.0, 1.25, 0); // exit at 125
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();

        grid.rebuild(&cfg, &anchored(&[(1, [90.0, 0.0, 0.0])]));
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert!(peer.contains(1), "90 m is inside the enter radius");

        for held in [110.0f32, 124.0] {
            grid.rebuild(&cfg, &anchored(&[(1, [held, 0.0, 0.0])]));
            update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
            assert!(peer.contains(1), "{held} m is inside the hysteresis band");
        }

        grid.rebuild(&cfg, &anchored(&[(1, [126.0, 0.0, 0.0])]));
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
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

        for shot in [1.0f32, 600.0, 700.0, 1999.0, sniper_range] {
            grid.rebuild(&cfg, &anchored(&[(1, [shot, 0.0, 0.0])]));
            update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
            assert!(
                peer.contains(1),
                "a target {shot} m away is within the sniper's reach and must still be replicated"
            );
        }

        // And the radius is what bounds it: past the hysteresis band the body does leave, which is
        // the behaviour an arena larger than the sniper's reach is meant to get.
        grid.rebuild(
            &cfg,
            &anchored(&[(1, [sniper_range * 1.25 + 1.0, 0.0, 0.0])]),
        );
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert!(!peer.contains(1), "past the exit radius it leaves");
    }

    #[test]
    fn hysteresis_band_does_not_admit_newcomers() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &anchored(&[(1, [110.0, 0.0, 0.0])]));
        let mut peer = PeerInterest::new();
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
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
        grid.rebuild(
            &cfg,
            &anchored(&[(1, [50.0, 0.0, 0.0]), (2, [60.0, 0.0, 0.0])]),
        );
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.len(), 2);

        // Body 1 teleports far away, body 2 despawns from the grid entirely.
        grid.rebuild(&cfg, &anchored(&[(1, [10_000.0, 0.0, 0.0])]));
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert!(!peer.contains(1));
        assert!(!peer.contains(2));
        assert!(peer.is_empty(), "stale members must not accumulate");
    }

    #[test]
    fn exit_factor_below_one_collapses_the_band() {
        let cfg = cfg(32.0, 100.0, 0.5, 0); // effective factor 1.0: no band
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        grid.rebuild(&cfg, &anchored(&[(1, [50.0, 0.0, 0.0])]));
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert!(peer.contains(1));
        grid.rebuild(&cfg, &anchored(&[(1, [101.0, 0.0, 0.0])]));
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert!(!peer.contains(1), "with no band, past enter means out");
    }

    #[test]
    fn a_negative_enter_radius_culls_everything_on_both_paths() {
        // The squared distance the filter compares against cannot carry the sign, so a raw
        // negative radius would admit everything within its magnitude on the linear path while the
        // grid query rejected it outright — the one input on which the two could disagree.
        let cfg = cfg(32.0, -100.0, 1.25, 0);
        let entities = [(1, [10.0, 0.0, 0.0]), (2, [0.0; 3])];
        let mut grid = InterestGrid::new();
        grid.rebuild(&cfg, &anchored(&entities));
        let mut via_grid = PeerInterest::new();
        update_grid(&mut via_grid, &grid, &cfg, [0.0; 3]);

        let mut via_linear = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        via_linear.update_linear_into(
            &cfg,
            [0.0; 3],
            MEMBERSHIP_GLOBAL,
            &anchored(&entities),
            &mut scratch,
            &mut leaves,
        );
        // A radius of zero still admits a body at exactly the centre, which body 2 is.
        assert_eq!(via_grid.iter().collect::<Vec<_>>(), vec![2]);
        assert_eq!(via_linear.iter().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn max_entities_keeps_the_nearest_and_members_win_ties() {
        let cfg = cfg(32.0, 100.0, 1.25, 2);
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();

        grid.rebuild(
            &cfg,
            &anchored(&[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [30.0, 0.0, 0.0]),
            ]),
        );
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // Body 3 closes to an exact distance tie with member 2: the member wins, so the set does
        // not churn on a mere tie.
        grid.rebuild(
            &cfg,
            &anchored(&[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [0.0, 0.0, 20.0]),
            ]),
        );
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // Strictly closer beats membership: 3 displaces 2, even though 2 is still in range.
        grid.rebuild(
            &cfg,
            &anchored(&[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [15.0, 0.0, 0.0]),
            ]),
        );
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn max_entities_tie_between_newcomers_is_broken_by_id() {
        let cfg = cfg(32.0, 100.0, 1.25, 2);
        let mut grid = InterestGrid::new();
        // Fresh peer, so everyone is a newcomer; 2 and 3 tie at 20 m.
        grid.rebuild(
            &cfg,
            &anchored(&[
                (3, [0.0, 0.0, 20.0]),
                (2, [20.0, 0.0, 0.0]),
                (1, [10.0, 0.0, 0.0]),
            ]),
        );
        let mut peer = PeerInterest::new();
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
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
        grid.rebuild(&cfg, &anchored(&entities));
        let mut peer = PeerInterest::new();
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![7, 42, 99]);

        peer.remove(42);
        assert!(!peer.contains(42));
        assert_eq!(peer.len(), 2);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![7, 99]);

        // Still present in the grid and inside the enter radius, so it re-enters next update.
        update_grid(&mut peer, &grid, &cfg, [0.0; 3]);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![7, 42, 99]);
    }

    // ------------------------------------------------------------------
    // What the grid path gained: worlds, an uncullable set, and a leave diff.
    // ------------------------------------------------------------------

    #[test]
    fn grid_bins_each_world_separately_at_identical_coordinates() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &[
                InterestCandidate::anchored_in(1, [5.0, 0.0, 5.0], 1),
                InterestCandidate::anchored_in(2, [5.0, 0.0, 5.0], 2),
                InterestCandidate::anchored(3, [5.0, 0.0, 5.0]),
            ],
        );
        let mut out = Vec::new();
        for (observer, expected) in [
            (1u64, vec![1u64, 3]),
            (2, vec![2, 3]),
            (MEMBERSHIP_GLOBAL, vec![1, 2, 3]),
            (99, vec![3]),
        ] {
            grid.query_within(&cfg, observer, [0.0; 3], 100.0, &mut out);
            let ids: Vec<BodyId> = sorted_by_id(out.clone())
                .iter()
                .map(|&(id, _)| id)
                .collect();
            assert_eq!(ids, expected, "observer {observer} saw the wrong world");
        }
    }

    #[test]
    fn grid_uncullable_candidates_bypass_both_the_radius_and_the_cap() {
        let cfg = cfg(32.0, 100.0, 1.25, 1);
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &[
                InterestCandidate::always(1),
                InterestCandidate::always_in(2, 5),
                InterestCandidate::always_in(3, 6),
                InterestCandidate::anchored(4, [10.0, 0.0, 0.0]),
                InterestCandidate::anchored(5, [20.0, 0.0, 0.0]),
            ],
        );
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 5, &[], &mut scratch, &mut leaves);
        // 1 is global and 2 shares the observer's world; 3 belongs to another one. The cap of 1
        // bounds only the cullable pair, keeping the nearer of 4 and 5.
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2, 4]);
        assert!(leaves.is_empty());

        // 500 m from anything, and the two always-entities are still there.
        peer.update_grid_into(
            &grid,
            &cfg,
            [500.0, 0.0, 0.0],
            5,
            &[],
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(leaves, vec![4]);
    }

    #[test]
    fn grid_reports_a_radius_exit_as_a_leave() {
        let cfg = cfg(32.0, 100.0, 1.25, 0); // exit at 125
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());

        grid.rebuild(&cfg, &anchored(&[(1, [90.0, 0.0, 0.0])]));
        peer.update_grid_into(
            &grid,
            &cfg,
            [0.0; 3],
            MEMBERSHIP_GLOBAL,
            &[],
            &mut scratch,
            &mut leaves,
        );
        assert!(peer.contains(1));
        assert!(leaves.is_empty(), "entering is not leaving");

        grid.rebuild(&cfg, &anchored(&[(1, [110.0, 0.0, 0.0])]));
        peer.update_grid_into(
            &grid,
            &cfg,
            [0.0; 3],
            MEMBERSHIP_GLOBAL,
            &[],
            &mut scratch,
            &mut leaves,
        );
        assert!(peer.contains(1), "still inside the band");
        assert!(leaves.is_empty());

        grid.rebuild(&cfg, &anchored(&[(1, [126.0, 0.0, 0.0])]));
        peer.update_grid_into(
            &grid,
            &cfg,
            [0.0; 3],
            MEMBERSHIP_GLOBAL,
            &[],
            &mut scratch,
            &mut leaves,
        );
        assert!(!peer.contains(1));
        assert_eq!(
            leaves,
            vec![1],
            "the exit must be reported, not just applied"
        );
    }

    #[test]
    fn grid_counts_a_cap_eviction_and_a_membership_refusal_as_leaves() {
        let cfg = cfg(32.0, 100.0, 1.25, 2);
        let mut grid = InterestGrid::new();
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());

        grid.rebuild(
            &cfg,
            &anchored(&[(1, [10.0, 0.0, 0.0]), (2, [20.0, 0.0, 0.0])]),
        );
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 1, &[], &mut scratch, &mut leaves);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // A third body arrives closer than member 2 and the cap evicts it. An eviction is a real
        // leave: body 2 has to re-enter through the enter radius like any newcomer.
        grid.rebuild(
            &cfg,
            &anchored(&[
                (1, [10.0, 0.0, 0.0]),
                (2, [20.0, 0.0, 0.0]),
                (3, [15.0, 0.0, 0.0]),
            ]),
        );
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 1, &[], &mut scratch, &mut leaves);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(leaves, vec![2]);

        // The peer's own body moves to another world: everything it held is refused at once, and
        // every refusal is a leave, whatever the distance says.
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 1, &[], &mut scratch, &mut leaves);
        assert_eq!(leaves, Vec::<BodyId>::new());
        grid.rebuild(
            &cfg,
            &[
                InterestCandidate::anchored_in(1, [10.0, 0.0, 0.0], 4),
                InterestCandidate::anchored_in(3, [15.0, 0.0, 0.0], 4),
            ],
        );
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 1, &[], &mut scratch, &mut leaves);
        assert!(peer.is_empty());
        assert_eq!(leaves, vec![1, 3]);
    }

    #[test]
    fn grid_fails_open_on_a_non_finite_centre() {
        let cfg = cfg(32.0, 100.0, 1.25, 1);
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &[
                InterestCandidate::anchored(1, [10.0, 0.0, 0.0]),
                InterestCandidate::anchored(2, [100_000.0, 0.0, 0.0]),
                InterestCandidate::always_in(3, 8),
                InterestCandidate::anchored_in(4, [10.0, 0.0, 0.0], 9),
            ],
        );
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_grid_into(
            &grid,
            &cfg,
            [f32::NAN, 0.0, 0.0],
            8,
            &[],
            &mut scratch,
            &mut leaves,
        );
        // An observer that cannot be located measures nothing, so nothing is culled by distance
        // and the cap of 1 evicts nothing either. The other world is still refused: a membership
        // is a declaration, and it did not fail.
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn grid_per_peer_overrides_shadow_the_binned_entry() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut grid = InterestGrid::new();
        grid.rebuild(
            &cfg,
            &[
                // The peer's own body, 3 km out in a world the peer is not in — the two facts a
                // grid shared by every peer cannot carry.
                InterestCandidate::anchored_in(1, [3000.0, 0.0, 0.0], 7),
                InterestCandidate::anchored(2, [10.0, 0.0, 0.0]),
            ],
        );
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 3, &[], &mut scratch, &mut leaves);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![2], "no override yet");

        let own = [InterestCandidate::always(1)];
        peer.update_grid_into(&grid, &cfg, [0.0; 3], 3, &own, &mut scratch, &mut leaves);
        assert_eq!(peer.iter().collect::<Vec<_>>(), vec![1, 2]);

        // The override answers for that id alone. A refusing one removes the body the grid would
        // otherwise have admitted, rather than the two both admitting it.
        let refused = [InterestCandidate::anchored_in(2, [10.0, 0.0, 0.0], 12)];
        peer.update_grid_into(
            &grid,
            &cfg,
            [0.0; 3],
            3,
            &refused,
            &mut scratch,
            &mut leaves,
        );
        assert!(peer.is_empty());
        assert_eq!(leaves, vec![1, 2]);
    }

    // ------------------------------------------------------------------
    // The linear path — the one the backend actually runs.
    // ------------------------------------------------------------------

    #[test]
    fn linear_agrees_with_the_grid_over_a_pseudo_random_walk() {
        // The two paths are separate implementations of one rule; if they ever disagree, the
        // measurement that chose between them was comparing different work. Everything that can
        // differ between them is varied here at once: three worlds at overlapping coordinates,
        // always-relevant channels, positions that go non-finite mid-walk, a cap that bites, an
        // observer whose centre cannot be resolved, and a per-peer override that shadows a binned
        // body. Members *and* leaves are compared every step, because the send path's correctness
        // rests on the leaves rather than on the set.
        let cfg = cfg(64.0, 100.0, 1.25, 12);
        let mut state = 0x0bad_f00du32;
        let mut candidates: Vec<InterestCandidate> = (0..64u64)
            .map(|id| {
                let pos = [
                    lcg_coord(&mut state),
                    lcg_coord(&mut state),
                    lcg_coord(&mut state),
                ];
                match id % 8 {
                    0 => InterestCandidate::always_in(id + 1, id % 3),
                    _ => InterestCandidate::anchored_in(id + 1, pos, id % 3),
                }
            })
            .collect();
        // One override per observer world, shadowing a body the grid holds anchored elsewhere.
        let overrides = [
            InterestCandidate::always(4),
            InterestCandidate::anchored_in(5, [0.0; 3], 2),
        ];

        let mut grid = InterestGrid::new();
        let mut via_grid = PeerInterest::new();
        let mut via_linear = PeerInterest::new();
        let mut scratch = Vec::new();
        let (mut grid_leaves, mut linear_leaves) = (Vec::new(), Vec::new());

        for step in 0..120u32 {
            for candidate in candidates.iter_mut() {
                candidate.pos[0] += lcg_coord(&mut state) * 0.05;
                candidate.pos[2] += lcg_coord(&mut state) * 0.05;
            }
            // A body's anchor goes non-finite for one step in eight — the fail-open the two paths
            // reach by opposite routes: the linear one classifies it, the grid holds it out of the
            // cells entirely.
            let sick = (step as usize * 7) % candidates.len();
            let restore = candidates[sick].pos;
            if step % 8 == 3 {
                candidates[sick].pos[1] = f32::NAN;
            }
            let observer = u64::from(step % 4);
            let center = if step % 16 == 9 {
                [f32::INFINITY, 0.0, 0.0]
            } else {
                [
                    lcg_coord(&mut state) * 0.2,
                    0.0,
                    lcg_coord(&mut state) * 0.2,
                ]
            };
            let also: &[InterestCandidate] = if step % 3 == 0 { &overrides } else { &[] };
            // The linear path sees one flat list, so an override replaces the row it names.
            let flat: Vec<InterestCandidate> = candidates
                .iter()
                .map(|c| *also.iter().find(|o| o.id == c.id).unwrap_or(c))
                .collect();

            grid.rebuild(&cfg, &candidates);
            via_grid.update_grid_into(
                &grid,
                &cfg,
                center,
                observer,
                also,
                &mut scratch,
                &mut grid_leaves,
            );
            via_linear.update_linear_into(
                &cfg,
                center,
                observer,
                &flat,
                &mut scratch,
                &mut linear_leaves,
            );
            assert_eq!(
                via_grid.iter_with_distance().collect::<Vec<_>>(),
                via_linear.iter_with_distance().collect::<Vec<_>>(),
                "grid and linear diverged on members at step {step}"
            );
            assert_eq!(
                grid_leaves, linear_leaves,
                "grid and linear diverged on leaves at step {step}"
            );
            candidates[sick].pos = restore;
        }
        // The walk has to actually exercise the rules it varies, or it proves the two paths agree
        // about nothing.
        assert!(!via_grid.is_empty());
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
                MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
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
            MEMBERSHIP_GLOBAL,
            &anchored(&[(9, [3.0, 0.0, 4.0]), (4, [10.0, 0.0, 0.0])]),
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(
            peer.iter_with_distance().collect::<Vec<_>>(),
            vec![(4, 100.0), (9, 25.0)]
        );
    }

    // ------------------------------------------------------------------
    // Membership: the second axis, checked before the radius.
    // ------------------------------------------------------------------

    /// One update against a fixed `cfg`, returning the resulting member ids.
    fn members_of(
        peer: &mut PeerInterest,
        cfg: &AoiConfig,
        observer: MembershipId,
        candidates: &[InterestCandidate],
    ) -> Vec<BodyId> {
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());
        peer.update_linear_into(
            cfg,
            [0.0; 3],
            observer,
            candidates,
            &mut scratch,
            &mut leaves,
        );
        peer.iter().collect()
    }

    /// The whole point of the feature: two worlds rebased on the same coordinates are one squared
    /// distance apart — zero — so only the membership id can separate them.
    #[test]
    fn overlapping_worlds_at_identical_coordinates_are_separated_by_membership() {
        let cfg = cfg(32.0, 200.0, 1.25, 0);
        // Same position, three different worlds, plus one session-global entity.
        let candidates = [
            InterestCandidate::anchored_in(1, [10.0, 0.0, 0.0], 1),
            InterestCandidate::anchored_in(2, [10.0, 0.0, 0.0], 2),
            InterestCandidate::anchored_in(3, [10.0, 0.0, 0.0], 3),
            InterestCandidate::anchored(4, [10.0, 0.0, 0.0]),
        ];
        assert_eq!(
            members_of(&mut PeerInterest::new(), &cfg, 1, &candidates),
            vec![1, 4],
            "an observer in world 1 sees world 1 and the global entity"
        );
        assert_eq!(
            members_of(&mut PeerInterest::new(), &cfg, 2, &candidates),
            vec![2, 4]
        );
        // An observer with no membership sees every world — the fail-open direction.
        assert_eq!(
            members_of(
                &mut PeerInterest::new(),
                &cfg,
                MEMBERSHIP_GLOBAL,
                &candidates
            ),
            vec![1, 2, 3, 4]
        );
    }

    /// `always` suppresses the radius and the cap, and must not suppress membership. This is the
    /// case the feature exists for: a channel with no position — health, inventory, a door — that
    /// still belongs to one world.
    #[test]
    fn an_always_candidate_in_another_world_is_still_refused() {
        let cfg = cfg(32.0, 1.0, 1.25, 0); // radius so small only `always` could get through
        let candidates = [
            InterestCandidate::always_in(1, 1),
            InterestCandidate::always_in(2, 2),
            InterestCandidate::always(3),
        ];
        assert_eq!(
            members_of(&mut PeerInterest::new(), &cfg, 1, &candidates),
            vec![1, 3],
            "always-in-world-2 must not reach a world-1 observer; always-in-every-world must"
        );
    }

    /// A membership refusal is a real leave, so the caller clears the delta bookkeeping and the
    /// entity comes back as a full block rather than a delta against a base its peer dropped.
    #[test]
    fn a_membership_change_reports_the_refusal_as_a_leave() {
        let cfg = cfg(32.0, 200.0, 1.25, 0);
        let mut peer = PeerInterest::new();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());

        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            1,
            &[InterestCandidate::anchored_in(7, [10.0, 0.0, 0.0], 1)],
            &mut scratch,
            &mut leaves,
        );
        assert!(peer.contains(7));
        assert!(leaves.is_empty());

        // The entity is rebased into world 2 without moving: distance says keep, membership says go.
        peer.update_linear_into(
            &cfg,
            [0.0; 3],
            1,
            &[InterestCandidate::anchored_in(7, [10.0, 0.0, 0.0], 2)],
            &mut scratch,
            &mut leaves,
        );
        assert!(!peer.contains(7));
        assert_eq!(leaves, vec![7]);
    }

    /// Hysteresis retains a current member out to the exit radius. Membership is not a band and has
    /// no hysteresis: a refused candidate leaves on the tick it is refused, member or not.
    #[test]
    fn membership_is_refused_without_a_hysteresis_band() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let mut peer = PeerInterest::new();
        // Inside the enter radius in world 1, so it is a member...
        assert_eq!(
            members_of(
                &mut peer,
                &cfg,
                1,
                &[InterestCandidate::anchored_in(1, [90.0, 0.0, 0.0], 1)]
            ),
            vec![1]
        );
        // ...and still inside the exit radius, which retains a member on distance alone.
        assert!(peer.dist_sq(1).is_some());
        assert_eq!(
            members_of(
                &mut peer,
                &cfg,
                1,
                &[InterestCandidate::anchored_in(1, [110.0, 0.0, 0.0], 2)]
            ),
            Vec::<BodyId>::new(),
            "membership is checked before the band, so being a member does not retain it"
        );
    }

    /// The cap bounds the cullable set. A candidate refused by membership was never cullable, so it
    /// must not consume one of the N slots the nearest entities compete for.
    #[test]
    fn a_membership_refusal_does_not_consume_a_cap_slot() {
        let cfg = cfg(32.0, 200.0, 1.25, 2); // nearest 2 win
        let candidates = [
            InterestCandidate::anchored_in(1, [1.0, 0.0, 0.0], 2), // nearest, wrong world
            InterestCandidate::anchored_in(2, [2.0, 0.0, 0.0], 2), // second nearest, wrong world
            InterestCandidate::anchored_in(3, [3.0, 0.0, 0.0], 1),
            InterestCandidate::anchored_in(4, [4.0, 0.0, 0.0], 1),
            InterestCandidate::anchored_in(5, [5.0, 0.0, 0.0], 1),
        ];
        assert_eq!(
            members_of(&mut PeerInterest::new(), &cfg, 1, &candidates),
            vec![3, 4],
            "the two nearest entities IN THE OBSERVER'S WORLD win the cap"
        );
    }

    /// Every existing call site passes [`MEMBERSHIP_GLOBAL`] on both sides, so the filter must be
    /// bit-identical to the distance-only one it replaces.
    #[test]
    fn declaring_no_memberships_leaves_the_distance_filter_unchanged() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let entities = [
            (1, [10.0, 0.0, 0.0]),
            (2, [99.0, 0.0, 0.0]),
            (3, [101.0, 0.0, 0.0]),
            (4, [5_000.0, 0.0, 0.0]),
        ];
        assert_eq!(
            members_of(
                &mut PeerInterest::new(),
                &cfg,
                MEMBERSHIP_GLOBAL,
                &anchored(&entities)
            ),
            vec![1, 2]
        );
    }

    /// The match rule is symmetric in [`MEMBERSHIP_GLOBAL`] and is otherwise plain equality.
    #[test]
    fn membership_matches_on_either_side_being_global_or_on_equality() {
        assert!(membership_matches(MEMBERSHIP_GLOBAL, MEMBERSHIP_GLOBAL));
        assert!(membership_matches(MEMBERSHIP_GLOBAL, 7));
        assert!(membership_matches(7, MEMBERSHIP_GLOBAL));
        assert!(membership_matches(7, 7));
        assert!(membership_matches(MembershipId::MAX, MembershipId::MAX));
        assert!(!membership_matches(7, 8));
        assert!(!membership_matches(1, MembershipId::MAX));
    }

    /// An unbinnable position fails open on **distance** only. Its membership is a declaration that
    /// did not fail, so a `NaN`-positioned body in another world stays out of this observer's set.
    #[test]
    fn a_nonfinite_position_fails_open_on_distance_but_not_on_membership() {
        let cfg = cfg(32.0, 100.0, 1.25, 0);
        let candidates = [
            InterestCandidate::anchored_in(1, [f32::NAN, 0.0, 0.0], 1),
            InterestCandidate::anchored_in(2, [f32::NAN, 0.0, 0.0], 2),
            InterestCandidate::anchored(3, [f32::INFINITY, 0.0, 0.0]),
        ];
        assert_eq!(
            members_of(&mut PeerInterest::new(), &cfg, 1, &candidates),
            vec![1, 3]
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
