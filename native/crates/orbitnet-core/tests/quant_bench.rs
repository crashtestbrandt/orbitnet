//! What `@ss3` costs per encode — the figures `quant.rs`'s `SS3_SKIP_GAP` rests on.
//!
//! `quat_to_ss3` has to answer bytes that survive their own decode, because a sender stores the row
//! it captures and a receiver stores what it reconstructs. It proves that by decoding its own pack
//! and re-packing the result, and that proof is most of the call's cost. A separation gate in front
//! of it skips the proof whenever the largest component leads the next one by more than a decode can
//! close, which is the case for all but a few orientations in a thousand.
//!
//! Four arms, over the same encoder, separated by which path the orientation takes:
//!
//! * `skip` — separation at or above `SS3_SKIP_GAP`. Four `abs`, a compare, and the pack. **What a
//!   send pays now.**
//! * `verify` — separation between the tie band and the gate: the pack, a decode, a re-pack and a
//!   compare, all of which agree. **What every send paid before the gate**, since the orientations
//!   that now skip used to run exactly this and reach exactly this answer.
//! * `contested` — separation inside the tie band, where `dropped_index` treats two components as
//!   tied and the pack can disagree with its own decode. The gate declines and the encoder may walk
//!   the orbit for its least member. A few orientations in ten thousand, and the only family the
//!   gate cannot help.
//! * `random` — uniform random rotations, unfiltered. **What a session's mix actually costs**, gate
//!   and all.
//!
//! Read `random` against `verify`: that ratio is what the gate bought on a real population. Read
//! `contested` against `verify` for what the orientations it cannot help cost, and the skip rate for
//! how few of them there are.
//!
//! Ignored by default so `cargo test` stays fast, and `--test-threads=1` is not optional: the arms
//! are timing loops and run concurrently they contend for the same cores.
//!
//! ```text
//! cargo test -p orbitnet-core --release --test quant_bench -- --ignored --nocapture \
//!   --test-threads=1
//! ```

use std::time::Instant;

use orbitnet_core::quant::quat_to_ss3;

/// Orientations per arm. Large enough that the clock's resolution is not the measurement.
const ITERS: usize = 2_000_000;

/// Orientations built per arm and then cycled. Large enough that no branch predictor or cache line
/// carries an answer over from the previous iteration.
const POPULATION: usize = 4096;

/// 15-bit payload scale, and one quantum of it in component magnitude — `quant.rs`'s own constants,
/// which are private to that module. The arms below are defined in quanta of separation.
const SS3_SCALE: f32 = 16383.0;
const QUANTUM: f32 = std::f32::consts::FRAC_1_SQRT_2 / SS3_SCALE;

/// Deterministic LCG — the same one the codec and interest suites use. No dev-dependency.
fn lcg(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state
}

/// A float in `[0, 1)`.
fn unit(state: &mut u32) -> f32 {
    (lcg(state) >> 8) as f32 / 16_777_216.0
}

/// A uniformly distributed rotation (Shoemake's subgroup algorithm).
fn random_rotation(state: &mut u32) -> [f32; 4] {
    let (u1, u2, u3) = (unit(state), unit(state), unit(state));
    let (s1, s2) = ((1.0f32 - u1).sqrt(), u1.sqrt());
    let t1 = 2.0 * std::f32::consts::PI * u2;
    let t2 = 2.0 * std::f32::consts::PI * u3;
    [s1 * t1.sin(), s1 * t1.cos(), s2 * t2.sin(), s2 * t2.cos()]
}

fn normalize(q: [f32; 4]) -> [f32; 4] {
    let inv = q.iter().map(|c| c * c).sum::<f32>().sqrt().recip();
    [q[0] * inv, q[1] * inv, q[2] * inv, q[3] * inv]
}

/// Index of the strictly largest component, and by how much it leads the next one.
fn separation(q: [f32; 4]) -> (usize, f32) {
    let mut largest = 0usize;
    for i in 1..4 {
        if q[i].abs() > q[largest].abs() {
            largest = i;
        }
    }
    let mut runner = 0.0f32;
    for (i, c) in q.iter().enumerate() {
        if i != largest {
            runner = runner.max(c.abs());
        }
    }
    (largest, q[largest].abs() - runner)
}

/// Uniform random rotations whose separation lies in `[low, high)` quanta, so an arm can pin which
/// path through the encoder it is timing. `high` of `None` means unbounded.
fn population_between(low: f32, high: Option<f32>, state: &mut u32) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity(POPULATION);
    while out.len() < POPULATION {
        let q = normalize(random_rotation(state));
        let (_, gap) = separation(q);
        let quanta = gap / QUANTUM;
        if quanta >= low && high.is_none_or(|h| quanta < h) {
            out.push(q);
        }
    }
    out
}

/// Rotations whose separation lands inside the 8-quantum tie band, built rather than sampled: at
/// well under one draw in a thousand, rejection sampling this arm costs more than the arm does.
/// The runner-up is moved to a chosen distance below the largest and the result renormalized, which
/// scales both by the same factor and leaves the separation where it was put.
fn population_contested(state: &mut u32) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity(POPULATION);
    while out.len() < POPULATION {
        let mut q = normalize(random_rotation(state));
        let (largest, _) = separation(q);
        let runner = (largest + 1) % 4;
        q[runner] = q[largest].abs() - unit(state) * 8.0 * QUANTUM;
        out.push(normalize(q));
    }
    out
}

/// Time `ITERS` encodes over `population`, answering nanoseconds per call.
fn time_encodes(population: &[[f32; 4]]) -> f64 {
    let start = Instant::now();
    for i in 0..ITERS {
        let q = population[i % population.len()];
        std::hint::black_box(quat_to_ss3(std::hint::black_box(q)));
    }
    start.elapsed().as_secs_f64() * 1e9 / ITERS as f64
}

#[test]
#[ignore = "measurement harness; run with --ignored --nocapture"]
fn the_ss3_encoder_costs_per_call() {
    let mut state = 0x1234_5678u32;
    // The gate is 16 quanta. `verify` sits above the 8-quantum tie band so every one of its packs is
    // stable and the full check agrees, and below the gate so every one of them runs that check.
    let skip = population_between(16.0, None, &mut state);
    let verify = population_between(9.5, Some(16.0), &mut state);
    let contested = population_contested(&mut state);
    let random: Vec<[f32; 4]> = (0..POPULATION)
        .map(|_| normalize(random_rotation(&mut state)))
        .collect();

    // What share of a uniform random population clears the gate, measured over the same generator.
    let mut cleared = 0u32;
    for _ in 0..1_000_000 {
        let q = normalize(random_rotation(&mut state));
        if separation(q).1 / QUANTUM >= 16.0 {
            cleared += 1;
        }
    }

    println!();
    println!("@ss3 encode, {ITERS} calls per arm over {POPULATION} orientations");
    println!("{:>34}  {:>10}", "arm", "ns/call");
    let verify_ns = time_encodes(&verify);
    let skip_ns = time_encodes(&skip);
    let contested_ns = time_encodes(&contested);
    let random_ns = time_encodes(&random);
    println!(
        "{:>34}  {verify_ns:>10.1}",
        "verify (what a send paid before)"
    );
    println!("{:>34}  {skip_ns:>10.1}", "skip (the gate admits)");
    println!(
        "{:>34}  {contested_ns:>10.1}",
        "contested (inside the tie band)"
    );
    println!("{:>34}  {random_ns:>10.1}", "random (a session's mix)");
    println!();
    println!(
        "  {:.3}% of uniform random rotations clear the gate, and the mix runs {:.1}x the old call.",
        f64::from(cleared) / 1e6 * 100.0,
        verify_ns / random_ns
    );
}
