//! OrbitNet core — the engine-agnostic half of the OrbitNet netcode addon.
//!
//! Nothing in this crate knows about Godot. Every type is a plain-data structure with pure
//! behavior, which is what lets the whole thing be exercised by `cargo test` in milliseconds
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
//! * [`auth`] — what the receive path refuses: the per-datagram MAC, the replay window, and the
//!   per-peer input budget.
//! * [`freshness`] — the #67 fix: per-(entity, tick) input confidence, so `is_fresh` keys on
//!   input *novelty* rather than tick visitation, plus the tick-indexed memo ring.
//! * [`interest`] — AOI: the uniform grid, per-seat interest sets with hysteresis, and the
//!   per-connection union of them.
//! * [`seats`] — which owned viewpoints a connection holds, and the add/remove diff both ends
//!   announce from.
//! * [`slots`] — the dense per-session entity index the wire carries in place of the 64-bit id.
//! * [`priority`] — the send rota: distance bands, weights, and `staleness × weight` ordering.
//! * [`pacing`] — coupled-mode tick slewing and the input-lead margin tracker.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod auth;
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
pub mod seats;
pub mod slots;
pub mod tick;

pub use auth::{
    compress_secret, confirm_tag, derive_session_key, AuthError, Direction, ReceiveBudget,
    ReplayWindow, SessionAuth, KEY_LEN,
};
pub use clock::ClockEstimator;
pub use codec::{CodecError, FrameHeader, FrameKind, Handshake, Reader, Writer};
pub use columnar::ColumnarHistory;
pub use freshness::{Confidence, FreshnessLedger, MemoRing};
pub use history::{plan_cost, BodyId, BodyResim, DirtyWindow, ResimPlanner, ResimRange, TickRing};
pub use interest::{
    membership_matches, AoiConfig, ConnectionInterest, InterestCandidate, InterestDelta,
    InterestGrid, InterestOccupancy, InterestPath, MembershipId, OccupancyScratch, PathSelector,
    PeerInterest, SeatObserver, SeatScratch, GRID_ENTER_SPANS, GRID_LEAVE_SPANS,
    GRID_MAX_OVERRIDES, MEMBERSHIP_GLOBAL,
};
pub use pacing::{CoupledSlew, LeadTracker, SlewDecision};
pub use priority::{Band, Candidate, WEIGHT_ONE, WEIGHT_OWNED};
pub use protocol::{PropKind, PropRole, PropSchema, QuantKind, SchemaBuilder, PROTOCOL_VERSION};
pub use quant::row_is_finite;
pub use seats::{
    releases_seats, SeatId, SeatIndex, SeatReleaseEvent, SeatReleasePolicy, SeatRoster,
};
pub use slots::{SlotError, SlotTable, MAX_SLOTS, SLOT_QUARANTINE_TICKS};
pub use tick::{TickAccumulator, TickRate, TickStep};
