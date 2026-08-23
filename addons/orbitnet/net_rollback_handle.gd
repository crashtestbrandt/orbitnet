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

func _init(sync: Node) -> void:
	_sync = sync

## Whether a real synchronizer backs this handle (false OFFLINE / inert).
func is_active() -> bool:
	return _sync != null

## Register a synchronized STATE property (e.g. the fields of the body's serialized simulation state).
func add_state(node: Object, property: String) -> void:
	if _sync != null:
		_sync.add_state(node, property)

## Register a synchronized INPUT property (the per-tick input frame the owning client authored).
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

## The tick of the latest authoritative state this body has received (-1 when inert). For netcode diagnostics.
func get_last_known_state() -> int:
	return _sync.get_last_known_state() if _sync != null else -1

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
## It covers the rollback loop only. Applying a RECEIVED row still walks the properties: that runs once per
## received block rather than once per replayed tick, and it is the one apply that must land cosmetics too.
##
## Call BEFORE process_settings().
func set_bulk_restore(method: String) -> void:
	if _sync != null:
		_sync.set(&"bulk_restore_method", method)

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
