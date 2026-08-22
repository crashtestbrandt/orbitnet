//! The per-entity synchronizers.
//!
//! [`OrbitRollbackSynchronizer`] owns one entity's replication state: the resolved property
//! bindings, the packed state/input history rings, the input-confidence ledger, and the
//! tick-memo ring. It does **not** drive anything — the `OrbitNet` singleton iterates entities in
//! ascending id order and calls the `pub(crate)` phase methods below, which keeps the whole
//! rollback loop a flat native iteration instead of the old five-signals-per-tick fan-out.
//!
//! [`OrbitStateSynchronizer`] is the no-rollback lane: server-authoritative extract-and-broadcast
//! with apply-on-receive, so a value set outside the tick loop (a NetCommand handler) is never
//! clobbered by a rollback restore. Holster containers, health, env sensors, the celestial rig
//! and NPC poses ride this lane.
//!
//! Entity identity is the FNV-1a hash of the synchronizer root's node path, salted per lane. Both
//! peers derive the same id because the `MultiplayerSpawner` guarantees identical node names —
//! the invariant any node-path-derived identity scheme leans on, made explicit.
//!
//! **Bulk marshalling** is opt-in per synchronizer: declare `bulk_capture_method` (and, on the
//! rollback lane, `bulk_restore_method`) and the lane moves its whole row through one
//! `Object::call` instead of one `Object::get` / `Object::set` per property. Because that call is
//! game code, it is **staged** rather than made in place — `stage_capture` and `restore_tick` fill
//! or drain the hook's preallocated array with the synchronizer bound, and `OrbitNet` runs the
//! staged calls with every `bind` dropped, the same surrender phase 2 makes for `_rollback_tick`.
//! Every lane that declares no hook keeps the walk, and the row it records is the same bytes
//! either way: the hook supplies `Variant`s, and the encode, the offsets and the quantized
//! canonicalization stay in `binding`.

use godot::classes::Node;
use godot::prelude::*;

use orbitnet_core::codec::{
    decode_state_block_into, encode_state_block, Reader, StateBlockMeta, Writer,
};
use orbitnet_core::{
    ColumnarHistory, Confidence, FreshnessLedger, MembershipId, MemoRing, PropKind, PropRole,
    PropSchema, SchemaBuilder, MEMBERSHIP_GLOBAL,
};

use crate::binding::{self, PropBinding};
use crate::orbit_net::SeatIndex;

/// Bulk-hook lane ordinal: the STATE lane — the state entries then the cosmetic entries, in the
/// order they were declared. The only lane an [`OrbitStateSynchronizer`] has.
pub(crate) const LANE_STATE: i64 = 0;
/// Bulk-hook lane ordinal: the INPUT lane of an [`OrbitRollbackSynchronizer`].
pub(crate) const LANE_INPUT: i64 = 1;

/// `relevancy`: this channel is replicated to every peer in every world, whatever the interest
/// radius says and whatever `membership_property` names.
pub(crate) const RELEVANCY_ALWAYS: i32 = 0;
/// `relevancy`: this channel is culled by distance from the anchor `anchor_property` names, and by
/// the membership `membership_property` names.
pub(crate) const RELEVANCY_ANCHORED: i32 = 1;
/// `relevancy`: this channel is never culled by distance, and is replicated only to peers in the
/// membership `membership_property` names.
///
/// The setting for a channel with **no position to be culled by** — health, inventory, a door's
/// state. Before this existed such a channel had one lever, all-or-nothing, so it reached every
/// peer in every world. See the `interest` module header in `orbitnet-core`.
pub(crate) const RELEVANCY_MEMBERSHIP: i32 = 2;

/// Resolve a `membership_property` entry into a live `(node, property)` pair.
///
/// `label` names the synchronizer in any diagnostic. Every failure path returns `None`, which reads
/// as [`MEMBERSHIP_GLOBAL`] — the same fail-open direction the anchor takes, and for the same
/// reason: a misconfigured membership costs bandwidth, a silently-wrong one deletes a body from
/// somebody's world.
///
/// The property must be a Godot **int**. A membership id is compared for equality and never
/// measured, so a float would introduce a rounding question the filter has no answer to.
fn resolve_membership(
    root: Option<&Gd<Node>>,
    entry: &GString,
    label: &str,
) -> Option<(Gd<Node>, StringName)> {
    let entry = entry.to_string();
    if entry.is_empty() {
        return None;
    }
    match root.and_then(|r| binding::resolve_entry(r, &entry)) {
        Some((target, name, PropKind::I64 | PropKind::I32)) => Some((target, name)),
        Some((_, _, kind)) => {
            godot_error!(
                "{label}: membership_property {entry:?} resolved to {kind:?}, not an int. A \
                 membership id is compared for equality; this channel stays in every world."
            );
            None
        }
        None => {
            godot_error!(
                "{label}: membership_property {entry:?} did not resolve against the root — this \
                 channel stays in every world."
            );
            None
        }
    }
}

/// The declared entry names one resolved hook marshals, or an empty list when the lane has none.
fn hook_order(hook: &Option<binding::BulkHook>, props: &[PropSchema]) -> PackedStringArray {
    match hook {
        Some(hook) => binding::hook_order(props, hook.slots()),
        None => PackedStringArray::new(),
    }
}

/// Read a resolved membership pair live, or [`MEMBERSHIP_GLOBAL`] when it is unset, unresolved, or
/// the node behind it has been freed.
///
/// Only the authority calls this, and the authority owns the value, so reading it live is both
/// correct and cheaper than replicating an id the wire does not otherwise need — the same argument
/// `position_hint` makes for the anchor.
///
/// A Godot int is an `i64` and a [`MembershipId`] is a `u64`, so the `as` is a **reinterpretation
/// of the same 64 bits**, not a conversion: every distinct declared value stays distinct, `0` stays
/// [`MEMBERSHIP_GLOBAL`], and there is no value a game can write that the filter has to reject.
/// Note that a game using `-1` as its own "unset" gets `u64::MAX`, which is a world like any other.
///
/// **TYPE THE PROPERTY.** A live read that stops converting to an `i64` falls back to
/// [`MEMBERSHIP_GLOBAL`] here, silently and every tick, which puts the entity in every world. That is
/// reachable without touching this export: [`resolve_membership`] validates the kind
/// `binding::resolve_entry` reported, and for an *untyped* GDScript `var world_id = 0` that kind is
/// sniffed from the value the property happened to hold at resolve time. Assign a float to that same
/// untyped var later — `world_id = 1.0` — and the conversion starts failing. Declaring `var world_id:
/// int = 0` makes the kind a fact about the property rather than about one moment.
///
/// It fails **open** rather than warning, deliberately and for the same reason as the rest of this
/// feature: the alternative to a silent leak is a silent deletion, and this runs once per entity per
/// tick, so a diagnostic here is a per-tick log flood on the authority. [`Self::get_membership`] on
/// both lanes reports the value the filter actually reads, which is where a misconfiguration is meant
/// to be caught.
fn read_membership(pair: Option<&(Gd<Node>, StringName)>) -> MembershipId {
    let Some((node, name)) = pair else {
        return MEMBERSHIP_GLOBAL;
    };
    let Some(node) = crate::orbit_net::live_handle(node) else {
        return MEMBERSHIP_GLOBAL;
    };
    node.get(name)
        .try_to::<i64>()
        .map(|value| value as MembershipId)
        .unwrap_or(MEMBERSHIP_GLOBAL)
}

/// Rows the **state lane** retains per entity.
///
/// This was `8`, chosen for "slack for reordering" — a comment that predates `acked_base` and is no
/// longer the constraint. A masked delta may only reference a tick the peer has ACKED, and an ack
/// costs a full round trip: at 60 Hz an 8-row ring spans 133 ms, so above roughly 130 ms RTT the
/// base has *always* fallen out of the ring by the time it becomes usable, and
/// [`OrbitStateSynchronizer::encode_block`] resolves `reference = None` on every block forever. The
/// fattest non-player lane on the wire therefore sent full rows on exactly the links least able to
/// carry them, and no amount of interest culling would have touched it.
///
/// 64 rows spans 1.07 s at 60 Hz and 2.13 s at 30 — past the 250 ms design ceiling netbench's
/// `worst_case` profile is calibrated against, and past the 32-frame ack window the frame header
/// carries. An entity the send rota visits *less* often than that still degrades to a full block,
/// which is correct: a body seen once a second is one whose delta base is worthless anyway.
///
/// The cost is `64 × row_stride` bytes per entity, which for a fat channel — 41 `i64` props at
/// 328 B/row, say an inventory or equipment block — is 21 kB, or 2.6 MB across a full 100-player
/// session, on the server alone.
const STATE_HISTORY_DEPTH: usize = 64;

/// What integrating a received authoritative state concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateIntegration {
    /// The row matched our recorded prediction bit-for-bit — nothing to resimulate.
    Confirmed,
    /// The row differed from our prediction at this tick: resimulate from here.
    Mispredict(u64),
    /// Applied/buffered for a display-only or future tick — no rollback interaction.
    Buffered,
    /// The block was a masked delta and the base row it references is not resident, so nothing
    /// could be decoded. **The only rejection a `WANT_FULL` NACK can fix**, because the same tick
    /// re-sent as a full block carries every prop and needs no base.
    NoBase,
    /// The row was decoded and then discarded because this receiver is already past it — an
    /// out-of-order or duplicate datagram, or a tick older than the history window can hold.
    ///
    /// **Separate from [`Self::NoBase`], and the separation is the fix.** Both
    /// were once `Rejected`, and the receiver answered any non-full rejection with `want_full`,
    /// which is a per-peer **all-entity** flag: one reordered datagram made the server's next frame
    /// full blocks for every entity that peer holds, at a byte budget that carries a fraction of
    /// them, so the rest deferred, arrived as deltas against a base that had moved, and re-raised
    /// it. Reordering is routine on a relayed or congested link, so the storm ran continuously
    /// there and never once on loopback — which is exactly the asymmetry a relayed-link playtest reported
    /// (remote bodies frozen then jumping, the client's own rows arriving every several ticks).
    /// A newer row for the same entity already applied, so there is nothing to ask for.
    Stale,
}

/// Rollback state + input replication for one entity.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct OrbitRollbackSynchronizer {
    base: Base<Node>,

    /// Node the declared property paths resolve against. Defaults to this node's parent.
    #[export]
    root: Option<Gd<Node>>,

    /// Node that owns the *input* properties.
    ///
    /// The server-authoritative split needs input authority to differ from state authority, which
    /// requires the input to live on its own node. A first-class export rather than a convention.
    #[export]
    input_authority_node: Option<Gd<Node>>,

    /// Simulation state entries, each `"NodePath:property"` or a bare `"property"`.
    #[export]
    state_properties: PackedStringArray,

    /// Player-intent entries, in the same form.
    #[export]
    input_properties: PackedStringArray,

    /// Presentation-only entries: replicated, never restored, never counted as a misprediction.
    #[export]
    cosmetic_properties: PackedStringArray,

    /// Whether this peer predicts this entity locally.
    #[export]
    enable_prediction: bool,

    /// Display-only exemption: this peer applies received state and never joins the rollback
    /// loop.
    #[export]
    exempt: bool,

    /// Send-rota priority, `1..=16`, multiplying the distance-band weight.
    ///
    /// The backend must not guess game semantics: a scene that considers this body worth four
    /// ordinary ones when the byte budget is tight says so here. The ownership floor is applied
    /// separately and needs no declaration — a peer's own body is recognised by its input authority.
    #[export]
    priority: i32,

    /// Which **seat** on the owning connection drives this body — `0` unless the game says
    /// otherwise.
    ///
    /// A seat is one owned viewpoint behind one transport peer. Local split-screen over a network
    /// session is two or more locally-owned, locally-predicted bodies on a single connection, and
    /// each needs its own interest anchor: the second player's surroundings are not the first
    /// player's. The owning peer is read off `input_authority_node`; this is the other half of the
    /// answer, and `(input owner, seat)` is what the interest pass keys an anchor on.
    ///
    /// **Server-side, and it replicates nothing.** Interest is computed only where state authority
    /// is, so this is read only there — the server assigns seats and declares them on its own copy
    /// of the scene. A client may leave every body at `0`; nothing on the wire carries a seat, and
    /// the anti-forgery check on received input is per entity and unchanged.
    ///
    /// **A label, not a slot index.** Two bodies with the same input owner and the same value here
    /// share one anchor — lowest entity id wins, as before — and every distinct value on one
    /// connection is one more interest set to maintain. The numbers need not be contiguous, and
    /// their order decides nothing but the order the sets are held in.
    ///
    /// Every body left at the default `0` is one seat per connection, which is what every
    /// connection had before seats existed.
    #[export]
    seat: i32,

    /// The world this body belongs to, as a `"NodePath:property"` entry naming an **int**, resolved
    /// against `root`.
    ///
    /// Interest is a distance test, and distance cannot separate several independent worlds inside
    /// one session when each is rebased near its own coordinate origin: two bodies at the same
    /// coordinates in different worlds are zero metres apart. A peer only ever replicates bodies
    /// whose membership matches its own, whatever the radius says.
    ///
    /// **This lane has no `relevancy` export and needs none.** A rollback body always carries a
    /// position, so it is always distance-cullable; membership narrows that, it does not replace it.
    ///
    /// Unset, unresolvable, or not an int leaves the body in `MEMBERSHIP_GLOBAL` — every world, the
    /// behaviour every rollback body had before this existed, and the fail-open direction.
    ///
    /// The entry need **not** be one of `state_properties`: it costs no wire bytes and is read live
    /// on the authority, the only peer that computes relevancy.
    #[export]
    membership_property: GString,

    /// Bulk **capture** hook: the name of a game method that fills a whole lane's values in one
    /// script-boundary crossing, or empty for the per-property walk.
    ///
    /// Signature: `func <name>(lane: int, values: Array) -> void`. `lane` is `0` for the state lane
    /// (state entries then cosmetic entries) and `1` for the input lane. Fill every slot of
    /// `values` in the order [`Self::bulk_capture_order`] publishes — the array is preallocated and
    /// reused, so a slot left alone keeps last tick's value.
    ///
    /// What it is worth: capture is `S` `Object::get` calls per lane, and the rollback loop pays
    /// them **per replayed tick, per entity**. This makes it `1` per lane per tick. A fat channel
    /// of 41 props replaying 12 ticks costs 492 property reads in one frame; through a hook it
    /// costs 12 calls.
    ///
    /// **Opt-in, and byte-identical.** An empty declaration keeps the walk. The hook supplies the
    /// `Variant`s and nothing else: the encode, the offsets and the quantized canonicalization stay
    /// where they are, because masks, delta bases and the mispredict compare read that layout.
    #[export]
    bulk_capture_method: GString,

    /// Bulk **restore** hook: the name of a game method that reads a whole lane's values back in
    /// one crossing, or empty for the per-property walk.
    ///
    /// Signature: `func <name>(lane: int, values: Array) -> void`, lanes as above. Read the slots
    /// in the order [`Self::bulk_restore_order`] publishes and write them onto the game's own
    /// fields; do not resize the array.
    ///
    /// **The restore order is not the capture order.** `Cosmetic` entries are captured and
    /// replicated but never restored, so they are absent here and present there. A lane that
    /// declares no cosmetics has identical lists.
    ///
    /// **It covers the rollback loop, not the receive path.** Applying a received row still walks
    /// the properties: that runs once per received block, not once per replayed tick, and it is the
    /// path that must also land cosmetics.
    #[export]
    bulk_restore_method: GString,

    // --- resolved at process_settings ---
    entity_id: u64,
    state_schema: SchemaBuilder,
    state_bindings: Vec<PropBinding>,
    input_schema: SchemaBuilder,
    input_bindings: Vec<PropBinding>,
    unresolved: PackedStringArray,
    rollback_nodes: Vec<Gd<Node>>,
    /// The resolved `membership_property`, or `None` when unset or unresolvable.
    membership: Option<(Gd<Node>, StringName)>,
    /// Resolved bulk hooks, `None` where the lane keeps the per-property walk.
    state_capture_hook: Option<binding::BulkHook>,
    state_restore_hook: Option<binding::BulkHook>,
    input_capture_hook: Option<binding::BulkHook>,
    input_restore_hook: Option<binding::BulkHook>,
    /// Whether the last staged bulk capture is the one `record_tick` / `capture_local_input`
    /// should read. Cleared as it is consumed, so a phase that never staged the call falls back to
    /// the walk rather than encoding a stale array.
    state_capture_staged: bool,
    input_capture_staged: bool,
    /// Whether the local peer authors this entity's input (its own player).
    input_local: bool,
    /// The peer id that owns this entity's input, cached at [`Self::process_authority`].
    ///
    /// Refreshed on exactly the same contract as `input_local`, which is the point: if this can go stale, so can
    /// that, and the whole ownership model with it. Read by the once-per-tick send-path gather, so the send path
    /// costs ZERO `get_multiplayer_authority()` calls. The anti-forgery check on received input deliberately does
    /// NOT read this -- see [`Self::input_owner_peer`].
    input_owner: i32,
    /// Whether the local peer owns this entity's state (the server).
    state_local: bool,

    // --- runtime ---
    state_history: ColumnarHistory,
    input_history: ColumnarHistory,
    ledger: FreshnessLedger,
    memo: MemoRing,
    history_limit: usize,
    /// Wire rows this receiver has decoded, keyed by tick. These are the **delta bases**, stored
    /// apart from `state_history`. `None` until the first block arrives, so a sender-only peer
    /// pays no memory for it.
    ///
    /// - A masked delta may decode only against the row the sender deltaed against. Decoding over
    ///   a locally simulated row corrupts silently and raises no error.
    /// - `state_history` rows are rewritten by the owner's own replay, so they cannot serve as
    ///   bases. Keeping the two apart is what makes a base survive a resim.
    /// - A row too old for the simulation to apply is still a valid base, because the receiver
    ///   acknowledged the frame it rode in and the sender may name its tick.
    auth_rows: Option<ColumnarHistory>,
    /// Newest authoritative state tick known (received on clients, broadcast tick on the server).
    latest_state_tick: i64,
    /// Newest received-and-not-yet-integrated authoritative row (owner reconcile path).
    pending_state: Option<(u64, Vec<u8>)>,
    /// Newest received row awaiting the next tick boundary (display path).
    pending_display: Option<(u64, Vec<u8>)>,
    /// One past the newest tick simulated at authoritative confidence — the broadcastable tick.
    latest_auth_sim_tick: u64,
    /// Newest input tick received from the remote owner (server side).
    latest_remote_input_tick: i64,
    /// Whether the most recent simulated tick ran on non-authoritative input.
    predicted_last: bool,
    scratch_state: Vec<u8>,
    scratch_input: Vec<u8>,
    scratch_wire: Vec<u8>,
}

#[godot_api]
impl INode for OrbitRollbackSynchronizer {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            root: None,
            input_authority_node: None,
            state_properties: PackedStringArray::new(),
            input_properties: PackedStringArray::new(),
            cosmetic_properties: PackedStringArray::new(),
            enable_prediction: true,
            exempt: false,
            priority: 1,
            seat: 0,
            membership_property: GString::new(),
            bulk_capture_method: GString::new(),
            bulk_restore_method: GString::new(),
            entity_id: 0,
            state_schema: SchemaBuilder::new(),
            state_bindings: Vec::new(),
            input_schema: SchemaBuilder::new(),
            input_bindings: Vec::new(),
            unresolved: PackedStringArray::new(),
            rollback_nodes: Vec::new(),
            membership: None,
            state_capture_hook: None,
            state_restore_hook: None,
            input_capture_hook: None,
            input_restore_hook: None,
            state_capture_staged: false,
            input_capture_staged: false,
            input_local: false,
            input_owner: 0,
            state_local: false,
            state_history: ColumnarHistory::new(0, 1),
            input_history: ColumnarHistory::new(0, 1),
            ledger: FreshnessLedger::with_capacity(1),
            memo: MemoRing::with_capacity(1),
            history_limit: 128,
            auth_rows: None,
            latest_state_tick: -1,
            pending_state: None,
            pending_display: None,
            latest_auth_sim_tick: 0,
            latest_remote_input_tick: -1,
            predicted_last: false,
            scratch_state: Vec::new(),
            scratch_input: Vec::new(),
            scratch_wire: Vec::new(),
        }
    }

    fn get_configuration_warnings(&self) -> PackedStringArray {
        let mut warnings = PackedStringArray::new();
        if self.root.is_none() && self.base().get_parent().is_none() {
            warnings.push("No root is set and this node has no parent to fall back on.");
        }
        if self.state_properties.is_empty() && self.input_properties.is_empty() {
            warnings.push(
                "Neither state_properties nor input_properties is set — this synchronizer \
                 replicates nothing.",
            );
        }
        if !self.unresolved.is_empty() {
            warnings.push(&format!(
                "{} declared propert{} could not be resolved; call process_settings() and check the paths.",
                self.unresolved.len(),
                if self.unresolved.len() == 1 { "y" } else { "ies" }
            ));
        }
        warnings
    }

    fn exit_tree(&mut self) {
        let me = self.to_gd().instance_id_unchecked();
        crate::orbit_net::unregister_entity(self.entity_id, me);
    }
}

#[godot_api]
impl OrbitRollbackSynchronizer {
    /// Add a state property. `node` may be a `Node`, a `NodePath`, or a string path; `property`
    /// is the property name on it. Call [`Self::process_settings`] after the last addition.
    #[func]
    fn add_state(&mut self, node: Variant, property: GString) {
        if let Some(entry) = self.make_entry(&node, &property) {
            if !self.state_properties.as_slice().contains(&entry) {
                self.state_properties.push(&entry);
            }
        }
    }

    /// Add an input property, in the same form as [`Self::add_state`].
    #[func]
    fn add_input(&mut self, node: Variant, property: GString) {
        if let Some(entry) = self.make_entry(&node, &property) {
            if !self.input_properties.as_slice().contains(&entry) {
                self.input_properties.push(&entry);
            }
        }
    }

    /// Add a cosmetic property: replicated, never restored during rollback, never counted as a
    /// misprediction. The test for cosmetic is "the simulation never reads it back".
    #[func]
    fn add_cosmetic(&mut self, node: Variant, property: GString) {
        if let Some(entry) = self.make_entry(&node, &property) {
            if !self.cosmetic_properties.as_slice().contains(&entry) {
                self.cosmetic_properties.push(&entry);
            }
        }
    }

    /// Resolve the declared properties, build the schemas and histories, and register with the
    /// OrbitNet singleton. Idempotent; call after any configuration change.
    #[func]
    fn process_settings(&mut self) {
        self.resolve_all();
        let id = self.entity_id;
        if id != 0 {
            crate::orbit_net::register_rollback_entity(id, self.to_gd());
        }
    }

    /// Re-resolve which peer owns state and input after an authority change.
    #[func]
    fn process_authority(&mut self) {
        let local_peer = self.local_peer_id();
        self.state_local = self
            .resolved_root()
            .map(|n| n.get_multiplayer_authority() == local_peer)
            .unwrap_or(false);
        let owner = self
            .resolved_input_root()
            .map(|n| n.get_multiplayer_authority())
            .unwrap_or(0);
        self.input_owner = owner;
        self.input_local = owner == local_peer;
    }

    /// Point this entity's input at `peer`, and re-resolve everything that reads the answer.
    ///
    /// The two halves have to happen together and one of them is easy to forget. Writing the authority alone
    /// changes what [`Self::input_owner_peer`] answers — the anti-forgery check on a received input block —
    /// but leaves `input_local` and [`Self::input_owner_hint`] naming the previous owner, so this peer keeps
    /// predicting (or keeps refusing to predict) the wrong body and the send path anchors the wrong peer's
    /// interest radius. That is why this exists as one call rather than as a note in a doc comment.
    ///
    /// **It is local, and it must be called on EVERY peer.** Multiplayer authority is a property of a node
    /// on the peer that holds it; nothing here replicates. A peer that missed the call disagrees about who
    /// owns the body, and on the server that disagreement is what starts rejecting the new owner's input.
    ///
    /// `peer` is a transport peer id. `1` hands the input back to the server, which is what a game does when
    /// a seat empties.
    #[func]
    fn set_input_authority(&mut self, peer: i64) {
        if let Some(mut node) = self.resolved_input_root() {
            node.set_multiplayer_authority(peer as i32);
        }
        self.process_authority();
    }

    /// Whether the most recent simulated tick ran on non-authoritative (predicted or
    /// extrapolated) input.
    #[func]
    fn is_predicting(&self) -> bool {
        self.predicted_last
    }

    /// This entity's stable replication id, as an opaque token.
    ///
    /// It is what `OrbitNet::set_peer_anchor_entity` names, and it is the ONLY reason it is
    /// published: nothing else in the API takes one. Derived from the root's scene path, so it is
    /// the same number on every peer, and it is `0` until `process_settings` has resolved a root
    /// that is inside the tree.
    ///
    /// **A token, not a quantity.** It is a 64-bit FNV hash reinterpreted as a signed integer, so it
    /// is routinely negative and comparing two of them for order means nothing — the same hash whose
    /// arbitrary ordering picks a peer's inferred observer. Pass it back unmodified.
    #[func]
    fn get_entity_id(&self) -> i64 {
        self.entity_id as i64
    }

    /// The tick of the newest authoritative state known for this entity (-1 before any).
    #[func]
    fn get_last_known_state(&self) -> i64 {
        self.latest_state_tick
    }

    /// The world this body is currently in, `0` meaning every world (diagnostics and tests).
    ///
    /// **The value to check first when membership filtering "does nothing".** A peer's own world is read off
    /// the body that anchors its interest radius, so a body reporting `0` here is a peer seeing every world,
    /// and every other entity's declaration is then irrelevant for that peer. Reports what the filter would
    /// read this tick, so a `membership_property` that did not resolve reports `0` rather than the value the
    /// game wrote.
    #[func]
    fn get_membership(&self) -> i64 {
        self.membership_hint() as i64
    }

    /// The tick of the newest input known for this entity (-1 before any).
    #[func]
    fn get_last_known_input(&self) -> i64 {
        self.input_history
            .latest_tick()
            .map(|t| i64::try_from(t).unwrap_or(i64::MAX))
            .unwrap_or(-1)
    }

    /// Record a per-tick memo value, keyed `(tick, key)`.
    ///
    /// The backend-owned alternative to a hand-rolled resim log: record on the fresh pass, read the
    /// same value back on every replayed pass, trimmed with history.
    #[func]
    fn memo_set(&mut self, tick: i64, key: i64, value: i64) {
        if tick >= 0 {
            self.memo.set(tick as u64, key, value);
        }
    }

    /// Read a per-tick memo value, or `fallback` when none was recorded.
    #[func]
    fn memo_get(&self, tick: i64, key: i64, fallback: i64) -> i64 {
        if tick < 0 {
            return fallback;
        }
        self.memo.get(tick as u64, key).unwrap_or(fallback)
    }

    /// Hash of the resolved state schema. Peers must agree on this exactly.
    #[func]
    pub fn schema_hash(&self) -> i64 {
        i64::from(self.state_schema.hash())
    }

    /// Hash of the resolved input schema.
    #[func]
    pub fn input_schema_hash(&self) -> i64 {
        i64::from(self.input_schema.hash())
    }

    /// Bytes one tick of state history occupies for this entity.
    #[func]
    fn row_stride(&self) -> i64 {
        self.state_schema.row_stride() as i64
    }

    /// How many properties resolved successfully across both schemas.
    #[func]
    fn property_count(&self) -> i64 {
        (self.state_schema.len() + self.input_schema.len()) as i64
    }

    /// Declared entries that could not be resolved.
    #[func]
    fn unresolved_properties(&self) -> PackedStringArray {
        self.unresolved.clone()
    }

    /// The declared entries a bulk **capture** hook marshals for `lane`, in the order its array
    /// carries them: every property of the lane, state entries then cosmetic entries.
    ///
    /// Empty when the lane has no hook. Published so a game can assert the order it wrote its hook
    /// against — reordering a property list silently reorders this.
    #[func]
    fn bulk_capture_order(&self, lane: i64) -> PackedStringArray {
        match lane {
            LANE_INPUT => hook_order(&self.input_capture_hook, self.input_schema.props()),
            _ => hook_order(&self.state_capture_hook, self.state_schema.props()),
        }
    }

    /// The declared entries a bulk **restore** hook marshals for `lane`, in array order.
    ///
    /// The restored subset, so it is SHORTER than the capture order by exactly the lane's
    /// `Cosmetic` entries — replicated, never written back. Empty when the lane has no hook.
    #[func]
    fn bulk_restore_order(&self, lane: i64) -> PackedStringArray {
        match lane {
            LANE_INPUT => hook_order(&self.input_restore_hook, self.input_schema.props()),
            _ => hook_order(&self.state_restore_hook, self.state_schema.props()),
        }
    }

    /// Whether `lane` marshals through a bulk hook rather than the per-property walk.
    ///
    /// The answer to "did my method name resolve", which the order lists give away only by being
    /// empty — and an empty lane is empty for both reasons.
    #[func]
    fn uses_bulk_capture(&self, lane: i64) -> bool {
        match lane {
            LANE_INPUT => self.input_capture_hook.is_some(),
            _ => self.state_capture_hook.is_some(),
        }
    }

    /// Whether `lane` restores through a bulk hook. See [`Self::uses_bulk_capture`].
    #[func]
    fn uses_bulk_restore(&self, lane: i64) -> bool {
        match lane {
            LANE_INPUT => self.input_restore_hook.is_some(),
            _ => self.state_restore_hook.is_some(),
        }
    }

    /// A human-readable summary, for the console and for debugging.
    #[func]
    fn describe(&self) -> GString {
        GString::from(
            format!(
                "OrbitRollbackSynchronizer[{:#018x}]: {} state + {} input props, {} B/tick, \
                 schema {:#010x}/{:#010x}, {} unresolved, world {}, {}",
                self.entity_id,
                self.state_schema.len(),
                self.input_schema.len(),
                self.state_schema.row_stride(),
                self.state_schema.hash(),
                self.input_schema.hash(),
                self.unresolved.len(),
                self.membership_hint(),
                if self.exempt { "exempt" } else { "active" },
            )
            .as_str(),
        )
    }
}

impl OrbitRollbackSynchronizer {
    fn make_entry(&self, node: &Variant, property: &GString) -> Option<GString> {
        let root = self.resolved_root()?;
        let prop = property.to_string();
        if let Ok(target) = node.try_to::<Gd<Node>>() {
            let path = root.get_path_to(&target).to_string();
            if path.is_empty() {
                return None;
            }
            return Some(GString::from(format!("{path}:{prop}").as_str()));
        }
        let path = node.to_string();
        if path.is_empty() || path == "." {
            return Some(GString::from(format!(".:{prop}").as_str()));
        }
        Some(GString::from(format!("{path}:{prop}").as_str()))
    }

    /// The property-resolution root, or `None` once it is gone.
    ///
    /// `Option<Gd<Node>>::clone` clones the inner handle, and cloning a handle whose node has been freed
    /// panics under godot-rust's balanced safeguards — so an export pointing at a since-freed node must be
    /// filtered, not cloned. Falls back to the parent, matching the editor-configured default.
    fn resolved_root(&self) -> Option<Gd<Node>> {
        self.root
            .as_ref()
            .and_then(crate::orbit_net::live_handle)
            .or_else(|| self.base().get_parent())
    }

    fn resolved_input_root(&self) -> Option<Gd<Node>> {
        self.input_authority_node
            .as_ref()
            .and_then(crate::orbit_net::live_handle)
            .or_else(|| self.resolved_root())
    }

    fn local_peer_id(&self) -> i32 {
        self.base()
            .get_multiplayer()
            .map(|m| m.clone().get_unique_id())
            .unwrap_or(1)
    }

    fn resolve_all(&mut self) {
        self.state_schema = SchemaBuilder::new();
        self.input_schema = SchemaBuilder::new();
        self.state_bindings.clear();
        self.input_bindings.clear();
        self.unresolved = PackedStringArray::new();
        self.rollback_nodes.clear();
        self.membership = None;
        self.state_capture_hook = None;
        self.state_restore_hook = None;
        self.input_capture_hook = None;
        self.input_restore_hook = None;
        self.state_capture_staged = false;
        self.input_capture_staged = false;

        let state_root = self.resolved_root();

        // EVERY entry — input included — is a state-root-relative path, because that is how the
        // entries are authored (add_input computes the path from the root). input_authority_node
        // is purely the AUTHORITY seam: it decides which peer owns the input, never how paths
        // resolve. Resolving input entries against the input node was the bug that silently
        // unresolved all fifteen nin_* props and made every body look inputless.
        binding::resolve_entries(
            state_root.as_ref(),
            &self.state_properties,
            PropRole::State,
            &mut self.state_schema,
            &mut self.state_bindings,
            &mut self.unresolved,
        );
        binding::resolve_entries(
            state_root.as_ref(),
            &self.cosmetic_properties,
            PropRole::Cosmetic,
            &mut self.state_schema,
            &mut self.state_bindings,
            &mut self.unresolved,
        );
        binding::resolve_entries(
            state_root.as_ref(),
            &self.input_properties,
            PropRole::Input,
            &mut self.input_schema,
            &mut self.input_bindings,
            &mut self.unresolved,
        );

        let label = format!("OrbitRollbackSynchronizer {}", self.base().get_path());
        self.membership =
            resolve_membership(state_root.as_ref(), &self.membership_property, &label);

        // Bulk hooks resolve AFTER the schemas, because the slot lists are the schemas' own
        // orders: capture marshals every property of a lane, restore only the roles the loop
        // writes back. Both resolve against the state root, the node every declared entry is
        // already relative to.
        let capture = self.bulk_capture_method.clone();
        let restore = self.bulk_restore_method.clone();
        let capture_target = binding::hook_target(state_root.as_ref(), &capture, &label);
        let restore_target = binding::hook_target(state_root.as_ref(), &restore, &label);
        let state_all: Vec<usize> = (0..self.state_bindings.len()).collect();
        let input_all: Vec<usize> = (0..self.input_bindings.len()).collect();
        let state_restored = self.state_schema.restored();
        let input_restored = self.input_schema.restored();
        self.state_capture_hook = binding::resolve_hook(
            capture_target.as_ref(),
            &capture,
            LANE_STATE,
            state_all,
            &label,
        );
        self.input_capture_hook = binding::resolve_hook(
            capture_target.as_ref(),
            &capture,
            LANE_INPUT,
            input_all,
            &label,
        );
        self.state_restore_hook = binding::resolve_hook(
            restore_target.as_ref(),
            &restore,
            LANE_STATE,
            state_restored,
            &label,
        );
        self.input_restore_hook = binding::resolve_hook(
            restore_target.as_ref(),
            &restore,
            LANE_INPUT,
            input_restored,
            &label,
        );

        // Gather the rollback-aware nodes to simulate: the root plus every descendant that
        // implements _rollback_tick. `owned=false` so runtime-added children (the input carrier)
        // are seen too; the has_method filter keeps over-inclusion harmless.
        if let Some(root) = state_root.clone() {
            let mut nodes: Vec<Gd<Node>> = Vec::new();
            if root.has_method("_rollback_tick") {
                nodes.push(root.clone());
            }
            let children = root
                .find_children_ex("*")
                .recursive(true)
                .owned(false)
                .done();
            for child in children.iter_shared() {
                if child.has_method("_rollback_tick") {
                    nodes.push(child);
                }
            }
            self.rollback_nodes = nodes;

            // Entity identity needs the tree path; out of tree (tests, tools) the synchronizer
            // stays unregistered until a later process_settings call inside the tree.
            if root.is_inside_tree() {
                let path = root.get_path().to_string();
                self.entity_id = binding::fnv64(format!("R|{path}").as_bytes());
            }
        }

        self.process_authority();

        if !self.unresolved.is_empty() {
            godot_warn!(
                "OrbitRollbackSynchronizer {}: {} declared propert{} did not resolve: {:?} — \
                 unresolved entries silently fall off the wire, so fix the path or the type.",
                self.base().get_path(),
                self.unresolved.len(),
                if self.unresolved.len() == 1 {
                    "y"
                } else {
                    "ies"
                },
                self.unresolved
            );
        }

        let capacity = self.history_limit.max(2);
        self.state_history = ColumnarHistory::new(self.state_schema.row_stride(), capacity);
        self.input_history = ColumnarHistory::new(self.input_schema.row_stride(), capacity);
        self.ledger = FreshnessLedger::with_capacity(capacity);
        self.memo = MemoRing::with_capacity(capacity * 2);
        self.auth_rows = None;
        self.latest_state_tick = -1;
        self.latest_auth_sim_tick = 0;
        self.latest_remote_input_tick = -1;
        self.pending_state = None;
        self.pending_display = None;

        self.base_mut().update_configuration_warnings();
    }

    // ------------------------------------------------------------------
    // pub(crate) phase API, driven by OrbitNet
    // ------------------------------------------------------------------

    /// Stable entity id (0 = unresolved).
    pub(crate) fn entity_id(&self) -> u64 {
        self.entity_id
    }

    /// Adopt the process-wide history depth. Registration hands this over AFTER the node resolved
    /// (resolution built the rings from the node-local default), so a changed limit must rebuild
    /// the rings in place — registration precedes any session traffic for the entity, so the drop
    /// loses nothing.
    pub(crate) fn set_history_limit(&mut self, limit: usize) {
        let limit = limit.max(2);
        if limit == self.history_limit {
            return;
        }
        self.history_limit = limit;
        self.state_history = ColumnarHistory::new(self.state_schema.row_stride(), limit);
        self.input_history = ColumnarHistory::new(self.input_schema.row_stride(), limit);
        self.ledger = FreshnessLedger::with_capacity(limit);
        self.memo = MemoRing::with_capacity(limit * 2);
        self.auth_rows = None;
        self.latest_state_tick = -1;
        self.latest_auth_sim_tick = 0;
        self.latest_remote_input_tick = -1;
        self.pending_state = None;
        self.pending_display = None;
    }

    /// Whether the local peer authors this entity's input.
    pub(crate) fn owns_input(&self) -> bool {
        self.input_local
    }

    /// Display-only exemption toggle, driven by the `net.remote_resim` lever.
    pub(crate) fn set_display_exempt(&mut self, exempt: bool) {
        self.exempt = exempt;
    }

    /// The peer id that owns this entity's input node — the anti-forgery check for received
    /// input frames.
    ///
    /// Read LIVE, every time, and not from the cache below it. This one answers "may this sender write this
    /// body's input", which is a security question; a cache is a window in which the answer can be wrong, and the
    /// saving is one engine call per received input block rather than per entity per peer per tick.
    pub(crate) fn input_owner_peer(&self) -> i32 {
        self.resolved_input_root()
            .map(|n| n.get_multiplayer_authority())
            .unwrap_or(0)
    }

    /// The input owner as last resolved by `process_authority` — the SEND path's copy.
    ///
    /// The send path uses it for two things, both about send ORDER: which body anchors a peer's interest radius,
    /// and which body gets the ownership weight floor. A stale value there costs a slightly wrong priority for a
    /// tick, never a wrong authority decision — which is why this is cached and [`Self::input_owner_peer`] is
    /// not. The first implementation asked the LIVE question once per entity per peer per tick; at 100 peers
    /// that is a hundredfold multiplier on an answer no peer's identity changes.
    pub(crate) fn input_owner_hint(&self) -> i32 {
        self.input_owner
    }

    /// Whether the local peer owns this entity's state (is the simulating server).
    pub(crate) fn owns_state(&self) -> bool {
        self.state_local
    }

    /// The declared send-rota priority, clamped into the range the scorer accepts.
    pub(crate) fn send_priority(&self) -> u32 {
        self.priority
            .clamp(1, orbitnet_core::priority::PRIORITY_MAX as i32) as u32
    }

    /// The declared seat, clamped into the range the interest pass keys an anchor on.
    ///
    /// A negative value is a game writing its own "unset" into an `int` export, and it reads as
    /// seat `0` — the same fail-onto-the-default direction the rest of these declarations take.
    /// The cost of getting it wrong is one connection's two viewpoints sharing an anchor, which is
    /// the behaviour that predates seats, not a body deleted from somebody's world.
    pub(crate) fn seat_hint(&self) -> SeatIndex {
        self.seat.clamp(0, i32::from(SeatIndex::MAX)) as SeatIndex
    }

    /// Whether this peer simulates the entity in the rollback loop.
    pub(crate) fn simulates(&self) -> bool {
        !self.exempt && (self.state_local || (self.input_local && self.enable_prediction))
    }

    /// Whether prediction of remote entities is allowed when un-exempted (`net.remote_resim`).
    pub(crate) fn predicts_remotely(&self) -> bool {
        !self.exempt && !self.state_local && !self.input_local && self.enable_prediction
    }

    /// This entity's frontier position, decoded from the first Vec3 State-role property of its
    /// newest state row, so register position FIRST. `None` when the entity has no
    /// positional prop or no recorded row yet — the AOI filter then keeps it always-replicated.
    pub(crate) fn position_hint(&self) -> Option<[f32; 3]> {
        let prop = self
            .state_schema
            .props()
            .iter()
            .find(|p| p.kind == PropKind::Vec3 && p.role == PropRole::State)?;
        let tick = self.state_history.latest_tick()?;
        let row = self.state_history.row(tick)?;
        let o = prop.offset;
        if o + 12 > row.len() {
            return None;
        }
        let f = |i: usize| f32::from_le_bytes([row[i], row[i + 1], row[i + 2], row[i + 3]]);
        Some([f(o), f(o + 4), f(o + 8)])
    }

    /// The world this body is in, read live from `membership_property`.
    ///
    /// [`MEMBERSHIP_GLOBAL`] when the export is unset, did not resolve, or its node has been freed —
    /// which is every rollback body in a game that declares no worlds, and is filtered on distance
    /// alone.
    pub(crate) fn membership_hint(&self) -> MembershipId {
        read_membership(self.membership.as_ref())
    }

    /// Whether `lane` captures through a bulk hook — the gate on the staging pass, so an entity
    /// that declared none never costs one.
    pub(crate) fn has_capture_hook(&self, lane: i64) -> bool {
        match lane {
            LANE_INPUT => self.input_capture_hook.is_some(),
            _ => self.state_capture_hook.is_some(),
        }
    }

    /// Stage this entity's bulk **capture** calls, to be run once every `bind` is dropped.
    ///
    /// Appended to `out` rather than returned, so one reusable `Vec` carries the whole frame's
    /// calls — the same shape phase 2 uses for `_rollback_tick`.
    pub(crate) fn stage_capture(&mut self, lane: i64, out: &mut Vec<binding::HookCall>) {
        let hook = match lane {
            LANE_INPUT => self.input_capture_hook.as_ref(),
            _ => self.state_capture_hook.as_ref(),
        };
        let staged = hook.and_then(binding::BulkHook::stage);
        let armed = staged.is_some();
        if let Some(call) = staged {
            out.push(call);
        }
        match lane {
            LANE_INPUT => self.input_capture_staged = armed,
            _ => self.state_capture_staged = armed,
        }
    }

    /// Capture the locally-authored input for `tick` into history.
    ///
    /// Reads the bulk hook's array when [`Self::stage_capture`] armed one this frame, and walks the
    /// properties otherwise — including when the hook handed back an unusable array, which is why
    /// the walk is never removed.
    pub(crate) fn capture_local_input(&mut self, tick: u64) {
        if self.input_bindings.is_empty() {
            return;
        }
        self.scratch_input.resize(self.input_schema.row_stride(), 0);
        let bulked = self.input_capture_staged
            && match self.input_capture_hook.as_mut() {
                Some(hook) => binding::capture_row_from_hook(
                    hook,
                    &self.input_bindings,
                    &mut self.scratch_input,
                ),
                None => false,
            };
        self.input_capture_staged = false;
        if !bulked {
            binding::capture_row(&self.input_bindings, &mut self.scratch_input);
        }
        // Only stamp fresh authority if the row is novel OR the tick is new: re-capturing the
        // same tick (paused frame) must not re-arm freshness.
        if self.input_history.row(tick) != Some(self.scratch_input.as_slice())
            || !self.input_history.has(tick)
        {
            self.input_history.write_row(tick, &self.scratch_input);
            self.ledger.set_confidence(tick, Confidence::Authoritative);
        }
    }

    /// Wire stride of one input row (quantized properties shrink it below the native stride).
    pub(crate) fn input_wire_stride(&self) -> usize {
        orbitnet_core::quant::wire_row_stride(self.input_schema.props())
    }

    /// Encode this entity's input block (newest rows, redundancy-armored) as standalone bytes.
    pub(crate) fn encode_input_block_bytes(
        &self,
        frame_tick: u64,
        redundancy: usize,
    ) -> Option<Vec<u8>> {
        let (newest, rows) = self.input_rows_for_send(redundancy)?;
        let row_refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        let mut writer = Writer::new();
        orbitnet_core::codec::encode_input_block(
            &mut writer,
            self.input_schema.props(),
            self.entity_id,
            frame_tick,
            newest,
            &row_refs,
        );
        Some(writer.into_inner())
    }

    /// Collect this entity's newest input rows for the wire, newest first.
    pub(crate) fn input_rows_for_send(&self, redundancy: usize) -> Option<(u64, Vec<Vec<u8>>)> {
        let newest = self.input_history.latest_tick()?;
        let mut rows = Vec::with_capacity(redundancy);
        for offset in 0..redundancy as u64 {
            let Some(tick) = newest.checked_sub(offset) else {
                break;
            };
            match self.input_history.row(tick) {
                Some(row) => rows.push(row.to_vec()),
                None => break,
            }
        }
        if rows.is_empty() {
            None
        } else {
            Some((newest, rows))
        }
    }

    /// Integrate one received input row (server side). Returns the tick if it was novel and in
    /// the past — the caller marks the resim planner with it.
    /// Decode one WIRE input row and integrate it (server side).
    pub(crate) fn integrate_remote_wire_row(&mut self, tick: u64, wire: &[u8]) -> Option<u64> {
        if wire.len() != self.input_wire_stride() {
            return None;
        }
        let mut native = std::mem::take(&mut self.scratch_wire);
        native.clear();
        native.resize(self.input_schema.row_stride(), 0);
        let decoded =
            orbitnet_core::quant::decode_row(self.input_schema.props(), wire, &mut native);
        let result = if decoded.is_some() {
            self.integrate_remote_input(tick, &native)
        } else {
            None
        };
        self.scratch_wire = native;
        result
    }

    pub(crate) fn integrate_remote_input(&mut self, tick: u64, row: &[u8]) -> Option<u64> {
        if row.len() != self.input_schema.row_stride() || self.input_history.is_stale(tick) {
            return None;
        }
        let novel = self.input_history.row(tick) != Some(row);
        if !novel {
            return None;
        }
        self.input_history.write_row(tick, row);
        self.ledger.set_confidence(tick, Confidence::Authoritative);
        self.latest_remote_input_tick = self
            .latest_remote_input_tick
            .max(i64::try_from(tick).unwrap_or(i64::MAX));
        Some(tick)
    }

    /// Encode this entity's state block for one peer.
    ///
    /// `reference_tick` is the last tick this peer applied (delta base) — `None` forces full.
    /// Returns the entity tick the block describes and whether it went out as a full row, which is
    /// what the keyframe clock is measured against.
    pub(crate) fn encode_block(
        &mut self,
        writer: &mut Writer,
        scratch: &mut Vec<bool>,
        frame_tick: u64,
        reference_tick: Option<u64>,
    ) -> Option<(u64, bool)> {
        let tick = if self.latest_auth_sim_tick > 0 {
            self.latest_auth_sim_tick
        } else {
            frame_tick
        };
        let row = self.state_history.row(tick)?.to_vec();
        let reference = reference_tick.and_then(|ref_tick| {
            self.state_history
                .row(ref_tick)
                .map(|base| (ref_tick, base.to_vec()))
        });
        let full = match reference {
            Some((ref_tick, base)) => encode_state_block(
                writer,
                scratch,
                self.state_schema.props(),
                self.entity_id,
                frame_tick,
                tick,
                Some((ref_tick, &base)),
                &row,
                false,
            ),
            None => encode_state_block(
                writer,
                scratch,
                self.state_schema.props(),
                self.entity_id,
                frame_tick,
                tick,
                None,
                &row,
                false,
            ),
        };
        Some((tick, full))
    }

    /// Keep a decoded wire row as a future delta base.
    ///
    /// Called for every row that decodes, including one the simulation discards as superseded: the
    /// receiver acknowledged the frame it rode in, so the sender may name its tick. The ring
    /// refuses a tick already outside its window.
    fn keep_auth_row(&mut self, tick: u64, row: &[u8]) {
        let stride = self.state_schema.row_stride();
        let capacity = self.history_limit.max(2);
        let rows = self
            .auth_rows
            .get_or_insert_with(|| ColumnarHistory::new(stride, capacity));
        rows.write_row(tick, row);
    }

    /// The wire row this receiver decoded for `tick`, if it still holds one.
    fn auth_row(&self, tick: u64) -> Option<&[u8]> {
        self.auth_rows.as_ref().and_then(|rows| rows.row(tick))
    }

    /// Decode a received state block into this entity, returning what to do about it.
    pub(crate) fn apply_state_block(
        &mut self,
        reader: &mut Reader<'_>,
        meta: &StateBlockMeta,
        scratch: &mut Vec<bool>,
        current_tick: u64,
    ) -> Result<StateIntegration, orbitnet_core::CodecError> {
        self.scratch_state.resize(self.state_schema.row_stride(), 0);
        let mut out = std::mem::take(&mut self.scratch_state);
        // The base comes from `auth_rows`, never from `state_history`: the owner's prediction
        // rewrites the latter. A missing base resolves to `NoBase`, which NACKs for a full row.
        let base = meta
            .reference_tick
            .and_then(|t| self.auth_row(t).map(<[u8]>::to_vec));
        let applied = decode_state_block_into(
            reader,
            meta,
            self.state_schema.props(),
            scratch,
            base.as_deref(),
            &mut out,
        )?;
        let result = if !applied {
            StateIntegration::NoBase
        } else {
            // Recorded BEFORE the integration decides what to do with it, because that decision is
            // about the simulation and this record is not.
            self.keep_auth_row(meta.tick, &out);
            self.integrate_authoritative_row(meta.tick, &out, current_tick)
        };
        self.scratch_state = out;
        Ok(result)
    }

    fn integrate_authoritative_row(
        &mut self,
        tick: u64,
        row: &[u8],
        current_tick: u64,
    ) -> StateIntegration {
        let tick_i = i64::try_from(tick).unwrap_or(i64::MAX);
        if tick_i <= self.latest_state_tick && self.latest_state_tick >= 0 {
            // Out-of-order or duplicate snapshot; the newer one already integrated.
            return StateIntegration::Stale;
        }
        self.latest_state_tick = tick_i;

        if !self.simulates() && !self.predicts_remotely() {
            // Display path: hold the newest row for the next tick boundary and keep it in history
            // so `restore_tick` has a pose to draw from. The delta base is the `auth_rows` copy
            // written above, so nothing here needs to survive a replay.
            self.state_history.write_row(tick, row);
            self.pending_display = Some((tick, row.to_vec()));
            return StateIntegration::Buffered;
        }

        // A REMOTELY PREDICTED BODY RECONCILES; it does not merely display.
        //
        // `predicts_remotely()` is what `net.remote_resim` turns on: the entity is un-exempted, joins the
        // rollback loop, and is simulated forward every tick even though this peer owns neither its state nor
        // its input. Guarding only on `simulates()` sent it down the display path anyway, which buffers the row
        // for the next tick boundary and returns `Buffered` -- never `Mispredict`, so the planner is never
        // marked and the loop never replays from the authoritative tick. The row was then overwritten by the
        // very next `restore_tick`, which reads the body's own recorded prediction.
        //
        // The result was a body that predicted forward from its own drift and NEVER RE-BASED on anything the
        // server said, for the whole session, with no error anywhere. That contradicts what the lever
        // documents itself as doing -- "predicts remote bodies forward from their latest authoritative state"
        // -- and what the comment below has always claimed. An inputless shared body (a puck, a ball, a
        // physics prop) makes it obvious within seconds; a remote player body hides it, because its own owner's
        // corrections keep the pose roughly plausible.
        //
        // Bodies that are exempt are unaffected, and exempt is the default: `remote_resim` is off unless a game
        // asks for it, so no existing configuration changes behaviour here.
        // Predicting path (owner reconcile, or the un-exempted remote-resim mode).
        if self.state_history.is_stale(tick) {
            // Older than the rollback ring can hold. A full block for the same tick would be just
            // as unusable, so this must not raise a NACK either.
            return StateIntegration::Stale;
        }
        let mispredicted = match self.state_history.row(tick) {
            Some(recorded) => {
                // Compare only resim-triggering (State-role) props: a cosmetic difference must
                // not cost a resimulation.
                let mut differs = false;
                for prop in self.state_schema.props() {
                    if !prop.role.triggers_resim() {
                        continue;
                    }
                    let end = prop.offset + prop.kind.stride();
                    if recorded.get(prop.offset..end) != row.get(prop.offset..end) {
                        differs = true;
                        break;
                    }
                }
                differs
            }
            None => true,
        };
        self.state_history.write_row(tick, row);
        if !mispredicted {
            return StateIntegration::Confirmed;
        }
        if tick >= current_tick {
            // The forward simulation will restore this row when it reaches the tick.
            return StateIntegration::Buffered;
        }
        StateIntegration::Mispredict(tick)
    }

    /// Apply the newest buffered display row (called at the tick-batch boundary).
    pub(crate) fn apply_pending_display(&mut self) {
        if let Some((_, row)) = self.pending_display.take() {
            binding::apply_row(&self.state_bindings, &row, false);
        }
    }

    /// Restore state + input for a tick about to be (re)simulated.
    ///
    /// A lane with a bulk restore hook decodes its row into the hook's array HERE, with the
    /// synchronizer bound, and appends the call to `out`; the caller runs it with every `bind`
    /// dropped. A lane without one walks its properties as before.
    pub(crate) fn restore_tick(&mut self, tick: u64, out: &mut Vec<binding::HookCall>) {
        if let Some(row) = self.state_history.row(tick) {
            match self.state_restore_hook.as_mut() {
                Some(hook) => {
                    if let Some(call) =
                        binding::stage_restore_from_row(hook, &self.state_bindings, row)
                    {
                        out.push(call);
                    }
                }
                None => binding::apply_row(&self.state_bindings, row, true),
            }
        }
        if !self.input_bindings.is_empty() {
            if let Some((input_tick, row)) = self.input_history.closest_at_or_before(tick) {
                match self.input_restore_hook.as_mut() {
                    Some(hook) => {
                        if let Some(call) =
                            binding::stage_restore_from_row(hook, &self.input_bindings, row)
                        {
                            out.push(call);
                        }
                    }
                    None => binding::apply_row(&self.input_bindings, row, true),
                }
                if input_tick != tick {
                    self.ledger.set_confidence(tick, Confidence::Extrapolated);
                }
            }
        }
    }

    /// Consume freshness for a tick about to be simulated, and update the prediction flag.
    pub(crate) fn begin_sim(&mut self, tick: u64) -> bool {
        let fresh = self.ledger.begin_sim(tick);
        self.predicted_last = self.ledger.confidence(tick) != Confidence::Authoritative;
        fresh
    }

    /// The nodes whose `_rollback_tick` runs for this entity.
    pub(crate) fn call_list(&self) -> Vec<Gd<Node>> {
        self.rollback_nodes
            .iter()
            .filter(|n| n.is_instance_valid())
            .cloned()
            .collect()
    }

    /// Capture the post-simulation state of `tick` into history at `tick + 1`.
    ///
    /// Reads the bulk hook's array when [`Self::stage_capture`] armed one for this tick, and walks
    /// the properties otherwise. Either way the row that lands is the same bytes: the hook supplies
    /// `Variant`s, and the encode, the offsets and the canonicalization below are unchanged.
    pub(crate) fn record_tick(&mut self, simulated_tick: u64) {
        let next = simulated_tick + 1;
        self.scratch_state.resize(self.state_schema.row_stride(), 0);
        let mut row = std::mem::take(&mut self.scratch_state);
        let bulked = self.state_capture_staged
            && match self.state_capture_hook.as_mut() {
                Some(hook) => binding::capture_row_from_hook(hook, &self.state_bindings, &mut row),
                None => false,
            };
        self.state_capture_staged = false;
        if !bulked {
            binding::capture_row(&self.state_bindings, &mut row);
        }
        self.state_history.write_row(next, &row);
        // Quantized-state write-back: forward simulation must continue from the canonical
        // (wire-representable) value the row holds, or replay-from-row would diverge from the
        // forward pass on every peer.
        binding::apply_quantized_row(&self.state_bindings, &row);
        self.scratch_state = row;
        if self.owns_state() && self.ledger.confidence(simulated_tick) == Confidence::Authoritative
        {
            self.latest_auth_sim_tick = self.latest_auth_sim_tick.max(next);
            self.latest_state_tick = self
                .latest_state_tick
                .max(i64::try_from(next).unwrap_or(i64::MAX));
        }
    }

    /// Server fallback: an entity with no input props (or the host's own) is always
    /// authoritative; make its frontier broadcastable each tick.
    pub(crate) fn mark_inputless_authoritative(&mut self, tick: u64) {
        if self.input_bindings.is_empty() && self.owns_state() {
            self.ledger.set_confidence(tick, Confidence::Authoritative);
        }
    }

    /// **The gap policy: an entity whose input owner is no longer connected is held on the NEUTRAL input
    /// row, and its state frontier keeps advancing.** Server-side, called once per tick for as long as the
    /// owner is away; [`OrbitNet::mark_forward_ticks`] decides when that is.
    ///
    /// Both halves are corrections, and each fixes something the default did wrong.
    ///
    /// **The neutral row replaces a carry-forward.** [`Self::restore_tick`] applies the closest input row at
    /// or before the tick, so with nobody sending, the departed player's last row was re-applied on every
    /// tick the ring could still reach — a body that walks into a wall keeps walking into it — and past the
    /// ring, nothing was written at all and the input node simply kept the last values anyone had put there.
    /// An all-zero row is written at the tick instead, so the body acts on no intent rather than on stale
    /// intent. Zero is the neutral value because it is the one the codec defines: an input schema's row is
    /// zero before anything fills it, which is the row every peer already agrees on.
    ///
    /// **Marking the tick authoritative unfreezes the broadcast.** [`Self::record_tick`] raises
    /// `latest_auth_sim_tick` only on an authoritative tick and [`Self::encode_block`] sends the row at that
    /// tick, so an orphan whose input never arrives kept simulating on the server while every other peer
    /// held it at the last tick a received row backed — and the moment its owner came back, the broadcast
    /// jumped forward in one step. The server IS the author now, so the tick is authoritative and says so.
    ///
    /// An entity with no input bindings is untouched: [`Self::mark_inputless_authoritative`] already covers
    /// it, and it has no owner to lose.
    pub(crate) fn mark_orphaned_authoritative(&mut self, tick: u64) {
        if self.input_bindings.is_empty() || !self.owns_state() {
            return;
        }
        // A fresh zero row rather than `scratch_input`: that buffer is the local-capture path's, and it
        // holds a real captured row. One small allocation per orphaned entity per tick is the right trade
        // for not sharing a buffer between "what this peer authored" and "what nobody authored".
        let neutral = vec![0u8; self.input_schema.row_stride()];
        self.input_history.write_row(tick, &neutral);
        self.ledger.set_confidence(tick, Confidence::Authoritative);
    }

    /// Reset all runtime state (session teardown).
    pub(crate) fn reset_session(&mut self) {
        self.state_history.clear();
        self.input_history.clear();
        self.ledger.clear();
        self.memo.clear();
        if let Some(rows) = self.auth_rows.as_mut() {
            rows.clear();
        }
        self.latest_state_tick = -1;
        self.latest_auth_sim_tick = 0;
        self.latest_remote_input_tick = -1;
        self.pending_state = None;
        self.pending_display = None;
        self.predicted_last = false;
    }
}

/// Server-broadcast state with no rollback restore — the OrbitNet StateSync lane.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct OrbitStateSynchronizer {
    base: Base<Node>,

    /// Node the declared property paths resolve against. Defaults to this node's parent.
    #[export]
    root: Option<Gd<Node>>,

    /// Replicated entries, each `"NodePath:property"` or a bare `"property"`.
    #[export]
    properties: PackedStringArray,

    /// Interest relevancy — the policy, one of three:
    ///
    /// | value | distance | membership |
    /// |---|---|---|
    /// | [`RELEVANCY_ALWAYS`] (0, default) | never culled | every world |
    /// | [`RELEVANCY_ANCHORED`] (1) | culled from `anchor_property` | `membership_property`'s world |
    /// | [`RELEVANCY_MEMBERSHIP`] (2) | never culled | `membership_property`'s world |
    ///
    /// Defaults to ALWAYS, which is the behaviour every state channel had before interest applied here — the lane
    /// was not culled at all. A channel only becomes cullable when it *also* names a resolvable
    /// `anchor_property` (or, for MEMBERSHIP, a resolvable `membership_property`), so declaring
    /// relevancy without one is inert rather than a way to accidentally delete a body from
    /// somebody's world.
    ///
    /// MEMBERSHIP is the setting for a channel that replicates **no position** — health, inventory,
    /// a door's state. It has no distance to be culled by, so before this value existed its only
    /// lever was all-or-nothing and it reached every peer in every world.
    #[export]
    relevancy: i32,

    /// The world-space interest anchor, as a `"NodePath:property"` entry resolved against `root`.
    ///
    /// **Explicitly named, never inferred.** The obvious heuristic — "the first Vec3 the channel
    /// replicates" — is actively wrong on this lane: a health channel's first Vec3 is as likely to be
    /// a local-space impact offset, and an environment channel's an acceleration vector.
    /// Binning either of those would park every one of those channels at the world origin and cull
    /// it for everybody. The entry need **not** be one of `properties` — it costs no wire bytes and
    /// is read live on the authority, which is the only peer that computes relevancy — so a channel
    /// whose root is a plain `Node` can point at an ancestor's `global_position`.
    #[export]
    anchor_property: GString,

    /// The world this channel belongs to, as a `"NodePath:property"` entry naming an **int**,
    /// resolved against `root`. See `OrbitRollbackSynchronizer::membership_property`.
    ///
    /// Read only under [`RELEVANCY_ANCHORED`] and [`RELEVANCY_MEMBERSHIP`]. Under
    /// [`RELEVANCY_ALWAYS`] the channel is session-global by declaration and this export is inert
    /// (a warning says so at resolve time rather than leaving it to be discovered).
    #[export]
    membership_property: GString,

    /// Send-rota priority, `1..=16`. See `OrbitRollbackSynchronizer::priority`.
    #[export]
    priority: i32,

    /// Bulk **capture** hook: the name of a game method that fills this channel's whole row in one
    /// script-boundary crossing, or empty for the per-property walk.
    ///
    /// Signature: `func <name>(lane: int, values: Array) -> void`, `lane` always [`LANE_STATE`] —
    /// this lane has only one, and the argument is there so a game can point both synchronizers at
    /// the same method. Fill every slot in the order [`Self::bulk_capture_order`] publishes.
    ///
    /// **No restore hook, deliberately.** This lane's apply is the receive path: it runs once per
    /// received block rather than once per replayed tick, so there is no replay multiplier for a
    /// hook to divide. Capture runs once per tick per owned channel on the authority, and the
    /// property count is what makes it worth removing — the history-depth note above sizes a fat
    /// channel at 41 `i64` props.
    #[export]
    bulk_capture_method: GString,

    entity_id: u64,
    schema: SchemaBuilder,
    bindings: Vec<PropBinding>,
    unresolved: PackedStringArray,
    state_local: bool,
    /// The resolved `bulk_capture_method`, or `None` when the channel keeps the per-property walk.
    capture_hook: Option<binding::BulkHook>,
    /// Whether the last staged bulk capture is the one [`Self::capture_frontier`] should read.
    capture_staged: bool,
    /// The resolved `anchor_property`, or `None` when unset, unresolvable or not a `Vector3`.
    anchor: Option<(Gd<Node>, StringName)>,
    /// The resolved `membership_property`, or `None` when unset, unresolvable or not an int.
    membership: Option<(Gd<Node>, StringName)>,
    /// The authority's captured frontier row, and each receiver's newest applied tick/row.
    history: ColumnarHistory,
    latest_tick: i64,
    pending: Option<(u64, Vec<u8>)>,
    scratch: Vec<u8>,
}

#[godot_api]
impl INode for OrbitStateSynchronizer {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            root: None,
            properties: PackedStringArray::new(),
            relevancy: RELEVANCY_ALWAYS,
            anchor_property: GString::new(),
            membership_property: GString::new(),
            priority: 1,
            bulk_capture_method: GString::new(),
            entity_id: 0,
            schema: SchemaBuilder::new(),
            bindings: Vec::new(),
            unresolved: PackedStringArray::new(),
            state_local: false,
            capture_hook: None,
            capture_staged: false,
            anchor: None,
            membership: None,
            history: ColumnarHistory::new(0, 1),
            latest_tick: -1,
            pending: None,
            scratch: Vec::new(),
        }
    }

    fn exit_tree(&mut self) {
        let me = self.to_gd().instance_id_unchecked();
        crate::orbit_net::unregister_entity(self.entity_id, me);
    }
}

#[godot_api]
impl OrbitStateSynchronizer {
    /// Add a replicated property, in the [`OrbitRollbackSynchronizer::add_state`] form.
    #[func]
    fn add_state(&mut self, node: Variant, property: GString) {
        let Some(root) = self.resolved_root() else {
            return;
        };
        let prop = property.to_string();
        let entry = if let Ok(target) = node.try_to::<Gd<Node>>() {
            let path = root.get_path_to(&target).to_string();
            if path.is_empty() {
                return;
            }
            GString::from(format!("{path}:{prop}").as_str())
        } else {
            let path = node.to_string();
            if path.is_empty() || path == "." {
                GString::from(format!(".:{prop}").as_str())
            } else {
                GString::from(format!("{path}:{prop}").as_str())
            }
        };
        if !self.properties.as_slice().contains(&entry) {
            self.properties.push(&entry);
        }
    }

    /// Resolve the declared properties and register with the OrbitNet singleton.
    #[func]
    fn process_settings(&mut self) {
        self.schema = SchemaBuilder::new();
        self.bindings.clear();
        self.unresolved = PackedStringArray::new();
        self.capture_hook = None;
        self.capture_staged = false;

        let root = self.resolved_root();
        binding::resolve_entries(
            root.as_ref(),
            &self.properties,
            PropRole::State,
            &mut self.schema,
            &mut self.bindings,
            &mut self.unresolved,
        );
        // After the schema, because the slot list is the schema's own order.
        let label = format!("OrbitStateSynchronizer {}", self.base().get_path());
        let capture = self.bulk_capture_method.clone();
        let target = binding::hook_target(root.as_ref(), &capture, &label);
        let slots: Vec<usize> = (0..self.bindings.len()).collect();
        self.capture_hook =
            binding::resolve_hook(target.as_ref(), &capture, LANE_STATE, slots, &label);
        if let Some(root) = &root {
            if root.is_inside_tree() {
                let path = root.get_path().to_string();
                self.entity_id = binding::fnv64(format!("S|{path}").as_bytes());
            }
        }
        self.resolve_anchor(root.as_ref());
        self.resolve_membership_declaration(root.as_ref());
        let local_peer = self
            .base()
            .get_multiplayer()
            .map(|m| m.clone().get_unique_id())
            .unwrap_or(1);
        self.state_local = root
            .map(|n| n.get_multiplayer_authority() == local_peer)
            .unwrap_or(false);
        self.history = ColumnarHistory::new(self.schema.row_stride(), STATE_HISTORY_DEPTH);
        self.latest_tick = -1;
        self.pending = None;

        if self.entity_id != 0 {
            crate::orbit_net::register_state_entity(self.entity_id, self.to_gd());
        }
    }

    /// This entity's stable replication id, as an opaque token.
    ///
    /// It is what `OrbitNet::set_peer_anchor_entity` names, and it is the ONLY reason it is
    /// published: nothing else in the API takes one. Derived from the root's scene path, so it is
    /// the same number on every peer, and it is `0` until `process_settings` has resolved a root
    /// that is inside the tree.
    ///
    /// **A token, not a quantity.** It is a 64-bit FNV hash reinterpreted as a signed integer, so it
    /// is routinely negative and comparing two of them for order means nothing — the same hash whose
    /// arbitrary ordering picks a peer's inferred observer. Pass it back unmodified.
    #[func]
    fn get_entity_id(&self) -> i64 {
        self.entity_id as i64
    }

    /// Hash of the resolved schema.
    #[func]
    fn schema_hash(&self) -> i64 {
        i64::from(self.schema.hash())
    }

    /// Declared entries that could not be resolved.
    #[func]
    fn unresolved_properties(&self) -> PackedStringArray {
        self.unresolved.clone()
    }

    /// The declared entries the bulk capture hook marshals, in the order its array carries them.
    /// Empty when the channel has no hook. `lane` is accepted and ignored — this lane has only one.
    #[func]
    fn bulk_capture_order(&self, lane: i64) -> PackedStringArray {
        let _ = lane;
        hook_order(&self.capture_hook, self.schema.props())
    }

    /// Whether this channel captures through a bulk hook rather than the per-property walk.
    #[func]
    fn uses_bulk_capture(&self, lane: i64) -> bool {
        let _ = lane;
        self.capture_hook.is_some()
    }

    /// The tick of the newest authoritative row this channel has (-1 before any).
    ///
    /// The twin of `OrbitRollbackSynchronizer::get_last_known_state`, and the client half of the
    /// S7: interest culling stops the updates but never removes the node, so a client that wants to
    /// stop drawing a frozen body has to notice for itself that the rows stopped. This is how it
    /// notices, and it covers packet loss and server stalls at no extra cost — which a leave
    /// message on the wire, itself droppable, would not.
    #[func]
    fn get_last_known_state(&self) -> i64 {
        self.latest_tick
    }

    /// Whether this channel declares a resolvable interest anchor (diagnostics and tests).
    #[func]
    fn is_anchored(&self) -> bool {
        self.relevancy == RELEVANCY_ANCHORED && self.anchor.is_some()
    }

    /// The world this channel is currently in, `0` meaning every world (diagnostics and tests).
    ///
    /// Reports what the filter would read this tick, so a channel whose `membership_property` did
    /// not resolve, or whose relevancy leaves the declaration inert, reports `0` rather than the
    /// value the game wrote.
    #[func]
    fn get_membership(&self) -> i64 {
        self.membership_hint() as i64
    }

    fn resolved_root(&self) -> Option<Gd<Node>> {
        self.root.clone().or_else(|| self.base().get_parent())
    }

    /// Resolve `anchor_property` into a live `(node, property)` pair, or report why it could not be.
    ///
    /// Every failure path leaves `anchor` as `None`, which means ALWAYS relevant — the fail-open
    /// direction. A misconfigured anchor costs bandwidth; a silently-wrong one deletes a body from
    /// somebody's world, which is the failure that must never be quiet.
    fn resolve_anchor(&mut self, root: Option<&Gd<Node>>) {
        self.anchor = None;
        let entry = self.anchor_property.to_string();
        if entry.is_empty() {
            if self.relevancy == RELEVANCY_ANCHORED {
                godot_warn!(
                    "OrbitStateSynchronizer {}: relevancy is ANCHORED but anchor_property is empty \
                     — this channel stays always-relevant.",
                    self.base().get_path()
                );
            }
            return;
        }
        match root.and_then(|r| binding::resolve_entry(r, &entry)) {
            Some((target, name, PropKind::Vec3)) => self.anchor = Some((target, name)),
            Some((_, _, kind)) => godot_error!(
                "OrbitStateSynchronizer {}: anchor_property {entry:?} resolved to {kind:?}, not a \
                 Vector3. An interest anchor must be a world-space position; this channel stays \
                 always-relevant rather than being culled against something that is not one.",
                self.base().get_path()
            ),
            None => godot_error!(
                "OrbitStateSynchronizer {}: anchor_property {entry:?} did not resolve against the \
                 root — this channel stays always-relevant.",
                self.base().get_path()
            ),
        }
    }

    /// Resolve `membership_property` into a live `(node, property)` pair, or report why it could not
    /// be.
    ///
    /// Skipped entirely under [`RELEVANCY_ALWAYS`], where the channel is session-global by
    /// declaration. Setting both is a contradiction, so it warns instead of silently picking one.
    fn resolve_membership_declaration(&mut self, root: Option<&Gd<Node>>) {
        self.membership = None;
        if self.relevancy == RELEVANCY_ALWAYS {
            if !self.membership_property.is_empty() {
                godot_warn!(
                    "OrbitStateSynchronizer {}: relevancy is ALWAYS, so membership_property is \
                     inert — this channel reaches every peer in every world. Set relevancy to \
                     MEMBERSHIP (2) or ANCHORED (1) to bound it to one.",
                    self.base().get_path()
                );
            }
            return;
        }
        if self.relevancy == RELEVANCY_MEMBERSHIP && self.membership_property.is_empty() {
            godot_warn!(
                "OrbitStateSynchronizer {}: relevancy is MEMBERSHIP but membership_property is \
                 empty — this channel stays in every world.",
                self.base().get_path()
            );
            return;
        }
        let label = format!("OrbitStateSynchronizer {}", self.base().get_path());
        self.membership = resolve_membership(root, &self.membership_property, &label);
    }

    // ------------------------------------------------------------------
    // pub(crate) phase API, driven by OrbitNet
    // ------------------------------------------------------------------

    /// Whether the local peer owns (and therefore broadcasts) this entity.
    pub(crate) fn owns_state(&self) -> bool {
        self.state_local
    }

    /// The declared send-rota priority, clamped into the range the scorer accepts.
    pub(crate) fn send_priority(&self) -> u32 {
        self.priority
            .clamp(1, orbitnet_core::priority::PRIORITY_MAX as i32) as u32
    }

    /// This channel's world-space interest anchor, read live from `anchor_property`.
    ///
    /// `None` — meaning always-relevant — when the channel declares ALWAYS, when the anchor did not
    /// resolve, or when the node behind it has been freed. Only the authority calls this, and the
    /// authority owns the value, so reading it live is both correct and cheaper than replicating a
    /// position the wire does not otherwise need.
    pub(crate) fn position_hint(&self) -> Option<[f32; 3]> {
        if self.relevancy != RELEVANCY_ANCHORED {
            return None;
        }
        let (node, name) = self.anchor.as_ref()?;
        let node = crate::orbit_net::live_handle(node)?;
        let value = node.get(name).try_to::<Vector3>().ok()?;
        Some([value.x, value.y, value.z])
    }

    /// The world this channel is in, read live from `membership_property`.
    ///
    /// [`MEMBERSHIP_GLOBAL`] under [`RELEVANCY_ALWAYS`] — the declaration that this channel
    /// describes the session rather than a place in it — and whenever the property is unset, did not
    /// resolve, or its node has been freed.
    ///
    /// Independent of [`Self::position_hint`]: a channel with no anchor still has a world, which is
    /// the whole of [`RELEVANCY_MEMBERSHIP`].
    pub(crate) fn membership_hint(&self) -> MembershipId {
        if self.relevancy == RELEVANCY_ALWAYS {
            return MEMBERSHIP_GLOBAL;
        }
        read_membership(self.membership.as_ref())
    }

    /// Stage this channel's bulk capture call, to be run once every `bind` is dropped.
    pub(crate) fn stage_capture(&mut self, out: &mut Vec<binding::HookCall>) {
        let staged = self
            .capture_hook
            .as_ref()
            .and_then(binding::BulkHook::stage);
        self.capture_staged = staged.is_some();
        if let Some(call) = staged {
            out.push(call);
        }
    }

    /// Capture the live values as the authority's frontier row for `tick`.
    ///
    /// Reads the bulk hook's array when [`Self::stage_capture`] armed one, and walks the properties
    /// otherwise. The row that lands is the same bytes either way.
    pub(crate) fn capture_frontier(&mut self, tick: u64) {
        self.scratch.resize(self.schema.row_stride(), 0);
        let mut row = std::mem::take(&mut self.scratch);
        let bulked = self.capture_staged
            && match self.capture_hook.as_mut() {
                Some(hook) => binding::capture_row_from_hook(hook, &self.bindings, &mut row),
                None => false,
            };
        self.capture_staged = false;
        if !bulked {
            binding::capture_row(&self.bindings, &mut row);
        }
        self.history.write_row(tick, &row);
        self.latest_tick = self
            .latest_tick
            .max(i64::try_from(tick).unwrap_or(i64::MAX));
        self.scratch = row;
    }

    /// Encode this entity's block for one peer (state lane flag set).
    pub(crate) fn encode_block(
        &mut self,
        writer: &mut Writer,
        scratch: &mut Vec<bool>,
        frame_tick: u64,
        reference_tick: Option<u64>,
    ) -> Option<(u64, bool)> {
        let tick = u64::try_from(self.latest_tick).ok()?;
        let row = self.history.row(tick)?.to_vec();
        let reference =
            reference_tick.and_then(|t| self.history.row(t).map(|base| (t, base.to_vec())));
        let full = match reference {
            Some((ref_tick, base)) => encode_state_block(
                writer,
                scratch,
                self.schema.props(),
                self.entity_id,
                frame_tick,
                tick,
                Some((ref_tick, &base)),
                &row,
                true,
            ),
            None => encode_state_block(
                writer,
                scratch,
                self.schema.props(),
                self.entity_id,
                frame_tick,
                tick,
                None,
                &row,
                true,
            ),
        };
        Some((tick, full))
    }

    /// Decode a received block; the row is buffered and applied at the next tick boundary.
    pub(crate) fn apply_state_block(
        &mut self,
        reader: &mut Reader<'_>,
        meta: &StateBlockMeta,
        scratch: &mut Vec<bool>,
    ) -> Result<StateIntegration, orbitnet_core::CodecError> {
        self.scratch.resize(self.schema.row_stride(), 0);
        let mut out = std::mem::take(&mut self.scratch);
        let base = meta
            .reference_tick
            .and_then(|t| self.history.row(t).map(<[u8]>::to_vec));
        let applied = decode_state_block_into(
            reader,
            meta,
            self.schema.props(),
            scratch,
            base.as_deref(),
            &mut out,
        )?;
        let result = if !applied {
            StateIntegration::NoBase
        } else {
            let tick_i = i64::try_from(meta.tick).unwrap_or(i64::MAX);
            if tick_i <= self.latest_tick {
                // A row too old to apply is still kept, because the sender promotes `acked_base`
                // off the frame ack and the next masked delta may name this tick as its base.
                // Dropping it breaks the chain: that delta and every later one names a base the
                // receiver no longer holds. `write_row` refuses a tick outside the ring, so this
                // cannot clobber a newer slot, and `latest_tick` still names the newest row.
                self.history.write_row(meta.tick, &out);
                StateIntegration::Stale
            } else {
                self.latest_tick = tick_i;
                self.history.write_row(meta.tick, &out);
                self.pending = Some((meta.tick, out.clone()));
                StateIntegration::Buffered
            }
        };
        self.scratch = out;
        Ok(result)
    }

    /// Apply the newest buffered row (called at the tick-batch boundary).
    pub(crate) fn apply_pending(&mut self) {
        if let Some((_, row)) = self.pending.take() {
            binding::apply_row(&self.bindings, &row, false);
        }
    }

    /// Drop all session-scoped state so a node that outlives the session works in the next one
    /// (mirrors [`OrbitRollbackSynchronizer::reset_session`]): the new session's ticks restart
    /// near 0, so a stale `latest_tick` would reject every incoming block forever.
    pub(crate) fn reset_session(&mut self) {
        self.history.clear();
        self.latest_tick = -1;
        self.pending = None;
    }
}
