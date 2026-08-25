extends RefCounted
class_name NetStateHandle
## Opaque handle around the vendored state synchronizer, created by orbitnet/net.gd. Server-authoritative, non-
## predicted state (remote avatars, replicated events) is driven THROUGH this handle so game code never names the
## backend (the net-check gate). The wrapped synchronizer is held as a plain Node.
##
## OFFLINE / no synchronizer: _sync is null and every method no-ops.

## Bulk-hook lane: this channel's only lane. Named so the same game method can serve a rollback body's lanes
## too -- see [constant NetRollbackHandle.LANE_STATE].
const LANE_STATE: int = 0

var _sync: Node = null   # the backend state synchronizer node (created + owned by orbitnet/net.gd), or null OFFLINE
# Whether the loaded cdylib can answer last_known_state(). Resolved ONCE: the answer cannot change within a
# process, and the question was being asked once per replicated body per render frame -- a ClassDB method-bind
# lookup, which is exactly the per-frame engine chatter CLAUDE.md names as the net-probe's timing hazard.
var _reports_last_state: bool = false
# The same once-only resolution for the two RECEIPT questions, and for the same reason: is_receiving() is
# called once per replicated channel per frame. [NetRollbackHandle] carries the identical pair.
var _reports_last_received: bool = false
var _reports_authors_state: bool = false

func _init(sync: Node) -> void:
	_sync = sync
	_reports_last_state = sync != null and sync.has_method(&"get_last_known_state")
	_reports_last_received = sync != null and sync.has_method(&"get_last_received_state")
	_reports_authors_state = sync != null and sync.has_method(&"authors_state")

## Whether a real synchronizer backs this handle (false OFFLINE / inert).
func is_active() -> bool:
	return _sync != null

## Register a replicated state property on a node.
##
## WIRE QUANTIZATION. `property` may carry an `@` suffix asking the backend to narrow the value on the wire.
## The property itself is unchanged in GDScript -- only its wire encoding is:
##
##   add_state(unit, "position@half")   # Vector3: 12 B -> 6 B  (three IEEE-754 binary16 components)
##   add_state(unit, "quaternion@ss3")  # Quaternion: 16 B -> 6 B  (smallest-three)
##   add_state(unit, "hp")              # no suffix: lossless
##
## The whole pairing table, and what each pairing costs on the wire:
##
## | Suffix | Valid for | Native -> wire |
## | --- | --- | --- |
## | `@half` | Vector3 | 12 B -> 6 B |
## | `@half` | Vector2 | 8 B -> 4 B |
## | `@half` | f32 | 4 B -> 2 B (unreachable from GDScript; see below) |
## | `@ss3` | Quaternion | 16 B -> 6 B |
## | `@ss3` | Basis | 36 B -> 6 B (rotation only -- scale and shear are discarded) |
##
## AN INVALID PAIRING, AND AN UNRECOGNIZED SUFFIX, ARE REPORTED. The backend drops the annotation, ships the
## property lossless, and raises a diagnostic naming this channel and the entry -- an ERROR in the editor and
## in any run from source, a warning in an exported build. [method quantizer_fallbacks] lists every such entry
## so a boot check can fail on it instead of a log line scrolling past.
##
## A GDScript `float` is an f64 and a GDScript `int` is an i64 -- that is what the language actually stores,
## and the backend records them at full width deliberately, since narrowing a float here would round every
## replayed value and quietly break a bit-exact resimulation. There is therefore NO way to narrow a bare
## scalar from GDScript: `"hp@half"` is an f64 on the wire, 8 bytes, and the suffix is reported and dropped.
## The idiom is to PACK scalars into a Vector3 and quantize that -- three normalized scalars in one
## `Vector3 @half` cost 6 bytes total instead of 24. The RTS demo's `net_aux` packs (cos, sin, hp01) that
## way; see docs/protocol.md.
##
## Budget context: a peer receives ONE UDP frame per tick with a ~1200-byte payload budget, and entities are
## served stalest-first. Every byte saved per entity is another entity refreshed per tick.
func add_state(node: Object, property: String) -> void:
	if _sync != null:
		_sync.add_state(node, property)

## Declare this channel's world-space interest ANCHOR, making it cullable by `net.aoi_radius`.
##
## `entry` is a `"NodePath:property"` (or bare `"property"`) resolved against the channel's root, and it must
## name a **Vector3 world position**. It does NOT have to be one of the replicated properties -- it costs no wire
## bytes and is read live on the authority, the only peer that computes relevancy -- so a channel whose root is a
## plain Node can point at an ancestor's `global_position`.
##
## Naming the anchor is the whole point: the obvious heuristic, "the first Vector3 the channel replicates", is
## actively WRONG on this lane. A health channel's first Vector3 is as likely to be a local-space impact offset,
## and an environment channel's an acceleration vector; binning either would park every one of those channels at
## the world origin and cull it for everybody. A channel that declares no anchor stays ALWAYS
## relevant -- which is what every state channel is by default -- and an anchor that fails to resolve, or is
## not a Vector3, falls back to that with an error rather than culling against a value that is not a position.
##
## Call BEFORE process_settings(); the anchor resolves with the property list.
func set_anchor(entry: String) -> void:
	if _sync != null:
		_sync.set(&"anchor_property", entry)
		_sync.set(&"relevancy", 1)   # ANCHORED; 0 = ALWAYS, the default

## Declare which WORLD this channel belongs to, so it reaches only peers in that world.
##
## `entry` is a `"NodePath:property"` (or bare `"property"`) resolved against the channel's root, and it must
## name an **int**. Like the anchor it need not be one of the replicated properties -- it costs no wire bytes
## and is read live on the authority, the only peer that computes relevancy. `0` means every world.
##
## The problem it solves: several independent worlds inside one session, each rebased near its own coordinate
## origin, overlap in coordinates. Interest is a distance test, and two entities at the same coordinates in
## different worlds are zero metres apart, so a radius cannot separate them. A peer only ever replicates
## channels whose world matches its own, whatever the radius says. A peer's own world is read off the rollback
## body that anchors its interest radius -- see [method NetRollbackHandle.set_membership].
##
## MEMBERSHIP IS WHAT A POSITIONLESS CHANNEL HAS INSTEAD OF A RADIUS. Health, inventory, a door's state: none
## of them replicate a position, so none of them can be distance-culled, and before this existed their only
## lever was all-or-nothing -- every peer in every world. Calling this on a channel with no anchor leaves it
## uncullable by distance and bounds it to one world.
##
## Composes with [method set_anchor]: call both and the channel is culled by distance WITHIN its world. Called
## alone it only sets the world. A property that does not resolve, or is not an int, leaves the channel in
## every world with an error -- the same fail-open direction the anchor takes.
##
## Call BEFORE process_settings(); the membership resolves with the property list.
func set_membership(entry: String) -> void:
	if _sync == null:
		return
	_sync.set(&"membership_property", entry)
	# An EMPTY entry declares nothing, so it must not switch the policy. Promoting on one would leave the
	# channel permanently non-ALWAYS, warning at every process_settings() about a membership_property it does
	# not have, with no way back through this handle.
	if entry.is_empty():
		return
	# Do not clobber ANCHORED: a channel with both an anchor and a world is culled by both. Only ALWAYS -- the
	# default, and the declaration "this channel describes the session, not a place in it" -- is promoted.
	#
	# TYPE-CHECKED, NOT ASSIGNED STRAIGHT TO AN `int`. `Object.get()` on a property the backend does not expose
	# returns `null`, and assigning Nil to a typed local is a hard runtime error that aborts the caller. The
	# cdylib is committed separately from this GDScript, so new addon code legitimately runs against an older
	# binary with no `relevancy` export -- the same mismatch last_known_state() guards against, and the same
	# rule: a binary mismatch degrades a declaration, it never aborts the game.
	var declared: Variant = _sync.get(&"relevancy")
	if typeof(declared) != TYPE_INT:
		return
	var relevancy: int = declared
	if relevancy == 0:               # ALWAYS
		_sync.set(&"relevancy", 2)   # MEMBERSHIP: no distance test, one world

## The world this channel is currently in, `0` meaning every world (0 OFFLINE, or on a backend too old to
## answer). Reports what the filter would read this tick, so a `membership_property` that did not resolve
## reports 0 rather than the value the game wrote -- which is how a misconfiguration is visible at all.
func membership() -> int:
	if _sync == null or not _sync.has_method(&"get_membership"):
		return 0
	return _sync.get_membership()

## Declare this channel's send-rota priority: 1..16, multiplying its distance-band weight when the
## byte budget cannot carry everything. The backend must not guess game semantics, so a channel worth more than
## an ordinary one says so here. Call before process_settings().
func set_priority(weight: int) -> void:
	if _sync != null:
		_sync.set(&"priority", clampi(weight, 1, 16))

## The tick of the newest authoritative row this channel has received (-1 before any, -1 OFFLINE).
##
## The client half of interest culling: it stops the updates but never removes the node, so a client that wants
## to stop drawing a body frozen at its last pose has to decide for itself what that means. Compare against
## `Net.current_tick()`.
##
## A THRESHOLD, WHICH IS THE RIGHT SHAPE FOR A FADE AND THE WRONG ONE FOR AN EDGE. "How stale is this" is a
## continuous quantity and a threshold is how you read one; "it stopped being sent" is a moment, and
## [signal Net.entity_left_interest] is where that moment is published. Use the signal to act once, and this
## to draw.
## A BACKEND THAT CANNOT ANSWER REPORTS THE PRESENT, so the staleness rule fails OPEN. The cdylib is committed by
## a bot in a commit separate from the Rust sources (CLAUDE.md), so new GDScript legitimately runs against an
## older binary that has no such method -- a PR branch before the bot lands, a bisect, any tree that has not run
## `just native-install`. Returning -1 there would read as "no row has ever arrived", the rule would measure from
## the body's SPAWN tick instead, and every remote body and every NPC on that peer would vanish one threshold
## after spawn and never return. A binary mismatch must degrade a diagnostic, never blank the world.
func last_known_state() -> int:
	if _sync == null:
		return -1
	if not _reports_last_state:
		return Net.current_tick()
	return _sync.get_last_known_state()

## Whether [method last_known_state] reports a MEASURED tick rather than its fail-open fallback.
##
## False means the loaded cdylib has no `get_last_known_state`, so `last_known_state()` is answering
## `Net.current_tick()` -- a value that rises on every peer whether or not a single row ever arrived. False is
## also what an inert handle reports, because there is nothing to measure.
##
## Published because the fallback is INVISIBLE in the reading. A staleness rule wants the fail-open and does not
## care; anything that treats the reading as evidence -- a probe asserting that rows reach a client, a HUD
## claiming a body is live, a bug report quoting a tick -- is measuring the fallback the moment this is false,
## and cannot tell from the number alone. Check it once at bind time and say which branch the reading came from.
func reports_last_known_state() -> bool:
	return _reports_last_state

## The tick of the newest authoritative row this peer RECEIVED for this channel. -1 when inert, -1 before the
## first row, and -1 for the whole session on the authority, which receives none.
##
## THE RECEIPT, and it is a different reading from [method last_known_state] even on this lane, where the two
## happen to agree today. That one fails open and reports the present against a backend that cannot answer;
## this one has a single source -- the wire -- and stays honest. [NetRollbackHandle] carries the same four
## methods with the same meanings, so one game helper spans both lanes; on that lane the difference is not
## cosmetic, because its `get_last_known_state()` is also raised by the authoring peer's own simulation.
##
## It counts a row that arrived too old to apply: it still proves the channel is being sent here.
##
## -1 IS A SENTINEL, NOT A FAIL-OPEN. A backend too old to answer reports -1 and says so through
## [method reports_last_received_state]. The fail-open lives one level up, in [method is_receiving]. The
## sentinel and the fail-open are deliberately different answers to the same unknown -- a staleness rule
## degrades rather than blanking the world, while a caller quoting the tick is handed -1 rather than a number
## nothing measured.
func last_received_state() -> int:
	if _sync == null or not _reports_last_received:
		return -1
	return _sync.get_last_received_state()

## Whether THIS PEER AUTHORS this channel, and therefore receives no rows for it. False when inert, and false
## on a backend too old to answer.
##
## The disambiguator for [method last_received_state]: on the authoring peer that reading is -1 for the whole
## session, which on its own is indistinguishable from "culled since it spawned". [method is_receiving] checks
## this first for exactly that reason.
func authors_state() -> bool:
	if _sync == null or not _reports_authors_state:
		return false
	return _sync.authors_state()

## Whether [method last_received_state] reports a MEASURED tick rather than the sentinel a backend too old to
## answer produces. False when inert, and false on that older backend.
##
## Resolved once at construction. Check it wherever the reading is used as EVIDENCE -- a probe asserting that
## rows reach a client, a HUD claiming a channel is live, a bug report quoting a tick -- because -1 alone
## cannot say which of "no row yet" and "cannot measure" produced it.
func reports_last_received_state() -> bool:
	return _reports_last_received

## Whether rows for this channel are still arriving, within `within_ticks` of the current tick. THE CALL A GAME
## MAKES; the three reads above are the parts it is built from.
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
## inside a per-tick byte budget, so a channel far down a busy rota waits several ticks between rows with
## nothing wrong anywhere. Size the window against that rota -- the default 24 ticks is about half a second at
## 50 Hz -- rather than against a round trip.
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

## Re-read the synchronizer's configuration after its state set changes.
func process_settings() -> void:
	if _sync != null:
		_sync.process_settings()

## Every declared entry whose `@` quantizer suffix is NOT IN FORCE, verbatim. Empty is the healthy answer.
##
## Three ways an entry lands here, and all three ship the property wider than the entry claims:
##
## | Cause | What the property does |
## | --- | --- |
## | the suffix is neither `@half` nor `@ss3` | ships lossless |
## | the pairing is invalid for the resolved type (`"hp@half"` on a GDScript float) | ships lossless |
## | the entry did not resolve at all -- a mistyped path | does not ship |
##
## ASSERT ON IT RATHER THAN READING THE LOG. Each of these also raises a diagnostic, but a dropped suffix is
## a BANDWIDTH bug: the game runs, the frames decode, and the only symptom is a wire two to six times wider
## than the property list says. Call this once after [method process_settings] in a boot check and fail CI on
## a non-empty result.
##
## Empty when the handle is inert, and empty on a backend too old to answer -- so a boot check written against
## a newer addon than the loaded cdylib PASSES rather than blocking a bisect. Both are the same fail-open the
## rest of this handle takes: a binary mismatch degrades a diagnostic, it never stops the game.
func quantizer_fallbacks() -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"quantizer_fallbacks"):
		return PackedStringArray()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant.
	var dropped: PackedStringArray = _sync.quantizer_fallbacks()
	return dropped

## This channel's stable replication id (0 when inert, or before process_settings() resolves a root inside the
## tree). See [method NetRollbackHandle.entity_id] -- same token, same caveat that it is a hash and not a
## quantity, and the same single consumer in [method Net.set_peer_anchor_entity].
func entity_id() -> int:
	if _sync == null or not _sync.has_method(&"get_entity_id"):
		return 0
	return _sync.get_entity_id()

## Declare the game method that CAPTURES this channel's whole row in one call, replacing the per-property walk.
##
## Signature: `func <method>(lane: int, values: Array) -> void`, declared on the channel's ROOT, with `lane`
## always [constant LANE_STATE]. Fill every slot of `values` in the order [method bulk_capture_order] publishes;
## the array is preallocated and reused, so a slot left alone keeps last tick's value, and resizing it drops the
## channel back to the walk with an error.
##
## What it buys: the authority captures every channel whose state it owns, once per tick, at one `Object.get`
## per property. A fat channel is 41 of them. This makes it one call.
##
## NO RESTORE HOOK ON THIS LANE, because it has no rollback restore. Its apply is the receive path, and that
## has a direction of its own -- [method set_bulk_apply].
##
## Call BEFORE process_settings(); the hook resolves with the property list.
func set_bulk_capture(method: String) -> void:
	if _sync != null:
		_sync.set(&"bulk_capture_method", method)

## Declare the game method that APPLIES a received row for this channel in one call, replacing the
## per-property walk.
##
## THE APPLY ORDER IS THE CAPTURE ORDER, NOT A RESTORE ORDER. On this lane the two are always the same list --
## every entry is replicated and applied, and there is no cosmetic role for them to differ by -- but a body's
## state lane DOES differ, so a method shared with [NetRollbackHandle] must be written against
## [method bulk_apply_order] there. See [method NetRollbackHandle.set_bulk_apply].
##
## Signature: `func <method>(lane: int, values: Array) -> void`, declared on the channel's ROOT, with `lane`
## always [constant LANE_STATE]. Read the slots and write your own fields; do not resize the array.
##
## WHAT IT SAVES, HONESTLY: one call instead of one `Object.set` per replicated property, and it runs once per
## DELIVERED BLOCK rather than once per replayed tick, so there is no replay multiplier and below roughly
## twenty delivered blocks a tick the saving is noise. Its multiplier is the number of channels a frame
## delivers, bounded by the receive byte budget rather than by the roster. It matters most on a peer that
## SIMULATES NOTHING -- a spectator's rollback loop returns on an empty plan, so this is the only property walk
## its frame runs and no other hook reaches it.
##
## Call BEFORE process_settings(); the hook resolves with the property list.
func set_bulk_apply(method: String) -> void:
	if _sync != null:
		_sync.set(&"bulk_apply_method", method)

## The declared entries the bulk capture hook marshals, in the order its array carries them. Empty when the
## channel has no hook, when the handle is inert, or on a backend too old to answer.
func bulk_capture_order() -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"bulk_capture_order"):
		return PackedStringArray()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant.
	var order: PackedStringArray = _sync.bulk_capture_order(LANE_STATE)
	return order

## Whether this channel captures through a bulk hook rather than the per-property walk. Check it after
## process_settings() when a hook seems to do nothing: a method name that does not resolve leaves the channel
## on the walk.
func uses_bulk_capture() -> bool:
	if _sync == null or not _sync.has_method(&"uses_bulk_capture"):
		return false
	return _sync.uses_bulk_capture(LANE_STATE)

## The declared entries the bulk apply hook marshals, in the order its array carries them -- the same list
## [method bulk_capture_order] publishes, since this lane has one role and no restored subset. Empty when the
## channel has no hook, when the handle is inert, or on a backend too old to answer.
func bulk_apply_order() -> PackedStringArray:
	if _sync == null or not _sync.has_method(&"bulk_apply_order"):
		return PackedStringArray()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant.
	var order: PackedStringArray = _sync.bulk_apply_order(LANE_STATE)
	return order

## Whether this channel DECLARES an apply hook for its receive path. Reports the declaration, so a method name
## that does not resolve reads false and leaves the channel on the walk.
func uses_bulk_apply() -> bool:
	if _sync == null or not _sync.has_method(&"uses_bulk_apply"):
		return false
	return _sync.uses_bulk_apply(LANE_STATE)
