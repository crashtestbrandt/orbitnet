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

use orbitnet_core::auth::{siphash24, TRAILER_LEN};
use orbitnet_core::codec::{
    decode_input_block_meta, decode_manifest, decode_state_block_meta, encode_manifest,
    input_block_row, skip_input_block_body, skip_state_block_body, FrameHeader, FrameKind,
    Handshake, ManifestEntry, Ping, Pong, Reader, Welcome, Writer, MAGIC, MAX_FRAME_PAYLOAD,
};
use orbitnet_core::interest::{
    ConnectionInterest, InterestCandidate, MembershipId, SeatObserver, SeatScratch,
    MEMBERSHIP_GLOBAL,
};
use orbitnet_core::priority::{self, Band};
use orbitnet_core::slots::SlotTable;
use orbitnet_core::{
    AoiConfig, AuthError, ClockEstimator, CoupledSlew, Direction, LeadTracker, ReceiveBudget,
    ResimPlanner, SessionAuth, TickAccumulator, TickRate, KEY_LEN,
};

use crate::binding;
use crate::sync::{self, OrbitRollbackSynchronizer, OrbitStateSynchronizer, StateIntegration};

/// Network role, mirroring the facade's `Net.Mode`.
const MODE_OFFLINE: i64 = 0;
const MODE_CLIENT: i64 = 1;
const MODE_SERVER: i64 = 2;
const MODE_HOST: i64 = 3;

const SERVER_PEER: i32 = 1;
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
/// The legitimate maximum is `net.input_delay`'s clamp (32) plus the dialled-in lead
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

/// Which seat on a connection a body belongs to, as the game declared it.
///
/// A `u16` because it is a **label** rather than a count: the interest pass holds one set per
/// distinct label present on a connection, so the numbers need not be small or contiguous, and
/// nothing is sized by the value.
pub(crate) type SeatIndex = u16;

/// One owned viewpoint: a connection, and which of its seats.
///
/// **The identity ownership could not express before.** `input_owner_peer()` answers "which
/// connection", and that is the whole answer only while a connection drives one predicted body.
/// Local split-screen drives several — two players on one couch behind one socket — and each needs
/// its own interest anchor, its own centre and its own world, because the second player's
/// surroundings are not the first player's.
///
/// **Seat is the word the demos already use for a player side**, and this is the same idea: a seat
/// is a player position, and what changes is only that a connection may hold more than one of them.
/// A game whose bodies all leave `seat` at `0` has one seat per connection, which is the bijection
/// the demos assume and is unchanged by any of this.
///
/// Ordered peer-major so [`owned_rows_into`]'s sort groups a connection's rows together and its
/// seats in ascending label order within that group — which is what makes both lookups a
/// `partition_point` rather than a per-tick map.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct SeatId {
    /// The connection this seat sits on.
    peer: i32,
    /// Which seat on it. `0` for every body that declares nothing.
    seat: SeatIndex,
}

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
/// A seat with no such row has no entry here at all: it is not distance-culled, and its membership
/// reads as [`MEMBERSHIP_GLOBAL`], so it sees every world. Both halves fail open together, which is
/// the only defensible direction — blanking a seat's world because its body has not spawned yet is
/// not. **That failure is now per seat.** It used to be per connection: one anchored seat supplied
/// the centre for the whole connection, so a second seat had its surroundings culled around a
/// position it was nowhere near.
#[derive(Clone, Copy)]
struct PeerObserver {
    /// The centre this seat's interest radius is measured from.
    center: [f32; 3],
    /// The world this seat is in.
    membership: MembershipId,
}

/// What one peer's filter actually runs against this tick: where it observes from, and its world.
///
/// The whole precedence rule, in one testable place. A declaration ([`PeerAnchor`]) wins on both
/// axes; only [`PeerAnchor::Inferred`] consults the pair read off the body the peer drives:
///
/// | Declaration | Centre | World |
/// | --- | --- | --- |
/// | [`PeerAnchor::Fixed`] | the declared position, always | the declared one |
/// | [`PeerAnchor::Entity`] | where that entity is this tick, else where it last was | the declared one |
/// | [`PeerAnchor::Inferred`] | the inferred body's, if it has one | the inferred body's, else [`MEMBERSHIP_GLOBAL`] |
///
/// **THE TWO AXES FAIL SEPARATELY, AND ONLY FOR A DECLARED PEER.** A tracked entity that has never
/// resolved gives no centre — so nothing is distance-culled, the same open direction an entity with
/// no anchor already takes — but the peer stays in the world it was DECLARED into. A membership is a
/// declaration and did not fail; a centre is a measurement and did. Collapsing them would drop a
/// peer whose avatar has not spawned into every world at once, which is the failure the declaration
/// exists to remove.
///
/// **A DECLARATION IS PER CONNECTION, AND IT COLLAPSES THAT CONNECTION TO ONE SEAT.** Only
/// [`PeerAnchor::Inferred`] is resolved per seat, because only the inferred pair is read off a body
/// and only bodies carry seats. A game that declares a centre for a split-screen connection has
/// stated where that connection observes from, and the backend does not then re-split it — the same
/// precedence that stops a declared centre from falling back to an avatar's. A game that wants a
/// centre per seat declares nothing and lets each seat's body anchor it.
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
/// ([`PeerObserver`]) reads a peer's centre and its world off the lowest-id body that peer's input
/// drives, which answers "what does this peer control" when the question interest management asks is
/// "what does this peer observe". Those are the same answer in a game with one world and one avatar
/// per player, and different answers in every other one: a spectator drives nothing, a commander
/// watches ground its body is not standing on, and a peer with a body in each of two worlds observes
/// exactly one of them.
///
/// Once a game answers the real question for a peer, the inferred pair is never consulted again for
/// that peer. Mixing them would re-centre a peer on its avatar the moment the declared centre was
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
    /// is on the other end and survives one. Only the second can recognise a rejoiner.
    ///
    /// Per CONNECTION, not per seat: a session identity says which player is on the far end of one socket,
    /// and every seat behind that socket belongs to the same player.
    session_id: u64,
    /// Where this peer observes from, declared by the game. See [`PeerAnchor`].
    anchor: PeerAnchor,
    /// The last position [`PeerAnchor::Entity`] resolved to, and the answer once it no longer can.
    ///
    /// **A tracked entity that despawns leaves the peer where it was.** The alternative — dropping
    /// to "no centre", which means "no distance filter" — hands a peer every body in its world at
    /// the exact moment its avatar died. A stale centre is wrong by however far the peer would have
    /// travelled; the open one is wrong by the size of the world.
    ///
    /// It is also what carries a declaration made BEFORE the named entity has a state row: the
    /// declaration survives on this struct and starts resolving the tick that entity does.
    anchor_last: Option<[f32; 3]>,
    /// The world declared alongside [`Self::anchor`]. Read ONLY when a declaration exists, so an
    /// undeclared peer still takes its world from the body it drives.
    ///
    /// It rides the anchor declaration rather than standing alone because the two are one statement
    /// — "this peer is at this point, in this world" — and a centre without the world it is measured
    /// in is precisely the pairing the inferred path takes from one row to keep consistent.
    anchor_membership: MembershipId,
    /// The key and replay window for this connection's datagrams, seated by its handshake.
    ///
    /// `None` until the handshake lands, and that is the gate: [`OrbitNet::open_datagram`] drops
    /// everything from a peer that has none, so a peer that never handshook cannot even draw a pong.
    auth: Option<SessionAuth>,
    /// What this peer has spent of the server's receive path in the current tick.
    budget: ReceiveBudget,
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
    /// Recent snapshot sends awaiting acknowledgement: (frame tick, entity ticks it carried).
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
}

/// The dropped sessions a server is holding, keyed by the identity their handshake carried.
///
/// **What is held is the identity and nothing else.** None of the departed peer's send bookkeeping is
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
    /// Hold `session_id` open until `expires_at_ms`. Identity `0` is refused — that is what a peer claiming
    /// no identity sends, and holding one slot for "everybody anonymous" would hand the next anonymous
    /// joiner the last one's seat.
    ///
    /// Re-holding an id already present overwrites it: the newer drop is the one whose window should run.
    fn hold(&mut self, session_id: u64, peer: i32, expires_at_ms: u64) -> bool {
        if session_id == 0 {
            return false;
        }
        self.held.insert(
            session_id,
            HeldSession {
                peer,
                expires_at_ms,
            },
        );
        true
    }

    /// Take `session_id` out of the table, answering the peer id it was last connected under.
    ///
    /// `None` for an unheld id, which is every first-time joiner and every peer that claimed no identity.
    /// Claiming REMOVES: a session is resumed once, by the connection that arrived first, and a second
    /// claimant with the same token is a newcomer rather than a second resume of one player's place.
    fn claim(&mut self, session_id: u64) -> Option<i32> {
        if session_id == 0 {
            return None;
        }
        self.held.remove(&session_id).map(|held| held.peer)
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

/// How many round-trip samples the per-peer rewind estimate keeps — about a second of
/// them at every rate the loop runs at, which is short enough to follow a real route change and
/// long enough that the minimum below is drawn from a healthy population.
const RTT_WINDOW: usize = 64;

/// The largest single round-trip sample worth storing, in milliseconds. Ten seconds is far past
/// any link a shooter is compensated on — the 250 ms ceiling in `NetLagComp` is what actually
/// bounds the rewind — and the cap only keeps one stalled peer from parking an absurd value in the
/// window. Same shape as the `history_limit` bound on accepted input ticks below: the wire says
/// what it says, and the server decides what it is willing to believe.
const RTT_SAMPLE_MAX_MS: f32 = 10_000.0;

/// What an arriving acknowledgement bought its sender. See [`PeerState::consume_ack`].
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
    fn set_entity_hidden(&mut self, id: u64, hidden: bool) {
        self.interest.set_hidden(id, hidden);
        if hidden {
            self.last_sent.remove(&id);
            self.last_full.remove(&id);
            self.acked_base.remove(&id);
        }
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

    /// Consume one arriving acknowledgement whole: check its proof, raise `newest_ack`, take a
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

    /// Consume an arriving acknowledgement: raise `newest_ack`, and take a round-trip sample IF the
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
    /// honestly. The containment for the remainder is the millisecond ceiling in `NetLagComp`.
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

    /// Interest radius in metres (0 = no **distance** filter).
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
    #[export]
    aoi_radius: f64,

    /// The scale the PRIORITY BANDS are derived from (edges at `scale/3` and `2*scale/3`), in
    /// metres. Independent of [`Self::aoi_radius`] on purpose, and 0 falls back to treating every
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
    /// Client: the key this session's datagrams are authenticated with, and the window that refuses a
    /// replayed one from the server. Minted fresh in [`OrbitNet::start`] and carried in the handshake.
    ///
    /// The server holds no session key of its own — a session's key is the client's, and the server
    /// keeps one [`SessionAuth`] per connected peer on [`PeerState`].
    session_auth: Option<SessionAuth>,
    /// Server: the sessions of dropped peers, held open until their grace window closes.
    resume: ResumeTable,
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
    manifest_dirty: bool,
    /// Client: schema fingerprints announced by the server, checked as entities register.
    expected_schemas: HashMap<u64, (u32, u32)>,
    /// Client: newest snapshot frame tick received (our ack).
    newest_snapshot_tick: u64,
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
    /// The observer slice handed to [`ConnectionInterest::update_linear_into`], rebuilt per peer.
    aoi_seats: Vec<SeatObserver>,
    aoi_seat_scratch: SeatScratch,
    /// This peer's candidate set for the order build: `(id, distance²)`. Filled from the peer's
    /// interest when culling is on and from every row when it is off, so the order loop has one
    /// shape either way. Pooled, so a warm frame allocates nothing.
    aoi_members: Vec<(u64, f32)>,
    aoi_leaves: Vec<u64>,
    order_scratch: Vec<(priority::Candidate, Band)>,

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
            // pre-decoupling behaviour (everything Near) rather than inventing a policy here.
            aoi_band_radius: 0.0,
            aoi_max_entities: 0,
            rate_tiering: false,
            reconnect_grace: 30.0,
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
            session_auth: None,
            resume: ResumeTable::default(),
            live_peers: std::collections::HashSet::new(),
            slots: SlotTable::new(),
            slots_dirty: false,
            slots_exhausted_warned: false,
            manifest_dirty: false,
            expected_schemas: HashMap::new(),
            newest_snapshot_tick: 0,
            snapshot_ack_bits: 0,
            snapshot_ack_token: 0,
            want_full: false,
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
            aoi_seats: Vec::new(),
            aoi_seat_scratch: SeatScratch::default(),
            aoi_members: Vec::new(),
            aoi_leaves: Vec::new(),
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
    /// beat its own keepalive timeout, and it is also what a forged token looks like. Honouring it hands the
    /// new claimant that peer's body, so a game that wants the conservative rule honours it only for a
    /// session it already saw [`Self::peer_dropped`] report as `held`. See `handle_hello`.
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
    /// **This is the release point, and the addon does not act on it.** The entity is still there, still
    /// replicating, still owned by a peer id that no longer exists. What to do about that — free the body,
    /// hand its input back to the server with `set_input_authority(1)`, open the seat to the next joiner —
    /// is the game's decision, exactly as it is for an entity a cull stopped sending.
    #[signal]
    fn peer_session_expired(session_id: i64, peer: i64);

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
        self.lead.clear();
        self.lead_bias_ticks = 0.0;
        self.want_full = false;
        self.ping_timer = 0.0;

        self.auth_warned = false;
        if self.mode == MODE_CLIENT {
            self.synced = false;
            self.running = false;
            // A FRESH key per session, never the previous one. Restarting the sequence numbers under
            // a key an observer already saw would make every datagram captured from the last session
            // replayable into this one.
            self.session_auth = Some(SessionAuth::new(Self::mint_session_key()));
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
        // session mints its own.
        self.session_auth = None;
        self.auth_warned = false;
        // A held session describes a player who can come back to THIS session. There is no session to come
        // back to now, and carrying the table into the next one would resume a stranger.
        self.resume.clear();
        self.live_peers.clear();
        self.planner.clear();
        self.clock.clear();
        self.lead.clear();
        self.lead_bias_ticks = 0.0;
        self.expected_schemas.clear();
        // Slots name entities within ONE session. Carrying the table into the next one would let a
        // stale slot resolve to a stranger, and a server would hand out indices it no longer owns.
        self.slots.clear();
        self.slots_dirty = true;
        self.slots_exhausted_warned = false;
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

    /// One peer's round trip to this server in MILLISECONDS, or `-1.0` when there is no estimate
    /// yet (an unknown peer, or one that has not acknowledged a snapshot frame since it joined).
    ///
    /// SERVER-SIDE ONLY, and a different quantity from the `rtt_ms` in [`Self::metrics`]: that one
    /// is this peer's own ping sampler and reads zero on a server, because `integrate_pong` only
    /// ever runs on a client. This is what the server measured about somebody else, and it is the
    /// input to the per-shooter lag-compensation rewind — `NetLagComp` owns the policy that
    /// turns it into a rewind depth, and the millisecond ceiling that bounds it.
    ///
    /// Derived from state the server already holds; nothing was added to the wire for it.
    #[func]
    fn peer_rtt_ms(&self, peer: i32) -> f64 {
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
    /// handshake goes out; a change afterwards reaches the server only on the next join, which is the right
    /// moment for it anyway.
    ///
    /// The token is opaque and it is never interpreted here: the server compares it for equality against the
    /// sessions it is holding and does nothing else with it. `0` claims no identity, and a peer claiming
    /// none is always seated as a newcomer.
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

    /// Declare where one peer observes from, and which world it observes in.
    ///
    /// SERVER-SIDE ONLY, and the answer to a question the backend cannot infer. Undeclared, a peer
    /// is centred on — and put in the world of — the lowest-id entity its input drives, which
    /// answers what that peer CONTROLS when interest management asks what it OBSERVES. Use this for
    /// a spectator, a strategic camera, an observation post, or any peer whose view is not bolted to
    /// a body it drives. [`Self::set_peer_anchor_entity`] is the same declaration for a centre that
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
    /// tracked centre follows the entity with no per-tick call. The entity NEED NOT be one the peer
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
    /// The peer returns to the inferred pair: centred on the lowest-id body its input drives, in
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
        self.peers
            .entry(peer)
            .or_default()
            .set_entity_hidden(entity_id as u64, hidden);
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
            "interarrival_near" => bw.interarrival_near,
            "interarrival_mid" => bw.interarrival_mid,
            "interarrival_far" => bw.interarrival_far,
            "interarrival_all" => bw.interarrival_all,
            "peers" => bw.peers,
            "interest_entities" => bw.interest_entities,
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
    #[func]
    fn _on_peer_disconnected(&mut self, id: i64) {
        let peer = id as i32;
        let session_id = self.peers.remove(&peer).map_or(0, |state| state.session_id);
        let server = self.mode == MODE_SERVER || self.mode == MODE_HOST;
        let grace_ms = (self.reconnect_grace.max(0.0) * 1000.0) as u64;
        let held = hold_on_drop(session_id, grace_ms, server)
            && self
                .resume
                .hold(session_id, peer, Self::now_ms().saturating_add(grace_ms));
        if server {
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
        // The one datagram sent unauthenticated, because it is what carries the key everything else
        // is authenticated with. `start()` mints it; this covers the transport connecting first.
        let key = self
            .session_auth
            .get_or_insert_with(|| SessionAuth::new(Self::mint_session_key()))
            .key();
        let hello = Handshake::local(self.tickrate.clamp(1, 240) as u16)
            .with_session(self.session_id)
            .with_key(key);
        self.send_raw(SERVER_PEER, &hello.encode(), TransferMode::RELIABLE);
    }

    /// 16 random bytes for this session's key.
    ///
    /// `Crypto` is Godot's platform CSPRNG. `RandomNumberGenerator` is the fallback for a build
    /// without the mbedtls module, and it is **not** cryptographic: an attacker who can predict its
    /// stream can forge this session's datagrams. It is stated here rather than substituted silently,
    /// and the shipped export templates all carry `Crypto`.
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
                    for peer in self.peers.values_mut() {
                        peer.last_sent.remove(&id);
                        peer.last_full.remove(&id);
                        peer.acked_base.remove(&id);
                        peer.interest.remove(id);
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
        // `clock.offset()` settles at MINUS the dialled-in lead -- by design, because a client must run ahead
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

        self.signals().after_rollback_loop().emit();
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

    fn apply_pending_rows(&mut self) {
        for sync in self.rollback_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().apply_pending_display();
        }
        for sync in self.state_entities.values() {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            sync.bind_mut().apply_pending();
        }
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
                sync.bind_mut().record_tick(tick);
            }
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
        // display-offset restore, which belong to neither. Published as they are rather than normalised, so the
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
    fn expire_held_sessions(&mut self) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
        let due = self.resume.expire(Self::now_ms());
        for (session_id, peer) in due {
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
                    "[orbitnet]   input_novel={} resim_spans={} resim_ticks={} fresh={}",
                    self.dbg_input_novel,
                    self.dbg_resim_spans,
                    self.dbg_resim_ticks_total,
                    self.dbg_fresh
                );
                self.dbg_input_novel = 0;
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
        if blocks.is_empty() && !self.want_full {
            return;
        }

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
            flags: if self.want_full {
                FrameHeader::FLAG_WANT_FULL
            } else {
                0
            },
            entity_count: carried.len() as u32,
        };
        header.encode(&mut writer);
        for &index in &carried {
            writer.bytes(&blocks[index]);
        }
        self.want_full = false;
        self.send_to(SERVER_PEER, writer.as_slice(), TransferMode::UNRELIABLE);
    }

    /// SERVER: publish the whole slot table, with each entity's schema fingerprints, to every
    /// synced peer.
    ///
    /// **Both lanes, and a complete snapshot every time.** It carried rollback entities only while
    /// it was purely a schema check — a state-lane entity has no input schema to disagree about —
    /// but it is now also the only channel that says what a wire slot names, and state-lane blocks
    /// carry slots too. Sending the whole table rather than a diff is what makes a receiver's copy
    /// self-repairing: rebuilding from each frame drops every binding that has gone away, with no
    /// removal record to lose.
    ///
    /// **An empty table is sent, not skipped.** A session whose last entity unregistered has to
    /// tell its peers so; returning early there left every client holding bindings for entities
    /// that no longer exist.
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
                let bound = sync.bind();
                entries.push(ManifestEntry {
                    slot,
                    id,
                    state_hash: bound.schema_hash() as u32,
                    input_hash: bound.input_schema_hash() as u32,
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
                });
            }
        }
        let bytes = encode_manifest(&entries);
        let peers: Vec<i32> = self
            .peers
            .iter()
            .filter(|(_, p)| p.synced)
            .map(|(&id, _)| id)
            .collect();
        for peer in peers {
            self.send_to(peer, &bytes, TransferMode::RELIABLE);
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
        // `update_linear_into` clears and refills a `BTreeMap` per peer per tick, and a host that
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
        let filtering = culling || rows.iter().any(|row| row.membership != MEMBERSHIP_GLOBAL);
        if filtering {
            Self::collect_observers(&rows, &mut observers);
            self.update_interest(&peer_ids, &rows, &observers);
        }
        self.acc_interest_us += interest_started.elapsed().as_micros() as u64;
        self.acc_interest_ticks += 1;

        // The cull radius is applied by `update_interest` above, which is the only thing that
        // decides membership; nothing down here re-derives a band from it. See `priority::band_of`:
        // the radius is sized by the longest shot in the game, the band scale by the distances a
        // firefight happens over, and reusing one for the other is what made this scorer inert.
        let band_scale = self.aoi_band_radius as f32;
        let tiering = self.rate_tiering;
        let mut order = std::mem::take(&mut self.order_scratch);
        let mut members = std::mem::take(&mut self.aoi_members);

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
                    // `NEG_INFINITY` so the nearest-N cap can never evict them, then normalised), and
                    // `band_of` reads `0.0` as `Near`. Typically only a handful of channels declare an
                    // anchor — the ones that carry a position — while every other state channel a body owns
                    // (its health, its equipment, its sensors, its lights, the doors around it) does not. Those
                    // would all be scored as though they were in the viewer's face. At four-plus such channels
                    // per body against the ONE anchored row that says where that body is, a distant player's
                    // torch and hit points outbid their position 4:1 under budget pressure. That is remote-body
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

            // --- admit ---
            //
            // THE BUDGET BOUNDS THE BODY, AND THE DATAGRAM IS THE BODY PLUS THE FRAME HEADER. `send_budget`
            // clamps to `MAX_FRAME_PAYLOAD` (1200) and every check below is against `body.len()`, so a full
            // frame leaves here at 1200 plus the header's own bytes -- not at 1200. That is deliberate rather
            // than an oversight, but it is not what the constant's name says: the real wire figure is header +
            // body + 12 (ENet) + 28 (IPv4/UDP), which stays comfortably inside a 1500 B path MTU. Do not read
            // `MAX_FRAME_PAYLOAD` as "the datagram size"; read it as "the entity payload one frame may carry".
            let mut writer = Writer::with_capacity(budget + 128);
            let mut body = Writer::with_capacity(budget);
            let mut sent: Vec<(u64, u64)> = Vec::new();
            // The subset of `sent` that went out full, so the keyframe clock is measured against
            // what repairs a chain. Kept beside `sent` rather than widening it, because `sent` is
            // moved into the ack log verbatim.
            let mut sent_full: Vec<(u64, u64)> = Vec::new();

            for index in 0..order.len() {
                let (candidate, band) = order[index];
                if body.len() >= budget {
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
                if body.len() > budget {
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
            // would bias the figure towards the peers that got served.
            let acc = self.acc_peer_band.entry(peer_id).or_insert((0, 0));
            acc.0 += peer_sends;
            acc.1 += peer_members;

            if sent.is_empty() {
                continue;
            }
            let header = FrameHeader {
                kind: FrameKind::ServerSnapshot,
                tick: u32::try_from(current).unwrap_or(u32::MAX),
                ack_tick,
                ack_bits: 0,
                ack_token,
                margin_ticks: margin,
                flags: 0,
                entity_count: sent.len() as u32,
            };
            header.encode(&mut writer);
            writer.bytes(body.as_slice());
            self.acc_blocks_admitted += sent.len() as u64;
            self.acc_blocks_full += sent_full.len() as u64;
            self.dbg_sent += sent.len() as u64;
            self.dbg_sent_bytes += writer.len() as u64;
            self.send_to(peer_id, writer.as_slice(), TransferMode::UNRELIABLE);

            if let Some(peer) = self.peers.get_mut(&peer_id) {
                peer.want_full = false;
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
            // Inert on the shipped path — the linear filter has no cells — but derived rather than
            // left at the 32 m default so a direct core call, or a future grid adoption, starts
            // from a size proportional to the radius it is querying.
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
    /// peer, a connection driving two bodies got one centre — whichever body sorted lowest — and the
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
    /// centred somewhere iteration order chose. Where each seat drives exactly one rollback body the
    /// rule is unobservable; it is written down because the failure it prevents is a whole
    /// viewpoint's world quietly centring on the wrong thing. The sort below is `sort_by_key`, which
    /// is STABLE, so the ascending-id order the scan collected in survives it.
    ///
    /// **One row supplies both facts.** A row with no resolved anchor is skipped entirely rather
    /// than contributing its membership, so a seat's centre and its world always describe the same
    /// body. Splitting the picks would let a seat be centred on one entity and filtered against
    /// another's world, which is the same class of failure the lowest-id rule exists to prevent. A
    /// seat that contributes no row at all still exists — [`Self::update_interest`] finds it in
    /// [`owned_rows_into`]'s output and gives it an unresolvable centre, which fails open.
    ///
    /// **THE LIMIT THIS INHERITS, AND WHAT IT COSTS FOR MEMBERSHIP.** "Lowest id" is lowest FNV hash
    /// of a node path, so among a seat's several bodies it is arbitrary — deterministic across peers
    /// and runs, which is what matters for the centre, but not chosen. For the centre a change of
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
                    },
                ));
            }
        }
        observers.sort_by_key(|&(seat, _)| seat);
        observers.dedup_by_key(|&mut (seat, _)| seat);
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
    /// Each peer is centred and placed in a world by [`resolve_observer`] — its own declaration when
    /// it made one, the body it drives when it did not — and then filtered on membership first and
    /// distance second, which is [`candidate_for_row`] plus `update_linear_into`.
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
    ///   reshape every row in the list; it is now [`UNLOCATABLE_CENTRE`], which reaches the same
    ///   fail-open through the filter's own non-finite-centre rule.
    ///
    /// The sets this produces are identical — `shared_candidates_match_a_per_peer_rebuild` asserts
    /// it row by row against a reference that rebuilds per peer, over every combination of owned,
    /// unanchored and foreign-world rows.
    ///
    /// **ONE FILTER PASS PER SEAT, ONE SET PER CONNECTION.** A connection may drive several
    /// predicted bodies — local split-screen behind one socket — and each is a viewpoint with its
    /// own centre and its own world. [`ConnectionInterest`] runs the filter once per seat and unions
    /// the results, because relevancy is a property of a viewpoint while the delta base, the ack
    /// window and the byte budget are properties of the datagram. Three consequences, all of them
    /// the reason the union is not simply the widest seat:
    ///
    /// * **A leave is a leave from the UNION.** Clearing `last_sent` when one seat lets go would
    ///   break the delta chain of a body the other seat is still watching.
    /// * **Culling is decided per seat.** A seat whose body has no state row yet gets
    ///   [`UNLOCATABLE_CENTRE`] of its own and stays relevant, instead of inheriting the centre of a
    ///   seat it is nowhere near.
    /// * **A declaration is per connection and collapses it to one seat.** See
    ///   [`resolve_observer`]: a game that stated where a connection observes from is not then
    ///   re-split by seat.
    fn update_interest(
        &mut self,
        peer_ids: &[i32],
        rows: &[EntityRow],
        observers: &[(SeatId, PeerObserver)],
    ) {
        let cfg = self.aoi_config();
        let mut candidates = std::mem::take(&mut self.aoi_candidates);
        let mut owned = std::mem::take(&mut self.aoi_owned_rows);
        let mut seats = std::mem::take(&mut self.aoi_seats);
        let mut scratch = std::mem::take(&mut self.aoi_seat_scratch);
        let mut leaves = std::mem::take(&mut self.aoi_leaves);
        let mut culled = 0u64;

        candidates.clear();
        candidates.extend(rows.iter().map(candidate_for_row));
        owned_rows_into(rows, &mut owned);

        for &peer_id in peer_ids {
            let Some(state) = self.peers.get(&peer_id) else {
                continue;
            };
            let (anchor, last, declared) =
                (state.anchor, state.anchor_last, state.anchor_membership);
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

            // This peer's own rows, in and out around the call. Restored unconditionally below, so
            // no path out of this body can leave the shared list describing the wrong peer. Every
            // seat on the connection gets every one of them as `always`: the datagram is shared, so
            // a body one seat drives rides on it whatever the others can see.
            let mine = owned_rows_of(&owned, peer_id);
            for &(_, index) in mine {
                candidates[index as usize] = candidate_for_own_row(&rows[index as usize]);
            }

            seats.clear();
            if matches!(anchor, PeerAnchor::Inferred) {
                // One seat per distinct label the connection's own rows declare. `mine` is sorted by
                // seat, so the run check is what deduplicates it — several bodies on one seat are
                // one viewpoint, anchored by the lowest-id one of them.
                let seen = Self::observers_of(observers, peer_id);
                let mut previous: Option<SeatIndex> = None;
                for &(seat_id, _) in mine {
                    if previous == Some(seat_id.seat) {
                        continue;
                    }
                    previous = Some(seat_id.seat);
                    let inferred = seen
                        .binary_search_by_key(&seat_id, |&(seat, _)| seat)
                        .ok()
                        .map(|index| seen[index].1);
                    let (resolved, membership) =
                        resolve_observer(anchor, declared, tracked, last, inferred);
                    seats.push(seat_observer(&cfg, resolved, membership));
                }
            }
            // Reached two ways, and they are one statement: this connection observes from ONE place.
            // A declaration says so outright, and an undeclared connection that drives nothing has
            // no seat to read a centre off — the fail-open every peer without a body has always
            // taken. An EMPTY slice is the different claim that there is no viewpoint at all, which
            // the filter reads as an empty set, so neither case may leave it empty.
            if seats.is_empty() {
                let (resolved, membership) =
                    resolve_observer(anchor, declared, tracked, last, None);
                seats.push(seat_observer(&cfg, resolved, membership));
            }

            if let Some(peer) = self.peers.get_mut(&peer_id) {
                // Remember where a tracked entity was, so its despawn leaves the peer here rather
                // than opening its radius to the whole world. Only a resolved position is recorded.
                if let Some(pos) = tracked {
                    peer.anchor_last = Some(pos);
                }
                peer.interest.update_linear_into(
                    &cfg,
                    &seats,
                    &candidates,
                    &mut scratch,
                    &mut leaves,
                );
                for &id in &leaves {
                    peer.last_sent.remove(&id);
                    peer.last_full.remove(&id);
                    peer.acked_base.remove(&id);
                }
                culled += (rows.len() as u64).saturating_sub(peer.interest.len() as u64);
            }
            for &(_, index) in mine {
                candidates[index as usize] = candidate_for_row(&rows[index as usize]);
            }
        }
        self.acc_blocks_culled += culled;

        self.aoi_owned_rows = owned;
        self.aoi_candidates = candidates;
        self.aoi_seats = seats;
        self.aoi_seat_scratch = scratch;
        self.aoi_leaves = leaves;
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
                    if let Ok(entries) = decode_manifest(&mut reader) {
                        // The manifest is a COMPLETE table, so the local copy is rebuilt rather
                        // than merged. That is what retires the binding of an entity the server has
                        // unregistered: a merge would keep naming it, and a slot reissued to a
                        // different entity would then be resolved to the wrong one.
                        self.slots.clear();
                        for entry in entries {
                            self.slots.bind(entry.slot, entry.id);
                            self.expected_schemas
                                .insert(entry.id, (entry.state_hash, entry.input_hash));
                            if let Some(sync) = self.rollback_entities.get(&entry.id) {
                                if sync.is_instance_valid() {
                                    self.check_expected_schema(entry.id, sync);
                                }
                            }
                        }
                    }
                }
            }
        }
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
             or sent before the handshake. Further refusals this session are silent; run with \
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
        if let Err(err) = ours.check_compatibility(&hello) {
            godot_error!("OrbitNet: rejecting peer {sender}: {err}");
            return;
        }
        // **Take the identity off any peer that still claims it.** One player cannot be connected twice
        // under one identity, so an entry that still claims it is a GHOST — a connection the transport has
        // not declared dead yet, which on ENet's defaults takes the better part of a minute. Without this
        // the ghost's disconnect arrives last and holds an identity the returning player is already using,
        // and closing that window releases the seat of somebody who is playing.
        //
        // **NOTHING HERE CHECKS THAT THE SUPERSEDED PEER IS DEAD, AND THE CONSEQUENCE IS A LIVE TAKEOVER,
        // NOT A FORFEITED FUTURE RESUME.** The match is on the token alone. A peer presenting a token that a
        // CONNECTED, PLAYING peer holds is reported through `peer_joined` as an ordinary resume of that
        // peer, and a roster that honours `resumed_from` — including the reference one in the RTS demo —
        // hands the returning claimant that player's body on the spot: the original keeps its connection,
        // receives no error, and simply stops driving its own entity. The superseded connection is not
        // closed; only its claim to the identity is taken.
        //
        // That is accepted rather than overlooked, because the alternative costs the case this exists for.
        // Sourcing a resume only from `ResumeTable::claim` — a drop the server actually observed — refuses
        // a genuinely returning player for as long as the transport takes to notice the old socket is gone,
        // which was measured here at anywhere from 45 s to never. Refusing every real fast reconnect to
        // close a hole that needs 63 guessed bits is the wrong trade. It is a hole, it is stated as one in
        // `README.md`, and a game that wants the conservative rule can have it without a backend change:
        // honour `resumed_from` only for a session it already saw `peer_dropped` report as `held`.
        let mut superseded = 0;
        if hello.session_id != 0 {
            for (&id, state) in self.peers.iter_mut() {
                if id != sender && state.session_id == hello.session_id {
                    state.session_id = 0;
                    superseded = id;
                }
            }
        }
        // Claimed BEFORE the peer entry is touched, and claiming REMOVES: the identity is spent here, so a
        // later connection carrying the same token resumes nothing. A held session wins over a superseded
        // ghost when somehow both exist — the held one is the drop the server actually saw.
        let resumed_from = self.resume.claim(hello.session_id).unwrap_or(superseded);
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
        if peer.auth.is_none_or(|auth| auth.key() != hello.session_key) {
            peer.auth = Some(SessionAuth::new(hello.session_key));
            peer.budget = ReceiveBudget::new();
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
        peer.session_id = hello.session_id;

        let welcome = Welcome {
            protocol_version: orbitnet_core::PROTOCOL_VERSION,
            server_tick: self.accumulator.tick(),
            tickrate: self.effective_rate().hz() as u16,
        };
        self.send_to(sender, &welcome.encode(), TransferMode::RELIABLE);
        self.manifest_dirty = true;
        // Last, and after every field this call sets: the game answers this signal by seating the player,
        // which re-enters this node through the facade.
        if first_hello {
            self.signals().peer_joined().emit(
                i64::from(sender),
                hello.session_id as i64,
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
        let unproven_ack;
        {
            let Some(peer) = self.peers.get_mut(&sender) else {
                return; // No handshake, no input.
            };
            if header.flags & FrameHeader::FLAG_WANT_FULL != 0 {
                peer.want_full = true;
                nacked = true;
            }
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
            for index in 0..meta.count {
                let Some(row) = input_block_row(reader, &meta, wire_stride, index) else {
                    break;
                };
                let Some(tick) = meta.newest_tick.checked_sub(u64::from(index)) else {
                    break;
                };
                if let Some(novel_tick) = bound.integrate_remote_wire_row(tick, row) {
                    earliest_novel = Some(match earliest_novel {
                        Some(existing) => existing.min(novel_tick),
                        None => novel_tick,
                    });
                }
            }
            let id = bound.entity_id();
            drop(bound);
            let _ = skip_input_block_body(reader, &meta);
            if let Some(tick) = earliest_novel {
                self.dbg_input_novel += 1;
                // Resim from the oldest novel row INSIDE the horizon. Rows older than that are
                // already integrated — history is truthful and any later resim replays through
                // them — but they may not start a replay themselves: a joiner's seconds-stale
                // stamps otherwise had the server resimulating its body across whole seconds,
                // and every peer watched the frontier pose flail while it settled.
                let floor = current.saturating_sub(RESIM_INPUT_HORIZON_TICKS);
                let from = tick.max(floor);
                if from < current {
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
///     channel (health, holster, inventory, the env sensor, the torch, every hatch) scored as though it were
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

/// The centre handed to the filter for a peer whose position cannot be established: one whose
/// avatar has not spawned, and every peer when no cull radius is configured.
///
/// [`PeerInterest::update_linear_into`] fails open on a non-finite centre — nothing is culled by
/// distance, while the membership test still runs — which is exactly what both cases mean.
/// Blanking a peer's world because its avatar has not spawned yet is not a defensible failure mode,
/// and a radius of zero asks for no distance culling rather than for all of it.
///
/// **Saying it in the centre is what lets one candidate list serve every peer.** The alternative is
/// a second list shaped for those peers, rebuilt per peer, which is the O(peers × entities) pass
/// this constant exists to delete.
const UNLOCATABLE_CENTRE: [f32; 3] = [f32::NAN; 3];

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
/// here, which is what forced the list to be rebuilt per peer; it is now [`UNLOCATABLE_CENTRE`].
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
/// This is the only row of the tick that differs between peers, and swapping it in and out around
/// each call is what a shared candidate list costs.
#[must_use]
fn candidate_for_own_row(row: &EntityRow) -> InterestCandidate {
    InterestCandidate::always(row.id)
}

/// Which of a client input frame's blocks ride this tick, and where the next walk starts.
///
/// A free function so the rule the send loop runs is the rule a test can call.
/// [`OrbitNet::send_client_input`] carries the reasoning; the mechanics, in order:
///
/// * The walk starts at `rotor` (taken modulo the block count, so a shrinking owned set cannot
///   index past the end) and wraps once, so every block is offered exactly once per tick.
/// * A block is admitted when it fits under `budget`; the walk **continues** past one that does
///   not, so an oversized block cannot starve the ones behind it.
/// * The returned rotor is the FIRST refusal, so next tick offers it first — which is what bounds
///   how long any block waits. With everything admitted the rotor holds still, since a rota with
///   nothing to rotate is one that already sends everything.
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
    for step in 0..lengths.len() {
        let index = (start + step) % lengths.len();
        if payload + lengths[index] <= budget {
            payload += lengths[index];
            out.push(index);
        } else if refused.is_none() {
            refused = Some(index);
        }
    }
    out.sort_unstable();
    refused.unwrap_or(start)
}

/// One seat's observer as the filter takes it: the resolved centre, or [`UNLOCATABLE_CENTRE`] when
/// there is none to measure from.
///
/// **The centre and the world fail separately, and only the centre fails here.** No radius
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
        _ => UNLOCATABLE_CENTRE,
    };
    SeatObserver { center, membership }
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

/// The direction this role SENDS in, and the direction it EXPECTS TO RECEIVE.
///
/// A free function so the one rule that must not be inverted is the rule a test can call. Getting it
/// backwards would authenticate every datagram in the direction it did not travel, and the whole
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

/// Whether a dropped peer's session should be held open for it to come back to.
///
/// - Not a server: only the authority holds sessions.
/// - No grace window configured: resume is switched off.
/// - **No identity**, which covers two different peers. One never sent a token. The other is a GHOST whose
///   token was taken from it by the returning player's handshake — see `handle_hello` — and its late
///   disconnect must not re-open a window on an identity somebody is currently playing under.
#[must_use]
fn hold_on_drop(session_id: u64, grace_ms: u64, server: bool) -> bool {
    server && grace_ms > 0 && session_id != 0
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

#[cfg(test)]
mod tests {
    use super::{
        admit_input_blocks, band_for_row, candidate_for_own_row, candidate_for_row, classify_rx,
        full_block_due, hold_on_drop, owned_rows_into, owned_rows_of, resolve_observer,
        seat_observer, session_directions, AckOutcome, EntityRow, OrbitNet, PeerAnchor,
        PeerObserver, PeerState, ResumeTable, RxOutcome, SeatId, SeatIndex, StateIntegration,
        FULL_STATE_INTERVAL, MODE_CLIENT, MODE_HOST, MODE_OFFLINE, MODE_SERVER, RTT_SAMPLE_MAX_MS,
        RTT_WINDOW, UNLOCATABLE_CENTRE,
    };
    use orbitnet_core::interest::{
        AoiConfig, ConnectionInterest, InterestCandidate, MembershipId, PeerInterest, SeatObserver,
        SeatScratch, MEMBERSHIP_GLOBAL,
    };
    use orbitnet_core::priority::Band;
    use orbitnet_core::KEY_LEN;

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

    /// A peer that cannot be located culls nothing by distance, and the centre is where that is
    /// said — not in the candidate list, which is why the list can be shared.
    ///
    /// The membership half does NOT fail open with it: an unlocatable peer reads as
    /// `MEMBERSHIP_GLOBAL`, which matches every world, but a peer that is merely out of radius keeps
    /// the world it declared.
    #[test]
    fn an_unlocatable_centre_admits_every_row_it_is_offered() {
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
            UNLOCATABLE_CENTRE,
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

        // The same rows from a centre that IS locatable, to prove the list itself culls normally.
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

    /// The centre fails open per SEAT and the world does not fail at all — the pair
    /// `update_interest` hands the filter for one viewpoint.
    #[test]
    fn a_seat_without_a_centre_culls_nothing_by_distance_and_keeps_its_world() {
        let cfg = AoiConfig {
            cell_size: 8.0,
            enter_radius: 100.0,
            exit_factor: 1.25,
            max_entities: 0,
        };
        let unlocated = seat_observer(&cfg, None, 5);
        assert!(unlocated.center[0].is_nan(), "no centre, no distance test");
        assert_eq!(unlocated.membership, 5, "a declared world did not fail");

        // A radius of zero asks for no distance culling rather than for all of it, and it says so
        // in the centre — which is what lets every seat share one candidate list.
        let no_radius = AoiConfig {
            enter_radius: 0.0,
            ..cfg
        };
        assert!(seat_observer(&no_radius, Some([1.0; 3]), 5).center[0].is_nan());

        let located = seat_observer(&cfg, Some([1.0, 2.0, 3.0]), 5);
        assert_eq!(located.center, [1.0, 2.0, 3.0]);
    }

    /// **The failure the per-seat centre removes**, composed the way `update_interest` composes it:
    /// two seats on one connection, one anchored and one whose body has no state row yet.
    ///
    /// Culling used to be decided per connection, so the anchored seat supplied the centre for both
    /// and the unspawned seat had its surroundings culled around a position it was nowhere near.
    /// Per seat, the unanchored one measures nothing and refuses nothing.
    #[test]
    fn an_unanchored_seat_does_not_inherit_the_other_seats_centre() {
        let cfg = AoiConfig {
            cell_size: 8.0,
            enter_radius: 50.0,
            exit_factor: 1.25,
            max_entities: 0,
        };
        let rows = [
            row_seat(1, 42, 0, Some([0.0; 3]), MEMBERSHIP_GLOBAL), // seat 0's body, anchored
            row_seat(2, 42, 1, None, MEMBERSHIP_GLOBAL),           // seat 1's body, not yet spawned
            row(3, 0, Some([900.0, 0.0, 0.0]), MEMBERSHIP_GLOBAL), // far scenery
        ];
        let candidates: Vec<InterestCandidate> = rows.iter().map(candidate_for_row).collect();
        let mut observers = Vec::new();
        OrbitNet::collect_observers(&rows, &mut observers);
        let seen = OrbitNet::observers_of(&observers, 42);

        // Seat 0 resolved; seat 1 has no observer at all and takes the unlocatable centre.
        let seats = [
            seat_observer(&cfg, Some(seen[0].1.center), seen[0].1.membership),
            seat_observer(&cfg, None, MEMBERSHIP_GLOBAL),
        ];
        let mut connection = ConnectionInterest::new();
        let (mut scratch, mut leaves) = (SeatScratch::default(), Vec::new());
        connection.update_linear_into(&cfg, &seats, &candidates, &mut scratch, &mut leaves);
        assert_eq!(
            connection.iter().collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the far row rides on the unlocatable seat"
        );

        // The same connection with both seats anchored at the origin culls the far row — so the
        // admission above is the unlocatable seat's doing, not the candidate list's.
        let both = [seats[0], seats[0]];
        connection.update_linear_into(&cfg, &both, &candidates, &mut scratch, &mut leaves);
        assert_eq!(connection.iter().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(leaves, vec![3]);
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
    /// it, and the rotor parks on it rather than advancing past the blocks it refused.
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

    /// The seat's own world comes from the same row its interest centre does: the LOWEST-id owned row
    /// on that seat that resolved an anchor. Rows arrive sorted by id, and a seat driving more than
    /// one body must not have either answer decided by `HashMap` iteration order.
    #[test]
    fn a_seats_centre_and_world_both_come_from_its_lowest_id_anchored_body() {
        let rows = [
            // Owned but unanchored, and the lowest id: skipped, so it supplies NEITHER fact.
            row(1, 42, None, 77),
            row(2, 42, Some([10.0, 0.0, 0.0]), 5),
            row(3, 42, Some([20.0, 0.0, 0.0]), 6),
            // Another peer's body, and an unowned state-lane row.
            row(4, 43, Some([30.0, 0.0, 0.0]), 8),
            row(5, 0, Some([40.0, 0.0, 0.0]), 9),
        ];
        let mut observers = vec![(
            seat_of(999, 9),
            PeerObserver {
                center: [7.0; 3],
                membership: 1,
            },
        )];
        OrbitNet::collect_observers(&rows, &mut observers);

        let peer = OrbitNet::observers_of(&observers, 42)[0].1;
        assert_eq!(peer.center, [10.0, 0.0, 0.0]);
        assert_eq!(
            peer.membership, 5,
            "the world comes from the row that supplied the centre, not from a lower unanchored one"
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
    /// two centres and two worlds. Keyed by connection, the lower entity id won and the other
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
    /// other seat on the same connection keeps its centre.
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
        let (mut scratch, mut leaves) = (SeatScratch::default(), Vec::new());
        peer.interest.update_linear_into(
            &AoiConfig::default(),
            &[SeatObserver {
                center: [0.0; 3],
                membership: MEMBERSHIP_GLOBAL,
            }],
            &[InterestCandidate::anchored(id, [1.0, 0.0, 0.0])],
            &mut scratch,
            &mut leaves,
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
    // A declared observer, and what it overrides.
    // ------------------------------------------------------------------

    const HERE: [f32; 3] = [1.0, 2.0, 3.0];
    const THERE: [f32; 3] = [900.0, 0.0, -900.0];

    fn body_in(center: [f32; 3], membership: MembershipId) -> Option<PeerObserver> {
        Some(PeerObserver { center, membership })
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
        // Driving nothing: no centre, and every world. Both halves fail open together.
        assert_eq!(
            resolve_observer(PeerAnchor::Inferred, 5, None, None, None),
            (None, MEMBERSHIP_GLOBAL)
        );
    }

    /// THE POINT OF THE DECLARATION. A peer observing one world while driving a body in another must
    /// be centred where it is LOOKING and filtered in the world it is WATCHING -- the body it drives
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
    /// body would move the peer into whichever world that body is in, and falling back to "no centre"
    /// would open its radius to the whole world at the moment its avatar died.
    #[test]
    fn a_tracked_centre_survives_the_entity_it_tracks() {
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
    /// spawned -- gives no centre, so nothing is distance-culled. The peer nonetheless stays in the
    /// world it was DECLARED into: a membership is a declaration and did not fail.
    #[test]
    fn a_tracked_centre_that_never_resolved_keeps_its_declared_world() {
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
    /// an anchor rather than on that stored zero -- otherwise a distant player's torch and hit points
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
        // The security property: a peer inflates samples by withholding acknowledgements and can
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

    #[test]
    fn a_rejoiner_claims_the_session_it_dropped_and_learns_its_old_peer_id() {
        let mut table = ResumeTable::default();
        assert!(table.hold(0xabcd, 7, 1_000));
        assert!(table.holds(0xabcd));
        assert_eq!(table.claim(0xabcd), Some(7));
        assert!(!table.holds(0xabcd), "claiming spends the session");
    }

    /// A peer that claimed no identity has nothing to resume, and `0` must not become a slot every
    /// anonymous joiner in turn inherits.
    #[test]
    fn identity_zero_is_never_held_and_never_claimed() {
        let mut table = ResumeTable::default();
        assert!(!table.hold(0, 7, 1_000));
        assert!(!table.holds(0));
        assert_eq!(table.claim(0), None);
    }

    /// Resuming is once. A second connection carrying a token the first already spent is a newcomer, or
    /// two live peers would be seated on one entity.
    #[test]
    fn a_second_claimant_of_one_token_is_a_newcomer() {
        let mut table = ResumeTable::default();
        table.hold(9, 3, 1_000);
        assert_eq!(table.claim(9), Some(3));
        assert_eq!(table.claim(9), None);
    }

    #[test]
    fn an_unheld_token_is_a_newcomer() {
        let mut table = ResumeTable::default();
        table.hold(1, 4, 1_000);
        assert_eq!(table.claim(2), None);
        assert!(table.holds(1), "and the held session is untouched");
    }

    /// The window is inclusive at its deadline, and a session past it is gone from the table as well as
    /// reported — a release the game hears about twice would open the seat twice.
    #[test]
    fn a_session_expires_at_its_deadline_and_is_reported_once() {
        let mut table = ResumeTable::default();
        table.hold(5, 2, 1_000);
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
        table.hold(30, 3, 100);
        table.hold(10, 1, 100);
        table.hold(20, 2, 100);
        assert_eq!(table.expire(100), vec![(10, 1), (20, 2), (30, 3)]);
    }

    /// A player who drops, rejoins, and drops again gets a window measured from the SECOND drop.
    #[test]
    fn re_holding_a_session_restarts_its_window() {
        let mut table = ResumeTable::default();
        table.hold(8, 2, 1_000);
        table.hold(8, 5, 4_000);
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
        // Also the GHOST case: a stale connection whose token was taken by the returning player's
        // handshake carries identity 0 by the time its disconnect lands, so it re-opens no window.
        assert!(!hold_on_drop(0, 30_000, true), "no identity");
    }

    #[test]
    fn teardown_forgets_every_held_session() {
        let mut table = ResumeTable::default();
        table.hold(1, 1, 1_000);
        table.hold(2, 2, 1_000);
        table.clear();
        assert!(table.expire(u64::MAX).is_empty());
        assert!(!table.holds(1));
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
}
