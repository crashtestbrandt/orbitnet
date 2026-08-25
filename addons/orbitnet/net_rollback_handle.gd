extends RefCounted
class_name NetRollbackHandle
## Opaque handle around the vendored rollback synchronizer, created by orbitnet/net.gd. Game code drives owner
## prediction + reconciliation THROUGH this handle so it never names the backend (the net-check gate). The
## wrapped synchronizer is held as a plain Node; method calls onto it are method NAMES (not backend symbols), so
## this file stays clear of the facade boundary.
##
## OFFLINE / no synchronizer: _sync is null and every method no-ops, so callers wire the same code path whether
## or not networking is live.

## Bulk-hook lane: this body's STATE lane -- its state entries followed by its cosmetic entries.
const LANE_STATE: int = 0
## Bulk-hook lane: this body's INPUT lane.
const LANE_INPUT: int = 1

var _sync: Node = null   # the backend rollback synchronizer node (created + owned by orbitnet/net.gd), or null OFFLINE
# Whether the loaded cdylib can answer the two receipt questions. Resolved ONCE, in _init: the answer cannot
# change within a process, and [method is_receiving] is called once per replicated body per frame -- a
# has_method() there is a ClassDB method-bind lookup, the per-frame engine chatter the facade exists to keep
# out of the hot path. [NetStateHandle] resolves its own capability flags the same way, for the same reason.
var _reports_last_received: bool = false
var _reports_authors_state: bool = false

func _init(sync: Node) -> void:
	_sync = sync
	_reports_last_received = sync != null and sync.has_method(&"get_last_received_state")
	_reports_authors_state = sync != null and sync.has_method(&"authors_state")

## Whether a real synchronizer backs this handle (false OFFLINE / inert).
func is_active() -> bool:
	return _sync != null

## Register a synchronized STATE property (e.g. the fields of the body's serialized simulation state).
##
## `property` may carry an `@half` or `@ss3` wire-quantizer suffix, on both lanes and on cosmetics too. The
## pairing table, the byte costs, and why a bare scalar cannot be narrowed at all are in the header of
## [NetStateHandle]; a suffix that is not in force is reported and listed by [method quantizer_fallbacks].
func add_state(node: Object, property: String) -> void:
	if _sync != null:
		_sync.add_state(node, property)

## Register a synchronized INPUT property (the per-tick input frame the owning client authored).
##
## Takes the same `@half` / `@ss3` suffix as [method add_state], and it pays off more here: an input frame is
## sent every tick by the owning client, where a state row is sent on the server's rota.
func add_input(node: Object, property: String) -> void:
	if _sync != null:
		_sync.add_input(node, property)

## Declare which WORLD this body belongs to, so it replicates only to peers in that world.
##
## `entry` is a `"NodePath:property"` (or bare `"property"`) resolved against the body's root, and it must name
## an **int**. It need not be one of the state properties -- it costs no wire bytes and is read live on the
## authority, the only peer that computes relevancy. `0` means every world, which is where every body starts.
##
## The problem it solves: several independent worlds inside one session, each rebased near its own coordinate
## origin, overlap in coordinates. Interest is a distance test, and two bodies at the same coordinates in
## different worlds are zero metres apart, so a radius cannot separate them.
##
## THIS ALSO SETS THE OWNING SEAT'S OWN WORLD. A seat's world is read off the body that anchors its interest
## radius -- the lowest-id body whose input authority is that peer and which declares that seat. That body's
## membership is the world every other entity is then filtered against for that seat, so declaring it on player
## bodies is what makes the feature work at all; declaring it only on scenery filters nothing. A connection
## carries the union of its seats' sets, so a peer with a body in each of two worlds sees both -- see
## [method set_seat].
##
## A SEAT WHOSE BODY HAS NOT SPAWNED YET HAS NO WORLD AND NO ANCHOR, AND CONTRIBUTES NEITHER. It is skipped
## rather than passed through as "sees everything", because the connection's set is a union and one unresolved
## seat would otherwise open the whole connection to every world until that body produced a state row. The
## fail-open is per CONNECTION: a peer with no resolved seat at all still sees everything, which is what stops
## a joining player from arriving in an empty world.
##
## UNLESS THE PEER DECLARED ITS OWN. [method Net.set_peer_anchor] states a peer's centre and world directly, and
## a peer that used it reads neither off any body -- which is the way out when a peer drives bodies in more than
## one world, or drives none at all.
##
## There is no relevancy switch on this lane and none is needed: a rollback body always carries a position, so
## it is always distance-cullable, and membership narrows that rather than replacing it.
##
## Call BEFORE process_settings(); the membership resolves with the property list. A body created through
## `Net.register_rollback_body()` has already had its settings processed, so call process_settings() after.
func set_membership(entry: String) -> void:
	if _sync != null:
		_sync.set(&"membership_property", entry)

## The world this body is currently in, `0` meaning every world (0 when inert, or on a backend too old to answer).
##
## CHECK THIS FIRST WHEN MEMBERSHIP FILTERING SEEMS TO DO NOTHING. The OWNING PEER's world is read off this
## body, so a body reporting 0 is a peer that sees every world, and every other entity's declaration is
## irrelevant for that peer. It reports what the filter would read this tick, so a `membership_property` that
## did not resolve -- or that was set after the last process_settings() -- reports 0 rather than the value the
## game wrote, which is how a misconfiguration becomes visible at all.
func membership() -> int:
	if _sync == null or not _sync.has_method(&"get_membership"):
		return 0
	return _sync.get_membership()

## Declare which SEAT on the owning connection drives this body. 0 unless you say otherwise.
##
## THIS IS THE LABEL HALF ONLY. Changing the label and the owning connection together is [method assign_seat],
## which writes both in one statement; emptying the seat is [method release_seat]. Use this alone only when the
## connection is not changing.
##
## A seat is one owned, predicted body behind one connection. Local split-screen over a network session is two
## or more locally-owned bodies on a single socket, and each needs its own interest anchor -- the second
## player's surroundings are not the first player's. The owning peer comes from the input node's multiplayer
## authority; this is the other half, and `(peer, seat)` is what the interest pass anchors on.
##
## CALL IT ON THE SERVER. Interest is computed only where state authority is, so this is read only there, and
## nothing on the wire carries a seat. The server assigns seats, so it declares them on its own copy of the
## scene; a client may leave every body at 0. The anti-forgery check on received input is per entity and is
## unaffected either way.
##
## A LABEL, NOT A SLOT INDEX. Two bodies on the same connection with the same value share one anchor (the
## lowest-id one supplies it); every distinct value is one more interest set the server maintains for that
## connection. The numbers need not be contiguous.
##
## What it costs to skip: with two bodies at the default 0, the connection gets ONE centre -- whichever body
## the id hash sorts lowest -- and the other player's surroundings are culled around a position that player is
## nowhere near. Visible only with a cull radius set.
##
## COMMANDS ARE NOT SEATED BY THIS. [NetCommand] hands its validator the sender's peer id; a game with several
## seats on one connection puts the seat in the payload and validates it against the seats it assigned. See
## the header of `net_command.gd`.
func set_seat(seat: int) -> void:
	if _sync != null:
		_sync.set(&"seat", seat)

## The seat declared for this body (0 when inert, or on a backend too old to carry one).
##
## Read as a PROPERTY rather than through a getter, and checked for type: a backend without the export answers
## `null`, which is the "too old" case rather than a value to convert.
func seat() -> int:
	if _sync == null:
		return 0
	var declared: Variant = _sync.get(&"seat")
	if typeof(declared) != TYPE_INT:
		return 0
	var index: int = declared
	return index

## SEAT THIS BODY: point its input at `peer` AND put it on that connection's seat `seat`. The add verb.
##
## Use this rather than [method set_input_authority] plus [method set_seat] whenever both change. The roster is
## derived from the pair, so writing them separately leaves a window -- the tick between the two calls -- in
## which the body reads as `(new peer, old label)`. That is a seat nobody assigned: [signal Net.seat_opened]
## announces it, the backend keeps an interest set for it, and [signal Net.seat_closed] retracts it a tick later.
##
## IT IS LOCAL AND MUST BE CALLED ON EVERY PEER, exactly as [method set_input_authority] is -- the authority half
## replicates nothing. The seat half is read only where state authority is, so a client may leave it at 0; the
## announcement reaches clients over the entity manifest rather than from this call.
##
## Announced on the NEXT TICK BOUNDARY, not inside this call, so a handler that seats another player in response
## is not doing it part-way through a tick.
##
## No-op OFFLINE. Against a backend that predates the call it falls back to the same two writes from here, in the
## order that cannot invent a seat: the label first, then the connection.
func assign_seat(peer: int, seat: int) -> void:
	if _sync == null:
		return
	if _sync.has_method(&"assign_seat"):
		_sync.assign_seat(peer, seat)
		return
	_sync.set(&"seat", seat)
	set_input_authority(peer)

## EMPTY THIS BODY'S SEAT: hand its input back to the server and return the body to seat 0. The remove verb.
##
## IT DOES NOT UNREGISTER ANYTHING. The body stays in the scene and stays replicated -- what leaves is the
## VIEWPOINT, so the connection stops carrying an interest set for it and [signal Net.seat_closed] fires.
## Freeing the body, or handing it to somebody else, is a separate decision and it is yours.
##
## Both halves for the same reason [method assign_seat] does both: releasing the connection while leaving the
## label behind means the next player seated on this body inherits a label nobody chose for them.
##
## No-op OFFLINE. Against a backend that predates the call it falls back to the same two writes from here.
func release_seat() -> void:
	if _sync == null:
		return
	if _sync.has_method(&"release_seat"):
		_sync.release_seat()
		return
	_sync.set(&"seat", 0)
	set_input_authority(1)

## Re-read the synchronizer's configuration after its state/input sets change (the backend re-resolves its schema here).
func process_settings() -> void:
	if _sync != null:
		_sync.process_settings()

## Re-evaluate which peer owns prediction for this body after an authority change.
func process_authority() -> void:
	if _sync != null:
		_sync.process_authority()

## Turn this peer's PREDICTION of this body on or off, after registration.
##
## THE ONE THING [method set_input_authority] DOES NOT RE-RESOLVE, and the reason it needs its own call.
## `predict` is an argument to [method Net.register_rollback_body] and it is read nowhere else, so a body
## registered before its owner was known stays at the answer that was true then. Re-pointing the input
## re-resolves *who owns which lane*; it leaves the prediction switch alone.
##
## The failure that produces is silent and expensive. A body registered with `predict = false` is also
## EXEMPTED from the rollback loop, so a seat handed to this connection afterwards is simulated by nobody
## here: its authoritative rows still arrive and still apply, so the body moves, the readouts look ordinary,
## and every input the player gives it is a full round trip late. Nothing errors.
##
## THE RULE: a body registered before its owner is known -- which is every body in a session whose world is
## built before the roster arrives -- calls this whenever ownership moves.
##
## [codeblock]
##     func set_owner_peer(peer: int) -> void:
##         handle.assign_seat(peer, seat)                       # the connection and the label
##         handle.set_predicted(peer == multiplayer.get_unique_id() or Net.is_server())
## [/codeblock]
##
## `on = false` returns the body to display-only and re-establishes the exemption, unless
## [method Net.remote_resim] asked this client to carry remote bodies -- the same rule registration applies,
## and the same one [method Net.set_remote_resim] re-applies live.
##
## NOT DERIVED AUTOMATICALLY FROM THE AUTHORITIES, deliberately. "This peer owns a lane of it" is the usual
## answer but not the only correct one: a body every peer predicts with nobody owning its input -- a puck, a
## ball, a shared physics prop -- is registered `predict = true` on peers that own neither lane, and deriving
## the switch would turn that off the first time anything touched its authority.
##
## Local, like every authority write: nothing here replicates, and each peer decides for itself. No-op OFFLINE.
func set_predicted(on: bool) -> void:
	if _sync == null:
		return
	_sync.set(&"enable_prediction", on)
	_sync.set(&"exempt", false if on else not Net.remote_resim())
	_sync.process_authority()

## Whether this peer is set to predict this body. False when inert, and false on a backend whose synchronizer
## does not carry the switch.
##
## NOT the same question as [method is_predicting], which asks whether the owner is currently MISPREDICTING.
## This one is the switch; that one is the reconciliation gate.
func is_predicted() -> bool:
	if _sync == null:
		return false
	var enabled: Variant = _sync.get(&"enable_prediction")
	return typeof(enabled) == TYPE_BOOL and enabled

## Point this body's INPUT at `peer`, and re-resolve everything that reads the answer. The one call a roster
## makes when this body changes hands -- a player joining, leaving, or reconnecting. `1` hands the input back
## to the server, which is what an unclaimed body means.
##
## THIS IS THE CONNECTION, NOT THE SEAT. `peer` is a transport peer id: it says WHICH CONNECTION authors this
## body's input. [method set_seat] is the other axis and is unaffected -- it says which of that connection's
## owned bodies this one is, for interest. Re-pointing a body at another connection leaves its seat index
## alone, which is right: the body is the same body, driven by somebody else.
##
## CHANGING BOTH IS [method assign_seat]. The seat roster is derived from the pair, so two separate writes are
## announced as a seat opening and closing again on the tick between them.
##
## THE TWO HALVES HAVE TO HAPPEN TOGETHER, and doing them by hand is where this goes wrong. Writing the node's
## multiplayer authority alone changes who the backend accepts input frames FROM, but leaves the cached owner
## naming the previous peer -- so this peer keeps predicting (or keeps refusing to predict) the wrong body, and
## the send path anchors the wrong peer's interest radius. That is why it is one call.
##
## IT IS LOCAL AND MUST BE CALLED ON EVERY PEER. Multiplayer authority is a property of a node on the peer that
## holds it; nothing here replicates. A peer that missed the call disagrees about who owns the body, and on the
## server that disagreement is what starts rejecting the new owner's input.
##
## No-op OFFLINE. Against a backend that predates the call it falls back to the same two steps from here, so a
## checkout pairing new GDScript with an older binary re-points the body rather than silently not.
func set_input_authority(peer: int) -> void:
	if _sync == null:
		return
	if _sync.has_method(&"set_input_authority"):
		_sync.set_input_authority(peer)
		return
	# The fallback resolves the input node the way Net.register_rollback_body() set it. `get` answers a
	# Variant, so it is ASSIGNED to a typed local rather than cast (the GDScript rule for wire-ish values).
	var input_node: Node = _sync.get(&"input_authority_node")
	if input_node != null:
		input_node.set_multiplayer_authority(peer)
	_sync.process_authority()

## Whether the owner is currently mis-predicting (the reconciliation gate). False when inert.
func is_predicting() -> bool:
	return _sync != null and _sync.is_predicting()

## The tick of the newest authoritative state this body KNOWS, from either source (-1 when inert). For netcode
## diagnostics.
##
## A FRONTIER OVER TWO SOURCES, and that is what makes it the wrong reading for "is this body still reaching
## me": it is raised both by a row decoded off the wire and by this peer's own simulation, for every body whose
## state this peer authors. On a server or a listen-server host it therefore rises every tick with no row in
## sight. Ask [method is_receiving] instead -- a cull, a membership change or a per-peer veto stops the rows
## and leaves this number climbing.
func get_last_known_state() -> int:
	return _sync.get_last_known_state() if _sync != null else -1

## The tick of the newest authoritative row this peer RECEIVED for this body. -1 when inert, -1 before the
## first row, and -1 for the whole session on the peer that authors this body's state, which receives none.
##
## One source, the wire, which is what separates it from [method get_last_known_state]. It counts a row the
## rollback ring then discarded as too old and it counts a duplicate: both prove the body is still being sent
## here, which is the question being asked.
##
## -1 IS A SENTINEL, NOT A FAIL-OPEN. A backend too old to answer reports -1 and says so through
## [method reports_last_received_state]; it does not report the present the way
## [method NetStateHandle.last_known_state] does. The fail-open lives one level up, in [method is_receiving].
## The sentinel and the fail-open are deliberately different answers to the same unknown -- a staleness rule
## degrades rather than blanking the world, while a caller quoting the tick is handed -1 rather than a number
## nothing measured.
func last_received_state() -> int:
	if _sync == null or not _reports_last_received:
		return -1
	return _sync.get_last_received_state()

## Whether THIS PEER AUTHORS this body's state, and therefore receives no rows for it. False when inert, and
## false on a backend too old to answer.
##
## The disambiguator for [method last_received_state]: on the authoring peer that reading is -1 for the whole
## session, which on its own is indistinguishable from "withheld since it spawned".
## [method is_receiving] checks this first for exactly that reason.
func authors_state() -> bool:
	if _sync == null or not _reports_authors_state:
		return false
	return _sync.authors_state()

## Whether [method last_received_state] reports a MEASURED tick rather than the sentinel a backend too old to
## answer produces. False when inert, and false on that older backend.
##
## Resolved once at construction. Check it wherever the reading is used as EVIDENCE -- a probe asserting that
## rows reach a client, a HUD claiming a body is live, a bug report quoting a tick -- because -1 alone cannot
## say which of "no row yet" and "cannot measure" produced it.
func reports_last_received_state() -> bool:
	return _reports_last_received

## Whether rows for this body are still arriving, within `within_ticks` of the current tick. THE CALL A GAME
## MAKES; the three reads above are the parts it is built from, and [NetStateHandle] carries the same four so
## one helper spans both lanes.
##
## IT FAILS OPEN -- true in every case where the answer is not known to be no:
##
## | Case | Answer |
## | --- | --- |
## | inert (OFFLINE) | true -- there is no wire to stop |
## | [method authors_state] | true -- the authority receives nothing and never will |
## | a backend too old to answer | true -- a binary mismatch degrades a rule, it never blanks the world |
## | `Net.current_tick() - last_received_state() <= within_ticks` | true -- a row landed recently enough |
## | a measuring peer that has never received a row | false -- the only known no |
##
## `within_ticks` IS A QUESTION ABOUT THE SEND ROTA, NOT ABOUT THE NETWORK. Entities are served stalest-first
## inside a per-tick byte budget, so a body far down a busy rota waits several ticks between rows with nothing
## wrong anywhere. Size the window against that rota -- the default 24 ticks is about half a second at 50 Hz --
## rather than against a round trip.
func is_receiving(within_ticks: int = 24) -> bool:
	if _sync == null:
		return true
	if authors_state():
		return true
	if not _reports_last_received:
		return true
	var tick: int = last_received_state()
	if tick < 0:
		return false
	return Net.current_tick() - tick <= within_ticks

## The tick of the newest input row in this body's ring (-1 when inert, or before any row arrives).
## On the authority for a wire-driven body this is the input lane's frontier: `tick - last_known_input()`
## is how long that lane has been silent, which is what the stale-input coast rule keys on.
func get_last_known_input() -> int:
	return _sync.get_last_known_input() if _sync != null else -1

## Record a per-tick memo value keyed (tick, key) -- the backend-owned resim log. Record on the
## is_fresh pass; every replayed pass reads the same value back, so a resim resolves against what the fresh
## pass saw (e.g. the weapon category held at the fire tick) even if live state changed since. Trimmed with
## rollback history. No-op when inert (offline reads fall through to the caller's live value).
func memo_set(tick: int, key: int, value: int) -> void:
	if _sync != null:
		_sync.memo_set(tick, key, value)

## Read a per-tick memo value recorded by memo_set, or `fallback` when none was recorded (or inert).
func memo_get(tick: int, key: int, fallback: int) -> int:
	return _sync.memo_get(tick, key, fallback) if _sync != null else fallback

## This body's stable replication id (0 when inert, or before process_settings() resolves a root inside the tree).
##
## The ONLY thing that takes one is [method Net.set_peer_anchor_entity], which is why it is published. It is a
## 64-bit hash reinterpreted as a signed integer -- routinely NEGATIVE, and meaningless to compare or order. Pass
## it back unmodified. A backend too old to answer reports 0, which that setter reads as a retraction.
func entity_id() -> int:
	if _sync == null or not _sync.has_method(&"get_entity_id"):
		return 0
	return _sync.get_entity_id()

## Declare the game method that CAPTURES a whole lane's values in one call, replacing the per-property walk.
##
## Signature: `func <method>(lane: int, values: Array) -> void`, declared on the body's ROOT (the node the
## property entries resolve against). Fill every slot of `values` in the order [method bulk_capture_order]
## publishes for that lane; the array is preallocated and reused, so a slot left alone keeps last tick's value,
## and resizing it drops the lane back to the walk with an error.
##
## What it buys: capture is one `Object.get` per property, and the rollback loop pays that PER REPLAYED TICK,
## PER BODY. This makes it one call per lane per tick. A body with 41 state props replaying 12 ticks pays 492
## property reads a frame without it and 12 calls with it. Measure it with `Net.perf_metrics()` -- `record_ms`
## is the capture half and `restore_ms` the restore half -- and `Net.set_resim_force()` fixes the replay depth
## so the comparison holds still.
##
## OPT-IN, AND THE ROW IS UNCHANGED. Declare nothing and every lane keeps the walk, byte for byte. The hook
## supplies the values and nothing else -- the encoding, the byte offsets and the wire quantization are the
## backend's, because masks, delta bases and the mispredict compare all read that layout.
##
## Call BEFORE process_settings(); the hook resolves with the property list.
func set_bulk_capture(method: String) -> void:
	if _sync != null:
		_sync.set(&"bulk_capture_method", method)

## Declare the game method that RESTORES a whole lane's values in one call. Same signature and same shape as
## [method set_bulk_capture]; read the slots and write them onto your own fields, and do not resize the array --
## a wrong-length one drops the lane back to the walk and reports it once, exactly as on the capture side.
##
## THE RESTORE ORDER IS NOT THE CAPTURE ORDER. Cosmetic entries are captured and replicated but never restored,
## so they are absent here and present there. Read [method bulk_restore_order], not the capture order, and a
## body that declares no cosmetics sees identical lists.
##
## It covers the rollback loop only. The receive apply and the quantized write-back have a direction of their
## own -- [method set_bulk_apply], whose order is the CAPTURE order.
##
## Call BEFORE process_settings().
func set_bulk_restore(method: String) -> void:
	if _sync != null:
		_sync.set(&"bulk_restore_method", method)

## Declare the game method that APPLIES a whole lane's values in one call -- the received row, and the
## quantized write-back.
##
## THE APPLY ORDER IS THE CAPTURE ORDER, NOT THE RESTORE ORDER. The two differ by exactly this body's COSMETIC
## entries, which are captured and replicated but never restored. Pass your existing restore method here on a
## body that declares cosmetics and it reads shifted slots and writes wrong values, with nothing erroring
## anywhere. Read [method bulk_apply_order]. A body that declares no cosmetics has identical lists and cannot
## hit this.
##
## Signature: `func <method>(lane: int, values: Array) -> void`, declared on the body's ROOT, reading the slots
## and writing your own fields -- the same direction [method set_bulk_restore] runs in. Do not resize the array:
## a wrong-length one drops the lane back to the walk and reports it once.
##
## WHAT IT SAVES, HONESTLY. It covers two walks and they are worth different amounts:
##
## | Walk | Crossings without / with | What multiplies it |
## | --- | --- | --- |
## | applying a RECEIVED row | `S + C` / 1 | delivered blocks this tick -- NO replay multiplier |
## | the quantized WRITE-BACK | `Q` / 1 | replayed ticks x planned bodies |
##
## The receive apply runs once per delivered block rather than once per replayed tick, so below roughly twenty
## delivered blocks a tick it is noise. What makes it worth declaring is the peer it runs on: a peer that
## SIMULATES NOTHING plans no bodies, its rollback loop returns on an empty plan, and this walk is then its
## entire per-tick crossing count with no other hook reaching it.
##
## The write-back does carry the multiplier. A body of 41 props with 8 quantized among them, replaying 12
## ticks, pays 96 property writes a frame on the walk and 12 calls through the hook. It is GATED: the hook takes
## the write-back only when the lane carries two or more quantized properties, so a lane with zero or one keeps
## the cheaper targeted walk. [method uses_bulk_apply] reports the DECLARATION, not which of the two paths ran.
##
## Only the STATE lane runs an apply hook today. The input lane resolves and publishes one so a single method
## can serve every lane, but a received input row lands in the ring and reaches the game through the restore
## direction, and that lane has no write-back.
##
## Call BEFORE process_settings().
func set_bulk_apply(method: String) -> void:
	if _sync != null:
		_sync.set(&"bulk_apply_method", method)

## The declared entries a bulk CAPTURE hook marshals for `lane`, in the order its array carries them.
##
## Empty when the lane has no hook, when the handle is inert, or on a backend too old to answer. Assert against
## it rather than inferring the order from the order you happened to register properties in -- reordering a
## registration silently reorders this.
func bulk_capture_order(lane: int) -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"bulk_capture_order"):
		return PackedStringArray()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant, and the
	# GDScript rule for a wire-ish value is that the conversion is an assignment, never a cast.
	var order: PackedStringArray = _sync.bulk_capture_order(lane)
	return order

## The declared entries a bulk RESTORE hook marshals for `lane`, in array order -- the restored subset, shorter
## than the capture order by exactly the lane's cosmetic entries. Empty when the lane has no hook.
func bulk_restore_order(lane: int) -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"bulk_restore_order"):
		return PackedStringArray()
	var order: PackedStringArray = _sync.bulk_restore_order(lane)
	return order

## Whether `lane` captures through a bulk hook rather than the per-property walk.
##
## CHECK THIS AFTER process_settings() WHEN A HOOK SEEMS TO DO NOTHING. A method name that does not resolve
## leaves the lane on the walk, and the order lists give that away only by being empty -- which an empty lane
## also is.
func uses_bulk_capture(lane: int) -> bool:
	if _sync == null or not _sync.has_method(&"uses_bulk_capture"):
		return false
	return _sync.uses_bulk_capture(lane)

## Whether `lane` restores through a bulk hook. See [method uses_bulk_capture].
func uses_bulk_restore(lane: int) -> bool:
	if _sync == null or not _sync.has_method(&"uses_bulk_restore"):
		return false
	return _sync.uses_bulk_restore(lane)

## The declared entries a bulk APPLY hook marshals for `lane`, in array order -- THE CAPTURE ORDER, cosmetics
## included, because a received row lands every entry. Assert against this and never against
## [method bulk_restore_order]: the two differ by exactly the cosmetics, and reading the wrong one shifts every
## slot after the first cosmetic entry. Empty when the lane has no hook, when the handle is inert, or on a
## backend too old to answer.
func bulk_apply_order(lane: int) -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"bulk_apply_order"):
		return PackedStringArray()
	var order: PackedStringArray = _sync.bulk_apply_order(lane)
	return order

## Whether `lane` DECLARES an apply hook. See [method uses_bulk_capture] for why to check it.
##
## It reports the declaration, not which path ran: the state lane's write-back is additionally gated on
## carrying two or more quantized properties, so a lane can answer true here and still canonicalize through the
## targeted walk while its receive apply, which is ungated, goes through the hook.
func uses_bulk_apply(lane: int) -> bool:
	if _sync == null or not _sync.has_method(&"uses_bulk_apply"):
		return false
	return _sync.uses_bulk_apply(lane)

## Every declared entry on EITHER LANE whose `@` quantizer suffix is NOT IN FORCE, verbatim. Empty is the
## healthy answer.
##
## Three ways an entry lands here, and all three ship the property wider than the entry claims: a suffix that
## is neither `@half` nor `@ss3`, a pairing the resolved type does not support (`"hp@half"` on a GDScript
## float, which is an f64), and an entry that did not resolve at all. The full pairing table and the byte
## costs are in the header of [NetStateHandle].
##
## ONE LIST FOR THE WHOLE BODY -- state, cosmetic and input entries together -- because the reading is a
## property of the body rather than of a lane, and a boot check wants the body's whole story in one call.
##
## ASSERT ON IT RATHER THAN READING THE LOG. Each of these also raises a diagnostic, but a dropped suffix is
## a BANDWIDTH bug: the game runs, the frames decode, and the only symptom is a wire wider than the property
## list says. Empty when the handle is inert, and empty on a backend too old to answer, so a boot check
## written against a newer addon than the loaded cdylib passes rather than blocking a bisect.
func quantizer_fallbacks() -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"quantizer_fallbacks"):
		return PackedStringArray()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant.
	var dropped: PackedStringArray = _sync.quantizer_fallbacks()
	return dropped
