//! Proving the receive path's tag compare does not leak how much of a guessed tag was right.
//!
//! `auth.rs` compares datagram tags by folding their difference to one bit and testing it once,
//! because a compare that returns at the first differing byte turns forging a tag from 2^64 guesses
//! into 8 x 256. Nothing enforced that. The source is branchless; a compiler is free to compile it
//! any way that preserves the result, and the two profiles this repository ships do not compile the
//! same code -- `template-debug` adds `debug-assertions` and `overflow-checks`, and it is the build
//! Godot loads for every run from source.
//!
//! This file is that enforcement, in three tests that fail for different reasons.
//!
//! | Test | What it asserts | Where it runs |
//! | --- | --- | --- |
//! | `the_tag_compare_compiles_without_a_branch` | the compare's own source, compiled on its own, emits no conditional branch and no call under either profile's flags | `just native-test`, every PR |
//! | `the_branch_scanner_reads_both_architectures` | the scanner's line parsing and four branch spellings, from x86-64 and AArch64 fixtures, on whichever host it runs | `just native-test`, every PR |
//! | `the_tag_compare_is_not_distinguishable_by_timing` | two refused datagrams differing in which tag byte is wrong are not separable by a Welch t-test | the nightly `constant-time` workflow on the self-hosted box, and `just native-timing` by hand |
//!
//! ## The codegen assertion, and why it needs no disassembler
//!
//! It reads `src/auth.rs`, extracts the text of `fn tags_equal` verbatim, writes it to a temporary
//! file under a `#[no_mangle]` wrapper, and compiles that with **`rustc --emit=asm`**. Anyone who can
//! build this repository already has `rustc`; a disassembler is not part of that toolchain. The
//! emitted assembly for the wrapper is scanned for conditional-branch and call mnemonics.
//!
//! - It asserts on the **shipped source text**, not on a copy. Rewriting the compare as a byte loop
//!   with an early return fails this test, because the rewritten text is what gets compiled.
//! - It compiles **twice**: `-C opt-level=3 -C codegen-units=1` alone, and again with
//!   `-C debug-assertions=on -C overflow-checks=on`. Those are the only two settings
//!   `[profile.template-debug]` changes from `[profile.release]`, so the two compiles are the two
//!   shipped profiles' flag sets applied to that text.
//! - It judges **the compare compiled on its own, which is a proxy for the form that ships**. The
//!   probe wraps the extracted text in an `extern "C"` function, so its two arguments arrive in
//!   registers and its result is returned; the shipped library contains no `tags_equal` symbol at
//!   all, because the fold is inlined into `SessionAuth::open`, where the tag arrives from a load
//!   off the datagram and the expected value from `SipHasher::finish`. The probe also omits the
//!   `lto = "thin"` that `[profile.release]` sets, since it compiles one crate. A rewrite that LLVM
//!   compiles branchless standalone and branchy once inlined at that call site would pass this
//!   test. Reading the inlined form takes the crate's own `--emit=asm` output, which this test does
//!   not do.
//! - It runs a **negative control** first: a deliberately leaky byte-at-a-time compare goes through
//!   the same pipeline and must be reported as branching. A scanner that does not understand the
//!   host's assembly fails there instead of passing everything, which is what makes the mnemonic
//!   lists safe to carry for two architectures rather than every architecture.
//! - It reads the **host's** assembly. Under `cargo test --target <other>` it still judges the host.
//!
//! ## The timing assertion, and why both classes are refused datagrams
//!
//! Both input classes are datagrams whose tag is **wrong**, differing only in which byte is wrong:
//! one in the first tag byte on the wire, one in the last. Comparing a matching datagram against a
//! non-matching one would measure nothing about the compare -- accept and refuse are public
//! outcomes, and their costs past the compare legitimately differ, because a datagram that verified
//! goes on to the replay window and one that did not returns immediately. The leak worth refusing is
//! between two refusals. That is also the only measurement an attacker can take, since it submits
//! guesses and every one of them is refused.
//!
//! Three arms, all through the same statistics:
//!
//! - **The null.** Two wrong tags that differ in the *same* byte, so no leak exists by
//!   construction. Whatever `|t|` this produces is the machine's noise, and it is where the
//!   tolerance comes from.
//! - **The negative control.** A mirror of `SessionAuth::open` -- same key, same SipHash over the
//!   same bytes -- with a byte-at-a-time compare that returns early. It must be detected, and by a
//!   margin, or the box is too noisy for the verdict to mean anything.
//! - **The shipped compare**, through `SessionAuth::open` itself. `tags_equal` is private, and the
//!   receive path is what actually runs per datagram.
//!
//! **Each run measures its own tolerance.** The null runs `CALIBRATIONS` times, half before the two
//! judged arms and half after, so drift over the measurement window lands inside the floor. The
//! tolerance is the largest `|t|` any of those runs produced, times [`SPREAD`]. The control must
//! then clear that tolerance by [`HEADROOM`] or the test fails as unresolvable rather than
//! passing.
//!
//! **The shipped arm is drawn [`JUDGED_DRAWS`] times and judged on the smallest draw.** The
//! tolerance is a maximum over ten null runs and the judged arm is a single draw from the same
//! noisy process, so one draw carries the full run-to-run spread with no leak present. The widest
//! draw recorded below reached 2.53 against a tolerance of 3.24, a 1.3x margin. A leak is
//! systematic and reproduces in every draw; noise does not.
//!
//! **Every arm's class order comes from one seed family** ([`shuffle_seed`]), so the orders the
//! null samples and the orders the judged arm samples are draws from the same distribution and the
//! floor bounds the judged arm.
//!
//! ## The numbers this was calibrated against
//!
//! Fifteen runs on one machine -- an Apple M-series laptop, not idle, with other builds on it --
//! under both profiles, 100000 samples per class of 128 opens each. One `open` over a 32-byte
//! datagram costs about 14 ns there. Nine of the fifteen predate [`JUDGED_DRAWS`] and are judged on
//! a single draw. **These are observations from one machine, not a claim about machines in
//! general.**
//!
//! | Arm | Across those fifteen runs |
//! | --- | --- |
//! | null, worst of 10 calibration runs | 1.55 to 3.37 |
//! | null, median of the same 10 | 1.07 to 1.80 |
//! | tolerance (worst null x [`SPREAD`]) | 3.11 to 6.75 |
//! | leaky control | 371 to 1340, i.e. 55x to 314x the tolerance |
//! | shipped compare, one draw | 0.60 to 2.72 |
//! | shipped compare, smallest of [`JUDGED_DRAWS`] | 0.60 to 1.85 |
//!
//! Four readings:
//!
//! - **The shipped compare is not distinguishable.** Every one of the fifteen runs put the judged
//!   value under that run's own tolerance, and the tightest margin was 1.9x. The two profiles read
//!   alike.
//! - **A single draw does cross the run's own worst null.** One of the draws above did, at 2.53
//!   against a worst null of 1.62 and a tolerance of 3.24, and a run on a second machine read 2.00
//!   against a worst null of 1.90 and a tolerance of 3.80. Neither crossed its tolerance. That
//!   spread is what [`JUDGED_DRAWS`] exists for, and it is why the header claims nothing about the
//!   judged arm staying under the *null* rather than under the tolerance.
//! - **The tolerance is worth having.** Its resolution came out between 0.006 and 0.19 ns per
//!   `open` depending on which crop won, against a 14 ns call. The control's actual leak measured
//!   1.3 to 1.9 ns per `open`, which is the cost of seven extra byte compares plus the `black_box`
//!   barrier that keeps them from being merged. A leak an order of magnitude smaller than the
//!   control's would still be caught.
//! - **The noise floor is stable enough to derive a tolerance from.** The worst-of-ten statistic
//!   stayed inside 1.55 to 3.37 across fifteen runs on a machine under load, which is what
//!   [`SPREAD`] is set against.
//!
//! Two defects found by running it, both worth knowing before touching the measurement:
//!
//! - **One buffer per class is wrong.** Two allocations at two addresses measured as systematically
//!   different at `|t|` = 34 with no leak present. One buffer, rewritten outside the timed window,
//!   is what `measure` does now.
//! - **`lcg(..) % bound` is wrong** for the class order. A power-of-two LCG's low bits are
//!   near-periodic, the resulting order correlated with position, drift landed on one class, and the
//!   null read `|t|` = 39. [`below`] uses the high bits.
//!
//! ## Why the timing test is not in `just check`
//!
//! It is a statistical measurement and its verdict is only as good as the machine's noise floor. A
//! shared GitHub-hosted runner is a noisy virtual machine with neighbours, which is where the
//! tolerance inflates until the test passes on anything, or the floor moves mid-run and it fails on
//! nothing. So it is `#[ignore]`d and `just native-timing` runs it under both shipped profiles. As
//! a PR gate it would flake.
//!
//! **A nightly runs it on the self-hosted Linux box**, which is the only machine in this project's
//! CI whose noise floor the measurement can use. `.github/workflows/constant-time.yml` runs
//! `just native-timing` there on a schedule, off a pull request entirely, and its header carries the
//! rest of the reasoning. What that job does with each outcome:
//!
//! | Outcome | The job |
//! | --- | --- |
//! | both profiles measured, judged draw under the tolerance | green |
//! | the control fell short of the [`HEADROOM`] margin, so no verdict was rendered | a warning, after one retry, and green |
//! | the judged arm cleared the tolerance in every draw on a resolvable run | red |
//! | no harness output at all | red |
//!
//! A noisy box leaving a warning rather than a red build is deliberate. This test invalidates itself
//! when it cannot resolve the control, so a busy night produces no claim rather than a false pass,
//! and a nightly that goes red because the box was busy stops being read.
//!
//! Run it by hand on an idle machine with:
//!
//! ```text
//! just native-timing
//! ```
//!
//! ## What none of the three proves
//!
//! - Nothing here says anything about the **rest** of the receive path. SipHash-2-4 over a fixed
//!   number of bytes is data-independent by construction, and the replay window's cost depends on a
//!   sequence number that is on the wire in the clear anyway.
//! - A CPU is free to be data-dependent below the instruction stream. What these tests assert is
//!   branchless assembly and an indistinguishable wall clock. A microarchitectural side channel is
//!   outside the reach of a t-test over timed batches.
//! - The tolerance is a **statement about this machine at this moment**. A pass means the leak, if
//!   any, is below what this box could resolve in this run, and the printed resolution says what
//!   that was.

use std::hint::black_box;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use orbitnet_core::auth::{
    AuthError, Direction, SessionAuth, SipHasher, KEY_LEN, TAG_LEN, TRAILER_LEN,
};

// =========================================================================================
// the timing harness
// =========================================================================================

/// Opens timed together as one sample.
///
/// One `open` over a 32-byte datagram is about 14 ns, and `Instant::now()` costs a meaningful
/// fraction of that, so a single call is below the timer's usable resolution. 128 of them is about
/// 1.8 us per sample, where the timer's own cost is a constant addend rather than the measurement.
const BATCH: usize = 128;

/// Samples per class per run. Both classes get exactly this many, from one shuffled balanced order.
///
/// 100000 puts a whole run at ~0.35 s, and the twelve runs of the test at ~4.3 s. The count is what
/// buys resolution. The measured resolvable difference at this count is 0.006 to 0.19 ns per
/// `open`, and it falls with the square root of the count.
const SAMPLES: usize = 100_000;

/// Null runs per test, half before the judged arms and half after.
const CALIBRATIONS: usize = 10;

/// Multiplier from the worst calibration `|t|` to the tolerance.
///
/// What this covers is the run-to-run spread of the worst-of-[`CALIBRATIONS`] statistic, which
/// measured 1.55 to 3.37 across the fifteen runs the header records. Doubling that statistic --
/// per run, from that run's own calibration -- cleared every value the judged arm produced in those
/// runs (0.60 to 2.72), by 1.9x in the tightest run and 11x in the loosest.
///
/// It is not a confidence level. The statistic is already a maximum over six crops of a
/// heavy-tailed sample, so it has no t distribution and a table value would be the wrong
/// instrument.
const SPREAD: f64 = 2.0;

/// How far above the tolerance the leaky control must land for the run to render a verdict.
///
/// The test asserts this margin and fails if the control does not reach it, so a box too noisy to
/// resolve a known leak reports a failure rather than a pass. The control's leak is a seven-byte
/// early exit; a one-byte exit, the smallest a byte-at-a-time compare can leak, is roughly a
/// seventh of it, so a box that resolves the control by 4x resolves a two-byte leak.
///
/// A usable machine is nowhere near the limit. The control measured 55x to 314x the tolerance
/// across the fifteen runs the header records, the low end on a machine running other builds at
/// the same time. A run that fails this assertion is a run whose measurement was worthless.
const HEADROOM: f64 = 4.0;

/// Draws of the judged arm. It fails only if **every** draw clears the tolerance.
///
/// The tolerance is a maximum over [`CALIBRATIONS`] null runs and the judged arm is a draw from the
/// same noisy process, so one unlucky draw can land above it with no leak present. A leak is
/// systematic and reproduces in every draw; noise does not. Two draws cost ~0.35 s and the failure
/// text a reader sees is a leak claim, which is worth not printing on one unlucky sample.
const JUDGED_DRAWS: usize = 2;

/// Fractions of the combined samples kept before each t-test, largest first.
///
/// An OS preemption inside a timed batch adds microseconds to it, and a handful of those inflate the
/// variance enough to hide a real difference of nanoseconds. Cropping is what dudect does, and it
/// crops **globally** -- one cutoff computed over both classes together, so cropping cannot itself
/// shift one class relative to the other. The statistic is the largest `|t|` over this list, and the
/// null is measured through the same list, so the multiple comparisons are already inside the
/// tolerance.
const CROPS: &[f64] = &[1.0, 0.95, 0.9, 0.8, 0.7, 0.5];

/// Payload bytes under the trailer. Deliberately short. What this test resolves is the compare's
/// share of one `open`, and a longer payload buries that share under more SipHash.
const PAYLOAD_LEN: usize = 20;

/// The session key. Arbitrary and fixed, so a failing run replays.
const KEY: [u8; KEY_LEN] = [
    0x9e, 0x37, 0x79, 0xb9, 0x7f, 0x4a, 0x7c, 0x15, 0xf3, 0x9c, 0xc0, 0x60, 0x5c, 0xed, 0xc8, 0x34,
];

/// Deterministic LCG -- the same one the codec and interest suites use. No dev-dependency.
fn lcg(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state
}

/// `draw` mapped into `0..bound`, from its **high** bits.
///
/// `draw % bound` is the obvious spelling and it is wrong here. A power-of-two LCG's low bits have
/// short periods -- bit 0 alternates every step -- so a modulo builds a class order that is
/// correlated with position, drift over the run lands on one class, and the null measured `|t|` = 39
/// where no leak existed. Multiplying and taking the top 32 bits uses the high bits instead.
fn below(draw: u32, bound: usize) -> usize {
    ((u64::from(draw) * bound as u64) >> 32) as usize
}

/// The shuffle seed for arm `index`. Every arm draws its class order from this one family.
///
/// The null, the control and the judged arm all shuffle through the same generator, so the order
/// each one sees is a draw from the same distribution. A fixed seed on the judged arm and varying
/// seeds on the null would let a systematic artifact of one class order repeat in every run of the
/// judged arm while the tolerance, measured over varying orders, did not bound it. The first
/// version of this harness measured exactly that kind of artifact at `|t|` = 39.
fn shuffle_seed(index: u32) -> u32 {
    index.wrapping_mul(2_654_435_761).max(1)
}

/// The tag over `signed` in `direction`, the way `auth.rs` computes it.
///
/// A replica rather than a call, because `tag_over` is private. Only the leaky control uses it; the
/// judged arm goes through `SessionAuth::open` itself.
fn tag_over(key: &[u8; KEY_LEN], signed: &[u8], direction: Direction) -> u64 {
    let mut hasher = SipHasher::new(key);
    hasher.write(signed);
    hasher.write(&[direction as u8]);
    hasher.finish()
}

/// `SessionAuth::open`'s verification, with a compare that returns at the first differing byte.
///
/// The negative control. `black_box` around each byte is what stops the optimizer from merging the
/// eight compares into one wide one, which is a real optimization and would delete the leak this
/// arm exists to produce. It makes the leak slightly larger than an unlucky compiler would, which is
/// stated where the numbers are reported.
fn leaky_open(key: &[u8; KEY_LEN], direction: Direction, datagram: &[u8]) -> Result<(), AuthError> {
    if datagram.len() < TRAILER_LEN {
        return Err(AuthError::Truncated);
    }
    let split = datagram.len() - TAG_LEN;
    let (signed, tag_bytes) = datagram.split_at(split);
    let expected = tag_over(key, signed, direction).to_le_bytes();
    for (byte, want) in tag_bytes.iter().zip(expected.iter()) {
        if black_box(*byte) != black_box(*want) {
            return Err(AuthError::BadTag);
        }
    }
    Ok(())
}

/// One genuine sealed datagram, reused by every class.
///
/// **One buffer serves every class.** Two buffers are two allocations at two addresses, and the
/// first version of this harness measured them as systematically different at `|t|` = 34 with no
/// leak present at all -- an artifact of alignment and cache placement that swamped the signal and
/// inflated the tolerance past usefulness. Every class writes its tag into this one buffer outside
/// the timed window instead, so the only thing that differs inside the window is the tag's value.
fn sealed_datagram() -> Vec<u8> {
    let mut session = SessionAuth::new(KEY);
    let mut datagram: Vec<u8> = (0..PAYLOAD_LEN).map(|index| index as u8).collect();
    session
        .seal(Direction::ToServer, &mut datagram)
        .expect("a fresh session has sequence numbers left");
    datagram
}

/// The datagram's tag with `mask` XORed into byte `tag_index`, so the datagram is refused.
///
/// `tag_index` 0 is the first tag byte on the wire, which a byte-at-a-time compare reaches first.
fn wrong_tag(datagram: &[u8], tag_index: usize, mask: u8) -> [u8; TAG_LEN] {
    assert_ne!(mask, 0, "a zero mask would leave the tag valid");
    let mut tag: [u8; TAG_LEN] = datagram[datagram.len() - TAG_LEN..]
        .try_into()
        .expect("a sealed datagram ends in its tag");
    tag[tag_index] ^= mask;
    tag
}

/// Time `BATCH` calls of `run` over `datagram`, in nanoseconds.
fn time_batch<F: FnMut(&[u8]) -> bool>(run: &mut F, datagram: &[u8]) -> u64 {
    let started = Instant::now();
    for _ in 0..BATCH {
        black_box(run(black_box(datagram)));
    }
    started.elapsed().as_nanos() as u64
}

/// Interleave two input classes and collect `SAMPLES` batch timings of each.
///
/// The order is a balanced list shuffled by `seed`, so thermal drift, frequency scaling and
/// scheduler noise over the run land on both classes alike. Both classes come back with exactly
/// `SAMPLES` entries.
fn measure<F: FnMut(&[u8]) -> bool>(
    run: &mut F,
    datagram: &mut [u8],
    tag_a: &[u8; TAG_LEN],
    tag_b: &[u8; TAG_LEN],
    seed: u32,
) -> (Vec<u64>, Vec<u64>) {
    let tag_at = datagram.len() - TAG_LEN;
    for _ in 0..(BATCH * 16) {
        datagram[tag_at..].copy_from_slice(tag_a);
        black_box(run(black_box(datagram)));
        datagram[tag_at..].copy_from_slice(tag_b);
        black_box(run(black_box(datagram)));
    }

    let mut order: Vec<bool> = (0..SAMPLES * 2).map(|index| index % 2 == 0).collect();
    let mut state = seed;
    for index in (1..order.len()).rev() {
        order.swap(index, below(lcg(&mut state), index + 1));
    }

    let mut samples_a: Vec<u64> = Vec::with_capacity(SAMPLES);
    let mut samples_b: Vec<u64> = Vec::with_capacity(SAMPLES);
    for &is_a in &order {
        let tag = if is_a { tag_a } else { tag_b };
        // Outside the timed window, and the same write whichever class it is.
        datagram[tag_at..].copy_from_slice(tag);
        let elapsed = time_batch(run, datagram);
        if is_a {
            samples_a.push(elapsed);
        } else {
            samples_b.push(elapsed);
        }
    }
    (samples_a, samples_b)
}

/// Mean and sample variance, in one pass (Welford).
fn moments(samples: &[u64]) -> (f64, f64, f64) {
    let mut count = 0.0_f64;
    let mut mean = 0.0_f64;
    let mut sum_squares = 0.0_f64;
    for &sample in samples {
        let value = sample as f64;
        count += 1.0;
        let delta = value - mean;
        mean += delta / count;
        sum_squares += delta * (value - mean);
    }
    let variance = if count > 1.0 {
        sum_squares / (count - 1.0)
    } else {
        0.0
    };
    (count, mean, variance)
}

/// One Welch t-test. `None` when either side is too small to have a variance.
fn welch_t(a: &[u64], b: &[u64]) -> Option<(f64, f64)> {
    let (count_a, mean_a, var_a) = moments(a);
    let (count_b, mean_b, var_b) = moments(b);
    if count_a < 2.0 || count_b < 2.0 {
        return None;
    }
    let standard_error = (var_a / count_a + var_b / count_b).sqrt();
    if standard_error <= 0.0 {
        return None;
    }
    Some(((mean_a - mean_b) / standard_error, standard_error))
}

/// What one run answers: the largest `|t|` over [`CROPS`], and the standard error at that crop.
struct Verdict {
    t: f64,
    crop: f64,
    standard_error: f64,
}

impl Verdict {
    /// Nanoseconds per call a difference would have to reach to produce `t` of `tolerance`.
    fn resolution_ns(&self, tolerance: f64) -> f64 {
        tolerance * self.standard_error / BATCH as f64
    }
}

/// The largest `|t|` over the crop list, cropping both classes at one cutoff drawn from the two
/// together.
fn judge(a: &[u64], b: &[u64]) -> Verdict {
    let mut combined: Vec<u64> = Vec::with_capacity(a.len() + b.len());
    combined.extend_from_slice(a);
    combined.extend_from_slice(b);
    combined.sort_unstable();

    let mut worst = Verdict {
        t: 0.0,
        crop: 1.0,
        standard_error: 0.0,
    };
    for &crop in CROPS {
        let index = ((combined.len() - 1) as f64 * crop) as usize;
        let cutoff = combined[index];
        let kept_a: Vec<u64> = a.iter().copied().filter(|&s| s <= cutoff).collect();
        let kept_b: Vec<u64> = b.iter().copied().filter(|&s| s <= cutoff).collect();
        let Some((t, standard_error)) = welch_t(&kept_a, &kept_b) else {
            continue;
        };
        if t.abs() > worst.t {
            worst = Verdict {
                t: t.abs(),
                crop,
                standard_error,
            };
        }
    }
    worst
}

#[test]
#[ignore = "timing measurement; run it with `just native-timing` on a quiet machine"]
fn the_tag_compare_is_not_distinguishable_by_timing() {
    // The first tag byte on the wire is the low byte of the tag word; the last is the high byte.
    // `early` and `early_twin` are wrong in the SAME byte, which is the null. `late` is wrong in
    // the byte a byte-at-a-time compare reaches last.
    let mut datagram = sealed_datagram();
    let early = wrong_tag(&datagram, 0, 0x01);
    let early_twin = wrong_tag(&datagram, 0, 0x80);
    let late = wrong_tag(&datagram, TAG_LEN - 1, 0x01);

    let mut session = SessionAuth::new(KEY);
    let mut shipped = |bytes: &[u8]| session.open(Direction::ToServer, bytes).is_ok();
    let mut leaky = |bytes: &[u8]| leaky_open(&KEY, Direction::ToServer, bytes).is_ok();

    println!();
    println!(
        "constant-time harness: {}-byte datagrams, {BATCH} opens per sample, {SAMPLES} samples per class",
        datagram.len()
    );
    println!("  null: two refusals wrong in the SAME tag byte, so no leak exists to find");

    let mut nulls: Vec<f64> = Vec::with_capacity(CALIBRATIONS);
    for run in 0..CALIBRATIONS / 2 {
        let verdict = judge_null(&mut shipped, &mut datagram, &early, &early_twin, run as u32);
        println!(
            "    before  |t| = {:6.2}  (crop {:.2})",
            verdict.t, verdict.crop
        );
        nulls.push(verdict.t);
    }

    let control = {
        let (a, b) = measure(
            &mut leaky,
            &mut datagram,
            &early,
            &late,
            shuffle_seed(0x100),
        );
        judge(&a, &b)
    };
    let mut draws: Vec<Verdict> = Vec::with_capacity(JUDGED_DRAWS);
    for draw in 0..JUDGED_DRAWS {
        let seed = shuffle_seed(0x200 + draw as u32);
        let (a, b) = measure(&mut shipped, &mut datagram, &early, &late, seed);
        draws.push(judge(&a, &b));
    }

    for run in 0..CALIBRATIONS - CALIBRATIONS / 2 {
        let verdict = judge_null(
            &mut shipped,
            &mut datagram,
            &early,
            &early_twin,
            0x1000 + run as u32,
        );
        println!(
            "    after   |t| = {:6.2}  (crop {:.2})",
            verdict.t, verdict.crop
        );
        nulls.push(verdict.t);
    }

    let worst_null = nulls.iter().copied().fold(0.0_f64, f64::max);
    let mut sorted = nulls.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a t statistic"));
    let median_null = sorted[sorted.len() / 2];
    let tolerance = worst_null * SPREAD;
    // The smallest draw is the judged value: a leak reproduces in every draw, noise does not.
    let judged = draws
        .iter()
        .min_by(|a, b| a.t.partial_cmp(&b.t).expect("no NaN in a t statistic"))
        .expect("JUDGED_DRAWS is at least one");
    let resolution = judged.resolution_ns(tolerance);

    println!(
        "  null worst |t| = {worst_null:.2}   median = {median_null:.2}   spread = {:.1}x",
        if median_null > 0.0 {
            worst_null / median_null
        } else {
            0.0
        }
    );
    println!("  tolerance = {tolerance:.2}  (worst null x {SPREAD})");
    println!("  a per-open difference of {resolution:.3} ns would have reached that tolerance");
    println!(
        "  leaky control (byte-at-a-time, early return)  |t| = {:8.2}  ({:.0}x tolerance, {:.3} ns per open)",
        control.t,
        control.t / tolerance,
        control.resolution_ns(control.t),
    );
    for (draw, verdict) in draws.iter().enumerate() {
        println!(
            "  shipped compare (auth.rs tags_equal) draw {draw}  |t| = {:8.2}  (crop {:.2})",
            verdict.t, verdict.crop
        );
    }
    println!(
        "  judged: smallest of {JUDGED_DRAWS} draws = {:.2}",
        judged.t
    );
    println!();

    assert!(
        control.t > tolerance * HEADROOM,
        "this machine cannot resolve a deliberately leaky compare: the control reached |t| = {:.2} \
         against a tolerance of {tolerance:.2}, short of the {HEADROOM}x margin a verdict needs. \
         The measurement is too noisy to mean anything here -- run it on an idle machine.",
        control.t,
    );
    let all_draws: Vec<String> = draws.iter().map(|v| format!("{:.2}", v.t)).collect();
    assert!(
        judged.t <= tolerance,
        "THE TAG COMPARE LEAKS. Two refused datagrams differing only in WHICH tag byte is wrong are \
         separable at |t| = {:.2} in every one of {JUDGED_DRAWS} draws ({}), against a tolerance of \
         {tolerance:.2} measured on this machine in this run. The leaky control reached |t| = {:.2}. \
         A compare whose timing depends on how much of the tag matched turns forging one into \
         8 x 256 guesses instead of 2^64.",
        judged.t,
        all_draws.join(", "),
        control.t,
    );
}

/// One null run: two wrong tags differing in the same byte, so no leak exists to find.
fn judge_null<F: FnMut(&[u8]) -> bool>(
    run: &mut F,
    datagram: &mut [u8],
    tag_a: &[u8; TAG_LEN],
    tag_b: &[u8; TAG_LEN],
    index: u32,
) -> Verdict {
    let (a, b) = measure(run, datagram, tag_a, tag_b, shuffle_seed(index));
    judge(&a, &b)
}

// =========================================================================================
// the codegen assertion
// =========================================================================================

/// A compare with the leak, for the negative control. Compiled through the same pipeline as the
/// extracted one and required to come back branching.
const LEAKY_SOURCE: &str = r#"
fn tags_equal(a: u64, b: u64) -> bool {
    let want = b.to_le_bytes();
    for (byte, expected) in a.to_le_bytes().iter().zip(want.iter()) {
        if std::hint::black_box(*byte) != std::hint::black_box(*expected) {
            return false;
        }
    }
    true
}
"#;

/// The wrapper that gives the extracted compare a symbol to find in the emitted assembly.
const PROBE_WRAPPER: &str = "
#[no_mangle]
pub extern \"C\" fn orbitnet_ct_probe(a: u64, b: u64) -> bool {
    tags_equal(a, b)
}
";

/// x86-64 conditional jumps and the loop instructions.
///
/// `cmov` and `set<cc>` are absent on purpose: both are conditional and neither branches, which is
/// how a constant-time compare is supposed to compile. `call` and `jmp` are absent because LLVM
/// spells them with an operand-size suffix (`callq`, `jmpq`) and [`branches`] matches those by
/// prefix.
const X86_BRANCHES: &[&str] = &[
    "ja", "jae", "jb", "jbe", "jc", "jcxz", "je", "jecxz", "jg", "jge", "jl", "jle", "jna", "jnae",
    "jnb", "jnbe", "jnc", "jne", "jng", "jnge", "jnl", "jnle", "jno", "jnp", "jns", "jnz", "jo",
    "jp", "jpe", "jpo", "jrcxz", "js", "jz", "loop", "loope", "loopne",
];

/// AArch64 branches. Anything spelled `b.<cond>` is caught by prefix in [`branches`], so the sixteen
/// condition codes are not listed. `csel`, `cset` and `tst` are absent for the same reason `cmov` is
/// absent above.
const ARM_BRANCHES: &[&str] = &["b", "bl", "blr", "br", "cbnz", "cbz", "tbnz", "tbz"];

/// Whether `mnemonic` transfers control.
///
/// Prefixes rather than an exhaustive list where the mnemonic carries a suffix: `b.eq` and its
/// fifteen siblings on AArch64, and `callq` / `jmpq` on x86-64, which is what LLVM emits rather than
/// the bare `call` and `jmp` an instruction reference lists.
fn branches(mnemonic: &str) -> bool {
    mnemonic.starts_with("b.")
        || mnemonic.starts_with("call")
        || mnemonic.starts_with("jmp")
        || X86_BRANCHES.contains(&mnemonic)
        || ARM_BRANCHES.contains(&mnemonic)
}

/// `fn tags_equal`'s text, read out of `src/auth.rs` at the path this crate was built from.
///
/// Extracted rather than copied, so the assertion is about the shipped compare and cannot go stale
/// when someone rewrites it.
fn extract_compare() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/auth.rs");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let start = source
        .find("fn tags_equal(")
        .unwrap_or_else(|| panic!("no `fn tags_equal(` in {} -- renamed?", path.display()));

    let bytes = source.as_bytes();
    let open = start
        + source[start..]
            .find('{')
            .expect("a function signature is followed by a body");
    let mut depth = 0_i32;
    let mut end = open;
    for (offset, &byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + offset + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        depth == 0 && end > open,
        "unbalanced braces in `tags_equal`"
    );
    source[start..end].to_string()
}

/// Compile `compare` plus the probe wrapper and return the wrapper's assembly, one instruction
/// mnemonic per entry.
///
/// `checked` adds the two settings `[profile.template-debug]` turns on over `[profile.release]`.
fn probe_mnemonics(compare: &str, checked: bool, slug: &str, label: &str) -> Vec<String> {
    let mut directory = std::env::temp_dir();
    directory.push(format!("orbitnet-ct-{}-{slug}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a writable temporary directory");
    let source_path = directory.join("probe.rs");
    let asm_path = directory.join("probe.s");
    std::fs::write(&source_path, format!("{compare}\n{PROBE_WRAPPER}"))
        .expect("the temporary probe is writable");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let mut command = Command::new(&rustc);
    command
        .arg("--edition=2021")
        .arg("--crate-type=lib")
        .arg("--emit=asm")
        .arg("-Copt-level=3")
        .arg("-Ccodegen-units=1")
        .arg("-Cdebuginfo=0");
    if checked {
        command
            .arg("-Cdebug-assertions=on")
            .arg("-Coverflow-checks=on");
    }
    let output = command
        .arg("-o")
        .arg(&asm_path)
        .arg(&source_path)
        .output()
        .unwrap_or_else(|error| {
            panic!("cannot run `{rustc}`: {error}. This test needs the compiler that built it.")
        });
    assert!(
        output.status.success(),
        "`{rustc}` refused the extracted compare:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let assembly =
        std::fs::read_to_string(&asm_path).expect("rustc wrote the assembly it was asked for");
    let _ = std::fs::remove_dir_all(&directory);
    mnemonics(&assembly, label)
}

/// The mnemonics inside the probe's own body, between its label and the end of its frame.
fn mnemonics(assembly: &str, label: &str) -> Vec<String> {
    let mut inside = false;
    let mut found: Vec<String> = Vec::new();
    for line in assembly.lines() {
        let trimmed = line.trim();
        if !inside {
            inside = trimmed == "orbitnet_ct_probe:" || trimmed == "_orbitnet_ct_probe:";
            continue;
        }
        if trimmed.starts_with(".cfi_endproc") || trimmed.starts_with(".size") {
            break;
        }
        if trimmed.is_empty()
            || trimmed.starts_with('.')
            || trimmed.starts_with(';')
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with('@')
            || trimmed.ends_with(':')
        {
            continue;
        }
        let mnemonic = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !mnemonic.is_empty() {
            found.push(mnemonic);
        }
    }
    assert!(
        inside,
        "no `orbitnet_ct_probe` symbol in the assembly emitted for {label}"
    );
    // `ret` on AArch64, `retq` on x86-64, so match by prefix. Its absence means the scanner found
    // the symbol and then read the wrong lines, which would make every verdict below vacuous.
    assert!(
        found.iter().any(|mnemonic| mnemonic.starts_with("ret")),
        "the probe's body in {label} has no return instruction -- the scanner found the symbol and \
         then read nothing it understood, so its verdict would be vacuous. Mnemonics read: {found:?}"
    );
    found
}

#[test]
fn the_tag_compare_compiles_without_a_branch() {
    // The negative control first. It also proves the mnemonic lists understand this host: a scanner
    // that recognizes nothing would otherwise report every compare as branchless.
    let leaky = probe_mnemonics(LEAKY_SOURCE, false, "control", "the leaky control");
    assert!(
        leaky.iter().any(|mnemonic| branches(mnemonic)),
        "a byte-at-a-time compare with an early return came back branchless, so this scanner does \
         not understand this target's assembly and its verdict on the real compare would be \
         vacuous. Add this target's branch mnemonics to X86_BRANCHES / ARM_BRANCHES. Mnemonics \
         read: {leaky:?}"
    );

    let compare = extract_compare();
    for (checked, slug, label) in [
        (false, "release", "`[profile.release]` (-C opt-level=3)"),
        (
            true,
            "template-debug",
            "`[profile.template-debug]` (-C opt-level=3 -C debug-assertions=on -C overflow-checks=on)",
        ),
    ] {
        let emitted = probe_mnemonics(&compare, checked, slug, label);
        let offenders: Vec<&String> = emitted
            .iter()
            .filter(|mnemonic| branches(mnemonic))
            .collect();
        assert!(
            offenders.is_empty(),
            "THE TAG COMPARE BRANCHES under {label}: {offenders:?}. A compare that transfers \
             control on the tag's contents can leak how much of a guessed tag was right, which \
             turns forging one into 8 x 256 guesses instead of 2^64. Full body: {emitted:?}"
        );
    }
}

/// Real `rustc --emit=asm` output for the shipped fold on AArch64 / Mach-O, captured verbatim.
const ARM_BRANCHLESS: &str = r#"
	.section	__TEXT,__text,regular,pure_instructions
	.globl	_orbitnet_ct_probe
	.p2align	2
_orbitnet_ct_probe:
	.cfi_startproc
	eor	x8, x1, x0
	lsr	x9, x8, #32
	orr	w8, w9, w8
	orr	w8, w8, w8, lsr #16
	orr	w8, w8, w8, lsr #8
	orr	w8, w8, w8, lsr #4
	orr	w8, w8, w8, lsr #2
	tst	w8, #0x3
	cset	w0, eq
	ret
	.cfi_endproc
"#;

/// Real output for the leaky byte compare on AArch64 / Mach-O, trimmed to the first two iterations.
const ARM_BRANCHING: &str = r#"
_orbitnet_ct_probe:
	.cfi_startproc
	sub	sp, sp, #16
	.cfi_def_cfa_offset 16
	strb	w0, [sp, #14]
	add	x8, sp, #14
	; InlineAsm Start
	; InlineAsm End
	ldrb	w8, [sp, #14]
	ldrb	w9, [sp, #15]
	cmp	w8, w9
	b.ne	LBB0_9
	lsr	x8, x0, #8
	mov	w0, #0
	b	LBB0_10
LBB0_9:
	mov	w0, #1
LBB0_10:
	add	sp, sp, #16
	ret
	.cfi_endproc
"#;

/// The same two shapes as LLVM spells them for x86-64 / ELF.
///
/// Written out rather than captured, because the machine this harness was written on has no x86-64
/// target installed and [`X86_BRANCHES`] would otherwise be carried untested until a Linux runner
/// picked it up. `callq` and `retq` carry the operand-size suffix LLVM emits and a bare instruction
/// reference does not, which is the spelling this fixture exists to hold [`branches`] to.
const X86_BRANCHLESS: &str = r#"
	.text
	.globl	orbitnet_ct_probe
	.p2align	4, 0x90
	.type	orbitnet_ct_probe,@function
orbitnet_ct_probe:
	.cfi_startproc
	movq	%rdi, %rax
	xorq	%rsi, %rax
	movq	%rax, %rcx
	shrq	$32, %rcx
	orl	%ecx, %eax
	movl	%eax, %ecx
	shrl	$16, %ecx
	orl	%eax, %ecx
	testb	$3, %cl
	sete	%al
	retq
.Lfunc_end0:
	.size	orbitnet_ct_probe, .Lfunc_end0-orbitnet_ct_probe
	.cfi_endproc
"#;

/// The leaky shape for x86-64 / ELF: a conditional jump per byte, and an indirect call.
const X86_BRANCHING: &str = r#"
orbitnet_ct_probe:
	.cfi_startproc
	subq	$16, %rsp
	.cfi_def_cfa_offset 24
	movb	%dil, 14(%rsp)
	#APP
	#NO_APP
	movzbl	14(%rsp), %eax
	cmpb	%al, 15(%rsp)
	jne	.LBB0_9
	callq	*%rax
	jmpq	.LBB0_10
.LBB0_9:
	movl	$1, %eax
.LBB0_10:
	addq	$16, %rsp
	retq
.Lfunc_end0:
	.size	orbitnet_ct_probe, .Lfunc_end0-orbitnet_ct_probe
	.cfi_endproc
"#;

/// The scanner's line parsing and four branch spellings, on both architectures, from fixtures.
///
/// `the_tag_compare_compiles_without_a_branch` only ever compiles for the host, so the other
/// architecture's assembly is never parsed on any one machine. What these four fixtures hold:
///
/// - [`mnemonics`] reads the body of a probe in either syntax -- Mach-O / AArch64 and ELF / x86-64
///   label, directive, comment and terminator spellings alike -- and stops at the function's end.
/// - [`branches`] recognizes the four spellings the fixtures contain: `b.ne`, `b`, `jne`, `callq`.
/// - [`branches`] calls neither shipped fold branching, so `cset`, `tst`, `sete` and `testb` stay
///   off the lists.
///
/// The remaining 40 entries of [`X86_BRANCHES`] and [`ARM_BRANCHES`] are not exercised here. What
/// covers them is the per-host negative control inside
/// `the_tag_compare_compiles_without_a_branch`, which fails unless the scanner recognizes whatever
/// branch mnemonic LLVM actually emitted on the machine running the test.
#[test]
fn the_branch_scanner_reads_both_architectures() {
    for (assembly, label) in [
        (ARM_BRANCHLESS, "aarch64, the shipped fold"),
        (X86_BRANCHLESS, "x86-64, the shipped fold"),
    ] {
        let read = mnemonics(assembly, label);
        let offenders: Vec<&String> = read.iter().filter(|found| branches(found)).collect();
        assert!(
            offenders.is_empty(),
            "the scanner called branchless assembly branching for {label}: {offenders:?}"
        );
    }

    for (assembly, label, expected) in [
        (ARM_BRANCHING, "aarch64, a leaky compare", ["b.ne", "b"]),
        (X86_BRANCHING, "x86-64, a leaky compare", ["jne", "callq"]),
    ] {
        let read = mnemonics(assembly, label);
        for wanted in expected {
            assert!(
                read.iter().any(|found| found == wanted) && branches(wanted),
                "the scanner missed `{wanted}` in {label}. Mnemonics read: {read:?}"
            );
        }
    }

    // The terminator, not just the branches: a scanner that ran past the function would read the
    // next symbol's instructions and could report a branch that is not in the compare at all.
    assert_eq!(
        mnemonics(X86_BRANCHLESS, "x86-64, the shipped fold").last(),
        Some(&"retq".to_string()),
        "the scanner read past the probe's `.size` / `.cfi_endproc`"
    );
}
