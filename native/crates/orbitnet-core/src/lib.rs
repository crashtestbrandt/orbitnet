//! OrbitNet core — the engine-agnostic half of the OrbitNet netcode addon.
//!
//! Nothing in this crate knows about Godot. Every type is a plain-data structure with pure
//! behaviour, which is what lets the whole thing be exercised by `cargo test` in milliseconds
//! instead of standing up a scene tree, a physics world and two peers.
//!
//! The split mirrors the four costs that motivated moving off the GDScript backend (see
//! docs/architecture.md):
//!
//! * [`tick`] — the tick clock: fixed-rate stepping, catch-up bounding, sub-tick factor.
//! * [`clock`] — remote clock discipline: RTT/jitter estimation and bounded time stretch.
//! * [`history`] — tick-indexed ring storage plus the *per-body dirty window* that replaces the
//!   single global resim window, which is where the per-tick cost actually went.
//! * [`columnar`] — the packed per-entity history: one flat `Vec<u8>` of fixed-stride rows,
//!   `memcmp` changed-masks, masked merges. No `Variant`, no per-tick allocation.
//! * [`protocol`] — property schema description and the schema hash peers agree on.
//! * [`codec`] — the wire encoding: varints, frame headers, handshake, entity blocks.
//! * [`freshness`] — the #67 fix: per-(entity, tick) input confidence, so `is_fresh` keys on
//!   input *novelty* rather than tick visitation, plus the tick-indexed memo ring.
//! * [`interest`] — AOI: the uniform grid and per-peer interest sets with hysteresis.
//! * [`priority`] — the send rota: distance bands, weights, and `staleness × weight` ordering.
//! * [`pacing`] — coupled-mode tick slewing and the input-lead margin tracker.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clock;
pub mod codec;
pub mod columnar;
pub mod freshness;
pub mod history;
pub mod interest;
pub mod pacing;
pub mod priority;
pub mod protocol;
pub mod quant;
pub mod tick;

pub use clock::ClockEstimator;
pub use codec::{CodecError, FrameHeader, FrameKind, Handshake, Reader, Writer};
pub use columnar::ColumnarHistory;
pub use freshness::{Confidence, FreshnessLedger, MemoRing};
pub use history::{plan_cost, BodyId, BodyResim, DirtyWindow, ResimPlanner, ResimRange, TickRing};
pub use interest::{
    membership_matches, AoiConfig, InterestCandidate, InterestGrid, MembershipId, PeerInterest,
    MEMBERSHIP_GLOBAL,
};
pub use pacing::{CoupledSlew, LeadTracker, SlewDecision};
pub use priority::{Band, Candidate, WEIGHT_ONE, WEIGHT_OWNED};
pub use protocol::{PropKind, PropRole, PropSchema, QuantKind, SchemaBuilder, PROTOCOL_VERSION};
pub use tick::{TickAccumulator, TickRate, TickStep};
