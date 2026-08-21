//! Interest-pass cost measurement — the decision harness for adopting the grid.
//!
//! This file exists to answer one question with a number rather than an opinion: **is adopting
//! [`InterestGrid`] actually faster than the linear scan it replaces, at the entity counts a real
//! session runs?** Both core paths compute the same sets and report the same leaves, so wiring the
//! grid in is only worth doing if it is faster; otherwise it is a refactor wearing an
//! optimisation's clothes.
//!
//! Five variants are timed over the same synthetic session. The three the decision rests on:
//!
//! * `scan/peer` — **what ships**. Per peer, a fresh candidate list, then
//!   [`PeerInterest::update_linear_into`]. The rebuild is inside the loop because a peer's own body
//!   is `always` to that peer alone, so the list cannot be shared as it stands: O(P·N) per tick on
//!   top of the filter.
//! * `scan/shared` — the same filter over **one** candidate list per tick, with that one row
//!   patched in and out around each call. Needs no grid, so whatever it recovers is not evidence
//!   for one.
//! * `grid` — [`InterestGrid`] rebuilt once per tick plus [`PeerInterest::update_grid_into`] per
//!   peer, the own body handed over as the `also` override. Cell size derived from the radius.
//!
//! Reading the three together is the point: `scan/peer` against `grid` says whether adopting the
//! grid would lower `interest_ms` today, and `scan/shared` against `grid` says how much of that is
//! the grid rather than the rebuild it happens to delete.
//!
//! Two more are kept because they are what the earlier restructure was measured against:
//!
//! * `legacy` — the shape `orbit_net.rs` shipped first: per peer, a nested scan to find that peer's
//!   anchor body, then a linear distance pass over every entity, membership in a `HashSet`. O(P·N)
//!   with an O(N) inner lookup.
//! * `prepass` — one pass over the entities per tick builds the `peer → anchor` map, then the same
//!   linear distance pass per peer.
//!
//! Three sweeps: by session scale, by arena extent, and by world count. The decision and the result
//! tables live in `interest.rs`'s module header, next to the code they govern.
//!
//! What this harness deliberately does NOT measure is the half that dominates in the real backend:
//! `legacy` calls `input_owner_peer()` — a Godot `get_multiplayer_authority()` round trip — once
//! per entity *per peer*, while every other variant calls it once per entity per tick. That is a
//! P-fold reduction in engine calls no pure-Rust bench can show, and it is why the prepass is worth
//! doing regardless of how the grid measures.
//!
//! Ignored by default so `cargo test` stays fast. **`--test-threads=1` is not optional**: all three
//! sweeps are timing loops, and run concurrently they contend for the same cores and inflate every
//! figure by around 12%.
//!
//! ```text
//! cargo test -p orbitnet-core --release --test interest_bench -- --ignored --nocapture \
//!   --test-threads=1
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use orbitnet_core::history::BodyId;
use orbitnet_core::{
    AoiConfig, InterestCandidate, InterestGrid, MembershipId, PeerInterest, MEMBERSHIP_GLOBAL,
};

/// Deterministic LCG — the same one the codec and interest suites use. No dev-dependency.
fn lcg(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state
}

/// A float in `[-half, half)`.
fn coord(state: &mut u32, half: f32) -> f32 {
    (lcg(state) >> 8) as f32 / 16_777_216.0 * (half * 2.0) - half
}

/// One synthetic session: `peers` player bodies plus filler entities, all inside `extent` metres.
struct Scene {
    /// `(id, position)` for every replicated entity, players first.
    entities: Vec<(BodyId, [f32; 3])>,
    /// `entities[i]`'s input owner, or `0` for an unowned body (NPC, hazard, state channel).
    owners: Vec<i32>,
    /// `entities[i]`'s world. All `MEMBERSHIP_GLOBAL` unless the scene was built with worlds.
    memberships: Vec<MembershipId>,
    /// `false` for a row that declares no anchor — a positionless state channel (health, a door),
    /// which is always-relevant within its world. `orbit_net.rs` offers those as `always_in`.
    anchored: Vec<bool>,
    peers: Vec<i32>,
    rng: u32,
    extent: f32,
}

impl Scene {
    /// One world, every row anchored — the shape the `legacy` and `prepass` sketches understand.
    fn new(peers: usize, entities: usize, extent: f32, seed: u32) -> Self {
        Self::session(peers, entities, extent, 1, 0, seed)
    }

    /// `worlds` independent worlds sharing one session, each **rebased on its own origin** — so
    /// unrelated entities sit at the same coordinates and only the membership separates them.
    /// That is the arrangement membership exists for, and the one a flat scan pays for most: every
    /// peer measures every other world's entities before refusing them.
    ///
    /// Entities and peers are dealt round-robin, so each world holds `entities / worlds` bodies
    /// and `peers / worlds` observers.
    ///
    /// One unowned row in `unanchored_every` declares no anchor (`0` for none). Those reach every
    /// peer in their world at any distance, so they are what fills the grid's uncullable list and
    /// what a scan cannot skip either — leaving them out measures a session no caller produces.
    fn session(
        peers: usize,
        entities: usize,
        extent: f32,
        worlds: usize,
        unanchored_every: usize,
        seed: u32,
    ) -> Self {
        let mut rng = seed;
        let mut ents = Vec::with_capacity(entities);
        let mut owners = Vec::with_capacity(entities);
        let mut memberships = Vec::with_capacity(entities);
        let mut anchored = Vec::with_capacity(entities);
        for index in 0..entities {
            let pos = [
                coord(&mut rng, extent),
                coord(&mut rng, extent * 0.1),
                coord(&mut rng, extent),
            ];
            ents.push((index as BodyId + 1, pos));
            // The first `peers` entities are the player bodies; everything after is unowned.
            let player = index < peers;
            owners.push(if player { index as i32 + 2 } else { 0 });
            memberships.push(if worlds <= 1 {
                MEMBERSHIP_GLOBAL
            } else {
                (index % worlds) as MembershipId + 1
            });
            anchored.push(player || unanchored_every == 0 || index % unanchored_every != 0);
        }
        Self {
            entities: ents,
            owners,
            memberships,
            anchored,
            peers: (0..peers).map(|i| i as i32 + 2).collect(),
            rng,
            extent,
        }
    }

    /// One candidate per entity, with no peer named — the shared list a grid is rebuilt from, and
    /// the one `scan/shared` patches a single entry of per peer.
    fn candidates_into(&self, out: &mut Vec<InterestCandidate>) {
        out.clear();
        out.extend(
            self.entities
                .iter()
                .enumerate()
                .map(|(index, &(id, pos))| self.candidate(index, id, pos, 0)),
        );
    }

    /// How one row is offered to one peer, mirroring `orbit_net.rs`'s `candidate_for_row`: the
    /// peer's own body is `always` in every world, an anchored row is distance-culled within its
    /// own, and a row with no anchor is `always` within its own. `peer` of `0` names no peer.
    fn candidate(&self, index: usize, id: BodyId, pos: [f32; 3], peer: i32) -> InterestCandidate {
        if peer != 0 && self.owners[index] == peer {
            InterestCandidate::always(id)
        } else if self.anchored[index] {
            InterestCandidate::anchored_in(id, pos, self.memberships[index])
        } else {
            InterestCandidate::always_in(id, self.memberships[index])
        }
    }

    /// Where each peer observes from, which world it is in, and which row supplied both — gathered
    /// once per tick, the prepass every core variant shares.
    fn observers_into(&self, out: &mut HashMap<i32, Observer>) {
        out.clear();
        for (index, entry) in self.entities.iter().enumerate() {
            let owner = self.owners[index];
            if owner != 0 {
                out.insert(
                    owner,
                    Observer {
                        index,
                        center: entry.1,
                        membership: self.memberships[index],
                    },
                );
            }
        }
    }

    /// Jitter every position a little, so the interest sets actually churn between ticks.
    fn step(&mut self) {
        for entry in &mut self.entities {
            entry.1[0] = (entry.1[0] + coord(&mut self.rng, 2.0)).clamp(-self.extent, self.extent);
            entry.1[2] = (entry.1[2] + coord(&mut self.rng, 2.0)).clamp(-self.extent, self.extent);
        }
    }
}

/// The original filter: a nested anchor lookup plus a linear distance pass, per peer.
fn tick_legacy(scene: &Scene, sets: &mut HashMap<i32, HashSet<BodyId>>, radius: f32) -> usize {
    let enter_sq = radius * radius;
    let exit_sq = enter_sq * 1.25 * 1.25;
    let mut total = 0;
    for &peer in &scene.peers {
        // The nested O(N) anchor lookup the backend originally ran.
        let mut center: Option<[f32; 3]> = None;
        for (index, entry) in scene.entities.iter().enumerate() {
            if scene.owners[index] == peer {
                center = Some(entry.1);
                break;
            }
        }
        let Some(center) = center else { continue };
        let set = sets.entry(peer).or_default();
        for (index, &(id, pos)) in scene.entities.iter().enumerate() {
            if scene.owners[index] == peer {
                set.insert(id);
                continue;
            }
            let dx = f64::from(pos[0] - center[0]);
            let dy = f64::from(pos[1] - center[1]);
            let dz = f64::from(pos[2] - center[2]);
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if set.contains(&id) {
                if dist_sq > f64::from(exit_sq) {
                    set.remove(&id);
                }
            } else if dist_sq <= f64::from(enter_sq) {
                set.insert(id);
            }
        }
        total += set.len();
    }
    total
}

/// One pass builds the anchor map; the distance pass is still linear.
fn tick_prepass(
    scene: &Scene,
    sets: &mut HashMap<i32, BTreeMap<BodyId, f32>>,
    radius: f32,
) -> usize {
    let mut anchors: HashMap<i32, [f32; 3]> = HashMap::with_capacity(scene.peers.len());
    for (index, entry) in scene.entities.iter().enumerate() {
        let owner = scene.owners[index];
        if owner != 0 {
            anchors.insert(owner, entry.1);
        }
    }
    let enter_sq = radius * radius;
    let exit_sq = enter_sq * 1.25 * 1.25;
    let mut total = 0;
    for &peer in &scene.peers {
        let Some(&center) = anchors.get(&peer) else {
            continue;
        };
        let set = sets.entry(peer).or_default();
        for &(id, pos) in &scene.entities {
            let dx = pos[0] - center[0];
            let dy = pos[1] - center[1];
            let dz = pos[2] - center[2];
            let dist_sq = dx * dx + dy * dy + dz * dz;
            let member = set.contains_key(&id);
            if member {
                if dist_sq > exit_sq {
                    set.remove(&id);
                }
            } else if dist_sq <= enter_sq {
                set.insert(id, dist_sq);
            }
        }
        total += set.len();
    }
    total
}

/// Where one peer observes from, which world it is in, and which row said so.
#[derive(Clone, Copy)]
struct Observer {
    index: usize,
    center: [f32; 3],
    membership: MembershipId,
}

/// Caller-owned working storage, so a variant is timed doing the filter rather than allocating.
#[derive(Default)]
struct Buffers {
    candidates: Vec<InterestCandidate>,
    observers: HashMap<i32, Observer>,
    scratch: Vec<(BodyId, f32)>,
    leaves: Vec<BodyId>,
}

/// **The shipped shape**: the prepass, then per peer a fresh candidate list and one
/// `update_linear_into` (`orbit_net.rs`'s `update_interest`).
///
/// The per-peer rebuild is not incidental. `candidate_for_row` takes the peer id, because that
/// peer's own body is `always` to it and to nobody else, so the list cannot be shared as it
/// stands — which makes this pass O(P·N) per tick, on top of the filter it feeds. Measuring the
/// filter without it charges the scan for less work than the backend does.
fn tick_scan_per_peer(
    scene: &Scene,
    sets: &mut HashMap<i32, PeerInterest>,
    buf: &mut Buffers,
    cfg: &AoiConfig,
) -> usize {
    scene.observers_into(&mut buf.observers);
    let mut total = 0;
    for &peer in &scene.peers {
        let Some(&observer) = buf.observers.get(&peer) else {
            continue;
        };
        buf.candidates.clear();
        buf.candidates.extend(
            scene
                .entities
                .iter()
                .enumerate()
                .map(|(index, &(id, pos))| scene.candidate(index, id, pos, peer)),
        );
        let set = sets.entry(peer).or_default();
        set.update_linear_into(
            cfg,
            observer.center,
            observer.membership,
            &buf.candidates,
            &mut buf.scratch,
            &mut buf.leaves,
        );
        total += set.len();
    }
    total
}

/// The same scan over **one** candidate list per tick, with the peer's own body patched in and out
/// around each call.
///
/// This isolates the two savings a grid adoption would collect at once. Dropping the per-peer
/// rebuild needs no grid — the one row that varies per peer can be swapped in place — so whatever
/// this variant recovers is not evidence for the grid, and whatever the grid beats *this* by is.
fn tick_scan_shared(
    scene: &Scene,
    sets: &mut HashMap<i32, PeerInterest>,
    buf: &mut Buffers,
    cfg: &AoiConfig,
) -> usize {
    scene.candidates_into(&mut buf.candidates);
    scene.observers_into(&mut buf.observers);
    let mut total = 0;
    for &peer in &scene.peers {
        let Some(&observer) = buf.observers.get(&peer) else {
            continue;
        };
        let shared = buf.candidates[observer.index];
        buf.candidates[observer.index] = InterestCandidate::always(shared.id);
        let set = sets.entry(peer).or_default();
        set.update_linear_into(
            cfg,
            observer.center,
            observer.membership,
            &buf.candidates,
            &mut buf.scratch,
            &mut buf.leaves,
        );
        buf.candidates[observer.index] = shared;
        total += set.len();
    }
    total
}

/// The prepass plus one grid rebuild per tick and one `update_grid_into` per peer, with the peer's
/// own body handed over as the `also` override — the fact a grid shared by every peer cannot hold.
fn tick_grid(
    scene: &Scene,
    grid: &mut InterestGrid,
    sets: &mut HashMap<i32, PeerInterest>,
    buf: &mut Buffers,
    cfg: &AoiConfig,
) -> usize {
    scene.candidates_into(&mut buf.candidates);
    scene.observers_into(&mut buf.observers);
    grid.rebuild(cfg, &buf.candidates);
    let mut total = 0;
    for &peer in &scene.peers {
        let Some(&observer) = buf.observers.get(&peer) else {
            continue;
        };
        let own = [InterestCandidate::always(scene.entities[observer.index].0)];
        let set = sets.entry(peer).or_default();
        set.update_grid_into(
            grid,
            cfg,
            observer.center,
            observer.membership,
            &own,
            &mut buf.scratch,
            &mut buf.leaves,
        );
        total += set.len();
    }
    total
}

/// The cell size both sweeps derive from the query radius: a scan rectangle of ~11x11 cells at the
/// exit radius, rather than the 21x21 the fixed 32 m default would produce.
fn grid_cfg(radius: f32, max_entities: usize) -> AoiConfig {
    AoiConfig {
        cell_size: (radius / 4.0).max(1.0),
        enter_radius: radius,
        exit_factor: 1.25,
        max_entities,
    }
}

/// Microseconds per tick, plus the checksum that proves the variant did the work.
fn micros_per_tick(ticks: u32, elapsed_ns: u128) -> f64 {
    elapsed_ns as f64 / f64::from(ticks) / 1000.0
}

/// One row of the three-variant comparison: the timings, and the set sizes that prove all three
/// computed the same membership.
struct CoreRow {
    per_peer_ns: u128,
    shared_ns: u128,
    grid_ns: u128,
    set_sum: usize,
}

impl CoreRow {
    fn print(&self, label: &str, ticks: u32, peers: usize) {
        let per_peer = micros_per_tick(ticks, self.per_peer_ns);
        let shared = micros_per_tick(ticks, self.shared_ns);
        let grid = micros_per_tick(ticks, self.grid_ns);
        let mean_set = self.set_sum as f64 / f64::from(ticks) / peers as f64;
        println!(
            "{label} {mean_set:>7.0} | {per_peer:>13.1} {shared:>13.1} {grid:>11.1} | \
             {:>8.2}x {:>10.2}x",
            per_peer / grid,
            shared / grid,
        );
    }
}

fn print_core_header(first: &str) {
    println!(
        "{first:>8} {:>7} | {:>13} {:>13} {:>11} | {:>9} {:>11}",
        "in-set", "scan/peer us/t", "scan/shared", "grid us/t", "vs shipped", "vs shared"
    );
}

/// Time the three core variants over the same session, and refuse to report if they disagree.
///
/// `build` is called once per variant so each starts from an identical scene and walks the same
/// jitter — the three timings then describe the same work, which is the only thing that makes the
/// ratios mean anything.
fn core_row(build: impl Fn() -> Scene, ticks: u32, cfg: &AoiConfig) -> CoreRow {
    let mut buf = Buffers::default();

    let mut scene = build();
    let mut sets: HashMap<i32, PeerInterest> = HashMap::new();
    let mut per_peer_sum = 0usize;
    let started = Instant::now();
    for _ in 0..ticks {
        scene.step();
        per_peer_sum += tick_scan_per_peer(&scene, &mut sets, &mut buf, cfg);
    }
    let per_peer_ns = started.elapsed().as_nanos();

    scene = build();
    let mut sets: HashMap<i32, PeerInterest> = HashMap::new();
    let mut shared_sum = 0usize;
    let started = Instant::now();
    for _ in 0..ticks {
        scene.step();
        shared_sum += tick_scan_shared(&scene, &mut sets, &mut buf, cfg);
    }
    let shared_ns = started.elapsed().as_nanos();

    scene = build();
    let mut grid = InterestGrid::new();
    let mut sets: HashMap<i32, PeerInterest> = HashMap::new();
    let mut grid_sum = 0usize;
    let started = Instant::now();
    for _ in 0..ticks {
        scene.step();
        grid_sum += tick_grid(&scene, &mut grid, &mut sets, &mut buf, cfg);
    }
    let grid_ns = started.elapsed().as_nanos();

    assert_eq!(
        per_peer_sum, shared_sum,
        "patching the shared list in place changed the sets"
    );
    assert_eq!(
        per_peer_sum, grid_sum,
        "the grid and the scan computed different sets"
    );
    assert!(grid_sum > 0, "the scene produced no interest at all");
    CoreRow {
        per_peer_ns,
        shared_ns,
        grid_ns,
        set_sum: grid_sum,
    }
}

#[test]
#[ignore = "measurement harness; run with --ignored --nocapture"]
fn interest_pass_cost_by_scale() {
    // (peers, entities). The last two rows are the 100-player target; the first three are the
    // scales a session actually runs at today.
    const CASES: &[(usize, usize)] = &[
        (4, 40),
        (8, 80),
        (16, 200),
        (32, 400),
        (64, 800),
        (100, 1200),
    ];
    const TICKS: u32 = 240;
    const RADIUS: f32 = 256.0;
    const EXTENT: f32 = 600.0;

    println!();
    println!("interest pass, {TICKS} ticks, radius {RADIUS} m, arena +/-{EXTENT} m");
    println!(
        "{:>6} {:>7} | {:>12} {:>12} {:>12} | {:>9} {:>9}",
        "peers", "ents", "legacy us/t", "prepass us/t", "grid us/t", "vs legacy", "vs prepass"
    );

    for &(peers, entities) in CASES {
        let base = Scene::new(peers, entities, EXTENT, 0x1234_5678);

        let mut scene = Scene::new(peers, entities, EXTENT, 0x1234_5678);
        let mut legacy_sets: HashMap<i32, HashSet<BodyId>> = HashMap::new();
        let mut legacy_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            legacy_sum += tick_legacy(&scene, &mut legacy_sets, RADIUS);
        }
        let legacy_ns = started.elapsed().as_nanos();

        scene = Scene::new(peers, entities, EXTENT, 0x1234_5678);
        let mut prepass_sets: HashMap<i32, BTreeMap<BodyId, f32>> = HashMap::new();
        let mut prepass_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            prepass_sum += tick_prepass(&scene, &mut prepass_sets, RADIUS);
        }
        let prepass_ns = started.elapsed().as_nanos();

        let cfg = grid_cfg(RADIUS, 0);
        scene = Scene::new(peers, entities, EXTENT, 0x1234_5678);
        let mut grid = InterestGrid::new();
        let mut grid_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut buf = Buffers::default();
        let mut grid_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            grid_sum += tick_grid(&scene, &mut grid, &mut grid_sets, &mut buf, &cfg);
        }
        let grid_ns = started.elapsed().as_nanos();

        let legacy_us = micros_per_tick(TICKS, legacy_ns);
        let prepass_us = micros_per_tick(TICKS, prepass_ns);
        let grid_us = micros_per_tick(TICKS, grid_ns);
        println!(
            "{peers:>6} {entities:>7} | {legacy_us:>12.1} {prepass_us:>12.1} {grid_us:>12.1} | \
             {:>8.2}x {:>9.2}x",
            legacy_us / grid_us,
            prepass_us / grid_us,
        );

        // The three variants must agree on the membership they compute, or the timings compare
        // different work. `prepass`/`grid` admit the peer's own body through the radius like any
        // other entity (it is at distance zero from itself), so the counts match exactly.
        assert_eq!(
            prepass_sum, grid_sum,
            "prepass and grid disagreed at {peers} peers / {entities} entities"
        );
        assert!(
            legacy_sum > 0 && grid_sum > 0,
            "the scene produced no interest at all — the bench is measuring nothing"
        );
        let _ = base.entities.len();
    }
    println!();
}

/// The first sweep holds the arena size fixed, which only answers the question for arenas that
/// size. A uniform grid can only win when the query radius covers a small fraction of the occupied
/// space — otherwise [`InterestGrid::query_within`]'s own guard finds the scan rectangle larger
/// than the occupancy and iterates every bucket, which *is* the linear scan, plus a rebuild. This
/// sweep finds the crossover: at what arena extent does the grid start to pay?
///
/// The three core variants run here, not the `prepass` sketch. All three apply identical rules and
/// report identical leaves — the runner asserts they compute the same sets — so the ratios are the
/// whole difference between adopting one and adopting another.
#[test]
#[ignore = "measurement harness; run with --ignored --nocapture"]
fn interest_grid_crossover_by_arena_extent() {
    const PEERS: usize = 64;
    const ENTITIES: usize = 800;
    const TICKS: u32 = 240;
    const RADIUS: f32 = 256.0;
    // 2fort's forts sit at +/-74 m and the container cube is 60 m on a side; the last entries are
    // hypothetical cislunar sprawl far beyond anything the game currently builds.
    const EXTENTS: &[f32] = &[300.0, 600.0, 1_200.0, 2_500.0, 5_000.0, 10_000.0, 25_000.0];
    // One unowned row in eight declares no anchor. A session with none is a session with no
    // positionless state channels at all, which is not one the backend produces.
    const UNANCHORED: usize = 8;

    println!();
    println!(
        "grid crossover, {PEERS} peers / {ENTITIES} entities, radius {RADIUS} m, {TICKS} ticks, \
         1 world, 1 unowned row in {UNANCHORED} positionless"
    );
    print_core_header("extent");

    for &extent in EXTENTS {
        let run = |worlds: usize| {
            core_row(
                || Scene::session(PEERS, ENTITIES, extent, worlds, UNANCHORED, 0x1234_5678),
                TICKS,
                &grid_cfg(RADIUS, 0),
            )
        };
        let row = run(1);
        row.print(&format!("{extent:>8.0}"), TICKS, PEERS);
    }
    println!();
}

/// Both sweeps above model one world. Several independent worlds in one session is the case the
/// grid was expected to win: a peer is entitled to a shrinking share of a session that keeps its
/// size. The flat scan measures every other world's entities before refusing them on membership;
/// the grid never reads their cells at all.
///
/// The sweep holds the **total** entity count fixed and splits it across more and more worlds, so
/// the only thing changing is how much of the session each peer may see — the mean set falls as the
/// world count rises, and the entity count the filter walks does not. Every world is rebased on its
/// own origin, which is what makes this different from spreading one world wider: the coordinates
/// overlap exactly, and nothing but the membership separates them.
///
/// Read the two ratio columns against each other here. They disagree, and the disagreement is the
/// finding.
#[test]
#[ignore = "measurement harness; run with --ignored --nocapture"]
fn interest_grid_by_world_count() {
    const PEERS: usize = 64;
    const ENTITIES: usize = 1_200;
    const TICKS: u32 = 240;
    const RADIUS: f32 = 256.0;
    // Each world is a 2fort-sized arena rebased on its own origin — a radius that covers all of it,
    // which is precisely the occupancy the crossover sweep finds the grid losing at.
    const EXTENT: f32 = 300.0;
    const WORLDS: &[usize] = &[1, 2, 4, 8, 16, 32];
    const UNANCHORED: usize = 8;

    println!();
    println!(
        "worlds, {PEERS} peers / {ENTITIES} entities total, arena +/-{EXTENT} m, radius {RADIUS} m, \
         1 unowned row in {UNANCHORED} positionless"
    );
    print_core_header("worlds");

    for &worlds in WORLDS {
        let row = core_row(
            || Scene::session(PEERS, ENTITIES, EXTENT, worlds, UNANCHORED, 0x1234_5678),
            TICKS,
            &grid_cfg(RADIUS, 0),
        );
        row.print(&format!("{worlds:>8}"), TICKS, PEERS);
    }
    println!();
}
