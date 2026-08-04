//! The OrbitNet session singleton.
//!
//! One node owns the whole netcode hot path: the tick clock, the per-entity rollback loop, the
//! packet pump, clock sync, and the diagnostics the `Net` facade republishes. The design is the
//! inverse of the backend it replaces: instead of every synchronizer subscribing to five signals
//! per rollback tick, `OrbitNet` iterates the entity registry (a `BTreeMap`, so replay order is
//! stable — the bit-exact resim gate would read a nondeterministic order as a phantom desync) and
//! calls plain methods. Per-entity dirty windows come from `orbitnet_core::ResimPlanner`, so one
//! late peer deepens only its own body's replay (#318).
//!
//! Transport: `SceneMultiplayer.send_bytes()` + the `peer_packet` signal — one batched frame per
//! peer per tick, riding above the `MultiplayerPeer`, so ENet, Steam and Offline peers are
//! indistinguishable from here (`docs/orbitnet-native.md` §5.1).
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
use orbitnet_core::{
    ClockEstimator, CoupledSlew, LeadTracker, ResimPlanner, TickAccumulator, TickRate,
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
/// Clock offset beyond which slew/stretch corrections are hopeless and the client reseeks.
const HARD_RESYNC_SECONDS: f64 = 0.25;

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
    /// Per-entity newest tick sent — drives send priority and the periodic-full phase.
    last_sent: HashMap<u64, u64>,
    /// The peer asked for full masks (its delta base broke).
    want_full: bool,
    /// Newest input tick received from this peer (server side).
    newest_input_tick: i64,
    /// Input-arrival margin reported back in snapshot headers.
    margin_last: i8,
    /// Rollback entities currently inside this peer's interest radius (AOI hysteresis).
    interest: std::collections::HashSet<u64>,
    /// Recent snapshot sends awaiting acknowledgement: (frame tick, entity ticks it carried).
    sent_log: std::collections::VecDeque<(u64, Vec<(u64, u64)>)>,
    /// Per-entity newest tick this peer CONFIRMED receiving (via ack_tick/ack_bits) — the only
    /// tick a masked delta may reference: the peer provably holds that base row.
    acked_base: HashMap<u64, u64>,
    /// Highest ack tick seen from this peer, for expiring the sent log.
    newest_ack: u64,
}

/// How many unacked snapshot frames a peer's sent log retains before the oldest expire.
const SENT_LOG_DEPTH: usize = 64;

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

    /// Interest radius in metres for rollback entities (0 = off: every peer receives everything).
    ///
    /// The 100-player lever: with a radius set, each peer receives only the bodies within it of
    /// that peer's own body, with a 1.25x exit hysteresis so boundary entities don't flicker.
    /// State-lane entities always replicate (they are small and carry gameplay-critical facts).
    #[export]
    aoi_radius: f64,

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
    m_net_ms: f64,
    m_rb_nodes: f64,

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
            m_net_ms: 0.0,
            m_rb_nodes: 0.0,
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
            self.step_decoupled(delta);
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
            "net_ms" => self.m_net_ms,
            "rb_nodes" => self.m_rb_nodes,
            "stretch" => self.stretch_now,
            "offset_ms" => self.clock.offset() * 1000.0,
            "rtt_ms" => self.clock.rtt() * 1000.0,
            "jitter_ms" => self.clock.jitter() * 1000.0,
            "lead_ticks" => self.lead_bias_ticks,
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

    fn send_to(&self, peer: i32, bytes: &[u8], mode: TransferMode) {
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
                        peer.acked_base.remove(&id);
                        peer.interest.remove(&id);
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
        if !self.clock.needs_hard_resync(HARD_RESYNC_SECONDS) {
            return false;
        }
        let offset = self.clock.offset();
        let local = (self.accumulator.tick() as f64 + self.accumulator.tick_factor()) * dt;
        let target = (((local + offset) / dt).max(0.0) as u64).saturating_add(INITIAL_LEAD_TICKS);
        godot_warn!(
            "OrbitNet: clock offset {:.0} ms is beyond the slew's reach — hard resync tick {} -> {}",
            offset * 1000.0,
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

        for tick in from..current {
            self.rollback_tick_now = Some(tick);

            // Phase 1 — restore state + input for every entity replaying this tick.
            for (_, range_from, range_to, sync) in &ranges {
                if tick < *range_from || tick >= *range_to {
                    continue;
                }
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                sync.bind_mut().restore_tick(tick);
            }

            // Phase 2 — simulate. Collect the call list with all binds dropped, then run the
            // game code under a base_mut() surrender so its callbacks can re-enter this node.
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
                let rollback_method = StringName::from("_rollback_tick");
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

            // Phase 3 — record the resulting state as tick + 1.
            for (_, range_from, range_to, sync) in &ranges {
                if tick < *range_from || tick >= *range_to {
                    continue;
                }
                let Some(mut sync) = live_handle(sync) else {
                    continue;
                };
                sync.bind_mut().record_tick(tick);
            }
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

    fn send_snapshots(&mut self, current: u64) {
        let budget = self.send_budget.max(256) as usize;
        let peer_ids: Vec<i32> = self
            .peers
            .iter()
            .filter(|(_, p)| p.synced)
            .map(|(&id, _)| id)
            .collect();

        for peer_id in peer_ids {
            // Priority: never-sent and stalest entities first, so the budget defers the
            // freshest instead of starving anyone.
            let mut order: Vec<(u64, u64)> = Vec::new();
            {
                let Some(peer) = self.peers.get(&peer_id) else {
                    continue; // disconnected while an earlier peer's frame was going out
                };
                for &id in self
                    .rollback_entities
                    .keys()
                    .chain(self.state_entities.keys())
                {
                    let last = peer.last_sent.get(&id).copied().unwrap_or(0);
                    order.push((last, id));
                }
            }
            order.sort_unstable();

            let want_full = self
                .peers
                .get(&peer_id)
                .map(|p| p.want_full)
                .unwrap_or(false);

            self.update_peer_interest(peer_id);

            let mut writer = Writer::with_capacity(budget + 128);
            let (ack_tick, margin) = {
                let Some(peer) = self.peers.get(&peer_id) else {
                    continue;
                };
                (
                    u32::try_from(peer.newest_input_tick.max(0)).unwrap_or(u32::MAX),
                    peer.margin_last,
                )
            };
            let header_pos_placeholder = FrameHeader {
                kind: FrameKind::ServerSnapshot,
                tick: u32::try_from(current).unwrap_or(u32::MAX),
                ack_tick,
                ack_bits: 0,
                margin_ticks: margin,
                flags: 0,
                entity_count: 0,
            };
            // Entity count is not known until the budget loop ends, so encode blocks into a
            // side buffer and write the header after.
            let mut body = Writer::with_capacity(budget);
            let mut sent: Vec<(u64, u64)> = Vec::new();

            for &(last_sent, id) in &order {
                if body.len() >= budget {
                    break;
                }
                let full_due = want_full
                    || last_sent == 0
                    || orbitnet_core::interest::send_phase(id, current, FULL_STATE_INTERVAL)
                        && current.saturating_sub(last_sent) >= FULL_STATE_INTERVAL;
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

                let tick_sent = if let Some(sync) = self.rollback_entities.get(&id) {
                    if !self
                        .peers
                        .get(&peer_id)
                        .map(|p| p.interest.contains(&id))
                        .unwrap_or(true)
                    {
                        continue;
                    }
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
                if let Some(tick) = tick_sent {
                    sent.push((id, tick));
                }
            }

            if sent.is_empty() {
                continue;
            }
            let header = FrameHeader {
                entity_count: sent.len() as u32,
                ..header_pos_placeholder
            };
            header.encode(&mut writer);
            writer.bytes(body.as_slice());
            self.dbg_sent += sent.len() as u64;
            self.dbg_sent_bytes += writer.len() as u64;
            self.send_to(peer_id, writer.as_slice(), TransferMode::UNRELIABLE);

            if let Some(peer) = self.peers.get_mut(&peer_id) {
                peer.want_full = false;
                for &(id, tick) in &sent {
                    peer.last_sent.insert(id, tick);
                }
                peer.sent_log.push_back((current, sent));
                while peer.sent_log.len() > SENT_LOG_DEPTH {
                    peer.sent_log.pop_front();
                }
            }
        }
    }

    /// Recompute which rollback entities are inside `peer`'s interest radius.
    ///
    /// Enter at `aoi_radius`, leave at 1.25x — the hysteresis band stops boundary flicker. The
    /// peer's own body (its input authority) is always in interest, as is any entity without a
    /// resolvable position. Distances come from packed frontier rows — zero Godot calls.
    fn update_peer_interest(&mut self, peer_id: i32) {
        let radius = self.aoi_radius;
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return;
        };
        if radius <= 0.0 {
            peer.interest.clear();
            for &id in self.rollback_entities.keys() {
                peer.interest.insert(id);
            }
            return;
        }

        // The peer's own body anchors the radius; without one (pre-spawn), everything stays
        // in interest rather than blanking the world.
        let mut center: Option<[f32; 3]> = None;
        for sync in self.rollback_entities.values() {
            if !sync.is_instance_valid() {
                continue;
            }
            let bound = sync.bind();
            if bound.input_owner_peer() == peer_id {
                center = bound.position_hint();
                break;
            }
        }
        let Some(center) = center else {
            for &id in self.rollback_entities.keys() {
                peer.interest.insert(id);
            }
            return;
        };

        let enter_sq = radius * radius;
        let exit_sq = enter_sq * 1.25 * 1.25;
        for (&id, sync) in &self.rollback_entities {
            if !sync.is_instance_valid() {
                continue;
            }
            let bound = sync.bind();
            if bound.input_owner_peer() == peer_id {
                peer.interest.insert(id);
                continue;
            }
            let Some(pos) = bound.position_hint() else {
                peer.interest.insert(id);
                continue;
            };
            let dx = f64::from(pos[0] - center[0]);
            let dy = f64::from(pos[1] - center[1]);
            let dz = f64::from(pos[2] - center[2]);
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if peer.interest.contains(&id) {
                if dist_sq > exit_sq {
                    peer.interest.remove(&id);
                }
            } else if dist_sq <= enter_sq {
                peer.interest.insert(id);
            }
        }
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

    fn server_time_now(&self) -> f64 {
        let dt = self.effective_rate().dt();
        (self.accumulator.tick() as f64 + self.accumulator.tick_factor()) * dt
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
        {
            let Some(peer) = self.peers.get_mut(&sender) else {
                return; // No handshake, no input.
            };
            if header.flags & FrameHeader::FLAG_WANT_FULL != 0 {
                peer.want_full = true;
            }
            // Consume the ack window: every snapshot frame the client confirms receiving
            // promotes the entity ticks it carried to `acked_base` — the only ticks a masked
            // delta may reference, because the client provably holds those rows.
            let ack = u64::from(header.ack_tick);
            if ack > 0 {
                peer.newest_ack = peer.newest_ack.max(ack);
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

        for _ in 0..header.entity_count {
            let Ok(meta) = decode_input_block_meta(reader, u64::from(header.tick)) else {
                return;
            };
            // Bound accepted input to the near future: the history ring only rejects the PAST, so
            // a hostile newest_tick near u64::MAX would rotate the ring's frontier out of reach
            // and freeze this body's input for the rest of the session. No honest client leads
            // the server by anywhere near a full history window.
            if meta.newest_tick > current.saturating_add(self.history_limit.max(2) as u64) {
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
            // the anti-forgery check.
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
                if tick < current {
                    self.planner.mark(id, tick);
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
                        Ok(StateIntegration::Rejected) => {
                            self.dbg_rx_rejected += 1;
                            if !meta.full {
                                self.want_full = true;
                            }
                        }
                        Ok(_) => {
                            self.dbg_rx_applied += 1;
                        }
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
                        Ok(StateIntegration::Rejected) => {
                            self.dbg_rx_rejected += 1;
                            if !meta.full {
                                self.want_full = true;
                            }
                        }
                        Ok(_) => {
                            self.dbg_rx_applied += 1;
                        }
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
