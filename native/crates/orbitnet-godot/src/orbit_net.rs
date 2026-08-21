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
use godot::classes::{Engine, MultiplayerApi, Node, SceneMultiplayer, Time};
use godot::prelude::*;

use orbitnet_core::codec::{
    decode_input_block_meta, decode_manifest, decode_state_block_meta, encode_manifest,
    input_block_row, skip_input_block_body, skip_state_block_body, FrameHeader, FrameKind,
    Handshake, ManifestEntry, Ping, Pong, Reader, Welcome, Writer, MAGIC, MAX_FRAME_PAYLOAD,
};
use orbitnet_core::interest::{InterestCandidate, MembershipId, MEMBERSHIP_GLOBAL};
use orbitnet_core::priority::{self, Band};
use orbitnet_core::{
    AoiConfig, ClockEstimator, CoupledSlew, LeadTracker, PeerInterest, ResimPlanner,
    TickAccumulator, TickRate,
};

use crate::sync::{OrbitRollbackSynchronizer, OrbitStateSynchronizer, StateIntegration};

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

/// What one peer's interest is measured against: where it observes from, and which world it is in.
///
/// Both facts come from the **same row** — the lowest-id entity whose input authority is that peer
/// and which resolved an anchor. A peer's membership has no home of its own on the wire or in the
/// registry, and taking it from the body that already anchors the peer's radius keeps the two
/// answers about one entity rather than about two that could disagree.
///
/// A peer with no such row has no entry here at all: it is not distance-culled, and its membership
/// reads as [`MEMBERSHIP_GLOBAL`], so it sees every world. Both halves fail open together, which is
/// the only defensible direction — blanking a peer's world because its avatar has not spawned yet
/// is not.
#[derive(Clone, Copy)]
struct PeerObserver {
    /// The centre the peer's interest radius is measured from.
    center: [f32; 3],
    /// The world the peer is in.
    membership: MembershipId,
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

#[derive(Default)]
struct PeerState {
    /// Whether the handshake completed (server side: Hello received and answered).
    synced: bool,
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
    /// Entities — **both lanes** — inside this peer's interest, with the squared
    /// distance the last update measured, which the priority scorer reads back as a band.
    interest: PeerInterest,
    /// Recent snapshot sends awaiting acknowledgement: (frame tick, entity ticks it carried).
    sent_log: std::collections::VecDeque<(u64, Vec<(u64, u64)>)>,
    /// Per-entity newest tick this peer CONFIRMED receiving (via ack_tick/ack_bits) — the only
    /// tick a masked delta may reference: the peer provably holds that base row.
    acked_base: HashMap<u64, u64>,
    /// Highest ack tick seen from this peer, for expiring the sent log.
    newest_ack: u64,
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
    /// **What this does NOT do**, because the claim has been overstated twice already: nothing here
    /// ties the reported ack to the highest frame the peer actually received, and any advance is
    /// accepted however small. A client that advances at full rate while holding a constant lag is
    /// measured at that lag and reads exactly like a genuinely slow peer — see the residual tests
    /// below. There is no server-side figure to cross-check it against (the ping/pong clock is
    /// client-initiated and `integrate_pong` runs only on clients), and pinning it would need a
    /// server-chosen value the client must echo, which is a wire change this rules out. The
    /// containment is the millisecond ceiling in `NetLagComp`, and it is adequate for the reason
    /// every rewind system relies on: a client that under-reports gains nothing a client routing
    /// through a traffic shaper does not already gain honestly, and the two are indistinguishable.
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

    /// Interest radius in metres (0 = off: every peer receives everything).
    ///
    /// The 100-player lever: with a radius set, each peer receives only the entities within it of
    /// that peer's own body, with a 1.25x exit hysteresis so boundary entities don't flicker.
    /// This covers the **state lane** too, but only for channels that declare
    /// `relevancy = ANCHORED` and a resolvable `anchor_property`; everything else stays
    /// unconditionally relevant, which is what every state channel was before.
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
    manifest_dirty: bool,
    /// Client: schema fingerprints announced by the server, checked as entities register.
    expected_schemas: HashMap<u64, (u32, u32)>,
    /// Client: newest snapshot frame tick received (our ack).
    newest_snapshot_tick: u64,
    /// Client: which of the 32 frame ticks before `newest_snapshot_tick` also arrived — rides
    /// every input header so the server deltas only against bases we provably hold.
    snapshot_ack_bits: u32,
    /// Client: raise WANT_FULL on the next input frame.
    want_full: bool,

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
    acc_stale_blocks: u64,
    acc_interest_us: u64,
    acc_interest_ticks: u64,
    acc_interest_peer_ticks: u64,
    acc_interest_members: u64,
    acc_band_sends: [u64; 3],
    acc_band_members: [u64; 3],
    win_peer_bytes: HashMap<i32, u64>,
    win_starve_ticks_max: u64,
    win_unsent_backlog_max: u64,

    // --- send-path allocation pools, reused every tick so a warm frame allocates nothing ---
    aoi_rows: Vec<EntityRow>,
    aoi_observers: HashMap<i32, PeerObserver>,
    aoi_candidates: Vec<InterestCandidate>,
    aoi_dist_scratch: Vec<(u64, f32)>,
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
            manifest_dirty: false,
            expected_schemas: HashMap::new(),
            newest_snapshot_tick: 0,
            snapshot_ack_bits: 0,
            want_full: false,
            m_resim_ticks: 0.0,
            m_rollback_ms: 0.0,
            m_restore_ms: 0.0,
            m_sim_ms: 0.0,
            m_record_ms: 0.0,
            m_net_ms: 0.0,
            m_rb_nodes: 0.0,
            m_bw: BandwidthMetrics::default(),
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
            acc_stale_blocks: 0,
            acc_interest_us: 0,
            acc_interest_ticks: 0,
            acc_interest_peer_ticks: 0,
            acc_interest_members: 0,
            acc_band_sends: [0; 3],
            acc_band_members: [0; 3],
            win_peer_bytes: HashMap::new(),
            win_starve_ticks_max: 0,
            win_unsent_backlog_max: 0,
            aoi_rows: Vec::new(),
            aoi_observers: HashMap::new(),
            aoi_candidates: Vec::new(),
            aoi_dist_scratch: Vec::new(),
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
        self.lead.clear();
        self.lead_bias_ticks = 0.0;
        self.want_full = false;
        self.ping_timer = 0.0;

        if self.mode == MODE_CLIENT {
            self.synced = false;
            self.running = false;
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
        self.planner.clear();
        self.clock.clear();
        self.lead.clear();
        self.lead_bias_ticks = 0.0;
        self.expected_schemas.clear();
        self.stretch_now = 1.0;
        // The window describes a session that has ended; carrying its rates into the next one would make the
        // first second of every session read as the last second of the previous.
        self.m_bw = BandwidthMetrics::default();
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

    /// Send-path accounting, windowed to per-second figures once a second.
    ///
    /// Deliberately a **separate** dictionary from [`Self::metrics`]: `bench_metrics.gd` and the
    /// perf probe read that one's exact shape, and widening a dictionary two harnesses index into
    /// is how a measurement change becomes a gate failure. Byte figures are OrbitNet **payload**;
    /// `tx_wire_bytes_s` is the same traffic with [`WIRE_OVERHEAD_BYTES`] per datagram added, and
    /// `tx_datagrams_s` is published so the sum can be checked rather than trusted.
    /// Just the near-band inter-arrival, without building [`Self::bandwidth_metrics`]'s dictionary.
    ///
    /// This one figure is read **every net tick on the authority** (it is the interpolation term
    /// in every shot's rewind depth, refreshed once per tick rather than once per shot). Going through
    /// the full dictionary to get it allocated a nineteen-key `VarDictionary` and boxed every value,
    /// per tick, forever — on the hot path of the loop this epic exists to make cheaper. Everything
    /// else in that dictionary is read by a probe or a HUD at human rates and can keep paying for it.
    #[func]
    fn interarrival_all(&self) -> f64 {
        self.m_bw.interarrival_all
    }

    #[func]
    fn interarrival_near(&self) -> f64 {
        self.m_bw.interarrival_near
    }

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
    /// created by the caller (`CrashLogger`, which already owns `user://logs`), so nothing in the
    /// signal path has to touch Godot. Idempotent; returns false if already installed.
    #[func]
    fn install_crash_handler(&self, dir: GString) -> bool {
        crate::crash::install(&dir.to_string())
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

    #[func]
    fn _on_peer_disconnected(&mut self, id: i64) {
        self.peers.remove(&(id as i32));
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

    /// Hand one datagram to the transport, and account for it.
    ///
    /// Every byte OrbitNet puts on the wire goes through here — snapshots, input frames, manifests,
    /// pings and the handshake alike — which is what makes `tx_bytes_s` a number about the session
    /// rather than about the snapshot loop.
    fn send_to(&mut self, peer: i32, bytes: &[u8], mode: TransferMode) {
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
        let hello = Handshake::local(0, self.tickrate.clamp(1, 240) as u16);
        self.send_to(SERVER_PEER, &hello.encode(), TransferMode::RELIABLE);
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
                }
            }
        }
        // Registry hygiene: drop any freed instances (a freed node cannot unregister itself if
        // it was freed without exiting the tree cleanly).
        self.rollback_entities.retain(|_, s| s.is_instance_valid());
        self.state_entities.retain(|_, s| s.is_instance_valid());
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

    fn capture_inputs(&mut self, tick: u64) {
        let delay = self.input_delay.max(0) as u64;
        let stamp = tick + delay;
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

    fn mark_forward_ticks(&mut self, tick: u64) {
        for (&id, sync) in &self.rollback_entities {
            let Some(mut sync) = live_handle(sync) else {
                continue;
            };
            let mut bound = sync.bind_mut();
            bound.mark_inputless_authoritative(tick);
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
        for entry in &plan {
            if let Some(sync) = self
                .rollback_entities
                .get(&entry.body)
                .and_then(live_handle)
            {
                ranges.push((entry.body, entry.range.from, entry.range.to, sync));
            }
        }

        let mut rb_nodes = 0u64;
        let mut call_batch: Vec<(Gd<Node>, bool)> = Vec::new();
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

            // Phase 1 — restore state + input for every entity replaying this tick.
            let phase_started = Instant::now();
            for (_, range_from, range_to, sync) in &ranges {
                if tick < *range_from || tick >= *range_to {
                    continue;
                }
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                sync.bind_mut().restore_tick(tick);
            }

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

            // Phase 3 — record the resulting state as tick + 1.
            let phase_started = Instant::now();
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
                sync.bind_mut().restore_tick(display_tick);
            }
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

    fn capture_state_lane(&mut self, tick: u64) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
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

    fn run_net_upkeep(&mut self, delta: f64) {
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
        self.acc_stale_blocks = 0;
        self.acc_interest_us = 0;
        self.acc_interest_ticks = 0;
        self.acc_interest_peer_ticks = 0;
        self.acc_interest_members = 0;
        self.acc_band_sends = [0; 3];
        self.acc_band_members = [0; 3];
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
                self.send_manifest_if_dirty();
                self.send_snapshots(current);
            }
            _ => {}
        }
        self.m_net_ms = started.elapsed().as_secs_f64() * 1000.0;
    }

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
            if let Some(bytes) = bound.encode_input_block_bytes(current, INPUT_REDUNDANCY) {
                blocks.push(bytes);
            }
        }
        if blocks.is_empty() && !self.want_full {
            return;
        }

        let mut writer = Writer::with_capacity(256);
        let header = FrameHeader {
            kind: FrameKind::ClientInput,
            tick: u32::try_from(current).unwrap_or(u32::MAX),
            ack_tick: u32::try_from(self.newest_snapshot_tick).unwrap_or(u32::MAX),
            ack_bits: self.snapshot_ack_bits,
            margin_ticks: 0,
            flags: if self.want_full {
                FrameHeader::FLAG_WANT_FULL
            } else {
                0
            },
            entity_count: blocks.len() as u32,
        };
        header.encode(&mut writer);
        for block in &blocks {
            writer.bytes(block);
        }
        self.want_full = false;
        self.send_to(SERVER_PEER, writer.as_slice(), TransferMode::UNRELIABLE);
    }

    fn send_manifest_if_dirty(&mut self) {
        if !self.manifest_dirty {
            return;
        }
        self.manifest_dirty = false;
        let mut entries: Vec<ManifestEntry> = Vec::new();
        for (&id, sync) in &self.rollback_entities {
            if !sync.is_instance_valid() {
                continue;
            }
            let bound = sync.bind();
            entries.push(ManifestEntry {
                id,
                state_hash: bound.schema_hash() as u32,
                input_hash: bound.input_schema_hash() as u32,
            });
        }
        if entries.is_empty() {
            return;
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
        self.collect_entity_rows(&mut rows, &mut observers);
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
            let (want_full, ack_tick, margin) = {
                let Some(peer) = self.peers.get(&peer_id) else {
                    continue; // disconnected while an earlier peer's frame was going out
                };
                (
                    peer.want_full,
                    u32::try_from(peer.newest_input_tick.max(0)).unwrap_or(u32::MAX),
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
                // Rate tiering is a deliberate hold-back, so it counts as culled, not deferred.
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
                }
            }

            if sent.is_empty() {
                continue;
            }
            let header = FrameHeader {
                kind: FrameKind::ServerSnapshot,
                tick: u32::try_from(current).unwrap_or(u32::MAX),
                ack_tick,
                ack_bits: 0,
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
    /// O(peers × entities) cost this pass exists to delete. `observers` is cleared here and filled
    /// by [`Self::collect_observers`] once the rows are sorted.
    fn collect_entity_rows(
        &self,
        rows: &mut Vec<EntityRow>,
        observers: &mut HashMap<i32, PeerObserver>,
    ) {
        rows.clear();
        observers.clear();
        for (&id, sync) in &self.rollback_entities {
            let Some(sync) = live_handle(sync) else {
                continue;
            };
            let bound = sync.bind();
            let owner = bound.input_owner_hint();
            let anchor = bound.position_hint();
            let membership = bound.membership_hint();
            let priority = bound.send_priority();
            drop(bound);
            rows.push(EntityRow {
                id,
                owner,
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
                anchor,
                membership,
                priority,
            });
        }
    }

    /// Where each peer observes from and which world it is in: both read off the entity that peer
    /// drives.
    ///
    /// **Called on rows already sorted by id, and it keeps the LOWEST id per owner.** `rows` is
    /// gathered by walking a `HashMap`, so a last-writer-wins insert would pick a different entity
    /// on different runs — and a peer that drives more than one rollback entity would have its
    /// interest centred somewhere iteration order chose. In a game where each peer drives exactly
    /// one rollback body the rule is unobservable; it is written down because the failure it
    /// prevents is a whole peer's world quietly centring on the wrong thing.
    ///
    /// **One row supplies both facts.** A row with no resolved anchor is skipped entirely rather
    /// than contributing its membership, so a peer's centre and its world always describe the same
    /// body. Splitting the picks would let a peer be centred on one entity and filtered against
    /// another's world, which is the same class of failure the lowest-id rule exists to prevent.
    fn collect_observers(rows: &[EntityRow], observers: &mut HashMap<i32, PeerObserver>) {
        observers.clear();
        for row in rows {
            if row.owner <= 0 {
                continue;
            }
            if let Some(center) = row.anchor {
                observers.entry(row.owner).or_insert(PeerObserver {
                    center,
                    membership: row.membership,
                });
            }
        }
    }

    /// Recompute every peer's interest set, and clear the delta bookkeeping of what left.
    ///
    /// The leave half is the correctness requirement here. Re-entry is already *safe* — a
    /// delta against a base the peer dropped is rejected and raises `WANT_FULL` — but `want_full` is
    /// a per-peer, **all-entity** flag, so one re-entering body would cost a round trip plus a
    /// full-state burst for every entity that peer holds, arriving exactly when a fight starts.
    /// Clearing `last_sent` and `acked_base` at the leave instead (the same pair the unregister path
    /// clears, for the same reason) forces a full block for that entity alone, and sorts it to the
    /// front of the rota while it is at it.
    fn update_interest(
        &mut self,
        peer_ids: &[i32],
        rows: &[EntityRow],
        observers: &HashMap<i32, PeerObserver>,
    ) {
        let cfg = self.aoi_config();
        let mut candidates = std::mem::take(&mut self.aoi_candidates);
        let mut scratch = std::mem::take(&mut self.aoi_dist_scratch);
        let mut leaves = std::mem::take(&mut self.aoi_leaves);
        let mut culled = 0u64;

        for &peer_id in peer_ids {
            let observer = observers.get(&peer_id).copied();
            // No radius, or no body to measure from: everything stays relevant *on distance*.
            // Blanking a peer's world because its avatar has not spawned yet is not a defensible
            // failure mode. Membership is a separate axis and is not switched off here — an
            // observer with no body reads as MEMBERSHIP_GLOBAL below, which matches every world, so
            // that case fails open too.
            let culling = cfg.enter_radius > 0.0 && observer.is_some();
            let observer_membership = observer.map_or(MEMBERSHIP_GLOBAL, |o| o.membership);
            candidates.clear();
            candidates.extend(
                rows.iter()
                    .map(|row| candidate_for_row(row, peer_id, culling)),
            );
            let Some(peer) = self.peers.get_mut(&peer_id) else {
                continue;
            };
            peer.interest.update_linear_into(
                &cfg,
                observer.map_or([0.0; 3], |o| o.center),
                observer_membership,
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
        self.acc_blocks_culled += culled;

        self.aoi_candidates = candidates;
        self.aoi_dist_scratch = scratch;
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
        let mut reader = Reader::new(bytes);
        let Ok(kind) = FrameKind::from_tag(bytes[0]) else {
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
                        for entry in entries {
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

    fn handle_hello(&mut self, sender: i32, bytes: &[u8]) {
        if self.mode != MODE_SERVER && self.mode != MODE_HOST {
            return;
        }
        let Ok(hello) = Handshake::decode(bytes) else {
            return;
        };
        let ours = Handshake::local(0, self.effective_rate().hz() as u16);
        if let Err(err) = ours.check_compatibility(&hello) {
            godot_error!("OrbitNet: rejecting peer {sender}: {err}");
            return;
        }
        let peer = self.peers.entry(sender).or_default();
        peer.synced = true;
        peer.want_full = true;
        peer.newest_input_tick = -1;

        let welcome = Welcome {
            protocol_version: orbitnet_core::PROTOCOL_VERSION,
            server_tick: self.accumulator.tick(),
            tickrate: self.effective_rate().hz() as u16,
        };
        self.send_to(sender, &welcome.encode(), TransferMode::RELIABLE);
        self.manifest_dirty = true;
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
        {
            let Some(peer) = self.peers.get_mut(&sender) else {
                return; // No handshake, no input.
            };
            if header.flags & FrameHeader::FLAG_WANT_FULL != 0 {
                peer.want_full = true;
                nacked = true;
            }
            // Consume the ack window: every snapshot frame the client confirms receiving
            // promotes the entity ticks it carried to `acked_base` — the only ticks a masked
            // delta may reference, because the client provably holds those rows.
            let ack = u64::from(header.ack_tick);
            if ack > 0 {
                // Raise `newest_ack` and measure the round trip, but ONLY when the ack has
                // actually advanced -- see `note_ack` for why an unadvanced one must not be
                // measured. `note_ack` uses `saturating_sub` because an ack can name a frame the
                // accumulator has not reached: ticks are published before the send phase runs, and
                // a peer's clock leads.
                peer.note_ack(ack, current, tick_ms);
                let newest_ack = peer.newest_ack;
                let mut promoted: Vec<(u64, u64)> = Vec::new();
                peer.sent_log.retain(|(frame, entities)| {
                    let confirmed = *frame == ack
                        || (*frame < ack
                            && ack - *frame <= 32
                            && (header.ack_bits >> (ack - *frame - 1)) & 1 == 1);
                    if confirmed {
                        promoted.extend_from_slice(entities);
                        return false;
                    }
                    // Older than the ack window can reach: it will never be confirmed.
                    frame.saturating_add(32) >= newest_ack
                });
                for (id, tick) in promoted {
                    let entry = peer.acked_base.entry(id).or_insert(0);
                    *entry = (*entry).max(tick);
                }
            }
        }
        // The acceptance bar for turning AOI on: a re-entering entity must get its full block
        // WITHOUT a want_full storm, and this is the number that says whether it did.
        if nacked {
            self.acc_want_full_nacks += 1;
        }

        for _ in 0..header.entity_count {
            let Ok(meta) = decode_input_block_meta(reader, u64::from(header.tick)) else {
                return;
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
            // server freed the body. Resolve the handle through live_handle so the corpse's stale
            // registry entry is skipped rather than cloned (which panics) — this is the exact line
            // the shipped crash logs pointed at.
            let Some(mut sync) = self.rollback_entities.get(&meta.id).and_then(live_handle) else {
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
            if let Some(mut sync) = self.rollback_entities.get(&meta.id).and_then(live_handle) {
                if !meta.state_lane {
                    let result = {
                        let mut bound = sync.bind_mut();
                        bound.apply_state_block(reader, &meta, &mut self.mask_scratch, current)
                    };
                    match result {
                        Ok(StateIntegration::Mispredict(tick)) => {
                            self.dbg_rx_applied += 1;
                            self.planner.mark(meta.id, tick);
                        }
                        Ok(outcome) => self.note_integration(outcome, meta.full),
                        Err(_) => return,
                    }
                    continue;
                }
            }
            if let Some(mut sync) = self.state_entities.get(&meta.id).and_then(live_handle) {
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
                    "[orbitnet] rx skip unknown entity {:#018x} lane={} tick={}",
                    meta.id,
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

/// How one gathered row is offered to one peer's interest filter. A free function so the rule the
/// send loop runs is the rule a test can call.
///
/// Three cases, in the order they are decided:
///
/// 1. **The peer's own body** — `always` in every world. Never culled by anything, and deliberately
///    not membership-tested: the peer's membership was read off this very row, so the test could
///    only restate a tautology, or, for a peer that drives bodies in two worlds, cull that peer's
///    own avatar out of its own view.
/// 2. **A row with a resolved anchor, with culling on** — distance-culled from that anchor, within
///    the world the row declares.
/// 3. **Everything else** — a row that declares no anchor, one whose anchor did not resolve, and
///    every row when culling is off: `always` **within the world the row declares**. The distance
///    half fails open because a missing anchor is a measurement that failed; the membership half
///    does not, because a membership is a declaration and did not.
///
/// Case 3 is the one the feature exists for. It is where a positionless state channel — health,
/// inventory, a door's state — lands, and before a membership existed it had exactly one setting:
/// every peer in every world.
#[must_use]
fn candidate_for_row(row: &EntityRow, peer_id: i32, culling: bool) -> InterestCandidate {
    if row.owner == peer_id {
        return InterestCandidate::always(row.id);
    }
    match (culling, row.anchor) {
        (true, Some(pos)) => InterestCandidate::anchored_in(row.id, pos, row.membership),
        _ => InterestCandidate::always_in(row.id, row.membership),
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

#[cfg(test)]
mod tests {
    use super::{
        band_for_row, candidate_for_row, classify_rx, full_block_due, EntityRow, OrbitNet,
        PeerObserver, PeerState, RxOutcome, StateIntegration, FULL_STATE_INTERVAL,
        RTT_SAMPLE_MAX_MS, RTT_WINDOW,
    };
    use orbitnet_core::interest::{InterestCandidate, MembershipId, MEMBERSHIP_GLOBAL};
    use orbitnet_core::priority::Band;
    use std::collections::HashMap;

    // ------------------------------------------------------------------
    // Membership: how a gathered row reaches the filter, and where a peer's own world comes from.
    // ------------------------------------------------------------------

    fn row(id: u64, owner: i32, anchor: Option<[f32; 3]>, membership: MembershipId) -> EntityRow {
        EntityRow {
            id,
            owner,
            anchor,
            membership,
            priority: 1,
        }
    }

    /// The case the feature exists for. A channel with no anchor has no distance to be culled by, so
    /// it goes in as `always` — but `always` must now carry the row's world, or the channel keeps its
    /// one pre-membership setting of "every peer in every world".
    #[test]
    fn a_row_with_no_anchor_is_always_relevant_within_its_own_world() {
        let candidate = candidate_for_row(&row(7, 0, None, 3), 42, true);
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
        for culling in [true, false] {
            let candidate = candidate_for_row(&row(7, 0, None, 9), 42, culling);
            assert!(candidate.always);
            assert_eq!(candidate.membership, 9);
        }
        // ...and with culling off, a row that DOES have an anchor takes the same treatment: no
        // distance to test against, world still enforced.
        let candidate = candidate_for_row(&row(7, 0, Some([1.0, 2.0, 3.0]), 9), 42, false);
        assert_eq!(candidate, InterestCandidate::always_in(7, 9));
    }

    /// A peer's own body is never culled by anything, membership included — the peer's world was read
    /// off this very row, and a peer driving bodies in two worlds must not lose its own avatar.
    #[test]
    fn a_peers_own_body_is_always_relevant_in_every_world() {
        for membership in [MEMBERSHIP_GLOBAL, 1, MembershipId::MAX] {
            let candidate =
                candidate_for_row(&row(7, 42, Some([500.0, 0.0, 0.0]), membership), 42, true);
            assert_eq!(
                candidate,
                InterestCandidate::always(7),
                "the peer's own body goes in global, whatever world the row declares"
            );
        }
    }

    /// An anchored row under culling carries both axes: the position for the radius, the declared
    /// world for the membership test.
    #[test]
    fn an_anchored_row_carries_its_position_and_its_world() {
        let candidate = candidate_for_row(&row(7, 5, Some([1.0, 2.0, 3.0]), 4), 42, true);
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
            candidate_for_row(
                &row(7, 5, Some([1.0, 2.0, 3.0]), MEMBERSHIP_GLOBAL),
                42,
                true
            ),
            InterestCandidate::anchored(7, [1.0, 2.0, 3.0])
        );
        assert_eq!(
            candidate_for_row(&row(7, 5, None, MEMBERSHIP_GLOBAL), 42, true),
            InterestCandidate::always(7)
        );
    }

    /// The peer's own world comes from the same row its interest centre does: the LOWEST-id owned row
    /// that resolved an anchor. Rows arrive sorted by id, and a peer driving more than one body must
    /// not have either answer decided by `HashMap` iteration order.
    #[test]
    fn a_peers_centre_and_world_both_come_from_its_lowest_id_anchored_body() {
        let rows = [
            // Owned but unanchored, and the lowest id: skipped, so it supplies NEITHER fact.
            row(1, 42, None, 77),
            row(2, 42, Some([10.0, 0.0, 0.0]), 5),
            row(3, 42, Some([20.0, 0.0, 0.0]), 6),
            // Another peer's body, and an unowned state-lane row.
            row(4, 43, Some([30.0, 0.0, 0.0]), 8),
            row(5, 0, Some([40.0, 0.0, 0.0]), 9),
        ];
        let mut observers = HashMap::new();
        OrbitNet::collect_observers(&rows, &mut observers);

        let peer = observers[&42];
        assert_eq!(peer.center, [10.0, 0.0, 0.0]);
        assert_eq!(
            peer.membership, 5,
            "the world comes from the row that supplied the centre, not from a lower unanchored one"
        );
        assert_eq!(observers[&43].membership, 8);
        assert!(!observers.contains_key(&0), "an unowned row anchors nobody");
        assert_eq!(observers.len(), 2);
    }

    /// A peer with no anchored body gets no entry, so it is neither distance-culled nor
    /// membership-filtered: `update_interest` reads the absence as MEMBERSHIP_GLOBAL and it sees
    /// every world. Both halves fail open together.
    #[test]
    fn a_peer_with_no_anchored_body_has_no_observer_at_all() {
        let rows = [row(1, 42, None, 77), row(2, 0, Some([1.0, 0.0, 0.0]), 5)];
        let mut observers: HashMap<i32, PeerObserver> = HashMap::new();
        OrbitNet::collect_observers(&rows, &mut observers);
        assert!(observers.is_empty());
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
        // attracted twice. `note_ack` refuses an ack that does not ADVANCE, which closes the free
        // version of the attack -- a peer that says nothing new gets no sample at all. It does NOT
        // tie the reported ack to the highest frame the peer actually received, and it accepts any
        // advance however small. A client that advances at full rate while holding a constant lag
        // is therefore measured at that lag, and reads identically to a genuinely slow peer.
        //
        // There is no server-side cross-check available: the ping/pong clock is client-initiated
        // and `integrate_pong` runs only on clients, so the server has no independent figure. The
        // containment is the millisecond ceiling in `NetLagComp`, not anything here.
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
}
