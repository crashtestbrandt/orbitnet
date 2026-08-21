//! Interest-pass cost measurement — the decision harness for adopting the grid.
//!
//! This file exists to answer one question with a number rather than an opinion: **is adopting
//! [`InterestGrid`] actually faster than the linear scan it replaces, at the entity counts a real
//! session runs?** Both core paths compute the same sets and report the same leaves, so wiring the
//! grid in is only worth doing if it is faster; otherwise it is a refactor wearing an
//! optimisation's clothes.
//!
//! Four variants are timed over the same synthetic session:
//!
//! * `legacy` — the shape `orbit_net.rs` shipped first: per peer, a nested scan to find
//!   that peer's anchor body, then a linear distance pass over every entity, membership in a
//!   `HashSet`. O(P·N) with an O(N) inner lookup.
//! * `prepass` — one pass over the entities per tick builds the `peer → anchor` map, then the same
//!   linear distance pass per peer. This isolates the *restructure* from the *grid*.
//! * `scan` — the shipped core call, [`PeerInterest::update_linear_into`], over the same prepass.
//! * `grid` — the prepass plus [`InterestGrid`] and [`PeerInterest::update_grid_into`], cell size
//!   derived from the radius.
//!
//! `scan` against `grid` is the comparison the decision rests on; `legacy` and `prepass` are kept
//! because they are what the restructure was measured against.
//!
//! Three sweeps: by session scale, by arena extent, and by world count. The decision and both
//! result tables live in `interest.rs`'s module header, next to the code they govern.
//!
//! What this harness deliberately does NOT measure is the half that dominates in the real backend:
//! `legacy` calls `input_owner_peer()` — a Godot `get_multiplayer_authority()` round trip — once
//! per entity *per peer*, while every other variant calls it once per entity per tick. That is a
//! P-fold reduction in engine calls no pure-Rust bench can show, and it is why the prepass is worth
//! doing regardless of how the grid measures.
//!
//! Ignored by default so `cargo test` stays fast; run it with:
//!
//! ```text
//! cargo test -p orbitnet-core --release --test interest_bench -- --ignored --nocapture
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
    peers: Vec<i32>,
    rng: u32,
    extent: f32,
}

impl Scene {
    fn new(peers: usize, entities: usize, extent: f32, seed: u32) -> Self {
        Self::with_worlds(peers, entities, extent, 1, seed)
    }

    /// `worlds` independent worlds sharing one session, each **rebased on its own origin** — so
    /// unrelated entities sit at the same coordinates and only the membership separates them.
    /// That is the arrangement the feature exists for, and the one a single flat scan pays for
    /// most: every peer measures every other world's entities before refusing them.
    ///
    /// Entities and peers are dealt round-robin, so each world holds `entities / worlds` bodies
    /// and `peers / worlds` observers.
    fn with_worlds(peers: usize, entities: usize, extent: f32, worlds: usize, seed: u32) -> Self {
        let mut rng = seed;
        let mut ents = Vec::with_capacity(entities);
        let mut owners = Vec::with_capacity(entities);
        let mut memberships = Vec::with_capacity(entities);
        for index in 0..entities {
            let pos = [
                coord(&mut rng, extent),
                coord(&mut rng, extent * 0.1),
                coord(&mut rng, extent),
            ];
            ents.push((index as BodyId + 1, pos));
            // The first `peers` entities are the player bodies; everything after is unowned.
            owners.push(if index < peers { index as i32 + 2 } else { 0 });
            memberships.push(if worlds <= 1 {
                MEMBERSHIP_GLOBAL
            } else {
                (index % worlds) as MembershipId + 1
            });
        }
        Self {
            entities: ents,
            owners,
            memberships,
            peers: (0..peers).map(|i| i as i32 + 2).collect(),
            rng,
            extent,
        }
    }

    /// Refill `out` with one anchored candidate per entity — the input both core paths take.
    fn candidates_into(&self, out: &mut Vec<InterestCandidate>) {
        out.clear();
        out.extend(
            self.entities
                .iter()
                .zip(&self.memberships)
                .map(|(&(id, pos), &membership)| {
                    InterestCandidate::anchored_in(id, pos, membership)
                }),
        );
    }

    /// Where each peer observes from and which world it is in, gathered once per tick — the
    /// prepass both core paths share.
    fn observers_into(&self, out: &mut HashMap<i32, ([f32; 3], MembershipId)>) {
        out.clear();
        for (index, entry) in self.entities.iter().enumerate() {
            let owner = self.owners[index];
            if owner != 0 {
                out.insert(owner, (entry.1, self.memberships[index]));
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

/// Caller-owned working storage, so a variant is timed doing the filter rather than allocating.
#[derive(Default)]
struct Buffers {
    candidates: Vec<InterestCandidate>,
    observers: HashMap<i32, ([f32; 3], MembershipId)>,
    scratch: Vec<(BodyId, f32)>,
    leaves: Vec<BodyId>,
}

/// The shipped core path: the prepass plus one `update_linear_into` per peer.
fn tick_linear(
    scene: &Scene,
    sets: &mut HashMap<i32, PeerInterest>,
    buf: &mut Buffers,
    cfg: &AoiConfig,
) -> usize {
    scene.candidates_into(&mut buf.candidates);
    scene.observers_into(&mut buf.observers);
    let mut total = 0;
    for &peer in &scene.peers {
        let Some(&(center, observer)) = buf.observers.get(&peer) else {
            continue;
        };
        let set = sets.entry(peer).or_default();
        set.update_linear_into(
            cfg,
            center,
            observer,
            &buf.candidates,
            &mut buf.scratch,
            &mut buf.leaves,
        );
        total += set.len();
    }
    total
}

/// The prepass plus one grid rebuild per tick and one `update_grid_into` per peer.
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
        let Some(&(center, observer)) = buf.observers.get(&peer) else {
            continue;
        };
        let set = sets.entry(peer).or_default();
        set.update_grid_into(
            grid,
            cfg,
            center,
            observer,
            &[],
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
/// The two variants here are the two **core** paths, [`PeerInterest::update_linear_into`] against
/// [`PeerInterest::update_grid_into`], rather than the `prepass` sketch the first sweep uses. They
/// apply identical rules and report identical leaves, so the ratio is the whole difference between
/// adopting one and adopting the other.
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

    println!();
    println!(
        "grid crossover, {PEERS} peers / {ENTITIES} entities, radius {RADIUS} m, {TICKS} ticks"
    );
    println!(
        "{:>8} {:>7} | {:>12} {:>12} | {:>10} {:>9}",
        "extent", "in-set", "scan us/t", "grid us/t", "vs scan", "verdict"
    );

    for &extent in EXTENTS {
        let cfg = grid_cfg(RADIUS, 0);

        let mut scene = Scene::new(PEERS, ENTITIES, extent, 0x1234_5678);
        let mut linear_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut buf = Buffers::default();
        let mut linear_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            linear_sum += tick_linear(&scene, &mut linear_sets, &mut buf, &cfg);
        }
        let linear_ns = started.elapsed().as_nanos();

        scene = Scene::new(PEERS, ENTITIES, extent, 0x1234_5678);
        let mut grid = InterestGrid::new();
        let mut grid_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut grid_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            grid_sum += tick_grid(&scene, &mut grid, &mut grid_sets, &mut buf, &cfg);
        }
        let grid_ns = started.elapsed().as_nanos();

        let linear_us = micros_per_tick(TICKS, linear_ns);
        let grid_us = micros_per_tick(TICKS, grid_ns);
        let ratio = linear_us / grid_us;
        let mean_set = linear_sum as f64 / f64::from(TICKS) / PEERS as f64;
        println!(
            "{extent:>8.0} {mean_set:>7.0} | {linear_us:>12.1} {grid_us:>12.1} | {ratio:>9.2}x \
             {:>9}",
            if ratio > 1.0 { "grid" } else { "scan" }
        );
        assert_eq!(
            linear_sum, grid_sum,
            "the two variants computed different sets"
        );
    }
    println!();
}

/// Both sweeps above model one world. Several independent worlds in one session is the case the
/// grid was expected to win: the entity count rises with every world added while each peer's radius
/// still covers only its own, so the mean set per peer stays flat as `N` grows. The flat scan
/// measures every other world's entities before refusing them on membership; the grid never reads
/// their cells at all.
///
/// The sweep holds the **total** entity count fixed and splits it across more and more worlds, so
/// the only thing changing is how much of the session each peer is entitled to see. Every world is
/// rebased on its own origin, which is what makes this different from spreading one world wider:
/// the coordinates overlap exactly, and nothing but the membership separates them.
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

    println!();
    println!(
        "worlds, {PEERS} peers / {ENTITIES} entities total, arena +/-{EXTENT} m, radius {RADIUS} m"
    );
    println!(
        "{:>7} {:>7} | {:>12} {:>12} | {:>10} {:>9}",
        "worlds", "in-set", "scan us/t", "grid us/t", "vs scan", "verdict"
    );

    for &worlds in WORLDS {
        let cfg = grid_cfg(RADIUS, 0);

        let mut scene = Scene::with_worlds(PEERS, ENTITIES, EXTENT, worlds, 0x1234_5678);
        let mut linear_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut buf = Buffers::default();
        let mut linear_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            linear_sum += tick_linear(&scene, &mut linear_sets, &mut buf, &cfg);
        }
        let linear_ns = started.elapsed().as_nanos();

        scene = Scene::with_worlds(PEERS, ENTITIES, EXTENT, worlds, 0x1234_5678);
        let mut grid = InterestGrid::new();
        let mut grid_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut grid_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            grid_sum += tick_grid(&scene, &mut grid, &mut grid_sets, &mut buf, &cfg);
        }
        let grid_ns = started.elapsed().as_nanos();

        let linear_us = micros_per_tick(TICKS, linear_ns);
        let grid_us = micros_per_tick(TICKS, grid_ns);
        let ratio = linear_us / grid_us;
        let mean_set = linear_sum as f64 / f64::from(TICKS) / PEERS as f64;
        println!(
            "{worlds:>7} {mean_set:>7.0} | {linear_us:>12.1} {grid_us:>12.1} | {ratio:>9.2}x \
             {:>9}",
            if ratio > 1.0 { "grid" } else { "scan" }
        );
        assert_eq!(
            linear_sum, grid_sum,
            "the two paths computed different sets at {worlds} worlds"
        );
    }
    println!();
}
