//! Generated coverage for the wire parsers: round-trip properties, and a decoder sweep.
//!
//! The five wire modules — `codec`, `protocol`, `columnar`, `quant`, `slots` — are hand-rolled
//! binary parsers over bytes a remote peer chooses, and `auth` is the length arithmetic that runs
//! on a datagram before any of them see it. Their own `#[cfg(test)]` suites are large and include
//! hostile-input cases, and every one of those cases is an input somebody thought of.
//!
//! This file is a **generator**. It produces inputs nobody enumerated, and it shrinks a failure to
//! the smallest input that still fails. Nothing else in the workspace generates test input:
//! `grep -rn "proptest\|quickcheck\|arbitrary\|fuzz" --include=Cargo.toml native/` found this
//! entry and nothing more when it was added.
//!
//! # Why `proptest` and not `cargo-fuzz`
//!
//! Recorded because it is a decision rather than a default. Both tools reach the same property,
//! and the four rows below are what picked between them.
//!
//! | `cargo-fuzz` | What it costs here |
//! | --- | --- |
//! | needs a nightly toolchain | `rust-toolchain.toml` pins 1.94.1 stable, and every gate runs on it |
//! | needs libFuzzer | a platform-specific sanitizer runtime on top of the pinned toolchain |
//! | needs a `fuzz/` workspace member | a new member must pass `cargo clippy --workspace --all-targets -D warnings` on every PR, and its `[dependencies]` would sit beside a crate whose emptiness is a rule |
//! | runs unbounded | a PR gate has to finish |
//!
//! A `proptest` sweep reaches the same property — **arbitrary bytes into a public decoder return an
//! error or a value, never a panic and never a read out of bounds** — on the pinned stable
//! toolchain, in a bounded number of cases, as one `[dev-dependencies]` entry.
//! What it gives up against a coverage-guided fuzzer is **corpus evolution**. It does not learn
//! which bytes reached new code, so it cannot grow a corpus toward an unexercised branch.
//! `arb_valid_frame` stands in for that: it generates well-formed frames and then corrupts them,
//! which is what reaches the deep paths at all. Uniform random bytes fail the first length or tag
//! check and exercise nothing past it. Measured over the four sweeps' 1,024 cases, the corrupted
//! corpus produced 176 headers that decoded, 166 state-block metas, 78 masked deltas applied to
//! completion, 168 input-block metas and 158 manifest deltas.
//!
//! # Why an integration test rather than `#[cfg(test)] mod tests`
//!
//! A file under `tests/` links the crate from outside, so it can only name the **public** API, and
//! that is the surface the sweep is defined over. A decoder that stops being public stops being
//! swept. One that becomes public is a compile-visible addition to the table below rather than a
//! silent gap. It also keeps a 3,500-line `codec.rs` from growing.
//!
//! # The public decode entry points, enumerated
//!
//! Every public function that reads a byte slice a remote peer chose, across the five wire modules
//! and `auth`, plus `ReplayWindow::accept`, whose `u32` comes straight off the wire. All of them are
//! driven by `drive_every_decoder`, which is the single place a new one is added.
//!
//! | Module | Entry points | Count |
//! | --- | --- | --- |
//! | `codec` frames and blocks | `FrameKind::from_tag`, `FrameHeader::decode`, `Handshake::decode`, `decode_state_block_meta`, `decode_state_block_into`, `skip_state_block_body`, `decode_input_block_meta`, `input_block_row`, `skip_input_block_body`, `Challenge::decode`, `Welcome::decode`, `Ping::decode`, `Pong::decode`, `decode_manifest_full`, `decode_manifest_delta`, `decode_interest_delta`, `decode_interest_table` | 17 |
//! | `codec::Reader` primitives | `u8`, `i8`, `u16`, `u32`, `u64`, `f32`, `f64`, `bytes`, `peek_bytes`, `varint`, `zigzag`, `bitmask_into` | 12 |
//! | `quant` | `decode_row`, `apply_masked_wire`, `row_is_finite`, `f16_bits_to_f32`, `ss3_to_quat` | 5 |
//! | `columnar` | `apply_masked` | 1 |
//! | `auth` receive path | `SessionAuth::open`, `ReplayWindow::accept`, `siphash24`, `SipHasher::write`, `SipHasher::finish`, `compress_secret` | 6 |
//! | **Total** | | **41** |
//!
//! Deliberately outside that table:
//!
//! - **Encoders.** Their inputs are local values, not wire bytes.
//! - **`quant::canonicalize_value`.** Capture-time, over a native row the local game wrote. It is
//!   covered as a round-trip property instead, because every other property depends on it.
//! - **`quant::basis_to_quat` and `quant::quat_to_basis`.** Native `f32` arrays rather than bytes.
//!   Both are reached transitively by `decode_row` for a `Basis` property under `QuantKind::Ss3`.
//! - **`auth::derive_session_key` and `auth::confirm_tag`.** Fixed-width `[u8; KEY_LEN]` arrays
//!   rather than slices, so there is no length off the wire for them to get wrong. `SessionAuth::seal`
//!   and `ReceiveBudget` are excluded for the reason encoders are: their inputs are local.
//! - **The `slots::SlotTable` mutators.** `bind` and `unbind` take a decoded slot and id rather than
//!   bytes. `a_manifest_delta_binds_the_slot_table_the_full_frame_binds` and
//!   `a_decoded_manifest_binds_a_slot_table_without_panicking` drive them from decoded frames,
//!   including frames the sweep corrupted.
//!
//! # Case counts
//!
//! `cargo test --workspace` is a PR gate, so the counts are chosen rather than left at the
//! `proptest` default of 256.
//!
//! | Constant | Cases | Applied to |
//! | --- | --- | --- |
//! | `CASES` | 256 | byte-level sweeps and the fixed-shape round trips; a case is one encode/decode of at most a few hundred bytes |
//! | `HEAVY_CASES` | 64 | properties that build two manifests, a `SlotTable` and a schema per case |
//! | `ORIENTATION_CASES` | 8,192 | the one property whose target family is a few orientations in ten thousand; see the constant for the measurement that fixed the number |
//!
//! Measured at **0.31 s** for the whole file, against 0.88 s for `cargo test --workspace` before
//! it. `PROPTEST_CASES` in the environment overrides all three constants, which is how to run this
//! as a long soak locally without editing it.
//!
//! # What runs under debug assertions
//!
//! `cargo test` uses the dev profile, which carries `debug-assertions` and `overflow-checks`, as
//! does the `template-debug` profile Godot loads from source. Two classes of failure are therefore
//! visible to these properties and invisible to a `--release` run:
//!
//! - **`debug_assert!`**, which `encode_state_block` uses to check that the encoder agrees with
//!   itself about how many bytes it wrote.
//! - **An arithmetic overflow**, which is the failure mode a decoder reaches by adding a
//!   wire-supplied length to a cursor. Every such site in the crate is `checked_`, `saturating_` or
//!   guarded, and these sweeps are what keep that true.
//!
//! # A failing case
//!
//! `proptest` records the seed of a failure in `tests/proptest-regressions/wire_properties.txt` —
//! the path `config` states — and replays it first on every later run. **Commit that file.** It is
//! the shrunk counterexample, and committing it is what turns a generated failure into a permanent
//! regression test.

use orbitnet_core::auth::{
    compress_secret, derive_cipher_key, siphash24, AuthError, Direction, ReplayWindow, SessionAuth,
    SipHasher, CIPHER_TRAILER_LEN, EXCHANGE_KEY_LEN, KEY_LEN, SEQ_LEN, TRAILER_LEN,
};
use orbitnet_core::codec::{
    apply_manifest_delta, decode_input_block_meta, decode_interest_delta, decode_interest_table,
    decode_manifest_delta, decode_manifest_full, decode_state_block_into, decode_state_block_meta,
    diff_manifest, encode_input_block, encode_interest_delta, encode_interest_table,
    encode_manifest_delta, encode_manifest_full, encode_state_block, input_block_row,
    skip_input_block_body, skip_state_block_body, Challenge, CodecError, FrameHeader, FrameKind,
    Handshake, InterestDeltaSection, ManifestDelta, ManifestEntry, Ping, Pong, Reader, Welcome,
    Writer,
};
use orbitnet_core::columnar::{apply_masked, changed_mask, masked_size, write_masked};
use orbitnet_core::protocol::{PropKind, PropRole, PropSchema, QuantKind, SchemaBuilder};
use orbitnet_core::quant::{
    apply_masked_wire, canonicalize_value, decode_row, encode_row, f16_bits_to_f32,
    masked_wire_size, quat_to_basis, row_is_finite, ss3_to_quat, wire_row_stride,
    write_masked_wire,
};
use orbitnet_core::slots::SlotTable;
use proptest::prelude::*;
use proptest::test_runner::{FileFailurePersistence, TestCaseError};

/// Cases for a byte-level sweep or a fixed-shape round trip. See the module header.
const CASES: u32 = 256;

/// Cases for a property that builds a schema, two manifests and a `SlotTable` per case.
const HEAVY_CASES: u32 = 64;

/// Cases for `a_near_balanced_orientation_stores_what_the_wire_reconstructs`.
///
/// **Far higher than everything else here, because the target family is thin.** Measured by
/// removing `quat_to_ss3`'s dropped-index rule and counting fresh runs that caught the break:
///
/// | Cases | Runs that caught it | Cost |
/// | --- | --- | --- |
/// | 256 | 0 of 12 | 0.02 s |
/// | 1,024 | 5 of 12 | 0.06 s |
/// | 4,096 | 9 of 12 | 0.23 s |
/// | **8,192** | **11 of 12** | **0.43 s** |
/// | 16,384 | 12 of 12 | 0.83 s |
///
/// 8,192 catches 11 of 12 at 0.43 s; 16,384 catches 12 of 12 but doubles this file's runtime.
/// **The catch is probabilistic rather than certain**: the defect sits in roughly one orientation in three thousand, and closing the last
/// twelfth doubles the runtime of this whole file on a gate that runs on every push.
/// `PROPTEST_CASES=200000` is the soak for anyone changing `quat_to_ss3`.
const ORIENTATION_CASES: u32 = 8_192;

/// `cases` plus an explicit home for a shrunk counterexample.
///
/// The path is stated rather than left to `proptest`'s default. That default looks upward for a
/// `lib.rs` or `main.rs` to place the file beside, finds neither from a `tests/` target, warns on
/// every failure, and falls back to a bare file named after the test. Naming the directory keeps a
/// committed regression file in one predictable place.
fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/proptest-regressions/wire_properties.txt",
        ))),
        ..ProptestConfig::default()
    }
}

// ---------------------------------------------------------------------------
// The enumerations the properties sweep over.
// ---------------------------------------------------------------------------

/// Every `PropKind`. Kept complete by `kind_index`.
const ALL_KINDS: [PropKind; 10] = [
    PropKind::Bool,
    PropKind::I32,
    PropKind::I64,
    PropKind::F32,
    PropKind::F64,
    PropKind::Vec3,
    PropKind::Quat,
    PropKind::Vec2,
    PropKind::Basis,
    PropKind::Transform,
];

/// Every `QuantKind`. Kept complete by `quant_index`.
const ALL_QUANTS: [QuantKind; 3] = [QuantKind::None, QuantKind::Ss3, QuantKind::Half];

/// Every `PropRole`.
const ALL_ROLES: [PropRole; 3] = [PropRole::State, PropRole::Input, PropRole::Cosmetic];

/// Every `FrameKind`. Kept complete by `frame_kind_index`.
const ALL_FRAME_KINDS: [FrameKind; 9] = [
    FrameKind::ServerSnapshot,
    FrameKind::ClientInput,
    FrameKind::Ping,
    FrameKind::Pong,
    FrameKind::Challenge,
    FrameKind::Welcome,
    FrameKind::EntityManifest,
    FrameKind::EntityManifestDelta,
    FrameKind::InterestTable,
];

/// Position of `kind` in `ALL_KINDS`.
///
/// **The match is exhaustive on purpose.** Adding a `PropKind` fails to compile here, which is the
/// only reminder a new kind needs a row in `ALL_KINDS` before these properties cover it.
fn kind_index(kind: PropKind) -> usize {
    match kind {
        PropKind::Bool => 0,
        PropKind::I32 => 1,
        PropKind::I64 => 2,
        PropKind::F32 => 3,
        PropKind::F64 => 4,
        PropKind::Vec3 => 5,
        PropKind::Quat => 6,
        PropKind::Vec2 => 7,
        PropKind::Basis => 8,
        PropKind::Transform => 9,
    }
}

/// Position of `quant` in `ALL_QUANTS`. Exhaustive for the reason `kind_index` is.
fn quant_index(quant: QuantKind) -> usize {
    match quant {
        QuantKind::None => 0,
        QuantKind::Ss3 => 1,
        QuantKind::Half => 2,
    }
}

/// Position of `kind` in `ALL_FRAME_KINDS`. Exhaustive for the reason `kind_index` is.
fn frame_kind_index(kind: FrameKind) -> usize {
    match kind {
        FrameKind::ServerSnapshot => 0,
        FrameKind::ClientInput => 1,
        FrameKind::Ping => 2,
        FrameKind::Pong => 3,
        FrameKind::Challenge => 4,
        FrameKind::Welcome => 5,
        FrameKind::EntityManifest => 6,
        FrameKind::EntityManifestDelta => 7,
        FrameKind::InterestTable => 8,
    }
}

/// Every `(PropKind, QuantKind)` pairing `QuantKind::valid_for` admits, in a stable order.
fn pairings() -> Vec<(PropKind, QuantKind)> {
    let mut out = Vec::new();
    for kind in ALL_KINDS {
        for quant in ALL_QUANTS {
            if quant.valid_for(kind) {
                out.push((kind, quant));
            }
        }
    }
    out
}

/// One schema holding every valid pairing exactly once, in `pairings()` order.
fn every_pairing_schema() -> SchemaBuilder {
    let mut schema = SchemaBuilder::new();
    for (index, (kind, quant)) in pairings().into_iter().enumerate() {
        schema.push_quantized(format!("p{index}"), kind, PropRole::State, quant);
    }
    schema
}

/// The schema the byte sweeps decode against: small, and spanning both quantizers plus a lossless
/// float, an integer and a bool. A sweep case decodes the same bytes many times, so the schema is
/// kept cheap rather than exhaustive — `every_pairing_schema` is what covers the pairings.
fn sweep_schema() -> SchemaBuilder {
    let mut schema = SchemaBuilder::new();
    schema.push("flag", PropKind::Bool, PropRole::State);
    schema.push("count", PropKind::I32, PropRole::Input);
    schema.push_quantized("speed", PropKind::F32, PropRole::State, QuantKind::Half);
    schema.push_quantized("velocity", PropKind::Vec3, PropRole::State, QuantKind::Half);
    schema.push_quantized("facing", PropKind::Quat, PropRole::State, QuantKind::Ss3);
    schema.push("mass", PropKind::F64, PropRole::Cosmetic);
    schema
}

// ---------------------------------------------------------------------------
// Row helpers.
// ---------------------------------------------------------------------------

/// Canonicalize every property of a native row in place, which is what a sender does at capture.
///
/// After this the row holds exactly the value a receiver reconstructs, which is the precondition of
/// every round-trip property below: an unquantized property is untouched, and a quantized one has
/// been driven to a fixed point of decode-then-encode.
fn canonicalize_row(props: &[PropSchema], row: &mut [u8]) {
    for prop in props {
        let end = prop.offset + prop.kind.stride();
        canonicalize_value(prop.kind, prop.quant, &mut row[prop.offset..end]);
    }
}

// ---------------------------------------------------------------------------
// Strategies.
// ---------------------------------------------------------------------------

/// A schema of 1 to `max_props` properties over arbitrary kind/quant/role pickings.
///
/// An invalid pairing is generated deliberately: `SchemaBuilder::push_quantized` normalizes one to
/// `QuantKind::None`, and that normalization is on the path every property here depends on.
fn arb_schema(max_props: usize) -> impl Strategy<Value = SchemaBuilder> {
    prop::collection::vec(
        (
            0usize..ALL_KINDS.len(),
            0usize..ALL_QUANTS.len(),
            0usize..ALL_ROLES.len(),
        ),
        1..=max_props,
    )
    .prop_map(|picks| {
        let mut schema = SchemaBuilder::new();
        for (index, (kind, quant, role)) in picks.into_iter().enumerate() {
            schema.push_quantized(
                format!("p{index}"),
                ALL_KINDS[kind],
                ALL_ROLES[role],
                ALL_QUANTS[quant],
            );
        }
        schema
    })
}

/// A schema plus `rows` native rows of its stride, every byte arbitrary.
///
/// Arbitrary bytes rather than plausible floats: a native row reaches the encoder holding whatever
/// the game wrote, infinities and NaN payloads included, and both quantizers claim to be total over
/// exactly that.
fn arb_schema_and_rows(
    max_props: usize,
    rows: usize,
) -> impl Strategy<Value = (SchemaBuilder, Vec<Vec<u8>>)> {
    arb_schema(max_props).prop_flat_map(move |schema| {
        let stride = schema.row_stride();
        (
            Just(schema),
            prop::collection::vec(prop::collection::vec(any::<u8>(), stride), rows),
        )
    })
}

/// A manifest table: **a bijection between slots and ids**, ascending by slot.
///
/// That is what a server can publish — a slot names one entity and an entity holds one slot — so a
/// round-trip property is stated over it. A table that is not a bijection is a hostile frame, and
/// `a_decoded_manifest_binds_a_slot_table_without_panicking` is where those are covered.
fn arb_manifest(max_rows: usize) -> impl Strategy<Value = Vec<ManifestEntry>> {
    (
        prop::collection::btree_set(any::<u16>(), 0..=max_rows),
        // Id 0 is the backend's "no entity" everywhere, and `SlotTable::bind` refuses it.
        prop::collection::btree_set(1u64.., 0..=max_rows),
        prop::collection::vec(
            (
                any::<u32>(),
                any::<u32>(),
                // A negative owner is not a peer id and the encoder writes it as 0; that
                // normalization is its own property rather than a hole in this one.
                0i32..=i32::MAX,
                any::<u16>(),
            ),
            0..=max_rows,
        ),
    )
        .prop_map(|(slots, ids, bodies)| {
            slots
                .into_iter()
                .zip(ids)
                .zip(bodies)
                .map(
                    |((slot, id), (state_hash, input_hash, owner, seat))| ManifestEntry {
                        slot,
                        id,
                        state_hash,
                        input_hash,
                        owner,
                        seat,
                    },
                )
                .collect()
        })
}

/// A quaternion whose four components are all near `±0.5` — an ordinary diagonal axis.
///
/// **Uniform random bytes never land here, and this is the family `quat_to_ss3`'s dropped-index
/// rule exists for.** A near-balanced orientation reconstructs with its largest component slightly
/// smaller than one of the stored smalls, so a naive encoder picks a different index on re-encode
/// and the sender's row and the receiver's row alternate one quantum apart forever. Interpreting an
/// arbitrary `[u8; 16]` as a quaternion produces a wildly unbalanced one essentially always, so the
/// generic row properties cover everything except the case that actually broke.
fn arb_near_balanced_quat() -> impl Strategy<Value = [f32; 4]> {
    // The jitter comes off an integer range and is then scaled. A float range would be sampled with
    // `proptest`'s own bias toward its endpoints and zero, and the family that breaks is a thin
    // shell rather than an endpoint.
    //
    prop::collection::vec((any::<bool>(), -500_000i32..=500_000), 4).prop_map(|parts| {
        let mut q = [0.0f32; 4];
        for (slot, (negative, jitter)) in q.iter_mut().zip(parts) {
            let magnitude = 0.5 + jitter as f32 * 1e-7;
            *slot = if negative { -magnitude } else { magnitude };
        }
        q
    })
}

/// An ascending run of distinct slots, the shape both interest frames carry.
fn arb_slots(max: usize) -> impl Strategy<Value = Vec<u16>> {
    prop::collection::btree_set(any::<u16>(), 0..=max).prop_map(|slots| slots.into_iter().collect())
}

/// A frame header with fields in the ranges the encoder is defined over.
fn arb_header() -> impl Strategy<Value = FrameHeader> {
    (
        0usize..ALL_FRAME_KINDS.len(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<i8>(),
        any::<u8>(),
        any::<u32>(),
    )
        .prop_map(
            |(kind, tick, ack_tick, ack_bits, ack_token, margin_ticks, flags, entity_count)| {
                FrameHeader {
                    kind: ALL_FRAME_KINDS[kind],
                    tick,
                    ack_tick,
                    ack_bits,
                    ack_token,
                    margin_ticks,
                    flags,
                    entity_count,
                }
            },
        )
}

/// A handshake with every field arbitrary, including the trailing optional ones.
fn arb_handshake() -> impl Strategy<Value = Handshake> {
    (
        any::<u32>(),
        any::<u16>(),
        any::<u64>(),
        any::<[u8; KEY_LEN]>(),
        any::<u64>(),
        any::<[u8; KEY_LEN]>(),
        any::<u64>(),
        any::<[u8; EXCHANGE_KEY_LEN]>(),
    )
        .prop_map(
            |(
                protocol_version,
                tickrate,
                session_id,
                joiner_nonce,
                resume_token,
                acceptor_nonce,
                confirm,
                joiner_exchange,
            )| {
                Handshake {
                    protocol_version,
                    tickrate,
                    session_id,
                    joiner_nonce,
                    resume_token,
                    acceptor_nonce,
                    confirm,
                    joiner_exchange,
                }
            },
        )
}

/// A well-formed `EntityManifest` frame, tag byte included.
fn arb_manifest_full_frame() -> impl Strategy<Value = Vec<u8>> {
    (any::<u64>(), arb_manifest(8))
        .prop_map(|(generation, rows)| encode_manifest_full(generation, &rows))
}

/// A well-formed `EntityManifestDelta` frame, tag byte included.
fn arb_manifest_delta_frame() -> impl Strategy<Value = Vec<u8>> {
    (any::<u64>(), any::<u64>(), arb_slots(8), arb_manifest(8)).prop_map(
        |(base_generation, generation, removed, added)| {
            encode_manifest_delta(&ManifestDelta {
                base_generation,
                generation,
                removed,
                added,
            })
        },
    )
}

/// A well-formed encoding of one frame, for the corruption sweep to edit.
///
/// Uniform random bytes fail the first tag or length check in every decoder, so on their own they
/// prove almost nothing about the code past it. Corrupting a frame that was valid a moment ago is
/// what puts a hostile length, count or tag in front of a decoder that has already consumed a
/// plausible prefix.
fn arb_valid_frame() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        arb_handshake().prop_map(|handshake| handshake.encode()),
        (any::<u32>(), any::<u64>(), any::<u16>(), any::<u64>()).prop_map(
            |(protocol_version, server_tick, tickrate, resume_token)| Welcome {
                protocol_version,
                server_tick,
                tickrate,
                resume_token,
            }
            .encode()
        ),
        (any::<u64>(), any::<u64>()).prop_map(|(seq, client_us)| Ping { seq, client_us }.encode()),
        (any::<u64>(), any::<u64>(), any::<f64>()).prop_map(|(seq, client_us, server_time)| Pong {
            seq,
            client_us,
            server_time,
        }
        .encode()),
        arb_manifest_full_frame(),
        arb_manifest_delta_frame(),
        (any::<u64>(), arb_slots(16))
            .prop_map(|(generation, slots)| encode_interest_table(generation, &slots)),
        arb_snapshot_frame(),
        arb_input_frame(),
    ]
}

/// A complete server snapshot: header, state blocks against `sweep_schema`, and an interest section.
fn arb_snapshot_frame() -> impl Strategy<Value = Vec<u8>> {
    let schema = sweep_schema();
    let stride = schema.row_stride();
    (
        any::<u32>(),
        prop::collection::vec(
            (any::<u16>(), prop::collection::vec(any::<u8>(), stride)),
            0..=4,
        ),
        any::<bool>(),
        (any::<u64>(), arb_slots(4), arb_slots(4)),
    )
        .prop_map(
            move |(tick, blocks, state_lane, (generation, left, entered))| {
                let props = schema.props();
                let mut writer = Writer::new();
                FrameHeader {
                    kind: FrameKind::ServerSnapshot,
                    tick,
                    ack_tick: tick,
                    ack_bits: 0,
                    ack_token: 0,
                    margin_ticks: 0,
                    flags: FrameHeader::FLAG_INTEREST_DELTA,
                    entity_count: blocks.len() as u32,
                }
                .encode(&mut writer);
                let mut scratch = Vec::new();
                let mut base = vec![0u8; stride];
                canonicalize_row(props, &mut base);
                for (slot, row) in &blocks {
                    let mut row = row.clone();
                    canonicalize_row(props, &mut row);
                    encode_state_block(
                        &mut writer,
                        &mut scratch,
                        props,
                        *slot,
                        u64::from(tick),
                        u64::from(tick),
                        Some((u64::from(tick).saturating_sub(1), &base)),
                        &row,
                        state_lane,
                    );
                }
                encode_interest_delta(generation, &left, &entered, &mut writer);
                writer.into_inner()
            },
        )
}

/// A complete client input frame: header, then input blocks against `sweep_schema`.
fn arb_input_frame() -> impl Strategy<Value = Vec<u8>> {
    let schema = sweep_schema();
    let stride = schema.row_stride();
    (
        any::<u32>(),
        prop::collection::vec(
            (
                any::<u16>(),
                prop::collection::vec(prop::collection::vec(any::<u8>(), stride), 1..=3),
            ),
            0..=3,
        ),
    )
        .prop_map(move |(tick, blocks)| {
            let props = schema.props();
            let mut writer = Writer::new();
            FrameHeader {
                kind: FrameKind::ClientInput,
                tick,
                ack_tick: tick,
                ack_bits: 0,
                ack_token: 0,
                margin_ticks: 0,
                flags: 0,
                entity_count: blocks.len() as u32,
            }
            .encode(&mut writer);
            for (slot, rows) in &blocks {
                let canonical: Vec<Vec<u8>> = rows
                    .iter()
                    .map(|row| {
                        let mut row = row.clone();
                        canonicalize_row(props, &mut row);
                        row
                    })
                    .collect();
                let borrowed: Vec<&[u8]> = canonical.iter().map(Vec::as_slice).collect();
                encode_input_block(
                    &mut writer,
                    props,
                    *slot,
                    u64::from(tick),
                    u64::from(tick),
                    &borrowed,
                );
            }
            writer.into_inner()
        })
}

/// One edit applied to an otherwise valid encoding.
#[derive(Debug, Clone, Copy)]
enum Edit {
    /// Flip one bit of one byte.
    Flip(u8),
    /// Overwrite one byte.
    Set(u8),
    /// Insert a byte, shifting every field after it.
    Insert(u8),
    /// Delete a byte, shifting every field after it.
    Remove,
    /// Cut the frame short.
    Truncate,
}

/// An edit paired with the offset it lands on, taken modulo the frame length.
fn arb_edit() -> impl Strategy<Value = (u16, Edit)> {
    (
        any::<u16>(),
        prop_oneof![
            any::<u8>().prop_map(Edit::Flip),
            any::<u8>().prop_map(Edit::Set),
            any::<u8>().prop_map(Edit::Insert),
            Just(Edit::Remove),
            Just(Edit::Truncate),
        ],
    )
}

/// Apply `edits` in order. An empty buffer accepts an insert and ignores the rest.
fn corrupt(mut bytes: Vec<u8>, edits: &[(u16, Edit)]) -> Vec<u8> {
    for &(offset, edit) in edits {
        if bytes.is_empty() {
            if let Edit::Insert(value) = edit {
                bytes.push(value);
            }
            continue;
        }
        let at = usize::from(offset) % bytes.len();
        match edit {
            Edit::Flip(bit) => bytes[at] ^= 1 << (bit % 8),
            Edit::Set(value) => bytes[at] = value,
            Edit::Insert(value) => bytes.insert(at, value),
            Edit::Remove => {
                bytes.remove(at);
            }
            Edit::Truncate => bytes.truncate(at),
        }
    }
    bytes
}

// ---------------------------------------------------------------------------
// The sweep: every public decode entry point, over one buffer.
// ---------------------------------------------------------------------------

/// Drive all 40 public decode entry points over `bytes`.
///
/// Every call must answer an error or a value. The assertions state the two things a successful
/// decode promises beyond not panicking:
///
/// - a block meta that decoded names a body **inside the buffer**, so the matching `skip` cannot
///   fail and cannot read past the end;
/// - a row accessor that answers `Some` answers exactly one wire stride;
/// - a datagram this key sealed opens back to the payload it was sealed over, opens a second time
///   as `Replayed`, and opens in the other direction as `BadTag`.
///
/// This is the one place a decoder is registered. A new public decode entry point belongs here and
/// in the module header's table, in the same change.
fn drive_every_decoder(bytes: &[u8], schema: &SchemaBuilder) -> Result<(), TestCaseError> {
    let props = schema.props();
    let stride = schema.row_stride();
    let wire = wire_row_stride(props);

    // --- Reader primitives. Each from a fresh cursor, so one failure does not mask the next.
    for count in [0usize, 1, 2, 4, 8, bytes.len(), bytes.len() + 1, usize::MAX] {
        let mut reader = Reader::new(bytes);
        let _ = reader.bytes(count);
        let _ = reader.peek_bytes(count, count);
    }
    let mut reader = Reader::new(bytes);
    let _ = reader.u8();
    let _ = reader.i8();
    let _ = reader.u16();
    let _ = reader.u32();
    let _ = reader.u64();
    let _ = reader.f32();
    let _ = reader.f64();
    let _ = reader.varint();
    let _ = reader.zigzag();
    prop_assert!(reader.remaining() <= bytes.len());
    let mut mask = Vec::new();
    for count in [0usize, 1, 7, 8, 9, 512, usize::MAX] {
        let mut reader = Reader::new(bytes);
        if reader.bitmask_into(count, &mut mask).is_ok() {
            prop_assert_eq!(mask.len(), count);
        }
    }

    // --- Frame-level decoders that own the whole buffer.
    let _ = Handshake::decode(bytes);
    for &byte in bytes.iter().take(8) {
        let _ = FrameKind::from_tag(byte);
    }

    // --- Frame-level decoders that read from a cursor, each from the start of the buffer.
    {
        let mut reader = Reader::new(bytes);
        let _ = Challenge::decode(&mut reader);
    }
    let mut reader = Reader::new(bytes);
    let _ = Welcome::decode(&mut reader);
    let mut reader = Reader::new(bytes);
    let _ = Ping::decode(&mut reader);
    let mut reader = Reader::new(bytes);
    let _ = Pong::decode(&mut reader);
    let mut reader = Reader::new(bytes);
    let _ = decode_manifest_full(&mut reader);
    let mut reader = Reader::new(bytes);
    let _ = decode_manifest_delta(&mut reader);
    let mut reader = Reader::new(bytes);
    let _ = decode_interest_delta(&mut reader);
    let mut reader = Reader::new(bytes);
    let _ = decode_interest_table(&mut reader);

    // --- The block path, which is where a length off the wire meets a cursor.
    let mut reader = Reader::new(bytes);
    if let Ok(header) = FrameHeader::decode(&mut reader) {
        let frame_tick = u64::from(header.tick);
        let mut scratch = Vec::new();
        let base = vec![0u8; stride];
        let mut out = vec![0u8; stride];
        // A row shorter than the schema's native stride is the receiver-side mismatch every
        // block decoder has to refuse rather than shear.
        let mut short = vec![0u8; stride / 2];

        // Read blocks until one refuses, bounded by the count the header claims.
        for _ in 0..header.entity_count.min(8) {
            let mut probe = reader.clone();
            let Ok(meta) = decode_state_block_meta(&mut probe, frame_tick) else {
                break;
            };
            prop_assert!(meta.body_len <= probe.remaining());
            let mut skipping = probe.clone();
            prop_assert!(skip_state_block_body(&mut skipping, &meta).is_ok());
            let mut full_row = probe.clone();
            let _ = decode_state_block_into(
                &mut full_row,
                &meta,
                props,
                &mut scratch,
                Some(&base),
                &mut out,
            );
            let mut no_base = probe.clone();
            let _ =
                decode_state_block_into(&mut no_base, &meta, props, &mut scratch, None, &mut out);
            let mut short_row = probe.clone();
            let _ = decode_state_block_into(
                &mut short_row,
                &meta,
                props,
                &mut scratch,
                Some(&base),
                &mut short,
            );
            reader = skipping;
        }

        // The input lane reads the same buffer, because the header's kind is whatever the bytes
        // said and a receiver must not be steered into the wrong block decoder by it.
        let mut reader = Reader::new(bytes);
        if FrameHeader::decode(&mut reader).is_ok() {
            for _ in 0..header.entity_count.min(8) {
                let mut probe = reader.clone();
                let Ok(meta) = decode_input_block_meta(&mut probe, frame_tick) else {
                    break;
                };
                prop_assert!(meta.body_len <= probe.remaining());
                for index in 0..meta.count.min(8) {
                    if let Some(row) = input_block_row(&probe, &meta, wire, index) {
                        prop_assert_eq!(row.len(), wire);
                        prop_assert!(decode_row(props, row, &mut out).is_some());
                    }
                }
                // A zero stride is what a caller holding an empty schema passes, and it must
                // answer `None` rather than dividing or multiplying by it.
                prop_assert!(input_block_row(&probe, &meta, 0, 0).is_none());
                prop_assert!(skip_input_block_body(&mut probe, &meta).is_ok());
                reader = probe;
            }
        }
    }

    // --- Row-level decoders, straight onto the raw buffer.
    let mut out = vec![0u8; stride];
    if let Some(consumed) = decode_row(props, bytes, &mut out) {
        prop_assert_eq!(consumed, wire);
    }
    let mut short = vec![0u8; stride / 2];
    prop_assert!(decode_row(props, bytes, &mut short).is_none() || stride == 0);
    for bits in 0..=props.len() {
        let mask: Vec<bool> = (0..props.len()).map(|index| index < bits).collect();
        let mut row = vec![0u8; stride];
        let _ = apply_masked_wire(props, &mask, bytes, &mut row);
        let mut native = vec![0u8; stride];
        let _ = apply_masked(props, &mask, bytes, &mut native);
    }
    // A mask longer and shorter than the schema: both zip against it and must not index past it.
    let long: Vec<bool> = (0..props.len() * 2).map(|index| index % 2 == 0).collect();
    let mut row = vec![0u8; stride];
    let _ = apply_masked_wire(props, &long, bytes, &mut row);
    let _ = apply_masked(props, &[], bytes, &mut row);
    let _ = row_is_finite(props, bytes);

    // --- `auth`, which meets a hostile datagram before any decoder above does.
    //
    // The key is fixed rather than generated. The property is over the bytes: a receiver holds one
    // key for a whole session and judges every datagram against it, and a generated key only
    // changes which tag is the one in 2^64 that verifies.
    let key = [0x5au8; KEY_LEN];
    let mut plain: Vec<u8> = Vec::new();
    let mut session = SessionAuth::new(key);
    for direction in [Direction::ToServer, Direction::ToClient] {
        match session.open(direction, bytes, &mut plain) {
            // Reached only by a buffer that happens to carry a tag this key produces, which is
            // what the sealed corpus below supplies. A payload that opened is the datagram with
            // the trailer stripped, so its length is fixed by the datagram's.
            Ok(payload) => prop_assert_eq!(payload.len(), bytes.len() - TRAILER_LEN),
            Err(error) => prop_assert!(matches!(
                error,
                AuthError::Truncated | AuthError::BadTag | AuthError::Replayed
            )),
        }
    }
    // Random bytes stop at `BadTag`, which is what a MAC is for, so the code past that check is
    // reached by sealing under the same key and opening the result.
    let mut sealed = bytes.to_vec();
    let mut sender = SessionAuth::new(key);
    if sender.seal(Direction::ToServer, &mut sealed).is_some() {
        let mut receiver = SessionAuth::new(key);
        prop_assert_eq!(
            receiver.open(Direction::ToServer, &sealed, &mut plain),
            Ok(bytes)
        );
        // The same datagram a second time is the replay the window refuses.
        prop_assert_eq!(
            receiver.open(Direction::ToServer, &sealed, &mut plain),
            Err(AuthError::Replayed)
        );
        // The other direction is a different tag over the same bytes, which is what stops a peer's
        // own datagram from being reflected back at it.
        let mut reflected = SessionAuth::new(key);
        prop_assert_eq!(
            reflected.open(Direction::ToClient, &sealed, &mut plain),
            Err(AuthError::BadTag)
        );
        // Every truncation of a datagram that would otherwise have verified. The cut is clamped
        // below the full length, so each one has to be refused.
        let last = sealed.len() - 1;
        for cut in [0, 1, TRAILER_LEN - 1, TRAILER_LEN, sealed.len() / 2, last] {
            let mut truncated = SessionAuth::new(key);
            prop_assert!(truncated
                .open(Direction::ToServer, &sealed[..cut.min(last)], &mut plain)
                .is_err());
        }
    }

    // --- The same sweep under a session secret, where the payload is ciphertext and the tag is
    // Poly1305. The decoder past it is the same one, so what this adds is the cipher's own refusals:
    // arbitrary bytes presented as a ciphertext, and every truncation of one that would have opened.
    let cipher_key = derive_cipher_key(&compress_secret(b"wire-properties"), &[0x5au8; KEY_LEN]);
    let mut encrypting = SessionAuth::encrypted(key, cipher_key);
    for direction in [Direction::ToServer, Direction::ToClient] {
        match encrypting.open(direction, bytes, &mut plain) {
            // Unreachable in practice — it needs a Poly1305 tag over bytes nobody encrypted — and
            // asserted rather than assumed, because a decryption that answered on a failed tag would
            // land here with unauthenticated plaintext.
            Ok(payload) => {
                prop_assert_eq!(payload.len(), bytes.len() - CIPHER_TRAILER_LEN);
            }
            Err(error) => prop_assert!(matches!(
                error,
                AuthError::Truncated | AuthError::BadTag | AuthError::Replayed
            )),
        }
    }
    let mut encrypted = bytes.to_vec();
    let mut cipher_sender = SessionAuth::encrypted(key, cipher_key);
    if cipher_sender
        .seal(Direction::ToServer, &mut encrypted)
        .is_some()
    {
        // The ciphertext is exactly as long as the plaintext, and the trailer is what a receiver
        // needs to reconstruct the nonce. A datagram of the length below carries neither more nor
        // less than that.
        prop_assert_eq!(encrypted.len(), bytes.len() + CIPHER_TRAILER_LEN);
        let mut receiver = SessionAuth::encrypted(key, cipher_key);
        prop_assert_eq!(
            receiver.open(Direction::ToServer, &encrypted, &mut plain),
            Ok(bytes)
        );
        prop_assert_eq!(
            receiver.open(Direction::ToServer, &encrypted, &mut plain),
            Err(AuthError::Replayed)
        );
        let mut reflected = SessionAuth::encrypted(key, cipher_key);
        prop_assert_eq!(
            reflected.open(Direction::ToClient, &encrypted, &mut plain),
            Err(AuthError::BadTag)
        );
        let last = encrypted.len() - 1;
        for cut in [
            0,
            1,
            CIPHER_TRAILER_LEN - 1,
            CIPHER_TRAILER_LEN,
            encrypted.len() / 2,
            last,
        ] {
            let mut truncated = SessionAuth::encrypted(key, cipher_key);
            prop_assert!(truncated
                .open(Direction::ToServer, &encrypted[..cut.min(last)], &mut plain)
                .is_err());
        }
        // One alteration in each region of the datagram rather than at every index. The ciphertext,
        // the sequence number and the tag are all covered by Poly1305, so each has to be refused and
        // has to leave the scratch empty rather than holding plaintext nobody authenticated.
        // Sweeping every byte of every generated datagram costs this suite ten times its run.
        // `auth.rs`'s own `an_altered_sequence_number_is_refused_rather_than_decrypted` sweeps a
        // fixed datagram once, which is where that finer-grained coverage belongs.
        let split = encrypted.len() - CIPHER_TRAILER_LEN;
        let positions = [
            0, // the first ciphertext byte, or the sequence number of an empty one
            split.saturating_sub(1),
            split, // the low byte of the sequence number
            split + SEQ_LEN - 1,
            split + SEQ_LEN, // the first tag byte
            last,
        ];
        for index in positions {
            let mut altered = encrypted.clone();
            altered[index] ^= 0x01;
            let mut receiver = SessionAuth::encrypted(key, cipher_key);
            prop_assert_eq!(
                receiver.open(Direction::ToServer, &altered, &mut plain),
                Err(AuthError::BadTag),
                "byte {} of an encrypted datagram",
                index
            );
            prop_assert!(plain.is_empty());
        }
    }
    // A sequence number is a `u32` the sender chose, including one far past the window, which is
    // where a shift wider than the bitmap would panic under `overflow-checks`. Four arbitrary bytes
    // are a uniform `u32`, so a jump of 64 or more is the common case rather than a rare one: one
    // sweep run feeds about 16,400 sequences and about 1,100 of them advance the window that far.
    let mut window = ReplayWindow::new();
    for four in bytes.chunks_exact(4) {
        let seq = u32::from_le_bytes([four[0], four[1], four[2], four[3]]);
        if window.accept(seq) {
            prop_assert!(window.newest() >= seq);
            // Accepting the same sequence twice in a row is the replay the window exists to refuse.
            prop_assert!(!window.accept(seq));
        }
    }
    // `SipHasher::write` documents that any split of the same bytes produces the tag one call
    // produces, and a receiver feeds it a payload whose length the sender chose.
    let one_shot = siphash24(&key, bytes);
    for split in [0usize, 1, 7, 8, 9, bytes.len()] {
        let at = split.min(bytes.len());
        let mut hasher = SipHasher::new(&key);
        hasher.write(&bytes[..at]);
        hasher.write(&bytes[at..]);
        prop_assert_eq!(hasher.finish(), one_shot);
    }
    let _ = compress_secret(bytes);

    // --- The two scalar decoders that take wire bits directly.
    for pair in bytes.chunks_exact(2) {
        let _ = f16_bits_to_f32(u16::from_le_bytes([pair[0], pair[1]]));
    }
    for six in bytes.chunks_exact(6) {
        let quat = ss3_to_quat([six[0], six[1], six[2], six[3], six[4], six[5]]);
        // Documented as total over hostile bytes: the payload clamps and the quaternion
        // renormalizes, so no byte pattern can put a poison float into a rotation.
        prop_assert!(quat.iter().all(|component| component.is_finite()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Completeness guards. Plain tests — nothing here is generated.
// ---------------------------------------------------------------------------

#[test]
fn every_enumeration_the_properties_sweep_is_complete() {
    for (index, kind) in ALL_KINDS.into_iter().enumerate() {
        assert_eq!(
            kind_index(kind),
            index,
            "ALL_KINDS is out of order at {index}"
        );
    }
    for (index, quant) in ALL_QUANTS.into_iter().enumerate() {
        assert_eq!(
            quant_index(quant),
            index,
            "ALL_QUANTS is out of order at {index}"
        );
    }
    for (index, kind) in ALL_FRAME_KINDS.into_iter().enumerate() {
        assert_eq!(
            frame_kind_index(kind),
            index,
            "ALL_FRAME_KINDS is out of order at {index}"
        );
    }
    // 10 kinds lossless, plus Ss3 over Quat and Basis, plus Half over Vec3, Vec2 and F32.
    assert_eq!(pairings().len(), 15);
    assert_eq!(every_pairing_schema().len(), 15);
}

#[test]
fn every_frame_kind_tag_round_trips() {
    for kind in ALL_FRAME_KINDS {
        assert_eq!(FrameKind::from_tag(kind.tag()), Ok(kind));
    }
}

// ---------------------------------------------------------------------------
// Round-trip properties.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config(CASES))]

    /// Encode-then-decode is the identity for **every `PropKind` and every `QuantKind` pairing**,
    /// one pairing at a time, so a failure names the pairing that broke.
    #[test]
    fn every_pairing_survives_encode_then_decode(
        index in 0usize..pairings().len(),
        bytes in prop::collection::vec(any::<u8>(), 48),
    ) {
        let (kind, quant) = pairings()[index];
        let mut schema = SchemaBuilder::new();
        schema.push_quantized("p", kind, PropRole::State, quant);
        let props = schema.props();
        let mut row = bytes[..schema.row_stride()].to_vec();
        canonicalize_row(props, &mut row);

        let mut wire = Vec::new();
        encode_row(props, &row, &mut wire);
        prop_assert_eq!(wire.len(), wire_row_stride(props));

        let mut decoded = vec![0u8; schema.row_stride()];
        prop_assert_eq!(decode_row(props, &wire, &mut decoded), Some(wire.len()));
        prop_assert_eq!(&decoded, &row);
        // A quantized value is finite whatever the bytes were. Both quantizers map every
        // non-finite pattern to a finite one, so an `@`-annotated property cannot carry poison in.
        if quant != QuantKind::None {
            prop_assert!(row_is_finite(props, &decoded));
        }
    }

    /// The same identity over one row holding every pairing at once, which is the layout a fat
    /// channel actually sends.
    #[test]
    fn a_row_of_every_pairing_survives_encode_then_decode(
        bytes in prop::collection::vec(any::<u8>(), every_pairing_schema().row_stride()),
    ) {
        let schema = every_pairing_schema();
        let props = schema.props();
        let mut row = bytes;
        canonicalize_row(props, &mut row);

        let mut wire = Vec::new();
        encode_row(props, &row, &mut wire);
        let mut decoded = vec![0u8; schema.row_stride()];
        prop_assert_eq!(decode_row(props, &wire, &mut decoded), Some(wire.len()));
        prop_assert_eq!(&decoded, &row);
    }

    /// A masked delta against a **bit-exact base reconstructs the base's successor**, on the wire
    /// path: `changed_mask` decides, `write_masked_wire` sends, `apply_masked_wire` applies.
    #[test]
    fn a_masked_wire_delta_reconstructs_the_successor(
        (schema, rows) in arb_schema_and_rows(6, 2),
    ) {
        let props = schema.props();
        let mut base = rows[0].clone();
        let mut next = rows[1].clone();
        canonicalize_row(props, &mut base);
        canonicalize_row(props, &mut next);

        let mut mask = Vec::new();
        changed_mask(props, &base, &next, &mut mask);
        prop_assert_eq!(mask.len(), props.len());

        let mut payload = Vec::new();
        write_masked_wire(props, &mask, &next, &mut payload);
        prop_assert_eq!(payload.len(), masked_wire_size(props, &mask));

        let mut reconstructed = base.clone();
        prop_assert_eq!(
            apply_masked_wire(props, &mask, &payload, &mut reconstructed),
            Some(payload.len())
        );
        prop_assert_eq!(&reconstructed, &next);
    }

    /// The same property on the **native** path the columnar history uses, where no quantizer sits
    /// between the two rows.
    #[test]
    fn a_masked_native_delta_reconstructs_the_successor(
        (schema, rows) in arb_schema_and_rows(6, 2),
    ) {
        let props = schema.props();
        let base = rows[0].clone();
        let next = rows[1].clone();

        let mut mask = Vec::new();
        changed_mask(props, &base, &next, &mut mask);
        let mut payload = Vec::new();
        write_masked(props, &mask, &next, &mut payload);
        prop_assert_eq!(payload.len(), masked_size(props, &mask));

        let mut reconstructed = base.clone();
        prop_assert_eq!(
            apply_masked(props, &mask, &payload, &mut reconstructed),
            Some(payload.len())
        );
        prop_assert_eq!(&reconstructed, &next);
    }

    /// The same reconstruction through the **whole block codec**: encode a delta block against a
    /// base, decode its meta, apply its body, and land on the successor.
    #[test]
    fn a_state_block_delta_reconstructs_the_successor(
        (schema, rows) in arb_schema_and_rows(6, 2),
        slot in any::<u16>(),
        entity_tick in 1u64..=1_000_000,
        lead in 0u64..=64,
        reference_lag in 1u64..=64,
        state_lane in any::<bool>(),
    ) {
        let props = schema.props();
        let mut base = rows[0].clone();
        let mut next = rows[1].clone();
        canonicalize_row(props, &mut base);
        canonicalize_row(props, &mut next);
        let frame_tick = entity_tick + lead;
        let reference_tick = entity_tick.saturating_sub(reference_lag).min(entity_tick - 1);

        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        let full = encode_state_block(
            &mut writer,
            &mut scratch,
            props,
            slot,
            frame_tick,
            entity_tick,
            Some((reference_tick, &base)),
            &next,
            state_lane,
        );
        prop_assert!(!full, "a reference at an earlier tick must take the delta branch");

        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        let meta = decode_state_block_meta(&mut reader, frame_tick)?;
        prop_assert_eq!(meta.slot, slot);
        prop_assert_eq!(meta.tick, entity_tick);
        prop_assert_eq!(meta.state_lane, state_lane);
        prop_assert_eq!(meta.reference_tick, Some(reference_tick));
        prop_assert!(!meta.full);

        let mut reconstructed = vec![0u8; schema.row_stride()];
        prop_assert_eq!(
            decode_state_block_into(
                &mut reader,
                &meta,
                props,
                &mut scratch,
                Some(&base),
                &mut reconstructed,
            )?,
            true
        );
        prop_assert_eq!(&reconstructed, &next);
        prop_assert!(reader.is_exhausted());
    }

    /// A full block carries the whole row and needs no base.
    #[test]
    fn a_full_state_block_reconstructs_the_row(
        (schema, rows) in arb_schema_and_rows(6, 1),
        slot in any::<u16>(),
        entity_tick in 0u64..=1_000_000,
        lead in 0u64..=64,
        state_lane in any::<bool>(),
    ) {
        let props = schema.props();
        let mut row = rows[0].clone();
        canonicalize_row(props, &mut row);
        let frame_tick = entity_tick + lead;

        let mut writer = Writer::new();
        let mut scratch = Vec::new();
        let full = encode_state_block(
            &mut writer, &mut scratch, props, slot, frame_tick, entity_tick, None, &row, state_lane,
        );
        prop_assert!(full);

        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        let meta = decode_state_block_meta(&mut reader, frame_tick)?;
        prop_assert!(meta.full);
        prop_assert_eq!(meta.reference_tick, None);
        prop_assert_eq!(meta.tick, entity_tick);

        let mut reconstructed = vec![0u8; schema.row_stride()];
        prop_assert_eq!(
            decode_state_block_into(&mut reader, &meta, props, &mut scratch, None, &mut reconstructed)?,
            true
        );
        prop_assert_eq!(&reconstructed, &row);
    }

    /// Every row an input block carries comes back, in the order it was written.
    #[test]
    fn an_input_block_round_trips_every_row_it_carries(
        (schema, rows) in arb_schema_and_rows(6, 4),
        slot in any::<u16>(),
        frame_tick in 0u64..=1_000_000,
        lead in -64i64..=64,
    ) {
        let props = schema.props();
        let canonical: Vec<Vec<u8>> = rows
            .into_iter()
            .map(|mut row| {
                canonicalize_row(props, &mut row);
                row
            })
            .collect();
        let borrowed: Vec<&[u8]> = canonical.iter().map(Vec::as_slice).collect();
        let newest_tick = frame_tick.saturating_add_signed(lead);

        let mut writer = Writer::new();
        encode_input_block(&mut writer, props, slot, frame_tick, newest_tick, &borrowed);
        let bytes = writer.into_inner();

        let mut reader = Reader::new(&bytes);
        let meta = decode_input_block_meta(&mut reader, frame_tick)?;
        prop_assert_eq!(meta.slot, slot);
        prop_assert_eq!(meta.newest_tick, newest_tick);
        prop_assert_eq!(meta.count as usize, canonical.len());

        let wire = wire_row_stride(props);
        for (index, expected) in canonical.iter().enumerate() {
            let row = input_block_row(&reader, &meta, wire, index as u32)
                .expect("a well-formed block answers every row it counted");
            let mut decoded = vec![0u8; schema.row_stride()];
            prop_assert_eq!(decode_row(props, row, &mut decoded), Some(wire));
            prop_assert_eq!(&decoded, expected);
        }
        prop_assert!(skip_input_block_body(&mut reader, &meta).is_ok());
        prop_assert!(reader.is_exhausted());
    }

    /// The hot-frame header survives its zigzag ack delta at every `(tick, ack_tick)` pairing.
    #[test]
    fn a_frame_header_round_trips(header in arb_header()) {
        let mut writer = Writer::new();
        header.encode(&mut writer);
        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(FrameHeader::decode(&mut reader)?, header);
        prop_assert!(reader.is_exhausted());
    }

    /// The handshake, including the two trailing optional fields.
    #[test]
    fn a_handshake_round_trips(handshake in arb_handshake()) {
        prop_assert_eq!(Handshake::decode(&handshake.encode())?, handshake);
    }

    /// The challenge, which carries both nonce halves and the acceptor's exchange key. Fixed width, so
    /// the property that matters is that no field is swapped for another on the way back -- a swap is
    /// invisible to a length check and would key the two ends differently.
    #[test]
    fn a_challenge_round_trips(
        joiner_nonce in any::<[u8; KEY_LEN]>(),
        acceptor_nonce in any::<[u8; KEY_LEN]>(),
        acceptor_exchange in any::<[u8; EXCHANGE_KEY_LEN]>(),
    ) {
        let challenge = Challenge { joiner_nonce, acceptor_nonce, acceptor_exchange };
        let bytes = challenge.encode();
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::Challenge.tag());
        prop_assert_eq!(Challenge::decode(&mut reader)?, challenge);
        prop_assert!(reader.is_exhausted());
    }

    /// The three control frames that carry a clock or a join reply.
    #[test]
    fn the_control_frames_round_trip(
        protocol_version in any::<u32>(),
        server_tick in any::<u64>(),
        tickrate in any::<u16>(),
        resume_token in any::<u64>(),
        seq in any::<u64>(),
        client_us in any::<u64>(),
        server_time in any::<f64>(),
    ) {
        let welcome = Welcome { protocol_version, server_tick, tickrate, resume_token };
        let bytes = welcome.encode();
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::Welcome.tag());
        prop_assert_eq!(Welcome::decode(&mut reader)?, welcome);

        let ping = Ping { seq, client_us };
        let bytes = ping.encode();
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::Ping.tag());
        prop_assert_eq!(Ping::decode(&mut reader)?, ping);

        let pong = Pong { seq, client_us, server_time };
        let bytes = pong.encode();
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::Pong.tag());
        let decoded = Pong::decode(&mut reader)?;
        prop_assert_eq!(decoded.seq, pong.seq);
        prop_assert_eq!(decoded.client_us, pong.client_us);
        // Compared as bits: a NaN server time must survive the wire unchanged, and NaN is not
        // equal to itself.
        prop_assert_eq!(decoded.server_time.to_bits(), server_time.to_bits());
    }

    /// Both interest frames, which carry the same slot-run shape behind different generations.
    #[test]
    fn the_interest_frames_round_trip(
        generation in any::<u64>(),
        left in arb_slots(32),
        entered in arb_slots(32),
    ) {
        let mut writer = Writer::new();
        encode_interest_delta(generation, &left, &entered, &mut writer);
        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        let section = decode_interest_delta(&mut reader)?;
        prop_assert_eq!(&section, &InterestDeltaSection { generation, left: left.clone(), entered });
        prop_assert!(reader.is_exhausted());
        prop_assert!(section.applies_to(generation));

        let bytes = encode_interest_table(generation, &left);
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::InterestTable.tag());
        prop_assert_eq!(decode_interest_table(&mut reader)?, (generation, left));
        prop_assert!(reader.is_exhausted());
    }

    /// An owner the wire cannot represent reaches the receiver as **unowned**, which is the
    /// documented normalization rather than a wrapped peer id naming somebody.
    #[test]
    fn a_negative_owner_reaches_the_wire_as_unowned(
        slot in any::<u16>(),
        id in 1u64..,
        owner in i32::MIN..0i32,
    ) {
        let entry = ManifestEntry { slot, id, state_hash: 0, input_hash: 0, owner, seat: 0 };
        let bytes = encode_manifest_full(0, &[entry]);
        let mut reader = Reader::new(&bytes);
        reader.u8()?;
        let (_, rows) = decode_manifest_full(&mut reader)?;
        prop_assert_eq!(rows[0].owner, 0);
        prop_assert_eq!(rows[0].slot, slot);
        prop_assert_eq!(rows[0].id, id);
    }

    /// The schema hash **separates any two schemas that differ**, in kind, role, quantizer, name or
    /// order. That is the whole guarantee the manifest's schema check rests on.
    #[test]
    fn the_schema_hash_separates_schemas_that_differ(
        left in arb_schema(6),
        right in arb_schema(6),
    ) {
        let same = left.props() == right.props();
        if !same && left.hash() == right.hash() {
            // FNV-1a is 32 bits, so a collision is possible rather than impossible. Report the
            // pair instead of failing on one: what this property is for is an accidental
            // insensitivity to a whole field, which shows up as a collision that shrinks to two
            // schemas differing in exactly that field.
            let differs_by_one_field = left.props().len() == right.props().len()
                && left
                    .props()
                    .iter()
                    .zip(right.props())
                    .filter(|(a, b)| a != b)
                    .count()
                    == 1;
            prop_assert!(
                !differs_by_one_field,
                "two schemas differing in one property hash the same: {:?} vs {:?}",
                left.props(),
                right.props()
            );
        }
        if same {
            prop_assert_eq!(left.hash(), right.hash());
            prop_assert_eq!(left.row_stride(), right.row_stride());
        }
    }
}

proptest! {
    #![proptest_config(config(ORIENTATION_CASES))]

    /// A **near-balanced orientation** stores the bytes a receiver reconstructs, under both `@ss3`
    /// carriers.
    ///
    /// This is the invariant `canonicalize_value` iterates for, and the one a single round trip does
    /// not reach: a sender keeps the row it captured and a receiver keeps what it rebuilt from the
    /// wire, and a masked delta is computed against the sender's copy. A one-quantum disagreement
    /// makes every later mask wrong for as long as the pose holds.
    #[test]
    fn a_near_balanced_orientation_stores_what_the_wire_reconstructs(
        q in arb_near_balanced_quat(),
    ) {
        for kind in [PropKind::Quat, PropKind::Basis] {
            let mut schema = SchemaBuilder::new();
            schema.push_quantized("facing", kind, PropRole::State, QuantKind::Ss3);
            let props = schema.props();

            let mut row = Vec::with_capacity(kind.stride());
            match kind {
                PropKind::Basis => {
                    for component in quat_to_basis(q) {
                        row.extend_from_slice(&component.to_le_bytes());
                    }
                }
                _ => {
                    for component in q {
                        row.extend_from_slice(&component.to_le_bytes());
                    }
                }
            }
            prop_assert_eq!(row.len(), schema.row_stride());
            canonicalize_row(props, &mut row);

            let mut wire = Vec::new();
            encode_row(props, &row, &mut wire);
            prop_assert_eq!(wire.len(), 6);
            let mut reconstructed = vec![0u8; schema.row_stride()];
            prop_assert_eq!(decode_row(props, &wire, &mut reconstructed), Some(wire.len()));
            prop_assert_eq!(
                &reconstructed,
                &row,
                "a canonicalized {:?} row differs from what the wire rebuilds",
                kind
            );
        }
    }
}

proptest! {
    #![proptest_config(config(HEAVY_CASES))]

    /// **A manifest delta applied to a generation lands on the table the full frame carries.**
    ///
    /// Diffs two tables, round-trips the delta, applies it to the table it was diffed from, and
    /// compares the result against the full frame for the same generation decoded from its own
    /// bytes.
    #[test]
    fn a_manifest_delta_reaches_the_table_the_full_frame_carries(
        previous in arb_manifest(24),
        current in arb_manifest(24),
        base_generation in any::<u64>(),
        step in 1u64..=1024,
    ) {
        let generation = base_generation.wrapping_add(step);
        let (removed, added) = diff_manifest(&previous, &current);
        let delta = ManifestDelta { base_generation, generation, removed, added };

        let bytes = encode_manifest_delta(&delta);
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::EntityManifestDelta.tag());
        let decoded = decode_manifest_delta(&mut reader)?;
        prop_assert_eq!(&decoded, &delta);
        prop_assert!(reader.is_exhausted());
        prop_assert!(decoded.applies_to(base_generation));
        prop_assert!(!decoded.applies_to(generation));

        let bytes = encode_manifest_full(generation, &current);
        let mut reader = Reader::new(&bytes);
        prop_assert_eq!(reader.u8()?, FrameKind::EntityManifest.tag());
        let (full_generation, full_rows) = decode_manifest_full(&mut reader)?;
        prop_assert_eq!(full_generation, generation);
        prop_assert!(reader.is_exhausted());

        prop_assert_eq!(apply_manifest_delta(&previous, &decoded), full_rows);
    }

    /// The same delta, applied to a `SlotTable`, binds what the full frame binds.
    ///
    /// The manifest is the only channel that tells a client what a wire slot names, so the table
    /// the two paths reach has to agree binding for binding — a client that took the delta path
    /// and one that was sent the whole frame decode the same block to the same entity.
    #[test]
    fn a_manifest_delta_binds_the_slot_table_the_full_frame_binds(
        previous in arb_manifest(24),
        current in arb_manifest(24),
    ) {
        let (removed, added) = diff_manifest(&previous, &current);
        let delta = ManifestDelta { base_generation: 1, generation: 2, removed, added };

        let mut from_delta = SlotTable::new();
        for entry in &previous {
            from_delta.bind(entry.slot, entry.id);
        }
        for &slot in &delta.removed {
            from_delta.unbind(slot);
        }
        for entry in &delta.added {
            from_delta.bind(entry.slot, entry.id);
        }

        let mut from_full = SlotTable::new();
        for entry in &current {
            from_full.bind(entry.slot, entry.id);
        }

        let delta_bindings: Vec<(u16, u64)> = from_delta.bindings().collect();
        let full_bindings: Vec<(u16, u64)> = from_full.bindings().collect();
        prop_assert_eq!(&delta_bindings, &full_bindings);
        for (slot, id) in full_bindings {
            prop_assert_eq!(from_delta.id_of(slot), Some(id));
            prop_assert_eq!(from_delta.slot_of(id), Some(slot));
        }
    }

    /// A manifest off the wire — hostile rows, duplicate slots, duplicate ids — binds a
    /// `SlotTable` without panicking, and leaves `slot_of` and `id_of` agreeing.
    ///
    /// The bijection a server publishes is not something a receiver may assume: these bytes are
    /// chosen by the sender.
    ///
    /// **Corrupted manifest frames rather than uniform random bytes.** Measured over 2,000 cases
    /// each: uniform random buffers of 0 to 512 bytes decoded as a full manifest 6.0% of the time
    /// and as a delta 1.9% of the time, which at `HEAVY_CASES` is about one case per run reaching
    /// the `apply_manifest_delta` assertions below. A valid manifest frame carrying up to four
    /// edits decodes 26% and 22% of the time, so the bind/unbind body and those assertions are
    /// what most cases reach. The leading tag byte is consumed first, because both decoders read
    /// from the byte after it.
    #[test]
    fn a_decoded_manifest_binds_a_slot_table_without_panicking(
        frame in prop_oneof![arb_manifest_full_frame(), arb_manifest_delta_frame()],
        edits in prop::collection::vec(arb_edit(), 0..=4),
    ) {
        let bytes = corrupt(frame, &edits);
        let mut reader = Reader::new(&bytes);
        let _ = reader.u8();
        if let Ok((_, rows)) = decode_manifest_full(&mut reader) {
            let mut table = SlotTable::new();
            for entry in rows.iter().take(256) {
                table.bind(entry.slot, entry.id);
            }
            for entry in rows.iter().take(256) {
                if let Some(id) = table.id_of(entry.slot) {
                    prop_assert_eq!(table.slot_of(id), Some(entry.slot));
                }
            }
            for entry in rows.iter().take(256) {
                table.unbind(entry.slot);
            }
        }
        let mut reader = Reader::new(&bytes);
        let _ = reader.u8();
        if let Ok(delta) = decode_manifest_delta(&mut reader) {
            let reached = apply_manifest_delta(&[], &delta);
            // Removals apply before additions, so a slot in both halves survives as its addition.
            prop_assert!(reached.len() <= delta.added.len());
            prop_assert!(reached.windows(2).all(|pair| pair[0].slot < pair[1].slot));
        }
    }
}

// ---------------------------------------------------------------------------
// Decoder sweeps.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config(CASES))]

    /// **Arbitrary bytes into every public decode entry point answer an error or a value.**
    #[test]
    fn arbitrary_bytes_never_panic_a_decoder(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        drive_every_decoder(&bytes, &sweep_schema())?;
    }

    /// The same sweep over a **corrupted valid frame**, which is what reaches the code a uniform
    /// random buffer never gets past.
    #[test]
    fn a_corrupted_frame_never_panics_a_decoder(
        frame in arb_valid_frame(),
        edits in prop::collection::vec(arb_edit(), 0..=6),
    ) {
        let bytes = corrupt(frame, &edits);
        drive_every_decoder(&bytes, &sweep_schema())?;
    }

    /// The same sweep over every **prefix** of a valid frame, which is the truncation a lost
    /// fragment or a shorter peer produces.
    #[test]
    fn a_truncated_frame_never_panics_a_decoder(
        frame in arb_valid_frame(),
        cut in any::<u16>(),
    ) {
        let keep = if frame.is_empty() { 0 } else { usize::from(cut) % (frame.len() + 1) };
        drive_every_decoder(&frame[..keep], &sweep_schema())?;
    }

    /// The same sweep against an **arbitrary schema**, because a receiver decodes with whatever
    /// schema its own game registered and the two ends can disagree about it.
    #[test]
    fn a_hostile_frame_never_panics_an_arbitrary_schema(
        schema in arb_schema(8),
        frame in arb_valid_frame(),
        edits in prop::collection::vec(arb_edit(), 0..=4),
    ) {
        let bytes = corrupt(frame, &edits);
        drive_every_decoder(&bytes, &schema)?;
    }

    /// The two varint readers **report rather than wrap**, and a value that decoded survives a
    /// second trip through the encoder.
    ///
    /// **LEB128 here is not canonical, deliberately stated as a property rather than assumed away.**
    /// `[0x80, 0x00]` is a two-byte spelling of `0` and this reader accepts it, so a decoded value
    /// does NOT have to re-encode to the bytes it came from. What it has to do is decode to the same
    /// value again, which is the only thing any caller depends on: nothing in the protocol compares
    /// two frames byte for byte, and `crate::auth` authenticates a datagram as it was sent rather
    /// than as it would be re-encoded.
    #[test]
    fn the_varint_readers_report_rather_than_wrap(
        bytes in prop::collection::vec(any::<u8>(), 0..24),
        value in any::<u64>(),
        signed in any::<i64>(),
    ) {
        // Encoder first: what this build writes, this build reads back, consuming exactly it.
        let mut writer = Writer::new();
        writer.varint(value);
        let encoded = writer.into_inner();
        prop_assert!(encoded.len() <= 10);
        let mut reader = Reader::new(&encoded);
        prop_assert_eq!(reader.varint()?, value);
        prop_assert!(reader.is_exhausted());

        let mut writer = Writer::new();
        writer.zigzag(signed);
        let encoded = writer.into_inner();
        let mut reader = Reader::new(&encoded);
        prop_assert_eq!(reader.zigzag()?, signed);
        prop_assert!(reader.is_exhausted());

        // Arbitrary bytes: one of two named errors, or a value that is idempotent under re-encoding.
        let mut reader = Reader::new(&bytes);
        match reader.varint() {
            Ok(decoded) => {
                prop_assert!(bytes.len() - reader.remaining() <= 10);
                let mut writer = Writer::new();
                writer.varint(decoded);
                let round = writer.into_inner();
                prop_assert_eq!(Reader::new(&round).varint()?, decoded);
            }
            Err(error) => prop_assert!(matches!(
                error,
                CodecError::UnexpectedEof | CodecError::VarintOverflow
            )),
        }
        let mut reader = Reader::new(&bytes);
        if let Ok(decoded) = reader.zigzag() {
            let mut writer = Writer::new();
            writer.zigzag(decoded);
            let round = writer.into_inner();
            prop_assert_eq!(Reader::new(&round).zigzag()?, decoded);
        }
    }

    /// A row of arbitrary native bytes answers `row_is_finite` without indexing past its end, at
    /// every length from empty to one byte over the stride.
    #[test]
    fn row_is_finite_reads_no_further_than_the_row(
        schema in arb_schema(8),
        bytes in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let props = schema.props();
        let stride = schema.row_stride();
        for length in [0usize, 1, stride / 2, stride, stride + 1] {
            let row = &bytes[..length.min(bytes.len())];
            let finite = row_is_finite(props, row);
            // A row shorter than the schema demands cannot be finite: there is no value to read.
            if row.len() < stride {
                prop_assert!(!finite);
            }
        }
    }
}
