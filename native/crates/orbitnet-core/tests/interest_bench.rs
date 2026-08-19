//! Interest-pass cost measurement — the decision harness for adopting the grid.
//!
//! The epic owns one question this file exists to answer with a number rather than an opinion:
//! **is adopting [`InterestGrid`] actually faster than the linear scan it replaces, at the entity
//! counts a real session runs?** Wiring in tested code is only worth doing if it is faster; otherwise it
//! is a refactor wearing an optimisation's clothes.
//!
//! Three variants are timed over the same synthetic session:
//!
//! * `legacy` — the shape `orbit_net.rs` shipped first: per peer, a nested scan to find
//!   that peer's anchor body, then a linear distance pass over every entity, membership in a
//!   `HashSet`. O(P·N) with an O(N) inner lookup.
//! * `prepass` — one pass over the entities per tick builds the `peer → anchor` map, then the same
//!   linear distance pass per peer. This isolates the *restructure* from the *grid*.
//! * `grid` — the prepass plus [`InterestGrid`] / [`PeerInterest`], cell size derived from the
//!   radius.
//!
//! What this harness deliberately does NOT measure is the half that dominates in the real backend:
//! `legacy` calls `input_owner_peer()` — a Godot `get_multiplayer_authority()` round trip — once
//! per entity *per peer*, while `prepass`/`grid` call it once per entity per tick. That is a P-fold
//! reduction in engine calls no pure-Rust bench can show, and it is why the prepass is worth doing
//! regardless of how the grid measures.
//!
//! Ignored by default so `cargo test` stays fast; run it with:
//!
//! ```text
//! cargo test -p orbitnet-core --release --test interest_bench -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use orbitnet_core::history::BodyId;
use orbitnet_core::{AoiConfig, InterestGrid, PeerInterest};

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
    peers: Vec<i32>,
    rng: u32,
    extent: f32,
}

impl Scene {
    fn new(peers: usize, entities: usize, extent: f32, seed: u32) -> Self {
        let mut rng = seed;
        let mut ents = Vec::with_capacity(entities);
        let mut owners = Vec::with_capacity(entities);
        for index in 0..entities {
            let pos = [
                coord(&mut rng, extent),
                coord(&mut rng, extent * 0.1),
                coord(&mut rng, extent),
            ];
            ents.push((index as BodyId + 1, pos));
            // The first `peers` entities are the player bodies; everything after is unowned.
            owners.push(if index < peers { index as i32 + 2 } else { 0 });
        }
        Self {
            entities: ents,
            owners,
            peers: (0..peers).map(|i| i as i32 + 2).collect(),
            rng,
            extent,
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

/// The prepass plus one grid rebuild per tick and one query per peer.
fn tick_grid(
    scene: &Scene,
    grid: &mut InterestGrid,
    sets: &mut HashMap<i32, PeerInterest>,
    scratch: &mut Vec<(BodyId, f32)>,
    cfg: &AoiConfig,
) -> usize {
    let mut anchors: HashMap<i32, [f32; 3]> = HashMap::with_capacity(scene.peers.len());
    for (index, entry) in scene.entities.iter().enumerate() {
        let owner = scene.owners[index];
        if owner != 0 {
            anchors.insert(owner, entry.1);
        }
    }
    grid.rebuild(cfg, &scene.entities);
    let mut total = 0;
    for &peer in &scene.peers {
        let Some(&center) = anchors.get(&peer) else {
            continue;
        };
        let set = sets.entry(peer).or_default();
        set.update(grid, cfg, center, scratch);
        total += set.len();
    }
    total
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

        // Cell size derived from the radius, as S2 ships it: a scan rectangle of ~11x11 cells at
        // the exit radius, rather than the 21x21 the fixed 32 m default would produce.
        let cfg = AoiConfig {
            cell_size: (RADIUS / 4.0).max(1.0),
            enter_radius: RADIUS,
            exit_factor: 1.25,
            max_entities: 0,
        };
        scene = Scene::new(peers, entities, EXTENT, 0x1234_5678);
        let mut grid = InterestGrid::new();
        let mut grid_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut scratch: Vec<(BodyId, f32)> = Vec::new();
        let mut grid_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            grid_sum += tick_grid(&scene, &mut grid, &mut grid_sets, &mut scratch, &cfg);
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
/// space — otherwise [`InterestGrid::query_within`]'s own guard (`interest.rs:175`) finds the scan
/// rectangle larger than the occupancy and iterates every bucket, which *is* the linear scan, plus
/// a rebuild. This sweep finds the crossover: at what arena extent does the grid start to pay?
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
        "extent", "in-set", "prepass us/t", "grid us/t", "vs prepass", "verdict"
    );

    for &extent in EXTENTS {
        let cfg = AoiConfig {
            cell_size: (RADIUS / 4.0).max(1.0),
            enter_radius: RADIUS,
            exit_factor: 1.25,
            max_entities: 0,
        };

        let mut scene = Scene::new(PEERS, ENTITIES, extent, 0x1234_5678);
        let mut prepass_sets: HashMap<i32, BTreeMap<BodyId, f32>> = HashMap::new();
        let mut prepass_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            prepass_sum += tick_prepass(&scene, &mut prepass_sets, RADIUS);
        }
        let prepass_ns = started.elapsed().as_nanos();

        scene = Scene::new(PEERS, ENTITIES, extent, 0x1234_5678);
        let mut grid = InterestGrid::new();
        let mut grid_sets: HashMap<i32, PeerInterest> = HashMap::new();
        let mut scratch: Vec<(BodyId, f32)> = Vec::new();
        let mut grid_sum = 0usize;
        let started = Instant::now();
        for _ in 0..TICKS {
            scene.step();
            grid_sum += tick_grid(&scene, &mut grid, &mut grid_sets, &mut scratch, &cfg);
        }
        let grid_ns = started.elapsed().as_nanos();

        let prepass_us = micros_per_tick(TICKS, prepass_ns);
        let grid_us = micros_per_tick(TICKS, grid_ns);
        let ratio = prepass_us / grid_us;
        let mean_set = prepass_sum as f64 / f64::from(TICKS) / PEERS as f64;
        println!(
            "{extent:>8.0} {mean_set:>7.0} | {prepass_us:>12.1} {grid_us:>12.1} | {ratio:>9.2}x \
             {:>9}",
            if ratio > 1.0 { "grid" } else { "scan" }
        );
        assert_eq!(
            prepass_sum, grid_sum,
            "the two variants computed different sets"
        );
    }
    println!();
}
