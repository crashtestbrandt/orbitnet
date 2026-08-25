//! The OrbitNet session singleton.
//!
//! One node owns the whole netcode hot path: the tick clock, the per-entity rollback loop, the
//! packet pump, clock sync, and the diagnostics the `Net` facade republishes. The design is the
//! inverse of the backend it replaces: instead of every synchronizer subscribing to five signals
//! per rollback tick, `OrbitNet` iterates the entity registry (a `BTreeMap`, so replay order is
//! stable — the bit-exact resim gate would read a nondeterministic order as a phantom desync) and
//! calls plain methods. Per-entity dirty windows come from `orbitnet_core::ResimPlanner`, so one
//! late peer deepens only its own body's replay.
//!
//! Transport: `SceneMultiplayer.send_bytes()` + the `peer_packet` signal — one batched frame per
//! peer per tick, riding above the `MultiplayerPeer`, so ENet, Steam and Offline peers are
//! indistinguishable from here (docs/protocol.md).
//!
//! Re-entrancy: game code called from inside the loop (`_rollback_tick`, property setters) can
//! legally call back into this node through the `Net` facade (`current_tick()`,
//! `rollback_tick()`, memo reads). Every such callback happens either under a `base_mut()`
//! surrender guard or with no outstanding `bind` on the callee, which is what makes those
//! re-entrant calls safe rather than a borrow panic. Registration from game code is decoupled the
//! same way: synchronizers enqueue into a thread-local pending list that is drained at the top of
//! the next frame, never mid-loop.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use godot::classes::multiplayer_peer::TransferMode;
use godot::classes::{
    Crypto, Engine, MultiplayerApi, Node, RandomNumberGenerator, SceneMultiplayer, Time,
};
use godot::prelude::*;

use orbitnet_core::auth::{
    compress_secret, confirm_tag, derive_session_key, siphash24, MAX_INPUT_BLOCKS_PER_TICK,
    TRAILER_LEN,
};
use orbitnet_core::codec::{
    apply_manifest_delta, decode_input_block_meta, decode_interest_delta, decode_interest_table,
    decode_manifest_delta, decode_manifest_full, decode_state_block_meta, diff_manifest,
    encode_interest_delta, encode_interest_table, encode_manifest_delta, encode_manifest_full,
    input_block_row, skip_input_block_body, skip_state_block_body, FrameHeader, FrameKind,
    Handshake, InterestDeltaSection, ManifestDelta, ManifestEntry, Ping, Pong, Reader, Welcome,
    Writer, MAGIC, MAX_FRAME_PAYLOAD,
};
use orbitnet_core::interest::{
    ConnectionInterest, InterestCandidate, InterestDelta, InterestGrid, InterestOccupancy,
    InterestPath, MembershipId, OccupancyScratch, PathSelector, SeatObserver, SeatScratch,
    MEMBERSHIP_GLOBAL,
};
use orbitnet_core::priority::{self, Band};
use orbitnet_core::seats::{
    releases_seats, SeatId, SeatIndex, SeatReleaseEvent, SeatReleasePolicy, SeatRoster,
};
use orbitnet_core::slots::SlotTable;
use orbitnet_core::{
    AoiConfig, AuthError, ClockEstimator, CoupledSlew, Direction, LeadTracker, ReceiveBudget,
    ResimPlanner, SessionAuth, TickAccumulator, TickRate, KEY_LEN,
};

use crate::binding;
use crate::sync::{
    self, InputIntegration, OrbitRollbackSynchronizer, OrbitStateSynchronizer, StateIntegration,
    STATE_HISTORY_DEPTH,
};

/// Network role, mirroring the facade's `Net.Mode`.
const MODE_OFFLINE: i64 = 0;
const MODE_CLIENT: i64 = 1;
const MODE_SERVER: i64 = 2;
const MODE_HOST: i64 = 3;

pub(crate) const SERVER_PEER: i32 = 1;

/// `seat_release_policy` values, mirroring the facade's `Net.SeatRelease` and
/// `orbitnet_core::seats::SeatReleasePolicy`. The property is an `i64` because that is what an
/// exported enum crosses the script boundary as; [`seat_release_policy_of`] is the only place the
/// number becomes the core enum, and [`clamp_seat_release_policy`] is the only place an unknown one
/// is decided.
const SEAT_RELEASE_HOLD: i64 = 0;
const SEAT_RELEASE_ON_EXPIRY: i64 = 1;
const SEAT_RELEASE_ON_DROP: i64 = 2;

/// `resume_policy` values, mirroring the facade's `Net.ResumePolicy`. Which claims on a held or
/// still-connected identity this server will grant. [`resume_grant`] is the whole rule.
const RESUME_ALWAYS: i64 = 0;
const RESUME_ONLY_IF_DROPPED: i64 = 1;
const RESUME_NEVER: i64 = 2;

/// What a connection that resolved **no** interest anchor receives, mirroring the facade's
/// `Net.UnanchoredPolicy`. See [`OrbitNet::set_unanchored_policy`] for the carve-out that decides
/// which connections the CLOSED value can reach at all.
const UNANCHORED_OPEN: i64 = 0;
const UNANCHORED_CLOSED: i64 = 1;

/// The known `unanchored_policy` values, and OPEN for anything else.
///
/// Clamped on set rather than on read, for the reason [`clamp_seat_release_policy`] is: the getter
/// then reports the policy **in force**, and a caller that writes a number this build does not know
/// learns it by reading back. OPEN is the direction that is safe to be wrong in — it is today's
/// behavior and it withholds nothing from anybody.
#[must_use]
fn clamp_unanchored_policy(policy: i64) -> i64 {
    if policy == UNANCHORED_CLOSED {
        UNANCHORED_CLOSED
    } else {
        UNANCHORED_OPEN
    }
}

/// Where the anchor `peer_anchor_info` reports came from, mirroring the facade's `Net.AnchorSource`.
///
/// | Value | Meaning |
/// | --- | --- |
/// | `0` | no answer — the peer names no connection, or the interest pass has not run for it |
/// | `1` | inferred from the bodies the connection drives |
/// | `2` | a declared fixed position ([`OrbitNet::set_peer_anchor`]) |
/// | `3` | a declared tracked entity ([`OrbitNet::set_peer_anchor_entity`]) |
const ANCHOR_SOURCE_NONE: i64 = 0;
const ANCHOR_SOURCE_INFERRED: i64 = 1;
const ANCHOR_SOURCE_FIXED: i64 = 2;
const ANCHOR_SOURCE_ENTITY: i64 = 3;

/// Seconds between clock probes.
const PING_INTERVAL: f64 = 0.25;
/// Ticks between forced full-state blocks per entity (phase-offset by entity id).
const FULL_STATE_INTERVAL: u64 = 16;
/// Input rows carried per frame for loss armor.
const INPUT_REDUNDANCY: usize = 4;
/// Ticks of lead the client seeds ahead of the server's welcome tick.
const INITIAL_LEAD_TICKS: u64 = 2;

/// How far into the past a late-arriving input row may still start a server-side resim.
///
/// Unbounded, this was the join lurch: a fresh client's clock is seconds off until its first
/// hard resync, its input arrives stamped up to a full history window (128 ticks) in the past,
/// and the server obligingly replayed that body across seconds of history — broadcasting a
/// frontier pose that flailed at over 100 m/s with direction reversals to every other peer for
/// the whole settling window. Sixteen ticks (~267 ms at 60 Hz) covers every honest late arrival
/// — the adaptive lead holds input margins near zero, and the whole rewind family clamps at
/// 250 ms — while a row older than that is still INTEGRATED into history (later resims replay
/// through it truthfully); it just cannot start one.
const RESIM_INPUT_HORIZON_TICKS: u64 = 16;

/// How far into the future a client's input stamps may run and still be accepted.
///
/// The legitimate maximum is `net.input_delay`'s clamp (32) plus the dialed-in lead
/// (`INITIAL_LEAD_TICKS` + the 8-tick lead-bias clamp) plus jitter — about 44 ticks. Sixty-four
/// leaves headroom without accepting the near-full-history stamps a joiner with an unsettled
/// clock (or a hostile peer walking the ring frontier away) can produce.
const INPUT_FUTURE_HORIZON_TICKS: u64 = 64;
/// Clock offset beyond which slew/stretch corrections are hopeless and the client reseeks.
const HARD_RESYNC_SECONDS: f64 = 0.25;

/// The interest hysteresis band: an entity enters at `aoi_radius` and leaves only past 1.25x it.
const AOI_EXIT_FACTOR: f32 = 1.25;

/// Per-datagram overhead the payload counters cannot see, in bytes.
///
/// 28 B of IPv4 + UDP header, 12 B of ENet (a 4-byte protocol header plus an 8-byte
/// send-unreliable command header), and the 1-byte `NETWORK_COMMAND_RAW` tag
/// `SceneMultiplayer::send_bytes` prefixes. Stated rather than folded into the counters so
/// `tx_bytes_s` stays a number about OrbitNet and `tx_wire_bytes_s` stays a number about the link:
/// on a full 1200 B frame the difference is 3%, on a 90 B one it is over 40%, and a bandwidth
/// budget quoted in the wrong one of those is not a budget.
const WIRE_OVERHEAD_BYTES: u64 = 28 + 12 + 1;

/// Seconds per accounting window — the period the per-second bandwidth figures are averaged over.
const BANDWIDTH_WINDOW_SECONDS: f64 = 1.0;

/// One replicated entity as the send path sees it, gathered once per tick before any peer is
/// considered.
///
/// The point of this struct is the word *once*: the filter it replaces asked every entity for its
/// input authority — a Godot `get_multiplayer_authority()` round trip — once per entity **per
/// peer**, which at 100 peers is a hundredfold multiplier on a call no peer's answer depends on.
struct EntityRow {
    /// Stable entity id.
    id: u64,
    /// The peer whose input drives this entity, or 0 when nobody's does.
    owner: i32,
    /// Which of that peer's seats drives it. `0` unless the game declared otherwise, and
    /// meaningless when `owner` is 0.
    seat: SeatIndex,
    /// World-space interest anchor, or `None` when the entity declares none — which means it is
    /// unconditionally relevant *within its membership*, the direction that fails open.
    anchor: Option<[f32; 3]>,
    /// The world this entity is in, or [`MEMBERSHIP_GLOBAL`] for every world.
    ///
    /// A separate axis from `anchor`, and the two fail independently: an entity whose anchor did not
    /// resolve is still bounded to the world it declares, because a declaration is not a
    /// measurement and did not fail. See the `interest` module header in `orbitnet-core`.
    membership: MembershipId,
    /// Declared send priority, already clamped.
    priority: u32,
}

impl EntityRow {
    /// The seat this row is driven by. Meaningless for an unowned row, and never asked for one.
    #[must_use]
    fn seat_id(&self) -> SeatId {
        SeatId {
            peer: self.owner,
            seat: self.seat,
        }
    }
}

/// What one SEAT's interest is measured against: where it observes from, and which world it is in.
///
/// Both facts come from the **same row** — the lowest-id entity whose input authority is that seat's
/// peer, which declares that seat, and which resolved an anchor. A seat's membership has no home of
/// its own on the wire or in the registry, and taking it from the body that already anchors the
/// seat's radius keeps the two answers about one entity rather than about two that could disagree.
///
/// A seat with no such row has no entry here at all, and [`seat_observers_into`] then gives it
/// **no viewpoint** rather than an unlocatable one — while any other seat on the connection has
/// resolved. Fail-open is a per-CONNECTION rule: a connection where nothing resolved sees everything,
/// which is what protects a peer whose avatar has not spawned; a connection where one seat resolved
/// and another has not is not blanked by the one that has not. Per-seat fail-open made adding a seat
/// a full-world burst for the whole connection, because the connection's set is the union of its
/// seats'.
///
/// **The center is per seat, and that half stays per seat.** It used to be per connection: one
/// anchored seat supplied the center for the whole connection, so a second seat had its surroundings
/// culled around a position it was nowhere near.
#[derive(Clone, Copy)]
struct PeerObserver {
    /// The center this seat's interest radius is measured from.
    center: [f32; 3],
    /// The world this seat is in.
    membership: MembershipId,
    /// **The seat drove more than one anchored body**, so the two facts above came from an
    /// arbitrary — deterministic, but arbitrary — one of them. See [`OrbitNet::collect_observers`]
    /// for why the pick is not moving.
    ///
    /// Reported as `ambiguous` by [`OrbitNet::peer_anchor_info`] and **not warned about**. A game
    /// that swaps one body for another on a seat holds two of them for the frame the swap takes,
    /// and a warning there fires on every swap for a configuration that is correct.
    ambiguous: bool,
    /// The dropped rows disagreed about the **world**, which is the misconfiguration worth a log
    /// line. `ambiguous` is always true beside it.
    ///
    /// The cost is a whole-world swap for the seat on any tick the pick changes: everything only
    /// that seat held leaves the connection's interest at once, and [`OrbitNet::update_interest`]
    /// clears `last_sent`, `last_full` and `acked_base` for each — a full-state burst rather than
    /// the per-entity repair clearing exists to buy. See [`OrbitNet::warn_anchor_conflicts`].
    membership_conflict: bool,
}

/// One connection's anchor declaration as the interest pass reads it for a tick.
///
/// The four facts [`resolve_observer`] needs, carried together because they are one statement about
/// one connection: what the game declared, and — for a declaration that names an entity — where that
/// entity is now and where it last was.
#[derive(Clone, Copy)]
struct PeerDeclaration {
    /// What the game declared, or [`PeerAnchor::Inferred`] when it declared nothing.
    anchor: PeerAnchor,
    /// The world declared alongside it. Read only when a declaration exists.
    membership: MembershipId,
    /// Where a [`PeerAnchor::Entity`] target is THIS tick, if it is still here and still positioned.
    tracked: Option<[f32; 3]>,
    /// Where it last resolved to, so its despawn leaves the peer there rather than opening its
    /// radius to the whole world.
    last: Option<[f32; 3]>,
    /// This connection's effective `unanchored_policy`: `true` for CLOSED.
    ///
    /// Rides the declaration because it is only ever read **against** one — it decides what happens
    /// to a connection that declared nothing and drives nothing, and nothing else.
    /// [`seat_observers_into`] states the whole carve-out.
    closed_when_unanchored: bool,
}

/// The viewpoints one connection's filter runs this tick, and what resolving them revealed.
///
/// **The observers are the answer that is IN EFFECT**, which no other read-back on this node
/// reports. [`OrbitNet::peer_membership`] answers the DECLARATION and therefore `0` for every
/// inferred peer, and `NetRollbackHandle.membership()` answers one body rather than the pick made
/// among a seat's several. Everything a game would have to reconstruct from those two — which of
/// several bodies anchored a seat, whether the connection is culling anything at all, whether the
/// pass ran — is here because it is a fact of the pass rather than a fact of the scene.
///
/// Filled by [`seat_observers_into`] once per connection per tick, then copied onto that
/// connection's [`AnchorReport`] so a getter can read it between ticks.
#[derive(Default)]
struct ResolvedSeats {
    /// The observers handed to the filter, in the order it runs them.
    observers: Vec<SeatObserver>,
    /// Positionally, which seat label each observer belongs to.
    ///
    /// `None` marks the single collapsed viewpoint a declaration — or the connection-wide fail-open
    /// — produces, which belongs to **every** seat on the connection rather than to one of them.
    /// That is what lets [`OrbitNet::seat_anchor_info`] answer for a seat label the inferred path
    /// never enumerated.
    labels: Vec<Option<SeatIndex>>,
    /// One of the `ANCHOR_SOURCE_*` values: what produced the observers above.
    source: i64,
    /// At least one seat drove several anchored bodies, so its center is one arbitrary pick among
    /// them. See [`PeerObserver::ambiguous`].
    ambiguous: bool,
}

/// One connection's [`ResolvedSeats`], kept between ticks so [`OrbitNet::peer_anchor_info`] can
/// report the anchor that is in effect rather than the one that was declared.
///
/// Held on [`PeerState`] rather than in a table of its own, so it is **dropped with the rest of the
/// connection** when the peer disconnects. A report that outlived its socket would be answered for
/// whoever the transport hands that peer id to next.
///
/// Copied into rather than reassigned ([`Self::adopt`]), so a steady session allocates nothing here:
/// this sits inside the per-peer loop of the interest pass, which the send path exists to keep cheap.
#[derive(Default)]
struct AnchorReport {
    /// Whether [`OrbitNet::update_interest`] has ever written this report for this connection.
    ///
    /// `false` is one half of `stale`; the other half is the pass not having run **this** tick, which
    /// is [`OrbitNet::interest_ran`]. A default [`PeerState`] starts here, so a reused peer id reports
    /// stale until its first pass rather than inheriting the last connection's viewpoints.
    resolved: bool,
    /// One of the `ANCHOR_SOURCE_*` values.
    source: i64,
    ambiguous: bool,
    observers: Vec<SeatObserver>,
    labels: Vec<Option<SeatIndex>>,
}

impl AnchorReport {
    /// Take a copy of this tick's resolution, reusing the vectors already allocated.
    fn adopt(&mut self, seats: &ResolvedSeats) {
        self.resolved = true;
        self.source = seats.source;
        self.ambiguous = seats.ambiguous;
        self.observers.clear();
        self.observers.extend_from_slice(&seats.observers);
        self.labels.clear();
        self.labels.extend_from_slice(&seats.labels);
    }
}

/// Whether a center is a measurement rather than [`UNLOCATABLE_CENTER`].
///
/// The same three-component finite test [`orbitnet_core::interest::PeerInterest`] runs, stated here
/// so the diagnostic reports the sentinel by the rule the filter reads it by rather than by
/// comparing against a NaN — which is never equal to itself.
#[must_use]
fn is_located(center: [f32; 3]) -> bool {
    center[0].is_finite() && center[1].is_finite() && center[2].is_finite()
}

/// What one peer's filter actually runs against this tick: where it observes from, and its world.
///
/// The whole precedence rule, in one testable place. A declaration ([`PeerAnchor`]) wins on both
/// axes; only [`PeerAnchor::Inferred`] consults the pair read off the body the peer drives:
///
/// | Declaration | Center | World |
/// | --- | --- | --- |
/// | [`PeerAnchor::Fixed`] | the declared position, always | the declared one |
/// | [`PeerAnchor::Entity`] | where that entity is this tick, else where it last was | the declared one |
/// | [`PeerAnchor::Inferred`] | the inferred body's, if it has one | the inferred body's, else [`MEMBERSHIP_GLOBAL`] |
///
/// **THE TWO AXES FAIL SEPARATELY, AND ONLY FOR A DECLARED PEER.** A tracked entity that has never
/// resolved gives no center — so nothing is distance-culled, the same open direction an entity with
/// no anchor already takes — but the peer stays in the world it was DECLARED into. A membership is a
/// declaration and did not fail; a center is a measurement and did. Collapsing them would drop a
/// peer whose avatar has not spawned into every world at once, which is the failure the declaration
/// exists to remove.
///
/// **A DECLARATION IS PER CONNECTION, AND IT COLLAPSES THAT CONNECTION TO ONE SEAT.** Only
/// [`PeerAnchor::Inferred`] is resolved per seat, because only the inferred pair is read off a body
/// and only bodies carry seats. A game that declares a center for a split-screen connection has
/// stated where that connection observes from, and the backend does not then re-split it — the same
/// precedence that stops a declared center from falling back to an avatar's. A game that wants a
/// center per seat declares nothing and lets each seat's body anchor it.
#[must_use]
fn resolve_observer(
    anchor: PeerAnchor,
    declared: MembershipId,
    tracked: Option<[f32; 3]>,
    last: Option<[f32; 3]>,
    inferred: Option<PeerObserver>,
) -> (Option<[f32; 3]>, MembershipId) {
    match anchor {
        PeerAnchor::Fixed(pos) => (Some(pos), declared),
        PeerAnchor::Entity(_) => (tracked.or(last), declared),
        PeerAnchor::Inferred => (
            inferred.map(|o| o.center),
            inferred.map_or(MEMBERSHIP_GLOBAL, |o| o.membership),
        ),
    }
}

/// The windowed send-path accounting `Net.bandwidth_metrics()` republishes.
///
/// Nothing downstream of this epic is tunable or gateable without it — a byte budget nobody can
/// measure is a constant, and a fairness rule nobody can measure is a hope. Every rate is per
/// second over the last completed window; the two `_max` figures are maxima over that window, not
/// rates.
#[derive(Default, Clone, Copy)]
struct BandwidthMetrics {
    /// OrbitNet payload handed to the transport.
    tx_bytes_s: f64,
    /// Datagrams sent — the multiplier on [`WIRE_OVERHEAD_BYTES`].
    tx_datagrams_s: f64,
    /// `tx_bytes_s` plus the per-datagram wire overhead: what the link actually carries.
    tx_wire_bytes_s: f64,
    /// The busiest single peer's payload — the figure an AOI A/B has to move.
    tx_peak_peer_bytes_s: f64,
    /// Payload received, and its datagram count.
    rx_bytes_s: f64,
    rx_datagrams_s: f64,
    /// Entity blocks that made it into a frame.
    blocks_admitted_s: f64,
    /// Blocks that wanted to go out and did not fit the budget. **Budget pressure.**
    blocks_deferred_s: f64,
    /// Blocks intentionally not sent — out of interest, or held back by rate tiering.
    /// **Deliberate.** Kept apart from `deferred` because conflating them hides the failure.
    blocks_culled_s: f64,
    /// Blocks admitted even though they alone exceeded the whole byte budget — see the admit loop.
    /// Non-zero means one entity's full state does not fit in a datagram, so that frame went out
    /// over the MTU and fragmented. **Not** a deferral: deferring it is what wedges the stream.
    blocks_oversize_s: f64,
    /// Blocks sent as full rows rather than masked deltas: the send lane's composition, and what
    /// the keyframe interval costs.
    ///
    /// - Floor: about `blocks_admitted_s / FULL_STATE_INTERVAL`. Every entity owes one keyframe
    ///   per interval, so nothing lower is reachable.
    /// - Near `blocks_admitted_s`: almost nothing is being deltaed. On a server that indicates a
    ///   `want_full` storm; read it beside `want_full_nacks_s`.
    blocks_full_s: f64,
    /// `WANT_FULL` NACKs received. **SERVER-SIDE ONLY** — it is incremented where a client's input
    /// frame is decoded, so a client reads a structural 0.00 here whatever its link is doing.
    want_full_nacks_s: f64,
    /// Acks discarded because the frame token the peer quoted was not the one the server minted for
    /// the tick it named. **SERVER-SIDE ONLY**, for the same reason `want_full_nacks_s` is.
    ///
    /// An honest client cannot produce one, so any sustained reading is a peer sending acks it cannot
    /// substantiate — a forged or replayed input frame, or a build on the wrong protocol major that got
    /// past the handshake. The cost to that peer is visible beside this: its acks buy it nothing, so
    /// `acked_base` never advances and `blocks_full_s` climbs toward `blocks_admitted_s`.
    unproven_acks_s: f64,
    /// State blocks discarded because a newer row for that entity had already been applied —
    /// reordered or duplicated datagrams. **Not** a fault: it is what the link does.
    ///
    /// **CLIENT-SIDE ONLY**, and that is the half of this pair that nobody wrote down. It is counted where a
    /// received snapshot is integrated, which a server never does; `want_full_nacks_s` is counted where a
    /// received input frame is decoded, which a client never does. So "read the two against each other" is a
    /// statement about TWO PROCESSES: the client's `stale_blocks_s` beside the server's `want_full_nacks_s`.
    /// Read inside one `net.perf` they can never both be non-zero, and a client reporting `want_full 0.00`
    /// says nothing at all about whether a storm is happening.
    stale_blocks_s: f64,
    /// Worst age, in ticks, of any in-interest entity that had been sent at least once.
    starve_ticks_max: f64,
    /// Worst count of in-interest entities never yet sent to a peer — the re-entry storm gauge,
    /// which `starve_ticks_max` cannot see because a never-sent entity has no age.
    unsent_backlog_max: f64,
    /// Milliseconds per tick spent in the interest pass. The number that would justify revisiting
    /// the grid-versus-scan decision recorded in `orbitnet_core::interest`.
    interest_ms: f64,
    /// Fraction of the window's ticks whose interest pass ran through the spatial index rather than
    /// the flat scan: `0.0` all-scan, `1.0` all-grid, in between while the session crosses the
    /// threshold. **The verdict, reported — there is no setting behind it.**
    ///
    /// Read it beside `interest_ms` and nowhere else. The two paths compute identical members,
    /// distances and leaves, so this column can never explain a behavior difference; the only
    /// question it answers is which cost `interest_ms` is the cost of. A session that sits at a
    /// fraction strictly between `0.0` and `1.0` for a whole window is one whose occupancy is
    /// hovering in the selector's hysteresis band, which is a description of the arena rather than
    /// a fault.
    interest_grid: f64,
    /// Mean ticks between admissions, per distance band. The evidence to demand before rate
    /// tiering may be turned on: it says whether the far band is genuinely far.
    interarrival_near: f64,
    interarrival_mid: f64,
    interarrival_far: f64,
    /// Mean ticks between admissions POOLED ACROSS EVERY BAND — the figure a consumer that does not
    /// know which band its subject is in must use. `interarrival_near` is the near band alone the
    /// moment culling is on, and reading it globally under-states how old a mid or far row is.
    interarrival_all: f64,
    /// Peers synced, and the mean size of one peer's interest set.
    peers: f64,
    interest_entities: f64,
    /// Connected peers whose RAW round-trip estimate is above [`OrbitNet::rtt_believed_max_ms`], so
    /// the figure `peer_rtt_ms` reports for them is the ceiling rather than what was measured.
    ///
    /// **A gauge like `starve_ticks_max`, not a per-second rate**: the standing count as of the
    /// publish. A subset of `peers`, so read it against that one — `3` out of `4` says the ceiling is
    /// the session's policy for nearly everybody, and `3` out of `40` says three players are having a
    /// bad time. Persistent and large is the reading that says the ceiling is set too low for the
    /// population actually playing.
    ///
    /// Non-zero is not an accusation. A peer above the ceiling is either lagging its acks
    /// deliberately or genuinely that far away, and nothing here can tell those apart — see
    /// [`PeerState::note_ack`].
    rtt_at_ceiling_peers: f64,
}

enum PendingOp {
    RegisterRollback(u64, Gd<OrbitRollbackSynchronizer>),
    RegisterState(u64, Gd<OrbitStateSynchronizer>),
    /// `(entity id, the synchronizer that asked)`. The instance id is what makes a respawn safe: entity
    /// ids are node-path-derived, so a body that respawns under its old name reclaims the SAME id, and an
    /// unregister queued by the corpse must not evict the replacement that registered ahead of it.
    Unregister(u64, InstanceId),
}

thread_local! {
    static PENDING_OPS: RefCell<Vec<PendingOp>> = const { RefCell::new(Vec::new()) };
    /// Published per frame for decoupled consumers (the interpolator) that have no reference to
    /// the singleton: the sub-tick blend weight and the frontier tick.
    static TICK_STATE: std::cell::Cell<(f64, u64)> = const { std::cell::Cell::new((1.0, 0)) };
}

/// A clonable handle to `entity`, or `None` if the node behind it has already been freed.
///
/// EVERY registry read goes through here. `Gd::clone` is NOT infallible: under godot-rust's balanced
/// safeguards (what a release build of this extension ships with) cloning a handle whose instance is
/// dead panics inside `check_rtti`, so the once-common `let sync = sync.clone(); if
/// !sync.is_instance_valid()` shape never reaches its own guard — the clone took the frame down first.
/// Validating the BORROWED handle is safe: `is_instance_valid` reads the cached instance id and asks
/// Godot's object database, dereferencing nothing.
///
/// The window this closes: a synchronizer can only enqueue its `Unregister` from `exit_tree`, and the
/// queue is drained at the top of the next `process`/`physics_process`. A body freed at the end of a
/// frame (`queue_free` — how every despawn lands) therefore leaves a dead handle in the registry until
/// then, and `SceneMultiplayer`'s `peer_packet` poll fires inside that gap. That is the death/respawn
/// panic seen in the wild: a client's still-in-flight input frame naming the corpse's entity.
pub(crate) fn live_handle<T: GodotClass>(entity: &Gd<T>) -> Option<Gd<T>> {
    if entity.is_instance_valid() {
        Some(entity.clone())
    } else {
        None
    }
}

/// The sub-tick interpolation weight, as last published by the running `OrbitNet` node.
pub(crate) fn global_tick_factor() -> f64 {
    TICK_STATE.with(|s| s.get().0)
}

/// The frontier tick, as last published by the running `OrbitNet` node.
pub(crate) fn global_frontier_tick() -> u64 {
    TICK_STATE.with(|s| s.get().1)
}

/// Queue a rollback synchronizer for registration (drained at the next frame boundary).
pub(crate) fn register_rollback_entity(id: u64, sync: Gd<OrbitRollbackSynchronizer>) {
    PENDING_OPS.with(|ops| ops.borrow_mut().push(PendingOp::RegisterRollback(id, sync)));
}

/// Queue a state-lane synchronizer for registration.
pub(crate) fn register_state_entity(id: u64, sync: Gd<OrbitStateSynchronizer>) {
    PENDING_OPS.with(|ops| ops.borrow_mut().push(PendingOp::RegisterState(id, sync)));
}

/// Queue an entity's removal (synchronizer leaving the tree).
pub(crate) fn unregister_entity(id: u64, who: InstanceId) {
    if id != 0 {
        PENDING_OPS.with(|ops| ops.borrow_mut().push(PendingOp::Unregister(id, who)));
    }
}

/// Where a peer observes from, as the GAME declared it — the alternative to inferring it.
///
/// **A declaration replaces inference outright**, on both axes at once. The inferred pair
/// ([`PeerObserver`]) reads a peer's center and its world off the lowest-id body that peer's input
/// drives, which answers "what does this peer control" when the question interest management asks is
/// "what does this peer observe". Those are the same answer in a game with one world and one avatar
/// per player, and different answers in every other one: a spectator drives nothing, a commander
/// watches ground its body is not standing on, and a peer with a body in each of two worlds observes
/// exactly one of them.
///
/// Once a game answers the real question for a peer, the inferred pair is never consulted again for
/// that peer. Mixing them would re-center a peer on its avatar the moment the declared center was
/// momentarily unavailable — and, worse, would put it back in its avatar's world.
#[derive(Default, Clone, Copy, PartialEq)]
enum PeerAnchor {
    /// Nothing declared: fall back to [`OrbitNet::collect_observers`].
    #[default]
    Inferred,
    /// A fixed world position — a spectator camera, a strategic view, an observation post.
    Fixed([f32; 3]),
    /// Track an entity by id, wherever it is this tick.
    Entity(u64),
}

#[derive(Default)]
struct PeerState {
    /// Whether the handshake completed (server side: Hello received and answered).
    synced: bool,
    /// The identity this peer's handshake carried, or `0` for a peer that claimed none.
    ///
    /// Kept beside the transport peer id rather than replacing it, because the two answer different
    /// questions: the peer id says where to send bytes and is reassigned on every reconnect, this says who
    /// is on the other end and survives one. Only the second can recognize a rejoiner.
    ///
    /// Per CONNECTION, not per seat: a session identity says which player is on the far end of one socket,
    /// and every seat behind that socket belongs to the same player.
    session_id: u64,
    /// The **server-minted resume token** issued for [`Self::session_id`], or `0` for a connection holding
    /// no identity.
    ///
    /// The identity names the player; this is what a claim on that identity has to quote. A rejoiner sends it
    /// back in its handshake, and [`resume_grant`] refuses a claim that does not match the token on record.
    ///
    /// **Minted once per identity, at the first hello that seats one**, and carried forward verbatim onto
    /// the connection a granted resume seats. Two reasons, and the first alone would be enough:
    ///
    /// - **A hello is retried until the welcome lands.** Re-minting on the retry would strand the token the
    ///   client stored from the welcome that did arrive, and the peer would be refused its own identity.
    /// - **The client persists it beside the session id.** A token that changed on every connection would
    ///   have to be re-persisted on every connection, and a process killed between the two would lose it.
    ///
    /// **This is NOT [`Self::token_salt`].** That value is deliberately never transmitted, and transmitting
    /// it would let a client mint the ack token for frames it never received — which is the exact claim
    /// `unproven_acks_s` counts. The two are separate draws from the same generator and never alias.
    ///
    /// It rides the connection so `self.peers.remove()` takes it away on a drop; what survives the drop is
    /// the copy on [`HeldSession::token`].
    resume_token: u64,
    /// Where this peer observes from, declared by the game. See [`PeerAnchor`].
    anchor: PeerAnchor,
    /// The last position [`PeerAnchor::Entity`] resolved to, and the answer once it no longer can.
    ///
    /// **A tracked entity that despawns leaves the peer where it was.** The alternative — dropping
    /// to "no center", which means "no distance filter" — hands a peer every body in its world at
    /// the exact moment its avatar died. A stale center is wrong by however far the peer would have
    /// traveled; the open one is wrong by the size of the world.
    ///
    /// It is also what carries a declaration made BEFORE the named entity has a state row: the
    /// declaration survives on this struct and starts resolving the tick that entity does.
    anchor_last: Option<[f32; 3]>,
    /// The world declared alongside [`Self::anchor`]. Read ONLY when a declaration exists, so an
    /// undeclared peer still takes its world from the body it drives.
    ///
    /// It rides the anchor declaration rather than standing alone because the two are one statement
    /// — "this peer is at this point, in this world" — and a center without the world it is measured
    /// in is precisely the pairing the inferred path takes from one row to keep consistent.
    anchor_membership: MembershipId,
    /// What THIS connection receives when it resolves no anchor, or `None` to follow the session
    /// default ([`OrbitNet::set_unanchored_policy`]). `Some(true)` is CLOSED.
    ///
    /// **Per connection and dropped with the connection.** It lives here rather than in a table
    /// keyed by peer id precisely so `_on_peer_disconnected`'s `self.peers.remove()` takes it away:
    /// a policy that outlived its socket would be applied to whoever the transport hands that id to
    /// next, and "receive nothing" is not a state to inherit from a stranger.
    unanchored_closed: Option<bool>,
    /// What the interest pass last resolved this connection's viewpoints to. See [`AnchorReport`].
    anchor_report: AnchorReport,
    /// The key and replay window for this connection's datagrams, seated by its handshake.
    ///
    /// `None` until the handshake lands, and that is the gate: [`OrbitNet::open_datagram`] drops
    /// everything from a peer that has none, so a peer that never handshook cannot even draw a pong.
    auth: Option<SessionAuth>,
    /// What this peer has spent of the server's receive path in the current tick.
    budget: ReceiveBudget,
    /// SERVER: the entity-manifest generation this connection is believed to hold.
    ///
    /// **`0` means "has never been sent a table", and it is the same statement as "holds the empty
    /// table"** — a receiver's manifest starts empty and only becomes non-empty by applying a frame
    /// that also moves this off `0`. So a fresh connection needs no special case: it is simply
    /// behind, and [`manifest_owed`] answers it with a full table unless the session has published
    /// nothing at all.
    ///
    /// **Every path that can desynchronize this peer zeroes it**, which is the whole of what stands
    /// in for the complete table's self-repair:
    ///
    /// | Path | Where |
    /// | --- | --- |
    /// | a reconnect | a dropped peer is removed from `peers`, so the rejoiner starts at a `PeerState::default` |
    /// | a rekey on a live connection | [`OrbitNet::handle_hello`], in the block that replaces [`Self::auth`] |
    /// | the peer could not apply a delta | [`FrameHeader::FLAG_WANT_MANIFEST`] on its next input frame |
    ///
    /// It rides the connection, so `self.peers.remove()` takes it away on a drop.
    manifest_generation: u64,
    /// Whether this connection has already been named in a non-finite-input warning.
    ///
    /// **One warning per peer per session**, for the reason [`OrbitNet::note_unauthenticated`]
    /// latches its own: under an actual flood the log is the second thing to fall over. It lives on
    /// the connection, so `self.peers.remove()` on a disconnect takes it away — a reconnecting
    /// player is warned about again, and a peer id handed to somebody else starts clean.
    nonfinite_warned: bool,
    /// Per-entity newest tick sent — drives send priority.
    last_sent: HashMap<u64, u64>,
    /// Per-entity newest tick sent as a full block. Drives the keyframe interval.
    ///
    /// Separate from `last_sent`: an entity whose chain this peer cannot decode keeps being sent
    /// as deltas, so `last_sent` never ages past [`FULL_STATE_INTERVAL`] and the keyframe that
    /// would repair the chain never comes due.
    last_full: HashMap<u64, u64>,
    /// The peer asked for full masks (its delta base broke).
    want_full: bool,
    /// Newest input tick received from this peer (server side).
    newest_input_tick: i64,
    /// Input-arrival margin reported back in snapshot headers.
    margin_last: i8,
    /// Entities — **both lanes** — inside this connection's interest, with the squared distance
    /// the last update measured, which the priority scorer reads back as a band.
    ///
    /// **One hysteretic set per seat, unioned.** The set is what the datagram carries and the
    /// datagram is per connection, so the union lives here beside `last_sent`, `last_full` and
    /// `acked_base` — all four are properties of one packet stream. Relevancy is not: it is a
    /// property of a viewpoint, and a split-screen connection has several. The distance stored per
    /// member is the nearest seat's, and a leave is a leave from the union. See
    /// [`ConnectionInterest`].
    interest: ConnectionInterest,
    /// The relevancy events this connection is owed but has not provably received: what left its
    /// interest and what entered it since the last section it acknowledged.
    ///
    /// **It is a net difference, not a log.** [`PeerState::note_interest_leave`] and
    /// [`PeerState::note_interest_enter`] each drop any pending entry for the same entity in the
    /// other half before appending, so an id is named in at most one of the two lists and the list
    /// says where that entity stands NOW. Applying it to the receiver's mirrored set therefore
    /// converges whether or not the intermediate frames landed — an entity that left and re-entered
    /// while a frame was in flight is announced once, as an enter.
    ///
    /// Three sources fill it, and only the first is a `leaves` list anything else could read:
    ///
    /// | Source | What it pushes |
    /// | --- | --- |
    /// | [`OrbitNet::update_interest`] | both halves of the union diff |
    /// | [`PeerState::set_entity_hidden`] | a leave, when a veto starts |
    /// | the despawn sweep in `drain_pending` | a leave, on every peer that held the entity |
    ///
    /// **A retraction pushes nothing.** The entity re-enters through the enter radius on the next
    /// update, and that update reports it.
    interest_pending: InterestDelta,
    /// The tick of the frame that FIRST carried the prefix currently in flight, or `None` when
    /// nothing is in flight.
    ///
    /// The section rides an unreliable datagram, so it is re-sent every tick until `newest_ack`
    /// reaches this — the ack window and the frame token are already verified per frame, so a
    /// relevancy event needs no reliable channel of its own, only a tick stamp. It does NOT move on
    /// a re-send: what an ack has to reach is the first frame that carried the prefix, because that
    /// is the frame whose arrival proves the client applied it.
    interest_delta_tick: Option<u64>,
    /// How many entries from the FRONT of each half of [`Self::interest_pending`] rode the frame
    /// stamped above.
    ///
    /// One frame carries at most [`INTEREST_DELTA_PER_FRAME`] of each half, so a burst — a joining
    /// peer, whose first update enters everything it can see — is spread over several frames instead
    /// of eating the send budget in one. Both halves are pushed at the back and retired from the
    /// front, so the prefix is stable across re-sends.
    interest_delta_left_sent: usize,
    interest_delta_entered_sent: usize,
    /// Whether this connection has ever been told that the session is filtering at all.
    ///
    /// A client that has received no section cannot tell "the server culls nothing" from "the server
    /// culls and I am owed a section", and the two want opposite answers out of
    /// [`OrbitNet::entities_in_interest`]. So a filtering server sends the section once even when it
    /// is empty — two bytes, once per connection — and the flag retires with the first ack of it.
    interest_seeded: bool,
    /// The generation of the interest set this connection is believed to hold.
    ///
    /// Bumped only when a whole [`FrameKind::InterestTable`] is sent, and stamped onto every delta
    /// section built after it. That is the whole of what it is for: the table is reliable and a
    /// delta is not, so a delta built BEFORE the table can arrive after it and undo it. A receiver
    /// refuses a section stamped below the generation it holds.
    interest_generation: u64,
    /// Whether this connection is owed a whole interest set rather than another delta.
    ///
    /// **FOUR THINGS SET IT, AND THE SERVER KNOWS THREE OF THEM ITSELF.** A pending half that
    /// overflowed dropped an event nobody can reconstruct; a prefix given up on unacknowledged left
    /// the two ends disagreeing about what was sent; a rekey on a live connection threw away
    /// everything that connection held. The fourth is the client raising
    /// [`FrameHeader::FLAG_WANT_INTEREST`], for the two cases only it can see — a section naming a
    /// slot its manifest has not bound, and one stamped at a generation it does not hold.
    ///
    /// Before this flag, all three were silent: the mirror stayed wrong for the rest of the session
    /// while the entity's rows kept arriving, and the documented repair
    /// ([`OrbitNet::entities_in_interest`]) answered a client from that same broken mirror.
    interest_full_due: bool,
    /// Recent snapshot sends awaiting acknowledgment: (frame tick, entity ticks it carried).
    sent_log: std::collections::VecDeque<(u64, Vec<(u64, u64)>)>,
    /// Per-entity newest tick this peer CONFIRMED receiving (via ack_tick/ack_bits) — the only
    /// tick a masked delta may reference: the peer provably holds that base row.
    acked_base: HashMap<u64, u64>,
    /// Highest ack tick seen from this peer, for expiring the sent log.
    newest_ack: u64,
    /// Server: the secret this peer's frame tokens are minted from, or `None` before its handshake.
    ///
    /// **It is never transmitted.** That is the whole of what makes a token proof of receipt: the client
    /// knows the session key (it minted it) and it knows every tick number, so anything derived from
    /// those two it could compute for a frame that never reached it. It cannot compute this.
    ///
    /// **Minted once per connection, and not rotated on a rekey.** A hello is retried, and a retry
    /// carrying a new key reseats [`PeerState::auth`]; rotating this alongside it would invalidate every
    /// token the client is already holding, so its next several acks would be refused and the server
    /// would fall back to full blocks for a peer that did nothing wrong. The salt guards nothing that a
    /// rekey changes — it is a per-connection secret, and the connection is the same one.
    token_salt: Option<[u8; KEY_LEN]>,
    /// Recent round-trip samples in MILLISECONDS, for the per-shooter rewind depth. Each is
    /// `server tick now - the newest snapshot frame this peer has confirmed`, taken at the instant
    /// that confirmation arrives, so it spans exactly send -> receive -> reply -> receive.
    ///
    /// Converted to time AT THE SAMPLE, not on the way out, because a tick is not a fixed amount of
    /// time and the rate can change mid-session. Storing ticks and dividing later would read the
    /// whole window at whatever rate happens to be running when somebody asks — the same mistake
    /// the tick-stamping cleanup removed from the rewind window itself.
    rtt_samples: std::collections::VecDeque<f32>,
}

/// One dropped session the server is holding open for its player to come back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeldSession {
    /// The transport peer id the session was last connected under.
    ///
    /// Reported to the game so it can name what it is releasing; never matched against, because a rejoiner
    /// arrives under a NEW id and matching on this one is precisely the thing that does not work.
    peer: i32,
    /// Wall-clock millisecond stamp past which the session is forgotten.
    expires_at_ms: u64,
    /// The **resume token** the departed connection held for this identity, or `0` if it held none.
    ///
    /// A rejoiner has to quote this to claim the session. It is copied off [`PeerState::resume_token`] at
    /// the drop rather than re-minted here, because the value the client is holding was issued at its first
    /// hello and a new one would reach nobody — the connection that would have carried it in a welcome is
    /// already gone.
    token: u64,
}

/// The dropped sessions a server is holding, keyed by the identity their handshake carried.
///
/// **What is held is the identity, the token that proves a claim on it, and nothing else.** None of the
/// departed peer's send bookkeeping is
/// retained — `last_sent`, `acked_base`, the sent log — and retaining it would be wrong: a rejoiner is a new
/// transport connection whose client-side delta bases are gone, so it is handshaked as a newcomer and asks
/// for full blocks. The one thing that cannot be rebuilt from the connection is who the player was, and that
/// is what this table is.
///
/// A plain struct with no Godot types, so the whole expiry rule is unit-testable without a `SceneTree`.
#[derive(Default)]
struct ResumeTable {
    held: HashMap<u64, HeldSession>,
}

impl ResumeTable {
    /// Hold `session_id` open until `expires_at_ms`, recording the resume `token` a claimant must quote.
    ///
    /// Identity `0` is refused — that is what a peer claiming no identity sends, and holding one slot for
    /// "everybody anonymous" would hand the next anonymous joiner the last one's seat.
    ///
    /// Re-holding an id already present overwrites it: the newer drop is the one whose window should run.
    fn hold(&mut self, session_id: u64, peer: i32, expires_at_ms: u64, token: u64) -> bool {
        if session_id == 0 {
            return false;
        }
        self.held.insert(
            session_id,
            HeldSession {
                peer,
                expires_at_ms,
                token,
            },
        );
        true
    }

    /// Take `session_id` out of the table, answering the peer id it was last connected under.
    ///
    /// `None` for an unheld id, which is every first-time joiner and every peer that claimed no identity.
    /// Claiming REMOVES: a session is resumed once, by the connection that arrived first, and a second
    /// claimant with the same token is a newcomer rather than a second resume of one player's place.
    ///
    /// **`presented_token` must match the token on record, and a mismatch LEAVES THE HELD SESSION IN
    /// PLACE.** Spending somebody else's window on a wrong quote would turn one forged hello into a denial
    /// of service — the real player comes back inside the grace window and finds nothing held — which is a
    /// worse outcome than the takeover the token exists to refuse. A record whose token is `0` accepts any
    /// quote: that is a session held for a connection this server minted no token for, and refusing it
    /// would refuse a resume nobody can ever satisfy.
    fn claim(&mut self, session_id: u64, presented_token: u64) -> Option<i32> {
        if session_id == 0 {
            return None;
        }
        let held = self.held.get(&session_id)?;
        if held.token != 0 && held.token != presented_token {
            return None;
        }
        self.held.remove(&session_id).map(|held| held.peer)
    }

    /// The resume token recorded for `session_id`, or `0` when no session is held under it.
    ///
    /// Read by `handle_hello` to answer [`resume_grant`]'s `token_on_record`, so the decision and the claim
    /// are made against the same value.
    fn token_of(&self, session_id: u64) -> u64 {
        if session_id == 0 {
            return 0;
        }
        self.held.get(&session_id).map_or(0, |held| held.token)
    }

    /// Remove and return every session whose window closed at or before `now_ms`, as `(session, peer)`.
    ///
    /// Ordered by session id so a game that logs the release reads the same order on every run — the table
    /// is a `HashMap`, whose iteration order is not.
    fn expire(&mut self, now_ms: u64) -> Vec<(u64, i32)> {
        if self.held.is_empty() {
            return Vec::new();
        }
        let mut due: Vec<(u64, i32)> = self
            .held
            .iter()
            .filter(|(_, held)| held.expires_at_ms <= now_ms)
            .map(|(&id, held)| (id, held.peer))
            .collect();
        due.sort_unstable();
        for (id, _) in &due {
            self.held.remove(id);
        }
        due
    }

    /// Whether `session_id` is currently being held open.
    fn holds(&self, session_id: u64) -> bool {
        session_id != 0 && self.held.contains_key(&session_id)
    }

    /// Forget every held session (session teardown).
    fn clear(&mut self) {
        self.held.clear();
    }
}

/// How many unacked snapshot frames a peer's sent log retains before the oldest expire.
const SENT_LOG_DEPTH: usize = 64;

/// How many entries of each half of a peer's pending interest delta ride one frame.
///
/// **This is what bounds the reserve the send path takes off the byte budget.** The section costs
/// [`interest_delta_reserve`] bytes, so 32 of each half is `13 + 2 × 64` = 141 B — about 12% of the
/// default 1200 B budget, and only on a tick that has relevancy news. A joining peer, whose first
/// update enters everything it can see, spreads that burst over consecutive frames rather than
/// paying for it in one: nothing is dropped, it arrives a round trip later.
const INTEREST_DELTA_PER_FRAME: usize = 32;

/// How many entries each half of a peer's pending interest delta holds before the OLDEST is dropped.
///
/// The backstop for a session churning relevancy faster than the acknowledged prefix drains — a peer
/// on a link that is up enough to be sent to and down enough never to ack, and a first update in a
/// world of more than this many filtered entities. What is dropped is the oldest event, never the
/// newest: the recent transitions are the ones a game is about to act on.
///
/// **THE DROP IS REPORTED, NOT ABSORBED.** `entities_in_interest` was named here as the repair and
/// could not be one: on a client it answers out of the mirror the drop had just made wrong. What
/// repairs it is [`OrbitNet::send_interest_tables`], which the overflow asks for.
const INTEREST_DELTA_PENDING_MAX: usize = 256;

/// The ceiling a pending half is actually trimmed at, as a backstop rather than a policy.
///
/// Reaching [`INTEREST_DELTA_PENDING_MAX`] owes the connection a whole set, and stating that set
/// collapses the half on the next flush — so in any session that sends one, this is unreachable.
/// What it bounds is the connection that is never sent one, which is a peer that is not `synced`,
/// and which therefore has no relevancy to accumulate in the first place. Four times the soft cap,
/// because the number that matters is that it is finite.
const INTEREST_DELTA_PENDING_HARD_MAX: usize = INTEREST_DELTA_PENDING_MAX * 4;

/// How many ticks a prefix rides unacknowledged before it is given up on.
///
/// **The same depth as [`SENT_LOG_DEPTH`]**, and for the same reason: past 64 frames an ack can no
/// longer confirm the frame anyway, so holding the prefix past that reserves budget on every tick
/// for a section nothing will ever retire. What is dropped is the prefix — those events are never
/// announced to that peer, so the drop owes that connection a whole set
/// ([`OrbitNet::send_interest_tables`]) rather than leaving its mirror wrong. The rest of the pending
/// delta is unaffected and takes the next frame.
const INTEREST_DELTA_RETRY_TICKS: u64 = SENT_LOG_DEPTH as u64;

/// How many round-trip samples the per-peer rewind estimate keeps — about a second of
/// them at every rate the loop runs at, which is short enough to follow a real route change and
/// long enough that the minimum below is drawn from a healthy population.
const RTT_WINDOW: usize = 64;

/// The largest single round-trip sample worth STORING, in milliseconds. It only keeps one stalled
/// peer from parking an absurd value in the window; at ten seconds it is 40x the rewind policy and
/// therefore never binds on anything a shooter is compensated on.
///
/// It is also the outer bound on [`OrbitNet::rtt_believed_max_ms`], which is the cap that does bind.
/// Same shape as the `history_limit` bound on accepted input ticks below: the wire says what it
/// says, and the server decides what it is willing to believe.
const RTT_SAMPLE_MAX_MS: f32 = 10_000.0;

/// What [`OrbitNet::rtt_believed_max_ms`] starts at, in milliseconds.
///
/// **The same figure `NetLagComp.max_delay_ms` defaults to, deliberately.** The two bound different
/// quantities — this one what the server BELIEVES about a link, that one how deep a shot REWINDS —
/// so they are not redundant, and see [`PeerState::rtt_believed_ms`] for why both exist. Starting
/// them at one number means a game that lowers its rewind ceiling and forgets this one is still
/// bounded at the depth it asked for, rather than believing 10 s about a peer it only ever rewinds
/// 250 ms for.
const RTT_BELIEVED_MAX_MS_DEFAULT: f64 = 250.0;

/// What an arriving acknowledgment bought its sender. See [`PeerState::consume_ack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckOutcome {
    /// Nothing was claimed — `ack_tick` is still `0`, which is every peer that has yet to receive a
    /// snapshot. Not a refusal.
    Empty,
    /// Claimed, and refused: the frame token quoted is not the one the server minted for that tick, so
    /// the peer cannot have received the frame it named. Nothing was consumed and nothing was granted.
    Unproven,
    /// Claimed, proven, and consumed.
    Consumed,
}

/// What a received state block did to this peer's bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RxOutcome {
    /// The row was integrated (or buffered for a tick still ahead).
    Applied,
    /// Ask the server for a full block: the delta named a base this peer does not hold.
    Nack,
    /// Dropped as already superseded. Counted, never answered.
    StaleDrop,
}

/// Whether a received block's outcome should raise a `WANT_FULL` NACK.
///
/// **A NACK is expensive and unaimed.** `want_full` is per-peer and all-entity: the server answers
/// it by marking every entity in that peer's next frame `full_due`, and a 1200-byte budget carries a
/// fraction of them, so the rest defer to a later tick — by which time their delta bases have moved,
/// so they reject too, and the flag goes back up. Raising it for anything a full block would not fix
/// is therefore not merely wasteful, it is self-sustaining.
///
/// [`StateIntegration::NoBase`] is the one outcome a full block fixes. A `Stale` block was decoded
/// and then discarded because a newer row for the same entity had already applied — reordering and
/// duplication, which every real link produces and loopback never does. Answering that with a NACK
/// is what makes a send-order change feel fine locally and fall apart on a relayed link.
///
/// A block that was ALREADY full and still failed cannot be fixed by asking for another one, so the
/// flag stays down there too.
#[must_use]
fn classify_rx(outcome: StateIntegration, block_was_full: bool) -> RxOutcome {
    match outcome {
        StateIntegration::NoBase if !block_was_full => RxOutcome::Nack,
        StateIntegration::NoBase => RxOutcome::StaleDrop,
        StateIntegration::Stale => RxOutcome::StaleDrop,
        _ => RxOutcome::Applied,
    }
}

impl PeerState {
    /// Start or stop withholding one entity from this peer. The whole of what
    /// [`OrbitNet::set_entity_hidden`] does, as a method a test can call without a `SceneTree`.
    ///
    /// [`ConnectionInterest::set_hidden`] carries the filter half — the refusal on every seat of the
    /// connection, and dropping the entity from the set in this call rather than at the next update. What is here is the other half:
    /// **starting a veto clears the same three per-entity entries a leave clears**. It is a leave —
    /// it just happened between updates, so no `leaves` list will ever name it, and the clearing
    /// cannot be left to the loop that reads one. Without it a later retraction encodes a delta
    /// against a base the peer dropped while it was withheld; the peer NACKs, and a NACK is per-peer
    /// and all-entity, so one re-admitted body costs a full-state burst for everything that peer
    /// holds.
    ///
    /// Retracting clears nothing, because nothing was sent while the veto was in force: the entries
    /// cleared at the start are still empty, which is what makes the re-admission a full block.
    fn set_entity_hidden(&mut self, id: u64, hidden: bool) -> bool {
        let held = self.interest.contains(id);
        self.interest.set_hidden(id, hidden);
        if hidden {
            self.last_sent.remove(&id);
            self.last_full.remove(&id);
            self.acked_base.remove(&id);
            // A veto is a leave, and it is one of the two that happen BETWEEN updates — no `leaves`
            // list will ever name it. Queued only when the entity was actually in the set: vetoing
            // something this connection never held announces a departure that never happened, and a
            // presentation layer that hides a node on it hides one it never showed.
            if held {
                self.note_interest_leave(id);
            }
            return held;
        }
        // RETRACTING QUEUES NOTHING. The entity re-enters through the enter radius on the next
        // update, and that update reports it — announcing it here would name a body the filter may
        // still refuse.
        false
    }

    /// Queue "this entity left your interest" for this connection.
    ///
    /// Any pending enter for the same id is dropped first, so the two halves never both name it and
    /// the receiver's idempotent apply cannot land them in the wrong order. See
    /// [`Self::interest_pending`].
    fn note_interest_leave(&mut self, id: u64) {
        Self::drop_pending(
            &mut self.interest_pending.enters,
            &mut self.interest_delta_entered_sent,
            id,
        );
        if Self::push_pending(
            &mut self.interest_pending.leaves,
            &mut self.interest_delta_left_sent,
            id,
        ) {
            self.interest_full_due = true;
        }
    }

    /// Queue "this entity entered your interest" for this connection. The mirror of
    /// [`Self::note_interest_leave`].
    fn note_interest_enter(&mut self, id: u64) {
        Self::drop_pending(
            &mut self.interest_pending.leaves,
            &mut self.interest_delta_left_sent,
            id,
        );
        if Self::push_pending(
            &mut self.interest_pending.enters,
            &mut self.interest_delta_entered_sent,
            id,
        ) {
            self.interest_full_due = true;
        }
    }

    /// Remove `id` from one half, keeping `sent` pointing at the same entries it did.
    ///
    /// `sent` counts entries from the FRONT, so removing one inside that range has to shrink it —
    /// otherwise the ack that retires the prefix would drain an entry that never rode.
    fn drop_pending(list: &mut Vec<u64>, sent: &mut usize, id: u64) {
        let Some(index) = list.iter().position(|&held| held == id) else {
            return;
        };
        list.remove(index);
        if index < *sent {
            *sent -= 1;
        }
    }

    /// Append `id` to one half, answering whether that took the half past
    /// [`INTEREST_DELTA_PENDING_MAX`].
    ///
    /// **NOTHING IS EVICTED, AND THAT IS THE POINT.** The cap cannot tell a recoverable entry from an
    /// unrecoverable one. A whole set restates every member the slot table can name, so a dropped
    /// entry is recoverable exactly when the set could have restated it anyway — and what the set
    /// CANNOT restate is a member with no slot yet, whose enter is held across the set precisely
    /// because of that. Evicting the oldest lost that held enter; evicting the newest loses the same
    /// thing whenever the slotless member is the one that just arrived. Any end this picks is wrong
    /// in some case, because the queue is not where the information is.
    ///
    /// What the overflow means is that this connection is owed a whole set, and the caller says so.
    /// The next flush states that set and collapses the queue to the members it could not name, so
    /// the half is bounded by the repair rather than by an eviction that guesses.
    ///
    /// [`INTEREST_DELTA_PENDING_HARD_MAX`] is the backstop for a connection that is never sent one —
    /// unsynced, so it has no relevancy to accumulate — and dropping the oldest there is a last
    /// resort rather than a policy.
    ///
    /// It is not only the unreachable-peer case the cap was written for: a first update in a world of
    /// more than [`INTEREST_DELTA_PENDING_MAX`] filtered entities overflows on a healthy link.
    #[must_use]
    fn push_pending(list: &mut Vec<u64>, sent: &mut usize, id: u64) -> bool {
        let overflowed = list.len() >= INTEREST_DELTA_PENDING_MAX;
        if list.len() >= INTEREST_DELTA_PENDING_HARD_MAX {
            list.remove(0);
            *sent = sent.saturating_sub(1);
        }
        list.push(id);
        overflowed
    }

    /// Drop everything this connection holds about one entity that has left the registry, answering
    /// whether it was in this connection's interest.
    ///
    /// The whole of what the despawn sweep does per peer, as a method a test can call without a
    /// `SceneTree`. The three per-entity entries go either way — they describe the DEPARTED body's
    /// history, and a replacement inheriting the id must not be delta-encoded against it — and the
    /// leave is queued only when the set actually held the entity, so a peer that was never sent it
    /// is told nothing.
    fn forget_entity(&mut self, id: u64) -> bool {
        self.last_sent.remove(&id);
        self.last_full.remove(&id);
        self.acked_base.remove(&id);
        let held = self.interest.contains(id);
        if held {
            self.note_interest_leave(id);
        }
        self.interest.remove(id);
        held
    }

    /// Retire the prefix that rode the stamped frame, or give up on it.
    ///
    /// Called once per peer per snapshot tick, BEFORE the section for this tick is built. Three
    /// outcomes, and the middle one is the whole of the reliability model:
    ///
    /// | State | What happens |
    /// | --- | --- |
    /// | nothing in flight | nothing |
    /// | `newest_ack` reached the stamp | the prefix is dropped, and the connection counts as seeded |
    /// | the stamp is [`INTEREST_DELTA_RETRY_TICKS`] old | the prefix is dropped unconfirmed, and so is the seed |
    ///
    /// The two drops are the same operation, which is deliberate: an unconfirmed prefix is not
    /// re-queued, because re-queuing it would make one unreachable peer accumulate for ever.
    fn retire_interest_delta(&mut self, current: u64) {
        let Some(stamp) = self.interest_delta_tick else {
            return;
        };
        let acked = self.newest_ack >= stamp;
        if !acked && current < stamp.saturating_add(INTEREST_DELTA_RETRY_TICKS) {
            return;
        }
        if !acked {
            // GIVEN UP ON, NOT DELIVERED. The prefix is still dropped — re-queuing it is what would
            // make one unreachable peer accumulate for ever — but the two ends now disagree about
            // what was sent, and a whole set is the only thing that settles that.
            self.interest_full_due = true;
        }
        let left = self
            .interest_delta_left_sent
            .min(self.interest_pending.leaves.len());
        self.interest_pending.leaves.drain(..left);
        let entered = self
            .interest_delta_entered_sent
            .min(self.interest_pending.enters.len());
        self.interest_pending.enters.drain(..entered);
        self.interest_delta_tick = None;
        self.interest_delta_left_sent = 0;
        self.interest_delta_entered_sent = 0;
        self.interest_seeded = true;
    }

    /// The token the snapshot frame at `tick` is minted with, or `None` for a peer with no salt yet.
    ///
    /// Derived rather than stored, so verifying an ack costs no per-frame bookkeeping and works for any
    /// tick — including one the sent log has already expired, which is exactly the range a round-trip
    /// sample may still be drawn from.
    fn frame_token(&self, tick: u64) -> Option<u32> {
        self.token_salt
            .map(|salt| siphash24(&salt, &tick.to_le_bytes()) as u32)
    }

    /// Whether `token` is the value this peer could only be holding because the frame at `ack` reached
    /// it. `false` for a peer with no salt, which is a peer that has been sent no frame to prove.
    ///
    /// **A bare comparison, deliberately**, where [`orbitnet_core::auth`] folds its MAC comparison to a
    /// single test. Two reasons, and the first alone would not be enough:
    ///
    /// 1. It compares two `u32`s, which is one machine comparison. The leak that folding closes is a
    ///    walk that returns at the first differing BYTE, turning 2^64 guesses into 8 × 256.
    /// 2. **A correct guess is worth less than no guess at all.** The only ack a peer cannot already
    ///    prove is one for a frame it does not hold — a HIGHER ack than it earned — and [`note_ack`]
    ///    measures `current - ack`, so a higher ack reports a SHORTER round trip and shrinks that peer's
    ///    own rewind window. It also promotes an `acked_base` for a row the peer does not hold, so the
    ///    next masked delta against that row is undecodable and the peer NACKs itself into full blocks.
    ///    The profitable direction is a LOWER ack, and there the peer quotes a token it genuinely holds.
    ///
    /// A forged MAC buys entry to the session, which is why that one is folded. This buys a self-inflicted
    /// `want_full` storm.
    ///
    /// [`note_ack`]: PeerState::note_ack
    fn ack_is_proven(&self, ack: u64, token: u32) -> bool {
        self.frame_token(ack) == Some(token)
    }

    /// Stop believing this peer holds any entity-manifest table.
    ///
    /// **The one repair for every way the manifest stream can break**, as a method a test can call
    /// without a `SceneTree`. Two callers, and they are the two breaks a live connection can suffer:
    /// a hello that rekeys it (the client restarted its session and its table went with it), and a
    /// [`FrameHeader::FLAG_WANT_MANIFEST`] NACK (the client could not apply a delta). The third
    /// break — a reconnect — needs no call, because the rejoiner arrives on a fresh `PeerState`.
    ///
    /// Generation `0` is not a sentinel that needs handling downstream: it is the generation of the
    /// empty table, so [`manifest_owed`] answers this peer with the whole table on the next publish
    /// and with nothing at all in a session that has published nothing.
    fn forget_manifest(&mut self) {
        self.manifest_generation = 0;
    }

    /// Take a NACK from this peer: ask for full rows, and stop trusting every delta base held for it.
    ///
    /// **AN ACK PROVES A FRAME ARRIVED, NOT THAT ITS BLOCKS INTEGRATED**, and the difference is what
    /// makes a NACK self-sustaining without this. [`consume_ack`] promotes `acked_base` for every
    /// entity a confirmed frame carried; a receiver that answered [`StateIntegration::NoBase`] for
    /// one of those blocks never called `keep_auth_row` and stored nothing. The sender is then
    /// holding a base the receiver provably does not have, and every later masked delta against it
    /// fails the same way -- forever, because `want_full` is per-peer and names no entity, so there
    /// is nothing to invalidate selectively.
    ///
    /// Dropping the whole map is what breaks the loop. `reference` degrades to `None` for this peer,
    /// so its next blocks are full rows: those always decode, are always stored, and re-promote a
    /// base that is real. The cost is one burst of full state per NACK -- which is what the NACK
    /// asked for. Leaving the map in place costs one every tick instead.
    ///
    /// [`consume_ack`]: PeerState::consume_ack
    fn note_nack(&mut self) {
        self.want_full = true;
        self.acked_base.clear();
    }

    /// Consume one arriving acknowledgment whole: check its proof, raise `newest_ack`, take a
    /// round-trip sample, and promote to `acked_base` every entity tick the frames it confirms carried.
    ///
    /// **The proof gate is first, and it gates everything after it.** `ack_tick`, `ack_bits` and the
    /// entity ticks they promote are all grants made on the strength of one claim — that the frame at
    /// `ack` arrived — so a claim that does not carry the token the server minted for that tick buys
    /// none of them. See [`FrameHeader::ack_token`] and [`PeerState::token_salt`].
    ///
    /// `ack_bits` rides on the proven tick rather than proving itself: it names the 32 frames before
    /// `ack`, all of them older, and an entity tick promoted from one of those is only ever a base the
    /// server may delta against. A peer that lies in the bits breaks its own delta chain and NACKs.
    ///
    /// The whole of what an arriving ack does, as a method a test can call without a `SceneTree`.
    fn consume_ack(
        &mut self,
        ack: u64,
        token: u32,
        ack_bits: u32,
        current: u64,
        tick_ms: f64,
    ) -> AckOutcome {
        if ack == 0 {
            return AckOutcome::Empty;
        }
        if !self.ack_is_proven(ack, token) {
            return AckOutcome::Unproven;
        }
        // Raise `newest_ack` and measure the round trip, but ONLY when the ack has actually advanced
        // -- see `note_ack` for why an unadvanced one must not be measured. `note_ack` uses
        // `saturating_sub` because an ack can name a frame the accumulator has not reached: ticks are
        // published before the send phase runs, and a peer's clock leads.
        self.note_ack(ack, current, tick_ms);
        let newest_ack = self.newest_ack;
        let mut promoted: Vec<(u64, u64)> = Vec::new();
        self.sent_log.retain(|(frame, entities)| {
            let confirmed = *frame == ack
                || (*frame < ack
                    && ack - *frame <= 32
                    && (ack_bits >> (ack - *frame - 1)) & 1 == 1);
            if confirmed {
                promoted.extend_from_slice(entities);
                return false;
            }
            // Older than the ack window can reach: it will never be confirmed.
            frame.saturating_add(32) >= newest_ack
        });
        for (id, tick) in promoted {
            let entry = self.acked_base.entry(id).or_insert(0);
            *entry = (*entry).max(tick);
        }
        AckOutcome::Consumed
    }

    /// Consume an arriving acknowledgment: raise `newest_ack`, and take a round-trip sample IF the
    /// ack advanced. Returns whether it did.
    ///
    /// **Only an ADVANCING ack is measured, and that is what makes withholding useless.** The gap
    /// `now - newest_ack` grows on its own every tick a peer stays quiet, so measuring on every
    /// arriving frame would let a client raise its own estimate to the ceiling by simply never
    /// moving its `ack_tick`. That costs the attacker nothing — it does not even degrade its own
    /// stream, because an unadvanced `acked_base` makes the SERVER fall back to full blocks, so a
    /// quiet peer receives MORE state and the bandwidth is spent by the victim. Measuring only on
    /// advance means a peer that goes quiet contributes no samples at all and its estimate stays
    /// frozen at its last honest measurement, while a peer on a genuinely lossy or slower route
    /// still acks whenever a packet lands and is measured correctly.
    ///
    /// **The ack this measures has already been proven.** Production reaches here only through
    /// [`PeerState::consume_ack`], which has matched the frame token the peer quoted back against the one
    /// the server minted for that tick, so `ack` names a frame that provably arrived rather than a number
    /// the peer chose. An ack for a frame the peer never received — forged, guessed, or replayed out of another
    /// session — carries the wrong token and never reaches this function.
    ///
    /// **What proof does NOT settle**, because the claim has been overstated twice already: a token says
    /// the peer received the frame it names, not that the peer received nothing newer. A client that
    /// advances at full rate while holding a constant lag quotes a real token every time, is measured at
    /// that lag, and reads exactly like a genuinely slow peer — see the residual test below. No wire field
    /// closes that one: `current - ack` is the whole round trip whatever lead the client runs at, so there
    /// is no second quantity for the server to derive an independent figure from, and a client that
    /// under-reports gains nothing a client routing through a traffic shaper does not already gain
    /// honestly.
    ///
    /// **The containment for the remainder is two ceilings, and they bound different things.**
    /// [`OrbitNet::rtt_believed_max_ms`] bounds what the server believes about the link, so every
    /// consumer of [`OrbitNet::peer_rtt_ms`] gets a bounded figure; `NetLagComp.max_delay_ms` bounds
    /// how deep a shot rewinds. Neither is applied here — the sample is stored as measured, and
    /// [`PeerState::rtt_believed_ms`] says why.
    fn note_ack(&mut self, ack: u64, current: u64, tick_ms: f64) -> bool {
        if ack <= self.newest_ack {
            return false;
        }
        self.newest_ack = ack;
        if self.rtt_samples.len() >= RTT_WINDOW {
            self.rtt_samples.pop_front();
        }
        let ms = (current.saturating_sub(ack) as f64 * tick_ms) as f32;
        self.rtt_samples.push_back(ms.clamp(0.0, RTT_SAMPLE_MAX_MS));
        true
    }

    /// This peer's round trip in milliseconds, as the MINIMUM of the recent window, or `None`
    /// before any sample has arrived.
    ///
    /// The minimum rather than a mean or a percentile, and it is HALF of the security argument;
    /// [`PeerState::note_ack`] is the other half. A peer influences a sample in exactly one
    /// direction: delaying an ack inflates it, and nothing it can send deflates it (`newest_ack`
    /// only ever rises). A minimum filter therefore ignores every inflated sample as long as ONE
    /// honest round trip lands inside the window. Standard practice for the same reason a
    /// congestion controller tracks min-RTT rather than mean-RTT: the floor is the path, the rest
    /// is queue.
    fn rtt_ms(&self) -> Option<f32> {
        self.rtt_samples
            .iter()
            .copied()
            .fold(None, |acc: Option<f32>, ms| {
                Some(acc.map_or(ms, |best| best.min(ms)))
            })
    }

    /// The same estimate, bounded by what this server is willing to BELIEVE about one link:
    /// [`PeerState::rtt_ms`] capped at `ceiling_ms`. `None` for a peer with no sample, exactly as
    /// the raw estimate answers.
    ///
    /// **It bounds the BELIEF, not the acknowledgment.** Every ack that proves its frame token is
    /// still consumed whole — `newest_ack` still rises, `acked_base` is still promoted, the sample
    /// still enters the window. Only the figure handed to a consumer is capped. Refusing the ack
    /// instead would break the peer's own delta chain over a measurement policy, which is a
    /// bandwidth failure imposed on a peer that may simply be far away.
    ///
    /// **Clamped at the READ rather than inside [`PeerState::note_ack`]**, and that is a deliberate
    /// choice rather than the obvious one:
    ///
    /// - Clamping the stored sample would re-tune five existing `#[test]` cases that assert exact
    ///   millisecond figures over gaps above this ceiling. Re-tuning five assertions is where one of
    ///   them quietly stops asserting anything.
    /// - The window keeps its honest contents, so the minimum filter still sees the real
    ///   distribution and [`OrbitNet::peer_rtt_raw_ms`] can report it. A clamped store would make
    ///   every peer above the ceiling indistinguishable from every other one, and the ceiling gauge
    ///   in the bandwidth metrics could not be computed at all.
    ///
    /// **What this does NOT close.** A client that advances its ack at full rate while holding a
    /// constant lag still reads as a slow link, up to `ceiling_ms`. No wire field closes that — see
    /// [`PeerState::note_ack`] for why there is no second quantity to derive an independent figure
    /// from. What changes is that the residual is now bounded by a number the server owns, instead
    /// of by [`RTT_SAMPLE_MAX_MS`], which is 40x any rewind policy and never binds.
    fn rtt_believed_ms(&self, ceiling_ms: f32) -> Option<f32> {
        self.rtt_ms().map(|ms| ms.min(ceiling_ms))
    }
}

/// How many of `peers` are connected AND have a raw round-trip estimate above `ceiling_ms` — the
/// `rtt_at_ceiling_peers` gauge in [`OrbitNet::bandwidth_metrics`].
///
/// **A gauge, not a rate**, like `starve_ticks_max` beside it: it is the count as of the publish, not
/// a per-second figure, so a window in which nothing changed still reports the standing count.
///
/// **Counted at the once-per-second publish, never in the per-tick read path.** It is a scan of every
/// connected peer's sample window, which is the shape of work the send path must not do per tick, and
/// nothing acts on it — it is a diagnostic that says how much of the session the belief ceiling is
/// currently binding on.
///
/// `synced` peers only, so the count is a subset of the `peers` figure published beside it. Strictly
/// above the ceiling: a peer measured at exactly the ceiling is believed in full and is not being
/// bound by it.
fn rtt_at_ceiling_peers<'a>(peers: impl Iterator<Item = &'a PeerState>, ceiling_ms: f32) -> u64 {
    peers
        .filter(|p| p.synced && p.rtt_ms().is_some_and(|ms| ms > ceiling_ms))
        .count() as u64
}

/// The OrbitNet session node. Owned and driven by the `Net` facade autoload.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct OrbitNet {
    base: Base<Node>,

    /// Simulation ticks per second when decoupled from physics.
    #[export]
    tickrate: i32,

    /// Whether the net tick is driven by the physics step (coupled) or the wall clock.
    #[export]
    sync_to_physics: bool,

    /// How many ticks of history/resim depth to retain.
    #[export]
    history_limit: i32,

    /// Ceiling on simulation ticks run in a single frame.
    #[export]
    max_ticks_per_frame: i32,

    /// Ticks of intentional input delay (stamp input this far into the future).
    #[export]
    input_delay: i32,

    /// Present this many ticks in the past (latency masking; resim cost unchanged).
    #[export]
    display_offset: i32,

    /// Test hook: force the rollback loop at least this deep every frame (0 = off).
    #[export]
    resim_force: i32,

    /// Bound on the local clock stretch used to chase the server clock (decoupled mode).
    #[export]
    max_stretch: f64,

    /// Per-peer snapshot byte budget per tick; lowest-priority entities defer past it.
    #[export]
    send_budget: i32,

    /// Interest radius in meters (0 = no **distance** filter).
    ///
    /// The 100-player lever: with a radius set, each peer receives only the entities within it of
    /// that peer's own body, with a 1.25x exit hysteresis so boundary entities don't flicker.
    /// This covers the **state lane** too, but only for channels that declare
    /// `relevancy = ANCHORED` and a resolvable `anchor_property`; everything else has no distance
    /// to be culled by, which is what every state channel was before.
    ///
    /// **`0` IS NOT "NO INTEREST FILTER" — it is "no radius".** Membership is the other axis and is
    /// not switched off here: when any entity declares one, the interest pass runs at radius `0`,
    /// refuses the worlds a peer is not in, and is billed as `interest_ms` like any other tick. A
    /// game that declares no memberships does get the whole pass skipped at `0`, which is what this
    /// used to say unconditionally.
    ///
    /// **At `0` the pass never takes the spatial index**, whatever the occupancy: there is no
    /// distance to index, so a rebuild would cost a tick and refuse nothing. `bandwidth_metrics()`'s
    /// `interest_grid` reads `0.00` for the whole session.
    #[export]
    aoi_radius: f64,

    /// The scale the PRIORITY BANDS are derived from (edges at `scale/3` and `2*scale/3`), in
    /// meters. Independent of [`Self::aoi_radius`] on purpose, and 0 falls back to treating every
    /// entity as near.
    ///
    /// These are two different questions with answers two orders of magnitude apart. The cull radius
    /// asks *send this at all*, so it must clear the longest engagement in the game — the sniper's
    /// 2000 m. The band scale asks *how often relative to everything else*, so it must resolve the
    /// distances a firefight happens over. Deriving both from one number meant a value that banded
    /// usefully culled bodies players were shooting at, and a value safe for the sniper banded
    /// everything on a 60 m arena as `Near`, where the weight is a constant that cancels out of the
    /// ordering. See `priority::band_of`.
    ///
    /// It only has an effect while the interest pass runs, because that pass is what produces the
    /// per-entity distances — so a session with [`Self::aoi_radius`] at 0 has no distances to band
    /// on and every entity reports near, whatever this is set to. The shipped configuration sets a
    /// radius wide enough to cull nothing on any current arena precisely so the distances exist.
    #[export]
    aoi_band_radius: f64,

    /// Hard cap on one peer's interest set (0 = uncapped). The nearest N cullable entities win;
    /// unconditionally-relevant ones are never evicted by it.
    #[export]
    aoi_max_entities: i32,

    /// Rate tiering: send the mid band every other tick and the far band every fourth.
    ///
    /// **Ships off.** The priority scorer already produces a weight-proportional send rate per band
    /// without a fixed schedule, so this is a hard cap for when even that is too expensive — and it
    /// is the item most likely to make remote bodies visibly stutter, which is why it must not be
    /// turned on before `interarrival_*` proves the far band is genuinely far.
    #[export]
    rate_tiering: bool,

    /// Seconds a dropped peer's session is held open for it to come back to. `0` disables resume.
    ///
    /// Server-side. It is a WALL-CLOCK window, not a tick count: a player alt-tabs, a router
    /// renegotiates, a phone changes network — none of those are measured in simulation ticks, and a
    /// window denominated in ticks would be a different policy at every rate.
    ///
    /// **Sizing it is a game decision with a real cost on both sides.** The entity is held for the whole
    /// window: nobody else can be given it, it keeps replicating, and it acts on no input (see
    /// `OrbitRollbackSynchronizer::mark_orphaned_authoritative`). Too short and a player who dropped on a
    /// loading screen comes back to a stranger in their body; too long and a full session refuses newcomers
    /// while it waits for players who left for good. 30 s is the default because it clears the ordinary
    /// causes — a renegotiated route, an application switch — without holding a competitive place through a
    /// whole engagement.
    #[export]
    reconnect_grace: f64,

    /// Which claims on an identity this server grants: `0` always, `1` only if the incumbent has dropped,
    /// `2` never. Clamped to a known value on set; anything else reads back as `0`.
    ///
    /// SERVER-SIDE. It is one of the two inputs to [`resume_grant`], and that function is the whole rule.
    ///
    /// **THE DEFAULT IS `0` (ALWAYS), AND THAT IS A DELIBERATE CHOICE RATHER THAN AN OMISSION.**
    ///
    /// - **The token is what removed the reachable attack, not the policy.** A claim is granted only when
    ///   the presented [`Handshake::resume_token`] matches the one on record, so a peer that merely
    ///   observed another's session id — off a roster broadcast, a kill feed, a log line, a screenshot —
    ///   is refused under ALWAYS exactly as it is under NEVER.
    /// - **What ALWAYS is still open to is an on-path observer**, who reads the welcome and can quote the
    ///   token verbatim. ONLY_IF_DROPPED buys nothing against that adversary: it can read the traffic, so
    ///   it can already do everything the client can, and it can wait for the drop like anybody else.
    /// - **What ALWAYS buys is every honest fast reconnect.** A relaunched client routinely arrives before
    ///   the transport reports its old socket gone — measured here at anywhere from 45 s to never on ENet's
    ///   defaults — and under ONLY_IF_DROPPED that player is refused their own body for the whole of that
    ///   span.
    ///
    /// ONLY_IF_DROPPED is a supported setting and one call, for a game that will not accept a live takeover
    /// on any terms. NEVER is for a game with no reconnect story at all.
    #[export]
    #[var(set = set_resume_policy)]
    resume_policy: i64,

    /// What this session does with a connection's seats once that connection ends: `0` hold, `1`
    /// release when the grace window closes, `2` release the moment the transport drops. Clamped to
    /// a known value on set; anything else reads back as `0`.
    ///
    /// SERVER-SIDE. It selects [`SeatReleasePolicy`] and nothing else: [`releases_seats`] is the whole
    /// rule, this node acts on its answer, and a release means the bodies that connection drove have
    /// their input handed back to the server (`OrbitRollbackSynchronizer::release_seat`) so their
    /// seats close.
    ///
    /// **THE DEFAULT IS `0` (HOLD), AND IT STAYS THERE.** Four reasons, and each of them on its own
    /// would be enough:
    ///
    /// - **It is what the pinned released binary already does.** The cdylib a project has on disk is
    ///   refreshed only at a release tag, so new GDScript routinely runs against an older one. A
    ///   default that released seats would mean the same project despawns players' viewpoints or does
    ///   not, depending on which binary happened to be installed — a behavior difference no source
    ///   change explains.
    /// - **It is the documented contract in three places**: [`Self::peer_session_expired`],
    ///   [`Self::seat_closed`], and the sizing note on [`Self::reconnect_grace`] all state that a
    ///   dropped connection keeps its seats until the game says otherwise. Changing the default
    ///   silently falsifies all three for every existing consumer.
    /// - **It is what the reconnect grace window is for.** A player whose wifi drops a burst of
    ///   packets comes back to the body they left, and that only works because nothing took it away
    ///   in the meantime. A session that released on every transient drop would despawn players for a
    ///   hiccup, which is the failure [`Self::reconnect_grace`] exists to prevent.
    /// - **The addon does not know what a released body should become.** Freed, parked as a corpse,
    ///   handed to a queued joiner, kept as an idle NPC — those are game rules, and the facade
    ///   declines to make that decision. THIS DOES NOT MAKE IT EITHER: `1` and `2` hand input back to
    ///   the server and close the seat, and freeing the node stays the game's call, exactly as it is
    ///   for a body a cull stopped sending.
    ///
    /// What choosing `1` or `2` buys is **one call instead of a second table**. The alternative every
    /// game hand-rolls is a peer-to-bodies map maintained beside the roster this node already derives
    /// from ownership, and two tables answering "which bodies does this connection drive" are two
    /// things that can disagree — while only one of them, ownership, is what the anti-forgery check
    /// on a received input block reads.
    #[export]
    #[var(set = set_seat_release_policy)]
    seat_release_policy: i64,

    /// The largest round trip this server will BELIEVE about a peer, in milliseconds. Clamped into
    /// `0.0..=RTT_SAMPLE_MAX_MS` on set; defaults to [`RTT_BELIEVED_MAX_MS_DEFAULT`].
    ///
    /// SERVER-SIDE. It caps [`Self::peer_rtt_ms`] and nothing else: no acknowledgment is refused, no
    /// stored sample is altered, and [`Self::peer_rtt_raw_ms`] still reports the unclamped window
    /// minimum for a scoreboard ping or an admin tool. See [`PeerState::rtt_believed_ms`].
    ///
    /// **What it is for.** A round-trip estimate is derived from acknowledgments the client chooses
    /// when to send, and the one thing the three ack rules do not close is a client that advances at
    /// full rate behind a constant lag: it quotes a real frame token every time and is believed at
    /// that lag. Without this the only bound on that figure was [`RTT_SAMPLE_MAX_MS`] at ten seconds,
    /// which is 40x any rewind policy and never binds — so a deeper rewind was there for the asking.
    ///
    /// **It does not close the residual, it bounds it.** A client behind a constant lag still reads as
    /// a slow link, up to this value. Lowering it is the only lever that narrows that, and it narrows
    /// the honest slow link by exactly as much: the two are indistinguishable by construction.
    ///
    /// `0.0` believes nothing about anybody — every connected peer reports a 0 ms round trip, which a
    /// per-shooter rewind reads as the shallowest window there is.
    #[export]
    #[var(set = set_rtt_believed_max_ms)]
    rtt_believed_max_ms: f64,

    mode: i64,
    running: bool,
    synced: bool,
    hello_pending: bool,
    accumulator: TickAccumulator,
    emitting_tick: Option<u64>,
    rollback_tick_now: Option<u64>,

    clock: ClockEstimator,
    slew: CoupledSlew,
    /// Client: window of server-reported input-arrival margins (the adaptive-lead signal).
    lead: LeadTracker,
    /// Client: extra ticks of clock lead the margin loop has dialed in, folded into the offset
    /// the slew (coupled) or stretch (decoupled) chases. Holds the worst margin slightly
    /// positive, which is what keeps the server's resim window shallow without wasted latency.
    lead_bias_ticks: f64,
    ping_seq: u64,
    ping_timer: f64,
    hello_timer: f64,
    stretch_now: f64,
    /// Wall-clock stamp of the previous decoupled `process` call, in usec (0 = none yet). The
    /// engine's own `delta` is CLAMPED to `max_physics_steps_per_frame / physics_tps` (~66 ms at
    /// the shipped 120 Hz) and everything past the clamp is silently dropped from game time — so
    /// every render hitch beyond one clamp's worth tore this peer's tick timeline away from wall
    /// clock, invisibly, before any of our code ran. Measured directly: a 600 ms SIGSTOP reaches
    /// `_process` as one ~130 ms delta. The tick accumulator is therefore fed a self-measured
    /// wall delta instead; its own cap/retention/discard bounds are exactly the catch-up policy
    /// the engine clamp was standing in for, applied where the clock can see it.
    last_process_wall_us: u64,

    planner: ResimPlanner,
    rollback_entities: BTreeMap<u64, Gd<OrbitRollbackSynchronizer>>,
    state_entities: BTreeMap<u64, Gd<OrbitStateSynchronizer>>,
    peers: HashMap<i32, PeerState>,
    /// This peer's own session identity, sent in its handshake. `0` claims none.
    session_id: u64,
    /// Client: the **resume token** a server issued for [`Self::session_id`], quoted back in every later
    /// handshake. `0` holds none.
    ///
    /// Server-minted and client-stored: it arrives in [`Welcome::resume_token`] and goes out in
    /// [`Handshake::resume_token`]. It survives [`Self::stop`], because it describes an identity rather
    /// than a session — but it does NOT survive the process, so a game that wants a restarted client to
    /// resume has to persist it beside the session id and write it back through
    /// [`Self::set_resume_token`].
    ///
    /// A welcome carrying `0` does not clear it. That answer means "this connection holds no identity of
    /// ours" — an anonymous seat, or a refused resume — and forgetting a live token on the strength of it
    /// would cost the peer its next honest reconnect.
    ///
    /// **ONE TOKEN PER CLIENT, naming whichever server last issued one.** A token is minted per server per
    /// identity, and joining a second server under the same identity replaces the stored value — so the
    /// resume on the first server is forfeited. Storing one per server would need a server identity the
    /// protocol does not carry, and the case it would buy is a player alternating between two servers inside
    /// one grace window, which is 30 s by default.
    resume_token: u64,
    /// Client: the key this session's datagrams are authenticated with, and the window that refuses a
    /// replayed one from the server. Seated in [`OrbitNet::start`] from [`Self::session_nonce`].
    ///
    /// The server holds no session key of its own — a session's key is derived from what the client
    /// minted, and the server keeps one [`SessionAuth`] per connected peer on [`PeerState`].
    session_auth: Option<SessionAuth>,
    /// Client: the 16 bytes drawn fresh for this session and carried in the handshake.
    ///
    /// **It is the key itself when no [`Self::session_secret`] is set, and only a nonce when one is.**
    /// The draw is the same either way; what changes is whether [`Self::session_auth`] is seated with
    /// these bytes or with what they and the secret derive. Held separately from the key because under a
    /// secret the two differ and the handshake carries this one.
    session_nonce: Option<[u8; KEY_LEN]>,
    /// The **shared session secret**, already folded to [`KEY_LEN`] bytes, or `None` for a session that
    /// configured none.
    ///
    /// The game distributes it out of band on a channel it has already authenticated — a lobby's
    /// metadata, a matchmaker's ticket, anything the player did not type into a public field — and hands
    /// it to both ends before either starts. It changes who can forge a datagram in this session:
    ///
    /// | | No secret | A secret |
    /// | --- | --- | --- |
    /// | What the handshake carries | the session key, in the clear | a nonce, in the clear |
    /// | What an on-path observer can do | everything the client can | read the traffic, forge nothing |
    ///
    /// **THE SECRET IS A DERIVATION INPUT AND IS NEVER SEATED AS THE SESSION KEY**, however much shorter
    /// that implementation looks. Sequence numbers restart at 1 on every join and the replay window only
    /// ever knows the session in front of it, so a key that did not change between joins would make every
    /// datagram captured in one session a valid, unreplayed datagram in the next. The per-join nonce is
    /// what keeps the key per-join. See [`session_key_from`].
    ///
    /// It is never read back out: there is [`OrbitNet::has_session_secret`] and no getter for the bytes.
    session_secret: Option<[u8; KEY_LEN]>,
    /// Server: the sessions of dropped peers, held open until their grace window closes.
    resume: ResumeTable,
    /// Server: peer ids whose seats a `RELEASE_ON_DROP` policy owes a release, queued by
    /// [`OrbitNet::_on_peer_disconnected`] and drained by [`OrbitNet::drain_seat_releases`].
    ///
    /// **THE RELEASE IS QUEUED RATHER THAN IMMEDIATE, AND THAT IS NOT A LATENCY CHOICE.**
    /// `_on_peer_disconnected` is a transport callback, and `SceneMultiplayer` delivers it from
    /// inside `poll()` — which this node calls from the tick loop, with a `bind` held on the
    /// synchronizer it is part-way through stepping. A release walks the registry and needs
    /// `bind_mut()` on every entity it touches, and godot-rust answers that with a **borrow panic**,
    /// not a wrong value: the frame goes down rather than doing something slightly wrong.
    ///
    /// Draining at the frame boundary also puts the release on **the tick boundary every other seat
    /// write already lands on**, so a game sees one announcement per frame whatever caused it.
    ///
    /// Nothing is queued under the default policy, so a session that sets none never allocates here.
    pending_seat_releases: Vec<i32>,
    /// Server: the transport peer ids connected as of this frame, plus our own.
    ///
    /// Refreshed once per frame and read once per rollback entity per tick to decide which entities have
    /// lost their input author. The alternative — asking the engine per entity per tick — is the same
    /// hundredfold multiplier `input_owner_hint` exists to avoid, on an answer that changes when a peer
    /// connects and at no other time.
    live_peers: std::collections::HashSet<i32>,
    /// The session's map between entity ids and the dense `u16` slots the wire carries.
    ///
    /// **The server allocates; a client only holds what the manifest told it.** One field for both
    /// roles because both answer the same two questions — what does this slot name, and what slot
    /// names this entity — and the send and receive paths run on either side of a host.
    slots: SlotTable,
    /// Server: an entity registered or unregistered, so the slot table needs a pass.
    ///
    /// Stays raised while any registered entity is still without a slot, which is how a slot refused
    /// during its predecessor's reuse quarantine gets retried instead of stranding the entity.
    slots_dirty: bool,
    /// Server: the slot table has been reported exhausted once already. The condition persists for
    /// as long as the session stays at the cap, and a per-tick per-entity error would bury the log.
    slots_exhausted_warned: bool,
    /// Something may be owed on the entity-manifest channel: the table changed, a peer joined, or a
    /// peer asked for the whole table again.
    ///
    /// **It no longer means "the table changed".** A flush diffs the rebuilt table against
    /// [`Self::manifest_published`] and publishes nothing when the two agree, so raising this
    /// costs a rebuild and a diff rather than a broadcast.
    manifest_dirty: bool,
    /// The generation of the entity-manifest table the far end holds.
    ///
    /// **One field, two roles, the way [`Self::slots`] is one table for two roles.** On a SERVER it
    /// is the generation of [`Self::manifest_published`], and a delta names it as its base. On a
    /// CLIENT it is the generation of the table this peer holds, and a delta naming any other base
    /// is refused. The send path runs only on a server and the receive arm only on a client, so the
    /// two never overlap.
    ///
    /// `0` is "nothing has been published" on a server and "no table has been applied" on a client,
    /// and both describe the empty table.
    manifest_generation: u64,
    /// The entity-manifest rows the far end holds, ascending by slot.
    ///
    /// SERVER: what every peer at [`Self::manifest_generation`] holds, and the table the next diff
    /// is taken against. CLIENT: the rows this peer holds, which is what a delta is applied to and
    /// what the seat roster is projected from — a delta carries only the change, so the client has
    /// to keep the whole table to project anything derived from it.
    manifest_published: Vec<ManifestEntry>,
    /// Client: this peer has already been warned once that its manifest stream broke.
    ///
    /// Latched for the reason [`OrbitNet::note_unauthenticated`] latches its own: a server that
    /// sends undecodable frames sends them every tick, and under a flood the log is the second thing
    /// to fall over.
    manifest_break_warned: bool,
    /// Client: schema fingerprints announced by the server, checked as entities register.
    expected_schemas: HashMap<u64, (u32, u32)>,
    /// Client: newest snapshot frame tick received (our ack).
    newest_snapshot_tick: u64,
    /// CLIENT: whether a snapshot has landed since the last input frame went out.
    ///
    /// The input frame carries this peer's ack, and it is the only frame that does. See
    /// [`input_frame_is_owed`].
    snapshot_unacked: bool,
    /// Client: which of the 32 frame ticks before `newest_snapshot_tick` also arrived — rides
    /// every input header so the server deltas only against bases we provably hold.
    snapshot_ack_bits: u32,
    /// Client: the token the frame at `newest_snapshot_tick` carried, quoted back in every input
    /// header. It is what turns `newest_snapshot_tick` from an assertion into a claim the server can
    /// check; see [`FrameHeader::ack_token`]. Moves only when `newest_snapshot_tick` moves, so it always
    /// names the frame the ack names.
    snapshot_ack_token: u32,
    /// Client: raise WANT_FULL on the next input frame.
    want_full: bool,
    /// Client: raise WANT_MANIFEST on the next input frame — this peer could not apply a manifest
    /// delta and needs the whole table.
    ///
    /// **Losing the frame that carries it costs one tick.** The refusal zeroed
    /// [`Self::manifest_generation`] at the same moment, so the next delta fails its base check as
    /// well and raises this again.
    want_manifest: bool,
    /// Client: which owned body the next input frame's admission walk starts at.
    ///
    /// Only ever off zero when the frame is full, which takes several seats on one connection. See
    /// [`admit_input_blocks`].
    input_rotor: usize,

    m_resim_ticks: f64,
    m_rollback_ms: f64,
    /// The three phases `m_rollback_ms` used to hide. RESTORE writes a tick's recorded state and input
    /// back onto every replaying entity; SIM is the game code (`_rollback_tick`); RECORD captures the result.
    /// The capture-cost claim this project's docs lead with is about restore + record, and until these existed
    /// nobody could say what share of a rollback loop they were.
    m_restore_ms: f64,
    m_sim_ms: f64,
    m_record_ms: f64,
    m_net_ms: f64,
    m_rb_nodes: f64,

    // --- Send-path accounting. Raw counters accumulate on the hot path; `m_bw` is what
    // `metrics()` reads, republished once a window because `metrics()` is `&self`. ---
    m_bw: BandwidthMetrics,
    /// Mean ticks between admissions for one peer's rows, pooled across every band, republished
    /// once a window from `acc_peer_band`. Rebuilt rather than updated, so a peer that left during
    /// the window is absent from the next one instead of frozen at its last figure.
    m_peer_interarrival: HashMap<i32, f64>,
    bw_timer: f64,
    acc_tx_bytes: u64,
    acc_tx_datagrams: u64,
    acc_rx_bytes: u64,
    acc_rx_datagrams: u64,
    acc_blocks_admitted: u64,
    acc_blocks_deferred: u64,
    acc_blocks_culled: u64,
    acc_blocks_oversize: u64,
    acc_blocks_full: u64,
    acc_want_full_nacks: u64,
    acc_unproven_acks: u64,
    acc_stale_blocks: u64,
    acc_interest_us: u64,
    acc_interest_ticks: u64,
    /// Ticks of the window whose interest pass ran through the grid rather than the flat scan.
    ///
    /// Divided by `acc_interest_ticks` — every tick of the send loop, including the ones that
    /// skipped the pass entirely — so the published `interest_grid` is a fraction of the window and
    /// reads `0.0` in a session that never selects the grid.
    acc_interest_grid_ticks: u64,
    acc_interest_peer_ticks: u64,
    acc_interest_members: u64,
    acc_band_sends: [u64; 3],
    acc_band_members: [u64; 3],
    /// `(sends, members)` per peer over the window: the same two counts as `acc_band_sends` and
    /// `acc_band_members`, split by peer and pooled across the three bands.
    ///
    /// Send cadence is a per-peer quantity. The byte budget is charged per peer per frame and the
    /// candidate list is rebuilt per peer, so a peer with a small interest set gets its rows every
    /// tick while a peer in a dense part of the world waits several. Pooling the counts across peers
    /// publishes one figure that describes no peer in the session, and the lag-compensation rewind
    /// then charges every shooter the pool mean for a view lag only their own cadence earns.
    ///
    /// Pooled across bands rather than split by them. The consumer is the interpolation term in a
    /// shot's rewind depth, which is applied at every range and cannot see which band its target sits
    /// in. The per-band split stays global, where it answers whether the far band is genuinely far.
    ///
    /// Accumulated into per-peer locals on the hot path and folded in once per peer per tick. A hash
    /// lookup per candidate row is the cost this accounting exists to avoid.
    acc_peer_band: HashMap<i32, (u64, u64)>,
    win_peer_bytes: HashMap<i32, u64>,
    win_starve_ticks_max: u64,
    win_unsent_backlog_max: u64,

    // --- send-path allocation pools, reused every tick so a warm frame allocates nothing ---
    aoi_rows: Vec<EntityRow>,
    /// Where each SEAT observes from, ascending by `(peer, seat)` — so one connection's seats are a
    /// contiguous slice, found by `partition_point` rather than by a per-tick map.
    aoi_observers: Vec<(SeatId, PeerObserver)>,
    aoi_candidates: Vec<InterestCandidate>,
    /// `(seat, row index)` for every gathered row a peer drives, ascending — the index into
    /// `aoi_candidates` that has to be swapped for that peer's own view of it. Pooled and rebuilt
    /// once per tick; a peer's slice is found by binary search, not by rescanning the rows.
    ///
    /// Keyed by seat rather than by owner because it answers two questions now: which candidates to
    /// patch for a connection (all of them, whatever seat drives them — the datagram is shared), and
    /// **which seats that connection has at all**, including one whose body has no anchor yet and so
    /// appears in no observer.
    aoi_owned_rows: Vec<(SeatId, u32)>,
    /// The observers handed to whichever update path the tick selected, rebuilt per peer, plus
    /// what resolving them revealed. See [`ResolvedSeats`].
    aoi_seats: ResolvedSeats,
    aoi_seat_scratch: SeatScratch,
    /// The tick's spatial index, rebuilt **once per tick** and only on [`InterestPath::Grid`]. Its
    /// bucket `Vec`s are pooled inside it, so a warm grid tick allocates nothing; a session that
    /// never leaves [`InterestPath::Linear`] never fills it and pays for an empty `HashMap`.
    aoi_grid: InterestGrid,
    /// Which path the interest pass runs, held across ticks so the verdict has hysteresis.
    ///
    /// **The session decides and the game declares nothing** — there is no setter, because a wrong
    /// verdict costs time and nothing else: both paths compute the same members, the same distances
    /// and the same leaves, which is what licenses an automatic rule in place of a knob. See
    /// `orbitnet_core::interest`'s `PathSelector` for the measurements the rule reproduces.
    aoi_path: PathSelector,
    /// The per-world bounds accumulator [`InterestOccupancy::measure`] reuses, so measuring the
    /// candidate list costs no allocation after the first tick.
    aoi_occupancy: OccupancyScratch,
    /// One connection's own rows, handed to [`ConnectionInterest::update_grid_into`] as `also`.
    ///
    /// **The grid cannot hold a per-connection fact.** It is rebuilt once for every peer in the
    /// session, and a body the connection drives is always-relevant to that connection and to no
    /// other, so the override list carries exactly the rows the linear path patches into the shared
    /// candidate vector. Pooled and refilled per connection.
    aoi_overrides: Vec<InterestCandidate>,
    /// This peer's candidate set for the order build: `(id, distance²)`. Filled from the peer's
    /// interest when culling is on and from every row when it is off, so the order loop has one
    /// shape either way. Pooled, so a warm frame allocates nothing.
    aoi_members: Vec<(u64, f32)>,
    /// The union diff one connection's update reports, pooled across peers and ticks. Both halves
    /// are consumed in the same loop that produced them; see [`OrbitNet::update_interest`].
    aoi_delta: InterestDelta,
    order_scratch: Vec<(priority::Candidate, Band)>,
    /// Relevancy transitions awaiting their signal, as `(peer, entity id, entered)`.
    ///
    /// **Queued rather than emitted where they are found, and drained on a tick boundary** — the
    /// same rule `announce_seats` follows. A server finds them inside the send path and a client
    /// inside a packet handler, and emitting from either would run game code with a bind held on a
    /// synchronizer. See [`OrbitNet::announce_interest`].
    interest_events: Vec<(i32, u64, bool)>,
    /// The wire slots one connection's section carries, pooled so a warm tick allocates nothing.
    delta_left_scratch: Vec<u16>,
    delta_entered_scratch: Vec<u16>,
    /// CLIENT: the generation of the interest set this peer holds.
    ///
    /// Set by a whole [`FrameKind::InterestTable`] and compared against every delta section, so a
    /// section built before the table cannot undo it. See [`InterestDeltaSection::generation`].
    interest_mirror_generation: u64,
    /// CLIENT: whether this peer owes the server a [`FrameHeader::FLAG_WANT_INTEREST`].
    ///
    /// Raised four ways, and cleared only by being ANSWERED — by a whole set that this peer could
    /// name in full. Clearing it on send instead made it a one-shot NACK on an unreliable frame, with
    /// nothing to re-raise it on a session quiet enough to send no further sections.
    ///
    /// | Raised by | Because |
    /// | --- | --- |
    /// | a section naming an `entered` slot the manifest has not bound | nothing else will ever produce that enter |
    /// | a section stamped at a generation this peer does not hold | its baseline is one this peer is not holding |
    /// | a whole set carrying a slot this peer cannot name | the manifest that binds it re-announces nothing |
    /// | a whole set that does not decode | there is no next one unless this asks |
    want_interest: bool,
    /// CLIENT: the entities this peer has been told are in its interest.
    ///
    /// **A mirror of the server's set, not a derivation.** A client cannot compute its own interest
    /// — it has no candidate list, no radius and no anchor for anybody — so this is exactly what the
    /// sections it has received say, applied idempotently: a repeat of a section it already applied
    /// changes nothing and announces nothing, which is what makes re-sending one free.
    interest_mirror: std::collections::HashSet<u64>,
    /// CLIENT: whether any interest-delta section has ever been applied this session.
    ///
    /// Until one has, a client answers [`OrbitNet::entities_in_interest`] with everything it holds,
    /// because a server that culls nothing sends no section at all and "no section" must not read as
    /// "nothing is relevant to you".
    interest_mirror_seeded: bool,

    /// The session default for a connection that resolves no anchor: `UNANCHORED_OPEN` or
    /// `UNANCHORED_CLOSED`, always one of the two because [`clamp_unanchored_policy`] runs on set.
    /// See [`Self::set_unanchored_policy`].
    unanchored_policy: i64,
    /// Whether [`Self::update_interest`] ran on the last frame that built snapshots.
    ///
    /// **The other half of `stale`**, and the half a per-peer flag cannot carry: the pass is skipped
    /// wholesale when nothing can be culled (no radius, no declared membership), and every
    /// connection's cached [`AnchorReport`] then describes a tick the session has moved on from.
    /// Without this a getter would answer "centered at the origin in world 0" for every peer in a
    /// session that is replicating everything to everybody.
    interest_ran: bool,
    /// Seats already warned about by [`Self::warn_anchor_conflicts`], so one misconfiguration is one
    /// log line rather than one per tick.
    ///
    /// **Entries are REMOVED when the seat stops colliding**, which is what makes this once per seat
    /// per EPISODE rather than once per process. A game that fixes the configuration, changes map,
    /// and reintroduces the same mistake is told about it again — a set that only ever grew would
    /// report the second occurrence to nobody.
    anchor_conflicts: std::collections::HashSet<SeatId>,

    // --- the seat roster, and the announcement it feeds ---
    /// The seats this session has already announced. See [`Self::announce_seats`].
    seat_roster: SeatRoster,
    /// `(entity id, owner, seat)` for every replicated entity, ascending by id — the seat half of
    /// what the entity manifest carries.
    ///
    /// **The server derives it and a client is told it.** On a server it is rebuilt from the
    /// registry once per frame and compared against itself to decide whether the manifest owes a
    /// republish; on a client it is rebuilt from the manifest rows this peer holds
    /// ([`Self::manifest_published`]), which a delta patches rather than replaces. A roster built
    /// from a delta's own rows would drop every seat that delta was silent about, so the client arm
    /// runs off the held table — see [`Self::rebuild_seats_from_manifest`]. Both then project the
    /// same roster out of it, so a seat event means the same thing on either end of the link.
    entity_seats: Vec<(u64, i32, SeatIndex)>,
    /// Announcement scratch, pooled so a steady session allocates nothing: the server's per-frame
    /// rescan, the projected seat set, and the two transition lists [`SeatRoster::replace_into`] fills.
    seat_scan: Vec<(u64, i32, SeatIndex)>,
    seat_gather: Vec<SeatId>,
    seat_opened: Vec<SeatId>,
    seat_closed: Vec<SeatId>,
    /// Whether [`Self::entity_seats`] has changed since the roster was last projected from it.
    ///
    /// The projection is the sort-and-dedup plus the diff, and a steady session changes seats
    /// approximately never — so the flag is what keeps the per-frame cost at the rescan that detects
    /// the change, rather than at re-deriving an answer that did not move.
    seats_dirty: bool,

    mask_scratch: Vec<bool>,
    signals_connected: bool,
    debug_wire: bool,
    dbg_timer: f64,
    dbg_sent: u64,
    dbg_sent_bytes: u64,
    dbg_rx_applied: u64,
    dbg_rx_rejected: u64,
    dbg_rx_skipped: u64,
    dbg_rx_kinds: [u64; 8],
    /// Datagrams refused by [`OrbitNet::open_datagram`]: forged, replayed, or from a peer with no
    /// handshake. Counted always; printed under `ORBITNET_DEBUG`.
    dbg_rx_unauth: u64,
    /// Whether this session has already warned that it is refusing datagrams. One warning per
    /// session: under an actual flood the log is the second thing to fall over.
    auth_warned: bool,
    dbg_input_novel: u64,
    /// Input rows refused for carrying a non-finite float, counted per row rather than per block —
    /// a redundancy window re-sends the same poisoned tick, and the row count is what says how much
    /// of the receive path it is costing. Counted always; printed under `ORBITNET_DEBUG`.
    dbg_input_nonfinite: u64,
    dbg_resim_spans: u64,
    dbg_resim_ticks_total: u64,
    dbg_fresh: u64,
}

#[godot_api]
impl INode for OrbitNet {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            tickrate: 60,
            sync_to_physics: true,
            history_limit: 128,
            max_ticks_per_frame: 8,
            input_delay: 0,
            display_offset: 0,
            resim_force: 0,
            max_stretch: 1.05,
            send_budget: MAX_FRAME_PAYLOAD as i32,
            aoi_radius: 0.0,
            // The shipped value is seeded from `[orbitnet]` in `project.godot` by `net.gd`, like
            // every other session default. This is the unconfigured fallback, and it matches the
            // pre-decoupling behavior (everything Near) rather than inventing a policy here.
            aoi_band_radius: 0.0,
            aoi_max_entities: 0,
            rate_tiering: false,
            reconnect_grace: 30.0,
            // ALWAYS. Token-gated, so it refuses the peer that merely observed an identity; see the
            // property's own comment for why the policy stays permissive on top of that.
            resume_policy: RESUME_ALWAYS,
            // HOLD. See the property's own comment for why this one does not move.
            seat_release_policy: SEAT_RELEASE_HOLD,
            rtt_believed_max_ms: RTT_BELIEVED_MAX_MS_DEFAULT,
            mode: MODE_OFFLINE,
            running: false,
            synced: false,
            hello_pending: false,
            accumulator: TickAccumulator::new(TickRate::new(60)),
            emitting_tick: None,
            rollback_tick_now: None,
            clock: ClockEstimator::default(),
            slew: CoupledSlew::new(),
            lead: LeadTracker::new(),
            lead_bias_ticks: 0.0,
            ping_seq: 0,
            ping_timer: 0.0,
            hello_timer: 0.0,
            stretch_now: 1.0,
            last_process_wall_us: 0,
            planner: ResimPlanner::new(),
            rollback_entities: BTreeMap::new(),
            state_entities: BTreeMap::new(),
            peers: HashMap::new(),
            session_id: 0,
            resume_token: 0,
            session_auth: None,
            session_nonce: None,
            session_secret: None,
            resume: ResumeTable::default(),
            pending_seat_releases: Vec::new(),
            live_peers: std::collections::HashSet::new(),
            slots: SlotTable::new(),
            slots_dirty: false,
            slots_exhausted_warned: false,
            manifest_dirty: false,
            manifest_generation: 0,
            manifest_published: Vec::new(),
            manifest_break_warned: false,
            expected_schemas: HashMap::new(),
            newest_snapshot_tick: 0,
            snapshot_unacked: false,
            snapshot_ack_bits: 0,
            snapshot_ack_token: 0,
            want_full: false,
            want_manifest: false,
            input_rotor: 0,
            m_resim_ticks: 0.0,
            m_rollback_ms: 0.0,
            m_restore_ms: 0.0,
            m_sim_ms: 0.0,
            m_record_ms: 0.0,
            m_net_ms: 0.0,
            m_rb_nodes: 0.0,
            m_bw: BandwidthMetrics::default(),
            m_peer_interarrival: HashMap::new(),
            bw_timer: 0.0,
            acc_tx_bytes: 0,
            acc_tx_datagrams: 0,
            acc_rx_bytes: 0,
            acc_rx_datagrams: 0,
            acc_blocks_admitted: 0,
            acc_blocks_deferred: 0,
            acc_blocks_culled: 0,
            acc_blocks_oversize: 0,
            acc_blocks_full: 0,
            acc_want_full_nacks: 0,
            acc_unproven_acks: 0,
            acc_stale_blocks: 0,
            acc_interest_us: 0,
            acc_interest_ticks: 0,
            acc_interest_grid_ticks: 0,
            acc_interest_peer_ticks: 0,
            acc_interest_members: 0,
            acc_band_sends: [0; 3],
            acc_band_members: [0; 3],
            acc_peer_band: HashMap::new(),
            win_peer_bytes: HashMap::new(),
            win_starve_ticks_max: 0,
            win_unsent_backlog_max: 0,
            aoi_rows: Vec::new(),
            aoi_observers: Vec::new(),
            aoi_candidates: Vec::new(),
            aoi_owned_rows: Vec::new(),
            aoi_seats: ResolvedSeats::default(),
            aoi_seat_scratch: SeatScratch::default(),
            aoi_grid: InterestGrid::new(),
            aoi_path: PathSelector::new(),
            aoi_occupancy: OccupancyScratch::default(),
            aoi_overrides: Vec::new(),
            aoi_members: Vec::new(),
            aoi_delta: InterestDelta::default(),
            interest_events: Vec::new(),
            delta_left_scratch: Vec::new(),
            delta_entered_scratch: Vec::new(),
            interest_mirror: std::collections::HashSet::new(),
            interest_mirror_seeded: false,
            interest_mirror_generation: 0,
            want_interest: false,
            // OPEN. Today's behavior, and the only default that cannot take a world away from a
            // consumer whose binary is refreshed without their source changing.
            unanchored_policy: UNANCHORED_OPEN,
            interest_ran: false,
            anchor_conflicts: std::collections::HashSet::new(),
            seat_roster: SeatRoster::new(),
            entity_seats: Vec::new(),
            seat_scan: Vec::new(),
            seat_gather: Vec::new(),
            seat_opened: Vec::new(),
            seat_closed: Vec::new(),
            seats_dirty: false,
            order_scratch: Vec::new(),
            mask_scratch: Vec::new(),
            signals_connected: false,
            debug_wire: std::env::var("ORBITNET_DEBUG").is_ok(),
            dbg_timer: 0.0,
            dbg_sent: 0,
            dbg_sent_bytes: 0,
            dbg_rx_applied: 0,
            dbg_rx_rejected: 0,
            dbg_rx_skipped: 0,
            dbg_rx_kinds: [0; 8],
            dbg_rx_unauth: 0,
            auth_warned: false,
            dbg_input_novel: 0,
            dbg_input_nonfinite: 0,
            dbg_resim_spans: 0,
            dbg_resim_ticks_total: 0,
            dbg_fresh: 0,
        }
    }

    fn ready(&mut self) {
        // Both callbacks stay enabled: `sync_to_physics` can flip at runtime, so whichever
        // callback is idle now may drive the loop a moment later.
        self.base_mut().set_process(true);
        self.base_mut().set_physics_process(true);
        self.connect_multiplayer_signals();
    }

    fn physics_process(&mut self, delta: f64) {
        self.drain_pending();
        if self.running && self.sync_to_physics {
            self.step_coupled();
        }
        self.publish_tick_state();
        let _ = delta;
    }

    fn process(&mut self, delta: f64) {
        self.drain_pending();
        self.client_handshake_upkeep(delta);
        if self.running && !self.sync_to_physics {
            // Feed the accumulator the TRUE elapsed wall time, not the engine's `delta`: the
            // engine clamps its step to max_physics_steps_per_frame worth and silently drops the
            // rest, so a render hitch reached us pre-shrunk and the lost time surfaced minutes
            // later as clock offset (see `last_process_wall_us`). The accumulator's own
            // cap/retention/discard bounds this instead, where the clock can account for it.
            let now_us = Time::singleton().get_ticks_usec();
            let wall = if self.last_process_wall_us == 0 {
                delta
            } else {
                now_us.saturating_sub(self.last_process_wall_us) as f64 / 1_000_000.0
            };
            self.last_process_wall_us = now_us;
            self.step_decoupled(wall);
        } else {
            // Not driving the decoupled loop this frame (offline-coupled, or not running): a
            // stale stamp must not turn the whole idle span into backlog when the loop resumes.
            self.last_process_wall_us = 0;
        }
        self.publish_tick_state();
    }
}

#[godot_api]
impl OrbitNet {
    /// Emitted once per simulation tick, before input capture — the facade's `pre_tick`.
    #[signal]
    fn before_tick(delta: f64, tick: i64);

    /// Emitted once per simulation tick, after input capture.
    #[signal]
    fn after_tick(delta: f64, tick: i64);

    /// Emitted once per frame that ran at least one tick, after resimulation completes — the
    /// facade's `post_tick`.
    #[signal]
    fn after_rollback_loop();

    /// Server: a peer completed the handshake. `resumed_from` is the transport peer id it held before it
    /// dropped, or `0` for a first-time joiner.
    ///
    /// **This, not the transport's `peer_connected`, is where a game seats a player.** `peer_connected`
    /// fires when the socket comes up, which is before the handshake, so no identity is known yet and there
    /// is nothing to match a rejoiner against.
    ///
    /// **`resumed_from` NAMES A CONNECTION THAT MAY STILL BE UP.** It is whichever connection last claimed
    /// this identity, whether the server saw it drop or not — the second case is a relaunched client that
    /// beat its own keepalive timeout. Honoring it hands the new claimant that peer's body.
    ///
    /// **It is reported only for a claim the server GRANTED**, and a claim is granted only when the joiner
    /// quoted the [`Handshake::resume_token`] this server issued for that identity — see [`resume_grant`].
    /// A peer that merely observed somebody's session id cannot produce one, so it arrives here with
    /// `resumed_from` `0`. [`Self::resume_policy`] set to `ONLY_IF_DROPPED` additionally refuses every claim
    /// against a connection that is still up, which is the conservative rule as one setting.
    ///
    /// **`session_id` is the identity the connection was SEATED under, not the one it presented.** A refused
    /// claim on an identity somebody else still holds is seated anonymously as `0`, so this is always safe
    /// as a roster key.
    #[signal]
    fn peer_joined(peer: i64, session_id: i64, resumed_from: i64);

    /// Server: a peer's transport connection is gone. `held` is whether its session is being kept open for
    /// the grace window — `false` means it is already forgotten and the game should release its seat now.
    ///
    /// `held` is false for a peer that claimed no identity, with the grace window at `0`, and — the case
    /// worth knowing about — for a GHOST whose identity a returning player already took back, which is what
    /// a relaunched client that beat its own keepalive timeout leaves behind. Such a drop reports
    /// `session_id` `0`. See [`hold_on_drop`].
    #[signal]
    fn peer_dropped(peer: i64, session_id: i64, held: bool);

    /// Server: a held session's grace window closed with nobody claiming it. `peer` is the transport id it
    /// was last connected under, for logging.
    ///
    /// **This is the release point, and BY DEFAULT the addon does not act on it.** The entity is still
    /// there, still replicating, still owned by a peer id that no longer exists. What to do about that —
    /// free the body, hand its input back to the server with `set_input_authority(1)`, open the seat to the
    /// next joiner — is the game's decision, exactly as it is for an entity a cull stopped sending.
    ///
    /// **A consumer can now say otherwise in one call.** [`Self::seat_release_policy`] set to
    /// `RELEASE_ON_EXPIRY` hands every body this connection drove back to the server before this signal
    /// fires, so a handler that seats a replacement is not undone a frame later. It closes the seat and
    /// nothing more: the body is still in the scene, and freeing it is still the game's decision. The
    /// default is unchanged.
    #[signal]
    fn peer_session_expired(session_id: i64, peer: i64);

    /// A seat arrived on a connection that is already in session. Emitted on **both sides**.
    ///
    /// A seat is one owned viewpoint: `(peer, seat)`. It exists because some replicated body says it
    /// is driven by that connection under that label, so this fires the tick after
    /// `OrbitRollbackSynchronizer::assign_seat` (or an equivalent authority write) lands on the
    /// server, and on a client the tick after the manifest carrying it does.
    ///
    /// **A joining connection's first seat is announced here too.** It is the same event — a seat
    /// arriving — and a game that seats every player through one handler needs no second one for the
    /// first player on a connection. `peer_joined` says a connection completed the handshake, which
    /// is before it drives anything.
    #[signal]
    fn seat_opened(peer: i64, seat: i64);

    /// A seat left a connection that stays in session. Emitted on **both sides**.
    ///
    /// It fires when nothing drives `(peer, seat)` any more: the body was released
    /// (`OrbitRollbackSynchronizer::release_seat`), re-pointed at another connection, or
    /// unregistered. The connection itself is unaffected and may still hold other seats.
    ///
    /// **BY DEFAULT a dropped connection does not close its seats by itself.** Its bodies keep the
    /// authority they were given until the game changes them, which is deliberate and is the same
    /// rule [`Self::peer_session_expired`] states: what to do with a body whose player is gone — free
    /// it, hand it back, hold it for a reconnect — is the game's decision. Release the seat and this
    /// fires.
    ///
    /// **A consumer can now say otherwise in one call.** [`Self::seat_release_policy`] set to
    /// `RELEASE_ON_DROP` or `RELEASE_ON_EXPIRY` hands the connection's bodies back to the server at
    /// the drop or at the end of the grace window, and this fires from the announcement that follows.
    /// [`Self::release_peer_seats`] does the same for one connection on demand, under every policy.
    /// The default is unchanged.
    #[signal]
    fn seat_closed(peer: i64, seat: i64);

    /// An entity became relevant to one connection. Emitted on **both sides**, on a tick boundary.
    ///
    /// `peer` is the connection that gained it — a remote connection on a server, this peer's own id
    /// on a client — and `entity_id` is the opaque token `get_entity_id()` answers.
    ///
    /// **A server announces from its own interest pass; a client announces from the trailing
    /// interest-delta section on the snapshot it is already receiving.** The two therefore mean the
    /// same thing one round trip apart, exactly as [`Self::seat_opened`] does.
    ///
    /// **This is not a per-handle signal, and it cannot be one.** Its twin routinely names an entity
    /// this client has no node for — that is the case that matters — so there is no handle to hang it
    /// on. See [`Self::entity_left_interest`].
    #[signal]
    fn entity_entered_interest(peer: i64, entity_id: i64);

    /// An entity stopped being relevant to one connection: culled by distance, refused by a
    /// membership, evicted by the nearest-N cap, withheld by [`Self::set_entity_hidden`], or
    /// unregistered outright. Emitted on **both sides**, on a tick boundary.
    ///
    /// **ONE SIGNAL COVERS BOTH CAUSES.** "The server stopped sending you this" and "this entity
    /// unregistered" are the same fact to a game holding a node it can no longer update, and a client
    /// emits this from an unregister as well as from a cull. An entity culled and unregistered on the
    /// same tick fires it exactly once.
    ///
    /// **THIS IS THE RELEASE POINT AND THE ADDON DOES NOT ACT ON IT** — the same contract
    /// [`Self::peer_session_expired`] states. The node is still in the scene, still holding the last
    /// pose it received, and nothing frees, hides, reparents or teleports it. What to do about that
    /// is the game's decision.
    ///
    /// **Hide, do not free.** A cap eviction oscillates at the boundary — a body at the edge of
    /// `aoi_max_entities` leaves and re-enters as the population around it moves — and freeing on the
    /// leave turns that into spawn churn. Hiding costs nothing to undo.
    ///
    /// **Teleport on re-entry.** A body that moved while it was away is interpolating from the pose
    /// it had when the rows stopped, so it would fly across the world over one tick.
    /// `NetInterpolatorHandle.teleport()` is what suppresses that.
    #[signal]
    fn entity_left_interest(peer: i64, entity_id: i64);

    // ------------------------------------------------------------------
    // Session control (facade API)
    // ------------------------------------------------------------------

    /// Set the network role: 0 offline, 1 client, 2 server, 3 host.
    #[func]
    fn set_mode(&mut self, mode: i64) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        if mode == MODE_OFFLINE {
            self.stop();
        }
    }

    /// The current network role.
    #[func]
    fn mode(&self) -> i64 {
        self.mode
    }

    /// Start the tick loop. On a client this begins the handshake; ticking starts when the
    /// server's welcome lands. Returns the tick the loop starts from.
    #[func]
    fn start(&mut self) -> i64 {
        let rate = self.effective_rate();
        self.accumulator = TickAccumulator::new(rate);
        self.accumulator
            .set_max_ticks_per_frame(self.max_ticks_per_frame.max(1) as u32);
        self.clock.clear();
        self.slew.reset();
        self.planner.clear();
        self.newest_snapshot_tick = 0;
        self.snapshot_ack_bits = 0;
        self.snapshot_ack_token = 0;
        self.snapshot_unacked = false;
        self.lead.clear();
        self.lead_bias_ticks = 0.0;
        self.want_full = false;
        // Both client NACK flags describe the session that just ended. A manifest NACK carried into
        // the next one would ask a server that has published nothing for a table it does not have.
        self.want_manifest = false;
        self.manifest_break_warned = false;
        self.ping_timer = 0.0;

        self.auth_warned = false;
        if self.mode == MODE_CLIENT {
            self.synced = false;
            self.running = false;
            // A FRESH DRAW per session, never the previous one. Restarting the sequence numbers under
            // a key an observer already saw would make every datagram captured from the last session
            // replayable into this one — equally true of the key these 16 bytes ARE with no secret set
            // and of the key they DERIVE with one.
            let nonce = Self::mint_session_key();
            self.session_nonce = Some(nonce);
            self.session_auth = Some(SessionAuth::new(session_key_from(
                self.session_secret.as_ref(),
                nonce,
            )));
            self.send_hello();
        } else {
            // Server, host, and the sessionless smoke path are their own ground truth.
            self.synced = true;
            self.running = true;
        }
        self.current_tick()
    }

    /// Stop the tick loop and drop all session state, leaving the tick index where it is.
    #[func]
    fn stop(&mut self) {
        self.running = false;
        self.synced = false;
        self.hello_pending = false;
        self.peers.clear();
        // The key describes a session that has ended, and its sequence numbers are spent. The next
        // session draws its own 16 bytes. The SECRET is not cleared here: it describes an agreement
        // between the game and its peer, not a session, and a game that set it once expects the next
        // join to use it.
        self.session_auth = None;
        self.session_nonce = None;
        self.auth_warned = false;
        // A held session describes a player who can come back to THIS session. There is no session to come
        // back to now, and carrying the table into the next one would resume a stranger.
        self.resume.clear();
        // A queued release names a peer id from THIS session, and the next session hands the same ids
        // to different people. The `peer_is_live` guard would refuse it anyway, so this is hygiene
        // rather than a fix — but a queue that survives its session is a queue that has to be reasoned
        // about, and there is nothing left to release.
        self.pending_seat_releases.clear();
        self.live_peers.clear();
        self.planner.clear();
        self.clock.clear();
        self.lead.clear();
        self.lead_bias_ticks = 0.0;
        self.expected_schemas.clear();
        // A manifest table and its generation describe ONE session. Carried into the next one, a
        // server would diff against rows nobody holds and a client would refuse the first delta of
        // a session it has every reason to accept.
        self.manifest_generation = 0;
        self.manifest_published.clear();
        self.manifest_break_warned = false;
        self.want_manifest = false;
        // Slots name entities within ONE session. Carrying the table into the next one would let a
        // stale slot resolve to a stranger, and a server would hand out indices it no longer owns.
        self.slots.clear();
        self.slots_dirty = true;
        self.slots_exhausted_warned = false;
        // Seats name viewpoints within ONE session, and this one is over. Dropped rather than
        // announced away: a `seat_closed` per seat is what a session DRAINING looks like, and a game
        // that just tore the session down is not seating anybody in response to it.
        self.seat_roster.clear();
        self.entity_seats.clear();
        self.seats_dirty = false;
        // No interest pass has run in the session that starts next, so every anchor read-back says
        // so. Carrying the flag would let a getter report the last session's viewpoints as current.
        self.interest_ran = false;
        // Relevancy is per session too. The mirrored set names entities of THIS session, and the
        // queued events name peer ids the next session hands to different people. Dropped rather
        // than announced away, for the reason the seat roster is: a game that tore the session down
        // is not hiding nodes in response to it.
        self.interest_mirror.clear();
        self.interest_mirror_seeded = false;
        self.interest_mirror_generation = 0;
        self.want_interest = false;
        self.interest_events.clear();
        // The path verdict describes THIS session's occupancy, and the next session's arena is not
        // this one's. It has hysteresis, so a held verdict would survive into a world it was never
        // measured on — for as many ticks as that world sat inside the selector's band. It costs
        // time and nothing else either way, and starting from the flat pass is the answer for every
        // arena that never earns the index.
        self.aoi_path = PathSelector::new();
        // A warned seat names a connection from THIS session. The next session hands the same peer
        // ids to different people, and a misconfiguration that survives the teardown is one the next
        // session is entitled to be told about.
        self.anchor_conflicts.clear();
        self.stretch_now = 1.0;
        // The window describes a session that has ended; carrying its rates into the next one would make the
        // first second of every session read as the last second of the previous.
        self.m_bw = BandwidthMetrics::default();
        self.m_peer_interarrival.clear();
        self.bw_timer = 0.0;
        self.reset_bandwidth_counters();
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().reset_session();
        }
        // The state lane holds session-scoped ticks too: a survivor with the old session's
        // latest_tick would reject every block of a NEW session (whose ticks restart near 0).
        for sync in self.state_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().reset_session();
        }
    }

    /// Whether the tick loop is running.
    #[func]
    fn is_running(&self) -> bool {
        self.running
    }

    /// Whether the initial clock sync completed (always true on the authority).
    #[func]
    fn is_synced(&self) -> bool {
        self.synced
    }

    // ------------------------------------------------------------------
    // Clock/tick queries (facade API)
    // ------------------------------------------------------------------

    /// The tick the simulation has reached. Inside a tick or rollback handler this is the tick
    /// being run, not the batch frontier — game code stamps captured state with it.
    #[func]
    fn current_tick(&self) -> i64 {
        let tick = self
            .rollback_tick_now
            .or(self.emitting_tick)
            .unwrap_or_else(|| self.accumulator.tick());
        i64::try_from(tick).unwrap_or(i64::MAX)
    }

    /// The frontier tick, ignoring any in-flight tick/rollback context.
    #[func]
    fn frontier_tick(&self) -> i64 {
        i64::try_from(self.accumulator.tick()).unwrap_or(i64::MAX)
    }

    /// The rollback tick currently being replayed, or the current tick outside the loop.
    #[func]
    fn rollback_tick(&self) -> i64 {
        let tick = self
            .rollback_tick_now
            .or(self.emitting_tick)
            .unwrap_or_else(|| self.accumulator.tick());
        i64::try_from(tick).unwrap_or(i64::MAX)
    }

    /// Seconds of network time: tick / tickrate, shared across peers.
    #[func]
    fn current_time(&self) -> f64 {
        let tick = self
            .rollback_tick_now
            .or(self.emitting_tick)
            .unwrap_or_else(|| self.accumulator.tick());
        tick as f64 * self.effective_rate().dt()
    }

    /// How far the clock sits between ticks, in `0..1` — the decoupled interpolation weight.
    #[func]
    fn tick_factor(&self) -> f64 {
        if self.sync_to_physics {
            Engine::singleton().get_physics_interpolation_fraction()
        } else {
            self.accumulator.tick_factor()
        }
    }

    /// Seconds per tick at the effective rate.
    #[func]
    fn tick_time(&self) -> f64 {
        self.effective_rate().dt()
    }

    /// The effective tick rate: the physics rate when coupled, the configured rate otherwise.
    #[func]
    fn effective_tickrate(&self) -> i64 {
        i64::from(self.effective_rate().hz())
    }

    /// The believed ceiling, clamped into `0.0..=RTT_SAMPLE_MAX_MS`. See
    /// [`Self::rtt_believed_max_ms`] for what it bounds.
    ///
    /// Clamped on set rather than on read so the property reads back the value that is in force, and
    /// so a caller that writes an absurd figure learns it by reading the property rather than by
    /// wondering why the rewind never got deeper. A NaN is refused outright and leaves the ceiling
    /// where it was: `f64::clamp` returns NaN for a NaN input, and a NaN ceiling makes every
    /// `min` against it answer the raw figure, which is the ceiling switched off by accident.
    #[func]
    fn set_rtt_believed_max_ms(&mut self, ms: f64) {
        if ms.is_nan() {
            return;
        }
        self.rtt_believed_max_ms = ms.clamp(0.0, f64::from(RTT_SAMPLE_MAX_MS));
    }

    /// One peer's round trip to this server in MILLISECONDS as this server is willing to BELIEVE it,
    /// or `-1.0` when there is no estimate yet (an unknown peer, or one that has not acknowledged a
    /// snapshot frame since it joined).
    ///
    /// SERVER-SIDE ONLY, and a different quantity from the `rtt_ms` in [`Self::metrics`]: that one
    /// is this peer's own ping sampler and reads zero on a server, because `integrate_pong` only
    /// ever runs on a client. This is what the server measured about somebody else, and it is the
    /// input to the per-shooter lag-compensation rewind — `NetLagComp` owns the policy that
    /// turns it into a rewind depth, and the millisecond ceiling that bounds THAT.
    ///
    /// **Capped at [`Self::rtt_believed_max_ms`].** The estimate is derived from acknowledgments the
    /// client chooses when to send, and the residual the ack rules cannot close is a client advancing
    /// at full rate behind a constant lag. This is the figure every rewind input reads, so bounding it
    /// here bounds that residual for every consumer at once. [`Self::peer_rtt_raw_ms`] is the
    /// unclamped figure for anything that wants the honest number instead.
    ///
    /// Derived from state the server already holds; nothing was added to the wire for it.
    #[func]
    fn peer_rtt_ms(&self, peer: i32) -> f64 {
        let Some(state) = self.peers.get(&peer) else {
            return -1.0;
        };
        match state.rtt_believed_ms(self.rtt_believed_max_ms as f32) {
            Some(ms) => f64::from(ms),
            None => -1.0,
        }
    }

    /// The same peer's round trip WITHOUT the belief ceiling: the raw minimum of the sample window,
    /// on the same `-1.0` contract as [`Self::peer_rtt_ms`].
    ///
    /// **For anything that reports a number to a human** — a scoreboard ping, an admin tool, a
    /// connection-quality readout. Those want to say what the link is doing, and a figure pinned at
    /// the ceiling would tell every player on a bad connection the same lie. Only the rewind input is
    /// bounded, and that is what [`Self::peer_rtt_ms`] is.
    ///
    /// **Do not feed this to a rewind.** It is the figure the belief ceiling exists to bound, so a
    /// consumer that reaches past `peer_rtt_ms` for it has undone the bound.
    #[func]
    fn peer_rtt_raw_ms(&self, peer: i32) -> f64 {
        let Some(state) = self.peers.get(&peer) else {
            return -1.0;
        };
        match state.rtt_ms() {
            Some(ms) => f64::from(ms),
            None => -1.0,
        }
    }

    /// Set this peer's session identity — the token its handshake carries.
    ///
    /// CLIENT-SIDE, and the whole of what a client contributes to being resumable. Set it before the join
    /// handshake goes out; a change afterward reaches the server only on the next join, which is the right
    /// moment for it anyway.
    ///
    /// The token is opaque and it is never interpreted here: the server compares it for equality against the
    /// sessions it is holding and does nothing else with it. `0` claims no identity, and a peer claiming
    /// none is always seated as a newcomer.
    ///
    /// **Set [`Self::set_resume_token`] beside it.** The identity on its own no longer resumes anything once
    /// a server has issued a token for it, so a restored identity with no restored token is seated as a
    /// newcomer — which is exactly what a peer that copied the identity off a roster presents.
    #[func]
    fn set_session_id(&mut self, id: i64) {
        self.session_id = id as u64;
    }

    /// This peer's session identity, as last set. `0` when none was.
    #[func]
    fn session_id(&self) -> i64 {
        self.session_id as i64
    }

    /// The session identity `peer` claimed in its handshake. `0` for an unknown peer and for one that
    /// claimed none.
    ///
    /// SERVER-SIDE. This is the key a game's roster should be built on, not the peer id: a peer id names the
    /// connection and is reassigned on every reconnect.
    #[func]
    fn peer_session_id(&self, peer: i32) -> i64 {
        self.peers
            .get(&peer)
            .map_or(0, |state| state.session_id as i64)
    }

    /// Whether a dropped session is currently being held open for `session_id` to reclaim.
    ///
    /// SERVER-SIDE. `false` once the window closes, once it is claimed, and for identity `0`.
    #[func]
    fn is_session_held(&self, session_id: i64) -> bool {
        self.resume.holds(session_id as u64)
    }

    /// The resume token this peer holds for its session identity. `0` when it holds none.
    ///
    /// CLIENT-SIDE. The server mints it, sends it in the welcome, and requires it back before it will hand
    /// this identity's body to anybody. **Persist it beside the session id**: a process that stored one and
    /// not the other cannot resume, because a stored identity with no token is exactly what an observer who
    /// copied the identity presents.
    #[func]
    fn resume_token(&self) -> i64 {
        self.resume_token as i64
    }

    /// Restore the resume token a previous run of this process was issued.
    ///
    /// CLIENT-SIDE, and set it before the join handshake goes out, beside [`Self::set_session_id`]. `0`
    /// quotes none, which is always seated as a newcomer once the server holds a token for that identity.
    ///
    /// **The pair is what resumes, not either half.** The token is not checked against the identity here —
    /// a mismatched pair is simply refused by the server and seated as a newcomer — so the two may be
    /// restored in either order.
    #[func]
    fn set_resume_token(&mut self, token: i64) {
        self.resume_token = token as u64;
    }

    /// Set the **shared session secret** every datagram key of this session is derived from. An empty
    /// array clears it.
    ///
    /// **Both ends must set the same one, and set it BEFORE [`Self::start`].** The client folds it into
    /// the key it seals with, the server folds it into the key it opens with, and a session where the two
    /// disagree authenticates nothing.
    ///
    /// **Source it from a channel the game already authenticated** — a lobby's metadata, a matchmaker's
    /// ticket, a session record fetched over TLS. Any length is accepted and folded to [`KEY_LEN`] bytes
    /// by [`compress_secret`], so a token, a ticket or a passphrase all work as they are.
    ///
    /// What it changes:
    ///
    /// | | No secret | A secret |
    /// | --- | --- | --- |
    /// | The handshake's 16 bytes | the session key, in the clear | a nonce, in the clear |
    /// | An on-path observer | can do everything the client can | can read the traffic and forge nothing |
    ///
    /// **THE SECRET IS A DERIVATION INPUT AND IS NEVER THE SESSION KEY.** See [`session_key_from`] for why
    /// seating it is the obvious wrong implementation and what it re-opens.
    ///
    /// Three ceilings, all unchanged by this: the tag is still 64 bits, the key still 128, and the derived
    /// key is worth exactly the entropy of the secret. **None of it encrypts anything** — every payload is
    /// still on the wire in the clear.
    ///
    /// **A misconfiguration looks the same to the player either way** — the two ends derive different keys,
    /// nothing either sends opens at the other, and the join never completes while the handshake retries.
    /// What differs is whether anything says why:
    ///
    /// - **Server with a secret, client without** is refused at the handshake, with one readable rejection
    ///   in the server's log. That is what [`Handshake::confirm`] exists for.
    /// - **Client with a secret, server without** cannot be reported at all. The server's reply is sealed
    ///   under a key the client will not derive, so the client never reads a byte of it — a rejection
    ///   included — and the server sees a hello it has no reason to refuse.
    ///
    /// [`Self::has_session_secret`] on both ends is the only thing that separates that from a dead link.
    #[func]
    fn set_session_secret(&mut self, secret: PackedByteArray) {
        let bytes = secret.as_slice();
        self.session_secret = if bytes.is_empty() {
            None
        } else {
            Some(compress_secret(bytes))
        };
    }

    /// Whether a session secret is set.
    ///
    /// **There is no getter for the bytes, deliberately.** The only questions a game has are "did my
    /// configuration take" and "am I about to join in the clear", and both are this one. Handing the
    /// material back out would put it in every debug print and crash report that walks the node.
    #[func]
    fn has_session_secret(&self) -> bool {
        self.session_secret.is_some()
    }

    /// The resume token this server issued to `peer`, or `0` for an unknown peer and one holding no
    /// identity.
    ///
    /// SERVER-SIDE, and a DIAGNOSTIC: it is the value a game prints when it wants to see why a rejoiner was
    /// or was not resumed. Nothing needs it to seat a player.
    #[func]
    fn peer_resume_token(&self, peer: i32) -> i64 {
        self.peers
            .get(&peer)
            .map_or(0, |state| state.resume_token as i64)
    }

    /// Which claims on an identity this server grants. Clamped to a known value; see
    /// [`Self::resume_policy`] for why the default stays ALWAYS.
    #[func]
    fn set_resume_policy(&mut self, policy: i64) {
        self.resume_policy = clamp_resume_policy(policy);
    }

    /// Declare where one peer observes from, and which world it observes in.
    ///
    /// SERVER-SIDE ONLY, and the answer to a question the backend cannot infer. Undeclared, a peer
    /// is centered on — and put in the world of — the lowest-id entity its input drives, which
    /// answers what that peer CONTROLS when interest management asks what it OBSERVES. Use this for
    /// a spectator, a strategic camera, an observation post, or any peer whose view is not bolted to
    /// a body it drives. [`Self::set_peer_anchor_entity`] is the same declaration for a center that
    /// moves with an entity.
    ///
    /// `membership` is the same id `membership_property` names on an entity, with the same rule:
    /// `0` is `MEMBERSHIP_GLOBAL` and matches every world. Declaring it here is what makes a peer's
    /// world a **fact rather than a pick** — the inferred path reads it off whichever of a peer's
    /// bodies sorts lowest by FNV hash, and a peer driving two bodies that declare different worlds
    /// has no defined world without this call.
    ///
    /// It rides the anchor call rather than standing alone because the two are one statement: "this
    /// peer is at this point, in this world". [`Self::clear_peer_anchor`] retracts both together.
    ///
    /// May be called before the peer completes its handshake; the declaration is held until it does.
    #[func]
    fn set_peer_anchor(&mut self, peer: i32, position: Vector3, membership: i64) {
        let state = self.peers.entry(peer).or_default();
        state.anchor = PeerAnchor::Fixed([position.x, position.y, position.z]);
        state.anchor_last = None;
        state.anchor_membership = membership as MembershipId;
    }

    /// Declare that one peer observes from an ENTITY, and which world it observes in.
    ///
    /// `entity_id` is the token `get_entity_id()` returns on either synchronizer — reached from
    /// GDScript through the rollback or state handle, never computed. `0` retracts, exactly as
    /// [`Self::clear_peer_anchor`] does, since `0` is what an unresolved synchronizer reports and
    /// centring a peer on "no entity" is not a state worth having.
    ///
    /// The same statement as [`Self::set_peer_anchor`], differing in what it costs the caller: a
    /// tracked center follows the entity with no per-tick call. The entity NEED NOT be one the peer
    /// drives, and that is the point.
    ///
    /// **When the tracked entity stops resolving — it despawns, or it has no state row yet — the
    /// peer keeps the last position it did resolve to, and stays in the world it was declared into.**
    /// A membership is a declaration and did not fail; see [`resolve_observer`]. A declaration made
    /// before the entity exists simply starts resolving on the tick it does.
    #[func]
    fn set_peer_anchor_entity(&mut self, peer: i32, entity_id: i64, membership: i64) {
        let state = self.peers.entry(peer).or_default();
        state.anchor = if entity_id == 0 {
            PeerAnchor::Inferred
        } else {
            PeerAnchor::Entity(entity_id as u64)
        };
        state.anchor_last = None;
        state.anchor_membership = membership as MembershipId;
    }

    /// Retract a peer's anchor declaration AND its world, together.
    ///
    /// The peer returns to the inferred pair: centered on the lowest-id body its input drives, in
    /// that body's world. Retracting one axis without the other would leave a peer declared into a
    /// world with no declared position in it, or positioned in a world it is no longer in — and the
    /// inferred path exists precisely to keep those two answers about one entity.
    #[func]
    fn clear_peer_anchor(&mut self, peer: i32) {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.anchor = PeerAnchor::Inferred;
            state.anchor_last = None;
            state.anchor_membership = MEMBERSHIP_GLOBAL;
        }
    }

    /// The world DECLARED for one peer, or `0` when nothing was declared for it.
    ///
    /// **Not the world an undeclared peer is filtered in.** That one is read off the body the peer
    /// drives and is reported by `NetRollbackHandle.membership()`, which is where a misconfigured
    /// `membership_property` shows. `0` here means "no declaration", which is also `MEMBERSHIP_GLOBAL`
    /// — the two have the same consequence for a peer that declared nothing, so they are not
    /// distinguished.
    #[func]
    fn peer_membership(&self, peer: i32) -> i64 {
        self.peers
            .get(&peer)
            .map_or(MEMBERSHIP_GLOBAL, |state| match state.anchor {
                PeerAnchor::Inferred => MEMBERSHIP_GLOBAL,
                _ => state.anchor_membership,
            }) as i64
    }

    /// The interest anchor ACTUALLY IN EFFECT for one connection, as the last interest pass resolved
    /// it.
    ///
    /// **The read-back [`Self::peer_membership`] is not.** That one reports the DECLARATION, so it
    /// answers `0` for every peer that declared nothing — which is most of them, and which is
    /// indistinguishable from a peer declared into every world. What the filter actually ran is
    /// computed inside [`Self::update_interest`] and was, until this call existed, thrown away with
    /// the scratch vector it was built in.
    ///
    /// Keys:
    ///
    /// | Key | Type | Meaning |
    /// | --- | --- | --- |
    /// | `source` | `int` | one of the `ANCHOR_SOURCE_*` values: 0 none, 1 inferred, 2 fixed position, 3 tracked entity |
    /// | `viewpoints` | `int` | how many observers the filter ran — one per resolved seat, `1` for a declared or failed-open connection, `0` for a CLOSED one |
    /// | `membership` | `int` | the world in effect, NOT the declared one |
    /// | `located` | `bool` | false when the center is [`UNLOCATABLE_CENTER`], so nothing is culled by distance |
    /// | `center` | `Vector3` | the center, or `ZERO` when `located` is false |
    /// | `open` | `bool` | this connection culls nothing by distance — some viewpoint of it is unlocatable |
    /// | `ambiguous` | `bool` | some seat drove several anchored bodies, so its center is one arbitrary pick among them |
    /// | `stale` | `bool` | **the interest pass has not run**; read nothing else |
    ///
    /// **`stale` IS THE GATE AND IT IS NOT AN EDGE CASE.** The pass is skipped entirely whenever
    /// nothing can be culled — no `aoi_radius` and no entity declaring a membership, which is a
    /// session replicating everything to everybody — and it never runs on a client at all. Without
    /// `stale` this call would answer "centered at the origin, in world 0, located" for every peer in
    /// those sessions, which is a description of a filter that is not running.
    ///
    /// **`center`, `located` and `membership` describe the FIRST viewpoint**, which is the whole
    /// connection whenever `viewpoints` is 1. A split-screen connection has one per seat and they
    /// differ; ask [`Self::seat_anchor_info`] per seat there. `open` and `ambiguous` are already
    /// facts about the whole connection.
    #[func]
    fn peer_anchor_info(&self, peer: i32) -> VarDictionary {
        let Some(report) = self
            .peers
            .get(&peer)
            .map(|state| &state.anchor_report)
            .filter(|report| report.resolved && self.interest_ran)
        else {
            return Self::no_anchor_info();
        };
        let first = report.observers.first();
        let located = first.is_some_and(|o| is_located(o.center));
        let center = match first {
            Some(o) if located => Vector3::new(o.center[0], o.center[1], o.center[2]),
            _ => Vector3::ZERO,
        };
        vdict! {
            "source" => report.source,
            "viewpoints" => report.observers.len() as i64,
            "membership" => first.map_or(0i64, |o| o.membership as i64),
            "located" => located,
            "center" => center,
            // A connection culls nothing by distance as soon as ANY of its viewpoints is
            // unlocatable: its interest is the union of its seats', and an unlocatable seat admits
            // everything its world allows. A connection with no viewpoint at all is the opposite
            // claim and reads `false` here.
            "open" => report.observers.iter().any(|o| !is_located(o.center)),
            "ambiguous" => report.ambiguous,
            "stale" => false,
        }
    }

    /// The same answer for ONE seat on a connection, for the split-screen case.
    ///
    /// Keys: `center` (`Vector3`, `ZERO` when unlocated), `located` (`bool`), `membership` (`int`).
    ///
    /// A **declared** connection answers its one collapsed viewpoint for every seat label, including
    /// labels no body currently declares — a declaration states where the CONNECTION observes from,
    /// and the backend does not re-split it. An inferred connection answers only for the seats that
    /// resolved a center; a seat whose body has not spawned has no viewpoint of its own and reads
    /// zeroed, which is exactly what the filter does with it.
    #[func]
    fn seat_anchor_info(&self, peer: i32, seat: i32) -> VarDictionary {
        let found = SeatIndex::try_from(seat).ok().and_then(|label| {
            let report = self
                .peers
                .get(&peer)
                .map(|state| &state.anchor_report)
                .filter(|report| report.resolved && self.interest_ran)?;
            let index = report
                .labels
                .iter()
                .position(|held| held.is_none() || *held == Some(label))?;
            report.observers.get(index)
        });
        let Some(observer) = found else {
            return Self::no_seat_anchor_info();
        };
        let located = is_located(observer.center);
        let center = if located {
            Vector3::new(observer.center[0], observer.center[1], observer.center[2])
        } else {
            Vector3::ZERO
        };
        vdict! {
            "center" => center,
            "located" => located,
            "membership" => observer.membership as i64,
        }
    }

    /// The fully keyed "no answer" dictionary [`Self::peer_anchor_info`] returns for a peer that
    /// names no connection, and for a session whose interest pass has not run.
    ///
    /// One definition, because every key must be present on every path: a caller that indexes the
    /// dictionary directly gets a value rather than a `nil` it then has to type-check, and the
    /// facade mirrors this exact shape for its OFFLINE answer.
    #[must_use]
    fn no_anchor_info() -> VarDictionary {
        vdict! {
            "source" => ANCHOR_SOURCE_NONE,
            "viewpoints" => 0i64,
            "membership" => 0i64,
            "located" => false,
            "center" => Vector3::ZERO,
            "open" => false,
            "ambiguous" => false,
            "stale" => true,
        }
    }

    /// [`Self::no_anchor_info`] for [`Self::seat_anchor_info`]'s three keys.
    #[must_use]
    fn no_seat_anchor_info() -> VarDictionary {
        vdict! {
            "center" => Vector3::ZERO,
            "located" => false,
            "membership" => 0i64,
        }
    }

    /// What a connection that resolved NO interest anchor receives. Session-wide default; `0` is
    /// OPEN and stays the default.
    ///
    /// **OPEN (0)** — today's behavior. Such a connection is handed [`UNLOCATABLE_CENTER`] and one
    /// observer in [`MEMBERSHIP_GLOBAL`], which makes every candidate uncullable, and an uncullable
    /// candidate is kept by `apply_cap` regardless of `aoi_max_entities`. So the connection receives
    /// every non-vetoed entity in every world, with the nearest-N cap not bounding it and the
    /// per-datagram send budget as the only remaining brake.
    ///
    /// **CLOSED (1)** — such a connection is given no viewpoint at all, and an empty viewpoint set
    /// makes nothing relevant. It receives nothing.
    ///
    /// **THE CARVE-OUT IS THE WHOLE DESIGN.** CLOSED applies ONLY to a connection that declared
    /// nothing AND drives no rollback row at all. A connection whose seats exist but have not
    /// RESOLVED a center yet — a player whose avatar is still spawning — keeps the connection-wide
    /// fail-open, and that is deliberate: closing it would deny a player its own avatar for as many
    /// ticks as the body takes to spawn, which is the failure fail-open exists to prevent.
    /// [`seat_observers_into`] states the conjunction as a table.
    ///
    /// **The default does not move.** The cdylib is refreshed only at a release tag, so the same
    /// project source runs against older and newer binaries; a CLOSED default would mean a game's
    /// spectators see the world or do not, depending on which binary is on disk. Choose it in one
    /// call, in a session whose spectators are supposed to declare an anchor.
    ///
    /// An unknown value clamps to OPEN. See [`clamp_unanchored_policy`].
    #[func]
    fn set_unanchored_policy(&mut self, policy: i64) {
        self.unanchored_policy = clamp_unanchored_policy(policy);
    }

    /// The session default in force, always `0` or `1`. Per-connection overrides are not folded in
    /// here — this is the value a connection nobody declared a policy for follows.
    #[func]
    fn unanchored_policy(&self) -> i64 {
        self.unanchored_policy
    }

    /// The same policy for ONE connection, overriding the session default outright.
    ///
    /// For the mixed session the session-wide value cannot express: a game whose spectators declare
    /// an anchor and whose late joiners do not can close the first without closing the second. The
    /// carve-out on [`Self::set_unanchored_policy`] applies here unchanged — a connection with an
    /// unresolved seat still fails open whatever this says.
    ///
    /// **Dropped with the connection.** It is held on [`PeerState`], which `_on_peer_disconnected`
    /// removes, so a reused peer id follows the session default again rather than inheriting a policy
    /// nobody set for it. May be called before the peer completes its handshake, exactly as
    /// [`Self::set_peer_anchor`] may.
    #[func]
    fn set_peer_unanchored_policy(&mut self, peer: i32, policy: i64) {
        let state = self.peers.entry(peer).or_default();
        state.unanchored_closed = Some(clamp_unanchored_policy(policy) == UNANCHORED_CLOSED);
    }

    /// Which seats one connection currently holds, ascending by label. Empty for a connection that
    /// drives nothing.
    ///
    /// Answered from the announced roster, so it agrees with the last
    /// [`Self::seat_opened`]/[`Self::seat_closed`] pair rather than with whatever the registry looks
    /// like part-way through a frame. A client answers from the manifest it last received; a server
    /// from its own registry.
    #[func]
    fn seats_of(&self, peer: i32) -> PackedInt32Array {
        self.seat_roster
            .seats_of(peer)
            .iter()
            .map(|id| i32::from(id.seat))
            .collect()
    }

    /// Every entity id driven by `(peer, seat)`. Empty when the seat holds none.
    ///
    /// **What makes a seat event actionable.** `seat_opened` names a viewpoint; a presentation layer
    /// binding a camera or a split-screen viewport to it needs the body, and a seat may drive several.
    /// The ids are the opaque tokens `get_entity_id()` answers — routinely negative, meaningless to
    /// compare or order, only ever passed back unmodified. The order here is the backend's own and is
    /// stable within a session; it is not a ranking.
    #[func]
    fn seat_entities(&self, peer: i32, seat: i64) -> PackedInt64Array {
        let Ok(label) = SeatIndex::try_from(seat) else {
            return PackedInt64Array::new();
        };
        self.entity_seats
            .iter()
            .filter(|&&(_, owner, held)| owner == peer && held == label)
            .map(|&(id, _, _)| id as i64)
            .collect()
    }

    /// Choose the seat-release policy, clamped to a known value. See [`Self::seat_release_policy`]
    /// for what each one does and for why the default is `0`.
    ///
    /// Clamped on set rather than on read so the property reads back the policy that is **in force**,
    /// and so a caller that writes a number this build does not know learns it by reading the
    /// property back rather than by wondering why nothing was ever released. An unknown value falls
    /// onto `HOLD`, which is the direction that is safe to be wrong in: it takes nobody's body away.
    #[func]
    fn set_seat_release_policy(&mut self, policy: i64) {
        self.seat_release_policy = clamp_seat_release_policy(policy);
    }

    /// Hand every body `peer` drives back to the server, closing its seats. Answers how many entities
    /// changed.
    ///
    /// **Available under every policy, including the default.** The policy decides whether this node
    /// calls it *by itself* on a drop or an expiry; this is the same work as one call, for a game that
    /// wants to decide case by case — a kick, an admin command, a match ending, a player who forfeits.
    ///
    /// SERVER-SIDE: `0` off the authority, and `0` for peer ids that name no connection (`0`, negative,
    /// and [`SERVER_PEER`] itself — handing the server's own bodies back to the server is what an
    /// unclaimed body already looks like).
    ///
    /// **It releases the seat and nothing else.** The body stays registered, stays replicated and stays
    /// in the scene; what leaves is the viewpoint. Freeing the node is the game's decision, exactly as
    /// it is for a session whose grace window expired.
    #[func]
    fn release_peer_seats(&mut self, peer: i32) -> i64 {
        self.release_owned_bodies(peer, None)
    }

    /// The same release, narrowed to one seat label on that connection. Answers how many entities
    /// changed.
    ///
    /// For a connection holding several seats — local split-screen — where only one of them is going
    /// away. A `seat` outside the label range answers `0` rather than releasing everything, because the
    /// caller asked for a seat that cannot exist.
    #[func]
    fn release_seat_of(&mut self, peer: i32, seat: i64) -> i64 {
        let Ok(label) = SeatIndex::try_from(seat) else {
            return 0;
        };
        self.release_owned_bodies(peer, Some(label))
    }

    /// Withhold one entity from one peer, or stop withholding it.
    ///
    /// SERVER-SIDE ONLY, and the third interest axis. Distance and membership are both properties of
    /// the *entity* — one position, one world, read the same by every peer — so neither can express
    /// "not this peer". This can, including the exception a membership id cannot: a class of entities
    /// scoped by a declared key, minus one.
    ///
    /// `entity_id` is the token `get_entity_id()` returns on either synchronizer, and `0` is ignored
    /// — that is what an unresolved synchronizer reports, and vetoing "no entity" is not a state
    /// worth holding. May be called before the peer completes its handshake; the veto is held until
    /// it does.
    ///
    /// **The veto beats everything the filter would otherwise say**, `always` included, and it
    /// refuses at the candidate rather than at the cap, so a withheld entity occupies no slot in
    /// `aoi_max_entities`. It withholds from the whole CONNECTION — a datagram is shared by every
    /// seat on it — see [`PeerState::set_entity_hidden`] for the delta bookkeeping it clears and
    /// [`ConnectionInterest::set_hidden`] for the filter rule itself.
    ///
    /// **IT NEEDS NO OTHER AXIS CONFIGURED.** A standing veto turns the interest pass on by itself,
    /// so a session with no radius and no declared membership — the one where a per-(peer, entity)
    /// refusal is the only lever a game has — gets the behaviour this method describes. It used to
    /// be inert there: the veto is enforced inside the filter, the filter ran only when a radius or
    /// a membership had already asked for it, and the two read-backs then disagreed, with
    /// `is_entity_hidden` answering `true` while `is_entity_in_interest` also answered `true`.
    ///
    /// **THE CLIENT-SIDE CONTRACT, STATED PLAINLY: a veto stops the rows and nothing else.** No
    /// despawn is sent, the receiving client's node is not removed, and the entity manifest still
    /// names the id — ids are session-global whatever any one peer receives. What the client sees is
    /// `get_last_known_state()` ceasing to advance, exactly as it does for a distance cull, and what
    /// to do about an entity that stopped updating is the consuming project's decision.
    #[func]
    fn set_entity_hidden(&mut self, peer: i32, entity_id: i64, hidden: bool) {
        if entity_id == 0 {
            return;
        }
        let left = self
            .peers
            .entry(peer)
            .or_default()
            .set_entity_hidden(entity_id as u64, hidden);
        // THE SIGNAL THE WIRE LEAVE ALREADY CARRIED. A veto drops the id from the set in that same
        // call, so no later `leaves` diff can ever name it — which is exactly why the rule stated
        // above `update_interest` is that each between-updates leave queues its own event. Without
        // this the server saw no `entity_left_interest` for a veto while a retraction still produced
        // an `entity_entered_interest`, so a handler mirroring the two recorded an unpaired enter.
        if left {
            self.interest_events.push((peer, entity_id as u64, false));
        }
    }

    /// Whether `entity_id` is currently in `peer`'s interest.
    ///
    /// **A session that culls nothing answers `true` for every registered entity.** The interest pass
    /// does not run at all when there is no radius and no declared membership, so `peer.interest` is
    /// an empty structure describing a tick that never happened — reading it there would answer
    /// `false` for a session replicating everything to everybody. [`Self::interest_ran`] is what
    /// separates the two, and it is the same flag the anchor read-backs are gated on.
    ///
    /// A CLIENT answers from the mirrored set the interest-delta sections built, and ignores `peer`:
    /// a client holds exactly one interest set, its own. Until it has received a section it answers
    /// `true` for everything it holds, because a server that culls nothing sends none.
    #[func]
    fn is_entity_in_interest(&self, peer: i32, entity_id: i64) -> bool {
        let id = entity_id as u64;
        if self.mode == MODE_CLIENT {
            return if self.interest_mirror_seeded {
                self.interest_mirror.contains(&id)
            } else {
                self.is_registered(id)
            };
        }
        if !self.interest_ran {
            return self.is_registered(id);
        }
        self.peers
            .get(&peer)
            .is_some_and(|state| state.interest.contains(id))
    }

    /// Every entity in `peer`'s interest, ascending by id. Empty for a connection that holds none.
    ///
    /// **What gives an edge a starting point.** A relevancy signal is a transition, so a handler
    /// bound mid-session, or a node spawned after the fact, has nothing to resync from and would wait
    /// for the next churn. This is the standing answer, and it follows the same "culling off means
    /// everything" rule [`Self::is_entity_in_interest`] states.
    #[func]
    fn entities_in_interest(&self, peer: i32) -> PackedInt64Array {
        if self.mode == MODE_CLIENT {
            if !self.interest_mirror_seeded {
                return self.registered_ids();
            }
            let mut ids: Vec<u64> = self.interest_mirror.iter().copied().collect();
            // A `HashSet` walk is not an order, and the server answers in ascending id order.
            ids.sort_unstable();
            return ids.iter().map(|&id| id as i64).collect();
        }
        if !self.interest_ran {
            return self.registered_ids();
        }
        self.peers
            .get(&peer)
            .map(|state| state.interest.iter().map(|id| id as i64).collect())
            .unwrap_or_default()
    }

    /// Whether either registry names `id` — the "everything is in interest" answer's one check, so a
    /// token that names nothing still reads as `false`.
    fn is_registered(&self, id: u64) -> bool {
        self.rollback_entities.contains_key(&id) || self.state_entities.contains_key(&id)
    }

    /// Every registered entity id, ascending. What "everything is in interest" resolves to.
    fn registered_ids(&self) -> PackedInt64Array {
        let mut ids: Vec<u64> = self
            .rollback_entities
            .keys()
            .chain(self.state_entities.keys())
            .copied()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.iter().map(|&id| id as i64).collect()
    }

    /// Whether `entity_id` is currently withheld from `peer`. `false` for an unknown peer, and for
    /// entity id `0`, which [`Self::set_entity_hidden`] refuses to record.
    #[func]
    fn is_entity_hidden(&self, peer: i32, entity_id: i64) -> bool {
        self.peers
            .get(&peer)
            .is_some_and(|state| state.interest.is_hidden(entity_id as u64))
    }

    /// Remote-resim lever: when true, un-exempt display-only entities so this client predicts
    /// remote bodies forward from their latest authoritative state.
    #[func]
    fn set_remote_resim(&mut self, on: bool) {
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            if !bound.owns_state() && !bound.owns_input() {
                bound.set_display_exempt(!on);
            }
        }
    }

    /// Diagnostic counters, keyed to match what `Net.perf_metrics()`/`clock_metrics()` return.
    #[func]
    fn metrics(&self) -> VarDictionary {
        vdict! {
            "tick" => self.frontier_tick(),
            "running" => self.running,
            "resim_ticks" => self.m_resim_ticks,
            "rollback_ms" => self.m_rollback_ms,
            "restore_ms" => self.m_restore_ms,
            "sim_ms" => self.m_sim_ms,
            "record_ms" => self.m_record_ms,
            "net_ms" => self.m_net_ms,
            "rb_nodes" => self.m_rb_nodes,
            "stretch" => self.stretch_now,
            "offset_ms" => self.clock.offset() * 1000.0,
            "rtt_ms" => self.clock.rtt() * 1000.0,
            "jitter_ms" => self.clock.jitter() * 1000.0,
            "lead_ticks" => self.lead_bias_ticks,
        }
    }

    /// The POOLED mean ticks between admissions across every band, without building
    /// [`Self::bandwidth_metrics`]'s dictionary.
    ///
    /// A scalar rather than a dictionary key because it is read at tick rates: going through
    /// [`Self::bandwidth_metrics`] to get it allocated a nineteen-key `VarDictionary` and boxed every
    /// value, per tick, forever — on the hot path of the loop this epic exists to make cheaper.
    /// Everything else in that dictionary is read by a probe or a HUD at human rates and can keep
    /// paying for it.
    ///
    /// This is the figure for a consumer that cannot name a peer. The interpolation term in a shot's
    /// rewind depth can name one, and reads [`Self::interarrival_ticks`] instead.
    #[func]
    fn interarrival_all(&self) -> f64 {
        self.m_bw.interarrival_all
    }

    /// Mean ticks between admissions for the rows in one distance band, as three scalars.
    ///
    /// Scalars rather than [`Self::bandwidth_metrics`] keys for the reason
    /// [`Self::interarrival_all`] is one: the lag-compensation rewind reads all three every net
    /// tick on the authority, to derive a rewind depth per TARGET rather than one depth per shot.
    /// A row's band is its distance from the peer's interest anchor, so a contested target and a
    /// body across the map are not the same age, and a rewind that applies the pooled figure to
    /// both errs long on the near one and short on the far one.
    ///
    /// Each answers 0.0 before the first window is published and for a band that admitted nothing
    /// — including every band but `near` in a session with no `aoi_band_radius` configured, where
    /// `priority::band_of` reports [`priority::Band::Near`] for every row. The caller's rule for
    /// 0.0 is unchanged: leave the fallback in place rather than invent a number.
    #[func]
    fn interarrival_near(&self) -> f64 {
        self.m_bw.interarrival_near
    }

    #[func]
    fn interarrival_mid(&self) -> f64 {
        self.m_bw.interarrival_mid
    }

    #[func]
    fn interarrival_far(&self) -> f64 {
        self.m_bw.interarrival_far
    }

    /// Mean ticks between admissions for the rows sent to one peer, pooled across every band.
    ///
    /// The per-peer form of [`Self::interarrival_all`], and the one the lag-compensation rewind
    /// wants. That rewind's interpolation term is the shooter's own view lag, and the round-trip
    /// term beside it ([`Self::peer_rtt_ms`]) is already per peer; a pooled interpolation term grants
    /// a peer served every tick a window measured partly from peers served every eighth.
    ///
    /// Answers 0.0 for an unknown peer, for a peer whose window admitted nothing, and before the
    /// first window has been published. That is the same "no measurement" answer
    /// [`Self::interarrival_all`] gives, and the caller's rule for it is unchanged: leave the
    /// fallback in place rather than invent a number.
    #[func]
    fn interarrival_ticks(&self, peer: i32) -> f64 {
        self.m_peer_interarrival.get(&peer).copied().unwrap_or(0.0)
    }

    /// Send-path accounting, windowed to per-second figures once a second.
    ///
    /// Deliberately a **separate** dictionary from [`Self::metrics`]: `bench_metrics.gd` and the
    /// perf probe read that one's exact shape, and widening a dictionary two harnesses index into
    /// is how a measurement change becomes a gate failure. Byte figures are OrbitNet **payload**;
    /// `tx_wire_bytes_s` is the same traffic with [`WIRE_OVERHEAD_BYTES`] per datagram added, and
    /// `tx_datagrams_s` is published so the sum can be checked rather than trusted.
    #[func]
    fn bandwidth_metrics(&self) -> VarDictionary {
        let bw = &self.m_bw;
        vdict! {
            "tx_bytes_s" => bw.tx_bytes_s,
            "tx_datagrams_s" => bw.tx_datagrams_s,
            "tx_wire_bytes_s" => bw.tx_wire_bytes_s,
            "tx_peak_peer_bytes_s" => bw.tx_peak_peer_bytes_s,
            "rx_bytes_s" => bw.rx_bytes_s,
            "rx_datagrams_s" => bw.rx_datagrams_s,
            "blocks_admitted_s" => bw.blocks_admitted_s,
            "blocks_deferred_s" => bw.blocks_deferred_s,
            "blocks_culled_s" => bw.blocks_culled_s,
            "blocks_oversize_s" => bw.blocks_oversize_s,
            "blocks_full_s" => bw.blocks_full_s,
            "want_full_nacks_s" => bw.want_full_nacks_s,
            "unproven_acks_s" => bw.unproven_acks_s,
            "stale_blocks_s" => bw.stale_blocks_s,
            "starve_ticks_max" => bw.starve_ticks_max,
            "unsent_backlog_max" => bw.unsent_backlog_max,
            "interest_ms" => bw.interest_ms,
            "interest_grid" => bw.interest_grid,
            "interarrival_near" => bw.interarrival_near,
            "interarrival_mid" => bw.interarrival_mid,
            "interarrival_far" => bw.interarrival_far,
            "interarrival_all" => bw.interarrival_all,
            "peers" => bw.peers,
            "interest_entities" => bw.interest_entities,
            "rtt_at_ceiling_peers" => bw.rtt_at_ceiling_peers,
        }
    }

    /// The wire protocol version this build speaks.
    #[func]
    fn protocol_version(&self) -> i64 {
        i64::from(orbitnet_core::PROTOCOL_VERSION)
    }

    /// The protocol version rendered as `major.minor.patch`.
    #[func]
    fn protocol_version_string(&self) -> GString {
        GString::from(
            orbitnet_core::codec::version_string(orbitnet_core::PROTOCOL_VERSION).as_str(),
        )
    }

    /// Install the native crash handler, appending reports to `<dir>/crash-native.log`.
    ///
    /// Lives on this node purely because the extension is the only first-party binary loaded by a
    /// RELEASE export template — Godot's own crash handler is `DEBUG_ENABLED`-only, so a shipped
    /// build otherwise dies with no backtrace and no `NOTIFICATION_CRASH`. `dir` is resolved and
    /// created by the caller, which already owns its own log directory, so nothing in the signal
    /// path has to touch Godot. Idempotent; returns false if already installed.
    #[func]
    fn install_crash_handler(&self, dir: GString) -> bool {
        crate::crash::install(&dir.to_string())
    }

    /// Where a Windows fail-fast would leave a dump, read back from Windows Error Reporting.
    ///
    /// [`Self::install_crash_handler`]'s Windows filter never runs for `__fastfail` — what the CRT
    /// raises on detected heap corruption — because a fail-fast bypasses every in-process handler by
    /// design. WER's out-of-process `LocalDumps` collector is the only thing that sees it, and
    /// OrbitNet never writes those keys: they are HKLM-only, need administrator privileges, and set
    /// policy for every application on the machine. So this READS what the machine is already
    /// configured to do, and a caller's crash report can name the folder or say nothing collects.
    ///
    /// Keys: `supported` (false off Windows), `configured`, `scope` (`none`/`global`/`image`),
    /// `folder`, `dump_type`, `dump_count`, `image`. See `docs/crash-capture.md`.
    #[func]
    fn crash_dump_config(&self) -> VarDictionary {
        let dumps = crate::crash::local_dumps();
        vdict! {
            "supported" => dumps.supported,
            "configured" => dumps.configured,
            "scope" => &GString::from(dumps.scope),
            "folder" => &GString::from(dumps.folder.as_str()),
            "dump_type" => dumps.dump_type,
            "dump_count" => dumps.dump_count,
            "image" => &GString::from(dumps.image.as_str()),
        }
    }

    /// The protocol major version. Peers must agree on this exactly.
    #[func]
    fn protocol_major(&self) -> i64 {
        i64::from(orbitnet_core::protocol::protocol_major(
            orbitnet_core::PROTOCOL_VERSION,
        ))
    }

    // ------------------------------------------------------------------
    // Multiplayer plumbing
    // ------------------------------------------------------------------

    #[func]
    fn _on_peer_packet(&mut self, id: i64, packet: PackedByteArray) {
        let bytes = packet.to_vec();
        self.acc_rx_bytes += bytes.len() as u64;
        self.acc_rx_datagrams += 1;
        if self.debug_wire {
            let slot = bytes.first().map(|&b| (b as usize).min(7)).unwrap_or(7);
            self.dbg_rx_kinds[slot] += 1;
        }
        self.handle_packet(id as i32, &bytes);
    }

    #[func]
    fn _on_peer_connected(&mut self, id: i64) {
        // Peer tracking starts at the handshake; connection alone means nothing yet.
        let _ = id;
    }

    /// Drop the peer's entry, and hold its session open if it had one.
    ///
    /// The peer entry itself is never retained. Everything on it describes a CONNECTION — what that socket
    /// was last sent, which rows it acked, how long its round trip was — and none of it is true of the new
    /// socket a rejoiner arrives on. What survives is the identity, in [`Self::resume`].
    ///
    /// **Under `RELEASE_ON_DROP` this QUEUES the seat release rather than performing it.** This callback is
    /// delivered by the transport, and `SceneMultiplayer` delivers it from inside `poll()` — which the tick
    /// loop calls with a `bind` held on the synchronizer it is stepping. A release needs `bind_mut()` on
    /// every entity it touches, and that is a borrow panic, not a wrong answer. See
    /// [`Self::pending_seat_releases`].
    #[func]
    fn _on_peer_disconnected(&mut self, id: i64) {
        let peer = id as i32;
        // The token goes into the held record with the identity. It is what the departed client is already
        // holding, so re-minting one here would reach nobody — the connection that would have carried a new
        // one in a welcome is exactly the connection that just went away.
        let (session_id, resume_token) = self
            .peers
            .remove(&peer)
            .map_or((0, 0), |state| (state.session_id, state.resume_token));
        let server = self.mode == MODE_SERVER || self.mode == MODE_HOST;
        let grace_ms = (self.reconnect_grace.max(0.0) * 1000.0) as u64;
        let held = hold_on_drop(session_id, grace_ms, server)
            && self.resume.hold(
                session_id,
                peer,
                Self::now_ms().saturating_add(grace_ms),
                resume_token,
            );
        if server {
            // Queued only under a policy that acts on drops at all, so the default allocates nothing and
            // a session whose loop is not running cannot grow a queue nobody drains. `false` for
            // liveness here asks "does this policy release on a drop"; the real liveness question is
            // re-asked at the drain, because the id may name a different connection by then.
            let policy = seat_release_policy_of(self.seat_release_policy);
            if releases_seats(policy, SeatReleaseEvent::Dropped, false) {
                queue_seat_release(&mut self.pending_seat_releases, peer);
            }
            self.signals()
                .peer_dropped()
                .emit(id, session_id as i64, held);
        }
    }

    #[func]
    fn _on_connected_to_server(&mut self) {
        if self.mode == MODE_CLIENT && !self.synced {
            self.send_hello();
        }
    }

    #[func]
    fn _on_server_disconnected(&mut self) {
        if self.mode == MODE_CLIENT {
            self.stop();
        }
    }

    fn connect_multiplayer_signals(&mut self) {
        if self.signals_connected {
            return;
        }
        let Some(mut api) = self.base().get_multiplayer() else {
            return;
        };
        let this = self.to_gd();
        api.connect("peer_connected", &this.callable("_on_peer_connected"));
        api.connect("peer_disconnected", &this.callable("_on_peer_disconnected"));
        api.connect(
            "connected_to_server",
            &this.callable("_on_connected_to_server"),
        );
        api.connect(
            "server_disconnected",
            &this.callable("_on_server_disconnected"),
        );
        if let Ok(mut scene) = api.try_cast::<SceneMultiplayer>() {
            scene.connect("peer_packet", &this.callable("_on_peer_packet"));
        }
        self.signals_connected = true;
    }

    fn scene_multiplayer(&self) -> Option<Gd<SceneMultiplayer>> {
        let api: Gd<MultiplayerApi> = self.base().get_multiplayer()?;
        api.try_cast::<SceneMultiplayer>().ok()
    }

    fn has_live_peer(&self) -> bool {
        self.base()
            .get_multiplayer()
            .map(|m| m.has_multiplayer_peer())
            .unwrap_or(false)
    }

    /// Authenticate one datagram and hand it to the transport.
    ///
    /// **Everything but the handshake goes through here, and a datagram this cannot authenticate is
    /// not sent.** The fail-safe direction is deliberate: a frame added later is sealed by default,
    /// and only [`OrbitNet::send_hello`] — which is what carries the key — opts out by calling
    /// [`OrbitNet::send_raw`].
    ///
    /// The sealed datagram is [`TRAILER_LEN`] bytes longer than the payload. That rides above
    /// `MAX_FRAME_PAYLOAD` the same way the frame header does.
    fn send_to(&mut self, peer: i32, bytes: &[u8], mode: TransferMode) {
        let Some((direction, _)) = session_directions(self.mode) else {
            return;
        };
        let mut sealed = Vec::with_capacity(bytes.len() + TRAILER_LEN);
        sealed.extend_from_slice(bytes);
        let auth = match direction {
            Direction::ToServer => self.session_auth.as_mut(),
            Direction::ToClient => self
                .peers
                .get_mut(&peer)
                .and_then(|state| state.auth.as_mut()),
        };
        // No key means no session — a server peer that has not handshaken, or a client that has not
        // started. Nothing that reaches here is worth sending in the clear.
        let Some(auth) = auth else {
            return;
        };
        if auth.seal(direction, &mut sealed).is_none() {
            return;
        }
        self.send_raw(peer, &sealed, mode);
    }

    /// Hand one datagram to the transport unauthenticated, and account for it.
    ///
    /// Every byte OrbitNet puts on the wire goes through here — snapshots, input frames, manifests,
    /// pings and the handshake alike — which is what makes `tx_bytes_s` a number about the session
    /// rather than about the snapshot loop. Only the handshake calls it directly; see
    /// [`OrbitNet::send_to`].
    fn send_raw(&mut self, peer: i32, bytes: &[u8], mode: TransferMode) {
        let Some(mut scene) = self.scene_multiplayer() else {
            return;
        };
        let data = PackedByteArray::from(bytes);
        scene
            .send_bytes_ex(&data)
            .id(peer)
            .mode(mode)
            .channel(0)
            .done();
        let sent = bytes.len() as u64;
        self.acc_tx_bytes += sent;
        self.acc_tx_datagrams += 1;
        *self.win_peer_bytes.entry(peer).or_insert(0) += sent;
    }

    /// Send the join handshake if the transport is actually connected; otherwise stay pending.
    ///
    /// A hello fired while the peer is still CONNECTING is silently lost (there is no routable
    /// destination yet), so the send is gated on the peer's connection status and retried from
    /// [`Self::client_handshake_upkeep`] until the server's welcome lands.
    fn send_hello(&mut self) {
        self.hello_pending = true;
        let Some(api) = self.base().get_multiplayer() else {
            return;
        };
        if !api.has_multiplayer_peer() || api.get_unique_id() == SERVER_PEER {
            return;
        }
        let Some(peer) = api.get_multiplayer_peer() else {
            return;
        };
        use godot::classes::multiplayer_peer::ConnectionStatus;
        if peer.get_connection_status() != ConnectionStatus::CONNECTED {
            return;
        }
        // The one datagram sent unauthenticated, because it is what carries the bytes everything else
        // is authenticated with. `start()` draws them; this covers the transport connecting first.
        let secret = self.session_secret;
        let nonce = *self
            .session_nonce
            .get_or_insert_with(Self::mint_session_key);
        self.session_auth
            .get_or_insert_with(|| SessionAuth::new(session_key_from(secret.as_ref(), nonce)));
        let hello = Handshake::local(self.tickrate.clamp(1, 240) as u16)
            .with_session(self.session_id)
            .with_nonce(nonce)
            .with_resume_token(self.resume_token);
        // The CONFIRMATION, and only when a secret is set. It is tagged over the version this frame
        // actually carries, because the accepting side recomputes it against the version it reads —
        // major must match but minor and patch may legitimately differ.
        let hello = match secret {
            Some(secret) => {
                let key = derive_session_key(&secret, &nonce);
                hello.with_confirm(confirm_tag(&key, &nonce, hello.protocol_version))
            }
            None => hello,
        };
        self.send_raw(SERVER_PEER, &hello.encode(), TransferMode::RELIABLE);
    }

    /// 16 unpredictable bytes, drawn fresh.
    ///
    /// **Three unrelated values come from here and each one is its own draw**: the client's session
    /// nonce, a connection's ack-token salt, and the high bits of a resume token. Sharing the draw
    /// would let a peer that learns one compute another, and the nonce is the one that is transmitted.
    ///
    /// `Crypto` is Godot's platform CSPRNG. `RandomNumberGenerator` is the fallback for a build
    /// without the mbedtls module, and it is **not** cryptographic: an attacker who can predict its
    /// stream can forge this session's datagrams — including under a session secret, because a
    /// predictable nonce is a predictable derived key. It is stated here rather than substituted
    /// silently, and the shipped export templates all carry `Crypto`.
    fn mint_session_key() -> [u8; KEY_LEN] {
        let mut key = [0u8; KEY_LEN];
        let random = Crypto::new_gd().generate_random_bytes(KEY_LEN as i32);
        let bytes = random.as_slice();
        if bytes.len() >= KEY_LEN {
            key.copy_from_slice(&bytes[..KEY_LEN]);
            return key;
        }
        let mut rng = RandomNumberGenerator::new_gd();
        rng.randomize();
        for chunk in key.chunks_mut(4) {
            chunk.copy_from_slice(&rng.randi().to_le_bytes());
        }
        key
    }

    /// 63 random bits for one identity's resume token.
    ///
    /// **A FRESH DRAW**, never a slice of a value the session already uses. In particular it is not
    /// [`PeerState::token_salt`]: that value is never transmitted, and this one is transmitted by
    /// definition, so deriving one from the other would hand every client the material to mint the ack
    /// token for frames it never received.
    ///
    /// **63 bits.** The value crosses the script boundary as a GDScript `int`, which is an `i64`,
    /// and games persist it beside the session id in whatever a save file or a config store holds. A
    /// negative id is a papercut in every one of those, so the sign bit is cleared for the same reason the
    /// facade's session id is minted positive. What is left is 9.2e18 values against an attacker who has to
    /// guess online, one handshake at a time.
    ///
    /// **Never `0`**, because `0` is the wire's absent value: a token that happened to draw zero would read
    /// as "this peer quotes no token" and refuse its own owner.
    fn mint_resume_token() -> u64 {
        let bytes = Self::mint_session_key();
        let mut half = [0u8; 8];
        half.copy_from_slice(&bytes[..8]);
        (u64::from_le_bytes(half) & 0x7fff_ffff_ffff_ffff).max(1)
    }

    /// Retry the handshake until the welcome lands. Reliable transport should make the first
    /// post-connection hello stick, but a server that was not yet in SERVER mode when it arrived
    /// would have dropped it — the retry makes the join robust to that ordering.
    fn client_handshake_upkeep(&mut self, delta: f64) {
        if self.mode != MODE_CLIENT || self.synced || !self.hello_pending {
            return;
        }
        self.hello_timer += delta;
        if self.hello_timer >= 0.5 {
            self.hello_timer = 0.0;
            self.send_hello();
        }
    }

    // ------------------------------------------------------------------
    // Registration
    // ------------------------------------------------------------------

    fn drain_pending(&mut self) {
        let ops = PENDING_OPS.with(|ops| std::mem::take(&mut *ops.borrow_mut()));
        for op in ops {
            match op {
                PendingOp::RegisterRollback(id, sync) => {
                    // The queued handle can be dead by the time it drains: a body spawned and freed
                    // inside one frame queues its Register and never its Unregister.
                    if let Some(mut sync_mut) = live_handle(&sync) {
                        sync_mut
                            .bind_mut()
                            .set_history_limit(self.history_limit.max(2) as usize);
                        self.check_expected_schema(id, &sync);
                        if self.debug_wire {
                            godot_print!(
                                "[orbitnet] reg rollback {:#018x} {}",
                                id,
                                sync.bind().base().get_path()
                            );
                        }
                        self.rollback_entities.insert(id, sync);
                        self.manifest_dirty = true;
                        self.slots_dirty = true;
                    }
                }
                PendingOp::RegisterState(id, sync) => {
                    if sync.is_instance_valid() {
                        if self.debug_wire {
                            godot_print!(
                                "[orbitnet] reg state {:#018x} {}",
                                id,
                                sync.bind().base().get_path()
                            );
                        }
                        self.state_entities.insert(id, sync);
                        self.manifest_dirty = true;
                        self.slots_dirty = true;
                    }
                }
                PendingOp::Unregister(id, who) => {
                    // Drop the map entry only if it is still the synchronizer that asked to leave. A
                    // respawn under the same node name derives the same entity id, and the replacement's
                    // Register can drain AHEAD of the corpse's Unregister (a body is deleted at the end of
                    // its frame, while the new one registers the moment it is built) — removing by bare id
                    // there would silently unregister the live body and stop replicating that player.
                    if self
                        .rollback_entities
                        .get(&id)
                        .is_some_and(|s| s.instance_id_unchecked() == who)
                    {
                        self.rollback_entities.remove(&id);
                    }
                    if self
                        .state_entities
                        .get(&id)
                        .is_some_and(|s| s.instance_id_unchecked() == who)
                    {
                        self.state_entities.remove(&id);
                    }
                    // The per-peer bookkeeping goes either way: it describes the DEPARTED body's history,
                    // so a replacement inheriting the id must not be delta-encoded against it. Cleared, the
                    // next send degrades to a full state block, which is exactly right for a fresh body.
                    self.planner.remove(id);
                    // THE SECOND LEAVE THAT HAPPENS BETWEEN UPDATES, and the one no `leaves` list
                    // can ever name: the entity is gone from the candidate list, so the next update
                    // diffs a union it has already been taken out of and reports nothing.
                    //
                    // **Announced to exactly the peers that held it**, and announced whenever the
                    // removal below actually changes that peer's set — including the respawn case,
                    // where the id survives in the registry but its interest entry does not. The
                    // replacement body re-enters through the filter on the next update and that
                    // update reports the enter, so the pair stays symmetric.
                    let mut lost: Vec<i32> = Vec::new();
                    for (&peer_id, peer) in self.peers.iter_mut() {
                        if peer.forget_entity(id) {
                            lost.push(peer_id);
                        }
                    }
                    for peer_id in lost {
                        self.interest_events.push((peer_id, id, false));
                    }
                    // The wire slot goes the same way, and for the same reason — but it is released
                    // by `reconcile_slots` rather than here, because "is this id still registered"
                    // is the question that decides it and the branches above may have left the id
                    // in place. A respawn whose Register drained ahead of the corpse's Unregister
                    // keeps its slot; nothing else does.
                    self.slots_dirty = true;
                }
            }
        }
        // Registry hygiene: drop any freed instances (a freed node cannot unregister itself if
        // it was freed without exiting the tree cleanly).
        self.rollback_entities.retain(|_, s| s.is_instance_valid());
        self.state_entities.retain(|_, s| s.is_instance_valid());
    }

    /// SERVER: bring the wire slot table back into agreement with the two entity registries.
    ///
    /// One reconciliation pass rather than a release beside every removal, because the registries
    /// lose entries three different ways — an `Unregister` op, a respawn that supersedes its
    /// predecessor, and the `is_instance_valid` sweep that catches a node freed without leaving the
    /// tree cleanly — and only the first of those has a call site to hang a release on.
    ///
    /// Runs when a registration changed, and whenever the table and the registries disagree on size
    /// — the size check is what catches the silent sweep, which raises no flag of its own.
    /// [`SlotTable::reconcile`] holds the algorithm and the reuse rules; this decides when to run it
    /// and what to do with what it reports.
    fn reconcile_slots(&mut self, current: u64) {
        let registered_count = self.rollback_entities.len() + self.state_entities.len();
        if !self.slots_dirty && self.slots.len() == registered_count {
            return;
        }
        let registered: std::collections::BTreeSet<u64> = self
            .rollback_entities
            .keys()
            .chain(self.state_entities.keys())
            .copied()
            .collect();
        let outcome = self.slots.reconcile(&registered, current);
        if outcome.released > 0 || outcome.named > 0 {
            self.manifest_dirty = true;
        }
        // Raised while any entity is still without a slot, which keeps this pass running next tick.
        // A quarantined slot is a refusal that expires on its own; stranding the entity instead
        // would stop replicating it for the rest of the session.
        self.slots_dirty = outcome.unnamed > 0;
        match outcome.exhausted {
            Some(id) if !self.slots_exhausted_warned => {
                self.slots_exhausted_warned = true;
                godot_error!(
                    "OrbitNet: every one of the {} entity slots this session can name on the wire \
                     is in use, so entity {:#018x} and any further one cannot be replicated. \
                     Unregister entities, or split the world across sessions.",
                    orbitnet_core::slots::MAX_SLOTS,
                    id
                );
            }
            // The condition lifted: a later exhaustion is worth reporting again.
            None => self.slots_exhausted_warned = false,
            Some(_) => {}
        }
    }

    fn check_expected_schema(&self, id: u64, sync: &Gd<OrbitRollbackSynchronizer>) {
        if let Some(&(state_hash, input_hash)) = self.expected_schemas.get(&id) {
            let bound = sync.bind();
            let local_state = bound.schema_hash() as u32;
            let local_input = bound.input_schema_hash() as u32;
            if local_state != state_hash || local_input != input_hash {
                godot_error!(
                    "OrbitNet schema mismatch for entity {:#018x}: server {:#010x}/{:#010x}, \
                     local {:#010x}/{:#010x}. The peers registered different properties, or \
                     registered them in a different order.",
                    id,
                    state_hash,
                    input_hash,
                    local_state,
                    local_input
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // Tick pacing
    // ------------------------------------------------------------------

    fn effective_rate(&self) -> TickRate {
        if self.sync_to_physics {
            TickRate::new(Engine::singleton().get_physics_ticks_per_second() as u32)
        } else {
            TickRate::new(self.tickrate.max(1) as u32)
        }
    }

    fn publish_tick_state(&self) {
        let factor = self.tick_factor();
        let tick = self.accumulator.tick();
        TICK_STATE.with(|s| s.set((factor, tick)));
    }

    /// The panic path the slew/stretch corrections cannot reach: after a multi-tick stall or
    /// drift, reseek straight to the server's estimated tick. A jump that size invalidates every
    /// tick-keyed ring (a backward jump would pin them refusing writes), so each entity gets a
    /// session reset and a full snapshot is NACKed — one visible correction instead of a
    /// minutes-long crawl at one slewed tick per cooldown.
    fn maybe_hard_resync(&mut self, dt: f64) -> bool {
        if self.mode != MODE_CLIENT || !self.synced || dt <= 0.0 {
            return false;
        }
        // TEST THE ERROR THE CONTROLLER IS ACTUALLY DRIVING TO ZERO, NOT THE RAW OFFSET.
        //
        // Steady state is `offset/dt + lead_bias_ticks == 0` (see `step_coupled`), so a healthy client's
        // `clock.offset()` settles at MINUS the dialed-in lead -- by design, because a client must run ahead
        // of the server for its input to arrive before the tick that consumes it. `lead_bias_ticks` clamps at
        // 8, which at 60 Hz is 133 ms of intended offset before a single millisecond of jitter.
        //
        // Comparing that against a 250 ms panic threshold fired the panic path on a correctly-leading client,
        // and the reseek below then targeted `offset == 0`, throwing the lead away. The controller drove
        // straight back to it, jitter carried it past the threshold again, and it fired again: measured at
        // ~30 hard resyncs per MINUTE on a rendered client over a LAN, on this branch and on main alike.
        // Each one reseeks the tick, clears the planner and NACKs a full snapshot -- a visible correction
        // twice a second, which is what a player calls rubber banding.
        let lead_seconds = self.lead_bias_ticks * dt;
        if !self
            .clock
            .needs_hard_resync_with_lead(HARD_RESYNC_SECONDS, lead_seconds)
        {
            return false;
        }
        let residual_ticks = self.clock.offset() / dt + self.lead_bias_ticks;
        let offset = self.clock.offset();
        // The backlog-inclusive timeline, to match what the offset was measured against — and
        // because `seek` clears the accumulator, so any retained backlog left out of `local`
        // would be silently dropped from the reseek target.
        let local = self.accumulator.timeline_seconds();
        // ...and land where the controller WANTS to be, lead included, so the reseek is a fixed point rather
        // than a place the very next tick leaves again.
        let lead = INITIAL_LEAD_TICKS as f64 + self.lead_bias_ticks.max(0.0);
        let target = (((local + offset) / dt + lead).max(0.0)) as u64;
        godot_warn!(
            "OrbitNet: clock residual {:.0} ms (offset {:.0} ms, lead {:.1} ticks) is beyond the slew's reach — hard resync tick {} -> {}",
            residual_ticks * dt * 1000.0,
            offset * 1000.0,
            self.lead_bias_ticks,
            self.accumulator.tick(),
            target
        );
        self.accumulator.seek(target);
        self.planner.clear();
        self.slew.reset();
        // Old samples measured the old local timeline; keep quiet until fresh pongs arrive.
        self.clock.clear();
        self.newest_snapshot_tick = 0;
        self.snapshot_ack_bits = 0;
        self.snapshot_ack_token = 0;
        // The margin window described the old timeline; the dialed-in bias is still the best
        // guess for steady-state need, so it survives the reseek.
        self.lead.clear();
        self.want_full = true;
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().reset_session();
        }
        for sync in self.state_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().reset_session();
        }
        true
    }

    /// Close the adaptive-lead loop: fold the server-reported margin window into a bounded
    /// tick bias the clock chases. Positive bias runs the client further ahead of the server, so
    /// its inputs arrive earlier; the LeadTracker's hysteresis keeps adjustments rare.
    fn update_lead_bias(&mut self) {
        if self.mode != MODE_CLIENT {
            return;
        }
        let adjustment = self.lead.suggest_adjustment(1);
        if adjustment != 0 {
            self.lead_bias_ticks = (self.lead_bias_ticks + f64::from(adjustment)).clamp(-2.0, 8.0);
        }
    }

    fn step_coupled(&mut self) {
        // Coupled mode pins stretch to exactly 1.0 — a stretched clock slides tick boundaries
        // across physics frames and renders as judder. Clock error is absorbed by slewing whole
        // ticks, rarely, under hysteresis + cooldown.
        self.stretch_now = 1.0;
        let rate = self.effective_rate();
        self.accumulator.set_rate(rate);
        let dt = rate.dt();
        self.maybe_hard_resync(dt);
        self.update_lead_bias();
        let ticks = if self.mode == MODE_CLIENT {
            let offset_ticks = self.clock.offset() / dt + self.lead_bias_ticks;
            self.slew.decide(offset_ticks).ticks()
        } else {
            1
        };
        // Exactly `ticks` whole tick-lengths: no fractional remainder can exist in coupled mode.
        let first = self.accumulator.tick();
        let step = self.accumulator.advance(dt * f64::from(ticks));
        self.run_frame(first, step.ticks, dt);
    }

    fn step_decoupled(&mut self, delta: f64) {
        let rate = TickRate::new(self.tickrate.max(1) as u32);
        self.accumulator.set_rate(rate);
        self.maybe_hard_resync(rate.dt());
        self.update_lead_bias();
        self.stretch_now = if self.mode == MODE_CLIENT {
            self.clock.stretch_with(
                self.lead_bias_ticks * rate.dt(),
                self.max_stretch.max(1.001),
                0.5,
            )
        } else {
            1.0
        };
        // The accumulator owns the sub-tick remainder; run_frame is handed the batch it decided
        // on. (An earlier draft walked the counter back with seek(), which CLEARS the remainder —
        // that quietly ran the loop at two-thirds of the configured rate.)
        let first = self.accumulator.tick();
        let step = self.accumulator.advance(delta * self.stretch_now);
        if step.clamped && self.mode == MODE_CLIENT {
            // A discard tore the local timeline: every sample in the window measured the one
            // that no longer exists, and a panic fired on a mixture of old and new samples
            // aims its reseek at neither. Go quiet until fresh pongs describe the new timeline,
            // then the hard resync (which this stall has all but guaranteed) fires once, aimed.
            self.clock.clear();
        }
        if step.ticks > 0 {
            self.run_frame(first, step.ticks, rate.dt());
        } else {
            self.run_net_upkeep(delta);
        }
    }

    fn run_frame(&mut self, first_tick: u64, ticks: u32, dt: f64) {
        if ticks == 0 {
            self.run_net_upkeep(dt);
            return;
        }

        // Who is connected, asked once for the whole frame. Read per entity per tick below to decide which
        // entities have lost their input author.
        self.refresh_live_peers();

        // Batch boundary: land buffered authoritative rows before anything reads state.
        self.apply_pending_rows();

        // Immediately before the announcement, so one frame carries both a queued release and the
        // `seat_closed` it causes. The transport callback that queued it could not do the work
        // itself: it is delivered from inside `poll()`, with a bind held on a synchronizer.
        self.drain_seat_releases();

        // Before the first tick of the batch, so a seat a handler opens in response is driving a
        // viewpoint from the next frame rather than from part-way through this one — the same
        // tick-boundary rule `drain_pending` gives a registration.
        self.announce_seats();

        for offset in 0..u64::from(ticks) {
            let tick = first_tick + offset;
            self.emitting_tick = Some(tick);

            // Game code fills its input frame in a pre_tick handler; the emit surrenders our
            // borrow so those handlers may call back into this node.
            self.signals().before_tick().emit(dt, tick as i64);

            self.capture_inputs(tick);
            self.mark_forward_ticks(tick);

            self.signals().after_tick().emit(dt, tick as i64);
        }
        self.emitting_tick = None;

        let current = first_tick + u64::from(ticks);
        self.run_rollback(current, dt);
        self.capture_state_lane(current);
        self.run_net_upkeep(dt * f64::from(ticks));
        self.flush_network(current);

        // AFTER the send, so a server announces the same tick's interest pass rather than the
        // previous one's, and on a tick boundary, so a client announces what its packet handlers
        // queued while `poll()` ran. Both ends therefore emit where `announce_seats` does.
        self.announce_interest();

        self.signals().after_rollback_loop().emit();
    }

    /// Rebuild the seat roster and emit what arrived and what left. Once per frame that runs a tick.
    ///
    /// **A seat is derived, never declared on its own.** It exists because some replicated body says
    /// its input is driven by connection `p` under label `s`; the roster is the deduplicated set of
    /// those pairs. Holding a seat table the game writes directly would be a second source of truth
    /// about ownership, and ownership is what the anti-forgery check on a received input block reads
    /// — the two disagreeing is a seat the server believes in and refuses input for.
    ///
    /// **Where the answer comes from differs by role and the announcement does not.** A server (and
    /// an offline session, and a host) rescans its own registry; a client projects the table the
    /// entity manifest gave it. Both then run the same diff, so `seat_opened` on a client means what
    /// it means on the server, one manifest later.
    ///
    /// **The rescan is the per-frame cost and the projection is not.** Detecting an authority write
    /// needs the walk — nothing signals one, and `set_input_authority` is a node property write the
    /// backend never sees — so this pays one cached-field read per rollback entity per frame, the
    /// same order as the per-tick gather the send path already does. Everything past it is behind
    /// `seats_dirty`, because a session changes seats approximately never.
    ///
    /// **A change here republishes the manifest.** The manifest carries the seat half of this table,
    /// and registration is the only thing that already dirties it; an authority or seat write on an
    /// entity that stays registered is exactly the case nothing else notices.
    ///
    /// **A DEDICATED SERVER HOLDS NO SEAT OF ITS OWN**, and a listen server does. See the filter
    /// below for why the distinction is made here rather than where the roster is projected.
    fn announce_seats(&mut self) {
        // A client is TOLD its roster. Deriving one from local authority instead would answer with
        // whatever this peer happens to have set on its own copy of the scene — which for every body
        // it does not drive is nothing at all.
        if self.mode != MODE_CLIENT {
            // **A DEDICATED SERVER IS NOT A PLAYER, SO IT HOLDS NO SEAT.** `set_input_authority(1)`
            // is how a game says a body is UNCLAIMED — it is what `release_seat` does — so counting
            // peer 1 there would announce a viewpoint for every body nobody is driving, and a client
            // running one seating handler would open a split-screen viewport for a player that does
            // not exist. A LISTEN SERVER is the opposite case: peer 1 is the host player, and the
            // couch this feature exists for is usually theirs.
            //
            // The two are indistinguishable to a client, which is why the rule is applied here
            // rather than at the projection. It leaves one ambiguity, on a listen server only: a
            // body the host holds unclaimed reads the same as one the host player drives. A game
            // that has to tell them apart seats its host player on a non-zero label.
            let local_is_a_player = self.mode != MODE_SERVER;
            let mut scan = std::mem::take(&mut self.seat_scan);
            scan.clear();
            for (&id, sync) in &self.rollback_entities {
                let Some(sync) = live_handle(sync) else {
                    continue;
                };
                let bound = sync.bind();
                let (owner, seat) = (bound.input_owner_hint(), bound.seat_hint());
                drop(bound);
                // `0` is an unresolved input root, not a connection. A body nobody drives seats
                // nobody, and the state lane never reaches here at all.
                if owner > 0 && (owner != SERVER_PEER || local_is_a_player) {
                    scan.push((id, owner, seat));
                }
            }
            // Ascending by id, so the comparison below is about the scene rather than about
            // `HashMap` iteration order — and so the manifest's rows come out in a stable order.
            scan.sort_unstable();
            if scan != self.entity_seats {
                self.manifest_dirty = true;
                self.seats_dirty = true;
                std::mem::swap(&mut self.entity_seats, &mut scan);
            }
            self.seat_scan = scan;
        }

        if !self.seats_dirty {
            return;
        }
        self.seats_dirty = false;

        let mut gather = std::mem::take(&mut self.seat_gather);
        let mut opened = std::mem::take(&mut self.seat_opened);
        let mut closed = std::mem::take(&mut self.seat_closed);
        gather.clear();
        gather.extend(
            self.entity_seats
                .iter()
                .map(|&(_, peer, seat)| SeatId::new(peer, seat)),
        );
        self.seat_roster
            .replace_into(&mut gather, &mut opened, &mut closed);
        // `replace_into` swapped the gathered set into the roster, so `gather` now holds the
        // previous one — returned to the pool as the buffer for the next announcement.
        self.seat_gather = gather;

        // Closed before opened, so a body moving between connections is reported as the old
        // viewpoint ending and then the new one beginning rather than the other way round. The emit
        // surrenders our borrow, so a handler may call back into this node; the roster is already
        // updated, so what it does lands in the next announcement.
        for id in &closed {
            self.signals()
                .seat_closed()
                .emit(i64::from(id.peer), i64::from(id.seat));
        }
        for id in &opened {
            self.signals()
                .seat_opened()
                .emit(i64::from(id.peer), i64::from(id.seat));
        }
        self.seat_opened = opened;
        self.seat_closed = closed;
    }

    /// Emit the relevancy transitions queued this frame. Once per frame that runs a tick.
    ///
    /// **Queued at the source and emitted here, for the reason `announce_seats` is a separate pass.**
    /// A server finds a transition inside the send path, with a bind held on a synchronizer; a client
    /// finds one inside a packet handler, called from `poll()`. Emitting from either would run game
    /// code there, and a handler is entitled to call back into this node.
    ///
    /// **`peer` names the connection the entity left or entered** — a remote connection on a server,
    /// this peer itself on a client. That is the `seat_opened` / `seat_closed` convention, and it is
    /// what lets one handler serve both ends.
    fn announce_interest(&mut self) {
        if self.interest_events.is_empty() {
            return;
        }
        let mut events = std::mem::take(&mut self.interest_events);
        for &(peer, id, entered) in &events {
            if entered {
                self.signals()
                    .entity_entered_interest()
                    .emit(i64::from(peer), id as i64);
            } else {
                self.signals()
                    .entity_left_interest()
                    .emit(i64::from(peer), id as i64);
            }
        }
        events.clear();
        // Re-pool the emptied buffer, unless a handler queued into the fresh one while the loop ran.
        if self.interest_events.is_empty() {
            self.interest_events = events;
        }
    }

    /// Release the seats queued by [`Self::_on_peer_disconnected`]. Once per frame that runs a tick,
    /// immediately before [`Self::announce_seats`].
    ///
    /// **Why the ordering is that and not something else.** The release changes what the rescan in
    /// `announce_seats` sees, so draining first means one frame carries both the release and the
    /// `seat_closed` it caused. Draining after would announce the old roster, then the new one a frame
    /// later — two announcements for one event, with a frame in between where the seat is closed on
    /// the server and open in every handler.
    ///
    /// **The liveness guard is re-asked here, not at the drop.** Transport peer ids are reused, and
    /// the whole point of `peer_is_live` is that an id naming a dead connection at one moment may name
    /// a live one at the next. See `orbitnet_core::seats::releases_seats`.
    fn drain_seat_releases(&mut self) {
        if self.pending_seat_releases.is_empty() {
            return;
        }
        let policy = seat_release_policy_of(self.seat_release_policy);
        let mut pending = std::mem::take(&mut self.pending_seat_releases);
        for &peer in &pending {
            if releases_seats(policy, SeatReleaseEvent::Dropped, self.peer_is_live(peer)) {
                self.release_owned_bodies(peer, None);
            }
        }
        pending.clear();
        // Re-pool the emptied buffer, unless a callback queued into the fresh one while the loop ran
        // — which is the re-entrancy this queue exists for, so it is handled rather than assumed away.
        if self.pending_seat_releases.is_empty() {
            self.pending_seat_releases = pending;
        }
    }

    /// Whether `peer` currently names a connected transport peer (or this peer itself).
    ///
    /// **Asked live rather than read from [`Self::live_peers`]**, because that set is refreshed once
    /// per frame that runs a tick and both release paths can run on a frame that ran none. This is one
    /// engine call on a path that fires at most once per drop and once per expiry, against a cached
    /// set whose staleness would be wrong in the one direction that costs a live player their body.
    fn peer_is_live(&self, peer: i32) -> bool {
        let Some(api) = self.base().get_multiplayer() else {
            return false;
        };
        if !api.has_multiplayer_peer() {
            return false;
        }
        if api.clone().get_unique_id() == peer {
            return true;
        }
        api.get_peers().as_slice().contains(&peer)
    }

    /// Hand every rollback body `peer` drives (optionally only those on seat `label`) back to the
    /// server. Answers how many entities changed.
    ///
    /// **A SERVER-SIDE-ONLY RELEASE IS SUFFICIENT, and the reason is worth stating** because the rest
    /// of the authority rules say a write like this must happen on every peer:
    ///
    /// - **Nobody on a client believed they owned the body.** The connection that did is gone, and no
    ///   other peer ever had `input_local` set for it, so no client stops predicting something it was
    ///   predicting and no client starts.
    /// - **Clients learn the seat closed from the ENTITY MANIFEST**, which this release dirties: the
    ///   manifest carries `(entity, owner, seat)`, the rescan in [`Self::announce_seats`] sees the
    ///   changed owner, and every client projects the new roster and emits `seat_closed` from it.
    /// - **The residue is inert.** A client's own copy of the node keeps a local multiplayer authority
    ///   naming the dead peer until the game's roster message re-points it. Nothing reads that: the
    ///   anti-forgery check on received input runs on the server, prediction is off for a body this
    ///   peer does not own, and the send path anchors interest from the server's own view. It is a
    ///   stale number, not a stale decision.
    ///
    /// The release itself is `OrbitRollbackSynchronizer::release_seat` — input back to the server, label
    /// back to `0`, in one call so the body is never briefly `(server, old label)`. It is reached
    /// through a dynamic call because the registry walk holds no bind at that point and must not: the
    /// verb re-resolves authority, which reaches back out into the scene.
    fn release_owned_bodies(&mut self, peer: i32, label: Option<SeatIndex>) -> i64 {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return 0;
        }
        // `0` is an unresolved input root and `SERVER_PEER` is what an unclaimed body already reads
        // as, so neither names a connection whose seats there is anything to release.
        if peer <= 0 || peer == SERVER_PEER {
            return 0;
        }
        let mut targets: Vec<Gd<OrbitRollbackSynchronizer>> = Vec::new();
        for sync in self.rollback_entities.values() {
            let Some(sync) = live_handle(sync) else {
                continue;
            };
            let matches = {
                let bound = sync.bind();
                bound.input_owner_hint() == peer
                    && label.is_none_or(|wanted| bound.seat_hint() == wanted)
            };
            if matches {
                targets.push(sync);
            }
        }
        let released = targets.len() as i64;
        if released > 0 {
            let verb = StringName::from("release_seat");
            let _guard = self.base_mut();
            for mut sync in targets {
                sync.call(&verb, &[]);
            }
        }
        released
    }

    /// Capture every locally-authored input row for `tick`.
    ///
    /// Two passes rather than one, because a bulk input hook is game code and has to run with the
    /// binds dropped. The staging pass is NOT gated on a session-wide flag the way phase 3's is:
    /// it runs once per tick rather than once per replayed tick, and what it costs an entity with
    /// no hook is one native `bind` — set beside the `Object::get` per input property the second
    /// pass makes on that same entity, which is a script-boundary crossing.
    fn capture_inputs(&mut self, tick: u64) {
        let delay = self.input_delay.max(0) as u64;
        let stamp = tick + delay;
        let mut hook_batch: Vec<binding::HookCall> = Vec::new();
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            if bound.owns_input() {
                bound.stage_capture(sync::LANE_INPUT, &mut hook_batch);
            }
        }
        self.run_hooks(&mut hook_batch);
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            if bound.owns_input() {
                bound.capture_local_input(stamp);
            }
        }
    }

    /// Per tick, decide what each rollback entity's input confidence is before anything simulates it, and
    /// plan the entities this peer will run.
    ///
    /// Two server fallbacks live here, and they are the same statement about two different absences. An
    /// entity with NO input bindings has no author by construction; an entity whose input owner has left has
    /// lost the one it had. In both cases the server is what is authoring the body, so its tick is
    /// authoritative — and the second case additionally has to say what that authorship IS, which is the
    /// neutral row. See `OrbitRollbackSynchronizer::mark_orphaned_authoritative`.
    ///
    /// The owner is read from the CACHED hint rather than live, and that is deliberate: the hint is what the
    /// game last declared through `set_input_authority`, so an owner it still names after the peer left is
    /// exactly the orphan this looks for. The moment the game re-points the body — at a rejoin, or at a
    /// release — the hint moves with it and the fallback stops applying, in the same call.
    fn mark_forward_ticks(&mut self, tick: u64) {
        let server = self.mode == MODE_SERVER || self.mode == MODE_HOST;
        for (&id, sync) in &self.rollback_entities {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            bound.mark_inputless_authoritative(tick);
            if server && !self.live_peers.contains(&bound.input_owner_hint()) {
                bound.mark_orphaned_authoritative(tick);
            }
            if bound.simulates() || bound.predicts_remotely() {
                self.planner.mark(id, tick);
            }
        }
    }

    /// Land every buffered authoritative row, on both lanes, at the frame's tick boundary.
    ///
    /// **The receive apply, and the only property walk a peer that simulates nothing runs.** Such a
    /// peer plans no entities, so `run_rollback` returns on an empty plan and neither the capture
    /// nor the restore hook is reached. A lane that declares a `bulk_apply_method` decodes its row
    /// into the hook's array bound, here, and the call runs below with every `bind` dropped.
    ///
    /// **Staging changes the interleaving.** One entity's apply used to complete before the next
    /// one's began; now every row decodes and then every game call runs. Nothing may notice: a hook
    /// is a marshalling method, the "do not call the facade from a hook" rule already forbids the
    /// code that could, and the capture direction accepted the same hazard when it was staged. What
    /// changed is that an apply hook reading ANOTHER entity's properties now reads them before that
    /// entity's own row has landed.
    fn apply_pending_rows(&mut self) {
        let mut hook_batch: Vec<binding::HookCall> = Vec::new();
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().apply_pending_display(&mut hook_batch);
        }
        for sync in self.state_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().apply_pending(&mut hook_batch);
        }
        self.run_hooks(&mut hook_batch);
    }

    /// Run a batch of staged bulk marshalling hooks with every `bind` on this node surrendered.
    ///
    /// **The surrender is required, not a precaution.** A bulk hook is game code, and game code
    /// called from inside the loop legally calls back into the facade — `current_tick()`,
    /// `rollback_tick()`, a memo read. Phase 2 has always run `_rollback_tick` under this guard;
    /// capture and restore are the same mechanism applied to marshalling, so they take it too.
    /// Calling one while the synchronizer or this node is bound is a borrow panic, not a wrong
    /// answer.
    ///
    /// The batch is drained and handed back empty, so one `Vec` carries a whole frame's calls.
    fn run_hooks(&mut self, batch: &mut Vec<binding::HookCall>) {
        if batch.is_empty() {
            return;
        }
        let mut calls = std::mem::take(batch);
        {
            let _guard = self.base_mut();
            for call in &mut calls {
                call.invoke();
            }
        }
        calls.clear();
        *batch = calls;
    }

    fn run_rollback(&mut self, current: u64, dt: f64) {
        let started = Instant::now();
        let limit = self.history_limit.max(2) as u64;

        if self.resim_force > 0 {
            let force_from = current.saturating_sub(self.resim_force.max(0) as u64);
            for (&id, sync) in &self.rollback_entities {
                let Some(sync) = live_handle(sync) else {
                    continue;
                };
                if sync.bind().simulates() {
                    self.planner.mark(id, force_from);
                }
            }
        }

        let plan = self.planner.plan(current, limit);
        if plan.is_empty() {
            self.m_resim_ticks = 0.0;
            self.m_rb_nodes = 0.0;
            self.m_rollback_ms = 0.0;
            self.m_restore_ms = 0.0;
            self.m_sim_ms = 0.0;
            self.m_record_ms = 0.0;
            return;
        }

        let from = plan.iter().map(|p| p.range.from).min().unwrap_or(current);
        self.dbg_resim_spans += plan.len() as u64;
        self.dbg_resim_ticks_total += current.saturating_sub(from);
        let mut ranges: Vec<(u64, u64, u64, Gd<OrbitRollbackSynchronizer>)> = Vec::new();
        // Whether ANY entity in this frame's plan captures through a bulk hook. Answered once here,
        // where the entities are already being walked, so phase 3's staging pass is skipped
        // outright in a session that declares none — the default, which must not pay for the
        // feature.
        let mut any_capture_hook = false;
        for entry in &plan {
            if let Some(sync) = self
                .rollback_entities
                .get(&entry.body)
                .and_then(live_handle)
            {
                any_capture_hook |= sync.bind().has_capture_hook(sync::LANE_STATE);
                ranges.push((entry.body, entry.range.from, entry.range.to, sync));
            }
        }

        let mut rb_nodes = 0u64;
        let mut call_batch: Vec<(Gd<Node>, bool)> = Vec::new();
        // Staged bulk marshalling calls, reused across every phase and every tick of this frame.
        let mut hook_batch: Vec<binding::HookCall> = Vec::new();
        // THREE NUMBERS WHERE THERE WAS ONE. The loop already ran these as three passes; what
        // changed is that they are now timed separately. `m_rollback_ms` wrapped restore + game code + record in
        // a single figure, so the documented "capture lands within 2-3x of the old backend" was an assertion
        // nobody could prove or refute -- the headline performance claim of this codebase had no measurement
        // behind it. `net.resim_force` multiplies the capture cost against a fixed sim cost, so with the three
        // reported apart the isolation lever finally has something to isolate. (Read the commit title as
        // "split the rollback MEASUREMENT"; the control flow below is the control flow that was already there.)
        //
        // The cost is four extra `Instant::now()` calls per replayed tick over the one pair that was always
        // taken -- a clock read each, and this is a diagnostic on the path the whole epic exists to make
        // cheaper. It is unconditional because the figures are what `net.perf` reports on demand and a flag
        // that had to be armed in advance would not be there when somebody wanted it.
        let mut restore_ns = 0u128;
        let mut sim_ns = 0u128;
        let mut record_ns = 0u128;
        // HOISTED OUT OF THE PER-TICK LOOP. A `StringName` construction per replayed tick is a string intern
        // per tick for a name that never changes; at `resim_force 12` that is twelve of them per frame for
        // nothing.
        let rollback_method = StringName::from("_rollback_tick");

        for tick in from..current {
            self.rollback_tick_now = Some(tick);

            // Phase 1 — restore state + input for every entity replaying this tick. A lane with a
            // bulk restore hook decodes its row into the hook's array here and stages the call;
            // every lane without one walks its properties, as before. The staged calls run after,
            // with the binds dropped.
            let phase_started = Instant::now();
            for (_, range_from, range_to, sync) in &ranges {
                if tick < *range_from || tick >= *range_to {
                    continue;
                }
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                sync.bind_mut().restore_tick(tick, &mut hook_batch);
            }
            self.run_hooks(&mut hook_batch);

            restore_ns += phase_started.elapsed().as_nanos();

            // Phase 2 — simulate. Collect the call list with all binds dropped, then run the
            // game code under a base_mut() surrender so its callbacks can re-enter this node.
            let phase_started = Instant::now();
            call_batch.clear();
            for (_, range_from, range_to, sync) in &ranges {
                if tick < *range_from || tick >= *range_to {
                    continue;
                }
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                let fresh = {
                    let mut bound = sync.bind_mut();
                    bound.begin_sim(tick)
                };
                if fresh {
                    self.dbg_fresh += 1;
                }
                for node in sync.bind().call_list() {
                    call_batch.push((node, fresh));
                }
            }
            if !call_batch.is_empty() {
                rb_nodes += call_batch.len() as u64;
                let batch = std::mem::take(&mut call_batch);
                {
                    let _guard = self.base_mut();
                    for (mut node, fresh) in batch {
                        if node.is_instance_valid() {
                            node.call(
                                &rollback_method,
                                &[
                                    Variant::from(dt),
                                    Variant::from(tick as i64),
                                    Variant::from(fresh),
                                ],
                            );
                        }
                    }
                }
            }

            sim_ns += phase_started.elapsed().as_nanos();

            // Phase 3 — record the resulting state as tick + 1. Entities with a bulk capture hook
            // run it first, binds dropped, filling their preallocated arrays; the encode into the
            // row then happens bound, in record_tick, whichever way the values arrived.
            //
            // The quantized write-back closes the phase, and an entity gated onto its apply hook
            // stages that call rather than making it: like the capture hook it is game code, so it
            // runs with the binds dropped. That defers it past the other entities' record_tick, the
            // same interleaving change staging makes everywhere else in this loop.
            let phase_started = Instant::now();
            if any_capture_hook {
                for (_, range_from, range_to, sync) in &ranges {
                    if tick < *range_from || tick >= *range_to {
                        continue;
                    }
                    let Some(mut sync) = live_handle(sync) else {
                        continue;
                    };
                    sync.bind_mut()
                        .stage_capture(sync::LANE_STATE, &mut hook_batch);
                }
                self.run_hooks(&mut hook_batch);
            }
            for (_, range_from, range_to, sync) in &ranges {
                if tick < *range_from || tick >= *range_to {
                    continue;
                }
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                sync.bind_mut().record_tick(tick, &mut hook_batch);
            }
            self.run_hooks(&mut hook_batch);
            record_ns += phase_started.elapsed().as_nanos();
        }
        self.rollback_tick_now = None;

        // Optional latency masking: present an older, more-confirmed tick.
        if self.display_offset > 0 {
            let display_tick = current.saturating_sub(self.display_offset as u64);
            for (_, _, _, sync) in &ranges {
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                sync.bind_mut().restore_tick(display_tick, &mut hook_batch);
            }
            self.run_hooks(&mut hook_batch);
        }

        self.m_resim_ticks = (current - from) as f64;
        self.m_rb_nodes = rb_nodes as f64;
        self.m_rollback_ms = started.elapsed().as_secs_f64() * 1000.0;
        // The three phases sum to slightly less than `rollback_ms`: the difference is the range setup and the
        // display-offset restore, which belong to neither. Published as they are rather than normalized, so the
        // gap stays visible instead of being quietly attributed to one of them.
        self.m_restore_ms = restore_ns as f64 / 1_000_000.0;
        self.m_sim_ms = sim_ns as f64 / 1_000_000.0;
        self.m_record_ms = record_ns as f64 / 1_000_000.0;
    }

    /// Capture the state lane's frontier row for every channel this peer's state owns.
    ///
    /// Two passes for the same reason [`Self::capture_inputs`] takes two, and ungated for the same
    /// reason.
    fn capture_state_lane(&mut self, tick: u64) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
        let mut hook_batch: Vec<binding::HookCall> = Vec::new();
        for sync in self.state_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            if bound.owns_state() {
                bound.stage_capture(&mut hook_batch);
            }
        }
        self.run_hooks(&mut hook_batch);
        for sync in self.state_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            if bound.owns_state() {
                bound.capture_frontier(tick);
            }
        }
    }

    /// Monotonic wall clock in milliseconds, for the resume windows.
    ///
    /// Wall clock rather than accumulated tick time, because a grace window measures how long a PLAYER has
    /// been away and that is not a simulation quantity: it must keep running while the loop stretches,
    /// catches up, or discards a backlog after a hitch.
    fn now_ms() -> u64 {
        Time::singleton().get_ticks_msec()
    }

    /// Refresh [`Self::live_peers`] from the transport, once per frame.
    ///
    /// Server-side only, because [`Self::mark_forward_ticks`] is the only reader and it asks only on the
    /// authority. A client would pay an engine call and an allocation every frame for a set nothing reads.
    fn refresh_live_peers(&mut self) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
        let mut live = std::collections::HashSet::new();
        if let Some(api) = self.base().get_multiplayer() {
            live.insert(api.clone().get_unique_id());
            for id in api.get_peers().as_slice() {
                live.insert(*id);
            }
        }
        self.live_peers = live;
    }

    /// Release every held session whose window closed, telling the game about each.
    ///
    /// **Under `RELEASE_ON_EXPIRY` the seats are released BEFORE the signal that motivated it.** A game
    /// that seats a replacement player from its `peer_session_expired` handler is doing the right thing
    /// with the event, and releasing afterward would undo that work — the walk would find bodies the
    /// handler had just re-pointed and hand them straight back to the server. Releasing first means the
    /// handler runs against a roster the release has already finished with.
    ///
    /// **Immediate here, unlike the drop path.** This runs from the frame's own upkeep with no bind
    /// held, which is what makes `bind_mut()` on the registry safe; `_on_peer_disconnected` is a
    /// transport callback and is not.
    ///
    /// The liveness guard is what keeps this from taking a live player's body: an expiry names the id a
    /// session was last connected under, and that connection ended up to a whole grace window ago — long
    /// enough for the transport to have handed the id to somebody else.
    fn expire_held_sessions(&mut self) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
        let due = self.resume.expire(Self::now_ms());
        let policy = seat_release_policy_of(self.seat_release_policy);
        for (session_id, peer) in due {
            if releases_seats(policy, SeatReleaseEvent::Expired, self.peer_is_live(peer)) {
                self.release_owned_bodies(peer, None);
            }
            self.signals()
                .peer_session_expired()
                .emit(session_id as i64, i64::from(peer));
        }
    }

    fn run_net_upkeep(&mut self, delta: f64) {
        self.expire_held_sessions();
        if self.mode == MODE_CLIENT && self.running {
            self.ping_timer += delta;
            if self.ping_timer >= PING_INTERVAL {
                self.ping_timer = 0.0;
                self.send_ping();
            }
        }
        // `metrics()` is `&self`, so the raw counters are windowed here — the one place that
        // already owns a one-second timer — rather than divided on every read.
        self.bw_timer += delta;
        if self.bw_timer >= BANDWIDTH_WINDOW_SECONDS {
            let window = self.bw_timer;
            self.bw_timer = 0.0;
            self.publish_bandwidth(window);
        }
        if self.debug_wire {
            self.dbg_timer += delta;
            if self.dbg_timer >= 1.0 {
                self.dbg_timer = 0.0;
                godot_print!(
                    "[orbitnet] tick={} mode={} peers={} ents={}r/{}s sent={} blk {} B rx applied={} rejected={} skipped={} kinds={:?}",
                    self.accumulator.tick(),
                    self.mode,
                    self.peers.len(),
                    self.rollback_entities.len(),
                    self.state_entities.len(),
                    self.dbg_sent,
                    self.dbg_sent_bytes,
                    self.dbg_rx_applied,
                    self.dbg_rx_rejected,
                    self.dbg_rx_skipped,
                    self.dbg_rx_kinds
                );
                if self.dbg_rx_unauth > 0 {
                    godot_print!("[orbitnet]   rx_unauthenticated={}", self.dbg_rx_unauth);
                }
                godot_print!(
                    "[orbitnet]   input_novel={} input_nonfinite={} resim_spans={} resim_ticks={} fresh={}",
                    self.dbg_input_novel,
                    self.dbg_input_nonfinite,
                    self.dbg_resim_spans,
                    self.dbg_resim_ticks_total,
                    self.dbg_fresh
                );
                self.dbg_input_novel = 0;
                self.dbg_input_nonfinite = 0;
                self.dbg_resim_spans = 0;
                self.dbg_resim_ticks_total = 0;
                self.dbg_fresh = 0;
                self.dbg_rx_kinds = [0; 8];
                self.dbg_sent = 0;
                self.dbg_sent_bytes = 0;
                self.dbg_rx_applied = 0;
                self.dbg_rx_rejected = 0;
                self.dbg_rx_skipped = 0;
                self.dbg_rx_unauth = 0;
            }
        }
    }

    /// Divide the window's raw counters into the published per-second figures, then reset them.
    fn publish_bandwidth(&mut self, window: f64) {
        let per_second = 1.0 / window.max(1.0e-6);
        let ticks = self.acc_interest_ticks.max(1) as f64;
        let band = |sends: u64, members: u64| -> f64 {
            // Mean ticks between admissions for one band: over the window the band offered
            // `members` entity-ticks of candidacy and `sends` of them were admitted, so the mean
            // gap is their ratio. Zero sends means the band was never admitted at all, which is
            // reported as 0.0 rather than infinity — the count next to it says which it was.
            if sends == 0 {
                0.0
            } else {
                members as f64 / sends as f64
            }
        };
        self.m_bw = BandwidthMetrics {
            tx_bytes_s: self.acc_tx_bytes as f64 * per_second,
            tx_datagrams_s: self.acc_tx_datagrams as f64 * per_second,
            tx_wire_bytes_s: (self.acc_tx_bytes + self.acc_tx_datagrams * WIRE_OVERHEAD_BYTES)
                as f64
                * per_second,
            tx_peak_peer_bytes_s: self.win_peer_bytes.values().copied().max().unwrap_or(0) as f64
                * per_second,
            rx_bytes_s: self.acc_rx_bytes as f64 * per_second,
            rx_datagrams_s: self.acc_rx_datagrams as f64 * per_second,
            blocks_admitted_s: self.acc_blocks_admitted as f64 * per_second,
            blocks_deferred_s: self.acc_blocks_deferred as f64 * per_second,
            blocks_culled_s: self.acc_blocks_culled as f64 * per_second,
            blocks_oversize_s: self.acc_blocks_oversize as f64 * per_second,
            blocks_full_s: self.acc_blocks_full as f64 * per_second,
            want_full_nacks_s: self.acc_want_full_nacks as f64 * per_second,
            unproven_acks_s: self.acc_unproven_acks as f64 * per_second,
            stale_blocks_s: self.acc_stale_blocks as f64 * per_second,
            starve_ticks_max: self.win_starve_ticks_max as f64,
            unsent_backlog_max: self.win_unsent_backlog_max as f64,
            interest_ms: self.acc_interest_us as f64 / ticks / 1000.0,
            interest_grid: self.acc_interest_grid_ticks as f64 / ticks,
            interarrival_near: band(self.acc_band_sends[0], self.acc_band_members[0]),
            interarrival_mid: band(self.acc_band_sends[1], self.acc_band_members[1]),
            interarrival_far: band(self.acc_band_sends[2], self.acc_band_members[2]),
            interarrival_all: band(
                self.acc_band_sends[0] + self.acc_band_sends[1] + self.acc_band_sends[2],
                self.acc_band_members[0] + self.acc_band_members[1] + self.acc_band_members[2],
            ),
            peers: self.peers.values().filter(|p| p.synced).count() as f64,
            interest_entities: self.acc_interest_members as f64
                / self.acc_interest_peer_ticks.max(1) as f64,
            // Once a second, here, rather than in `peer_rtt_ms`: it walks every connected peer's
            // sample window, and nothing acts on the answer. See `rtt_at_ceiling_peers`.
            rtt_at_ceiling_peers: rtt_at_ceiling_peers(
                self.peers.values(),
                self.rtt_believed_max_ms as f32,
            ) as f64,
        };
        // Cleared and refilled rather than updated in place: a peer that disconnected during the
        // window has no entry in the accumulator, and must not keep answering with the figure it
        // last earned.
        self.m_peer_interarrival.clear();
        for (&peer, &(sends, members)) in &self.acc_peer_band {
            self.m_peer_interarrival.insert(peer, band(sends, members));
        }
        self.reset_bandwidth_counters();
    }

    /// Zero every raw send-path counter. Called at each window boundary, and at session teardown so a new
    /// session's first window cannot inherit the tail of the previous one's traffic.
    fn reset_bandwidth_counters(&mut self) {
        self.acc_tx_bytes = 0;
        self.acc_tx_datagrams = 0;
        self.acc_rx_bytes = 0;
        self.acc_rx_datagrams = 0;
        self.acc_blocks_admitted = 0;
        self.acc_blocks_deferred = 0;
        self.acc_blocks_culled = 0;
        self.acc_blocks_oversize = 0;
        self.acc_blocks_full = 0;
        self.acc_want_full_nacks = 0;
        self.acc_unproven_acks = 0;
        self.acc_stale_blocks = 0;
        self.acc_interest_us = 0;
        self.acc_interest_ticks = 0;
        self.acc_interest_grid_ticks = 0;
        self.acc_interest_peer_ticks = 0;
        self.acc_interest_members = 0;
        self.acc_band_sends = [0; 3];
        self.acc_band_members = [0; 3];
        self.acc_peer_band.clear();
        self.win_peer_bytes.clear();
        self.win_starve_ticks_max = 0;
        self.win_unsent_backlog_max = 0;
    }

    fn send_ping(&mut self) {
        self.ping_seq += 1;
        let ping = Ping {
            seq: self.ping_seq,
            client_us: Time::singleton().get_ticks_usec(),
        };
        self.send_to(SERVER_PEER, &ping.encode(), TransferMode::UNRELIABLE);
    }

    // ------------------------------------------------------------------
    // Send path
    // ------------------------------------------------------------------

    fn flush_network(&mut self, current: u64) {
        if self.mode == MODE_OFFLINE || !self.has_live_peer() {
            return;
        }
        let started = Instant::now();
        match self.mode {
            MODE_CLIENT => self.send_client_input(current),
            MODE_SERVER | MODE_HOST => {
                // Before the manifest, because the manifest is what publishes the table.
                self.reconcile_slots(current);
                self.send_manifest_if_dirty();
                self.send_snapshots(current);
            }
            _ => {}
        }
        self.m_net_ms = started.elapsed().as_secs_f64() * 1000.0;
    }

    /// Build and send this client's input frame: one block per owned body, bounded to one datagram.
    ///
    /// **THE FRAME IS BOUNDED, AND THE BOUND IS WHY THE ROTA EXISTS.** One body per connection made
    /// the size question moot; several — local split-screen, two players behind one socket — do not,
    /// because each carries [`INPUT_REDUNDANCY`] rows per frame and the sum has no reason to fit.
    /// Past the path MTU an unreliable datagram fragments, and losing one fragment loses the whole
    /// frame, which is the input lane of every seat on the connection rather than of one.
    ///
    /// So the frame is capped at [`MAX_FRAME_PAYLOAD`] — the same ceiling the snapshot path spends,
    /// and the header rides above it there too — and what does not fit is carried on a later tick.
    /// `input_rotor` is where the next walk starts, so a body refused this tick is offered first
    /// next tick and no body can be starved by the ones that sort ahead of it.
    ///
    /// **What a deferred body costs, and why it is small.** A block carries the last
    /// `INPUT_REDUNDANCY` ticks of that body's input, so a body skipped for up to three consecutive
    /// frames loses nothing at all: the next frame it appears in re-sends every tick it missed. Past
    /// that the oldest ticks fall out of the redundancy window and the server extrapolates them, the
    /// same as it does for a lost datagram. That is the real bound on how many seats a connection
    /// can carry, and it is a function of the input schema's width rather than a constant worth
    /// declaring.
    ///
    /// **A single block larger than the whole payload is refused every tick, and starves nobody
    /// else.** The walk continues past it rather than stopping, and the rotor stays parked on it, so
    /// the other seats are admitted normally. Such a block cannot be sent at all — no rota fixes an
    /// input row wider than a datagram — and the fix is the schema, not the send path.
    fn send_client_input(&mut self, current: u64) {
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        for sync in self.rollback_entities.values() {
            let Some(sync) = live_handle(sync) else {
                continue;
            };
            let bound = sync.bind();
            if !bound.owns_input() {
                continue;
            }
            // A client cannot derive a slot; the server assigns it and the manifest carries it. A
            // body whose binding has not arrived yet sends nothing this tick — the block would name
            // an entity the server could not resolve, and input rides `INPUT_REDUNDANCY` ticks of
            // history, so the first block after the binding lands re-sends what these ticks held.
            let Some(slot) = self.slots.slot_of(bound.entity_id()) else {
                continue;
            };
            if let Some(bytes) = bound.encode_input_block_bytes(slot, current, INPUT_REDUNDANCY) {
                blocks.push(bytes);
            }
        }
        if !input_frame_is_owed(
            !blocks.is_empty(),
            self.snapshot_unacked,
            self.want_full,
            self.want_manifest,
            self.want_interest,
        ) {
            return;
        }
        self.snapshot_unacked = false;

        let mut carried: Vec<usize> = Vec::with_capacity(blocks.len());
        let lengths: Vec<usize> = blocks.iter().map(Vec::len).collect();
        self.input_rotor =
            admit_input_blocks(&lengths, self.input_rotor, MAX_FRAME_PAYLOAD, &mut carried);
        let payload: usize = carried.iter().map(|&index| lengths[index]).sum();

        let mut writer = Writer::with_capacity(payload + 64);
        let header = FrameHeader {
            kind: FrameKind::ClientInput,
            tick: u32::try_from(current).unwrap_or(u32::MAX),
            ack_tick: u32::try_from(self.newest_snapshot_tick).unwrap_or(u32::MAX),
            ack_bits: self.snapshot_ack_bits,
            ack_token: self.snapshot_ack_token,
            margin_ticks: 0,
            // All three client-to-server NACKs ride the same byte, on a frame this peer is sending
            // anyway, so none costs a frame kind or a byte of its own. They are independent: a broken
            // delta base, a broken manifest and a broken interest set have nothing to do with each
            // other, and a client can be owed all three on one tick.
            flags: (u8::from(self.want_full) * FrameHeader::FLAG_WANT_FULL)
                | (u8::from(self.want_manifest) * FrameHeader::FLAG_WANT_MANIFEST)
                | (u8::from(self.want_interest) * FrameHeader::FLAG_WANT_INTEREST),
            entity_count: carried.len() as u32,
        };
        header.encode(&mut writer);
        for &index in &carried {
            writer.bytes(&blocks[index]);
        }
        self.want_full = false;
        self.want_manifest = false;
        self.send_to(SERVER_PEER, writer.as_slice(), TransferMode::UNRELIABLE);
    }

    /// SERVER: publish what has CHANGED in the slot table, with each entity's schema fingerprints,
    /// to every synced peer.
    ///
    /// **Both lanes.** It carried rollback entities only while it was purely a schema check — a
    /// state-lane entity has no input schema to disagree about — but it is now also the only channel
    /// that says what a wire slot names, and state-lane blocks carry slots too.
    ///
    /// **The table is rebuilt in full and then DIFFED, rather than sent in full.** The rebuild is
    /// unchanged and still runs on every dirty flush; what changed is what goes on the wire. A row
    /// costs ~22.5 bytes ([`ManifestEntry`]) and this frame is dirtied by a registration, an
    /// unregistration, a slot reconcile, a seat or authority write and every hello — so the old
    /// ceiling was one whole-table broadcast per net tick per peer, which at 8,000 named entities is
    /// ~180 kB per peer per republish against an unreliable hot lane of ~36 kB/s.
    ///
    /// **A rebuild that reproduces the published table publishes NOTHING.** That alone deletes the
    /// whole-table broadcast a single join used to cost every peer already in the session.
    ///
    /// **What a delta gives up, and what replaces it.** A complete table was self-repairing: a
    /// receiver rebuilt from it and thereby dropped every binding that had gone away, with no
    /// removal record to lose. A delta reintroduces that record, and a receiver that misses one
    /// keeps a slot bound past its unregister — past the reuse quarantine that slot names a
    /// different entity and the stale receiver applies the new entity's rows to the old one,
    /// silently. Three things stand in for the rebuild, and all three are needed:
    ///
    /// | Guarantee | What it covers |
    /// | --- | --- |
    /// | the channel is **reliable and ordered** ([`TransferMode::RELIABLE`] on one channel) | a removal cannot be dropped or reordered while the connection lives |
    /// | a delta names the **base generation** it was computed against | a peer holding any other table is sent the whole table instead |
    /// | every path that can desynchronize a peer **zeroes its generation** | see [`PeerState::manifest_generation`] |
    ///
    /// **The seat columns come from the ANNOUNCED table, not from a live read.** `entity_seats` is
    /// what [`Self::announce_seats`] emitted from at the top of this frame, and a handler it woke may
    /// have re-seated a body since. Publishing the announced values is what keeps a client's roster
    /// equal to the server's rather than one frame ahead of it in places.
    fn send_manifest_if_dirty(&mut self) {
        if !self.manifest_dirty {
            return;
        }
        self.manifest_dirty = false;
        let mut entries: Vec<ManifestEntry> = Vec::new();
        for (slot, id) in self.slots.bindings() {
            if let Some(sync) = self.rollback_entities.get(&id) {
                if !sync.is_instance_valid() {
                    continue;
                }
                let (owner, seat) = self
                    .entity_seats
                    .binary_search_by_key(&id, |&(seated, _, _)| seated)
                    .map_or((0, 0), |index| {
                        let (_, owner, seat) = self.entity_seats[index];
                        (owner, seat)
                    });
                let bound = sync.bind();
                entries.push(ManifestEntry {
                    slot,
                    id,
                    state_hash: bound.schema_hash() as u32,
                    input_hash: bound.input_schema_hash() as u32,
                    owner,
                    seat,
                });
            } else if let Some(sync) = self.state_entities.get(&id) {
                if !sync.is_instance_valid() {
                    continue;
                }
                let bound = sync.bind();
                entries.push(ManifestEntry {
                    slot,
                    id,
                    state_hash: bound.schema_hash() as u32,
                    // A state-lane entity has no input schema, so `0` is the declared absence
                    // rather than a hash. Nothing compares it: `check_expected_schema` runs against
                    // rollback synchronizers only, and a state entity's id can never reach one —
                    // the two lanes salt the node path differently (`S|` against `R|`).
                    input_hash: 0,
                    // The state lane has no input authority, so it drives no seat. `owner == 0` is
                    // what every reader keys on and the label beside it is never consulted.
                    owner: 0,
                    seat: 0,
                });
            }
        }
        // ASCENDING BY SLOT, which is what the rebuild loop above does not give: it walks
        // `bindings()`, and that is ascending by id. The slot is the key of the whole table — a
        // removal names one and nothing else — so both the diff and the receiver's copy are held in
        // that order.
        entries.sort_unstable_by_key(|entry| entry.slot);

        let base_generation = self.manifest_generation;
        let (removed, added) = diff_manifest(&self.manifest_published, &entries);
        if !removed.is_empty() || !added.is_empty() {
            // Saturating rather than wrapping, and it is unreachable either way: one bump per net
            // tick reaches `u64::MAX` in longer than the universe has run. A wrap would land the
            // table on a generation some peer already believes it holds, which is the one outcome
            // that misapplies silently; a saturation degrades every peer to full tables instead.
            self.manifest_generation = base_generation.saturating_add(1);
            self.manifest_published = entries;
        }
        let generation = self.manifest_generation;

        // What each peer is owed, decided before a byte is encoded: a session where nothing changed
        // and every peer is current encodes nothing at all.
        let held: Vec<(i32, u64)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.synced)
            .map(|(&id, p)| (id, p.manifest_generation))
            .collect();
        let owed =
            |peer_generation: u64| manifest_owed(peer_generation, base_generation, generation);
        let delta_bytes = held
            .iter()
            .any(|&(_, at)| owed(at) == ManifestOwed::Delta)
            .then(|| {
                encode_manifest_delta(&ManifestDelta {
                    base_generation,
                    generation,
                    removed,
                    added,
                })
            });
        let full_bytes = held
            .iter()
            .any(|&(_, at)| owed(at) == ManifestOwed::Full)
            .then(|| encode_manifest_full(generation, &self.manifest_published));

        for (peer, at) in held {
            // ONE delta, encoded once and sent to every peer that can apply it; the full table is
            // addressed to the peers that cannot, which is a joiner and a peer that asked.
            let bytes = match owed(at) {
                ManifestOwed::Nothing => continue,
                ManifestOwed::Delta => delta_bytes.as_ref(),
                ManifestOwed::Full => full_bytes.as_ref(),
            };
            // Unreachable — `owed` answering `Delta` is what caused the delta to be encoded, and the
            // same for `Full` — and it advances no generation if it ever is. What this peer is
            // believed to hold may only move on a frame that was actually written for it.
            let Some(bytes) = bytes else {
                continue;
            };
            self.send_to(peer, bytes, TransferMode::RELIABLE);
            if let Some(state) = self.peers.get_mut(&peer) {
                state.manifest_generation = generation;
            }
        }
    }

    /// SERVER: send the whole interest set to every connection owed one, and bump its generation.
    ///
    /// **THE REPAIR PATH FOR RELEVANCY, AND THE MIRROR OF `send_manifest_if_dirty`.** Four things
    /// owe a connection a whole set, and none of them can be answered by another delta:
    ///
    /// | Cause | Who noticed |
    /// | --- | --- |
    /// | a pending half overflowed [`INTEREST_DELTA_PENDING_MAX`] | the server, in `push_pending` |
    /// | a prefix was given up on unacknowledged | the server, in `retire_interest_delta` |
    /// | a connection rekeyed on a live session | the server, in `handle_hello` |
    /// | a section the client could not name or could not place | the client, via [`FrameHeader::FLAG_WANT_INTEREST`] |
    ///
    /// Before this, all three were silent and permanent: the peer's mirror stayed short of an entity
    /// whose rows kept arriving, `entity_entered_interest` never fired for it, and the documented
    /// repair answered that client out of the same broken mirror.
    ///
    /// **It runs AFTER the interest pass and before the frames**, which is what makes the set it
    /// states the set this tick computed. The manifest binding these slots went out earlier in the
    /// same flush, on the reliable channel this rides.
    ///
    /// **A SECTION IS PLACED EXACTLY, NOT APPROXIMATELY.** A table is reliable and a section is not,
    /// so a section built after a table can reach the client first. It is stamped with the
    /// generation it was built against and the client applies it only at that exact generation —
    /// anything else is a section whose baseline the client is not holding, which it drops and asks
    /// again for. Re-sends of a prefix carry the generation they were built at, so the
    /// re-send-until-acked model is untouched.
    ///
    /// The pending halves are cleared rather than sent: every entry in them is a transition into or
    /// out of the set this frame states outright.
    fn send_interest_tables(&mut self) {
        if self.mode == MODE_CLIENT {
            return;
        }
        let owed: Vec<i32> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.synced && peer.interest_full_due)
            .map(|(&id, _)| id)
            .collect();
        if owed.is_empty() {
            return;
        }
        let mut frames: Vec<(i32, Vec<u8>)> = Vec::with_capacity(owed.len());
        for peer_id in owed {
            let Some(peer) = self.peers.get_mut(&peer_id) else {
                continue;
            };
            let (generation, slots) = state_whole_interest_set(&self.slots, peer);
            frames.push((peer_id, encode_interest_table(generation, &slots)));
        }
        for (peer_id, bytes) in frames {
            self.send_to(peer_id, &bytes, TransferMode::RELIABLE);
        }
    }

    /// Build and send one snapshot frame per synced peer.
    ///
    /// The shape, in order: **gather once, cull, order, admit.** Each step exists because the
    /// step before it was the wrong place to pay:
    ///
    /// 1. [`Self::collect_entity_rows`] asks every entity for its owner and anchor **once per
    ///    tick**, not once per entity per peer.
    /// 2. [`Self::update_interest`] runs the hysteretic filter over both lanes and clears the delta
    ///    bookkeeping of everything that left.
    /// 3. The send order is built from the **surviving** set — so it is `O(peers · K log K)` in the
    ///    interest size rather than `O(peers · N log N)` over the whole registry, which is where the
    ///    real per-peer cost lived. Ordering ahead of the cull bought bandwidth and no CPU.
    /// 4. Admission spends the byte budget down the ordered list.
    fn send_snapshots(&mut self, current: u64) {
        let budget = self.effective_send_budget();
        let peer_ids: Vec<i32> = self
            .peers
            .iter()
            .filter(|(_, p)| p.synced)
            .map(|(&id, _)| id)
            .collect();
        if peer_ids.is_empty() {
            return;
        }

        // WHETHER ANYTHING CAN BE CULLED AT ALL, decided once — and it is THE SAME QUESTION
        // `update_interest` asks per peer, so the two must give the same answer.
        //
        // This read `aoi_radius > 0.0 || aoi_max_entities > 0` while the inner one reads
        // `enter_radius > 0.0 && center.is_some()`, and the disagreement had a cost with nothing to
        // show for it: with a cap set and no radius, every peer built its full candidate list and
        // rebuilt its `PeerInterest` every tick, and every candidate went in as `always` — pushed at
        // `NEG_INFINITY` precisely so the nearest-N cap can never evict it. So the cap culled
        // nothing and was billed for the whole apparatus. `aoi_max_entities` is a cap WITHIN a
        // radius, not a substitute for one; `net.aoi_max_entities`'s own help says so now.
        //
        // With a long cull radius this is TRUE in a shipped session — the price is real and is
        // reported as `interest_ms`. What it buys is the band split the priority scorer needs, even
        // on a map where nothing is actually culled (`blocks_culled_s` measures 0.00).
        // Either path clears and refills a `BTreeMap` per peer per tick, and a host that
        // overruns its net tick is how "rubber banding, sticky input, hits stop landing" arrives all
        // at once, so watch that column.
        let culling = self.aoi_radius > 0.0;

        let interest_started = Instant::now();
        let mut rows = std::mem::take(&mut self.aoi_rows);
        let mut observers = std::mem::take(&mut self.aoi_observers);
        self.collect_entity_rows(&mut rows);
        // Ascending by id, so the per-peer walk over an (ascending) interest set can binary-search
        // back to the row rather than carrying a per-tick map — and so the anchor pick below is a
        // fact about the scene rather than about `HashMap` iteration order.
        rows.sort_unstable_by_key(|row| row.id);
        // WHETHER THE FILTER CAN REFUSE ANYTHING AT ALL — the gate the comment above is about, now
        // that there are two levers rather than one. Membership is not a radius: a game that
        // separates worlds and sets no `aoi_radius` still needs the pass to run, because refusing an
        // overlapping world is the only culling it asked for. Answered from the gathered rows rather
        // than from a config value, since a membership is a per-entity declaration and no setting
        // announces that any entity made one.
        //
        // READING LIVE VALUES RATHER THAN DECLARATIONS IS SAFE HERE, AND IT IS NOT OBVIOUS. This can
        // flip to `false` on a tick where every membership happens to read `MEMBERSHIP_GLOBAL` —
        // several entities sharing one world-id node, say, on the tick that node is freed. It leaks
        // nothing, because the observer's membership is read from a row by the same rule: if every
        // row is GLOBAL then the observer is GLOBAL, `membership_matches` is true for every pair,
        // and running the pass would refuse exactly nothing. The two branches produce the same set.
        // What that tick loses is one update of `peer.interest` — which is not read on that tick
        // either, and whose next update diffs against it and emits the leaves as usual.
        // A STANDING VETO TURNS THE PASS ON BY ITSELF. `set_entity_hidden` is enforced inside
        // `PeerInterest::classify`, which only the pass reaches — so without this term the veto did
        // nothing at all in the configuration it exists for: no radius, no declared membership, the
        // one case where a per-(peer, entity) refusal is the only lever a game has. It is a cheap
        // standing count rather than a scan, and a session that vetoes nothing pays one `bool` per
        // connection per flush and takes the same branch it always did.
        // SYNCED ONLY. `set_entity_hidden` creates a connection's state on demand, because a veto
        // may be declared before that peer finishes its handshake — and a veto declared for one that
        // has already gone leaves a state nothing removes again. Counting only live connections is
        // what stops such a record pinning the pass on for the rest of the session.
        let vetoing = self
            .peers
            .values()
            .any(|peer| peer.synced && peer.interest.hidden_len() > 0);
        // ONCE A SESSION FILTERS IT KEEPS FILTERING. Every client that has received a section holds
        // a mirrored set and answers `entities_in_interest` out of it; a session that switched the
        // pass back off — by retracting its last veto, or by unregistering its last non-global row —
        // would leave every one of those mirrors frozen at the last thing it was told while the
        // server went back to answering "everything is in interest". The pass is cheap on a session
        // with nothing to refuse; a mirror that silently stops tracking is not.
        let filtering = session_is_filtering(
            culling,
            vetoing,
            self.interest_ran,
            rows.iter().any(|row| row.membership != MEMBERSHIP_GLOBAL),
        );
        if filtering {
            Self::collect_observers(&rows, &mut observers);
            self.warn_anchor_conflicts(&observers);
            self.update_interest(&peer_ids, &rows, &observers);
        }
        // What every anchor read-back is gated on. A pass that did not run left each connection's
        // `AnchorReport` describing an earlier tick, and reporting that as current would state a
        // center and a world for a session that is culling nothing and filtering nobody.
        self.interest_ran = filtering;
        self.acc_interest_us += interest_started.elapsed().as_micros() as u64;
        self.acc_interest_ticks += 1;
        // AFTER THE PASS, so the set a table states is the set this tick actually computed. Built
        // before it, a table described the previous tick while the section built later in this same
        // flush carried the same generation — two frames disagreeing under one stamp, on channels
        // with no ordering between them. It also clears the pending halves, so the peer it answers
        // sends no section this tick: the table already says where every entity stands.
        if filtering {
            self.send_interest_tables();
        }

        // The cull radius is applied by `update_interest` above, which is the only thing that
        // decides membership; nothing down here re-derives a band from it. See `priority::band_of`:
        // the radius is sized by the longest shot in the game, the band scale by the distances a
        // firefight happens over, and reusing one for the other is what made this scorer inert.
        let band_scale = self.aoi_band_radius as f32;
        let tiering = self.rate_tiering;
        let mut order = std::mem::take(&mut self.order_scratch);
        let mut members = std::mem::take(&mut self.aoi_members);
        // The section's wire slots, pooled: one connection's worth at a time, refilled per peer.
        let mut delta_left = std::mem::take(&mut self.delta_left_scratch);
        let mut delta_entered = std::mem::take(&mut self.delta_entered_scratch);
        // The receiver's own retention, in ticks, and THE SHORTER OF THE TWO LANES holds for both.
        // A rollback receiver keeps its bases in `auth_rows`, sized from the same `history_limit`
        // both ends read out of the `[orbitnet]` block. A state receiver keeps them in `history`,
        // sized from the fixed `STATE_HISTORY_DEPTH` instead -- which does NOT track
        // `history_limit`, and at the default 128 is half of it. Taking the minimum spends a few
        // more full rows on the rollback lane and is correct on both; taking `history_limit` alone
        // would leave the state lane, the fatter one on the wire, doing exactly what this guard
        // exists to stop.
        let base_span = (self.history_limit.max(2) as u64).min(STATE_HISTORY_DEPTH as u64);

        for peer_id in peer_ids {
            let (want_full, ack_tick, ack_token, margin) = {
                let Some(peer) = self.peers.get(&peer_id) else {
                    continue; // disconnected while an earlier peer's frame was going out
                };
                (
                    peer.want_full,
                    u32::try_from(peer.newest_input_tick.max(0)).unwrap_or(u32::MAX),
                    // What this peer must quote back to have its ack of this frame believed.
                    peer.frame_token(current).unwrap_or(0),
                    peer.margin_last,
                )
            };

            // --- order, over the surviving set only ---
            //
            // With the filter off there IS no surviving set — every row is a candidate, at a
            // distance no radius will be compared against, which `band_of` reports as `Near` for all
            // of them. Reading `peer.interest` there would read a structure nothing has maintained.
            //
            // The gate is `filtering`, not `culling`: with memberships declared and no radius, the
            // pass DID run and `peer.interest` is exactly the set that survived it. `band_for_row`
            // below still takes `culling`, because a membership refusal produces no distance and so
            // no band — every surviving row takes the one constant weight, as it does with the
            // radius off.
            members.clear();
            if filtering {
                let Some(peer) = self.peers.get(&peer_id) else {
                    continue;
                };
                members.extend(peer.interest.iter_with_distance());
            } else {
                members.extend(rows.iter().map(|row| (row.id, 0.0f32)));
            }

            order.clear();
            let mut starve_max = 0u64;
            let mut unsent = 0u64;
            // This peer's own share of the two band counters, folded into `acc_peer_band` once the
            // frame is built. The band arrays beside them stay global.
            let mut peer_members = 0u64;
            let mut peer_sends = 0u64;
            {
                let Some(peer) = self.peers.get(&peer_id) else {
                    continue;
                };
                for &(id, dist_sq) in members.iter() {
                    let Ok(index) = rows.binary_search_by_key(&id, |row| row.id) else {
                        continue; // despawned between the gather and here
                    };
                    let row = &rows[index];
                    // AN ENTITY WITH NO ANCHOR HAS NO DISTANCE, AND MUST NOT COLLECT A DISTANCE BOOST.
                    //
                    // `PeerInterest` stores always-relevant members at `0.0` (they are pushed at
                    // `NEG_INFINITY` so the nearest-N cap can never evict them, then normalized), and
                    // `band_of` reads `0.0` as `Near`. Typically only a handful of channels declare an
                    // anchor — the ones that carry a position — while every other state channel a body owns
                    // (its health, its equipment, its sensors, its lights, the doors around it) does not. Those
                    // would all be scored as though they were in the viewer's face. At four-plus such channels
                    // per body against the ONE anchored row that says where that body is, a distant player's
                    // flashlight and hit points outbid their position 4:1 under budget pressure. That is remote-body
                    // stutter by construction.
                    //
                    // `Far` rather than a middle band: "always relevant" is a statement about never being
                    // culled, and says nothing about priority. Unanchored channels are on-change, so staleness
                    // carries them the moment they have something to say, and `score = staleness x weight`
                    // makes starvation impossible by construction whatever the weight is.
                    let band = band_for_row(culling, row.anchor.is_some(), dist_sq, band_scale);
                    let last_sent = peer.last_sent.get(&id).copied().unwrap_or(0);
                    // Never sent: sorts ahead of everything already sent, which is what a re-entrant
                    // entity needs — and why clearing `last_sent` at the leave is the whole of the
                    // re-entry fix. NOT `u64::MAX`: that saturates the product and cancels the weight,
                    // so a join burst ordered by node-path hash. See `priority::NEVER_SENT_STALENESS`.
                    let staleness = if last_sent == 0 {
                        unsent += 1;
                        priority::NEVER_SENT_STALENESS
                    } else {
                        let age = current.saturating_sub(last_sent);
                        starve_max = starve_max.max(age);
                        age
                    };
                    let weight = priority::weight_for(band, row.priority, row.owner == peer_id);
                    self.acc_band_members[band.index()] += 1;
                    peer_members += 1;
                    order.push((
                        priority::Candidate {
                            id,
                            staleness,
                            weight,
                        },
                        band,
                    ));
                }
                // The candidate set as this tick actually used it, so the published figure is the
                // number the order loop walked rather than the state of a structure that may not
                // have been maintained.
                self.acc_interest_members += members.len() as u64;
                self.acc_interest_peer_ticks += 1;
            }
            self.win_starve_ticks_max = self.win_starve_ticks_max.max(starve_max);
            self.win_unsent_backlog_max = self.win_unsent_backlog_max.max(unsent);

            // Descending score, ties by ascending id — and it CALLS `priority::cmp` rather than restating it.
            // The pairs carry the band through the sort without a parallel array, which is why this cannot use
            // `priority::order`; writing the comparison out again left two copies of the shipping rule, and the
            // tests could only ever reach the other one.
            order.sort_unstable_by(|a, b| priority::cmp(&a.0, &b.0));

            // --- the trailing interest-delta section, decided BEFORE the admit loop ---
            //
            // Its bytes come off the budget the loop is about to spend, because a section appended
            // to a frame already filled to `MAX_FRAME_PAYLOAD` is a datagram past the path MTU. The
            // ack that retires it is the ordinary one every frame already carries and proves.
            let (carries_delta, interest_generation) = {
                let Some(peer) = self.peers.get_mut(&peer_id) else {
                    continue;
                };
                // READ BESIDE THE SECTION IT STAMPS, not at the encode below: a whole set sent later
                // in this same flush would bump it, and the section would then claim a generation it
                // was not built against.
                let generation = peer.interest_generation;
                (
                    build_interest_section(
                        &self.slots,
                        peer,
                        filtering,
                        current,
                        &mut delta_left,
                        &mut delta_entered,
                    ),
                    generation,
                )
            };
            let admit_budget = budget.saturating_sub(interest_delta_reserve(
                delta_left.len() + delta_entered.len(),
            ));

            // --- admit ---
            //
            // THE BUDGET BOUNDS THE BODY, AND THE DATAGRAM IS THE BODY PLUS THE FRAME HEADER. `send_budget`
            // clamps to `MAX_FRAME_PAYLOAD` (1200) and every check below is against `body.len()`, so a full
            // frame leaves here at 1200 plus the header's own bytes -- not at 1200. That is deliberate rather
            // than an oversight, but it is not what the constant's name says: the real wire figure is header +
            // body + 12 (ENet) + 28 (IPv4/UDP), which stays comfortably inside a 1500 B path MTU. Do not read
            // `MAX_FRAME_PAYLOAD` as "the datagram size"; read it as "the entity payload one frame may carry".
            let mut writer = Writer::with_capacity(budget + 256);
            let mut body = Writer::with_capacity(budget);
            let mut sent: Vec<(u64, u64)> = Vec::new();
            // The subset of `sent` that went out full, so the keyframe clock is measured against
            // what repairs a chain. Kept beside `sent` rather than widening it, because `sent` is
            // moved into the ack log verbatim.
            let mut sent_full: Vec<(u64, u64)> = Vec::new();

            for index in 0..order.len() {
                let (candidate, band) = order[index];
                if body.len() >= admit_budget {
                    // Everything left wanted to go out and did not fit: budget pressure, which is
                    // a different fact from a cull and is counted as one.
                    self.acc_blocks_deferred += (order.len() - index) as u64;
                    break;
                }
                let id = candidate.id;
                // The wire name for this entity. Missing only while `reconcile_slots` is holding
                // the entity back — a slot still inside its predecessor's reuse quarantine — which
                // is a delay this entity's next tick resolves, so it counts as deferred.
                let Some(slot) = self.slots.slot_of(id) else {
                    self.acc_blocks_deferred += 1;
                    continue;
                };
                // Rate tiering is a deliberate hold-back, so it counts as culled, not deferred.
                // **It phases on the 64-bit id, not the wire slot**, and so does `full_block_due`
                // below. Either value spreads a set of entities across an interval — dense
                // sequential slots spread more evenly than hashes do, which
                // `send_phase_spreads_dense_sequential_indices` pins — but only the id is STABLE.
                // A slot is released and reissued, so an entity that took a different slot would
                // jump its tier phase and its keyframe phase with it, restarting the interval it
                // was part-way through.
                if tiering
                    && !orbitnet_core::interest::send_phase(id, current, band.tiered_interval())
                {
                    self.acc_blocks_culled += 1;
                    continue;
                }
                let last_full = self
                    .peers
                    .get(&peer_id)
                    .and_then(|p| p.last_full.get(&id))
                    .copied()
                    .unwrap_or(0);
                let full_due =
                    full_block_due(want_full, id, current, last_full, FULL_STATE_INTERVAL);
                // Masked deltas reference only CLIENT-ACKED ticks: the peer provably applied
                // that base, so loss can no longer leave it reconstructing against its own
                // prediction. No acked base yet (or an evicted row) degrades to a full block.
                let reference = if full_due {
                    None
                } else {
                    self.peers
                        .get(&peer_id)
                        .and_then(|p| p.acked_base.get(&id))
                        .copied()
                        .and_then(|base| delta_reference(base, current, base_span))
                };

                // An entity block's encoded size is not known until it is written, so the budget can only
                // be enforced by writing and un-writing. The pre-check above admits an entity whenever the
                // body is at `budget - 1`, and the block that follows can be any size -- which is how a frame
                // capped at MAX_FRAME_PAYLOAD went out at 1456 bytes and drew ENet's over-MTU warning. An
                // unreliable datagram past the path MTU fragments, and a lost fragment loses the whole frame.
                let body_before = body.len();
                let tick_sent = if let Some(sync) = self.rollback_entities.get(&id) {
                    let Some(mut sync) = live_handle(sync) else {
                        continue;
                    };
                    let tick = sync.bind_mut().encode_block(
                        &mut body,
                        &mut self.mask_scratch,
                        slot,
                        current,
                        reference,
                    );
                    tick
                } else if let Some(sync) = self.state_entities.get(&id) {
                    let Some(mut sync) = live_handle(sync) else {
                        continue;
                    };
                    let tick = sync.bind_mut().encode_block(
                        &mut body,
                        &mut self.mask_scratch,
                        slot,
                        current,
                        reference,
                    );
                    tick
                } else {
                    None
                };
                if body.len() > admit_budget {
                    // IT DID NOT FIT. Deferring is right whenever the frame already carries something --
                    // but if it carries NOTHING, deferring this block sends no frame at all, and that is
                    // not a delay, it is the end of the stream. An entity that has never been sent scores
                    // `u64::MAX` staleness, so it is first again next tick, does not fit again, and defers
                    // again: this peer never receives another snapshot for the rest of the session, for
                    // every entity, silently. (The first implementation had no un-write at all -- an oversized
                    // block simply went out, which is where ENet's over-MTU warning came from.)
                    //
                    // So the frame carries it anyway. One datagram past the path MTU fragments and a lost
                    // fragment costs that frame; a wedged peer costs the session. The condition is counted
                    // rather than swallowed, because "one entity's full state does not fit in a datagram"
                    // is a fact about the schema that somebody has to be told.
                    if sent.is_empty() {
                        if let Some((tick, was_full)) = tick_sent {
                            self.acc_blocks_oversize += 1;
                            sent.push((id, tick));
                            if was_full {
                                sent_full.push((id, tick));
                            }
                            self.acc_band_sends[band.index()] += 1;
                            peer_sends += 1;
                            self.acc_blocks_deferred += (order.len() - index - 1) as u64;
                            break;
                        }
                        // No tick came back, so there is no row to admit. Nothing was written either
                        // (an entity in neither map writes no bytes), so this is unreachable in
                        // practice -- but if it happens, drop it and let the rest of the order run
                        // rather than ending the frame on an entity that contributed nothing.
                        body.truncate(body_before);
                        continue;
                    }
                    body.truncate(body_before);
                    self.acc_blocks_deferred += (order.len() - index) as u64;
                    break;
                }
                if let Some((tick, was_full)) = tick_sent {
                    sent.push((id, tick));
                    if was_full {
                        sent_full.push((id, tick));
                    }
                    self.acc_band_sends[band.index()] += 1;
                    peer_sends += 1;
                }
            }

            // Folded in before the empty-frame `continue` below. A tick that offered this peer
            // candidates and admitted none of them is part of that peer's cadence, and dropping it
            // would bias the figure toward the peers that got served.
            let acc = self.acc_peer_band.entry(peer_id).or_insert((0, 0));
            acc.0 += peer_sends;
            acc.1 += peer_members;

            // A LEAVE-ONLY TICK STILL SENDS. The gate is "did this frame carry anything", and a
            // relevancy event is something: skipping the frame because no entity block was admitted
            // is exactly the tick on which a peer needs to be told that an entity stopped being sent
            // to it.
            if sent.is_empty() && !carries_delta {
                continue;
            }
            let header = FrameHeader {
                kind: FrameKind::ServerSnapshot,
                tick: u32::try_from(current).unwrap_or(u32::MAX),
                ack_tick,
                ack_bits: 0,
                ack_token,
                margin_ticks: margin,
                flags: if carries_delta {
                    FrameHeader::FLAG_INTEREST_DELTA
                } else {
                    0
                },
                entity_count: sent.len() as u32,
            };
            header.encode(&mut writer);
            writer.bytes(body.as_slice());
            // AFTER the blocks, which is what makes it invisible to a peer that does not know about
            // it: a receiver reads exactly `entity_count` blocks and stops.
            if carries_delta {
                encode_interest_delta(
                    interest_generation,
                    &delta_left,
                    &delta_entered,
                    &mut writer,
                );
            }
            self.acc_blocks_admitted += sent.len() as u64;
            self.acc_blocks_full += sent_full.len() as u64;
            self.dbg_sent += sent.len() as u64;
            self.dbg_sent_bytes += writer.len() as u64;
            self.send_to(peer_id, writer.as_slice(), TransferMode::UNRELIABLE);

            if let Some(peer) = self.peers.get_mut(&peer_id) {
                peer.want_full = false;
                // The stamp is set by the FIRST frame to carry this prefix and does not move on a
                // re-send: what an ack has to reach is the frame whose arrival proves the client
                // applied these entries.
                if carries_delta && peer.interest_delta_tick.is_none() {
                    peer.interest_delta_tick = Some(current);
                }
                for &(id, tick) in &sent {
                    peer.last_sent.insert(id, tick);
                }
                for &(id, tick) in &sent_full {
                    peer.last_full.insert(id, tick);
                }
                peer.sent_log.push_back((current, sent));
                while peer.sent_log.len() > SENT_LOG_DEPTH {
                    peer.sent_log.pop_front();
                }
            }
        }

        self.aoi_rows = rows;
        self.aoi_observers = observers;
        self.order_scratch = order;
        self.aoi_members = members;
        self.delta_left_scratch = delta_left;
        self.delta_entered_scratch = delta_entered;
    }

    /// The snapshot byte budget actually used, clamped to what the codec can carry.
    ///
    /// The floor is not cosmetic: a budget below a single full block would defer every entity
    /// forever, so the cvar round-trips against this rather than against whatever was typed.
    fn effective_send_budget(&self) -> usize {
        (self.send_budget.max(0) as usize).clamp(256, MAX_FRAME_PAYLOAD)
    }

    /// The interest configuration this tick, derived from the exported knobs.
    fn aoi_config(&self) -> AoiConfig {
        let radius = if self.aoi_radius.is_finite() {
            self.aoi_radius.max(0.0) as f32
        } else {
            0.0
        };
        AoiConfig {
            // **A quarter of the radius**, which is the size both tables in
            // `orbitnet_core::interest`'s header were measured at and the size its thresholds are
            // worked at: a query rectangle is then 11 cells a side whatever the radius. Read by the
            // flat path only through `select_interest_path` — the filter itself has no cells — and
            // by every rebuild and every query on the grid path.
            cell_size: (radius / 4.0).max(1.0),
            enter_radius: radius,
            exit_factor: AOI_EXIT_FACTOR,
            max_entities: self.aoi_max_entities.max(0) as usize,
        }
    }

    /// Gather every replicated entity's owner, anchor, membership and priority — **once per tick**.
    ///
    /// `input_owner_peer()` is a Godot `get_multiplayer_authority()` call, and `position_hint()` and
    /// `membership_hint()` are live property reads; doing any of them once per peer is the
    /// O(peers × entities) cost this pass exists to delete.
    fn collect_entity_rows(&self, rows: &mut Vec<EntityRow>) {
        rows.clear();
        for (&id, sync) in &self.rollback_entities {
            let Some(sync) = live_handle(sync) else {
                continue;
            };
            let bound = sync.bind();
            let owner = bound.input_owner_hint();
            let seat = bound.seat_hint();
            let anchor = bound.position_hint();
            let membership = bound.membership_hint();
            let priority = bound.send_priority();
            drop(bound);
            rows.push(EntityRow {
                id,
                owner,
                seat,
                anchor,
                membership,
                priority,
            });
        }
        for (&id, sync) in &self.state_entities {
            let Some(sync) = live_handle(sync) else {
                continue;
            };
            let bound = sync.bind();
            // `None` here is the "no distance to cull by" declaration, not a missing value: a state
            // channel is distance-culled only when it names an anchor that resolved to a Vector3.
            // It says nothing about the channel's world, which is the next line and is what lets a
            // positionless channel — health, inventory, a door — be bounded at all.
            let anchor = bound.position_hint();
            let membership = bound.membership_hint();
            let priority = bound.send_priority();
            drop(bound);
            rows.push(EntityRow {
                id,
                owner: 0,
                // The state lane has no input authority, so it drives no seat and takes the
                // default. `owner == 0` is what every reader keys on; this is never consulted.
                seat: 0,
                anchor,
                membership,
                priority,
            });
        }
    }

    /// Where each SEAT observes from and which world it is in: both read off the entity that seat
    /// drives.
    ///
    /// **KEYED BY SEAT, NOT BY CONNECTION**, which is the change local split-screen needs. Keyed by
    /// peer, a connection driving two bodies got one center — whichever body sorted lowest — and the
    /// other player's surroundings were culled around a position that player was nowhere near.
    ///
    /// **THE FALLBACK, consulted only for a peer that declared nothing.** `OrbitNet::set_peer_anchor`
    /// and `set_peer_anchor_entity` answer both questions outright, and [`resolve_observer`] does not
    /// look here for a peer that used either. This remains the default because a game with one world
    /// and one avatar per player gets the right answer from it with no declaration at all.
    ///
    /// **Called on rows already sorted by id, and it keeps the LOWEST id per seat.** `rows` is
    /// gathered by walking a `HashMap`, so a last-writer-wins insert would pick a different entity
    /// on different runs — and a seat driving more than one rollback entity would have its interest
    /// centered somewhere iteration order chose. Where each seat drives exactly one rollback body the
    /// rule is unobservable; it is written down because the failure it prevents is a whole
    /// viewpoint's world quietly centring on the wrong thing. The sort below is `sort_by_key`, which
    /// is STABLE, so the ascending-id order the scan collected in survives it.
    ///
    /// **One row supplies both facts.** A row with no resolved anchor is skipped entirely rather
    /// than contributing its membership, so a seat's center and its world always describe the same
    /// body. Splitting the picks would let a seat be centered on one entity and filtered against
    /// another's world, which is the same class of failure the lowest-id rule exists to prevent. A
    /// seat that contributes no row at all still exists — [`owned_rows_into`]'s output is what
    /// enumerates a connection's seats — and [`seat_observers_into`] decides what that seat is worth:
    /// no viewpoint at all while another seat on the connection resolved, and the connection-wide
    /// fail-open when none did.
    ///
    /// **THE LIMIT THIS INHERITS, AND WHAT IT COSTS FOR MEMBERSHIP.** "Lowest id" is lowest FNV hash
    /// of a node path, so among a seat's several bodies it is arbitrary — deterministic across peers
    /// and runs, which is what matters for the center, but not chosen. For the center a change of
    /// pick moves a radius. For the membership it changes the seat's *world*, and everything only
    /// that seat held leaves the connection's union on that one tick: `update_interest` clears
    /// `last_sent`, `last_full` and `acked_base` for each, which is a full-state burst rather than
    /// the per-entity repair that clearing exists to buy.
    ///
    /// It takes a seat driving **two anchored bodies that declare different worlds** (or one
    /// declaring a world and one not) to reach, which is a misconfiguration rather than a shape a
    /// game wants — a viewpoint is in one world. Three ways out, and the last two remove the pick
    /// rather than making it agree with itself: declare the same membership on every body a seat
    /// drives, put the bodies on separate seats, or declare the connection's world directly with
    /// `OrbitNet::set_peer_anchor`. `NetRollbackHandle.membership()` reports what the filter reads
    /// for an undeclared peer, which is where the mistake shows.
    ///
    /// **THE PICK IS NOT SILENT ANY MORE, AND IT STILL DOES NOT MOVE.** Moving it would relocate
    /// every existing consumer's interest sets on the tick their binary was refreshed, so the
    /// dropped rows are REPORTED instead, on the row that survived them:
    ///
    /// | Dropped rows | [`PeerObserver::ambiguous`] | [`PeerObserver::membership_conflict`] |
    /// | --- | --- | --- |
    /// | none — one anchored body on the seat | `false` | `false` |
    /// | several, all in the row's own world | `true` | `false` |
    /// | several, disagreeing about the world | `true` | `true` |
    ///
    /// A row with no resolved anchor is still skipped before any of this, so a seat whose second
    /// body has not spawned is not ambiguous — nothing was dropped, because nothing was eligible.
    fn collect_observers(rows: &[EntityRow], observers: &mut Vec<(SeatId, PeerObserver)>) {
        observers.clear();
        for row in rows {
            if row.owner <= 0 {
                continue;
            }
            if let Some(center) = row.anchor {
                observers.push((
                    row.seat_id(),
                    PeerObserver {
                        center,
                        membership: row.membership,
                        ambiguous: false,
                        membership_conflict: false,
                    },
                ));
            }
        }
        observers.sort_by_key(|&(seat, _)| seat);
        // `dedup_by` keeps the FIRST of each run — the lowest-id anchored row, since the scan
        // collected in ascending id order and the sort above is stable — and hands every dropped row
        // to the closure beside the one that survived. Folding the two flags in there is what makes
        // the pick reportable at no extra pass: the run is already being walked.
        observers.dedup_by(|dropped, kept| {
            if dropped.0 != kept.0 {
                return false;
            }
            kept.1.ambiguous = true;
            kept.1.membership_conflict |= dropped.1.membership != kept.1.membership;
            true
        });
    }

    /// Log the seats whose several anchored bodies disagree about the **world**, once per seat per
    /// episode.
    ///
    /// **TWO TIERS, BECAUSE ONE OF THE TWO AMBIGUITIES IS A SHAPE GAMES LEGITIMATELY HAVE.**
    ///
    /// | Seat drives | Reported as | Logged |
    /// | --- | --- | --- |
    /// | several anchored bodies in the SAME world | `ambiguous` on [`Self::peer_anchor_info`] | no |
    /// | several anchored bodies in DIFFERENT worlds | `ambiguous`, and this warning | once per episode |
    ///
    /// The quiet tier is quiet because a game that swaps one body for another on a seat holds both
    /// for the frame the swap takes. Warning there fires on every swap, for a configuration that is
    /// correct — and a warning a game learns to ignore reports nothing. Inside one world the pick
    /// costs a radius that is centered on one of two bodies the same seat drives, which is a
    /// difference of meters; across worlds it costs the seat's whole membership.
    ///
    /// **Once per seat per EPISODE, not per process.** The set is inserted into on the warning and
    /// pruned of every seat that is no longer colliding, so the same mistake reintroduced after a map
    /// change is reported again. The alternative — a set that only grows — tells the second
    /// occurrence to nobody, and the second occurrence is the one somebody is debugging.
    ///
    /// The rule is [`anchor_conflicts_owed`]; this is that plus the log line, because `godot_warn!`
    /// needs the Godot runtime and no unit test has one.
    fn warn_anchor_conflicts(&mut self, observers: &[(SeatId, PeerObserver)]) {
        // Empty on every tick of a correctly configured session, and an empty `Vec` allocates
        // nothing — so the common path here is one `is_empty` and one scan of an already-hot slice.
        let mut owed: Vec<SeatId> = Vec::new();
        anchor_conflicts_owed(&mut self.anchor_conflicts, observers, &mut owed);
        for seat in owed {
            let membership = observers
                .binary_search_by_key(&seat, |&(id, _)| id)
                .map_or(MEMBERSHIP_GLOBAL, |index| observers[index].1.membership);
            godot_warn!(
                "OrbitNet: seat {}/{} drives several anchored bodies that declare DIFFERENT \
                 worlds. Its world is taken from the lowest-id one of them ({}), and that pick is \
                 arbitrary — every entity only this seat held leaves the connection's interest on \
                 any tick the pick changes, which costs that connection a full-state burst rather \
                 than a per-entity repair. Fix it by declaring the same membership on every body \
                 the seat drives, by putting them on separate seats, or by declaring the \
                 connection's world with set_peer_anchor().",
                seat.peer,
                seat.seat,
                membership
            );
        }
    }

    /// The slice of [`Self::collect_observers`]'s output that belongs to one connection, ascending
    /// by seat.
    #[must_use]
    fn observers_of(
        observers: &[(SeatId, PeerObserver)],
        peer_id: i32,
    ) -> &[(SeatId, PeerObserver)] {
        let start = observers.partition_point(|&(seat, _)| seat.peer < peer_id);
        let end = observers.partition_point(|&(seat, _)| seat.peer <= peer_id);
        &observers[start..end]
    }

    /// Recompute every peer's interest set, and clear the delta bookkeeping of what left.
    ///
    /// Each peer is centered and placed in a world by [`resolve_observer`] — its own declaration when
    /// it made one, the body it drives when it did not — and then filtered on membership first and
    /// distance second, which is [`candidate_for_row`] plus whichever update path
    /// [`select_interest_path`] answered for the tick.
    ///
    /// **The shared candidate list carries no per-peer facts and does not have to.** A visibility
    /// veto ([`OrbitNet::set_entity_hidden`]) is held on the connection's own `ConnectionInterest`,
    /// mirrored onto each of its seats and applied inside the filter, so it costs this loop nothing
    /// and cannot be forgotten by a caller that builds candidates some other way. A vetoed entity is
    /// absent from `interest`, so the cull figure below — `rows.len() - interest.len()` — already
    /// counts it, on every tick the veto holds.
    ///
    /// The leave half is the correctness requirement here. Re-entry is already *safe* — a
    /// delta against a base the peer dropped is rejected and raises `WANT_FULL` — but `want_full` is
    /// a per-peer, **all-entity** flag, so one re-entering body would cost a round trip plus a
    /// full-state burst for every entity that peer holds, arriving exactly when a fight starts.
    /// Clearing `last_sent` and `acked_base` at the leave instead (the same pair the unregister path
    /// clears, for the same reason) forces a full block for that entity alone, and sorts it to the
    /// front of the rota while it is at it.
    ///
    /// **THE DIFF IS SYMMETRIC, AND THE ENTER HALF IS PUBLISHED RATHER THAN CONSUMED.** The leave
    /// half clears bookkeeping; the enter half clears nothing — an entity that was never sent to
    /// this peer has none to invalidate. Both halves are queued onto the connection's pending
    /// [`InterestDelta`], which rides the snapshot that peer is already receiving, and onto
    /// [`Self::interest_events`] for the signal. Two more leaves happen BETWEEN updates and no
    /// `leaves` list can ever name them — [`PeerState::set_entity_hidden`] and the despawn sweep in
    /// `drain_pending` — so each queues its own.
    ///
    /// **ONE CANDIDATE LIST PER TICK, NOT ONE PER PEER.** This loop used to rebuild the whole list
    /// inside itself, which is O(peers · entities) *before* the filter it feeds even runs — measured
    /// at 58% of the interest pass in a session with several worlds, where the sets are small and the
    /// filter is therefore cheap. Two facts forced the rebuild and neither needs it:
    ///
    /// * **A peer's own body is `always` to that peer and to nobody else.** That is a handful of
    ///   rows out of the whole tick, so they are swapped in around the call and swapped back after
    ///   ([`candidate_for_own_row`], [`owned_rows_of`]) rather than rebuilding everything around
    ///   them. A peer may drive more than one body and every one of them is patched.
    /// * **A peer with no radius or no resolved anchor culls on distance at all.** That used to
    ///   reshape every row in the list; it is now [`UNLOCATABLE_CENTER`], which reaches the same
    ///   fail-open through the filter's own non-finite-center rule.
    ///
    /// The sets this produces are identical — `shared_candidates_match_a_per_peer_rebuild` asserts
    /// it row by row against a reference that rebuilds per peer, over every combination of owned,
    /// unanchored and foreign-world rows.
    ///
    /// **ONE FILTER PASS PER SEAT, ONE SET PER CONNECTION.** A connection may drive several
    /// predicted bodies — local split-screen behind one socket — and each is a viewpoint with its
    /// own center and its own world. [`ConnectionInterest`] runs the filter once per seat and unions
    /// the results, because relevancy is a property of a viewpoint while the delta base, the ack
    /// window and the byte budget are properties of the datagram. Three consequences, all of them
    /// the reason the union is not simply the widest seat:
    ///
    /// * **A leave is a leave from the UNION.** Clearing `last_sent` when one seat lets go would
    ///   break the delta chain of a body the other seat is still watching.
    /// * **Culling is decided per seat, and an unresolved seat decides nothing.** A seat is filtered
    ///   around its own body rather than inheriting the center of a seat it is nowhere near; a seat
    ///   whose body has no state row yet is skipped instead, so a seat ARRIVING does not open the
    ///   whole connection to every world for as long as its body takes to spawn. Only a connection
    ///   with no resolved seat at all falls back to [`UNLOCATABLE_CENTER`].
    /// * **A declaration is per connection and collapses it to one seat.** See
    ///   [`resolve_observer`]: a game that stated where a connection observes from is not then
    ///   re-split by seat.
    ///
    /// All three are [`seat_observers_into`], which states the whole rule — including what happens
    /// when the connection's set of seats CHANGES — as one table.
    ///
    /// **THE SESSION PICKS ITS OWN PATH, ONCE PER TICK.** [`select_interest_path`] measures the
    /// candidate list this pass already built and answers [`InterestPath::Grid`] or
    /// [`InterestPath::Linear`]; on `Grid` one [`InterestGrid::rebuild`] runs here, before the
    /// per-connection loop, and every connection queries that one index. Three facts about the
    /// switch, each of them load-bearing:
    ///
    /// * **A mid-session switch emits no leaves.** The enter radius is a live setting a game may
    ///   change at runtime, and changing it moves the cell size and therefore the verdict. Both
    ///   paths compute the same members from the same state, so the diff against the same set
    ///   reports nothing — and a spurious leave here would clear `last_sent` for every entity on
    ///   that peer, which is the full-state burst the leave list exists to prevent.
    /// * **The grid's iteration is a `HashMap` walk**, and nothing downstream sees it. Every set
    ///   lands in a `BTreeMap` through `commit`'s id sort, the cap breaks distance ties by
    ///   ascending id, and the send rota's own sort ties by ascending id — so the wire order is
    ///   fixed by three separate normalizations and cannot vary run to run with the path.
    /// * **The verdict is published, never declared.** `bandwidth_metrics()`'s `interest_grid`
    ///   reports the fraction of the window's ticks that took the index. There is no setter,
    ///   because a wrong verdict costs time and nothing else.
    fn update_interest(
        &mut self,
        peer_ids: &[i32],
        rows: &[EntityRow],
        observers: &[(SeatId, PeerObserver)],
    ) {
        let cfg = self.aoi_config();
        let session_closed = self.unanchored_policy == UNANCHORED_CLOSED;
        let mut candidates = std::mem::take(&mut self.aoi_candidates);
        let mut owned = std::mem::take(&mut self.aoi_owned_rows);
        let mut seats = std::mem::take(&mut self.aoi_seats);
        let mut scratch = std::mem::take(&mut self.aoi_seat_scratch);
        let mut delta = std::mem::take(&mut self.aoi_delta);
        let mut overrides = std::mem::take(&mut self.aoi_overrides);
        let mut grid = std::mem::take(&mut self.aoi_grid);
        let mut occupancy = std::mem::take(&mut self.aoi_occupancy);
        let mut culled = 0u64;

        candidates.clear();
        candidates.extend(rows.iter().map(candidate_for_row));
        owned_rows_into(rows, &mut owned);

        // The tick's verdict, taken from the list the loop below is about to filter with — one
        // measurement and one rebuild for the whole session, not one per connection.
        let path = select_interest_path(
            &mut self.aoi_path,
            &cfg,
            &candidates,
            &owned,
            peer_ids,
            &mut occupancy,
        );
        if path == InterestPath::Grid {
            grid.rebuild(&cfg, &candidates);
        }
        self.acc_interest_grid_ticks += u64::from(path == InterestPath::Grid);
        let pass = InterestPass {
            path,
            grid: &grid,
            cfg: &cfg,
            rows,
        };

        for &peer_id in peer_ids {
            let Some(state) = self.peers.get(&peer_id) else {
                continue;
            };
            let (anchor, last, declared) =
                (state.anchor, state.anchor_last, state.anchor_membership);
            // A per-connection policy overrides the session default outright. `None` is "nobody said
            // anything about this connection", which is not the same statement as OPEN.
            let closed_when_unanchored = state.unanchored_closed.unwrap_or(session_closed);
            // Where a tracked entity is THIS tick, if it is still here and still has a position.
            // `rows` is sorted by id, which is what makes this a binary search rather than the
            // per-tick map that sort exists to avoid.
            let tracked = match anchor {
                PeerAnchor::Entity(id) => rows
                    .binary_search_by_key(&id, |row| row.id)
                    .ok()
                    .and_then(|index| rows[index].anchor),
                _ => None,
            };

            // This peer's own rows. Every seat on the connection gets every one of them as
            // `always`: the datagram is shared, so a body one seat drives rides on it whatever the
            // others can see. Where they are handed to the filter is what the path decides, and
            // [`filter_connection`] owns both halves of that — including restoring the shared list.
            let mine = owned_rows_of(&owned, peer_id);

            seat_observers_into(
                &cfg,
                mine,
                Self::observers_of(observers, peer_id),
                PeerDeclaration {
                    anchor,
                    membership: declared,
                    tracked,
                    last,
                    closed_when_unanchored,
                },
                &mut seats,
            );

            if let Some(peer) = self.peers.get_mut(&peer_id) {
                // Remember where a tracked entity was, so its despawn leaves the peer here rather
                // than opening its radius to the whole world. Only a resolved position is recorded.
                if let Some(pos) = tracked {
                    peer.anchor_last = Some(pos);
                }
                // THE ANSWER THAT IS IN EFFECT, KEPT. Taken here, beside the call that consumes it,
                // so the two cannot describe different ticks — and copied into the connection's own
                // vectors rather than reassigning them, so a steady session allocates nothing on the
                // path the send loop exists to keep cheap.
                peer.anchor_report.adopt(&seats);
                filter_connection(
                    &pass,
                    mine,
                    &seats.observers,
                    &mut candidates,
                    &mut overrides,
                    &mut peer.interest,
                    &mut scratch,
                    &mut delta,
                );
                for &id in &delta.leaves {
                    peer.last_sent.remove(&id);
                    peer.last_full.remove(&id);
                    peer.acked_base.remove(&id);
                    peer.note_interest_leave(id);
                }
                // The enter half clears nothing: an entity that was never sent to this peer has no
                // per-entity bookkeeping to invalidate, and one that left and came back had its
                // three entries cleared by the leave.
                for &id in &delta.enters {
                    peer.note_interest_enter(id);
                }
                culled += (rows.len() as u64).saturating_sub(peer.interest.len() as u64);
            }
            // Queued outside the borrow above, and closed before opened for the reason
            // `announce_seats` orders its own pair that way: an entity that moved between two
            // states in one tick reads as the old one ending and the new one beginning.
            for &id in &delta.leaves {
                self.interest_events.push((peer_id, id, false));
            }
            for &id in &delta.enters {
                self.interest_events.push((peer_id, id, true));
            }
        }
        self.acc_blocks_culled += culled;

        self.aoi_owned_rows = owned;
        self.aoi_candidates = candidates;
        self.aoi_seats = seats;
        self.aoi_seat_scratch = scratch;
        self.aoi_delta = delta;
        self.aoi_overrides = overrides;
        self.aoi_grid = grid;
        self.aoi_occupancy = occupancy;
    }

    // ------------------------------------------------------------------
    // Receive path
    // ------------------------------------------------------------------

    fn handle_packet(&mut self, sender: i32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // The reliable hello starts with the magic, everything else with a frame kind byte.
        if bytes.len() >= MAGIC.len() && bytes[..MAGIC.len()] == MAGIC {
            self.handle_hello(sender, bytes);
            return;
        }
        let Some(payload) = self.open_datagram(sender, bytes) else {
            return;
        };
        if payload.is_empty() {
            return;
        }
        let mut reader = Reader::new(payload);
        let Ok(kind) = FrameKind::from_tag(payload[0]) else {
            return;
        };
        match kind {
            // NOTE: the hot frames embed the kind byte in their header — FrameHeader::decode
            // consumes it — so the reader is handed over UNADVANCED here, unlike the control
            // frames below whose decoders start after the kind byte.
            FrameKind::ClientInput => {
                if self.mode == MODE_SERVER || self.mode == MODE_HOST {
                    self.handle_client_input(sender, &mut reader);
                }
            }
            FrameKind::ServerSnapshot => {
                if self.mode == MODE_CLIENT && sender == SERVER_PEER {
                    self.handle_snapshot(&mut reader);
                }
            }
            FrameKind::Ping => {
                if self.mode == MODE_SERVER || self.mode == MODE_HOST {
                    let _ = reader.u8();
                    if let Ok(ping) = Ping::decode(&mut reader) {
                        let pong = Pong {
                            seq: ping.seq,
                            client_us: ping.client_us,
                            server_time: self.server_time_now(),
                        };
                        self.send_to(sender, &pong.encode(), TransferMode::UNRELIABLE);
                    }
                }
            }
            FrameKind::Pong => {
                if self.mode == MODE_CLIENT && sender == SERVER_PEER {
                    let _ = reader.u8();
                    if let Ok(pong) = Pong::decode(&mut reader) {
                        self.integrate_pong(&pong);
                    }
                }
            }
            FrameKind::Welcome => {
                if self.mode == MODE_CLIENT && sender == SERVER_PEER {
                    let _ = reader.u8();
                    if let Ok(welcome) = Welcome::decode(&mut reader) {
                        self.integrate_welcome(&welcome);
                    }
                }
            }
            FrameKind::EntityManifest => {
                if self.mode == MODE_CLIENT && sender == SERVER_PEER {
                    let _ = reader.u8();
                    match decode_manifest_full(&mut reader) {
                        // A FULL TABLE OLDER THAN THE ONE HELD IS IGNORED. The channel is reliable
                        // and ordered, so this should be unreachable; it is checked because the
                        // alternative is a client that quietly adopts a table the server has already
                        // moved past and then refuses every delta built on the newer one.
                        Ok((generation, entries)) if generation >= self.manifest_generation => {
                            self.adopt_manifest_full(generation, entries);
                        }
                        Ok(_) => {}
                        // **A DECODE ERROR IS REPORTED HERE, NOT SWALLOWED.** Dropping it was
                        // safe only while the next frame carried the whole table and repaired
                        // whatever it left behind. There is no next whole table unless this asks
                        // for one, so a swallowed error is permanent corruption.
                        Err(err) => {
                            let why = err.to_string();
                            self.refuse_manifest(&why);
                        }
                    }
                }
            }
            FrameKind::InterestTable => {
                if self.mode == MODE_CLIENT && sender == SERVER_PEER {
                    let _ = reader.u8();
                    match decode_interest_table(&mut reader) {
                        // An older table is ignored for the reason a stale manifest is: the channel
                        // is reliable and ordered, so it should be unreachable, and adopting one
                        // would make this peer refuse every delta built on the newer set.
                        Ok((generation, slots))
                            if generation >= self.interest_mirror_generation =>
                        {
                            self.adopt_interest_table(generation, &slots);
                        }
                        Ok(_) => {}
                        // A table that did not decode leaves the mirror as it was and asks again.
                        // There is no next whole set unless this asks for one.
                        Err(_) => {
                            self.want_interest = true;
                        }
                    }
                }
            }
            FrameKind::EntityManifestDelta => {
                if self.mode == MODE_CLIENT && sender == SERVER_PEER {
                    let _ = reader.u8();
                    match decode_manifest_delta(&mut reader) {
                        Ok(delta) if delta.applies_to(self.manifest_generation) => {
                            self.adopt_manifest_delta(&delta);
                        }
                        // A delta against a table this peer is not holding. Refused whole rather
                        // than applied in part: the records that would land are the ones for slots
                        // this peer happens to agree about, and the ones that would not are exactly
                        // the ones that disagree.
                        Ok(delta) => {
                            let why = format!(
                                "it states a change to generation {}, and this peer holds {}",
                                delta.base_generation, self.manifest_generation
                            );
                            self.refuse_manifest(&why);
                        }
                        Err(err) => {
                            let why = err.to_string();
                            self.refuse_manifest(&why);
                        }
                    }
                }
            }
        }
    }

    /// CLIENT: adopt a whole entity-manifest table, replacing everything derived from the last one.
    ///
    /// The table arrives complete, so the slot table is **cleared and rebuilt** rather than merged:
    /// a merge would keep naming an entity the server has unregistered, and a slot reissued to a
    /// different entity would then resolve to the wrong one.
    ///
    /// The seat table is rebuilt the same way. The roster is projected from it on the next tick
    /// boundary rather than here, so a client emits its seat events where a server emits its own —
    /// see [`Self::announce_seats`].
    fn adopt_manifest_full(&mut self, generation: u64, entries: Vec<ManifestEntry>) {
        self.slots.clear();
        for entry in &entries {
            self.slots.bind(entry.slot, entry.id);
            self.note_manifest_row(entry);
        }
        self.manifest_published = entries;
        self.manifest_generation = generation;
        self.rebuild_seats_from_manifest();
        // An entity that has left the table has left this peer's interest, whatever the last
        // section said. Run against the REBUILT table, so what it reads is the set of ids the
        // server still names.
        self.apply_manifest_interest();
    }

    /// CLIENT: apply one entity-manifest delta this peer's generation says it can apply.
    ///
    /// **THE FRAME IS DECODED WHOLE BEFORE A BYTE OF IT IS APPLIED, and the rows are built into a
    /// SCRATCH table that is swapped in only once every record has landed.** A complete table could
    /// be abandoned half-decoded and repaired by the next one; a delta cannot, so the swap point is
    /// the decode. `decode_manifest_delta` answers a whole [`ManifestDelta`] or an error and touches
    /// nothing on the way, [`apply_manifest_delta`] builds the new rows beside the held ones, and
    /// only then is anything the client reads back allowed to move.
    ///
    /// The slot table is patched rather than rebuilt — [`SlotTable::unbind`] for each retired slot,
    /// [`SlotTable::bind`] for each stated row — which is the point of a delta. `bind` replaces both
    /// directions, so one `added` record covers a new binding, a reissued slot and a changed row
    /// alike, and applying one twice lands in the same place.
    fn adopt_manifest_delta(&mut self, delta: &ManifestDelta) {
        let rows = apply_manifest_delta(&self.manifest_published, delta);
        self.manifest_published = rows;
        self.manifest_generation = delta.generation;
        for &slot in &delta.removed {
            self.slots.unbind(slot);
        }
        for entry in &delta.added {
            self.slots.bind(entry.slot, entry.id);
            self.note_manifest_row(entry);
        }
        self.rebuild_seats_from_manifest();
        self.apply_manifest_interest();
    }

    /// CLIENT: record one row's schema fingerprints, and report a disagreement by name.
    ///
    /// Run per row that ARRIVED rather than per row held: a delta states only what changed, and
    /// re-binding every entity in the table on every delta is the per-entity cost a delta exists to
    /// avoid.
    fn note_manifest_row(&mut self, entry: &ManifestEntry) {
        self.expected_schemas
            .insert(entry.id, (entry.state_hash, entry.input_hash));
        if let Some(sync) = self.rollback_entities.get(&entry.id) {
            if sync.is_instance_valid() {
                self.check_expected_schema(entry.id, sync);
            }
        }
    }

    /// CLIENT: project the seat table out of the manifest rows this peer holds.
    ///
    /// **From the held table, not from the frame that just arrived.** A seat is a projection of the
    /// whole manifest — a delta that states nothing about an entity says that entity's seat has not
    /// changed — so a roster built from a delta's rows alone would drop every seat the delta was
    /// silent about.
    fn rebuild_seats_from_manifest(&mut self) {
        self.entity_seats.clear();
        for entry in &self.manifest_published {
            if entry.owner > 0 {
                self.entity_seats.push((entry.id, entry.owner, entry.seat));
            }
        }
        // Ascending by id, the order the server's own table is held in. The rows arrive in SLOT
        // order, so this is a sort rather than an assumption, and it is what makes the two sides'
        // tables comparable row for row.
        self.entity_seats.sort_unstable();
        self.seats_dirty = true;
    }

    /// CLIENT: the entity-manifest stream broke — ask for the whole table and stop claiming to hold
    /// one.
    ///
    /// **Both halves are needed.** Zeroing the generation is what makes every later delta fail its
    /// base check, so a lost NACK costs one tick rather than the session; the flag is what tells the
    /// server to answer with the whole table rather than another delta.
    ///
    /// **The stale table is KEPT until the replacement lands.** Clearing it would stop every block
    /// resolving for a round trip, which is a worse outage than the one this repairs, and the reuse
    /// quarantine already covers a binding that is wrong rather than merely old.
    fn refuse_manifest(&mut self, why: &str) {
        self.manifest_generation = 0;
        self.want_manifest = true;
        if self.manifest_break_warned {
            return;
        }
        self.manifest_break_warned = true;
        godot_warn!(
            "OrbitNet: could not apply an entity manifest from the server ({why}) — asking for the \
             whole table. Slot bindings, schema checks and the seat roster all ride that frame, so \
             until it arrives this peer is holding the table it had. Further breaks this session \
             are silent."
        );
    }

    /// Verify a datagram against its session's key and replay window, answering the payload.
    ///
    /// **Nothing below this line decodes a byte a peer chose until this returns.** `None` means the
    /// datagram was forged, replayed, or sent by a peer with no handshake — including the ping a
    /// server used to answer for any connected sender, which is now refused with the rest.
    fn open_datagram<'a>(&mut self, sender: i32, bytes: &'a [u8]) -> Option<&'a [u8]> {
        // A peer authenticates with the direction it EXPECTS TO RECEIVE. That is what makes a
        // reflected datagram fail: the direction is mixed into the MAC and never sent.
        let (_, direction) = session_directions(self.mode)?;
        let auth = match direction {
            Direction::ToClient => self.session_auth.as_mut(),
            Direction::ToServer => self
                .peers
                .get_mut(&sender)
                .and_then(|state| state.auth.as_mut()),
        };
        let opened = auth.map(|auth| auth.open(direction, bytes));
        match opened {
            Some(Ok(payload)) => Some(payload),
            Some(Err(AuthError::Truncated | AuthError::BadTag | AuthError::Replayed)) | None => {
                self.note_unauthenticated(sender);
                None
            }
        }
    }

    /// Count a refused datagram, and say so once per session.
    ///
    /// Once, because the log is the second thing to fall over under a flood. The count is what a
    /// `ORBITNET_DEBUG` run prints every second, and it is the number that says whether a session is
    /// being probed or merely misconfigured.
    fn note_unauthenticated(&mut self, sender: i32) {
        self.dbg_rx_unauth += 1;
        if self.auth_warned {
            return;
        }
        self.auth_warned = true;
        godot_warn!(
            "OrbitNet: refusing an unauthenticated datagram from peer {sender} — forged, replayed, \
             sent before the handshake, or sealed under a key derived from a different session \
             secret. Compare has_session_secret() on both ends if a join never completes. Further \
             refusals this session are silent; run with ORBITNET_DEBUG to count them."
        );
    }

    /// Count input rows refused for carrying a non-finite float, and name the sender once.
    ///
    /// **Visibility is part of the refusal.** A dropped row is indistinguishable from packet loss
    /// from the outside — the body coasts on its last received input either way — so an operator
    /// watching a body behave oddly would have nothing to look at. The count is what a
    /// `ORBITNET_DEBUG` run prints every second beside `input_novel`, and it is the number that says
    /// whether one client is misbehaving or one property is simply going non-finite in the game.
    ///
    /// **One warning per peer per session**, latched on [`PeerState::nonfinite_warned`] the way
    /// [`Self::note_unauthenticated`] latches its own: under an actual flood the log is the second
    /// thing to fall over.
    fn note_nonfinite_input(&mut self, sender: i32, rows: u32) {
        self.dbg_input_nonfinite += u64::from(rows);
        let Some(peer) = self.peers.get_mut(&sender) else {
            return;
        };
        if peer.nonfinite_warned {
            return;
        }
        peer.nonfinite_warned = true;
        godot_warn!(
            "OrbitNet: refusing an input row from peer {sender} — a float property arrived \
             non-finite (NaN or infinity), which would poison this body's simulation on every peer \
             and make it uncullable. The row is dropped rather than sanitized, so the body coasts \
             on its last received input. Further refusals from this peer are silent; run with \
             ORBITNET_DEBUG to count them."
        );
    }

    fn handle_hello(&mut self, sender: i32, bytes: &[u8]) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
        let Ok(hello) = Handshake::decode(bytes) else {
            return;
        };
        let ours = Handshake::local(self.effective_rate().hz() as u16);
        if let Err(err) = ours.check_compatibility(&hello, self.session_secret.as_ref()) {
            godot_error!("OrbitNet: rejecting peer {sender}: {err}");
            return;
        }
        // THE SESSION KEY, DERIVED BEFORE THE REKEY COMPARISON BELOW so that the comparison is
        // derived-key against derived-key. Comparing the wire nonce instead and re-deriving on every
        // hello would reset the replay window on each RETRY of one join, which is exactly the property
        // that comparison exists to preserve.
        let session_key = session_key_from(self.session_secret.as_ref(), hello.session_nonce);
        // The whole resume decision, and the two mutations it implies — stripping a superseded incumbent's
        // identity, and spending the held window. It is a free function over the two plain tables so the
        // rule this defect lived in is one thing a test can call with no `SceneTree`; see [`seat_hello`].
        let seat = seat_hello(
            &mut self.peers,
            &mut self.resume,
            self.resume_policy,
            sender,
            hello.session_id,
            hello.resume_token,
        );
        let resumed_from = seat.resumed_from;
        let seated_session_id = seat.session_id;
        let peer = self.peers.entry(sender).or_default();
        // A hello is RETRIED until the welcome lands, so this runs again for a peer already synced. The
        // welcome has to go out again — that is what the retry is for — but the game must hear about the join
        // exactly once, or a roster answers a lost packet by re-seating somebody who never left.
        let first_hello = !peer.synced;
        // Seat the session key. A hello is retried, so the usual case is the SAME key arriving again
        // — and the window must SURVIVE that, because resetting it on every hello would let anything
        // captured from this peer be replayed by sending one copy of its own handshake first.
        //
        // A hello carrying a DIFFERENT key is a peer that restarted its session on a live connection,
        // and it gets a fresh window and a fresh budget. That a hello can rekey a connection at all is
        // the same trust the transport's sender id already carries: nothing but the connection says
        // who sent it.
        //
        // Under a session secret the key is derived rather than read off the wire, and the comparison
        // is unchanged because it was already derived above: a retried hello repeats its nonce, derives
        // the same key, and keeps its window.
        let rekeyed = peer.auth.is_some_and(|auth| auth.key() != session_key);
        if peer.auth.is_none_or(|auth| auth.key() != session_key) {
            peer.auth = Some(SessionAuth::new(session_key));
            peer.budget = ReceiveBudget::new();
            // A REKEY IS A CLIENT THAT RESTARTED ITS SESSION ON A LIVE CONNECTION, so its entity
            // manifest went with it. Zeroed in the same block that replaces the auth, because these
            // are one fact: everything this connection held is gone. A delta sent against the
            // generation it held before would apply cleanly to a table it no longer has, and the
            // rebind of a reissued slot is the record it would then be missing. A RETRIED hello
            // repeats its nonce, derives the same key and does not enter here, so it keeps the
            // table it is still holding.
            peer.forget_manifest();
            // AND THE DELTA BASES WITH IT. `stop()` cleared every row the client held, so an
            // `acked_base` entry here names a row that no longer exists on the other end. `want_full`
            // alone does not cover it: it makes the NEXT frame full for the entities that fit the
            // byte budget, and every one deferred past that budget was then encoded as a masked delta
            // against a base the client had already dropped — a guaranteed `NoBase` and a NACK on a
            // connection that had just told the server everything was gone.
            peer.acked_base.clear();
            peer.last_full.clear();
            peer.last_sent.clear();
            // The interest set went the same way, and the whole set is what re-seats it. Only on
            // a true REKEY: a first hello has no set to have lost, and owing one there sent a table
            // for an interest pass that had never run — an empty set, which a client then holds as
            // the whole truth.
            if rekeyed {
                peer.interest_seeded = false;
                peer.interest_full_due = true;
            }
        }
        // The secret this connection's frame tokens are minted from. `get_or_insert_with`, not an
        // assignment: a hello is retried, and re-minting would strand every token the client already
        // holds. See `PeerState::token_salt` for why a rekey does not rotate it either.
        if peer.token_salt.is_none() {
            peer.token_salt = Some(Self::mint_session_key());
        }
        peer.synced = true;
        peer.want_full = true;
        peer.newest_input_tick = -1;
        peer.session_id = seated_session_id;
        // **The resume token is minted ONCE PER IDENTITY, and only for a connection that holds one.**
        //
        // - A granted resume carries the token on record forward verbatim. The client is holding that value
        //   and quoted it to get here; issuing a fresh one would make every stored copy stale.
        // - A retried hello re-enters this function for a peer that is already synced, so the mint is
        //   guarded on the connection already having none. Re-minting there would strand the token the
        //   client took from the welcome that did arrive.
        // - A connection seated with identity `0` gets NO token, and its welcome carries `0`. A token names
        //   an identity, an anonymous seat has none, and a fresh token in that welcome would overwrite the
        //   one the client is storing for the identity it is still waiting to reclaim.
        if peer.session_id != 0 && peer.resume_token == 0 {
            peer.resume_token = if seat.grant == ResumeGrant::Resume && seat.token_on_record != 0 {
                seat.token_on_record
            } else {
                Self::mint_resume_token()
            };
        }
        let resume_token = peer.resume_token;

        let welcome = Welcome {
            protocol_version: orbitnet_core::PROTOCOL_VERSION,
            server_tick: self.accumulator.tick(),
            tickrate: self.effective_rate().hz() as u16,
            resume_token,
        };
        self.send_to(sender, &welcome.encode(), TransferMode::RELIABLE);
        self.manifest_dirty = true;
        // Last, and after every field this call sets: the game answers this signal by seating the player,
        // which re-enters this node through the facade.
        //
        // **It reports the identity the connection was SEATED under, not the one it presented.** A refused
        // claim on an identity somebody else holds is seated anonymously, and announcing the presented
        // value would hand a game the roster key of the player whose claim was just refused.
        if first_hello {
            self.signals().peer_joined().emit(
                i64::from(sender),
                seated_session_id as i64,
                i64::from(resumed_from),
            );
        }
    }

    fn integrate_welcome(&mut self, welcome: &Welcome) {
        if self.synced {
            return;
        }
        if !self.sync_to_physics {
            let advertised = TickRate::new(u32::from(welcome.tickrate.max(1)));
            if advertised.hz() != self.effective_rate().hz() {
                godot_warn!(
                    "OrbitNet: adopting the server tickrate {} Hz (local configuration said {})",
                    advertised.hz(),
                    self.effective_rate().hz()
                );
                self.tickrate = advertised.hz() as i32;
                self.accumulator.set_rate(advertised);
            }
        }
        // The resume token this server issued for our identity. **A `0` does not clear the stored one**:
        // that answer means the server seated this connection with no identity of its own — an anonymous
        // peer, or a claim it refused — and forgetting a live token on the strength of it would cost this
        // peer the honest reconnect the token exists to grant.
        if welcome.resume_token != 0 {
            self.resume_token = welcome.resume_token;
        }
        self.accumulator
            .seek(welcome.server_tick.saturating_add(INITIAL_LEAD_TICKS));
        self.synced = true;
        self.hello_pending = false;
        self.running = true;
    }

    /// This peer's position on the shared tick timeline, in seconds — the quantity the clock
    /// discipline exchanges and disciplines. It is `timeline_seconds`, NOT `(tick + factor) * dt`:
    /// retained catch-up backlog after a render hitch counts, because that time has already
    /// arrived and will be simulated. Measuring the simulated position instead reported every
    /// hitch on either end as a clock offset, which the bounded stretch then chased for seconds —
    /// and past the panic threshold, hard-resynced. A LAN listen host with two rendered clients
    /// measured a steady-state hard resync every 13–25 s from exactly that.
    fn server_time_now(&self) -> f64 {
        self.accumulator.timeline_seconds()
    }

    fn integrate_pong(&mut self, pong: &Pong) {
        let now_us = Time::singleton().get_ticks_usec();
        let rtt = (now_us.saturating_sub(pong.client_us)) as f64 / 1_000_000.0;
        // The server stamped its sim time at reply; by now it advanced ~rtt/2 further.
        let server_time_est = pong.server_time + rtt * 0.5;
        let local_time = self.server_time_now();
        self.clock.push_sample(rtt, server_time_est - local_time);
    }

    fn handle_client_input(&mut self, sender: i32, reader: &mut Reader<'_>) {
        let Ok(header) = FrameHeader::decode(reader) else {
            return;
        };
        let current = self.accumulator.tick();
        // Read the tick period BEFORE the peer is borrowed mutably below, and stamp the
        // round-trip sample in milliseconds while it is still true.
        let tick_ms = self.effective_rate().dt() * 1000.0;
        let mut nacked = false;
        let mut manifest_nacked = false;
        let unproven_ack;
        {
            let Some(peer) = self.peers.get_mut(&sender) else {
                return; // No handshake, no input.
            };
            // Consume the ack window: every snapshot frame the client PROVES it received promotes
            // the entity ticks that frame carried to `acked_base` — the only ticks a masked delta
            // may reference, because the client provably holds those rows. An ack that carries the
            // wrong frame token proves nothing and is refused whole; see `PeerState::consume_ack`.
            let outcome = peer.consume_ack(
                u64::from(header.ack_tick),
                header.ack_token,
                header.ack_bits,
                current,
                tick_ms,
            );
            unproven_ack = outcome == AckOutcome::Unproven;
            // THE NACK IS TAKEN AFTER THE ACK, AND THE ORDER IS THE WHOLE POINT. One packet carries
            // both: a receiver that answered `NoBase` for a block sets its `want_full` inside the
            // per-entity loop, while `handle_snapshot` has already advanced the ack fields for that
            // same frame unconditionally. So the packet reporting the failure also acknowledges the
            // frame that failed, and `sent_log` records every entity that frame carried regardless
            // of what the receiver made of them.
            //
            // Clearing first and consuming second therefore re-promotes the exact base that just
            // proved undecodable. `want_full` hides it for one round, because `full_block_due`
            // forces every entity full — but only for the entities that fit the byte budget, and the
            // ones deferred past that round carry the re-poisoned entry into a masked delta with the
            // flag already back down. Taking the NACK last leaves no base this peer has not proven
            // it holds.
            if header.flags & FrameHeader::FLAG_WANT_FULL != 0 {
                peer.note_nack();
                nacked = true;
            }
            // THE MANIFEST NACK: this peer could not apply a delta, so stop believing it holds any
            // table at all. Zeroing here is the whole answer — the next publish finds it behind and
            // addresses it the whole table. See `PeerState::manifest_generation`.
            if header.flags & FrameHeader::FLAG_WANT_MANIFEST != 0 {
                peer.forget_manifest();
                manifest_nacked = true;
            }

            // THE INTEREST NACK. Answered every time it is raised, and deliberately not rate
            // limited: a client asks because it is holding a section it cannot place or a set it
            // cannot name, and the server retires that section's prefix on this same frame's ack
            // whether or not the client integrated it. Muting the ask for a window therefore loses
            // exactly the transitions the ask exists to recover — a table built before them cannot
            // restate them, and no later diff re-enters an id already in the set.
            //
            // What that costs is a burst: while a reliable table is still in flight, every section
            // behind it is stamped at a generation the client does not hold, so it asks once per
            // tick for about a round trip and is answered each time. Bounded, self-limiting, and
            // cheaper than the alternative, which is a permanently wrong mirror.
            if header.flags & FrameHeader::FLAG_WANT_INTEREST != 0 {
                peer.interest_full_due = true;
            }
        }
        // The publish loop runs only on a dirty flush, and a peer asking for the table it should
        // already be holding is not a change to the table. Raise it so the flush runs at all.
        if manifest_nacked {
            self.manifest_dirty = true;
        }
        // The acceptance bar for turning AOI on: a re-entering entity must get its full block
        // WITHOUT a want_full storm, and this is the number that says whether it did.
        if nacked {
            self.acc_want_full_nacks += 1;
        }
        if unproven_ack {
            self.acc_unproven_acks += 1;
        }

        // What this peer may spend of the receive path this tick. Every block below costs a handle
        // resolve and a live authority call, so without a bound a peer could spend the server's tick
        // having blocks it does not own correctly refused. See `orbitnet_core::auth::ReceiveBudget`.
        let mut budget = self
            .peers
            .get(&sender)
            .map_or_else(ReceiveBudget::new, |peer| peer.budget);
        budget.open(current);
        for _ in 0..header.entity_count {
            if !budget.admit() {
                break;
            }
            let Ok(meta) = decode_input_block_meta(reader, u64::from(header.tick)) else {
                break;
            };
            // Bound accepted input to the near future: the history ring only rejects the PAST, so
            // a hostile newest_tick near u64::MAX would rotate the ring's frontier out of reach
            // and freeze this body's input for the rest of the session. No honest client leads
            // the server by anywhere near this horizon (see the constant), and a joiner whose
            // unsettled clock runs ahead has its stamps refused until its first hard resync
            // rather than parked in the ring as seconds-stale intent the server later honors.
            if meta.newest_tick > current.saturating_add(INPUT_FUTURE_HORIZON_TICKS) {
                let _ = skip_input_block_body(reader, &meta);
                continue;
            }
            // The packet may name a body that died since the last drain_pending — a client keeps
            // sending input for its avatar until the despawn reaches it, a full round trip after the
            // server freed the body. Two ways that shows up now: the slot has already been released
            // (so it names nothing), or it still names the corpse. Resolve the handle through
            // live_handle so the corpse's stale registry entry is skipped rather than cloned (which
            // panics) — this is the exact line the shipped crash logs pointed at.
            let Some(entity) = self.slots.id_of(meta.slot) else {
                let _ = skip_input_block_body(reader, &meta);
                continue;
            };
            let Some(mut sync) = self.rollback_entities.get(&entity).and_then(live_handle) else {
                let _ = skip_input_block_body(reader, &meta);
                continue;
            };
            let mut bound = sync.bind_mut();
            // Reject input for entities this sender does not own: the input node's authority is
            // the anti-forgery check. LIVE, never the send path's cached hint — a cache is a window
            // in which "may this sender write this body" can be wrong, and this is the one call
            // site where that question is a security one.
            let owner = bound.input_owner_peer();
            if owner != sender {
                drop(bound);
                let _ = skip_input_block_body(reader, &meta);
                if !budget.note_foreign() {
                    break;
                }
                continue;
            }
            let wire_stride = bound.input_wire_stride();
            let mut earliest_novel: Option<u64> = None;
            let mut refused_nonfinite: u32 = 0;
            for index in 0..meta.count {
                let Some(row) = input_block_row(reader, &meta, wire_stride, index) else {
                    break;
                };
                let Some(tick) = meta.newest_tick.checked_sub(u64::from(index)) else {
                    break;
                };
                match bound.integrate_remote_wire_row(tick, row) {
                    InputIntegration::Landed(novel_tick) => {
                        earliest_novel = Some(match earliest_novel {
                            Some(existing) => existing.min(novel_tick),
                            None => novel_tick,
                        });
                    }
                    // Refused for a non-finite float, and NOT folded into the arm below: a dropped
                    // row is otherwise indistinguishable from packet loss. `saturating_add` because
                    // `meta.count` is a wire field and `[profile.template-debug]` sets
                    // `overflow-checks = true`.
                    InputIntegration::NonFinite => {
                        refused_nonfinite = refused_nonfinite.saturating_add(1);
                    }
                    InputIntegration::Ignored => {}
                }
            }
            let id = bound.entity_id();
            drop(bound);
            let _ = skip_input_block_body(reader, &meta);
            if refused_nonfinite > 0 {
                self.note_nonfinite_input(sender, refused_nonfinite);
            }
            if let Some(tick) = earliest_novel {
                self.dbg_input_novel += 1;
                if let Some(from) = resim_input_from(tick, current) {
                    self.planner.mark(id, from);
                }
            }
        }

        if let Some(peer) = self.peers.get_mut(&sender) {
            peer.budget = budget;
            let newest = i64::from(header.tick);
            peer.newest_input_tick = peer.newest_input_tick.max(newest);
            let margin = newest - i64::try_from(current).unwrap_or(i64::MAX);
            peer.margin_last = margin.clamp(-127, 127) as i8;
        }
    }

    /// Fold one integration outcome into the receive counters, raising a `WANT_FULL` NACK for the
    /// one rejection a full block can fix.
    ///
    /// **Both lanes route through here so the rule cannot drift between them**, which is how the
    /// storm survived review: the rollback lane and the state lane each carried their own copy
    /// of `if !meta.full { want_full = true }` under a `Rejected` arm that meant two different
    /// things. The classification is [`classify_rx`], which is pure and gated.
    fn note_integration(&mut self, outcome: StateIntegration, block_was_full: bool) {
        match classify_rx(outcome, block_was_full) {
            RxOutcome::Applied => self.dbg_rx_applied += 1,
            RxOutcome::Nack => {
                self.dbg_rx_rejected += 1;
                self.want_full = true;
            }
            RxOutcome::StaleDrop => self.acc_stale_blocks += 1,
        }
    }

    fn handle_snapshot(&mut self, reader: &mut Reader<'_>) {
        let Ok(header) = FrameHeader::decode(reader) else {
            return;
        };
        let frame_tick = u64::from(header.tick);
        // What this peer now owes an input frame for, whether or not it drives a body. The window
        // slid below is only worth sliding if something carries it back.
        self.snapshot_unacked = true;
        // Slide the ack window: the server only deltas against frames we confirm here.
        if frame_tick > self.newest_snapshot_tick {
            let shift = frame_tick - self.newest_snapshot_tick;
            self.snapshot_ack_bits = if shift >= 32 || self.newest_snapshot_tick == 0 {
                0
            } else {
                (self.snapshot_ack_bits << shift) | (1u32 << (shift - 1))
            };
            self.newest_snapshot_tick = frame_tick;
            // The proof rides with the tick it proves. An older frame arriving out of order sets its
            // ack BIT below but not this, because the ack the server checks names the newest.
            self.snapshot_ack_token = header.ack_token;
        } else if frame_tick < self.newest_snapshot_tick {
            let behind = self.newest_snapshot_tick - frame_tick;
            if (1..=32).contains(&behind) {
                self.snapshot_ack_bits |= 1u32 << (behind - 1);
            }
        }
        self.lead.push(header.margin_ticks);
        let current = self.accumulator.tick();

        for _ in 0..header.entity_count {
            let Ok(meta) = decode_state_block_meta(reader, frame_tick) else {
                return;
            };
            // A slot with no binding is the ordinary in-flight case — the manifest that binds it
            // rides the reliable channel and an unreliable snapshot can overtake it — so it falls
            // through to the skip below, exactly as an unknown entity id used to.
            let entity = self.slots.id_of(meta.slot).unwrap_or(0);
            if let Some(mut sync) = self.rollback_entities.get(&entity).and_then(live_handle) {
                if !meta.state_lane {
                    let result = {
                        let mut bound = sync.bind_mut();
                        bound.apply_state_block(reader, &meta, &mut self.mask_scratch, current)
                    };
                    match result {
                        Ok(StateIntegration::Mispredict(tick)) => {
                            self.dbg_rx_applied += 1;
                            self.planner.mark(entity, tick);
                        }
                        Ok(outcome) => self.note_integration(outcome, meta.full),
                        Err(_) => return,
                    }
                    continue;
                }
            }
            if let Some(mut sync) = self.state_entities.get(&entity).and_then(live_handle) {
                if meta.state_lane {
                    let result = {
                        let mut bound = sync.bind_mut();
                        bound.apply_state_block(reader, &meta, &mut self.mask_scratch)
                    };
                    match result {
                        Ok(outcome) => self.note_integration(outcome, meta.full),
                        Err(_) => return,
                    }
                    continue;
                }
            }
            // Unknown entity (spawn in flight) — skip its body cleanly.
            self.dbg_rx_skipped += 1;
            if self.debug_wire && self.dbg_rx_skipped % 120 == 1 {
                godot_print!(
                    "[orbitnet] rx skip unknown slot {} (entity {:#018x}) lane={} tick={}",
                    meta.slot,
                    entity,
                    meta.state_lane,
                    meta.tick
                );
            }
            if skip_state_block_body(reader, &meta).is_err() {
                return;
            }
        }

        // THE TRAILING SECTION, AFTER THE BLOCKS AND BEHIND ITS FLAG. Everything above reads exactly
        // `entity_count` blocks and stops, which is what lets these bytes exist at all: a build that
        // predates the flag never looks at them.
        if header.flags & FrameHeader::FLAG_INTEREST_DELTA != 0 {
            if let Ok(section) = decode_interest_delta(reader) {
                self.apply_interest_delta(&section);
            }
        }
    }

    /// CLIENT: fold one interest-delta section into the mirrored set, queuing what changed.
    ///
    /// **Idempotent, which is what makes a re-send free.** The section rides an unreliable datagram
    /// and is re-sent until this peer acks the frame it first rode on, so the same content arrives
    /// several times on a lossy link. Each slot is applied as a set operation and only a set that
    /// actually changed queues an event, so every copy after the first announces nothing.
    ///
    /// **An unbound slot in the `left` half is dropped in silence**, exactly as a block naming one
    /// is. That case is ordinary rather than hostile: a leave whose cause is an unregister names a
    /// slot the manifest that follows releases, and the reliable manifest can arrive before the
    /// unreliable snapshot that names it. The leave is not lost — [`Self::apply_manifest_interest`]
    /// emits it from the rebuild — and the mirrored set is what makes the two produce exactly one
    /// event between them. An unbound slot in the `entered` half has no such second source, so it
    /// asks for the whole set instead.
    fn apply_interest_delta(&mut self, section: &InterestDeltaSection) {
        // A SECTION THIS PEER CANNOT PLACE IS DROPPED, AND ASKED ABOUT. It states a change against
        // one baseline, and this peer is holding another: below its own generation it is a section
        // the whole set already superseded, above it one built against a set that has not arrived.
        // Either way applying it would leave a mirror matching neither end, and the ask is what
        // stops the drop being silent — the server retires a prefix on the frame's ack whether or
        // not the section in it was integrated.
        if !section.applies_to(self.interest_mirror_generation) {
            self.want_interest = true;
            return;
        }
        let peer = self.local_peer_id();
        self.interest_mirror_seeded = true;
        let resolved = apply_interest_section(
            &self.slots,
            &mut self.interest_mirror,
            &mut self.interest_events,
            peer,
            section,
        );
        // AN ENTER NAMING A SLOT THIS PEER CANNOT RESOLVE IS THE ONE CASE ONLY A CLIENT SEES. The
        // section rides an unreliable snapshot and the manifest that binds its slots rides a
        // reliable channel, with no ordering between them. Dropping the enter and letting the
        // server retire it on this frame's ack is what left the mirror permanently short.
        if !resolved {
            self.want_interest = true;
        }
    }

    /// CLIENT: adopt a whole interest set, replacing what this peer believed it held.
    ///
    /// The events are the DIFF against the old mirror rather than an enter for every slot: a resync
    /// that announced the whole set would re-announce every entity the peer already had, and a game
    /// acting on those signals would rebuild nodes it never lost.
    fn adopt_interest_table(&mut self, generation: u64, slots: &[u16]) {
        let peer = self.local_peer_id();
        let resolved = adopt_whole_set(
            &self.slots,
            &mut self.interest_mirror,
            &mut self.interest_events,
            peer,
            slots,
        );
        self.interest_mirror_generation = generation;
        self.interest_mirror_seeded = true;
        // A set this peer could not read in full leaves the ask up: the manifest that binds the
        // missing slot re-announces nothing, so calling it answered is how the hole becomes permanent.
        self.want_interest = !resolved;
    }

    /// CLIENT: emit a leave for every mirrored entity the new manifest no longer names.
    ///
    /// **ONE SIGNAL COVERS BOTH CAUSES.** "The server stopped sending you this" and "this entity
    /// unregistered" are the same fact to a game holding a node it can no longer update, and making
    /// it subscribe to two mechanisms to learn one thing is what this closes.
    ///
    /// **It runs after the manifest has been APPLIED, full or delta, against the slot table that
    /// results.** A complete table made the absence self-evident: an id the frame did not name had
    /// gone, with no removal record to lose. A delta names the retirement instead
    /// ([`ManifestDelta::removed`]), so what this reads is the table after those removals landed —
    /// and the leave is emitted only because the removal was applied, which is why every path that
    /// can lose one resolves to a full table.
    ///
    /// **Culled and unregistered on the same tick fires EXACTLY ONCE.** Both paths gate on
    /// `interest_mirror.remove()`, which answers whether the set actually held the id, so whichever
    /// arrives second finds nothing to remove and announces nothing.
    fn apply_manifest_interest(&mut self) {
        if self.interest_mirror.is_empty() {
            return;
        }
        let peer = self.local_peer_id();
        retire_unnamed_interest(
            &self.slots,
            &mut self.interest_mirror,
            &mut self.interest_events,
            peer,
        );
    }

    /// This peer's own transport id, or `0` before the transport has assigned one.
    ///
    /// What a client stamps on its own relevancy events: the signal's `peer` names the connection
    /// that lost or gained the entity, which on a client is always itself.
    fn local_peer_id(&self) -> i32 {
        self.base()
            .get_multiplayer()
            .map_or(0, |api| api.get_unique_id())
    }
}

/// The priority band one candidate row scores in, as a free function so the rule the send loop runs is the
/// rule a test can call. It was three inline branches, and the middle one — the fix that let interest
/// management ship on at all — had no gate: the only test that named it compared two `weight_for` results and
/// so asserted a property of `weight_for`.
///
/// Three cases, and the middle one is the whole of the send-order fix:
///   * culling off — no distances exist, so the term is one constant across the candidate set and cancels out
///     of a descending sort. Uniform is the honest answer rather than a guess.
///   * a row WITH an anchor — band it by its distance.
///   * a row with NO anchor — [`priority::Band::Far`], NOT its stored `0.0` distance. `PeerInterest` keeps
///     always-relevant members at `0.0` and [`priority::band_of`] reads `0.0` as `Near`, so every unanchored
///     channel (health, holster, inventory, the env sensor, the flashlight, every hatch) scored as though it were
///     in the viewer's face — four-plus channels per body outbidding the one row that says where that body is.
///     "Always relevant" is a statement about never being culled and says nothing about priority.
///
/// THE CONSEQUENCE, STATED RATHER THAN LEFT TO BE DISCOVERED: a player's OWN health, holster, inventory and
/// environment channels sit at the lowest weight in the system. `WEIGHT_OWNED` cannot lift them, because
/// `collect_entity_rows` stamps every state-lane row with `owner: 0` -- a `StateSynchronizer`'s multiplayer
/// authority is the server, not the player the channel describes, and the backend refuses to guess game
/// semantics. Starvation is still impossible (`score = staleness * weight`), so under budget pressure those
/// rows arrive later rather than never. If that ordering is ever wrong for a channel, the lever the design
/// provides is that channel's own declared `priority` export, which is what an arena uses to say a row matters
/// more than its distance suggests.
#[must_use]
fn band_for_row(culling: bool, has_anchor: bool, dist_sq: f32, band_scale: f32) -> priority::Band {
    if !culling {
        priority::Band::Near
    } else if has_anchor {
        priority::band_of(dist_sq, band_scale)
    } else {
        priority::Band::Far
    }
}

/// The center handed to the filter for a peer whose position cannot be established: one whose
/// avatar has not spawned, and every peer when no cull radius is configured.
///
/// [`PeerInterest::update_linear_into`] fails open on a non-finite center — nothing is culled by
/// distance, while the membership test still runs — which is exactly what both cases mean.
/// Blanking a peer's world because its avatar has not spawned yet is not a defensible failure mode,
/// and a radius of zero asks for no distance culling rather than for all of it.
///
/// **Saying it in the center is what lets one candidate list serve every peer.** The alternative is
/// a second list shaped for those peers, rebuilt per peer, which is the O(peers × entities) pass
/// this constant exists to delete.
const UNLOCATABLE_CENTER: [f32; 3] = [f32::NAN; 3];

/// How one gathered row is offered to the interest filter of every peer that does **not** drive it.
/// A free function so the rule the send loop runs is the rule a test can call.
///
/// Two cases:
///
/// 1. **A row with a resolved anchor** — distance-culled from that anchor, within the world the row
///    declares.
/// 2. **A row with none** — one that declares no anchor, and one whose anchor did not resolve:
///    `always` **within the world the row declares**. The distance half fails open because a missing
///    anchor is a measurement that failed; the membership half does not, because a membership is a
///    declaration and did not.
///
/// Case 2 is the one the feature exists for. It is where a positionless state channel — health,
/// inventory, a door's state — lands, and before a membership existed it had exactly one setting:
/// every peer in every world.
///
/// **This says nothing about a peer that has no radius to cull by.** That used to be a third case
/// here, which is what forced the list to be rebuilt per peer; it is now [`UNLOCATABLE_CENTER`].
#[must_use]
fn candidate_for_row(row: &EntityRow) -> InterestCandidate {
    match row.anchor {
        Some(pos) => InterestCandidate::anchored_in(row.id, pos, row.membership),
        None => InterestCandidate::always_in(row.id, row.membership),
    }
}

/// How a row is offered to the peer that **drives** it: `always`, in every world.
///
/// Never culled by anything, and deliberately not membership-tested — the peer's membership was
/// read off this very row, so the test could only restate a tautology, or, for a peer that drives
/// bodies in two worlds, cull that peer's own avatar out of its own view.
///
/// This is the only row of the tick that differs between peers. On the flat path it is swapped into
/// the shared candidate list and back out around each call, which is what sharing that list costs;
/// on the grid path it is the whole of the connection's override list, because a grid rebuilt once
/// for every peer cannot hold it. See [`filter_connection`].
#[must_use]
fn candidate_for_own_row(row: &EntityRow) -> InterestCandidate {
    InterestCandidate::always(row.id)
}

/// The tick-wide facts one connection's interest update reads: the path the session picked, the
/// index that path rebuilt, the config both were derived from, and the rows the shared candidate
/// list describes.
///
/// A bundle rather than four more parameters on [`filter_connection`], and it is also what a test
/// pins: the same call filters a connection on either path, so "the two paths agree" is a statement
/// about the send loop and not only about the core.
struct InterestPass<'a> {
    /// The verdict [`select_interest_path`] answered for this tick.
    path: InterestPath,
    /// Rebuilt once for the tick, and read only on [`InterestPath::Grid`]. On the flat path it holds
    /// whatever the last grid tick left in it and nothing queries it, so a session that flips back
    /// onto the index pays one rebuild and no allocation.
    grid: &'a InterestGrid,
    /// The config the grid was binned under. A query derives its cell scan from the size the
    /// entities were binned at, so the rebuild and every query must be handed the same one.
    cfg: &'a AoiConfig,
    /// This tick's gathered rows, ascending by id — what the shared candidate list was built from,
    /// and what a connection's overrides are rebuilt from.
    rows: &'a [EntityRow],
}

/// Which path this tick's interest pass runs, measured from the list it is about to filter with.
///
/// A free function so the rule the send loop runs is the rule a test can call. Three inputs, and
/// `PathSelector` is the rule that combines them:
///
/// * **The occupancy of the widest world**, from [`InterestOccupancy::measure`] over the shared
///   candidate list. Per world rather than per session, because the grid bins each world separately
///   and a query is measured against one world's cells.
/// * **The config**, which fixes how many cells one query rectangle covers.
/// * **The widest connection's override count.** The override list is scanned once per grid hit, so
///   a connection driving many bodies is the case the index cannot pay for. One rebuild serves the
///   whole tick, so the count taken is the largest any connection in `peer_ids` will hand it: a
///   single connection over `GRID_MAX_OVERRIDES` keeps the whole tick on the flat pass, which is
///   the conservative direction — the flat pass folds those same rows in for free.
///
/// **A session with no enter radius never selects the grid**, and it is refused twice over. The
/// caller runs the pass at all only when there is a radius or a declared membership, and
/// `PathSelector::select` refuses an enter radius of `0` (or `NaN`) before it looks at the
/// occupancy. A membership-only session has no distance to index and a rebuild would buy it
/// nothing.
fn select_interest_path(
    selector: &mut PathSelector,
    cfg: &AoiConfig,
    candidates: &[InterestCandidate],
    owned: &[(SeatId, u32)],
    peer_ids: &[i32],
    scratch: &mut OccupancyScratch,
) -> InterestPath {
    let overrides = peer_ids
        .iter()
        .map(|&peer_id| owned_rows_of(owned, peer_id).len())
        .max()
        .unwrap_or(0);
    let occupancy = InterestOccupancy::measure(candidates, scratch);
    selector.select(cfg, occupancy, overrides)
}

/// One connection's interest update, on whichever path the tick picked, with the setup and the
/// restore that path needs.
///
/// A free function so the rule the send loop runs is the rule a test can call, and so both paths'
/// bookkeeping sits in one place instead of in two arms of a loop.
///
/// **The two paths differ only in where this connection's own rows go**, because those rows are the
/// one per-connection fact in the tick:
///
/// | path | this connection's rows | what the filter reads |
/// | --- | --- | --- |
/// | `Linear` | patched into the shared candidate list, restored on the way out | the whole shared list |
/// | `Grid` | copied into `overrides` | the tick's index, plus `overrides` as `also` |
///
/// **The grid cannot hold a per-connection fact.** It is rebuilt once for every peer in the
/// session, so patching it the way the shared list is patched would state one connection's view of
/// a body to every other connection. The override list carries it instead: an id named there is
/// answered by it alone and its binned entry is ignored, so the two can never both admit it.
///
/// **The shared list is restored before this returns**, which is what stops the next connection
/// being filtered against this one's view of the tick. On the grid path the restore is free,
/// because nothing patched the list.
#[allow(clippy::too_many_arguments)]
fn filter_connection(
    pass: &InterestPass<'_>,
    mine: &[(SeatId, u32)],
    seats: &[SeatObserver],
    candidates: &mut [InterestCandidate],
    overrides: &mut Vec<InterestCandidate>,
    interest: &mut ConnectionInterest,
    scratch: &mut SeatScratch,
    delta: &mut InterestDelta,
) {
    match pass.path {
        InterestPath::Linear => {
            for &(_, index) in mine {
                candidates[index as usize] = candidate_for_own_row(&pass.rows[index as usize]);
            }
            interest.update_linear_into(pass.cfg, seats, candidates, scratch, delta);
            for &(_, index) in mine {
                candidates[index as usize] = candidate_for_row(&pass.rows[index as usize]);
            }
        }
        InterestPath::Grid => {
            overrides.clear();
            overrides.extend(
                mine.iter()
                    .map(|&(_, index)| candidate_for_own_row(&pass.rows[index as usize])),
            );
            interest.update_grid_into(pass.grid, pass.cfg, seats, overrides, scratch, delta);
        }
    }
}

/// What the trailing interest-delta section costs, and therefore what the admit loop must leave
/// unspent.
///
/// **The reserve is taken BEFORE the admit loop runs, not after it.** The loop admits entity blocks
/// until the body reaches the budget, so a section appended afterward would push the datagram past
/// [`MAX_FRAME_PAYLOAD`] — an unreliable datagram past the path MTU fragments, and a lost fragment
/// costs the whole frame.
///
/// `13 + 2 × count`: two count varints at one byte each for any count one frame can carry, a byte of
/// slack, a flat 2 bytes per slot, and **ten bytes for the leading generation** — a `u64` varint's
/// worst case, reserved in full rather than measured. The generation only reaches two bytes after 127
/// resyncs on one connection and ten is unreachable, but the reserve is what keeps an unreliable
/// datagram inside the path MTU, and a bound that holds only for small values is not a bound.
///
/// Zero for an empty section, because no flag is raised and no bytes are written.
///
/// **It can leave less than one block's worth of budget**, at the 256 B floor
/// [`OrbitNet::effective_send_budget`] clamps to: a maximal section there leaves 115 B. That does not
/// wedge the peer — the admit loop sends an oversized first block anyway rather than end the stream —
/// and the maximum is only reached on a tick with relevancy news to carry.
#[must_use]
fn interest_delta_reserve(count: usize) -> usize {
    if count == 0 {
        0
    } else {
        13 + 2 * count
    }
}

/// What one synced peer is owed by an entity-manifest publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestOwed {
    /// It already holds the table the server published. **The ordinary answer**, and the one that
    /// deletes the broadcast: a rebuild that reproduced the published table owes every current peer
    /// nothing, and a join owes every peer but the joiner nothing.
    Nothing,
    /// It holds exactly the table this delta was diffed against, so the delta applies.
    Delta,
    /// It holds some other table — it has just joined, it asked for one with
    /// [`FrameHeader::FLAG_WANT_MANIFEST`], or it rekeyed a live connection — so it is sent the
    /// whole table, addressed to it alone.
    Full,
}

/// Decide what a peer at `peer_generation` is owed when the published table moves from
/// `base_generation` to `current_generation`.
///
/// A free function so the rule the send path runs is the rule a test can call. `base` and `current`
/// are equal exactly when the rebuild changed nothing, and then no delta exists to send — every peer
/// that is behind gets the whole table, which is how a joiner is served on a tick where nothing else
/// happened.
fn manifest_owed(
    peer_generation: u64,
    base_generation: u64,
    current_generation: u64,
) -> ManifestOwed {
    if peer_generation == current_generation {
        ManifestOwed::Nothing
    } else if current_generation != base_generation && peer_generation == base_generation {
        ManifestOwed::Delta
    } else {
        ManifestOwed::Full
    }
}

/// Fill `left` and `entered` with the wire slots this peer's next snapshot frame should carry, and
/// answer whether the frame should raise [`FrameHeader::FLAG_INTEREST_DELTA`] at all.
///
/// **CULLING OFF SENDS NOTHING, and that is the first thing this decides.** With no radius and no
/// declared membership the interest pass does not run at all, so `peer.interest` is a set describing
/// a tick the session has moved on from. A naive gate would diff against it and announce a leave for
/// every entity in a session that is replicating all of them to everybody. `filtering` is the same
/// flag [`OrbitNet::interest_ran`] publishes, so the send path and the read-backs cannot disagree.
///
/// **A prefix already in flight is re-sent verbatim and nothing is mutated.** That is what lets one
/// tick stamp retire it: the ack has to reach the frame that first carried *these* entries, so the
/// entries may not move underneath it.
///
/// **A fresh prefix resolves ids to slots, and the two halves fail differently.**
///
/// | Half | An id the slot table cannot name |
/// | --- | --- |
/// | leaves | retired here — the id has unregistered, and the client's own manifest rebuild emits that leave |
/// | enters | held, and the walk stops there — its slot is inside another entity's reuse quarantine and arrives shortly |
///
/// Stopping rather than skipping is what keeps the prefix contiguous from the front, which is what
/// makes retiring it a `drain(..n)`. A held enter cannot block for ever: the entity either takes a
/// slot, or leaves interest and the leave drops the pending enter.
fn build_interest_section(
    slots: &SlotTable,
    peer: &mut PeerState,
    filtering: bool,
    current: u64,
    left: &mut Vec<u16>,
    entered: &mut Vec<u16>,
) -> bool {
    left.clear();
    entered.clear();
    if !filtering {
        return false;
    }
    peer.retire_interest_delta(current);
    if peer.interest_delta_tick.is_some() {
        // A re-send. An entry whose slot has gone since is simply absent from this copy; the counts
        // stay put so the ack retires the same range.
        left.extend(
            peer.interest_pending.leaves[..peer.interest_delta_left_sent]
                .iter()
                .filter_map(|&id| slots.slot_of(id)),
        );
        entered.extend(
            peer.interest_pending.enters[..peer.interest_delta_entered_sent]
                .iter()
                .filter_map(|&id| slots.slot_of(id)),
        );
        return true;
    }

    peer.interest_pending
        .leaves
        .retain(|&id| slots.slot_of(id).is_some());
    left.extend(
        peer.interest_pending
            .leaves
            .iter()
            .take(INTEREST_DELTA_PER_FRAME)
            .filter_map(|&id| slots.slot_of(id)),
    );
    for &id in peer
        .interest_pending
        .enters
        .iter()
        .take(INTEREST_DELTA_PER_FRAME)
    {
        let Some(slot) = slots.slot_of(id) else {
            break;
        };
        entered.push(slot);
    }
    peer.interest_delta_left_sent = left.len();
    peer.interest_delta_entered_sent = entered.len();
    // An empty section still rides once per connection, to say that the session is filtering at all.
    !left.is_empty() || !entered.is_empty() || !peer.interest_seeded
}

/// Fold one interest-delta section into a client's mirrored set, queuing what changed.
///
/// A free function so the rule the receive path runs is the rule a test can call. Two properties,
/// and both are what let an unreliable datagram carry an event at all:
///
/// * **Idempotent.** Each slot is a set operation and only a set that actually changed queues an
///   event, so the second and third copies of a re-sent section announce nothing.
/// * **An unbound slot in the `left` half is dropped in silence**, exactly as a block naming one
///   is. That is ordinary rather than hostile: a leave whose cause is an unregister names a slot the
///   very next manifest releases, and the reliable manifest can arrive first.
/// * **An unbound slot in the `entered` half is reported**, which is what the `bool` answers. Nothing
///   else will ever produce that enter — the manifest rebuild only removes — and the server retires
///   the prefix on this frame's ack whether or not the section in it was integrated.
#[must_use]
fn apply_interest_section(
    slots: &SlotTable,
    mirror: &mut std::collections::HashSet<u64>,
    events: &mut Vec<(i32, u64, bool)>,
    peer: i32,
    section: &InterestDeltaSection,
) -> bool {
    for &slot in &section.left {
        let Some(id) = slots.id_of(slot) else {
            continue;
        };
        if mirror.remove(&id) {
            events.push((peer, id, false));
        }
    }
    // An unresolvable LEAVE is not reported: the id is not in the mirror to remove, and the manifest
    // that releases the slot emits that leave from its own rebuild. An unresolvable ENTER is, because
    // nothing else will ever produce it.
    let mut resolved = true;
    for &slot in &section.entered {
        let Some(id) = slots.id_of(slot) else {
            resolved = false;
            continue;
        };
        if mirror.insert(id) {
            events.push((peer, id, true));
        }
    }
    resolved
}

/// Drop every mirrored entity the slot table no longer names, queuing a leave for each.
///
/// Run against the slot table an entity manifest has just been applied to, and an id it no longer
/// names has unregistered. That is what makes one signal cover both "you stopped being sent it" and
/// "it unregistered".
///
/// **The manifest used to be a complete table and is now a delta**, so what retires an id here is
/// the removal record the frame carried rather than the id's absence from a rebuild. The guarantee
/// moved with it: the channel is reliable and ordered, a delta refuses to apply to any table but the
/// one it was diffed against, and every path that breaks the stream is answered with a full table.
/// See [`OrbitNet::send_manifest_if_dirty`].
///
/// **Culled and unregistered on the same tick fires EXACTLY ONCE**, because this and
/// [`apply_interest_section`] both gate on the set actually holding the id.
fn retire_unnamed_interest(
    slots: &SlotTable,
    mirror: &mut std::collections::HashSet<u64>,
    events: &mut Vec<(i32, u64, bool)>,
    peer: i32,
) {
    let mut gone: Vec<u64> = mirror
        .iter()
        .copied()
        .filter(|&id| slots.slot_of(id).is_none())
        .collect();
    // A `HashSet` walk is not an order. Sorted so a game that logs the events reads the same
    // sequence on every run — the rule `ResumeTable::expire` follows, for the same reason.
    gone.sort_unstable();
    for id in gone {
        mirror.remove(&id);
        events.push((peer, id, false));
    }
}

/// Which of a client input frame's blocks ride this tick, and where the next walk starts.
///
/// A free function so the rule the send loop runs is the rule a test can call.
/// [`OrbitNet::send_client_input`] carries the reasoning; the mechanics, in order:
///
/// * The walk starts at `rotor` (taken modulo the block count, so a shrinking owned set cannot
///   index past the end) and wraps once, so every block is offered exactly once per tick.
/// * A block is admitted when it fits under `budget` **and the frame is under
///   [`MAX_INPUT_BLOCKS_PER_TICK`]**, which is the count the server refuses past. The byte budget
///   alone does not keep a frame under it: a one-property input schema packs a block into 9 bytes,
///   so a hundred owned bodies fit in one datagram and nothing here refuses any of them.
/// * The walk **continues** past a block it refused, so one that cannot fit does not starve the ones
///   behind it.
/// * The returned rotor is the first refusal THAT COULD RIDE AN EMPTY FRAME, so next tick offers it
///   first — which is what bounds how long any block waits. A block wider than the whole payload can
///   never ride, so parking on it would hand it the front of the rota for ever; the rotor passes over
///   it instead. With everything admitted it holds still, since a rota with nothing to rotate is one
///   that already sends everything.
///
/// `out` is filled with the admitted indices in **ascending** order, not walk order: the rota
/// decides which blocks ride, never what the wire looks like.
fn admit_input_blocks(
    lengths: &[usize],
    rotor: usize,
    budget: usize,
    out: &mut Vec<usize>,
) -> usize {
    out.clear();
    if lengths.is_empty() {
        return 0;
    }
    let start = rotor % lengths.len();
    let mut payload = 0usize;
    let mut refused: Option<usize> = None;
    // TWO CAPS, AND THE COUNT ONE IS NOT REDUNDANT. The server refuses past
    // `MAX_INPUT_BLOCKS_PER_TICK` blocks from one peer in one tick, and the byte budget alone does
    // not keep a frame under it: a one-property input schema packs a block into 9 bytes, so a
    // hundred owned bodies fit inside `MAX_FRAME_PAYLOAD` and nothing here refuses any of them. The
    // frame then carried the same ascending run every tick and the server truncated at the same
    // index every tick, so every body past it was never driven at all. Refusing here instead is
    // what puts the tail into the rota — `refused` is the next tick's start.
    let cap = MAX_INPUT_BLOCKS_PER_TICK as usize;
    for step in 0..lengths.len() {
        let index = (start + step) % lengths.len();
        if out.len() < cap && payload + lengths[index] <= budget {
            payload += lengths[index];
            out.push(index);
        } else if refused.is_none() && lengths[index] <= budget {
            // THE ROTOR PARKS ONLY ON A BLOCK THAT COULD RIDE AN EMPTY FRAME. One that cannot fit
            // whatever else is admitted is refused every tick, so parking on it would hand it the
            // front of the rota for ever and starve everything behind it — which the count cap made
            // reachable, since a fleet can now be refused for its size rather than its bytes.
            refused = Some(index);
        }
    }
    out.sort_unstable();
    refused.unwrap_or(start)
}

/// Adopt a whole interest set into a client's mirror, answering whether every slot resolved.
///
/// A free function so the rule the receive path runs is the rule a test can call.
///
/// **THE EVENTS ARE THE DIFF, NOT THE SET.** A resync that announced every slot would re-announce
/// every entity the peer never lost, and a game acting on those signals would rebuild nodes that were
/// never gone.
///
/// **A SLOT THIS PEER CANNOT NAME LEAVES THE SET SHORT.** The manifest that binds it is reliable and
/// will arrive, but its arrival re-announces nothing — [`retire_unnamed_interest`] only removes — so
/// adopting a set with a hole in it and calling the ask answered is how that hole becomes permanent.
/// It is the same rule a section follows, for the same reason.
#[must_use]
fn adopt_whole_set(
    slots: &SlotTable,
    mirror: &mut std::collections::HashSet<u64>,
    events: &mut Vec<(i32, u64, bool)>,
    peer: i32,
    stated: &[u16],
) -> bool {
    let mut next: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut resolved = true;
    for &slot in stated {
        match slots.id_of(slot) {
            Some(id) => {
                next.insert(id);
            }
            None => resolved = false,
        }
    }
    for &id in mirror.iter() {
        if !next.contains(&id) {
            events.push((peer, id, false));
        }
    }
    for &id in next.iter() {
        if !mirror.contains(&id) {
            events.push((peer, id, true));
        }
    }
    *mirror = next;
    resolved
}

/// Take the whole interest set a connection is owed, and retire what stating it supersedes.
///
/// A free function so the rule the send path runs is the rule a test can call — the same shape as
/// [`admit_input_blocks`].
///
/// **A MEMBER WITH NO SLOT YET IS OMITTED AND STILL HELD.** An entity can sit in a connection's
/// interest before the slot table can name it, which is why the delta path holds such an enter rather
/// than sending it and why the admit loop defers its block. Stating the set without it is right — the
/// wire has no way to say it — but dropping the enter that was holding it would be the permanent
/// divergence this frame exists to close, reintroduced by the repair itself.
///
/// So a leave is superseded whatever it named, because the set says where every entity stands and
/// absence is what a leave was going to say; an enter is superseded only if the set actually named it.
fn state_whole_interest_set(slots: &SlotTable, peer: &mut PeerState) -> (u64, Vec<u16>) {
    let mut named: Vec<u64> = Vec::new();
    let mut stated: Vec<u16> = Vec::new();
    for id in peer.interest.iter() {
        if let Some(slot) = slots.slot_of(id) {
            stated.push(slot);
            named.push(id);
        }
    }
    stated.sort_unstable();
    named.sort_unstable();
    // Saturating for the reason the manifest's is: a wrap would land on a generation some peer
    // already believes it holds, which is the one outcome that misapplies in silence.
    peer.interest_generation = peer.interest_generation.saturating_add(1);
    peer.interest_pending.leaves.clear();
    peer.interest_pending
        .enters
        .retain(|id| named.binary_search(id).is_err());
    peer.interest_delta_left_sent = 0;
    peer.interest_delta_entered_sent = 0;
    peer.interest_delta_tick = None;
    peer.interest_seeded = true;
    peer.interest_full_due = false;
    (peer.interest_generation, stated)
}

/// Whether the interest pass runs this tick.
///
/// A free function so the rule the send path runs is the rule a test can call.
///
/// **ONCE A SESSION FILTERS IT KEEPS FILTERING**, which is what `ran` carries. Every client that has
/// received a section holds a mirrored set and answers `entities_in_interest` out of it. A session
/// that switched the pass back off — by retracting its last veto, or by unregistering its last
/// non-global row — would leave every one of those mirrors frozen at the last thing it was told
/// while the server went back to answering "everything is in interest". The flag is per session and
/// clears with it, so a torn-down session starts on the fast path again.
#[must_use]
fn session_is_filtering(culling: bool, vetoing: bool, ran: bool, any_membership: bool) -> bool {
    culling || vetoing || ran || any_membership
}

/// Whether a client owes the server an input frame this tick.
///
/// A free function so the rule the send path runs is the rule a test can call — the same shape as
/// [`admit_input_blocks`].
///
/// **A FRAME WITH NO BLOCKS STILL RIDES WHEN ANY NACK IS UP.** A client whose owned bodies have not
/// been named yet is exactly the client whose manifest may have broken, and a NACK it cannot send is
/// a session that never repairs.
///
/// **AND WHEN IT OWES AN ACK**, because the input frame is the only thing that carries one. An
/// OBSERVER drives no body, so without this it sent nothing at all: its `newest_ack` never moved, so
/// every interest prefix was given up on unacknowledged, which now owes it a whole set every
/// [`INTEREST_DELTA_RETRY_TICKS`] for the rest of the session. Interest filtering is what an observer
/// is for, and it was the one connection that could never confirm any of it.
#[must_use]
fn input_frame_is_owed(
    has_blocks: bool,
    owes_ack: bool,
    want_full: bool,
    want_manifest: bool,
    want_interest: bool,
) -> bool {
    has_blocks || owes_ack || want_full || want_manifest || want_interest
}

/// The tick a block's oldest newly-landed input row starts a resim from, or `None` for no resim.
///
/// A free function so the rule the receive loop runs is the rule a test can call — the same shape
/// as [`admit_input_blocks`] above.
///
/// * **Clamped to the horizon.** Rows older than `current - RESIM_INPUT_HORIZON_TICKS` are already
///   integrated — history is truthful and any later resim replays through them — but they may not
///   start a replay themselves: a joiner's seconds-stale stamps otherwise had the server
///   resimulating its body across whole seconds, and every peer watched the frontier pose flail
///   while it settled.
/// * **Nothing at or ahead of `current`.** The forward simulation has not reached that tick yet, so
///   it will read the row when it gets there.
///
/// A row the receive path refused never reaches here: it produces no
/// [`InputIntegration::Landed`], so the caller's `earliest_novel` stays `None` and the planner is
/// not marked. That is what keeps a resim from starting at a poisoned tick.
#[must_use]
fn resim_input_from(novel_tick: u64, current: u64) -> Option<u64> {
    let floor = current.saturating_sub(RESIM_INPUT_HORIZON_TICKS);
    let from = novel_tick.max(floor);
    (from < current).then_some(from)
}

/// One seat's observer as the filter takes it: the resolved center, or [`UNLOCATABLE_CENTER`] when
/// there is none to measure from.
///
/// **The center and the world fail separately, and only the center fails here.** No radius
/// configured, or no position resolved, means this seat culls nothing *by distance*; its declared
/// world is passed through untouched, because a membership is a declaration and did not fail.
/// Blanking a viewpoint's world because its body has not spawned yet is not a defensible failure
/// mode — and deciding it per seat is what stops one seat's missing body from opening, or one seat's
/// present body from closing, the whole connection.
#[must_use]
fn seat_observer(
    cfg: &AoiConfig,
    resolved: Option<[f32; 3]>,
    membership: MembershipId,
) -> SeatObserver {
    let center = match resolved {
        Some(center) if cfg.enter_radius > 0.0 => center,
        _ => UNLOCATABLE_CENTER,
    };
    SeatObserver { center, membership }
}

/// Fill `seats` with the observers one connection's filter runs against this tick — one per seat it
/// drives, or exactly one for the connection when it observes from a single place.
///
/// **THE ANCHOR RULE WHEN A CONNECTION'S SEAT SET CHANGES**, which is why this is a function of its
/// own rather than a loop inside [`OrbitNet::update_interest`]. Adding and removing a seat mid-session
/// is a supported verb (`OrbitRollbackSynchronizer::assign_seat` / `release_seat`), so what the
/// arriving and departing ends do to the connection's interest has to be stated rather than inherited
/// from whichever body happened to sort lowest.
///
/// | Case | Observers |
/// | --- | --- |
/// | The connection declared an anchor ([`PeerAnchor::Fixed`] / [`PeerAnchor::Entity`]) | exactly one, the declared pair — a declaration collapses a connection to one viewpoint |
/// | Undeclared, some seats resolved a center | one per RESOLVED seat; unresolved seats contribute nothing |
/// | Undeclared, no seat resolved a center, but the connection DRIVES something | exactly one, unlocatable — the connection fails open |
/// | Undeclared, drives nothing, policy OPEN (the default) | exactly one, unlocatable — the connection fails open |
/// | Undeclared, drives nothing, policy CLOSED | **none** — the connection has no viewpoint, so nothing is relevant to it |
///
/// **An unresolved seat is skipped, not passed through unlocatable.** That is the row that matters for
/// a seat arriving. The connection's interest is the UNION of its seats', and an unlocatable center
/// fails open, so a seat whose body has not produced a state row yet would blank the culling of every
/// other seat on the connection for as many ticks as that body took to spawn — a full-world burst
/// down one datagram, caused by a body that is not in the world yet. It costs the arriving seat
/// nothing: every body the connection drives is `always` to it whatever any seat can see.
///
/// **Fail-open is kept at CONNECTION granularity, and CLOSED is the ONE carve-out from it.** An empty
/// output is the claim that there is no viewpoint at all, which the filter reads as an empty set and
/// which therefore withholds every entity. Exactly one case may make that claim, and it is stated as
/// a conjunction because each half is load-bearing:
///
/// * **The connection declared nothing** — a declaration is an answer, and a declared anchor that has
///   not resolved a center is a measurement that failed, not an absent viewpoint.
/// * **AND it drives no rollback row at all** — `mine` is empty. A connection whose seats exist but
///   have not RESOLVED a center yet keeps the fail-open above. That is **the joining-player
///   protection**: a player's body takes ticks to spawn, and closing that window would deny the
///   player their own avatar for every one of them, which is the failure fail-open exists to prevent.
///
/// CLOSED is therefore reachable only by a connection nothing in the session has anything to say
/// about: no declaration, no body, no seat. `set_unanchored_policy` states what that is for.
fn seat_observers_into(
    cfg: &AoiConfig,
    mine: &[(SeatId, u32)],
    seen: &[(SeatId, PeerObserver)],
    declaration: PeerDeclaration,
    out: &mut ResolvedSeats,
) {
    let PeerDeclaration {
        anchor,
        membership: declared,
        tracked,
        last,
        closed_when_unanchored,
    } = declaration;
    out.observers.clear();
    out.labels.clear();
    out.ambiguous = false;
    out.source = match anchor {
        PeerAnchor::Inferred => ANCHOR_SOURCE_INFERRED,
        PeerAnchor::Fixed(_) => ANCHOR_SOURCE_FIXED,
        PeerAnchor::Entity(_) => ANCHOR_SOURCE_ENTITY,
    };
    if matches!(anchor, PeerAnchor::Inferred) {
        // One seat per distinct label the connection's own rows declare. `mine` is sorted by seat,
        // so the run check is what deduplicates it — several bodies on one seat are one viewpoint,
        // anchored by the lowest-id one of them.
        let mut previous: Option<SeatIndex> = None;
        for &(seat_id, _) in mine {
            if previous == Some(seat_id.seat) {
                continue;
            }
            previous = Some(seat_id.seat);
            let Ok(index) = seen.binary_search_by_key(&seat_id, |&(seat, _)| seat) else {
                continue;
            };
            let observed = seen[index].1;
            out.ambiguous |= observed.ambiguous;
            let (resolved, membership) =
                resolve_observer(anchor, declared, tracked, last, Some(observed));
            out.observers.push(seat_observer(cfg, resolved, membership));
            out.labels.push(Some(seat_id.seat));
        }
    }
    if out.observers.is_empty() {
        if closed_when_unanchored && matches!(anchor, PeerAnchor::Inferred) && mine.is_empty() {
            return;
        }
        let (resolved, membership) = resolve_observer(anchor, declared, tracked, last, None);
        out.observers.push(seat_observer(cfg, resolved, membership));
        // `None`, not seat 0: this one viewpoint answers for every seat on the connection, including
        // labels the inferred path above never enumerated.
        out.labels.push(None);
    }
}

/// Fill `out` with `(seat, row index)` for every row a peer drives, ascending by seat.
///
/// Sorted so [`owned_rows_of`] can binary-search a peer's slice — and, within that slice, so a run
/// of equal seat labels is contiguous, which is what lets [`OrbitNet::update_interest`] count a
/// connection's seats without a set. `rows` arrives sorted by id and owners are scattered through
/// it, so the sort is real work, done once per tick rather than rescanning every row for every peer.
///
/// **This is where a connection's seats are enumerated**, not [`OrbitNet::collect_observers`]: a
/// seat whose body has no anchor yet contributes no observer and must still get its own set.
fn owned_rows_into(rows: &[EntityRow], out: &mut Vec<(SeatId, u32)>) {
    out.clear();
    out.extend(
        rows.iter()
            .enumerate()
            .filter(|(_, row)| row.owner != 0)
            .map(|(index, row)| (row.seat_id(), index as u32)),
    );
    out.sort_unstable();
}

/// The slice of [`owned_rows_into`]'s output that belongs to `peer_id`, ascending by seat.
///
/// Usually one entry, and never assumed to be: a connection may drive several bodies across several
/// seats, and every one of them is `always` to it.
#[must_use]
fn owned_rows_of(owned: &[(SeatId, u32)], peer_id: i32) -> &[(SeatId, u32)] {
    let start = owned.partition_point(|&(seat, _)| seat.peer < peer_id);
    let end = owned.partition_point(|&(seat, _)| seat.peer <= peer_id);
    &owned[start..end]
}

/// Which seats owe an anchor-conflict warning this tick, and the bookkeeping that makes it **once
/// per seat per episode**.
///
/// A free function so the rule the send loop runs is the rule a test can call:
/// [`OrbitNet::warn_anchor_conflicts`] is this plus `godot_warn!`, and the warning itself needs the
/// Godot runtime while the rule needs nothing.
///
/// Two steps, and the order matters:
///
/// 1. **Prune.** Every seat in `warned` that is no longer reporting a conflict is dropped — it fixed
///    its configuration, or its bodies left, or the connection did. That is what makes a mistake
///    reintroduced after a map change reportable a second time.
/// 2. **Insert.** Every seat conflicting now that was not already in `warned` is added and named in
///    `owed`. A seat conflicting on consecutive ticks is owed nothing after the first.
///
/// `observers` is ascending by seat, which is what makes the prune a binary search rather than a
/// second set. `owed` is cleared on entry.
fn anchor_conflicts_owed(
    warned: &mut std::collections::HashSet<SeatId>,
    observers: &[(SeatId, PeerObserver)],
    owed: &mut Vec<SeatId>,
) {
    owed.clear();
    if warned.is_empty() && !observers.iter().any(|(_, o)| o.membership_conflict) {
        return; // the shape of every correctly configured session, on every tick
    }
    warned.retain(|seat| {
        observers
            .binary_search_by_key(seat, |&(id, _)| id)
            .is_ok_and(|index| observers[index].1.membership_conflict)
    });
    for &(seat, observer) in observers {
        if observer.membership_conflict && warned.insert(seat) {
            owed.push(seat);
        }
    }
}

/// The direction this role SENDS in, and the direction it EXPECTS TO RECEIVE.
///
/// A free function so the one rule that must not be inverted is the rule a test can call. Getting it
/// backward would authenticate every datagram in the direction it did not travel, and the whole
/// session would refuse itself — which is loud, but it is also exactly what the direction byte exists
/// to make impossible for an attacker, so it is stated once and checked once.
///
/// `None` OFFLINE: there is no session and nothing to authenticate.
#[must_use]
fn session_directions(mode: i64) -> Option<(Direction, Direction)> {
    match mode {
        MODE_CLIENT => Some((Direction::ToServer, Direction::ToClient)),
        MODE_SERVER | MODE_HOST => Some((Direction::ToClient, Direction::ToServer)),
        _ => None,
    }
}

/// The key one session's datagrams are authenticated with, from the shared secret and the 16 bytes the
/// handshake carries.
///
/// A free function so that both ends run the same line: the client seats it in [`OrbitNet::start`], the
/// server in `handle_hello`, and a peer that derived differently from the other refuses every datagram
/// the other sends. It touches no Godot type, so a test calls it directly.
///
/// | `secret` | The key | What the handshake's 16 bytes are |
/// | --- | --- | --- |
/// | `None` | those 16 bytes, verbatim | the key |
/// | `Some` | [`derive_session_key`] of the two | a nonce |
///
/// **THE SECRET IS AN INPUT AND IS NEVER SEATED AS THE KEY.** Returning `*secret` here is one character
/// shorter and re-opens cross-session replay: [`SessionAuth`] starts every session's sequence counter at
/// 1 and the replay window only ever knows the session in front of it, so under a key that did not change
/// between joins every datagram captured in one session is a valid, unreplayed datagram in the next. The
/// per-join nonce is the only thing keeping the key per-join, which is why an all-zero nonce is refused
/// at the handshake as well.
///
/// **It changes who can forge, not how hard forging is.** The tag is still 64 bits and the key still 128,
/// and a derived key is worth exactly the entropy of the secret it came from. Nothing here encrypts
/// anything.
#[must_use]
fn session_key_from(secret: Option<&[u8; KEY_LEN]>, nonce: [u8; KEY_LEN]) -> [u8; KEY_LEN] {
    match secret {
        Some(secret) => derive_session_key(secret, &nonce),
        None => nonce,
    }
}

/// Whether a dropped peer's session should be held open for it to come back to.
///
/// - Not a server: only the authority holds sessions.
/// - No grace window configured: resume is switched off.
/// - **No identity**, which covers two different peers. One never sent a token. The other is a GHOST whose
///   identity was taken from it by a GRANTED resume — see `handle_hello` — and its late disconnect must not
///   re-open a window on an identity somebody is currently playing under. A ghost whose resume was REFUSED
///   keeps its identity, so this answers `true` for it and its own drop opens the real window the refusing
///   policy was waiting for.
#[must_use]
fn hold_on_drop(session_id: u64, grace_ms: u64, server: bool) -> bool {
    server && grace_ms > 0 && session_id != 0
}

/// The stored `resume_policy`, reduced to a value this build knows.
///
/// Total by construction, and an unknown number falls onto `ALWAYS`. That is the opposite direction from
/// [`clamp_seat_release_policy`], deliberately: there the safe answer is the one that takes nothing away,
/// here the safe answer is the one that refuses nobody. A stricter policy selected by accident locks honest
/// players out of their own bodies, and `ALWAYS` is token-gated, so falling onto it forfeits nothing the
/// token was closing.
#[must_use]
fn clamp_resume_policy(policy: i64) -> i64 {
    match policy {
        RESUME_ONLY_IF_DROPPED => RESUME_ONLY_IF_DROPPED,
        RESUME_NEVER => RESUME_NEVER,
        _ => RESUME_ALWAYS,
    }
}

/// What a hello presenting a session identity is seated as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeGrant {
    /// The claim is granted: take the identity, and report whichever connection held it as `resumed_from`.
    Resume,
    /// The claim is refused. The presenter is seated as an ordinary joiner and nothing is taken from
    /// anybody — no identity is stripped, no held window is spent.
    Newcomer,
}

/// Whether a hello quoting `presented_token` may resume the identity it named. A free function so the whole
/// rule is one thing a test can call with no `SceneTree`.
///
/// The order is the rule, and each step is a refusal the next one cannot undo:
///
/// | Step | Answer |
/// | --- | --- |
/// | `policy` is `NEVER` | `Newcomer` |
/// | a non-zero `token_on_record` the presented token does not match | `Newcomer` |
/// | `incumbent_is_live` under any policy but `ALWAYS` | `Newcomer` |
/// | otherwise | `Resume` |
///
/// **THE TOKEN IS THE STEP THAT CLOSES THE REACHABLE HOLE.** Matching on the identity alone let a peer that
/// merely OBSERVED another's session id — off a roster broadcast, a kill feed, a log line, a screenshot —
/// take that player's body: the incumbent kept its connection, received no error, and simply stopped
/// driving its entity. A claim now has to quote a value the server minted and sent only to the client that
/// owned the identity.
///
/// **What it does not close is an on-path observer**, who reads the welcome and can quote the token
/// verbatim. That is the same boundary the session key already has, and it closes the same way — a shared
/// session secret. Under one, that observer can still copy the token but cannot authenticate the handshake
/// that quotes it.
///
/// **`token_on_record` of `0` grants on the identity alone**, which is what a server that has minted no
/// token for that identity yet has. That is a first-time join, and refusing it would refuse everybody.
///
/// A policy this build does not know reads as `ALWAYS`; see [`clamp_resume_policy`].
#[must_use]
fn resume_grant(
    policy: i64,
    presented_token: u64,
    token_on_record: u64,
    incumbent_is_live: bool,
) -> ResumeGrant {
    let policy = clamp_resume_policy(policy);
    if policy == RESUME_NEVER {
        return ResumeGrant::Newcomer;
    }
    if token_on_record != 0 && presented_token != token_on_record {
        return ResumeGrant::Newcomer;
    }
    if incumbent_is_live && policy != RESUME_ALWAYS {
        return ResumeGrant::Newcomer;
    }
    ResumeGrant::Resume
}

/// How one hello is seated: the identity the connection gets, what it resumed, and the two values the
/// caller needs to issue its resume token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HelloSeat {
    /// The identity to seat this connection under. `hello.session_id` on a granted resume, and `0` when a
    /// refused claim named an identity somebody else still holds.
    session_id: u64,
    /// The transport peer id this connection took over from, or `0`. Reported to the game as `resumed_from`.
    resumed_from: i32,
    /// Whether the claim was granted. Decided by [`resume_grant`].
    grant: ResumeGrant,
    /// The token the server had on record for the presented identity before this hello, or `0`.
    token_on_record: u64,
}

/// Seat one hello: decide the resume, take the identity off a superseded incumbent, and spend the held
/// window.
///
/// **A free function over the two plain tables, and every rule this defect lived in is inside it.** It
/// touches no Godot type, so a test constructs a `HashMap<i32, PeerState>` and a [`ResumeTable`] and calls
/// the same code the receive path runs.
///
/// The order is load-bearing:
///
/// 1. **Find the incumbent and the token on record BEFORE anything mutates.** An `incumbent` is a CONNECTED
///    peer still claiming the presented identity — usually a GHOST, a connection the transport has not
///    declared dead yet, which on ENet's defaults takes the better part of a minute. Both the grant and the
///    claim are made against these two values, and re-deriving either from a table the other has already
///    changed would answer a different question.
/// 2. **The held record wins as the source of the token.** It is the drop the server actually saw, and its
///    copy was taken off the connection that departed. An incumbent's own token answers when nothing is
///    held, which is the fast-reconnect case.
/// 3. **The supersede step runs ONLY on a granted resume.** It takes the identity off the incumbent, so
///    running it under a refusal would leave that incumbent unable to hold its own window when its
///    disconnect finally lands — [`hold_on_drop`] refuses identity `0` — and the player who was actually
///    playing would lose the seat to the peer that was just told it could not have it.
/// 4. **Claiming REMOVES**, so the identity is spent here and a later connection quoting the same token
///    resumes nothing. A held session wins over a superseded ghost when both somehow exist.
/// 5. **A refused claim is seated ANONYMOUSLY whenever anything else still holds the identity**, live or
///    merely held. Two live peers under one identity is the state that makes `peer_session_id` and
///    `is_session_held` lie, and seating a refused claimant under a HELD identity is worse still: its own
///    later drop would overwrite the held record, token and all, and take the identity from the player it
///    belongs to permanently. A refusal with nothing on record keeps the identity — that is a first-time
///    joiner under `NEVER`, and taking its identity away protects nobody while leaving a game under that
///    policy with no roster key at all.
fn seat_hello(
    peers: &mut HashMap<i32, PeerState>,
    resume: &mut ResumeTable,
    policy: i64,
    sender: i32,
    session_id: u64,
    presented_token: u64,
) -> HelloSeat {
    let mut incumbent: Option<i32> = None;
    if session_id != 0 {
        for (&id, state) in peers.iter() {
            if id != sender && state.session_id == session_id {
                incumbent = Some(id);
            }
        }
    }
    let token_on_record = match resume.token_of(session_id) {
        0 => incumbent
            .and_then(|id| peers.get(&id))
            .map_or(0, |state| state.resume_token),
        held => held,
    };
    let grant = resume_grant(
        policy,
        presented_token,
        token_on_record,
        incumbent.is_some(),
    );
    let identity_is_taken = incumbent.is_some() || resume.holds(session_id);
    if grant == ResumeGrant::Newcomer {
        return HelloSeat {
            session_id: if identity_is_taken { 0 } else { session_id },
            resumed_from: 0,
            grant,
            token_on_record,
        };
    }
    let mut superseded = 0;
    if session_id != 0 {
        for (&id, state) in peers.iter_mut() {
            if id != sender && state.session_id == session_id {
                state.session_id = 0;
                superseded = id;
            }
        }
    }
    HelloSeat {
        session_id,
        resumed_from: resume
            .claim(session_id, presented_token)
            .unwrap_or(superseded),
        grant,
        token_on_record,
    }
}

/// The tick a masked delta may reference, or `None` when the sender must send a full row instead.
///
/// **A BASE THE RECEIVER CANNOT STILL HOLD IS NOT A BASE.** `acked_base` records that the peer once
/// acknowledged a frame carrying this row. It does not record that the row is still RESIDENT. A
/// receiver keeps its authoritative rows in a direct-mapped ring of `history_limit` ticks
/// (`Synchronizer::keep_auth_row`), so the row for tick `t` is overwritten the moment a row for
/// `t + history_limit` is written into the same slot.
///
/// The two run apart under loss. `acked_base` only advances when an ack arrives, and a lost ack
/// leaves it frozen while `current` runs on -- so the gap widens on exactly the links that can least
/// afford what happens next.
///
/// **What happens next is the expensive part.** A delta against an evicted base decodes to
/// [`StateIntegration::NoBase`], which raises the per-peer, ALL-ENTITY `want_full`. The server
/// answers that by marking every entity in the peer's next frame full-due, the frame budget carries
/// a fraction of them, and the rest defer -- so one unanswerable delta buys a multi-tick full-state
/// burst, and the flag re-arms while it drains. Degrading to a full block here spends the same bytes
/// the NACK was going to cost anyway, without the round trip and without the burst.
///
/// Conservative in the safe direction: a base inside the span may still have been evicted if the
/// receiver wrote a colliding tick, and a full block is always decodable, so a false full costs
/// bytes while a false delta costs a storm.
#[must_use]
fn delta_reference(base: u64, current: u64, span: u64) -> Option<u64> {
    if current.saturating_sub(base) < span {
        Some(base)
    } else {
        None
    }
}

/// Whether this entity's next block for this peer must be a full row. A free function so the rule
/// the send loop runs is the rule a test can call.
///
/// Two reasons:
/// - The peer NACKed. `want_full` is per-peer and all-entity, so it names no particular entity and
///   cannot be the only repair.
/// - The keyframe is due (see [`FULL_STATE_INTERVAL`]), phase-spread across the interval by entity
///   id. `last_full == 0` covers an entity nothing full has ever gone out for: a fresh one, or one
///   whose bookkeeping was cleared when it left this peer's interest. The arithmetic reaches that
///   case on its own, since `current - 0 >= interval`; it is written out to say so.
///
/// The clock is `last_full`, and `last_sent` cannot stand in for it. A keyframe exists to clear a
/// delta chain the receiver cannot decode, and such a chain keeps being **sent** — once per rota
/// visit, as another delta the receiver discards. Measured from `last_sent` the interval never
/// elapses for the entities that need it, and fires only for entities the rota already skipped for
/// a whole interval, which have nothing to repair.
#[must_use]
fn full_block_due(want_full: bool, id: u64, current: u64, last_full: u64, interval: u64) -> bool {
    want_full
        || last_full == 0
        || (orbitnet_core::interest::send_phase(id, current, interval)
            && current.saturating_sub(last_full) >= interval)
}

/// The stored `seat_release_policy`, reduced to a value this build knows.
///
/// Total by construction: an unknown number is `HOLD`, which is the policy that releases nothing.
/// A build that gains a fourth policy therefore reads an older project's stored value correctly, and
/// an older build reads a newer project's as "do nothing" rather than as whichever policy happens to
/// sit at that number.
#[must_use]
fn clamp_seat_release_policy(policy: i64) -> i64 {
    match policy {
        SEAT_RELEASE_ON_EXPIRY => SEAT_RELEASE_ON_EXPIRY,
        SEAT_RELEASE_ON_DROP => SEAT_RELEASE_ON_DROP,
        _ => SEAT_RELEASE_HOLD,
    }
}

/// The core policy the stored `seat_release_policy` selects. Unknown values select `Hold`, the same
/// way [`clamp_seat_release_policy`] does, so the property and the behavior cannot disagree.
#[must_use]
fn seat_release_policy_of(policy: i64) -> SeatReleasePolicy {
    match policy {
        SEAT_RELEASE_ON_EXPIRY => SeatReleasePolicy::OnExpiry,
        SEAT_RELEASE_ON_DROP => SeatReleasePolicy::OnDrop,
        _ => SeatReleasePolicy::Hold,
    }
}

/// Queue `peer` for a seat release at the next frame boundary, at most once.
///
/// **Deduplicated**, because a peer id that drops, is reused by a joiner and drops again inside one
/// frame would otherwise be walked twice for the same answer. A linear scan is the right shape: the
/// vector holds the connections that dropped since the last frame, which is empty on almost every
/// frame and single-digit on the worst one.
fn queue_seat_release(pending: &mut Vec<i32>, peer: i32) {
    if !pending.contains(&peer) {
        pending.push(peer);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        admit_input_blocks, adopt_whole_set, anchor_conflicts_owed, apply_interest_section,
        band_for_row, build_interest_section, candidate_for_own_row, candidate_for_row,
        clamp_resume_policy, clamp_seat_release_policy, clamp_unanchored_policy, classify_rx,
        delta_reference, encode_interest_delta, filter_connection, full_block_due, hold_on_drop,
        input_frame_is_owed, interest_delta_reserve, is_located, manifest_owed, owned_rows_into,
        owned_rows_of, queue_seat_release, resim_input_from, resolve_observer, resume_grant,
        retire_unnamed_interest, rtt_at_ceiling_peers, seat_hello, seat_observer,
        seat_observers_into, seat_release_policy_of, select_interest_path, session_directions,
        session_is_filtering, session_key_from, state_whole_interest_set, AckOutcome, EntityRow,
        FrameHeader, InterestPass, ManifestOwed, OrbitNet, PeerAnchor, PeerDeclaration,
        PeerObserver, PeerState, ResolvedSeats, ResumeGrant, ResumeTable, RxOutcome, SeatId,
        SeatIndex, SeatReleaseEvent, SeatReleasePolicy, SlotTable, StateIntegration, Writer,
        ANCHOR_SOURCE_FIXED, ANCHOR_SOURCE_INFERRED, AOI_EXIT_FACTOR, FULL_STATE_INTERVAL,
        INTEREST_DELTA_PENDING_HARD_MAX, INTEREST_DELTA_PENDING_MAX, INTEREST_DELTA_PER_FRAME,
        INTEREST_DELTA_RETRY_TICKS, MAX_FRAME_PAYLOAD, MAX_INPUT_BLOCKS_PER_TICK, MODE_CLIENT,
        MODE_HOST, MODE_OFFLINE, MODE_SERVER, RESUME_ALWAYS, RESUME_NEVER, RESUME_ONLY_IF_DROPPED,
        RTT_BELIEVED_MAX_MS_DEFAULT, RTT_SAMPLE_MAX_MS, RTT_WINDOW, SEAT_RELEASE_HOLD,
        SEAT_RELEASE_ON_DROP, SEAT_RELEASE_ON_EXPIRY, UNANCHORED_CLOSED, UNANCHORED_OPEN,
        UNLOCATABLE_CENTER,
    };
    use orbitnet_core::codec::InterestDeltaSection;
    use std::collections::HashMap;

    use orbitnet_core::interest::{
        AoiConfig, ConnectionInterest, InterestCandidate, InterestDelta, InterestGrid,
        InterestPath, MembershipId, OccupancyScratch, PathSelector, PeerInterest, SeatObserver,
        SeatScratch, GRID_MAX_OVERRIDES, MEMBERSHIP_GLOBAL,
    };
    use orbitnet_core::priority::Band;
    use orbitnet_core::seats::releases_seats;
    use orbitnet_core::{
        compress_secret, AuthError, ColumnarHistory, Confidence, Direction, FreshnessLedger,
        PropKind, PropRole, ResimPlanner, ResimRange, SchemaBuilder, SessionAuth, KEY_LEN,
    };

    use crate::sync::{input_restore_row, integrate_input_row, InputIntegration};

    // ------------------------------------------------------------------
    // The entity manifest: what a publish owes each peer, and every path that zeroes a generation.
    // ------------------------------------------------------------------

    /// A rebuild that reproduced the published table publishes NOTHING to a peer that is current,
    /// which is the saving. The same publish still owes a joiner the whole table, which is the case
    /// a naive "nothing changed, return early" would strand.
    #[test]
    fn a_publish_that_changed_nothing_owes_a_current_peer_nothing_and_a_joiner_the_table() {
        // Nothing changed: base and current are the same generation, so no delta exists to send.
        assert_eq!(manifest_owed(7, 7, 7), ManifestOwed::Nothing);
        assert_eq!(manifest_owed(0, 7, 7), ManifestOwed::Full, "a joiner");
        assert_eq!(
            manifest_owed(3, 7, 7),
            ManifestOwed::Full,
            "and a straggler"
        );

        // A session that has published nothing owes a joiner nothing either: generation 0 is the
        // empty table on both sides, and the client's table starts empty.
        assert_eq!(manifest_owed(0, 0, 0), ManifestOwed::Nothing);
    }

    /// A publish that DID change the table: the peers holding the base take the one delta, and
    /// everybody else takes the whole table addressed to them alone.
    #[test]
    fn a_publish_that_changed_the_table_sends_one_delta_and_a_full_to_the_rest() {
        assert_eq!(manifest_owed(7, 7, 8), ManifestOwed::Delta);
        assert_eq!(
            manifest_owed(8, 7, 8),
            ManifestOwed::Nothing,
            "already there"
        );
        assert_eq!(manifest_owed(0, 7, 8), ManifestOwed::Full, "a joiner");
        assert_eq!(
            manifest_owed(6, 7, 8),
            ManifestOwed::Full,
            "behind the base"
        );
        assert_eq!(manifest_owed(9, 7, 8), ManifestOwed::Full, "ahead of it");

        // A server that coalesced several dirty ticks into one delta still names the base it
        // diffed against, so the peers holding that base take the delta.
        assert_eq!(manifest_owed(7, 7, 19), ManifestOwed::Delta);
        assert_eq!(manifest_owed(18, 7, 19), ManifestOwed::Full);
    }

    /// The generation cannot be reached, but if it ever saturated the table must degrade to full
    /// frames rather than wrap onto a number a peer already believes it holds.
    #[test]
    fn a_saturated_generation_degrades_to_full_tables() {
        assert_eq!(u64::MAX.saturating_add(1), u64::MAX);
        assert_eq!(
            manifest_owed(u64::MAX, u64::MAX, u64::MAX),
            ManifestOwed::Nothing
        );
        assert_eq!(manifest_owed(3, u64::MAX, u64::MAX), ManifestOwed::Full);
    }

    /// **Every path that can desynchronize a peer has to zero its generation**, and a reviewer's
    /// whole job on this change is confirming that. Three paths, and this pins the two a live
    /// connection takes plus the state the third arrives in.
    #[test]
    fn every_break_in_the_manifest_stream_resolves_to_a_full_table() {
        // A RECONNECT. A dropped peer is removed from `peers`, so the rejoiner is seated on a fresh
        // `PeerState` — this is the state it starts in, and it is owed the whole table.
        let fresh = PeerState::default();
        assert_eq!(fresh.manifest_generation, 0);
        assert_eq!(
            manifest_owed(fresh.manifest_generation, 8, 9),
            ManifestOwed::Full
        );

        // A REKEY on a live connection, and a WANT_MANIFEST NACK: both run `forget_manifest`, and
        // both leave the peer owed the whole table on the very next publish — including one that
        // changed nothing, since `send_manifest_if_dirty` is re-entered for the NACK.
        let mut held = PeerState {
            manifest_generation: 8,
            ..Default::default()
        };
        assert_eq!(
            manifest_owed(held.manifest_generation, 8, 9),
            ManifestOwed::Delta
        );
        held.forget_manifest();
        assert_eq!(held.manifest_generation, 0);
        assert_eq!(
            manifest_owed(held.manifest_generation, 8, 9),
            ManifestOwed::Full
        );
        assert_eq!(
            manifest_owed(held.manifest_generation, 9, 9),
            ManifestOwed::Full
        );
    }

    /// The manifest NACK rides the same flags byte as the delta-base NACK and is independent of it.
    /// A client that lost both on one tick raises both, and the server reads each on its own.
    #[test]
    fn the_two_client_nacks_share_a_byte_and_nothing_else() {
        let both = FrameHeader::FLAG_WANT_FULL | FrameHeader::FLAG_WANT_MANIFEST;

        let mut peer = PeerState {
            manifest_generation: 4,
            ..Default::default()
        };
        peer.acked_base.insert(11, 100);
        if both & FrameHeader::FLAG_WANT_FULL != 0 {
            peer.note_nack();
        }
        if both & FrameHeader::FLAG_WANT_MANIFEST != 0 {
            peer.forget_manifest();
        }
        assert!(
            peer.want_full,
            "the delta-base NACK still asks for full rows"
        );
        assert!(peer.acked_base.is_empty());
        assert_eq!(peer.manifest_generation, 0);

        // And a manifest NACK on its own leaves the delta bases alone: they are unrelated failures.
        let mut manifest_only = PeerState {
            manifest_generation: 4,
            ..Default::default()
        };
        manifest_only.acked_base.insert(11, 100);
        if FrameHeader::FLAG_WANT_MANIFEST & FrameHeader::FLAG_WANT_FULL != 0 {
            manifest_only.note_nack();
        }
        manifest_only.forget_manifest();
        assert!(!manifest_only.want_full);
        assert_eq!(manifest_only.acked_base.len(), 1);
        assert_eq!(manifest_only.manifest_generation, 0);
    }

    // ------------------------------------------------------------------
    // Membership: how a gathered row reaches the filter, and where a peer's own world comes from.
    // ------------------------------------------------------------------

    fn row(id: u64, owner: i32, anchor: Option<[f32; 3]>, membership: MembershipId) -> EntityRow {
        row_seat(id, owner, 0, anchor, membership)
    }

    /// The same row, driven by a named seat on `owner`'s connection.
    fn row_seat(
        id: u64,
        owner: i32,
        seat: SeatIndex,
        anchor: Option<[f32; 3]>,
        membership: MembershipId,
    ) -> EntityRow {
        EntityRow {
            id,
            owner,
            seat,
            anchor,
            membership,
            priority: 1,
        }
    }

    fn seat_of(peer: i32, seat: SeatIndex) -> SeatId {
        SeatId { peer, seat }
    }

    /// One seat's inferred pair, unambiguous: the shape `collect_observers` produces for a seat
    /// driving exactly one anchored body.
    fn observed(center: [f32; 3], membership: MembershipId) -> PeerObserver {
        PeerObserver {
            center,
            membership,
            ambiguous: false,
            membership_conflict: false,
        }
    }

    /// The case the feature exists for. A channel with no anchor has no distance to be culled by, so
    /// it goes in as `always` — but `always` must now carry the row's world, or the channel keeps its
    /// one pre-membership setting of "every peer in every world".
    #[test]
    fn a_row_with_no_anchor_is_always_relevant_within_its_own_world() {
        let candidate = candidate_for_row(&row(7, 0, None, 3));
        assert_eq!(candidate, InterestCandidate::always_in(7, 3));
        assert!(candidate.always, "no anchor still means no distance test");
        assert_eq!(
            candidate.membership, 3,
            "and it is still bounded to world 3"
        );
    }

    /// An anchor that did not resolve fails open on DISTANCE. Its membership is a declaration rather
    /// than a measurement and did not fail, so it must not fail open too.
    #[test]
    fn an_unresolved_anchor_fails_open_on_distance_only() {
        let candidate = candidate_for_row(&row(7, 0, None, 9));
        assert!(candidate.always);
        assert_eq!(candidate.membership, 9);
    }

    /// A peer that cannot be located culls nothing by distance, and the center is where that is
    /// said — not in the candidate list, which is why the list can be shared.
    ///
    /// The membership half does NOT fail open with it: an unlocatable peer reads as
    /// `MEMBERSHIP_GLOBAL`, which matches every world, but a peer that is merely out of radius keeps
    /// the world it declared.
    #[test]
    fn an_unlocatable_center_admits_every_row_it_is_offered() {
        let cfg = AoiConfig {
            cell_size: 8.0,
            enter_radius: 4.0,
            exit_factor: 1.25,
            max_entities: 1,
        };
        let rows = [
            row(1, 0, Some([9_000.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
            row(2, 0, Some([0.0, 0.0, 0.0]), 5),
            row(3, 0, None, 7),
        ];
        let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let (mut scratch, mut leaves) = (Vec::new(), Vec::new());

        let mut interest = PeerInterest::new();
        interest.update_linear_into(
            &cfg,
            UNLOCATABLE_CENTER,
            MEMBERSHIP_GLOBAL,
            &candidates,
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(
            interest.iter().collect::<Vec<_>>(),
            vec![1, 2, 3],
            "9 km away and a cap of one, and nothing is culled: the peer measured nothing"
        );

        // The same rows from a center that IS locatable, to prove the list itself culls normally.
        let mut located = PeerInterest::new();
        located.update_linear_into(
            &cfg,
            [0.0; 3],
            MEMBERSHIP_GLOBAL,
            &candidates,
            &mut scratch,
            &mut leaves,
        );
        assert_eq!(located.iter().collect::<Vec<_>>(), vec![2, 3]);
    }

    /// The center fails open per SEAT and the world does not fail at all — the pair
    /// `update_interest` hands the filter for one viewpoint.
    #[test]
    fn a_seat_without_a_center_culls_nothing_by_distance_and_keeps_its_world() {
        let cfg = AoiConfig {
            cell_size: 8.0,
            enter_radius: 100.0,
            exit_factor: 1.25,
            max_entities: 0,
        };
        let unlocated = seat_observer(&cfg, None, 5);
        assert!(unlocated.center[0].is_nan(), "no center, no distance test");
        assert_eq!(unlocated.membership, 5, "a declared world did not fail");

        // A radius of zero asks for no distance culling rather than for all of it, and it says so
        // in the center — which is what lets every seat share one candidate list.
        let no_radius = AoiConfig {
            enter_radius: 0.0,
            ..cfg
        };
        assert!(seat_observer(&no_radius, Some([1.0; 3]), 5).center[0].is_nan());

        let located = seat_observer(&cfg, Some([1.0, 2.0, 3.0]), 5);
        assert_eq!(located.center, [1.0, 2.0, 3.0]);
    }

    /// Everything `update_interest` resolves for one connection, from a row set: the observers, the
    /// seat label each belongs to, the source, and whether any seat's anchor was an arbitrary pick.
    fn resolve_for(
        cfg: &AoiConfig,
        rows: &[EntityRow],
        peer: i32,
        anchor: PeerAnchor,
        declared: MembershipId,
        closed_when_unanchored: bool,
    ) -> ResolvedSeats {
        let mut observers = Vec::new();
        OrbitNet::collect_observers(rows, &mut observers);
        let mut owned = Vec::new();
        owned_rows_into(rows, &mut owned);
        let mut seats = ResolvedSeats::default();
        seat_observers_into(
            cfg,
            owned_rows_of(&owned, peer),
            OrbitNet::observers_of(&observers, peer),
            PeerDeclaration {
                anchor,
                membership: declared,
                tracked: None,
                last: None,
                closed_when_unanchored,
            },
            &mut seats,
        );
        seats
    }

    /// The observers alone, under the default OPEN policy — the shape every case predating the
    /// policy asserts against.
    fn observers_for(
        cfg: &AoiConfig,
        rows: &[EntityRow],
        peer: i32,
        anchor: PeerAnchor,
        declared: MembershipId,
    ) -> Vec<SeatObserver> {
        resolve_for(cfg, rows, peer, anchor, declared, false).observers
    }

    fn radius_cfg(enter_radius: f32) -> AoiConfig {
        AoiConfig {
            cell_size: 8.0,
            enter_radius,
            exit_factor: 1.25,
            max_entities: 0,
        }
    }

    /// **The failure the per-seat center removes**, composed the way `update_interest` composes it:
    /// two seats on one connection, each with its own anchored body.
    ///
    /// Culling used to be decided per connection, so whichever body sorted lowest supplied the center
    /// for both and the other player had its surroundings culled around a position it was nowhere
    /// near. Per seat, each measures from its own body.
    #[test]
    fn each_seat_is_centered_on_its_own_body() {
        let cfg = radius_cfg(50.0);
        let rows = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row_seat(2, 42, 1, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
            row(3, 0, Some([905.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL), // scenery beside seat 1 only
        ];
        let seats = observers_for(&cfg, &rows, 42, PeerAnchor::Inferred, MEMBERSHIP_GLOBAL);
        assert_eq!(seats.len(), 2, "one viewpoint per seat");
        assert_eq!(seats[0].center, [0.0; 3]);
        assert_eq!(seats[1].center, [900.0, 0.0, 0.0]);

        let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let mut connection = ConnectionInterest::new();
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        connection.update_linear_into(&cfg, &seats, &candidates, &mut scratch, &mut delta);
        assert_eq!(
            connection.iter().collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the union carries what either seat can see"
        );
    }

    /// **THE RULE A SEAT ARRIVING NEEDS.** A seat whose body has no state row yet contributes no
    /// viewpoint while another seat on the connection has one.
    ///
    /// The union is what makes this matter: an unlocatable center refuses nothing, so passing the
    /// arriving seat through as unlocatable would blank the CONNECTION's culling — every far row in
    /// every world admitted down one datagram — for as many ticks as the new body took to spawn.
    /// That is a full-state burst caused by a body that is not in the world yet, and it arrives
    /// exactly when a player is being seated.
    #[test]
    fn a_seat_that_has_not_spawned_does_not_open_the_connections_culling() {
        let cfg = radius_cfg(50.0);
        let rows = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL), // seated and spawned
            row_seat(2, 42, 1, None, MEMBERSHIP_GLOBAL),           // just seated, no state row yet
            row(3, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL), // far scenery
        ];
        let seats = observers_for(&cfg, &rows, 42, PeerAnchor::Inferred, MEMBERSHIP_GLOBAL);
        assert_eq!(seats.len(), 1, "the unresolved seat contributes nothing");
        assert_eq!(seats[0].center, [0.0; 3]);

        let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let mut connection = ConnectionInterest::new();
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        connection.update_linear_into(&cfg, &seats, &candidates, &mut scratch, &mut delta);
        assert_eq!(
            connection.iter().collect::<Vec<_>>(),
            vec![1, 2],
            "the far row stays culled while the arriving seat has nowhere to observe from"
        );
        assert!(
            delta.leaves.is_empty(),
            "and nothing the connection already held left"
        );
    }

    /// The other half of the same rule: fail-open survives, at CONNECTION granularity.
    ///
    /// A connection whose every seat is still unresolved — a peer being seated for the first time,
    /// before any of its bodies has a state row — is not distance-culled at all. Refusing rows there
    /// would leave a joining player with an empty world, which is the failure the fail-open direction
    /// exists to prevent.
    #[test]
    fn a_connection_with_no_resolved_seat_still_fails_open() {
        let cfg = radius_cfg(50.0);
        let rows = [
            row_seat(1, 42, 0, None, MEMBERSHIP_GLOBAL),
            row_seat(2, 42, 1, None, MEMBERSHIP_GLOBAL),
            row(3, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];
        let seats = observers_for(&cfg, &rows, 42, PeerAnchor::Inferred, MEMBERSHIP_GLOBAL);
        assert_eq!(
            seats.len(),
            1,
            "never empty — an empty slice is no viewpoint"
        );
        assert!(seats[0].center[0].is_nan(), "and it refuses nothing");
        assert_eq!(seats[0].membership, MEMBERSHIP_GLOBAL);

        // A connection that drives nothing at all reaches the same place.
        let none = observers_for(&cfg, &rows, 99, PeerAnchor::Inferred, MEMBERSHIP_GLOBAL);
        assert_eq!(none.len(), 1);
        assert!(none[0].center[0].is_nan());
    }

    /// A seat LEAVING takes its viewpoint with it, and leaves the connection's other seats alone.
    #[test]
    fn releasing_a_seat_removes_that_viewpoint_and_no_other() {
        let cfg = radius_cfg(50.0);
        let seated = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row_seat(2, 42, 1, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];
        assert_eq!(
            observers_for(&cfg, &seated, 42, PeerAnchor::Inferred, MEMBERSHIP_GLOBAL).len(),
            2
        );

        // `release_seat` hands the body's input back to the server: same body, same position, no
        // longer this connection's. The body stays replicated — what left is the viewpoint.
        let released = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row_seat(2, 1, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];
        let seats = observers_for(&cfg, &released, 42, PeerAnchor::Inferred, MEMBERSHIP_GLOBAL);
        assert_eq!(seats.len(), 1);
        assert_eq!(
            seats[0].center, [0.0; 3],
            "the seat that stayed is untouched"
        );
    }

    /// A DECLARATION collapses a split-screen connection to one viewpoint, whatever its seats are
    /// doing — including while a seat is arriving. The precedence that stops a declared center from
    /// falling back to an avatar's applies to the seat split as well.
    #[test]
    fn a_declared_anchor_is_one_viewpoint_however_many_seats_the_connection_has() {
        let cfg = radius_cfg(50.0);
        let rows = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row_seat(2, 42, 1, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];
        let seats = observers_for(&cfg, &rows, 42, PeerAnchor::Fixed([5.0, 6.0, 7.0]), 3);
        assert_eq!(seats.len(), 1);
        assert_eq!(seats[0].center, [5.0, 6.0, 7.0]);
        assert_eq!(seats[0].membership, 3, "the declared world, not a body's");
    }

    // ------------------------------------------------------------------
    // "Declare nothing, receive nothing": the opt-in CLOSED policy and the carve-out that bounds it.
    // ------------------------------------------------------------------

    /// THE CASE THE POLICY EXISTS FOR. A connection that declared no anchor and drives no rollback
    /// row gets no viewpoint at all, and an empty viewpoint set makes nothing relevant.
    ///
    /// Under OPEN — today's behavior and still the default — the same connection is handed one
    /// unlocatable observer in every world, which makes every candidate uncullable, and `apply_cap`
    /// keeps every uncullable entry regardless of `aoi_max_entities`. So it receives the whole
    /// session with the nearest-N cap not bounding it.
    #[test]
    fn a_closed_connection_that_drives_nothing_gets_no_viewpoint() {
        let cfg = radius_cfg(50.0);
        let rows = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row(2, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];

        // Peer 7 is in the session and drives none of it: a spectator that declared nothing.
        let open = resolve_for(
            &cfg,
            &rows,
            7,
            PeerAnchor::Inferred,
            MEMBERSHIP_GLOBAL,
            false,
        );
        assert_eq!(open.observers.len(), 1, "OPEN still fails open");
        assert!(open.observers[0].center[0].is_nan());

        let closed = resolve_for(
            &cfg,
            &rows,
            7,
            PeerAnchor::Inferred,
            MEMBERSHIP_GLOBAL,
            true,
        );
        assert!(
            closed.observers.is_empty(),
            "no viewpoint is the whole mechanism — the filter reads it as an empty set"
        );
        assert!(closed.labels.is_empty(), "and it belongs to no seat");
        assert_eq!(
            closed.source, ANCHOR_SOURCE_INFERRED,
            "the inference ran and produced nothing; it is not 'no connection'"
        );

        // What that means on the wire: the connection's interest is empty, so every row is culled.
        let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let mut connection = ConnectionInterest::new();
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        connection.update_linear_into(
            &cfg,
            &closed.observers,
            &candidates,
            &mut scratch,
            &mut delta,
        );
        assert_eq!(connection.iter().collect::<Vec<_>>(), Vec::<u64>::new());
    }

    /// **THE CARVE-OUT, AND IT IS THE JOINING-PLAYER PROTECTION.** A connection whose seat exists but
    /// has not RESOLVED a center yet keeps the connection-wide fail-open under CLOSED as well.
    ///
    /// A player's body takes ticks to spawn and to produce its first state row. Closing that window
    /// would deny the player their own avatar for every one of those ticks — the exact failure
    /// fail-open exists to prevent — and it would do it to a player who did nothing but join.
    #[test]
    fn a_closed_connection_with_an_unresolved_seat_still_fails_open() {
        let cfg = radius_cfg(50.0);
        let rows = [
            // Seated on 42, no state row yet: the connection drives a row, it just cannot be located.
            row_seat(1, 42, 0, None, MEMBERSHIP_GLOBAL),
            row(2, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];
        let closed = resolve_for(
            &cfg,
            &rows,
            42,
            PeerAnchor::Inferred,
            MEMBERSHIP_GLOBAL,
            true,
        );
        assert_eq!(
            closed.observers.len(),
            1,
            "never empty for a seated connection"
        );
        assert!(
            closed.observers[0].center[0].is_nan(),
            "and it refuses nothing while the body spawns"
        );
        assert_eq!(
            closed.labels,
            vec![None],
            "one viewpoint, for every seat on it"
        );
    }

    /// A DECLARATION IS AN ANSWER, so the policy never reaches a connection that made one — including
    /// one whose declared center has not resolved. That half is a measurement that failed, and the
    /// policy is about connections nothing in the session has anything to say about.
    #[test]
    fn a_declared_anchor_ignores_the_closed_policy() {
        let cfg = radius_cfg(50.0);
        let rows = [row(1, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL)];

        let fixed = resolve_for(&cfg, &rows, 7, PeerAnchor::Fixed([1.0, 2.0, 3.0]), 4, true);
        assert_eq!(fixed.observers.len(), 1);
        assert_eq!(fixed.observers[0].center, [1.0, 2.0, 3.0]);
        assert_eq!(fixed.observers[0].membership, 4);
        assert_eq!(fixed.source, ANCHOR_SOURCE_FIXED);

        // A tracked entity that has never resolved: no center, so no distance culling — but the peer
        // stays in the world it was declared into, and it keeps its viewpoint.
        let tracked = resolve_for(&cfg, &rows, 7, PeerAnchor::Entity(9), 4, true);
        assert_eq!(
            tracked.observers.len(),
            1,
            "a failed measurement is not an absent declaration"
        );
        assert!(tracked.observers[0].center[0].is_nan());
        assert_eq!(tracked.observers[0].membership, 4);
    }

    /// The policy value that is in force, and the direction an unknown one falls in. OPEN takes
    /// nothing away from anybody, so it is the safe answer for a number this build does not know.
    #[test]
    fn an_unknown_unanchored_policy_clamps_to_open() {
        assert_eq!(clamp_unanchored_policy(UNANCHORED_OPEN), UNANCHORED_OPEN);
        assert_eq!(
            clamp_unanchored_policy(UNANCHORED_CLOSED),
            UNANCHORED_CLOSED
        );
        for junk in [2i64, 7, -1, i64::MIN, i64::MAX] {
            assert_eq!(
                clamp_unanchored_policy(junk),
                UNANCHORED_OPEN,
                "policy {junk} is not a policy, and withholding nothing is the safe reading"
            );
        }
    }

    // ------------------------------------------------------------------
    // Reporting the pick: which seats are ambiguous, and which are misconfigured.
    // ------------------------------------------------------------------

    /// The lowest-id pick does not move, and it is no longer silent: the row that survived the dedup
    /// carries the fact that rows were dropped beside it.
    ///
    /// Two anchored bodies in the SAME world is the QUIET tier — a game swapping one body for another
    /// on a seat holds both for the frame the swap takes, so it is reported and not logged.
    #[test]
    fn a_seat_with_two_anchored_bodies_is_reported_ambiguous() {
        let rows = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), 5),
            row_seat(2, 42, 0, Some([20.0, 0.0, 0.0]), 5),
            // A second seat with one body: not ambiguous, and unaffected by the first seat's mess.
            row_seat(3, 42, 1, Some([30.0, 0.0, 0.0]), 5),
        ];
        let mut observers = Vec::new();
        OrbitNet::collect_observers(&rows, &mut observers);

        let seats = OrbitNet::observers_of(&observers, 42);
        assert_eq!(seats.len(), 2);
        assert_eq!(seats[0].1.center, [10.0, 0.0, 0.0], "still the lowest id");
        assert!(seats[0].1.ambiguous, "and the pick is reported as a pick");
        assert!(
            !seats[0].1.membership_conflict,
            "one world, so nothing is misconfigured and nothing is logged"
        );
        assert!(!seats[1].1.ambiguous, "the unambiguous seat is untouched");

        // The connection carries the flag up, because a game asks about a connection.
        let resolved = resolve_for(
            &radius_cfg(50.0),
            &rows,
            42,
            PeerAnchor::Inferred,
            MEMBERSHIP_GLOBAL,
            false,
        );
        assert!(resolved.ambiguous);
        assert_eq!(resolved.labels, vec![Some(0), Some(1)]);
    }

    /// The LOUD tier, flagged separately: the dropped rows disagreed about the WORLD.
    ///
    /// That is the misconfiguration worth a log line. Inside one world the pick costs a radius
    /// centered on one of two bodies the same seat drives; across worlds it costs the seat its whole
    /// membership, and everything only that seat held leaves the connection's interest on any tick
    /// the pick changes.
    #[test]
    fn a_seat_whose_bodies_disagree_about_the_world_is_flagged_separately() {
        let rows = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), 5),
            row_seat(2, 42, 0, Some([20.0, 0.0, 0.0]), 6),
        ];
        let mut observers = Vec::new();
        OrbitNet::collect_observers(&rows, &mut observers);
        let seat = OrbitNet::observers_of(&observers, 42)[0].1;
        assert_eq!(seat.membership, 5, "the lowest-id row still decides");
        assert!(seat.ambiguous);
        assert!(
            seat.membership_conflict,
            "and the disagreement is its own flag"
        );

        // A body declaring no world at all disagrees with one that does: MEMBERSHIP_GLOBAL is a
        // value, and a seat half in every world and half in world 5 has no defined world.
        let mixed = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
            row_seat(2, 42, 0, Some([20.0, 0.0, 0.0]), 5),
        ];
        OrbitNet::collect_observers(&mixed, &mut observers);
        assert!(
            OrbitNet::observers_of(&observers, 42)[0]
                .1
                .membership_conflict
        );
    }

    /// **A seat whose second body has no resolved anchor is NOT ambiguity.** An unanchored row is
    /// skipped before the dedup ever sees it, so nothing was dropped — the seat has exactly one
    /// candidate for its center, and the pick was not a pick.
    ///
    /// Warning there would fire on every body a game spawns beside an existing one, in the window
    /// before that body has a state row.
    #[test]
    fn a_seats_unanchored_second_body_is_not_ambiguity() {
        let rows = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), 5),
            row_seat(2, 42, 0, None, 6), // no state row yet, and it declares a different world
        ];
        let mut observers = Vec::new();
        OrbitNet::collect_observers(&rows, &mut observers);
        let seat = OrbitNet::observers_of(&observers, 42)[0].1;
        assert_eq!(seat.center, [10.0, 0.0, 0.0]);
        assert_eq!(seat.membership, 5);
        assert!(
            !seat.ambiguous,
            "nothing was dropped, so nothing was picked between"
        );
        assert!(
            !seat.membership_conflict,
            "and a world nothing could be measured in cannot disagree with one that could"
        );
    }

    /// ONCE PER SEAT PER EPISODE. The warning fires on the tick a conflict appears, is silent while
    /// it persists, and is armed again once the seat stops colliding — so the same mistake after a
    /// map change is reported rather than swallowed.
    #[test]
    fn an_anchor_conflict_warns_once_then_rearms_when_it_clears() {
        let conflicted = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), 5),
            row_seat(2, 42, 0, Some([20.0, 0.0, 0.0]), 6),
        ];
        let fixed = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), 5),
            row_seat(2, 42, 0, Some([20.0, 0.0, 0.0]), 5),
        ];

        let mut warned = std::collections::HashSet::new();
        let mut observers = Vec::new();
        let mut owed = Vec::new();

        OrbitNet::collect_observers(&conflicted, &mut observers);
        anchor_conflicts_owed(&mut warned, &observers, &mut owed);
        assert_eq!(owed, vec![seat_of(42, 0)], "the tick it appears");

        anchor_conflicts_owed(&mut warned, &observers, &mut owed);
        assert!(owed.is_empty(), "and silent for every tick it persists");

        // The game declares the same world on both bodies: the seat stops colliding.
        OrbitNet::collect_observers(&fixed, &mut observers);
        anchor_conflicts_owed(&mut warned, &observers, &mut owed);
        assert!(
            owed.is_empty(),
            "a seat that stopped colliding is not re-warned"
        );
        assert!(
            warned.is_empty(),
            "it is dropped from the set, which is what re-arms it"
        );

        // Reintroduced after a map change. A set that only ever grew would report this to nobody.
        OrbitNet::collect_observers(&conflicted, &mut observers);
        anchor_conflicts_owed(&mut warned, &observers, &mut owed);
        assert_eq!(
            owed,
            vec![seat_of(42, 0)],
            "the second episode is its own report"
        );

        // A seat whose rows leave the session entirely also stops colliding.
        observers.clear();
        anchor_conflicts_owed(&mut warned, &observers, &mut owed);
        assert!(owed.is_empty());
        assert!(
            warned.is_empty(),
            "a seat with no rows left holds no warning"
        );
    }

    /// `is_located` reads the sentinel by the rule the filter reads it by. A NaN is never equal to
    /// itself, so a diagnostic that compared against `UNLOCATABLE_CENTER` would report every peer as
    /// located.
    #[test]
    fn an_unlocatable_center_is_recognized_by_finiteness_not_by_equality() {
        assert!(!is_located(UNLOCATABLE_CENTER));
        assert!(
            is_located([0.0; 3]),
            "the origin is a position like any other"
        );
        assert!(is_located([1.0, -2.0, 3.0]));
        assert!(
            !is_located([1.0, f32::NAN, 3.0]),
            "one bad component is enough"
        );
        assert!(!is_located([f32::INFINITY, 0.0, 0.0]));
    }

    // ------------------------------------------------------------------
    // The client input frame's byte bound
    // ------------------------------------------------------------------

    /// One body per connection never filled a datagram; several can. Everything that fits rides,
    /// in ascending block order, and the rotor holds still while nothing is being deferred.
    #[test]
    fn an_input_frame_that_fits_carries_every_block_and_does_not_rotate() {
        let mut carried = vec![99]; // must be cleared, not appended to
        let rotor = admit_input_blocks(&[10, 20, 30], 0, 1200, &mut carried);
        assert_eq!(carried, vec![0, 1, 2]);
        assert_eq!(rotor, 0, "nothing was refused, so nothing needs a turn");

        // No owned bodies at all: no blocks, and a rotor that cannot index anything.
        assert_eq!(admit_input_blocks(&[], 7, 1200, &mut carried), 0);
        assert!(carried.is_empty());
    }

    /// The rota. Past the budget, the refused block is what the next tick offers first, so no seat
    /// can be starved by the ones that sort ahead of it.
    #[test]
    fn a_full_input_frame_defers_to_the_next_tick_and_rotates() {
        let lengths = [400usize, 400, 400, 400];
        let mut carried = Vec::new();

        let rotor = admit_input_blocks(&lengths, 0, 1000, &mut carried);
        assert_eq!(carried, vec![0, 1], "800 fits, 1200 does not");
        assert_eq!(rotor, 2);

        // Next tick starts at 2 and wraps, so the two that waited go first.
        let rotor = admit_input_blocks(&lengths, rotor, 1000, &mut carried);
        assert_eq!(carried, vec![2, 3]);
        assert_eq!(rotor, 0, "and the walk wrapped to refuse 0");

        // Over two ticks every block rode exactly once — which is the starvation-freedom claim.
        let rotor = admit_input_blocks(&lengths, rotor, 1000, &mut carried);
        assert_eq!(carried, vec![0, 1]);
        assert_eq!(rotor, 2);
    }

    /// A block wider than the whole payload can never be sent — no rota fixes an input row larger
    /// than a datagram — and it must not take the other seats down with it. The walk continues past
    /// it, and the rotor does not park on it: a refusal nothing can satisfy is passed over, so the
    /// walk starts from the same place next tick and the blocks behind it ride again.
    #[test]
    fn an_oversized_input_block_starves_nobody() {
        let mut carried = Vec::new();
        let rotor = admit_input_blocks(&[2000, 100, 100], 0, 1200, &mut carried);
        assert_eq!(carried, vec![1, 2], "the other two still ride");
        assert_eq!(rotor, 0);

        // And it stays that way tick after tick rather than converging on sending nothing.
        let rotor = admit_input_blocks(&[2000, 100, 100], rotor, 1200, &mut carried);
        assert_eq!(carried, vec![1, 2]);
        assert_eq!(rotor, 0);
    }

    /// The rotor is a position in a list that changes size — a body despawns, a seat leaves — so it
    /// is taken modulo the block count rather than trusted as an index.
    #[test]
    fn the_rotor_survives_a_shrinking_owned_set() {
        let mut carried = Vec::new();
        let rotor = admit_input_blocks(&[100], 9, 1200, &mut carried);
        assert_eq!(carried, vec![0]);
        assert_eq!(rotor, 0);
    }

    /// A peer's own body is never culled by anything, membership included — the peer's world was read
    /// off this very row, and a peer driving bodies in two worlds must not lose its own avatar.
    #[test]
    fn a_peers_own_body_is_always_relevant_in_every_world() {
        for membership in [MEMBERSHIP_GLOBAL, 1, MembershipId::MAX] {
            let candidate = candidate_for_own_row(&row(7, 42, Some([500.0, 0.0, 0.0]), membership));
            assert_eq!(
                candidate,
                InterestCandidate::always(7),
                "the peer's own body goes in global, whatever world the row declares"
            );
        }
    }

    /// An anchored row carries both axes: the position for the radius, the declared world for the
    /// membership test.
    #[test]
    fn an_anchored_row_carries_its_position_and_its_world() {
        let candidate = candidate_for_row(&row(7, 5, Some([1.0, 2.0, 3.0]), 4));
        assert_eq!(
            candidate,
            InterestCandidate::anchored_in(7, [1.0, 2.0, 3.0], 4)
        );
        assert!(!candidate.always);
    }

    /// A game that declares no worlds produces exactly the candidates it did before memberships
    /// existed.
    #[test]
    fn declaring_no_membership_reproduces_the_pre_membership_candidates() {
        assert_eq!(
            candidate_for_row(&row(7, 5, Some([1.0, 2.0, 3.0]), MEMBERSHIP_GLOBAL)),
            InterestCandidate::anchored(7, [1.0, 2.0, 3.0])
        );
        assert_eq!(
            candidate_for_row(&row(7, 5, None, MEMBERSHIP_GLOBAL)),
            InterestCandidate::always_in(7, MEMBERSHIP_GLOBAL)
        );
    }

    /// A peer's owned rows are found by binary search over one sorted table, and a peer driving
    /// several bodies gets all of them.
    #[test]
    fn owned_rows_are_indexed_once_and_looked_up_per_peer() {
        let rows = [
            row(1, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row(2, 7, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row(3, 4, None, MEMBERSHIP_GLOBAL),
            row(4, 7, None, MEMBERSHIP_GLOBAL),
        ];
        let mut owned = vec![(seat_of(999, 9), 999)]; // must be cleared, not appended to
        owned_rows_into(&rows, &mut owned);
        assert_eq!(
            owned,
            vec![(seat_of(4, 0), 2), (seat_of(7, 0), 1), (seat_of(7, 0), 3)],
            "unowned rows are absent"
        );
        assert_eq!(
            owned_rows_of(&owned, 7),
            &[(seat_of(7, 0), 1), (seat_of(7, 0), 3)]
        );
        assert_eq!(owned_rows_of(&owned, 4), &[(seat_of(4, 0), 2)]);
        assert_eq!(owned_rows_of(&owned, 9), &[], "a peer driving nothing");
        assert_eq!(owned_rows_of(&owned, 1), &[], "and one below every owner");
    }

    /// The table is keyed by seat, and a connection's slice is ascending by seat label with a run
    /// per seat — which is what lets `update_interest` count a connection's seats without a set.
    /// Every row is still that CONNECTION's, whichever seat drives it: the datagram is shared.
    #[test]
    fn owned_rows_group_a_connections_seats_in_label_order() {
        let rows = [
            row_seat(1, 7, 2, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row_seat(2, 7, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row_seat(3, 7, 2, None, MEMBERSHIP_GLOBAL),
            row_seat(4, 8, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
        ];
        let mut owned = Vec::new();
        owned_rows_into(&rows, &mut owned);
        assert_eq!(
            owned_rows_of(&owned, 7),
            &[(seat_of(7, 0), 1), (seat_of(7, 2), 0), (seat_of(7, 2), 2)],
            "one connection's rows, ascending by seat, each seat's rows contiguous"
        );
        // A seat label decides the grouping and nothing else — the labels need not be contiguous,
        // and seat 1 being absent costs nothing.
        assert_eq!(owned_rows_of(&owned, 8), &[(seat_of(8, 0), 3)]);
    }

    /// **The equivalence the shared list rests on.** One list per tick with the peer's own rows
    /// patched in must equal the list a per-peer rebuild would have produced, row for row.
    ///
    /// The reference below is the rule as it was written before the list was shared. If the two ever
    /// part, a peer is filtered against somebody else's view of the tick — which is silent, because
    /// every candidate is individually well-formed.
    #[test]
    fn shared_candidates_match_a_per_peer_rebuild() {
        /// The rule `update_interest` ran when it rebuilt the list inside the per-peer loop.
        fn reference(row: &EntityRow, peer_id: i32) -> InterestCandidate {
            if row.owner == peer_id {
                return InterestCandidate::always(row.id);
            }
            match row.anchor {
                Some(pos) => InterestCandidate::anchored_in(row.id, pos, row.membership),
                None => InterestCandidate::always_in(row.id, row.membership),
            }
        }

        let rows = [
            row(1, 0, Some([1.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL), // unowned, anchored, global
            row(2, 7, Some([2.0, 0.0, 0.0]), 3),                 // peer 7's body, in world 3
            row(3, 0, None, 4),                                  // positionless channel, world 4
            row(4, 9, None, MEMBERSHIP_GLOBAL),                  // peer 9's body, unanchored
            row(5, 7, Some([5.0, 0.0, 0.0]), 8),                 // peer 7's SECOND body
            row(6, 0, Some([6.0, 0.0, 0.0]), MembershipId::MAX), // a far world's body
        ];
        let mut shared: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let mut owned = Vec::new();
        owned_rows_into(&rows, &mut owned);

        // 7 drives two bodies, 9 drives one, 42 is a peer with no body at all. `0` is not in the
        // list because it is the unowned sentinel rather than a peer id — Godot's sender id is
        // always positive — and the reference would read every unowned row as peer 0's own body.
        for peer_id in [7, 9, 42] {
            let mine = owned_rows_of(&owned, peer_id);
            for &(_, index) in mine {
                shared[index as usize] = candidate_for_own_row(&rows[index as usize]);
            }
            let expected: Vec<InterestCandidate> =
                rows.iter().map(|row| reference(row, peer_id)).collect();
            assert_eq!(shared, expected, "peer {peer_id} saw the wrong tick");
            for &(_, index) in mine {
                shared[index as usize] = candidate_for_row(&rows[index as usize]);
            }
        }

        // And the list is back to its peer-independent shape, so the next tick's peers are not
        // filtered against the last peer of this one.
        let fresh: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        assert_eq!(shared, fresh, "the patch was not restored");
    }

    // ------------------------------------------------------------------
    // The path the session picks, and the equivalence that licenses picking it.
    // ------------------------------------------------------------------

    /// The config `aoi_config` derives at a given radius. The cell size is a quarter of the radius,
    /// which is what both tables in `orbitnet_core::interest`'s header were measured at: a query
    /// rectangle is then 11 cells a side whatever the radius is.
    fn grid_cfg(radius: f32) -> AoiConfig {
        AoiConfig {
            cell_size: (radius / 4.0).max(1.0),
            enter_radius: radius,
            exit_factor: AOI_EXIT_FACTOR,
            max_entities: 0,
        }
    }

    /// Everything `update_interest` pools for one connection, so one test tick is one call.
    ///
    /// The path is **pinned** rather than selected, which is the whole point: the send loop picks it
    /// from the session's occupancy, and the two paths have to agree whichever way that lands.
    #[derive(Default)]
    struct InterestHarness {
        grid: InterestGrid,
        candidates: Vec<InterestCandidate>,
        owned: Vec<(SeatId, u32)>,
        overrides: Vec<InterestCandidate>,
        interest: ConnectionInterest,
        scratch: SeatScratch,
        delta: InterestDelta,
    }

    impl InterestHarness {
        /// One tick of the pass for one connection, composed the way `update_interest` composes it:
        /// the shared list rebuilt, the grid rebuilt once when the pinned path reads it, the seats
        /// resolved, then `filter_connection`.
        fn tick(&mut self, path: InterestPath, cfg: &AoiConfig, rows: &[EntityRow], peer: i32) {
            self.candidates.clear();
            self.candidates.extend(rows.iter().map(candidate_for_row));
            owned_rows_into(rows, &mut self.owned);
            if path == InterestPath::Grid {
                self.grid.rebuild(cfg, &self.candidates);
            }
            let seats = resolve_for(
                cfg,
                rows,
                peer,
                PeerAnchor::Inferred,
                MEMBERSHIP_GLOBAL,
                false,
            );
            let pass = InterestPass {
                path,
                grid: &self.grid,
                cfg,
                rows,
            };
            filter_connection(
                &pass,
                owned_rows_of(&self.owned, peer),
                &seats.observers,
                &mut self.candidates,
                &mut self.overrides,
                &mut self.interest,
                &mut self.scratch,
                &mut self.delta,
            );
        }

        /// The union as the send loop reads it: ascending by id, with each member's distance.
        fn members(&self) -> Vec<(u64, f32)> {
            self.interest.iter_with_distance().collect()
        }
    }

    /// One connection on two seats, over the three shapes the shared candidate list distinguishes:
    /// rows the connection drives (anchored and not), rows nobody drives, and rows in a world
    /// neither seat is in.
    fn mixed_rows() -> Vec<EntityRow> {
        vec![
            row_seat(1, 42, 0, Some([0.0; 3]), 5), // seat 0's body: its center and its world
            row_seat(2, 42, 1, Some([200.0, 0.0, 0.0]), 6), // seat 1's, 200 m away in world 6
            row_seat(3, 42, 1, None, 9),           // driven, no anchor, a world of its own
            row(4, 0, Some([10.0, 0.0, 0.0]), 5),  // near seat 0, in seat 0's world
            row(5, 0, Some([205.0, 0.0, 0.0]), 6), // near seat 1, in seat 1's world
            row(6, 0, Some([12.0, 0.0, 0.0]), 7),  // near seat 0, in a world neither is in
            row(7, 0, None, 4),                    // positionless, in a world neither is in
            row(8, 0, None, MEMBERSHIP_GLOBAL),    // positionless, every world
            row(9, 0, Some([2000.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL), // outside every radius
            row(10, 0, Some([30.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL), // near seat 0
        ]
    }

    /// A world too wide for the flat pass to be the cheap answer: `count` unowned, anchored, global
    /// bodies spread along X over ±`half_extent`.
    fn sprawl(count: u64, half_extent: f32) -> Vec<EntityRow> {
        (0..count)
            .map(|i| {
                let t = i as f32 / (count - 1) as f32 * 2.0 - 1.0;
                row(
                    i + 1,
                    0,
                    Some([t * half_extent, 0.0, 0.0]),
                    MEMBERSHIP_GLOBAL,
                )
            })
            .collect()
    }

    /// The same sprawl with the first `owned` rows driven by peer 42 — the overrides a grid tick
    /// would have to scan per hit.
    fn owned_sprawl(owned: usize) -> Vec<EntityRow> {
        let mut rows = sprawl(600, 900.0);
        for row in rows.iter_mut().take(owned) {
            row.owner = 42;
        }
        rows
    }

    /// **THE EQUIVALENCE THE AUTOMATIC PATH RESTS ON, at the wiring.** The core suite asserts the
    /// two paths agree over a randomized walk; this asserts the send loop hands them the same tick,
    /// which is the half a core test cannot see — the override list and the patched shared list have
    /// to carry the same facts about the same connection.
    ///
    /// Members, per-member distances **and** the ids the leave list clears from the delta
    /// bookkeeping, row for row. The leave half is the one that costs bandwidth when it is wrong: a
    /// leave clears `last_sent` and `acked_base`, so a leave one path reports and the other does not
    /// is a full block for a body that never went anywhere.
    #[test]
    fn both_interest_paths_agree_on_members_distances_and_leaves() {
        let cfg = grid_cfg(50.0);
        let mut rows = mixed_rows();
        let mut linear = InterestHarness::default();
        let mut grid = InterestHarness::default();

        linear.tick(InterestPath::Linear, &cfg, &rows, 42);
        grid.tick(InterestPath::Grid, &cfg, &rows, 42);
        assert_eq!(
            linear.members(),
            vec![
                (1, 0.0),    // seat 0's own body: an override, never culled
                (2, 0.0),    // seat 1's own body, and an override to seat 0 as well
                (3, 0.0),    // driven with no anchor: an override, so its world is not consulted
                (4, 100.0),  // 10 m from seat 0, in seat 0's world
                (5, 25.0),   // 5 m from seat 1, in seat 1's world
                (8, 0.0),    // positionless and global: always, at every distance
                (10, 900.0), // 30 m from seat 0
            ],
            "the flat pass admitted the wrong set"
        );
        assert_eq!(
            grid.members(),
            linear.members(),
            "the index disagreed with the flat pass on members or distances"
        );
        assert!(linear.delta.leaves.is_empty() && grid.delta.leaves.is_empty());
        // The first tick of a fresh connection enters everything it holds — what seeds a joining
        // peer's mirrored set, and it must come out of both paths identically.
        assert_eq!(linear.delta.enters, vec![1, 2, 3, 4, 5, 8, 10]);
        assert_eq!(grid.delta.enters, linear.delta.enters);

        // A body walks out of seat 0's radius. What leaves is what the caller clears its delta
        // bookkeeping from, so the two paths have to name the same id.
        rows[9].anchor = Some([900.0, 0.0, 0.0]);
        linear.tick(InterestPath::Linear, &cfg, &rows, 42);
        grid.tick(InterestPath::Grid, &cfg, &rows, 42);
        assert_eq!(
            linear.delta.leaves,
            vec![10],
            "the flat pass reported the wrong leave"
        );
        assert_eq!(
            grid.delta.leaves, linear.delta.leaves,
            "the index cleared different delta bookkeeping"
        );
        assert!(
            linear.delta.enters.is_empty() && grid.delta.enters.is_empty(),
            "a body walking out is not a body walking in"
        );
        assert_eq!(grid.members(), linear.members());

        // And the shared list is peer-independent again on both: the flat pass restored what it
        // patched, and the grid path never patched it at all.
        let fresh: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        assert_eq!(
            linear.candidates, fresh,
            "the flat pass did not restore the shared list"
        );
        assert_eq!(
            grid.candidates, fresh,
            "the grid path patched a list it does not read"
        );
    }

    /// **A MID-SESSION PATH SWITCH MUST EMIT NO LEAVES.** The enter radius is a live setting a game
    /// may change at runtime, and changing it moves the cell size and therefore the verdict, so a
    /// session can flip path on any tick.
    ///
    /// Both paths compute the same members from the same state, so the diff against the same set
    /// reports nothing. A spurious leave here clears `last_sent` and `acked_base` for every entity
    /// on that peer, which is a full-state burst for a world that did not move — the exact failure
    /// the leave list exists to prevent.
    #[test]
    fn switching_the_path_mid_session_reports_no_leaves() {
        let cfg = grid_cfg(50.0);
        let rows = mixed_rows();
        let mut peer = InterestHarness::default();

        peer.tick(InterestPath::Linear, &cfg, &rows, 42);
        let settled = peer.members();
        assert!(!settled.is_empty(), "nothing to lose is not a test");

        peer.tick(InterestPath::Grid, &cfg, &rows, 42);
        assert!(
            peer.delta.is_empty(),
            "the flip onto the index reported a transition"
        );
        assert_eq!(
            peer.members(),
            settled,
            "and it moved a member or a distance"
        );

        peer.tick(InterestPath::Linear, &cfg, &rows, 42);
        assert!(peer.delta.is_empty(), "the flip back reported a transition");
        assert_eq!(peer.members(), settled);
    }

    /// **The cap and the wire order survive the grid's iteration.** `InterestGrid::query_within`
    /// walks a `HashMap`, so the order its hits arrive in is unspecified. Two normalizations make
    /// that unobservable and both have to hold at the wiring: the cap breaks a distance tie by
    /// ascending id, and `commit` sorts by id before it stores, so the union is a `BTreeMap` either
    /// way. Five bodies at one distance and a cap of two is the case that can only be answered by
    /// the tie-break.
    #[test]
    fn the_cap_and_the_wire_order_survive_the_grids_iteration() {
        let cfg = AoiConfig {
            max_entities: 2,
            ..grid_cfg(50.0)
        };
        let rows = vec![
            row(1, 42, Some([0.0; 3]), MEMBERSHIP_GLOBAL),
            row(11, 0, Some([10.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
            row(12, 0, Some([-10.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL),
            row(13, 0, Some([0.0, 0.0, 10.0]), MEMBERSHIP_GLOBAL),
            row(14, 0, Some([0.0, 0.0, -10.0]), MEMBERSHIP_GLOBAL),
            row(15, 0, Some([0.0, 10.0, 0.0]), MEMBERSHIP_GLOBAL),
        ];
        let mut linear = InterestHarness::default();
        let mut grid = InterestHarness::default();
        linear.tick(InterestPath::Linear, &cfg, &rows, 42);
        grid.tick(InterestPath::Grid, &cfg, &rows, 42);

        assert_eq!(
            linear.members(),
            vec![(1, 0.0), (11, 100.0), (12, 100.0)],
            "the lowest two ids of the tie, plus the connection's own uncapped row"
        );
        assert_eq!(
            grid.members(),
            linear.members(),
            "the index broke the tie by its bucket walk"
        );
    }

    /// **RADIUS 0 MUST NEVER SELECT THE GRID**, and the refusal has no hysteresis: a session already
    /// on the index that drops its radius to zero is off it on the same tick.
    ///
    /// A membership-only session still runs the pass — refusing an overlapping world is the only
    /// culling it asked for — and it has no distance to index at all, so a rebuild would buy it
    /// nothing. The first half is the same list at a shipped radius, so the refusal is not vacuous.
    #[test]
    fn a_session_with_no_radius_never_selects_the_grid() {
        let rows = sprawl(600, 900.0);
        let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let owned: Vec<(SeatId, u32)> = Vec::new();
        let mut selector = PathSelector::new();
        let mut scratch = OccupancyScratch::default();

        assert_eq!(
            select_interest_path(
                &mut selector,
                &grid_cfg(256.0),
                &candidates,
                &owned,
                &[42],
                &mut scratch
            ),
            InterestPath::Grid,
            "a world 29 cells a side against an 11-cell query rectangle earns the index"
        );
        assert_eq!(
            select_interest_path(
                &mut selector,
                &grid_cfg(0.0),
                &candidates,
                &owned,
                &[42],
                &mut scratch
            ),
            InterestPath::Linear,
            "a membership-only session has no distance to index"
        );
    }

    /// **A connection driving many bodies keeps the linear path.** The override list is scanned once
    /// per grid hit — that is what lets a connection's own rows shadow the shared index — so its
    /// cost is `overrides × hits` on the path whose whole purpose is to cut the hits. The flat pass
    /// folds the same rows in for free.
    ///
    /// One rebuild serves the whole tick, so the count the selector is given is the largest any
    /// connection will hand it: one connection over the bound keeps the tick on the flat pass.
    #[test]
    fn a_connection_driving_many_bodies_keeps_the_linear_path() {
        for (owned_rows, expected) in [
            (GRID_MAX_OVERRIDES, InterestPath::Grid),
            (GRID_MAX_OVERRIDES + 1, InterestPath::Linear),
        ] {
            let rows = owned_sprawl(owned_rows);
            let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
            let mut owned = Vec::new();
            owned_rows_into(&rows, &mut owned);
            let mut selector = PathSelector::new();
            let mut scratch = OccupancyScratch::default();
            assert_eq!(
                select_interest_path(
                    &mut selector,
                    &grid_cfg(256.0),
                    &candidates,
                    &owned,
                    &[42],
                    &mut scratch
                ),
                expected,
                "{owned_rows} overrides on one connection"
            );
        }
    }

    /// The seat's own world comes from the same row its interest center does: the LOWEST-id owned row
    /// on that seat that resolved an anchor. Rows arrive sorted by id, and a seat driving more than
    /// one body must not have either answer decided by `HashMap` iteration order.
    #[test]
    fn a_seats_center_and_world_both_come_from_its_lowest_id_anchored_body() {
        let rows = [
            // Owned but unanchored, and the lowest id: skipped, so it supplies NEITHER fact.
            row(1, 42, None, 77),
            row(2, 42, Some([10.0, 0.0, 0.0]), 5),
            row(3, 42, Some([20.0, 0.0, 0.0]), 6),
            // Another peer's body, and an unowned state-lane row.
            row(4, 43, Some([30.0, 0.0, 0.0]), 8),
            row(5, 0, Some([40.0, 0.0, 0.0]), 9),
        ];
        let mut observers = vec![(seat_of(999, 9), observed([7.0; 3], 1))];
        OrbitNet::collect_observers(&rows, &mut observers);

        let peer = OrbitNet::observers_of(&observers, 42)[0].1;
        assert_eq!(peer.center, [10.0, 0.0, 0.0]);
        assert_eq!(
            peer.membership, 5,
            "the world comes from the row that supplied the center, not from a lower unanchored one"
        );
        assert_eq!(OrbitNet::observers_of(&observers, 43)[0].1.membership, 8);
        assert!(
            OrbitNet::observers_of(&observers, 0).is_empty(),
            "an unowned row anchors nobody"
        );
        assert_eq!(
            observers.len(),
            2,
            "the stale entry was cleared, not appended to"
        );
    }

    /// **The change local split-screen needs.** One connection driving two bodies on two seats gets
    /// two centers and two worlds. Keyed by connection, the lower entity id won and the other
    /// player's surroundings were culled around a position that player was nowhere near.
    #[test]
    fn each_seat_on_one_connection_anchors_itself() {
        let rows = [
            row_seat(1, 42, 0, Some([10.0, 0.0, 0.0]), 5),
            row_seat(2, 42, 1, Some([900.0, 0.0, 0.0]), 6),
            // A second body on seat 1, higher id: the lowest-id rule still picks per seat.
            row_seat(3, 42, 1, Some([950.0, 0.0, 0.0]), 7),
        ];
        let mut observers = Vec::new();
        OrbitNet::collect_observers(&rows, &mut observers);

        let seats = OrbitNet::observers_of(&observers, 42);
        assert_eq!(seats.len(), 2);
        assert_eq!(seats[0].0, seat_of(42, 0));
        assert_eq!(seats[0].1.center, [10.0, 0.0, 0.0]);
        assert_eq!(seats[0].1.membership, 5);
        assert_eq!(seats[1].0, seat_of(42, 1));
        assert_eq!(seats[1].1.center, [900.0, 0.0, 0.0]);
        assert_eq!(seats[1].1.membership, 6);
    }

    /// A seat with no anchored body gets no entry, so it is neither distance-culled nor
    /// membership-filtered: `update_interest` reads the absence as MEMBERSHIP_GLOBAL and it sees
    /// every world. Both halves fail open together, and now they fail open FOR THAT SEAT — the
    /// other seat on the same connection keeps its center.
    #[test]
    fn a_seat_with_no_anchored_body_has_no_observer_at_all() {
        let rows = [row(1, 42, None, 77), row(2, 0, Some([1.0, 0.0, 0.0]), 5)];
        let mut observers: Vec<(SeatId, PeerObserver)> = Vec::new();
        OrbitNet::collect_observers(&rows, &mut observers);
        assert!(observers.is_empty());

        // The same, with a second seat that DID resolve one: the unanchored seat is still absent,
        // and the located one is unaffected by it.
        let rows = [
            row_seat(1, 42, 0, None, 77),
            row_seat(2, 42, 1, Some([4.0, 0.0, 0.0]), 5),
        ];
        OrbitNet::collect_observers(&rows, &mut observers);
        assert_eq!(observers.len(), 1);
        assert_eq!(observers[0].0, seat_of(42, 1));
    }

    // ------------------------------------------------------------------
    // The visibility veto: what the send path clears when one starts.
    // ------------------------------------------------------------------

    /// One peer holding an entity on a single seat: in interest, sent, sent full, and with an acked
    /// delta base.
    fn peer_holding(id: u64) -> PeerState {
        let mut peer = PeerState::default();
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        peer.interest.update_linear_into(
            &AoiConfig::default(),
            &[SeatObserver {
                center: [0.0; 3],
                membership: MEMBERSHIP_GLOBAL,
            }],
            &[InterestCandidate::anchored(id, [1.0, 0.0, 0.0])],
            &mut scratch,
            &mut delta,
        );
        peer.last_sent.insert(id, 400);
        peer.last_full.insert(id, 390);
        peer.acked_base.insert(id, 395);
        peer
    }

    /// A veto is a leave that happens between updates, so no `leaves` list can name it and the same
    /// three entries have to be cleared right here. Left in place, the re-admission encodes a delta
    /// against a base the peer dropped while it was withheld.
    #[test]
    fn starting_a_veto_clears_the_same_bookkeeping_a_leave_clears() {
        let mut peer = peer_holding(7);
        assert!(peer.interest.contains(7));

        peer.set_entity_hidden(7, true);
        assert!(peer.interest.is_hidden(7), "the veto is recorded");
        assert!(
            !peer.interest.contains(7),
            "and it left the set in this call"
        );
        assert!(!peer.last_sent.contains_key(&7));
        assert!(!peer.last_full.contains_key(&7));
        assert!(!peer.acked_base.contains_key(&7));
    }

    /// The veto covers ONE entity. Everything else this peer holds keeps its delta chain, which is
    /// the difference between a veto and the per-peer, all-entity `want_full`.
    #[test]
    fn a_veto_leaves_every_other_entity_of_that_peer_untouched() {
        let mut peer = peer_holding(7);
        peer.last_sent.insert(8, 401);
        peer.last_full.insert(8, 391);
        peer.acked_base.insert(8, 396);

        peer.set_entity_hidden(7, true);
        assert_eq!(peer.last_sent.get(&8), Some(&401));
        assert_eq!(peer.last_full.get(&8), Some(&391));
        assert_eq!(peer.acked_base.get(&8), Some(&396));
        assert!(!peer.interest.is_hidden(8));
    }

    /// Retracting clears nothing, because nothing was sent while the veto held. The entries stay
    /// empty, which is exactly what makes the re-admission a full block rather than a delta.
    #[test]
    fn retracting_a_veto_leaves_the_bookkeeping_empty() {
        let mut peer = peer_holding(7);
        peer.set_entity_hidden(7, true);
        peer.set_entity_hidden(7, false);
        assert!(!peer.interest.is_hidden(7));
        assert!(peer.last_sent.is_empty(), "still nothing to delta against");
        assert!(peer.last_full.is_empty());
        assert!(peer.acked_base.is_empty());
    }

    /// A vetoed entity is absent from the set, so the cull figure the send path derives per peer —
    /// `rows.len() - interest.len()` — counts it on every tick the veto holds, with no separate
    /// accounting to keep in step.
    #[test]
    fn a_vetoed_entity_counts_as_culled_by_the_derived_figure() {
        let rows = 3usize;
        let mut peer = peer_holding(7);
        assert_eq!(rows - peer.interest.len(), 2);
        peer.set_entity_hidden(7, true);
        assert_eq!(rows - peer.interest.len(), 3);
    }

    // ------------------------------------------------------------------
    // The per-peer relevancy delta: what fills it, what rides, and what retires it.
    // ------------------------------------------------------------------

    /// A slot table naming every id these tests use, at slot `i` for `ids[i]`, so
    /// `build_interest_section` can resolve them.
    fn table_naming(ids: &[u64]) -> SlotTable {
        table_binding(
            &ids.iter()
                .enumerate()
                .map(|(index, &id)| (index as u16, id))
                .collect::<Vec<_>>(),
        )
    }

    /// The same, with the slots named outright — for the manifest-rebuild cases, where an entity has
    /// to KEEP the slot it had while another releases one.
    fn table_binding(pairs: &[(u16, u64)]) -> SlotTable {
        let mut slots = SlotTable::new();
        for &(slot, id) in pairs {
            slots.bind(slot, id);
        }
        slots
    }

    /// One frame's worth of section, with the flag decision beside it.
    fn section_for(
        slots: &SlotTable,
        peer: &mut PeerState,
        current: u64,
    ) -> (bool, Vec<u16>, Vec<u16>) {
        let (mut left, mut entered) = (Vec::new(), Vec::new());
        let carries = build_interest_section(slots, peer, true, current, &mut left, &mut entered);
        (carries, left, entered)
    }

    // ------------------------------------------------------------------
    // The resync: three ways a connection is owed the whole set, and the two gates on the client.
    // ------------------------------------------------------------------

    /// **A CLIENT THAT DRIVES NOTHING STILL HAS TO SEND.** The gate suppresses a frame carrying no
    /// blocks, and an observer drives no body, so every frame it sends carries none.
    ///
    /// Two things it must not suppress. A NACK that cannot leave is a session that never repairs.
    /// And the input frame is the only frame that carries this peer's ack — without it an observer
    /// never acked at all, so every interest prefix was given up on unacknowledged, which owes it a
    /// whole set every retry window for the rest of the session.
    #[test]
    fn an_ack_or_any_nack_keeps_an_empty_frame_alive() {
        assert!(
            !input_frame_is_owed(false, false, false, false, false),
            "nothing to say, nothing to send"
        );
        assert!(
            input_frame_is_owed(true, false, false, false, false),
            "blocks are the ordinary reason"
        );
        assert!(
            input_frame_is_owed(false, true, false, false, false),
            "an unacked snapshot is the observer's reason, and it has no other"
        );
        assert!(
            input_frame_is_owed(false, false, true, false, false),
            "a broken delta base must reach the server"
        );
        assert!(
            input_frame_is_owed(false, false, false, true, false),
            "so must a broken manifest"
        );
        assert!(
            input_frame_is_owed(false, false, false, false, true),
            "and so must a broken interest set"
        );
    }

    /// The pass does not switch back off under a game that retracts its last veto or unregisters its
    /// last non-global row. A client that has been told anything answers out of a mirrored set, and a
    /// server that stopped filtering would leave that mirror frozen while answering "everything".
    #[test]
    fn a_session_that_has_filtered_keeps_filtering() {
        assert!(
            !session_is_filtering(false, false, false, false),
            "nothing to refuse"
        );
        assert!(session_is_filtering(true, false, false, false), "a radius");
        assert!(
            session_is_filtering(false, true, false, false),
            "a standing veto on its own"
        );
        assert!(
            session_is_filtering(false, false, false, true),
            "a declared membership"
        );
        assert!(
            session_is_filtering(false, false, true, false),
            "and a session that has already filtered, whatever it holds now"
        );
    }

    /// **A WHOLE SET STATES WHAT IT CAN NAME, AND KEEPS HOLDING WHAT IT CANNOT.** An entity can be in
    /// a connection's interest before the slot table can name it — the delta path holds such an enter
    /// rather than sending it, and the admit loop defers its block — so the set has to omit it.
    /// Clearing the enter that was holding it in the same breath is what would make the omission
    /// permanent: no later diff re-enters an id already in the set, so nothing would ever announce it
    /// again while its rows kept arriving. That is the divergence this frame exists to close,
    /// produced by the repair itself.
    #[test]
    fn a_whole_set_keeps_holding_a_member_it_could_not_name() {
        // Two members; the slot table can name only the first.
        let slots = table_naming(&[11]);
        let mut peer = PeerState::default();
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        peer.interest.update_linear_into(
            &AoiConfig::default(),
            &[SeatObserver {
                center: [0.0; 3],
                membership: MEMBERSHIP_GLOBAL,
            }],
            &[
                InterestCandidate::anchored(11, [1.0, 0.0, 0.0]),
                InterestCandidate::anchored(12, [1.0, 0.0, 0.0]),
            ],
            &mut scratch,
            &mut delta,
        );
        assert!(peer.interest.contains(11) && peer.interest.contains(12));
        peer.note_interest_enter(11);
        peer.note_interest_enter(12);
        peer.note_interest_leave(99);
        peer.interest_full_due = true;

        let (generation, stated) = state_whole_interest_set(&slots, &mut peer);

        assert_eq!(
            generation, 1,
            "stating a set bumps the generation it is stamped with"
        );
        assert_eq!(
            stated,
            vec![0u16],
            "and states only what the slot table can name"
        );
        assert_eq!(
            peer.interest_pending.enters,
            vec![12],
            "the member it could not name is still held, for a section once its slot binds"
        );
        assert!(
            peer.interest_pending.leaves.is_empty(),
            "a leave is superseded whatever it named -- absence is what it was going to say"
        );
        assert!(peer.interest_seeded, "the connection holds a set now");
        assert!(!peer.interest_full_due, "and is owed nothing further");
        assert!(peer.interest_delta_tick.is_none(), "nothing is in flight");
    }

    /// **THE SOFT CAP EVICTS NOTHING.** It cannot tell a recoverable entry from an unrecoverable one:
    /// a whole set restates every member the slot table can name, so a dropped entry is recoverable
    /// exactly when the set could have restated it — and what the set cannot restate is a member with
    /// no slot, whose enter is held across the set for that reason. Dropping the oldest lost that held
    /// enter; dropping the newest loses it whenever the slotless member is the one that just arrived.
    ///
    /// So the overflow says the connection is owed a whole set, and stating that set collapses the
    /// half. The hard ceiling is a backstop for a connection that is never sent one.
    #[test]
    fn the_pending_cap_owes_a_whole_set_rather_than_evicting() {
        let mut peer = PeerState::default();
        peer.note_interest_enter(7); // the held one, at the front
        for id in 100..(100 + INTEREST_DELTA_PENDING_MAX as u64 - 1) {
            peer.note_interest_enter(id);
        }
        assert_eq!(
            peer.interest_pending.enters.len(),
            INTEREST_DELTA_PENDING_MAX
        );
        assert!(
            !peer.interest_full_due,
            "filling it exactly is not an overflow"
        );

        peer.note_interest_enter(9_999);
        assert!(peer.interest_full_due, "the overflow owes a whole set");
        assert_eq!(
            peer.interest_pending.enters.len(),
            INTEREST_DELTA_PENDING_MAX + 1,
            "and nothing was evicted to make room"
        );
        assert_eq!(
            peer.interest_pending.enters.first().copied(),
            Some(7),
            "the held enter at the front survives"
        );
        assert!(
            peer.interest_pending.enters.contains(&9_999),
            "and so does the one that just arrived"
        );

        // The backstop bounds a connection that is never sent a set. It is the only thing that drops.
        while peer.interest_pending.enters.len() < INTEREST_DELTA_PENDING_HARD_MAX {
            let next = 500_000 + peer.interest_pending.enters.len() as u64;
            peer.note_interest_enter(next);
        }
        peer.note_interest_enter(999_999);
        assert_eq!(
            peer.interest_pending.enters.len(),
            INTEREST_DELTA_PENDING_HARD_MAX,
            "the ceiling holds"
        );
    }

    /// Adopting a whole set emits the DIFF against what this peer held, not an event per slot: a
    /// resync that announced the whole set would re-announce every entity the peer never lost, and a
    /// game acting on those signals would rebuild nodes that were never gone.
    ///
    /// And a slot it cannot name leaves the set short, so the ask stays up. The manifest that binds
    /// that slot re-announces nothing — the rebuild only removes — so calling the ask answered is how
    /// the hole becomes permanent.
    #[test]
    fn adopting_a_whole_set_emits_the_diff_and_keeps_asking_when_it_is_short() {
        let slots = table_naming(&[11, 12]);
        let mut mirror = Mirror::default();
        // Held: 11 and 99. Stated: 11 and 12. So 99 leaves, 12 enters, 11 says nothing.
        mirror.apply(&slots, &[], &[0]);
        mirror.held.insert(99);
        mirror.take();

        let resolved = adopt_whole_set(&slots, &mut mirror.held, &mut mirror.events, 4, &[0, 1]);
        assert!(resolved, "every slot named resolved");
        assert_eq!(mirror.held, std::collections::HashSet::from([11u64, 12]));
        let mut events = mirror.take();
        events.sort_by_key(|&(_, id, entered)| (id, entered));
        assert_eq!(
            events,
            vec![(4, 12, true), (4, 99, false)],
            "the diff, and nothing for the member that never moved"
        );

        // A slot this peer cannot name leaves the set short, and the ask stays up.
        let resolved = adopt_whole_set(&slots, &mut mirror.held, &mut mirror.events, 4, &[0, 1, 7]);
        assert!(
            !resolved,
            "an unnameable slot is a set this peer cannot hold in full"
        );
    }

    /// **CAUSE 1: THE PENDING HALF OVERFLOWED.** The cap was written as a backstop for a peer that
    /// never acks, but a first update in a world of more than `INTEREST_DELTA_PENDING_MAX` filtered
    /// entities reaches it on a healthy link. What was dropped never reached the wire and nothing
    /// downstream could reconstruct it, so the client stayed permanently short of those entities
    /// while their rows kept arriving.
    #[test]
    fn an_overflowing_pending_half_owes_the_whole_set() {
        let mut peer = PeerState::default();
        for id in 1..=(INTEREST_DELTA_PENDING_MAX as u64) {
            peer.note_interest_enter(id);
        }
        assert!(
            !peer.interest_full_due,
            "filling the half exactly is not an overflow"
        );
        assert_eq!(
            peer.interest_pending.enters.len(),
            INTEREST_DELTA_PENDING_MAX
        );

        peer.note_interest_enter(9_999);
        assert!(
            peer.interest_full_due,
            "the entry that pushed the oldest out owes this peer a whole set"
        );
    }

    /// **CAUSE 2: A PREFIX GIVEN UP ON.** The prefix is still dropped rather than re-queued — that
    /// is what stops one unreachable peer accumulating for ever — but the two ends now disagree
    /// about what was sent, and only a whole set settles that. An ACKNOWLEDGED prefix owes nothing.
    #[test]
    fn a_prefix_given_up_on_owes_the_whole_set_and_an_acked_one_does_not() {
        let mut peer = peer_holding(7);
        peer.note_interest_enter(7);
        peer.interest_delta_tick = Some(100);
        peer.interest_delta_entered_sent = 1;
        peer.retire_interest_delta(100 + INTEREST_DELTA_RETRY_TICKS);
        assert!(peer.interest_full_due, "given up on unacknowledged");

        let mut acked = peer_holding(7);
        acked.note_interest_enter(7);
        acked.interest_delta_tick = Some(100);
        acked.interest_delta_entered_sent = 1;
        acked.newest_ack = 100;
        acked.retire_interest_delta(101);
        assert!(
            !acked.interest_full_due,
            "an acknowledged prefix is the ordinary path and owes nothing"
        );
    }

    /// **CAUSE 3, AND THE ONE ONLY A CLIENT SEES: an enter naming a slot the manifest has not bound.**
    ///
    /// The section rides an UNRELIABLE snapshot; the manifest binding its slots rides a RELIABLE
    /// channel, and the two have no ordering relationship. So a snapshot can arrive naming a slot
    /// whose binding is still in ENet's retransmit queue. Dropping it in silence, which is what this
    /// used to do, left the server free to retire the enter on that frame's ack — and the entity was
    /// never announced again while its rows kept arriving.
    ///
    /// An unresolvable LEAVE is deliberately NOT reported: the id is not in the mirror to remove,
    /// and the manifest rebuild emits that leave itself.
    #[test]
    fn an_enter_naming_an_unbound_slot_is_reported_and_a_leave_is_not() {
        let slots = table_naming(&[11]);
        let mut mirror = std::collections::HashSet::new();
        let mut events = Vec::new();

        let unbound_enter = InterestDeltaSection {
            generation: 0,
            left: Vec::new(),
            entered: vec![9],
        };
        assert!(
            !apply_interest_section(&slots, &mut mirror, &mut events, 4, &unbound_enter),
            "an enter this peer cannot name is what raises FLAG_WANT_INTEREST"
        );
        assert!(mirror.is_empty());
        assert!(events.is_empty(), "and it announces nothing meanwhile");

        let unbound_leave = InterestDeltaSection {
            generation: 0,
            left: vec![9],
            entered: Vec::new(),
        };
        assert!(
            apply_interest_section(&slots, &mut mirror, &mut events, 4, &unbound_leave),
            "an unresolvable leave asks for nothing -- the manifest rebuild emits it"
        );
    }

    /// A whole set supersedes every pending transition, so the halves are cleared rather than sent:
    /// each entry in them is a move into or out of the set the frame states outright.
    ///
    /// Driven through `PeerState`'s own methods rather than by restating the send path's body, so a
    /// change to that body can still fail this.
    #[test]
    fn a_seeded_connection_owes_nothing_until_something_makes_it() {
        let mut peer = peer_holding(7);
        peer.note_interest_enter(7);
        peer.interest_delta_tick = Some(100);
        peer.interest_delta_entered_sent = 1;
        assert!(
            !peer.interest_full_due,
            "an ordinary tick owes no whole set"
        );

        // The ack retires the prefix and seeds the connection, and still owes nothing.
        peer.newest_ack = 100;
        peer.retire_interest_delta(101);
        assert!(peer.interest_seeded);
        assert!(!peer.interest_full_due);
        assert!(
            peer.interest_pending.enters.is_empty(),
            "the prefix is drained"
        );

        // Only a cause does. Overflow is the one a connection can reach on a healthy link.
        for id in 1..=(INTEREST_DELTA_PENDING_MAX as u64 + 1) {
            peer.note_interest_enter(id);
        }
        assert!(
            peer.interest_full_due,
            "and then a whole set is what it is owed"
        );
    }

    /// A veto is the second leave that happens between updates, and the rule stated above
    /// `update_interest` is that each queues its own event. The despawn sweep did; this did not, so
    /// a server saw no `entity_left_interest` for a veto while a retraction still produced an
    /// `entity_entered_interest` — an unpaired enter in any handler mirroring the two.
    #[test]
    fn a_veto_reports_the_leave_it_queued() {
        let mut peer = peer_holding(7);
        assert!(
            peer.set_entity_hidden(7, true),
            "vetoing an entity this connection held is a leave to announce"
        );
        assert!(
            !peer.set_entity_hidden(7, true),
            "and vetoing it again announces nothing -- it is already out of the set"
        );
        let mut stranger = peer_holding(7);
        assert!(
            !stranger.set_entity_hidden(9, true),
            "nor does vetoing one this connection never held"
        );
        assert!(
            !peer.set_entity_hidden(7, false),
            "a retraction announces nothing either -- the next update reports the re-entry"
        );
    }

    /// **THE COUNT CAP IS NOT REDUNDANT WITH THE BYTE BUDGET.** A narrow input schema packs a block
    /// into a handful of bytes, so a fleet of owned bodies fits inside one datagram and the byte
    /// budget refuses none of them — while the server refuses everything past
    /// `MAX_INPUT_BLOCKS_PER_TICK` and truncated at the same index every tick, so the bodies past it
    /// were never driven at all. Refusing here is what puts them in the rota instead.
    #[test]
    fn a_narrow_input_schema_still_rotates_past_the_block_cap() {
        let cap = MAX_INPUT_BLOCKS_PER_TICK as usize;
        let count = cap + 36;
        // 9 bytes each: the whole fleet is far inside one frame, so only the count cap can refuse.
        let lengths = vec![9usize; count];
        let budget = MAX_FRAME_PAYLOAD;
        assert!(
            lengths.iter().sum::<usize>() <= budget,
            "the fixture has to fit, or the byte budget would be doing the refusing"
        );

        let mut out = Vec::new();
        let rotor = admit_input_blocks(&lengths, 0, budget, &mut out);
        assert_eq!(out.len(), cap, "one frame carries at most the cap");
        assert_eq!(
            rotor, cap,
            "and the first refusal is where the next tick starts"
        );

        // The tail rides next tick, which is the property the truncation denied it.
        let mut second = Vec::new();
        let _ = admit_input_blocks(&lengths, rotor, budget, &mut second);
        assert!(
            second.contains(&(count - 1)),
            "the last body must get a turn; it never did before"
        );

        // Every body is offered within the two ticks it takes to walk the fleet.
        let mut seen: std::collections::HashSet<usize> = out.iter().copied().collect();
        seen.extend(second.iter().copied());
        assert_eq!(seen.len(), count, "and the rota covers the whole fleet");

        // AND A BLOCK THAT CAN NEVER FIT DOES NOT HOLD THE FRONT OF THE ROTA. Parking on it would
        // refuse it again every tick and starve everything behind it -- which the count cap made
        // reachable, since a fleet can now be refused for its size rather than its bytes.
        let mut wedged = vec![9usize; count];
        wedged[0] = budget + 1;
        let mut third = Vec::new();
        let parked = admit_input_blocks(&wedged, 0, budget, &mut third);
        assert!(!third.contains(&0), "the oversized block cannot ride");
        assert_ne!(parked, 0, "and the rota does not park on it");
        let mut fourth = Vec::new();
        let _ = admit_input_blocks(&wedged, parked, budget, &mut fourth);
        assert!(
            !fourth.is_empty() && fourth != third,
            "so the blocks behind it keep making progress"
        );
    }

    /// A veto is a leave that happens between updates, so it has to be queued where it is declared.
    /// Exactly one, and only for an entity this connection actually held — vetoing something it was
    /// never sent announces a departure that never happened.
    #[test]
    fn a_veto_queues_exactly_one_leave() {
        let mut peer = peer_holding(7);
        peer.set_entity_hidden(7, true);
        assert_eq!(peer.interest_pending.leaves, vec![7]);
        assert!(peer.interest_pending.enters.is_empty());

        // A second veto of the same entity queues nothing further: it is no longer in the set.
        peer.set_entity_hidden(7, true);
        assert_eq!(peer.interest_pending.leaves, vec![7], "still one leave");

        // And a veto of an entity this peer never held announces nothing at all.
        let mut stranger = peer_holding(7);
        stranger.set_entity_hidden(9, true);
        assert!(
            stranger.interest_pending.is_empty(),
            "a veto on a body this connection was never sent is not a departure"
        );
    }

    /// Retracting a veto queues NOTHING. The entity re-enters through the enter radius on the next
    /// update, and that update is what reports it — announcing it at the retraction would name a
    /// body the filter may still refuse.
    #[test]
    fn a_retraction_queues_no_enter_until_the_next_update() {
        let mut peer = peer_holding(7);
        peer.set_entity_hidden(7, true);
        peer.set_entity_hidden(7, false);
        assert_eq!(peer.interest_pending.leaves, vec![7]);
        assert!(
            peer.interest_pending.enters.is_empty(),
            "the retraction announced an enter the filter had not granted"
        );

        // The next update is what grants it, and the enter replaces the pending leave rather than
        // racing it — the receiver applies a net difference, not a history.
        let (mut scratch, mut delta) = (SeatScratch::default(), InterestDelta::default());
        peer.interest.update_linear_into(
            &AoiConfig::default(),
            &[SeatObserver {
                center: [0.0; 3],
                membership: MEMBERSHIP_GLOBAL,
            }],
            &[InterestCandidate::anchored(7, [1.0, 0.0, 0.0])],
            &mut scratch,
            &mut delta,
        );
        assert_eq!(delta.enters, vec![7]);
        peer.note_interest_enter(7);
        assert!(peer.interest_pending.leaves.is_empty());
        assert_eq!(peer.interest_pending.enters, vec![7]);
    }

    /// A despawn is the other leave no `leaves` list can name: the entity is out of the candidate
    /// list, so the next update diffs a union it has already been taken out of.
    #[test]
    fn a_despawn_queues_a_leave_on_every_peer_holding_it() {
        let mut holders = [peer_holding(7), peer_holding(7)];
        let mut stranger = PeerState::default();
        stranger.last_sent.insert(7, 400);

        for peer in &mut holders {
            assert!(peer.forget_entity(7), "this connection held it");
            assert_eq!(peer.interest_pending.leaves, vec![7]);
            assert!(!peer.interest.contains(7), "and it is out of the set");
            assert!(!peer.last_sent.contains_key(&7), "with its delta chain");
        }
        assert!(
            !stranger.forget_entity(7),
            "a connection the entity was never relevant to is told nothing"
        );
        assert!(stranger.interest_pending.is_empty());
        assert!(!stranger.last_sent.contains_key(&7), "cleared either way");
    }

    /// **CULLING OFF SENDS NOTHING.** The interest pass does not run without a radius or a declared
    /// membership, so a peer's set describes a tick the session has moved on from. A gate that
    /// diffed against it would announce a leave for every entity in a session replicating all of
    /// them to everybody.
    #[test]
    fn culling_off_queues_nothing_and_sends_no_section() {
        let slots = table_naming(&[7]);
        let mut peer = peer_holding(7);
        // Something IS pending — from a veto declared before the radius was turned off — so the
        // test proves the gate rather than an empty queue.
        peer.set_entity_hidden(7, true);
        assert_eq!(peer.interest_pending.leaves, vec![7]);

        let (mut left, mut entered) = (Vec::new(), Vec::new());
        let carries =
            build_interest_section(&slots, &mut peer, false, 100, &mut left, &mut entered);
        assert!(!carries, "a session that culls nothing raises no flag");
        assert!(left.is_empty() && entered.is_empty());
        assert_eq!(
            interest_delta_reserve(left.len() + entered.len()),
            0,
            "and it takes nothing off the byte budget"
        );
        assert!(
            peer.interest_delta_tick.is_none(),
            "nothing rode, so nothing is waiting to be acknowledged"
        );
    }

    /// The section rides an unreliable datagram, so it is re-sent until the peer's ack reaches the
    /// tick it FIRST rode on. The stamp does not move on a re-send: what an ack has to reach is the
    /// frame whose arrival proves the client applied these entries.
    #[test]
    fn a_pending_delta_rides_again_until_the_peer_acks_the_tick_it_first_rode_on() {
        let slots = table_naming(&[7, 8]);
        let mut peer = peer_holding(7);
        peer.note_interest_leave(7);
        peer.note_interest_enter(8);

        let (carries, left, entered) = section_for(&slots, &mut peer, 100);
        assert!(carries);
        assert_eq!((left, entered), (vec![0u16], vec![1u16]));
        peer.interest_delta_tick = Some(100); // what the send path stamps

        // Re-sent verbatim on the next tick, and the stamp holds still.
        let (carries, left, entered) = section_for(&slots, &mut peer, 101);
        assert!(carries);
        assert_eq!((left, entered), (vec![0u16], vec![1u16]));
        assert_eq!(peer.interest_delta_tick, Some(100));

        // An ack that has not reached the stamp changes nothing.
        peer.newest_ack = 99;
        let (carries, _, _) = section_for(&slots, &mut peer, 102);
        assert!(
            carries,
            "an ack for an older frame proves nothing about this"
        );

        // An ack that reaches it retires the prefix, and there is nothing left to send.
        peer.newest_ack = 100;
        let (carries, left, entered) = section_for(&slots, &mut peer, 103);
        assert!(!carries);
        assert!(left.is_empty() && entered.is_empty());
        assert!(peer.interest_pending.is_empty());
        assert!(peer.interest_seeded, "and the connection counts as seeded");
    }

    /// The retry bound. Past [`INTEREST_DELTA_RETRY_TICKS`] an ack can no longer confirm the frame
    /// anyway, so the prefix is dropped unconfirmed rather than reserving budget for ever. What is
    /// lost is those events; the rest of the pending delta takes the next frame.
    #[test]
    fn an_unacked_prefix_is_given_up_on_at_the_retry_bound() {
        let slots = table_naming(&[7, 8]);
        let mut peer = peer_holding(7);
        peer.note_interest_leave(7);
        let (carries, _, _) = section_for(&slots, &mut peer, 100);
        assert!(carries);
        peer.interest_delta_tick = Some(100);
        // A second event, queued behind the prefix that is already in flight.
        peer.note_interest_enter(8);

        let (carries, left, entered) =
            section_for(&slots, &mut peer, 100 + INTEREST_DELTA_RETRY_TICKS - 1);
        assert!(carries, "still inside the window");
        assert_eq!((left, entered), (vec![0u16], Vec::new()));

        // At the bound the prefix goes, and the event queued behind it takes the next frame.
        let (carries, left, entered) =
            section_for(&slots, &mut peer, 100 + INTEREST_DELTA_RETRY_TICKS);
        assert!(carries);
        assert_eq!(left, Vec::<u16>::new(), "the unconfirmed leave was dropped");
        assert_eq!(entered, vec![1u16], "and the one behind it rides");
        assert_eq!(peer.interest_pending.leaves, Vec::<u64>::new());
    }

    /// An id is named in at most ONE half, and the newer transition is the one that survives. That
    /// is what makes the receiver's "remove each `left`, add each `entered`" apply correct whatever
    /// order it walks them in, and correct whether or not the intermediate frame landed.
    #[test]
    fn the_newer_transition_replaces_the_older_rather_than_racing_it() {
        let mut peer = PeerState::default();
        peer.note_interest_enter(7);
        peer.note_interest_leave(7);
        assert_eq!(peer.interest_pending.leaves, vec![7]);
        assert!(peer.interest_pending.enters.is_empty());

        peer.note_interest_enter(7);
        assert_eq!(peer.interest_pending.enters, vec![7]);
        assert!(peer.interest_pending.leaves.is_empty());
    }

    /// A burst larger than one frame — a joining peer, whose first update enters everything it can
    /// see — is spread over frames rather than eating the byte budget in one. Nothing is dropped.
    #[test]
    fn a_burst_larger_than_one_frame_is_spread_over_frames() {
        let ids: Vec<u64> = (1..=(INTEREST_DELTA_PER_FRAME as u64 + 5)).collect();
        let slots = table_naming(&ids);
        let mut peer = PeerState::default();
        for &id in &ids {
            peer.note_interest_enter(id);
        }

        let (carries, left, entered) = section_for(&slots, &mut peer, 100);
        assert!(carries && left.is_empty());
        assert_eq!(entered.len(), INTEREST_DELTA_PER_FRAME, "one frame's worth");
        peer.interest_delta_tick = Some(100);

        peer.newest_ack = 100;
        let (carries, _, entered) = section_for(&slots, &mut peer, 101);
        assert!(carries);
        assert_eq!(entered.len(), 5, "and the remainder on the next frame");
    }

    /// **A LEAVE WHOSE CAUSE IS AN UNREGISTER NAMES A SLOT THE TABLE HAS RELEASED.** It is retired
    /// here rather than carried, because the client's own manifest rebuild emits that leave — the
    /// two produce exactly one event between them, and the mirrored set is what guarantees it.
    ///
    /// An ENTER whose slot has not arrived is the opposite case and is HELD: its slot is inside
    /// another entity's reuse quarantine and lands shortly. The walk stops there, so the prefix
    /// stays contiguous from the front.
    #[test]
    fn an_unnameable_leave_is_retired_and_an_unnameable_enter_is_held() {
        let slots = table_naming(&[7]); // 9 is not in the table
        let mut peer = PeerState::default();
        peer.note_interest_leave(9);
        peer.note_interest_leave(7);
        let (carries, left, _) = section_for(&slots, &mut peer, 100);
        assert!(carries);
        assert_eq!(left, vec![0u16], "only the one the table can still name");
        assert_eq!(
            peer.interest_pending.leaves,
            vec![7],
            "and the unnameable one is gone from the queue for good"
        );

        let mut peer = PeerState::default();
        peer.note_interest_enter(9);
        peer.note_interest_enter(7);
        let (carries, _, entered) = section_for(&slots, &mut peer, 100);
        assert!(!carries || entered.is_empty());
        assert_eq!(entered, Vec::<u16>::new(), "the walk stopped at the first");
        assert_eq!(
            peer.interest_pending.enters,
            vec![9, 7],
            "and both are still queued for the tick the slot lands"
        );
    }

    /// **THE SEND-PATH OVERRUN THIS RESERVE EXISTS TO STOP.** The admit loop fills the body to its
    /// budget, and the section is appended afterward; without taking the reserve off the budget
    /// FIRST, a full frame plus a maximal section is a datagram past the path MTU, which fragments,
    /// and a lost fragment costs the whole frame.
    #[test]
    fn a_full_frame_plus_a_maximal_interest_delta_still_fits_the_budget() {
        let budget = MAX_FRAME_PAYLOAD;
        let left: Vec<u16> = (0..INTEREST_DELTA_PER_FRAME as u16).collect();
        let entered: Vec<u16> = (1000..1000 + INTEREST_DELTA_PER_FRAME as u16).collect();
        let reserve = interest_delta_reserve(left.len() + entered.len());
        assert_eq!(reserve, 13 + 2 * 2 * INTEREST_DELTA_PER_FRAME);

        // The admit loop spends every byte it is allowed to, and the last block lands exactly on
        // the line.
        let mut body = Writer::with_capacity(budget);
        body.bytes(&vec![0u8; budget - reserve]);
        assert_eq!(body.len(), budget - reserve);
        encode_interest_delta(u64::MAX, &left, &entered, &mut body);
        assert!(
            body.len() <= budget,
            "a full frame plus its section overran the budget by {}",
            body.len() - budget
        );

        // And the reserve is not merely large enough on average: it is exact at every count one
        // frame can carry.
        for count in 0..=(2 * INTEREST_DELTA_PER_FRAME) {
            let slots: Vec<u16> = (0..count as u16).collect();
            let mut writer = Writer::new();
            // `u64::MAX` is the widest varint the generation can be, which is what the
            // reserve claims to cover.
            encode_interest_delta(u64::MAX, &slots, &[], &mut writer);
            assert!(
                writer.len() <= interest_delta_reserve(count.max(1)),
                "the reserve underestimated a section of {count} slots"
            );
        }
    }

    // ------------------------------------------------------------------
    // The client half: the mirrored set, and the two things that can shrink it.
    // ------------------------------------------------------------------

    /// A client's mirrored set and the events it queued, as one thing a test can drive.
    #[derive(Default)]
    struct Mirror {
        held: std::collections::HashSet<u64>,
        events: Vec<(i32, u64, bool)>,
    }

    impl Mirror {
        fn apply(&mut self, slots: &SlotTable, left: &[u16], entered: &[u16]) {
            let section = InterestDeltaSection {
                generation: 0,
                left: left.to_vec(),
                entered: entered.to_vec(),
            };
            let _ = apply_interest_section(slots, &mut self.held, &mut self.events, 4, &section);
        }

        fn rebuild(&mut self, slots: &SlotTable) {
            retire_unnamed_interest(slots, &mut self.held, &mut self.events, 4);
        }

        fn take(&mut self) -> Vec<(i32, u64, bool)> {
            std::mem::take(&mut self.events)
        }
    }

    /// A re-sent section announces nothing the second time. That is what makes the server's
    /// re-send-until-acked free, and it is the whole reason the apply is a set operation.
    #[test]
    fn re_applying_an_interest_section_announces_nothing() {
        let slots = table_naming(&[11, 12]);
        let mut mirror = Mirror::default();

        mirror.apply(&slots, &[], &[0, 1]);
        assert_eq!(mirror.take(), vec![(4, 11, true), (4, 12, true)]);

        mirror.apply(&slots, &[], &[0, 1]);
        assert!(mirror.take().is_empty(), "a repeat is free");

        mirror.apply(&slots, &[0], &[]);
        assert_eq!(mirror.take(), vec![(4, 11, false)]);
        mirror.apply(&slots, &[0], &[]);
        assert!(mirror.take().is_empty(), "and so is a repeated leave");
    }

    /// **A LEAVE WHOSE CAUSE IS AN UNREGISTER NAMES A SLOT THE TABLE IS ABOUT TO RELEASE.** The
    /// client resolves against its LIVE table and drops an unbound slot silently — the alternative
    /// is announcing whatever entity that slot is rebound to next.
    #[test]
    fn a_section_naming_an_unbound_slot_is_dropped_silently() {
        let slots = table_naming(&[11]);
        let mut mirror = Mirror::default();
        mirror.apply(&slots, &[], &[0]);
        mirror.take();

        // Slot 9 is bound to nothing. Neither half acts on it, and neither errors.
        mirror.apply(&slots, &[9], &[9]);
        assert!(mirror.take().is_empty());
        assert_eq!(mirror.held.len(), 1, "and the set is untouched");
    }

    /// **ONE SIGNAL COVERS BOTH CAUSES**, and an entity culled and unregistered on the same tick
    /// fires it EXACTLY ONCE — whichever of the two arrives second finds nothing to remove.
    #[test]
    fn a_cull_and_an_unregister_on_one_tick_fire_exactly_once() {
        let bound = table_naming(&[11, 12]);
        // 11 has unregistered and slot 0 is released; 12 KEEPS the slot it was bound to.
        let released = table_binding(&[(1, 12)]);

        // The section lands first, then the manifest rebuild that drops the same entity.
        let mut mirror = Mirror::default();
        mirror.apply(&bound, &[], &[0, 1]);
        mirror.take();
        mirror.apply(&bound, &[0], &[]);
        mirror.rebuild(&released);
        assert_eq!(
            mirror.take(),
            vec![(4, 11, false)],
            "the cull announced it; the rebuild found nothing left to announce"
        );

        // And the other order, which is the one a reliable manifest overtaking an unreliable
        // snapshot actually produces. The section then names a slot the table has released.
        let mut mirror = Mirror::default();
        mirror.apply(&bound, &[], &[0, 1]);
        mirror.take();
        mirror.rebuild(&released);
        mirror.apply(&released, &[0], &[]);
        assert_eq!(
            mirror.take(),
            vec![(4, 11, false)],
            "the rebuild announced it; the section named a slot nothing binds"
        );
    }

    /// A manifest rebuild announces only what LEFT it. An entity still named keeps its place, and an
    /// entity the mirror never held is not announced as leaving.
    #[test]
    fn a_manifest_rebuild_announces_only_what_it_stopped_naming() {
        let slots = table_naming(&[11, 12, 13]);
        let mut mirror = Mirror::default();
        mirror.apply(&slots, &[], &[0, 1]);
        mirror.take();

        mirror.rebuild(&slots);
        assert!(mirror.take().is_empty(), "nothing stopped being named");

        // 11 and 13 unregister. Only 11 was in this peer's interest.
        mirror.rebuild(&table_binding(&[(1, 12)]));
        assert_eq!(mirror.take(), vec![(4, 11, false)]);
        assert_eq!(mirror.held.iter().copied().collect::<Vec<_>>(), vec![12]);
    }

    // ------------------------------------------------------------------
    // A declared observer, and what it overrides.
    // ------------------------------------------------------------------

    const HERE: [f32; 3] = [1.0, 2.0, 3.0];
    const THERE: [f32; 3] = [900.0, 0.0, -900.0];

    fn body_in(center: [f32; 3], membership: MembershipId) -> Option<PeerObserver> {
        Some(observed(center, membership))
    }

    /// The default, and the whole rule before a peer could declare one: both facts off the body the
    /// peer drives.
    #[test]
    fn an_undeclared_peer_takes_both_facts_from_the_body_it_drives() {
        assert_eq!(
            resolve_observer(PeerAnchor::Inferred, 5, None, None, body_in(HERE, 8)),
            (Some(HERE), 8),
            "the declared field is not read at all without a declaration"
        );
        // Driving nothing: no center, and every world. Both halves fail open together.
        assert_eq!(
            resolve_observer(PeerAnchor::Inferred, 5, None, None, None),
            (None, MEMBERSHIP_GLOBAL)
        );
    }

    /// THE POINT OF THE DECLARATION. A peer observing one world while driving a body in another must
    /// be centered where it is LOOKING and filtered in the world it is WATCHING -- the body it drives
    /// must pull it back on neither axis.
    #[test]
    fn a_declaration_overrides_the_driven_body_on_both_axes() {
        assert_eq!(
            resolve_observer(PeerAnchor::Fixed(HERE), 5, None, None, body_in(THERE, 8)),
            (Some(HERE), 5)
        );
        assert_eq!(
            resolve_observer(
                PeerAnchor::Entity(7),
                5,
                Some(HERE),
                None,
                body_in(THERE, 8)
            ),
            (Some(HERE), 5)
        );
    }

    /// A declaration of MEMBERSHIP_GLOBAL is a declaration, not an absence: a peer told to watch
    /// every world must not be pulled back into its avatar's one.
    #[test]
    fn a_peer_declared_into_every_world_is_not_returned_to_its_bodys_world() {
        assert_eq!(
            resolve_observer(
                PeerAnchor::Fixed(HERE),
                MEMBERSHIP_GLOBAL,
                None,
                None,
                body_in(THERE, 8)
            ),
            (Some(HERE), MEMBERSHIP_GLOBAL)
        );
    }

    /// A tracked entity that despawns leaves the peer where it last was. Falling back to the driven
    /// body would move the peer into whichever world that body is in, and falling back to "no center"
    /// would open its radius to the whole world at the moment its avatar died.
    #[test]
    fn a_tracked_center_survives_the_entity_it_tracks() {
        assert_eq!(
            resolve_observer(
                PeerAnchor::Entity(7),
                5,
                None,
                Some(HERE),
                body_in(THERE, 8)
            ),
            (Some(HERE), 5)
        );
    }

    /// THE TWO AXES FAIL SEPARATELY. A tracked entity that has never resolved -- declared before it
    /// spawned -- gives no center, so nothing is distance-culled. The peer nonetheless stays in the
    /// world it was DECLARED into: a membership is a declaration and did not fail.
    #[test]
    fn a_tracked_center_that_never_resolved_keeps_its_declared_world() {
        assert_eq!(
            resolve_observer(PeerAnchor::Entity(7), 5, None, None, body_in(THERE, 8)),
            (None, 5)
        );
    }

    // ------------------------------------------------------------------
    // Which band a candidate row scores in.
    // ------------------------------------------------------------------

    /// THE FIX THAT LET INTEREST MANAGEMENT SHIP ON. An unanchored channel has no distance, and the
    /// `0.0` `PeerInterest` stores for it reads as `Near`. It must be banded `Far` on the absence of
    /// an anchor rather than on that stored zero -- otherwise a distant player's flashlight and hit points
    /// outbid that player's POSITION row four to one under budget pressure.
    #[test]
    fn a_row_with_no_anchor_is_far_whatever_distance_is_stored_for_it() {
        for dist_sq in [0.0f32, 1.0, 400.0, 9_000_000.0] {
            assert_eq!(
                band_for_row(true, false, dist_sq, 256.0),
                Band::Far,
                "no anchor, so no distance boost -- stored dist_sq {dist_sq} must not matter"
            );
        }
        // ...and the row it must not outbid: the same body's POSITION, which does carry an anchor and
        // is banded by where that body actually is.
        assert_eq!(band_for_row(true, true, 0.0, 256.0), Band::Near);
        assert_eq!(band_for_row(true, true, 300.0 * 300.0, 256.0), Band::Far);
    }

    /// With culling off there are no distances to band by, so every row takes one constant weight that
    /// cancels out of the descending sort. Reading the stored `0.0` as a real distance would be a guess.
    #[test]
    fn with_culling_off_every_row_takes_the_same_band() {
        for has_anchor in [true, false] {
            for dist_sq in [0.0f32, 9_000_000.0] {
                assert_eq!(band_for_row(false, has_anchor, dist_sq, 256.0), Band::Near);
            }
        }
    }

    // ------------------------------------------------------------------
    // When a block must go out full: the keyframe interval.
    // ------------------------------------------------------------------

    /// The rule this replaces, written out so the regression reads as a disagreement between two
    /// arithmetics rather than as prose. It is `full_block_due` with the keyframe measured from
    /// the last send.
    fn superseded_rule(
        want_full: bool,
        id: u64,
        current: u64,
        last_sent: u64,
        interval: u64,
    ) -> bool {
        want_full
            || last_sent == 0
            || (orbitnet_core::interest::send_phase(id, current, interval)
                && current.saturating_sub(last_sent) >= interval)
    }

    /// The tick inside `[from, from + interval)` at which `id`'s keyframe phase comes up.
    fn phase_tick(id: u64, from: u64, interval: u64) -> u64 {
        (from..from + interval)
            .find(|&t| orbitnet_core::interest::send_phase(id, t, interval))
            .expect("send_phase fires exactly once per interval")
    }

    /// Both rules evaluated on the same tick. An entity sent every tick as a delta the receiver
    /// cannot decode has `last_sent == current`, so an interval measured from the last send never
    /// elapses and the only unconditional repair never comes due. Measured from the last full
    /// block it comes due on schedule, whatever the delta traffic in between is doing.
    // ------------------------------------------------------------------
    // What a NACK invalidates. See `PeerState::note_nack`.
    // ------------------------------------------------------------------

    #[test]
    fn a_nack_asks_for_full_rows() {
        let mut peer = PeerState::default();
        assert!(!peer.want_full);
        peer.note_nack();
        assert!(peer.want_full, "the peer asked for full state");
    }

    #[test]
    fn a_nack_drops_every_delta_base_held_for_that_peer() {
        // The loop this closes: the peer acked the FRAME, so `consume_ack` promoted a base for
        // every entity it carried -- including the one whose block answered NoBase and was never
        // stored. Keeping those bases means every later masked delta against them fails the same
        // way, and the NACK is per-peer so no single entry can be picked out as the bad one.
        let mut peer = PeerState::default();
        peer.acked_base.insert(7, 400);
        peer.acked_base.insert(8, 401);
        peer.acked_base.insert(9, 402);
        peer.note_nack();
        assert!(
            peer.acked_base.is_empty(),
            "an ack proves a frame arrived, not that its blocks integrated, so none of these bases \
             survives the peer telling us one of them was undecodable"
        );
    }

    #[test]
    fn a_dropped_base_sends_a_full_row_rather_than_a_delta() {
        // The consequence the drop exists for, at the one place it is read: no entry means no
        // reference, and no reference is a full block -- which always decodes.
        let mut peer = PeerState::default();
        peer.acked_base.insert(7, 400);
        assert_eq!(peer.acked_base.get(&7).copied(), Some(400));
        peer.note_nack();
        assert_eq!(
            peer.acked_base.get(&7).copied(),
            None,
            "and the send path turns a missing base into a full row"
        );
    }

    #[test]
    fn a_nack_leaves_the_other_per_entity_bookkeeping_alone() {
        // `last_sent` and `last_full` describe what the SENDER did, which a NACK does not call into
        // question. Clearing `last_full` here would re-arm the keyframe interval on every NACK and
        // spend a second full block on entities that just had one.
        let mut peer = PeerState::default();
        peer.last_sent.insert(7, 400);
        peer.last_full.insert(7, 396);
        peer.acked_base.insert(7, 400);
        peer.note_nack();
        assert_eq!(peer.last_sent.get(&7).copied(), Some(400));
        assert_eq!(peer.last_full.get(&7).copied(), Some(396));
        assert!(peer.acked_base.is_empty());
    }

    // ------------------------------------------------------------------
    // The delta base a receiver can still hold. See `delta_reference`.
    // ------------------------------------------------------------------

    #[test]
    fn a_fresh_base_is_referenced() {
        assert_eq!(delta_reference(100, 104, 128), Some(100));
    }

    #[test]
    fn a_base_one_tick_inside_the_span_is_still_referenced() {
        // `current - base == span - 1` is the oldest tick the ring still maps to its own slot.
        assert_eq!(delta_reference(100, 100 + 127, 128), Some(100));
    }

    #[test]
    fn a_base_exactly_one_span_old_is_refused() {
        // `t + span` lands in the same ring slot as `t`, so writing it evicts the base. This is the
        // boundary the guard exists for: one tick either side is a keyframe or a want_full storm.
        assert_eq!(delta_reference(100, 100 + 128, 128), None);
    }

    #[test]
    fn a_base_far_beyond_the_span_is_refused() {
        assert_eq!(delta_reference(100, 100_000, 128), None);
    }

    #[test]
    fn a_frozen_base_is_refused_once_the_tick_runs_past_it() {
        // What a lost ack looks like: `acked_base` stops advancing while `current` does not. The
        // guard has to flip from Some to None as the gap crosses the span, because every delta after
        // that point is one the peer is guaranteed to reject.
        let span: u64 = 64;
        let base: u64 = 500;
        assert!(delta_reference(base, base + span - 1, span).is_some());
        assert!(delta_reference(base, base + span, span).is_none());
        assert!(delta_reference(base, base + span + 1, span).is_none());
    }

    #[test]
    fn a_base_at_or_ahead_of_the_current_tick_is_referenced() {
        // `saturating_sub` floors at zero rather than wrapping to a huge gap. Ticks are published
        // before the send phase runs and a peer's clock leads, so this is reachable, not defensive.
        assert_eq!(delta_reference(120, 100, 128), Some(120));
        assert_eq!(delta_reference(100, 100, 128), Some(100));
    }

    #[test]
    fn the_smallest_span_still_admits_the_present_tick() {
        // The caller passes `history_limit.max(2)`, so the span is never 0 or 1.
        assert_eq!(delta_reference(10, 10, 2), Some(10));
        assert_eq!(delta_reference(10, 11, 2), Some(10));
        assert_eq!(delta_reference(10, 12, 2), None);
    }

    #[test]
    fn a_keyframe_comes_due_even_while_deltas_keep_going_out() {
        let id: u64 = 7;
        let interval = FULL_STATE_INTERVAL;
        let last_full: u64 = 100;
        let due = phase_tick(id, last_full + interval, interval);
        // Deltas have gone out every tick since, so the last send is this very tick.
        let last_sent = due;
        assert!(
            full_block_due(false, id, due, last_full, interval),
            "a keyframe {} ticks past the last full block is due however recently a delta went out",
            due - last_full
        );
        assert!(
            !superseded_rule(false, id, due, last_sent, interval),
            "the superseded rule must disagree here; that disagreement is the defect, and a gate \
             both rules pass would not catch it"
        );
    }

    #[test]
    fn a_keyframe_is_not_due_before_its_interval_elapses() {
        let id: u64 = 7;
        let interval = FULL_STATE_INTERVAL;
        let last_full: u64 = 100;
        for tick in last_full..last_full + interval {
            assert!(
                !full_block_due(false, id, tick, last_full, interval),
                "tick {tick} is inside the interval and must stay a delta"
            );
        }
    }

    /// The two unconditional reasons, each on its own: a NACK, and an entity nothing full has ever
    /// gone out for (fresh, or bookkeeping cleared at an interest leave).
    #[test]
    fn a_nack_or_an_absent_base_forces_a_full_block() {
        let interval = FULL_STATE_INTERVAL;
        assert!(full_block_due(true, 7, 500, 499, interval), "NACK");
        assert!(
            full_block_due(false, 7, 500, 0, interval),
            "never sent full"
        );
    }

    /// Phase-spread by id, so a session's keyframes are level traffic rather than one spike per
    /// interval. `send_phase` provides that property and this rule must not lose it.
    #[test]
    fn keyframes_spread_across_the_interval_by_entity_id() {
        let interval = FULL_STATE_INTERVAL;
        // Not 0: that is "never had a full block", which is unconditionally due for every id.
        let last_full: u64 = 1;
        for tick in interval * 2..interval * 3 {
            let due: Vec<u64> = (0..interval)
                .filter(|&id| full_block_due(false, id, tick, last_full, interval))
                .collect();
            assert_eq!(
                due.len(),
                1,
                "tick {tick} must owe exactly one id a keyframe"
            );
        }
    }

    // ------------------------------------------------------------------
    // Which rejections may raise a WANT_FULL NACK.
    // ------------------------------------------------------------------

    #[test]
    fn only_a_missing_delta_base_asks_for_a_full_block() {
        assert_eq!(
            classify_rx(StateIntegration::NoBase, false),
            RxOutcome::Nack
        );
        for outcome in [
            StateIntegration::Confirmed,
            StateIntegration::Buffered,
            StateIntegration::Mispredict(7),
        ] {
            assert_eq!(classify_rx(outcome, false), RxOutcome::Applied);
        }
    }

    /// The regression this split exists to stop: a reordered or duplicated datagram is what a real
    /// link does every second, and answering it with a per-peer all-entity full-state burst is a
    /// storm that sustains itself.
    #[test]
    fn a_superseded_block_is_counted_and_never_nacked() {
        assert_eq!(
            classify_rx(StateIntegration::Stale, false),
            RxOutcome::StaleDrop
        );
        assert_eq!(
            classify_rx(StateIntegration::Stale, true),
            RxOutcome::StaleDrop
        );
    }

    /// Asking for a full block cannot fix a full block.
    #[test]
    fn a_block_that_was_already_full_does_not_ask_for_another() {
        assert_eq!(
            classify_rx(StateIntegration::NoBase, true),
            RxOutcome::StaleDrop
        );
    }

    // 60 Hz, so a tick is 16.667 ms and the arithmetic below reads in the units the policy uses.
    const TICK_MS: f64 = 1000.0 / 60.0;

    // A peer that has acked `samples.len()` frames, each `gap` ticks after the server sent it. The ack
    // tick advances by one each time, which is what an honest client does.
    fn peer_with(gaps: &[u64]) -> PeerState {
        let mut peer = PeerState::default();
        for (i, &gap) in gaps.iter().enumerate() {
            let ack = i as u64 + 1;
            peer.note_ack(ack, ack + gap, TICK_MS);
        }
        peer
    }

    #[test]
    fn no_samples_is_no_estimate() {
        assert_eq!(PeerState::default().rtt_ms(), None);
    }

    #[test]
    fn a_sample_is_stamped_in_milliseconds_at_the_rate_it_arrived_on() {
        // The same six-tick gap is a different DURATION at a different rate, and the stored figure
        // has to be the duration -- reading ticks back later is what the tick-stamping cleanup removed elsewhere.
        let fast = peer_with(&[6]);
        let mut slow = PeerState::default();
        slow.note_ack(1, 7, 1000.0 / 30.0);
        assert!((fast.rtt_ms().unwrap() - 100.0).abs() < 0.1);
        assert!((slow.rtt_ms().unwrap() - 200.0).abs() < 0.1);
    }

    #[test]
    fn the_estimate_is_the_minimum_of_the_window_not_the_newest() {
        // The security property: a peer inflates samples by withholding acknowledgments and can
        // never deflate one, so one honest round trip inside the window discards every inflated
        // sample. The newest reading here is the worst one and must not be what is believed.
        let peer = peer_with(&[3, 60, 90, 120]);
        assert!((peer.rtt_ms().unwrap() - 3.0 * TICK_MS as f32).abs() < 0.1);
    }

    #[test]
    fn an_honest_sample_leaving_the_window_lets_the_estimate_rise() {
        // ...and the filter is not a permanent floor: once the good sample ages out, the estimate
        // follows the link up, so a peer whose route genuinely got worse is compensated for the
        // worse route. That is also why the window cannot be the containment on its own -- see the
        // residual tests below.
        let mut peer = peer_with(&[3]);
        for i in 0..RTT_WINDOW as u64 {
            let ack = i + 2;
            peer.note_ack(ack, ack + 30, TICK_MS);
        }
        assert!((peer.rtt_ms().unwrap() - 30.0 * TICK_MS as f32).abs() < 0.1);
    }

    #[test]
    fn the_window_never_grows_past_its_bound() {
        let peer = peer_with(&vec![10; RTT_WINDOW * 4]);
        assert_eq!(peer.rtt_samples.len(), RTT_WINDOW);
    }

    #[test]
    fn an_absurd_gap_is_capped_rather_than_stored() {
        // A peer that has said nothing for an hour must not park an hour in the window. The 250 ms
        // ceiling in NetLagComp is what bounds the rewind; this only bounds what is remembered.
        let mut peer = PeerState::default();
        peer.note_ack(1, u64::MAX, TICK_MS);
        assert_eq!(peer.rtt_ms().unwrap(), RTT_SAMPLE_MAX_MS);
    }

    #[test]
    fn withholding_an_ack_cannot_raise_the_estimate() {
        // THE ATTACK. A peer acks honestly for a while, then stops advancing its ack_tick while the
        // server ticks on. Measuring every arriving frame would grow `now - newest_ack` without
        // bound and hand this peer the ceiling -- for free, because an unadvanced acked_base makes
        // the SERVER send full blocks, so going quiet costs the attacker nothing at all.
        let mut peer = peer_with(&[3, 3, 3]);
        let honest = peer.rtt_ms().unwrap();
        for now in 100..1000 {
            // Same stale ack, every frame, while the server's clock runs away from it.
            assert!(
                !peer.note_ack(3, now, TICK_MS),
                "a stale ack is not a measurement"
            );
        }
        assert_eq!(
            peer.rtt_ms().unwrap(),
            honest,
            "the estimate is frozen, not inflated"
        );
    }

    #[test]
    fn a_deliberately_lagged_ack_still_inflates_the_estimate() {
        // KNOWN RESIDUAL, pinned here so nobody re-derives the stronger claim this code has already
        // attracted twice. Two rules close the cheap versions of the attack: `note_ack` refuses an
        // ack that does not ADVANCE, so a peer that says nothing new gets no sample at all, and
        // `consume_ack` refuses an ack whose frame token the peer could not be holding, so a peer
        // cannot name a frame that never reached it. Neither ties the ack to the HIGHEST frame the
        // peer received. A client that advances at full rate while holding a constant lag quotes a
        // real token every time, is measured at that lag, and reads identically to a slow peer.
        //
        // No wire field closes this one. `current - ack` is the whole round trip whatever lead the
        // client runs at, so the server has no second quantity to derive an independent figure from.
        // The containment is the millisecond ceiling in `NetLagComp`, not anything here.
        let mut peer = peer_with(&[3, 3, 3]);
        let honest = peer.rtt_ms().unwrap();
        for i in 0..(RTT_WINDOW as u64 * 2) {
            let ack = i + 10;
            peer.note_ack(ack, ack + 16, TICK_MS); // full-rate advance, constant 16-tick lag
        }
        let inflated = peer.rtt_ms().unwrap();
        assert!(
            inflated > honest,
            "a lagged-but-advancing ack is not rejected"
        );
        assert!(
            (inflated - 16.0 * TICK_MS as f32).abs() < 0.1,
            "it is believed at exactly the lag claimed: {inflated} ms"
        );
    }

    #[test]
    fn a_worsening_under_report_is_held_to_its_smallest_claimed_lag() {
        // The half of the residual the minimum filter DOES answer. A peer whose claimed lag grows
        // over the window is believed at the SMALLEST gap in it, not the newest or the largest --
        // so an under-report that ramps is pinned to wherever it started, and only a lag held flat
        // from the first sample is believed in full. That is the same property that makes an
        // honest peer's occasional late ack harmless.
        let mut peer = PeerState::default();
        for i in 1..=(RTT_WINDOW as u64) {
            // Claimed lag grows: 59 ticks on the first sample, ~3800 by the last.
            peer.note_ack(i, i * 60, TICK_MS);
        }
        let smallest_gap_ms = 59.0 * TICK_MS as f32;
        assert!(
            (peer.rtt_ms().unwrap() - smallest_gap_ms).abs() < 0.1,
            "held to the first (smallest) claimed lag, not the largest: {} ms",
            peer.rtt_ms().unwrap()
        );
    }

    // ------------------------------------------------------------------
    // The BELIEF ceiling: what the server is willing to believe about a link, as distinct from what
    // it measured. Read `PeerState::rtt_believed_ms` for why the clamp is at the read.
    // ------------------------------------------------------------------

    /// The shipped ceiling, in the `f32` the peer-state read takes.
    const CEILING_MS: f32 = RTT_BELIEVED_MAX_MS_DEFAULT as f32;

    #[test]
    fn a_lagged_but_advancing_ack_is_believed_only_to_the_ceiling() {
        // The companion to `a_deliberately_lagged_ack_still_inflates_the_estimate`, same 16-tick lag.
        // That test pins the residual: the inflated figure is measured and stored, because nothing on
        // the wire can tell this peer from an honest one 267 ms away. This one pins the containment:
        // what the server BELIEVES about the link is the ceiling, and that is what the rewind reads.
        let mut peer = peer_with(&[3, 3, 3]);
        for i in 0..(RTT_WINDOW as u64 * 2) {
            let ack = i + 10;
            peer.note_ack(ack, ack + 16, TICK_MS); // full-rate advance, constant 16-tick lag
        }
        let raw = peer.rtt_ms().unwrap();
        assert!(
            (raw - 16.0 * TICK_MS as f32).abs() < 0.1,
            "the raw estimate still reports the inflated figure: {raw} ms"
        );
        assert!(
            raw > CEILING_MS,
            "16 ticks at 60 Hz is 267 ms, which must be above the ceiling or this proves nothing"
        );
        assert_eq!(
            peer.rtt_believed_ms(CEILING_MS),
            Some(CEILING_MS),
            "the believed figure is the ceiling, not the claim"
        );
    }

    #[test]
    fn an_honest_link_under_the_ceiling_is_untouched() {
        // The ceiling binds AT the ceiling and nowhere below it. A peer measured at 50 ms is believed
        // at 50 ms, and a peer measured at exactly the ceiling is believed in full -- the cap is a
        // minimum against the ceiling, not a rounding of everything toward it.
        let peer = peer_with(&[3, 3, 3]);
        assert!((peer.rtt_ms().unwrap() - 3.0 * TICK_MS as f32).abs() < 0.1);
        assert_eq!(
            peer.rtt_believed_ms(CEILING_MS),
            peer.rtt_ms(),
            "an honest link reads exactly as measured"
        );
        let at_ceiling = peer_with(&[15]); // 15 ticks at 60 Hz is 250 ms, the ceiling itself
        assert_eq!(at_ceiling.rtt_believed_ms(CEILING_MS), at_ceiling.rtt_ms());
    }

    #[test]
    fn the_raw_estimate_survives_the_ceiling_for_diagnostics() {
        // Clamping the STORED sample would make every peer above the ceiling report the same figure,
        // and a scoreboard ping would tell each of them the same lie. The window keeps what it
        // measured, so the two reads answer differently at the same instant, from the same peer.
        let peer = peer_with(&[60]); // 1000 ms
        let raw = peer.rtt_ms().unwrap();
        assert!(
            (raw - 60.0 * TICK_MS as f32).abs() < 0.1,
            "the raw read is the honest number a diagnostic wants: {raw} ms"
        );
        assert_eq!(peer.rtt_believed_ms(CEILING_MS), Some(CEILING_MS));
        assert!(
            raw > peer.rtt_believed_ms(CEILING_MS).unwrap(),
            "the two reads disagree, which is the whole point of keeping both"
        );
    }

    #[test]
    fn a_peer_with_no_estimate_answers_neither() {
        // "No sample yet" is a different state from "a perfect link", and the ceiling must not turn
        // one into the other: a caller reading 0.0 for a fresh joiner would hand it the shallowest
        // rewind in the session at the moment its link is least settled.
        let peer = PeerState::default();
        assert_eq!(peer.rtt_ms(), None);
        assert_eq!(peer.rtt_believed_ms(CEILING_MS), None);
        assert_eq!(
            peer.rtt_believed_ms(0.0),
            None,
            "a ceiling of zero still answers 'no estimate' rather than 0 ms"
        );
    }

    #[test]
    fn the_ceiling_gauge_counts_only_peers_above_it() {
        // The gauge is a count of peers the ceiling is CURRENTLY BINDING ON, so every other state has
        // to be excluded by construction: at the ceiling is believed in full, no sample is no
        // measurement, and an unsynced peer is not in the session the `peers` figure counts.
        let synced = |mut p: PeerState| -> PeerState {
            p.synced = true;
            p
        };
        let peers = [
            synced(peer_with(&[60])),     // 1000 ms -- above
            synced(peer_with(&[30])),     // 500 ms  -- above
            synced(peer_with(&[15])),     // 250 ms  -- exactly at the ceiling, believed in full
            synced(peer_with(&[3])),      // 50 ms   -- below
            synced(PeerState::default()), // no sample at all
            peer_with(&[60]),             // above, but never handshook
        ];
        assert_eq!(rtt_at_ceiling_peers(peers.iter(), CEILING_MS), 2);
        assert_eq!(
            rtt_at_ceiling_peers(peers.iter(), RTT_SAMPLE_MAX_MS),
            0,
            "a ceiling nothing can reach binds on nobody"
        );
        assert_eq!(
            rtt_at_ceiling_peers(peers.iter(), 0.0),
            4,
            "and a ceiling of zero binds on every synced peer that has a sample at all"
        );
        assert_eq!(
            rtt_at_ceiling_peers(std::iter::empty::<&PeerState>(), CEILING_MS),
            0,
            "an empty session reports 0 rather than dividing by anything"
        );
    }

    // A peer holding a server-minted secret, as one does from its handshake onward.
    fn peer_with_salt(salt: u8) -> PeerState {
        PeerState {
            token_salt: Some([salt; KEY_LEN]),
            ..Default::default()
        }
    }

    #[test]
    fn a_frame_token_is_specific_to_the_frame_and_to_the_peer() {
        // What makes a token proof of receipt rather than arithmetic: a client knows every tick number
        // and it knows the session key it minted, so a token derived from those would be computable for
        // a frame that never arrived. Both axes have to separate.
        let peer = peer_with_salt(0x5a);
        assert_ne!(
            peer.frame_token(100),
            peer.frame_token(101),
            "two frames of one session share a token"
        );
        assert_ne!(
            peer.frame_token(100),
            peer_with_salt(0x5b).frame_token(100),
            "two peers share a token for the same tick"
        );
    }

    #[test]
    fn a_peer_with_no_salt_can_prove_nothing() {
        // A peer that has not handshaken has been sent no frame, so there is nothing it could prove
        // and no value that may pass for a proof -- including the `0` an empty header carries.
        let peer = PeerState::default();
        assert_eq!(peer.frame_token(10), None);
        assert!(!peer.ack_is_proven(10, 0));
    }

    #[test]
    fn an_ack_quoting_its_frames_token_is_consumed() {
        let mut peer = peer_with_salt(0x11);
        peer.sent_log.push_back((10, vec![(7, 10)]));
        let token = peer.frame_token(10).unwrap();
        assert_eq!(
            peer.consume_ack(10, token, 0, 13, TICK_MS),
            AckOutcome::Consumed
        );
        assert_eq!(peer.newest_ack, 10);
        assert!((peer.rtt_ms().unwrap() - 3.0 * TICK_MS as f32).abs() < 0.1);
        assert_eq!(peer.acked_base.get(&7), Some(&10));
    }

    #[test]
    fn an_ack_the_peer_cannot_prove_buys_nothing() {
        // THE ATTACK THIS CLOSES. An ack is a claim about what arrived, and `newest_ack`, the
        // round-trip sample and the `acked_base` promotion are all granted on the strength of it. A
        // peer that names a frame it never received cannot produce the token for it, and gets none of
        // the three -- the sent log still holds the frame, awaiting an ack that is real.
        let mut peer = peer_with_salt(0x11);
        peer.sent_log.push_back((10, vec![(7, 10)]));
        let forged = peer.frame_token(10).unwrap() ^ 1;
        assert_eq!(
            peer.consume_ack(10, forged, 0, 13, TICK_MS),
            AckOutcome::Unproven
        );
        assert_eq!(peer.newest_ack, 0);
        assert_eq!(
            peer.rtt_ms(),
            None,
            "an unproven claim is not a measurement"
        );
        assert!(peer.acked_base.is_empty());
        assert_eq!(peer.sent_log.len(), 1);
    }

    #[test]
    fn a_token_from_another_frame_does_not_prove_this_one() {
        // The replay shape, and the one an under-reporting peer would reach for first: it genuinely
        // holds the token of every frame that reached it, so refusing a token is not enough -- the
        // token has to be refused FOR THE TICK BEING CLAIMED.
        let mut peer = peer_with_salt(0x22);
        let held = peer.frame_token(40).unwrap();
        assert_eq!(
            peer.consume_ack(41, held, 0, 45, TICK_MS),
            AckOutcome::Unproven
        );
        assert_eq!(
            peer.consume_ack(40, held, 0, 45, TICK_MS),
            AckOutcome::Consumed
        );
    }

    #[test]
    fn a_peer_that_has_received_nothing_yet_is_not_refused() {
        // `ack_tick` 0 is every peer between its handshake and its first snapshot. It claims nothing,
        // so there is nothing to prove and nothing to count as a refusal.
        let mut peer = peer_with_salt(0x33);
        assert_eq!(peer.consume_ack(0, 0, 0, 9, TICK_MS), AckOutcome::Empty);
        assert_eq!(peer.rtt_ms(), None);
    }

    #[test]
    fn the_ack_bits_ride_on_the_proven_tick() {
        // The bits name 32 frames older than `ack` and prove nothing themselves. They are consumed
        // because the tick they hang off was proven, and refused with it when it was not.
        let mut peer = peer_with_salt(0x44);
        peer.sent_log.push_back((8, vec![(1, 8)]));
        peer.sent_log.push_back((10, vec![(2, 10)]));
        let token = peer.frame_token(10).unwrap();
        assert_eq!(
            peer.consume_ack(10, token ^ 0xff, 0b10, 12, TICK_MS),
            AckOutcome::Unproven
        );
        assert!(peer.acked_base.is_empty(), "the bits came in on a lie");
        assert_eq!(
            peer.consume_ack(10, token, 0b10, 12, TICK_MS),
            AckOutcome::Consumed
        );
        assert_eq!(peer.acked_base.get(&1), Some(&8));
        assert_eq!(peer.acked_base.get(&2), Some(&10));
    }

    #[test]
    fn a_genuinely_slower_route_is_still_measured() {
        // The other side of the same rule: a peer whose acks keep ADVANCING but arrive later is on
        // a worse link and must be compensated for it. Only refusing to advance is refused.
        let mut peer = peer_with(&[3, 3, 3]);
        let fast = peer.rtt_ms().unwrap();
        for i in 0..RTT_WINDOW as u64 {
            let ack = i + 10;
            peer.note_ack(ack, ack + 24, TICK_MS);
        }
        assert!(
            peer.rtt_ms().unwrap() > fast,
            "a slower but honest peer reads slower"
        );
        assert!((peer.rtt_ms().unwrap() - 24.0 * TICK_MS as f32).abs() < 0.1);
    }

    #[test]
    fn an_ack_ahead_of_the_accumulator_reads_as_zero_rather_than_underflowing() {
        // `current.saturating_sub(newest_ack)` at the call site: ticks are published before the
        // send phase runs and a peer's clock leads, so the gap really can be negative. On u64 that
        // would wrap to ~1.8e19 ticks and, uncapped, hand the peer the deepest window there is.
        let peer = peer_with(&[0]);
        assert_eq!(peer.rtt_ms().unwrap(), 0.0);
    }

    // ------------------------------------------------------------------
    // Resume: which dropped sessions a rejoiner may claim, and for how long.
    // ------------------------------------------------------------------

    /// The resume token a held record was minted with. Every claim in this section quotes it, because a
    /// claim that does not is refused before any of the rules under test are reached.
    const HELD_TOKEN: u64 = 0x5ec0_ffee_1234_5678;

    #[test]
    fn a_rejoiner_claims_the_session_it_dropped_and_learns_its_old_peer_id() {
        let mut table = ResumeTable::default();
        assert!(table.hold(0xabcd, 7, 1_000, HELD_TOKEN));
        assert!(table.holds(0xabcd));
        assert_eq!(table.claim(0xabcd, HELD_TOKEN), Some(7));
        assert!(!table.holds(0xabcd), "claiming spends the session");
    }

    /// A peer that claimed no identity has nothing to resume, and `0` must not become a slot every
    /// anonymous joiner in turn inherits.
    #[test]
    fn identity_zero_is_never_held_and_never_claimed() {
        let mut table = ResumeTable::default();
        assert!(!table.hold(0, 7, 1_000, HELD_TOKEN));
        assert!(!table.holds(0));
        assert_eq!(table.claim(0, HELD_TOKEN), None);
        assert_eq!(table.claim(0, 0), None, "nor with no token at all");
    }

    /// Resuming is once. A second connection carrying a token the first already spent is a newcomer, or
    /// two live peers would be seated on one entity.
    #[test]
    fn a_second_claimant_of_one_token_is_a_newcomer() {
        let mut table = ResumeTable::default();
        table.hold(9, 3, 1_000, HELD_TOKEN);
        assert_eq!(table.claim(9, HELD_TOKEN), Some(3));
        assert_eq!(table.claim(9, HELD_TOKEN), None);
    }

    #[test]
    fn an_unheld_token_is_a_newcomer() {
        let mut table = ResumeTable::default();
        table.hold(1, 4, 1_000, HELD_TOKEN);
        assert_eq!(table.claim(2, HELD_TOKEN), None);
        assert!(table.holds(1), "and the held session is untouched");
    }

    /// **A WRONG QUOTE MUST NOT SPEND SOMEBODY ELSE'S WINDOW.** Refusing the claim closes the takeover;
    /// refusing it and consuming the record would turn one forged hello into a denial of service — the real
    /// player comes back inside the grace window and finds nothing to resume — which is worse than the
    /// takeover the token exists to refuse.
    #[test]
    fn a_mismatched_claim_is_refused_and_leaves_the_held_session_in_place() {
        let mut table = ResumeTable::default();
        table.hold(0xabcd, 7, 1_000, HELD_TOKEN);
        assert_eq!(
            table.claim(0xabcd, HELD_TOKEN ^ 1),
            None,
            "one bit is wrong"
        );
        assert_eq!(table.claim(0xabcd, 0), None, "and quoting nothing is wrong");
        assert!(table.holds(0xabcd), "the window is still open");
        assert_eq!(
            table.claim(0xabcd, HELD_TOKEN),
            Some(7),
            "and the player it belongs to still resumes"
        );
    }

    /// A record minted with no token grants on the identity alone. It is what a session held for a
    /// connection this server issued no token to looks like, and refusing it would refuse a resume nobody
    /// could ever satisfy.
    #[test]
    fn a_record_with_no_token_accepts_any_quote() {
        let mut table = ResumeTable::default();
        table.hold(4, 6, 1_000, 0);
        assert_eq!(table.token_of(4), 0);
        assert_eq!(table.claim(4, 0xdead_beef), Some(6));
    }

    /// The token on record is what `handle_hello` reads to decide the grant, so it has to answer for a held
    /// session and answer `0` — rather than some other session's token — for anything else.
    #[test]
    fn the_token_on_record_is_readable_and_zero_for_an_unheld_identity() {
        let mut table = ResumeTable::default();
        table.hold(11, 2, 1_000, HELD_TOKEN);
        assert_eq!(table.token_of(11), HELD_TOKEN);
        assert_eq!(table.token_of(12), 0, "an identity nothing is held for");
        assert_eq!(table.token_of(0), 0, "and identity zero is never held");
    }

    /// The window is inclusive at its deadline, and a session past it is gone from the table as well as
    /// reported — a release the game hears about twice would open the seat twice.
    #[test]
    fn a_session_expires_at_its_deadline_and_is_reported_once() {
        let mut table = ResumeTable::default();
        table.hold(5, 2, 1_000, HELD_TOKEN);
        assert!(table.expire(999).is_empty(), "not due yet");
        assert_eq!(table.expire(1_000), vec![(5, 2)]);
        assert!(table.expire(2_000).is_empty(), "and not reported again");
        assert!(!table.holds(5));
    }

    /// Expiries are reported in session order rather than `HashMap` order, so a game that logs or acts on
    /// the batch behaves the same on every run.
    #[test]
    fn several_expiries_are_reported_in_a_stable_order() {
        let mut table = ResumeTable::default();
        table.hold(30, 3, 100, HELD_TOKEN);
        table.hold(10, 1, 100, HELD_TOKEN);
        table.hold(20, 2, 100, HELD_TOKEN);
        assert_eq!(table.expire(100), vec![(10, 1), (20, 2), (30, 3)]);
    }

    /// A player who drops, rejoins, and drops again gets a window measured from the SECOND drop, and the
    /// token that comes with it is the one the second connection was holding.
    #[test]
    fn re_holding_a_session_restarts_its_window() {
        let mut table = ResumeTable::default();
        table.hold(8, 2, 1_000, HELD_TOKEN);
        table.hold(8, 5, 4_000, HELD_TOKEN ^ 0xff);
        assert_eq!(table.token_of(8), HELD_TOKEN ^ 0xff, "the newer token");
        assert!(table.expire(1_000).is_empty(), "the first deadline is gone");
        assert_eq!(
            table.expire(4_000),
            vec![(8, 5)],
            "and the newer peer id is what is reported"
        );
    }

    #[test]
    fn a_drop_is_not_held_without_a_server_a_window_or_an_identity() {
        assert!(
            hold_on_drop(0xabcd, 30_000, true),
            "the ordinary drop holds"
        );
        assert!(!hold_on_drop(0xabcd, 30_000, false), "not a server");
        assert!(!hold_on_drop(0xabcd, 0, true), "resume switched off");
        // Also the GHOST case: a stale connection whose identity was taken by a GRANTED resume carries
        // identity 0 by the time its disconnect lands, so it re-opens no window. A ghost whose resume was
        // REFUSED keeps its identity, and its own drop opens the real window.
        assert!(!hold_on_drop(0, 30_000, true), "no identity");
    }

    #[test]
    fn teardown_forgets_every_held_session() {
        let mut table = ResumeTable::default();
        table.hold(1, 1, 1_000, HELD_TOKEN);
        table.hold(2, 2, 1_000, HELD_TOKEN);
        table.clear();
        assert!(table.expire(u64::MAX).is_empty());
        assert!(!table.holds(1));
    }

    // ------------------------------------------------------------------
    // The resume decision: which claims on an identity a server grants, and what a refusal leaves behind.
    // ------------------------------------------------------------------

    /// An unknown policy number falls onto ALWAYS, which is the OPPOSITE direction from the seat-release
    /// clamp and deliberately so: there the safe answer takes nothing away, here the safe answer refuses
    /// nobody. ALWAYS is token-gated, so falling onto it forfeits nothing the token was closing, while
    /// falling onto a stricter policy would lock honest players out of their own bodies.
    #[test]
    fn an_unknown_resume_policy_number_reads_back_as_always() {
        assert_eq!(RESUME_ALWAYS, 0, "and the default is the unset value");
        assert_eq!(clamp_resume_policy(3), RESUME_ALWAYS);
        assert_eq!(clamp_resume_policy(-1), RESUME_ALWAYS);
        assert_eq!(clamp_resume_policy(i64::MAX), RESUME_ALWAYS);
        assert_eq!(clamp_resume_policy(RESUME_ALWAYS), RESUME_ALWAYS);
        assert_eq!(
            clamp_resume_policy(RESUME_ONLY_IF_DROPPED),
            RESUME_ONLY_IF_DROPPED
        );
        assert_eq!(clamp_resume_policy(RESUME_NEVER), RESUME_NEVER);
    }

    /// The whole grant matrix, written out rather than derived, because it is the rule the facade doc and
    /// `docs/protocol.md` both paraphrase.
    #[test]
    fn the_resume_grant_matrix_is_what_the_documentation_says() {
        const TOKEN: u64 = 0x1234_5678_9abc_def0;
        // (policy, presented, on record, incumbent is live, granted)
        let table = [
            // Nothing on record: a first-time join, granted under everything but NEVER.
            (RESUME_ALWAYS, 0, 0, false, true),
            (RESUME_ONLY_IF_DROPPED, 0, 0, false, true),
            (RESUME_NEVER, 0, 0, false, false),
            // A dropped incumbent, token quoted correctly: the case resume exists for.
            (RESUME_ALWAYS, TOKEN, TOKEN, false, true),
            (RESUME_ONLY_IF_DROPPED, TOKEN, TOKEN, false, true),
            (RESUME_NEVER, TOKEN, TOKEN, false, false),
            // A LIVE incumbent, token quoted correctly: the fast reconnect ALWAYS exists for, and the
            // one case ONLY_IF_DROPPED refuses.
            (RESUME_ALWAYS, TOKEN, TOKEN, true, true),
            (RESUME_ONLY_IF_DROPPED, TOKEN, TOKEN, true, false),
            (RESUME_NEVER, TOKEN, TOKEN, true, false),
            // The observer: it has the identity and not the token. Refused under every policy, which is
            // the whole point of the token.
            (RESUME_ALWAYS, 0, TOKEN, false, false),
            (RESUME_ALWAYS, TOKEN ^ 1, TOKEN, false, false),
            (RESUME_ALWAYS, TOKEN ^ 1, TOKEN, true, false),
            (RESUME_ONLY_IF_DROPPED, TOKEN ^ 1, TOKEN, false, false),
            (RESUME_NEVER, TOKEN ^ 1, TOKEN, false, false),
            // A quoted token against a record that has none grants on the identity alone.
            (RESUME_ALWAYS, TOKEN, 0, false, true),
            (RESUME_ALWAYS, TOKEN, 0, true, true),
            (RESUME_ONLY_IF_DROPPED, TOKEN, 0, true, false),
            // A policy number this build does not know behaves as ALWAYS.
            (7, TOKEN, TOKEN, true, true),
            (7, TOKEN ^ 1, TOKEN, false, false),
        ];
        for (policy, presented, on_record, live, granted) in table {
            let want = if granted {
                ResumeGrant::Resume
            } else {
                ResumeGrant::Newcomer
            };
            assert_eq!(
                resume_grant(policy, presented, on_record, live),
                want,
                "policy {policy}, presented {presented:#x}, on record {on_record:#x}, live {live}"
            );
        }
    }

    /// A connected peer holding the identity, as the ghost of a client that relaunched.
    fn incumbent(session_id: u64, token: u64) -> PeerState {
        PeerState {
            synced: true,
            session_id,
            resume_token: token,
            ..Default::default()
        }
    }

    /// **THE DEFECT, PINNED.** A peer that merely observed another's session id quotes it with no token and
    /// takes nothing: the incumbent keeps its identity, and the observer is seated anonymously rather than
    /// under a name that belongs to somebody else.
    #[test]
    fn an_observer_quoting_an_identity_without_its_token_takes_nothing() {
        const ID: u64 = 0xabcd;
        const TOKEN: u64 = 0x5ec0_ffee_0000_0001;
        let mut peers = HashMap::new();
        peers.insert(7, incumbent(ID, TOKEN));
        let mut resume = ResumeTable::default();

        let seat = seat_hello(&mut peers, &mut resume, RESUME_ALWAYS, 9, ID, 0);
        assert_eq!(seat.grant, ResumeGrant::Newcomer);
        assert_eq!(seat.resumed_from, 0, "nothing was taken over");
        assert_eq!(seat.session_id, 0, "and the claimant is anonymous");
        assert_eq!(
            peers[&7].session_id, ID,
            "the player who was playing keeps its identity"
        );
        assert_eq!(peers[&7].resume_token, TOKEN, "and its token");
    }

    /// The case ALWAYS exists for, and it still works: the returning player quotes the token it was issued
    /// and takes its own body back from the ghost, before the transport has noticed the old socket is gone.
    #[test]
    fn the_player_holding_the_token_resumes_past_a_live_ghost() {
        const ID: u64 = 0xabcd;
        const TOKEN: u64 = 0x5ec0_ffee_0000_0002;
        let mut peers = HashMap::new();
        peers.insert(7, incumbent(ID, TOKEN));
        let mut resume = ResumeTable::default();

        let seat = seat_hello(&mut peers, &mut resume, RESUME_ALWAYS, 9, ID, TOKEN);
        assert_eq!(seat.grant, ResumeGrant::Resume);
        assert_eq!(
            seat.resumed_from, 7,
            "and the game is told which connection"
        );
        assert_eq!(seat.session_id, ID);
        assert_eq!(seat.token_on_record, TOKEN, "carried onto the new seat");
        assert_eq!(
            peers[&7].session_id, 0,
            "the ghost's identity is taken, so its late disconnect opens no window"
        );
    }

    /// **UNDER `ONLY_IF_DROPPED` THE SUPERSEDE STEP MUST NOT RUN.** The incumbent keeps its identity, so
    /// its own disconnect still opens a real window; running it backward would leave the ghost holding
    /// identity `0`, `hold_on_drop` would refuse to hold anything for it, and the player would lose the
    /// session to a peer that was just told it could not have it.
    #[test]
    fn a_refused_resume_leaves_the_incumbents_identity_alone() {
        const ID: u64 = 0xabcd;
        const TOKEN: u64 = 0x5ec0_ffee_0000_0003;
        let mut peers = HashMap::new();
        peers.insert(7, incumbent(ID, TOKEN));
        let mut resume = ResumeTable::default();

        // The honest player, with the right token, refused only because the incumbent is still connected.
        let seat = seat_hello(
            &mut peers,
            &mut resume,
            RESUME_ONLY_IF_DROPPED,
            9,
            ID,
            TOKEN,
        );
        assert_eq!(seat.grant, ResumeGrant::Newcomer);
        assert_eq!(seat.session_id, 0, "seated as an anonymous newcomer");
        assert_eq!(seat.resumed_from, 0);
        assert_eq!(peers[&7].session_id, ID, "the incumbent keeps its identity");
        assert!(
            hold_on_drop(peers[&7].session_id, 30_000, true),
            "so its own drop still opens a window the player can come back to"
        );
    }

    /// The other half of the `ONLY_IF_DROPPED` story: once the drop the policy was waiting for lands, the
    /// same claim is granted.
    #[test]
    fn only_if_dropped_grants_the_claim_once_the_incumbent_has_gone() {
        const ID: u64 = 0xabcd;
        const TOKEN: u64 = 0x5ec0_ffee_0000_0004;
        let mut peers: HashMap<i32, PeerState> = HashMap::new();
        let mut resume = ResumeTable::default();
        resume.hold(ID, 7, 1_000, TOKEN);

        let seat = seat_hello(
            &mut peers,
            &mut resume,
            RESUME_ONLY_IF_DROPPED,
            9,
            ID,
            TOKEN,
        );
        assert_eq!(seat.grant, ResumeGrant::Resume);
        assert_eq!(seat.resumed_from, 7);
        assert_eq!(seat.session_id, ID);
        assert!(!resume.holds(ID), "and the window is spent");
    }

    /// A refused claim on a HELD identity must not be seated under it either. Its own later drop would
    /// re-hold the record with the wrong token, which takes the identity from the player it belongs to for
    /// good — a worse outcome than the takeover being refused here.
    #[test]
    fn a_refused_claim_on_a_held_identity_is_seated_anonymously() {
        const ID: u64 = 0xabcd;
        const TOKEN: u64 = 0x5ec0_ffee_0000_0005;
        let mut peers: HashMap<i32, PeerState> = HashMap::new();
        let mut resume = ResumeTable::default();
        resume.hold(ID, 7, 1_000, TOKEN);

        let seat = seat_hello(&mut peers, &mut resume, RESUME_ALWAYS, 9, ID, TOKEN ^ 1);
        assert_eq!(seat.grant, ResumeGrant::Newcomer);
        assert_eq!(seat.session_id, 0);
        assert!(
            resume.holds(ID),
            "and the wrong quote spent nobody else's window"
        );
    }

    /// A refusal with NOTHING on record keeps the identity. That is a first-time joiner under NEVER: no
    /// resume is granted, but taking its identity away would leave a game under that policy with no roster
    /// key at all while protecting nobody.
    #[test]
    fn a_first_time_joiner_under_never_still_carries_its_identity() {
        const ID: u64 = 0xabcd;
        let mut peers: HashMap<i32, PeerState> = HashMap::new();
        let mut resume = ResumeTable::default();

        let seat = seat_hello(&mut peers, &mut resume, RESUME_NEVER, 9, ID, 0);
        assert_eq!(seat.grant, ResumeGrant::Newcomer, "nothing was resumed");
        assert_eq!(seat.session_id, ID, "but the identity is seated");
        assert_eq!(seat.resumed_from, 0);
    }

    /// A hello is RETRIED until the welcome lands, so the same identity re-enters for the connection that
    /// already holds it. The sender is excluded from the incumbent scan, so a retry does not supersede
    /// itself, does not report a resume it already reported, and leaves the seat where it was.
    #[test]
    fn a_retried_hello_does_not_supersede_the_connection_that_sent_it() {
        const ID: u64 = 0xabcd;
        const TOKEN: u64 = 0x5ec0_ffee_0000_0006;
        let mut peers = HashMap::new();
        peers.insert(9, incumbent(ID, TOKEN));
        let mut resume = ResumeTable::default();

        let seat = seat_hello(&mut peers, &mut resume, RESUME_ALWAYS, 9, ID, TOKEN);
        assert_eq!(seat.grant, ResumeGrant::Resume);
        assert_eq!(seat.resumed_from, 0, "it took over from nobody");
        assert_eq!(seat.session_id, ID);
        assert_eq!(peers[&9].session_id, ID, "and kept its own identity");
    }

    /// A peer that claims no identity resumes nothing, whatever it quotes. `0` is what an anonymous joiner
    /// sends, and a token cannot conjure an identity out of it.
    #[test]
    fn an_anonymous_hello_is_seated_anonymously_whatever_it_quotes() {
        let mut peers = HashMap::new();
        peers.insert(7, incumbent(0xabcd, 0x5ec0_ffee_0000_0007));
        let mut resume = ResumeTable::default();

        let seat = seat_hello(
            &mut peers,
            &mut resume,
            RESUME_ALWAYS,
            9,
            0,
            0x5ec0_ffee_0000_0007,
        );
        assert_eq!(seat.session_id, 0);
        assert_eq!(seat.resumed_from, 0);
        assert_eq!(seat.token_on_record, 0);
        assert_eq!(peers[&7].session_id, 0xabcd, "and nobody was superseded");
    }

    #[test]
    fn a_role_receives_in_the_direction_it_does_not_send() {
        // The property the direction byte rests on: each role's send and receive directions are
        // opposites, and the two roles are mirror images. An inversion here refuses every datagram.
        let (client_tx, client_rx) = session_directions(MODE_CLIENT).unwrap();
        let (server_tx, server_rx) = session_directions(MODE_SERVER).unwrap();
        assert_ne!(client_tx, client_rx);
        assert_ne!(server_tx, server_rx);
        assert_eq!(client_tx, server_rx);
        assert_eq!(server_tx, client_rx);
        assert_eq!(
            session_directions(MODE_HOST),
            session_directions(MODE_SERVER)
        );
        assert_eq!(session_directions(MODE_OFFLINE), None);
    }

    // ------------------------------------------------------------------
    // The session secret: which key a session actually seals with.
    //
    // The draw itself needs Godot's `Crypto` and is not reachable here. Everything DOWNSTREAM of the
    // draw is: `session_key_from` is the whole of what a secret changes, and both ends call it.
    // ------------------------------------------------------------------

    /// A nonce as `mint_session_key` would hand one over, without the Godot RNG.
    fn nonce_bytes(seed: u8) -> [u8; KEY_LEN] {
        let mut out = [0u8; KEY_LEN];
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = seed.wrapping_mul(31).wrapping_add(index as u8);
        }
        out
    }

    #[test]
    fn two_sessions_under_one_secret_derive_different_keys() {
        // THE CROSS-SESSION REPLAY PROPERTY, and the reason the secret is a derivation input rather
        // than the key. `SessionAuth` restarts its sequence counter at 1 on every join and the replay
        // window only ever knows the session in front of it, so two joins landing on one key would make
        // every datagram captured in the first a valid, unreplayed datagram in the second.
        let secret = compress_secret(b"a secret the lobby handed both ends");
        let first = session_key_from(Some(&secret), nonce_bytes(1));
        let second = session_key_from(Some(&secret), nonce_bytes(2));
        assert_ne!(first, second, "a fresh nonce is a fresh key");
        // The trap, named: seating the secret AS the key is the shorter implementation, and it is what
        // makes the two sessions above identical.
        assert_ne!(first, secret);
        assert_ne!(second, secret);

        // And the replay it would have allowed, run end to end. A datagram sealed in the first session
        // does not open in the second.
        let mut captured = b"input for tick 1".to_vec();
        SessionAuth::new(first)
            .seal(Direction::ToServer, &mut captured)
            .unwrap();
        assert_eq!(
            SessionAuth::new(second).open(Direction::ToServer, &captured),
            Err(AuthError::BadTag),
            "the next session refuses what the last one sealed"
        );
        assert!(
            SessionAuth::new(first)
                .open(Direction::ToServer, &captured)
                .is_ok(),
            "the negative control: it opens under the session that sealed it"
        );
    }

    #[test]
    fn a_session_with_no_secret_seals_exactly_the_bytes_it_did_before() {
        // THE COMPATIBILITY PROMISE. With no secret set, the handshake's 16 bytes are the session key,
        // verbatim, exactly as they were before a secret was a thing — so a session that configures
        // nothing puts identical bytes on the wire.
        let nonce = nonce_bytes(7);
        assert_eq!(session_key_from(None, nonce), nonce);

        let mut derived_path = b"snapshot".to_vec();
        SessionAuth::new(session_key_from(None, nonce))
            .seal(Direction::ToClient, &mut derived_path)
            .unwrap();
        let mut old_path = b"snapshot".to_vec();
        SessionAuth::new(nonce)
            .seal(Direction::ToClient, &mut old_path)
            .unwrap();
        assert_eq!(
            derived_path, old_path,
            "sequence number and tag both, byte for byte"
        );
    }

    #[test]
    fn a_peer_with_a_different_secret_derives_a_key_that_opens_nothing() {
        // What refuses a peer that does not hold the secret, once it is past the handshake: its key is
        // not the session's, so nothing it sends verifies and nothing sent to it does either.
        let nonce = nonce_bytes(3);
        let ours = session_key_from(Some(&compress_secret(b"the right secret")), nonce);
        let theirs = session_key_from(Some(&compress_secret(b"the wrong secret")), nonce);
        assert_ne!(ours, theirs, "the same nonce, a different secret");
        let mut datagram = b"input".to_vec();
        SessionAuth::new(theirs)
            .seal(Direction::ToServer, &mut datagram)
            .unwrap();
        assert_eq!(
            SessionAuth::new(ours).open(Direction::ToServer, &datagram),
            Err(AuthError::BadTag)
        );
    }

    #[test]
    fn a_retried_hello_derives_the_same_key_and_keeps_its_replay_window() {
        // A hello is retried until the welcome lands, so `handle_hello` runs again for a peer already
        // seated. It compares DERIVED KEY against derived key, and a repeated nonce derives the same
        // one — which is what makes the comparison answer "unchanged" and leave the window alone.
        // Re-deriving into a fresh `SessionAuth` on every retry would reset the window instead, and
        // anything captured from that peer could then be replayed by sending one copy of its handshake.
        let secret = compress_secret(b"a secret the lobby handed both ends");
        let nonce = nonce_bytes(11);
        let seated = session_key_from(Some(&secret), nonce);
        assert_eq!(
            session_key_from(Some(&secret), nonce),
            seated,
            "the retry's nonce is the same nonce, so the comparison sees no rekey"
        );

        let mut window = SessionAuth::new(seated);
        let mut first = b"input".to_vec();
        SessionAuth::new(seated)
            .seal(Direction::ToServer, &mut first)
            .unwrap();
        assert!(window.open(Direction::ToServer, &first).is_ok());
        assert_eq!(
            window.open(Direction::ToServer, &first),
            Err(AuthError::Replayed),
            "the window that survived the retry still refuses the repeat"
        );
    }

    // ------------------------------------------------------------------
    // Seat release: the stored policy, and the queue the drop path drains.
    //
    // What is reachable without a SceneTree is everything the two release paths DECIDE with. The
    // walk itself is not: it binds `OrbitRollbackSynchronizer` instances, which hold a `Base<Node>`
    // and cannot be constructed here, so the drain's position in `run_frame` is pinned by reading
    // the function rather than by a test.
    // ------------------------------------------------------------------

    #[test]
    fn an_unknown_policy_number_reads_back_as_hold() {
        // The property is an i64 and a project file can hold anything. Falling onto HOLD is what
        // stops an unrecognized number selecting whichever policy sits at that index in some other
        // build — the failure that takes a live player's body away.
        assert_eq!(clamp_seat_release_policy(3), SEAT_RELEASE_HOLD);
        assert_eq!(clamp_seat_release_policy(-1), SEAT_RELEASE_HOLD);
        assert_eq!(clamp_seat_release_policy(i64::MAX), SEAT_RELEASE_HOLD);
        assert_eq!(clamp_seat_release_policy(SEAT_RELEASE_HOLD), 0);
    }

    #[test]
    fn a_known_policy_number_survives_the_clamp() {
        // The negative control for the test above: a clamp that answered HOLD for everything would
        // satisfy it while switching the feature off.
        assert_eq!(
            clamp_seat_release_policy(SEAT_RELEASE_ON_EXPIRY),
            SEAT_RELEASE_ON_EXPIRY
        );
        assert_eq!(
            clamp_seat_release_policy(SEAT_RELEASE_ON_DROP),
            SEAT_RELEASE_ON_DROP
        );
    }

    #[test]
    fn the_stored_number_and_the_selected_policy_cannot_disagree() {
        // Two functions read the same number, and a game reads one of them back while this node acts
        // on the other. They are pinned against each other rather than each against a literal.
        for stored in [-2, 0, 1, 2, 3, 99] {
            let expected = match clamp_seat_release_policy(stored) {
                SEAT_RELEASE_ON_EXPIRY => SeatReleasePolicy::OnExpiry,
                SEAT_RELEASE_ON_DROP => SeatReleasePolicy::OnDrop,
                _ => SeatReleasePolicy::Hold,
            };
            assert_eq!(seat_release_policy_of(stored), expected, "stored {stored}");
        }
    }

    #[test]
    fn the_default_stored_policy_is_hold() {
        // The default-drift guard on the backend side. `OrbitNet::init` cannot be called here, so
        // what is pinned is the constant it seeds the field from.
        assert_eq!(SEAT_RELEASE_HOLD, 0);
        assert_eq!(
            seat_release_policy_of(SEAT_RELEASE_HOLD),
            SeatReleasePolicy::Hold
        );
    }

    #[test]
    fn the_default_policy_queues_no_drop_at_all() {
        // Why `_on_peer_disconnected` consults the policy before pushing: under HOLD the queue is
        // never written, so a session that sets nothing allocates nothing and cannot grow a backlog
        // while its tick loop is stopped.
        let policy = seat_release_policy_of(SEAT_RELEASE_HOLD);
        assert!(!releases_seats(policy, SeatReleaseEvent::Dropped, false));
    }

    #[test]
    fn a_repeated_drop_of_one_peer_id_is_queued_once() {
        let mut pending = Vec::new();
        queue_seat_release(&mut pending, 7);
        queue_seat_release(&mut pending, 9);
        queue_seat_release(&mut pending, 7);
        assert_eq!(pending, vec![7, 9], "the second 7 is the same walk");
    }

    #[test]
    fn the_queue_keeps_the_order_the_drops_arrived_in() {
        // Two connections dropping in one frame release in the order the transport reported them, so
        // the announcement a game sees does not depend on a hash iteration order.
        let mut pending = Vec::new();
        for peer in [4, 2, 9] {
            queue_seat_release(&mut pending, peer);
        }
        assert_eq!(pending, vec![4, 2, 9]);
    }

    #[test]
    fn the_two_paths_release_once_between_them() {
        // A held connection produces a drop AND, a grace window later, an expiry. Whichever policy is
        // chosen, exactly one of the two acts — which is what keeps the expiry from reaching seats the
        // drop already let go of.
        for stored in [
            SEAT_RELEASE_HOLD,
            SEAT_RELEASE_ON_EXPIRY,
            SEAT_RELEASE_ON_DROP,
        ] {
            let policy = seat_release_policy_of(stored);
            let on_drop = releases_seats(policy, SeatReleaseEvent::Dropped, false);
            let on_expiry = releases_seats(policy, SeatReleaseEvent::Expired, false);
            assert!(!(on_drop && on_expiry), "policy {stored} released twice");
        }
    }

    #[test]
    fn a_recycled_peer_id_releases_nothing_on_either_path() {
        // The guard both call sites re-ask at the moment they act. An id that names a live connection
        // is a newcomer holding the number a departed session was last seen under.
        for stored in [
            SEAT_RELEASE_HOLD,
            SEAT_RELEASE_ON_EXPIRY,
            SEAT_RELEASE_ON_DROP,
        ] {
            let policy = seat_release_policy_of(stored);
            assert!(!releases_seats(policy, SeatReleaseEvent::Dropped, true));
            assert!(!releases_seats(policy, SeatReleaseEvent::Expired, true));
        }
    }

    // ------------------------------------------------------------------
    // The input receive path: a row carrying a non-finite float never enters history.
    //
    // The whole rule is reachable without a SceneTree. `integrate_input_row` and
    // `input_restore_row` are free functions over `ColumnarHistory` and `FreshnessLedger`, and
    // `resim_input_from` is the planner decision the receive loop makes with the answer — so the
    // three steps a received row takes are driven here in the order `handle_client_input` drives
    // them. Only the property walk that writes the row onto the game's input node needs an engine.
    // ------------------------------------------------------------------

    /// One unannotated `Vec3` input property — the exposure the refusal exists for. With no `@`
    /// quantizer between the wire and history, whatever bit pattern arrives is what gets stored.
    fn move_schema() -> SchemaBuilder {
        let mut schema = SchemaBuilder::new();
        schema.push("move", PropKind::Vec3, PropRole::Input);
        schema
    }

    /// One native input row: three little-endian `f32` lanes, 12 bytes.
    fn move_row(x: f32, y: f32, z: f32) -> Vec<u8> {
        let mut row = Vec::with_capacity(12);
        for lane in [x, y, z] {
            row.extend_from_slice(&lane.to_le_bytes());
        }
        row
    }

    /// The receive loop's fold over one row's answer, as `handle_client_input` runs it: a landed row
    /// marks the planner at the horizon-clamped tick, and every other answer marks nothing.
    fn fold_into_planner(
        planner: &mut ResimPlanner,
        entity: u64,
        outcome: InputIntegration,
        current: u64,
    ) {
        if let InputIntegration::Landed(tick) = outcome {
            if let Some(from) = resim_input_from(tick, current) {
                planner.mark(entity, from);
            }
        }
    }

    #[test]
    fn a_non_finite_input_row_is_refused_and_the_body_coasts_on_the_previous_row() {
        let schema = move_schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 64);
        let mut ledger = FreshnessLedger::with_capacity(64);
        let mut planner = ResimPlanner::new();
        let mut latest: i64 = -1;
        let current = 20u64;
        let entity = 7u64;

        // Tick 10 is honest: it lands, stamps authoritative, and marks the planner. The baseline the
        // refusal below is measured against.
        let honest = move_row(1.0, 2.0, 3.0);
        let outcome = integrate_input_row(
            schema.props(),
            &mut history,
            &mut ledger,
            &mut latest,
            10,
            &honest,
        );
        assert_eq!(outcome, InputIntegration::Landed(10));
        fold_into_planner(&mut planner, entity, outcome, current);
        assert!(planner.is_dirty());
        planner.clear();

        // Tick 11 from the same sender poisons one lane of the same property.
        let poisoned = move_row(1.0, f32::NAN, 3.0);
        let outcome = integrate_input_row(
            schema.props(),
            &mut history,
            &mut ledger,
            &mut latest,
            11,
            &poisoned,
        );
        assert_eq!(outcome, InputIntegration::NonFinite);
        assert!(
            history.row(11).is_none(),
            "the refused row never entered history"
        );
        assert_eq!(
            latest, 10,
            "and it did not advance the newest-received-input frontier"
        );
        fold_into_planner(&mut planner, entity, outcome, current);
        assert!(
            !planner.is_dirty(),
            "no resim starts from a tick whose row was refused"
        );

        // What the restore then hands the game: the previous honest row, with the tick stamped
        // Extrapolated — the same path, and the same answer, a lost datagram gets.
        let restored =
            input_restore_row(&history, &mut ledger, 11).expect("a row at or before tick 11");
        assert_eq!(
            restored,
            honest.as_slice(),
            "the body coasts on its last honest intent rather than on an invented zero row"
        );
        assert_eq!(ledger.confidence(11), Confidence::Extrapolated);
        assert!(
            !ledger.begin_sim(11),
            "an extrapolated tick is not fresh, so no one-shot fires on the refused tick"
        );

        // THE NEGATIVE CONTROL, and it matters more than the assertions above: a check that refused
        // every row would satisfy all of them. The next good row, for a later tick, still lands,
        // still stamps authoritative, and still marks the planner.
        let next = move_row(4.0, 5.0, 6.0);
        let outcome = integrate_input_row(
            schema.props(),
            &mut history,
            &mut ledger,
            &mut latest,
            12,
            &next,
        );
        assert_eq!(outcome, InputIntegration::Landed(12));
        assert_eq!(history.row(12), Some(next.as_slice()));
        assert_eq!(ledger.confidence(12), Confidence::Authoritative);
        assert_eq!(latest, 12);
        fold_into_planner(&mut planner, entity, outcome, current);
        assert_eq!(
            planner.global_window(current, 64),
            Some(ResimRange {
                from: 12,
                to: current
            }),
            "the good row plans a resim from its own tick"
        );
    }

    #[test]
    fn every_non_finite_pattern_is_refused_and_an_absurd_finite_one_is_not() {
        // Range, rate and plausibility stay the game's job, inside `_rollback_tick`. Narrowing the
        // refusal to non-finite floats is the whole of what the backend now checks, so a movement
        // axis of 1e9 has to land: a check that also refused implausible values would break every
        // game that clamps for itself.
        let schema = move_schema();
        let mut tick = 100u64;
        for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut history = ColumnarHistory::new(schema.row_stride(), 64);
            let mut ledger = FreshnessLedger::with_capacity(64);
            let mut latest: i64 = -1;
            let refused = integrate_input_row(
                schema.props(),
                &mut history,
                &mut ledger,
                &mut latest,
                tick,
                &move_row(0.0, 0.0, poison),
            );
            assert_eq!(
                refused,
                InputIntegration::NonFinite,
                "{poison} must be refused"
            );
            let landed = integrate_input_row(
                schema.props(),
                &mut history,
                &mut ledger,
                &mut latest,
                tick,
                &move_row(0.0, 0.0, 1.0e9),
            );
            assert_eq!(
                landed,
                InputIntegration::Landed(tick),
                "an absurd but finite axis is the game's problem, not the backend's"
            );
            tick += 1;
        }
    }

    #[test]
    fn a_row_of_the_wrong_stride_is_ignored_rather_than_reported_as_poison() {
        // The gate order, and it is observable: `row_is_finite` answers false for a row shorter than
        // the schema's native extent, so a short row reaching the finiteness check before the stride
        // check would be counted as poison and would warn about the wrong thing. A sender whose
        // schema disagrees is what the entity manifest's per-entity hash reports, by name.
        let schema = move_schema();
        let mut history = ColumnarHistory::new(schema.row_stride(), 64);
        let mut ledger = FreshnessLedger::with_capacity(64);
        let mut latest: i64 = -1;
        for short in [Vec::new(), move_row(1.0, 2.0, 3.0)[..8].to_vec()] {
            assert_eq!(
                integrate_input_row(
                    schema.props(),
                    &mut history,
                    &mut ledger,
                    &mut latest,
                    5,
                    &short,
                ),
                InputIntegration::Ignored,
            );
        }
        assert!(history.row(5).is_none());
        // The negative control: the full-width row at the same tick still lands.
        assert_eq!(
            integrate_input_row(
                schema.props(),
                &mut history,
                &mut ledger,
                &mut latest,
                5,
                &move_row(1.0, 2.0, 3.0),
            ),
            InputIntegration::Landed(5),
        );
    }
}
