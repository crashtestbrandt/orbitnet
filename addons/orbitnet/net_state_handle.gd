extends RefCounted
class_name NetStateHandle
## Opaque handle around the vendored state synchronizer, created by orbitnet/net.gd. Server-authoritative, non-
## predicted state (remote avatars, replicated events) is driven THROUGH this handle so game code never names the
## backend (the net-check gate). The wrapped synchronizer is held as a plain Node.
##
## OFFLINE / no synchronizer: _sync is null and every method no-ops.

var _sync: Node = null   # the backend state synchronizer node (created + owned by orbitnet/net.gd), or null OFFLINE
# Whether the loaded cdylib can answer last_known_state(). Resolved ONCE: the answer cannot change within a
# process, and the question was being asked once per replicated body per render frame -- a ClassDB method-bind
# lookup, which is exactly the per-frame engine chatter CLAUDE.md names as the net-probe's timing hazard.
var _reports_last_state: bool = false

func _init(sync: Node) -> void:
	_sync = sync
	_reports_last_state = sync != null and sync.has_method(&"get_last_known_state")

## Whether a real synchronizer backs this handle (false OFFLINE / inert).
func is_active() -> bool:
	return _sync != null

## Register a replicated state property on a node.
##
## WIRE QUANTIZATION. `property` may carry an `@` suffix asking the backend to narrow the value on the wire.
## The property itself is unchanged in GDScript -- only its wire encoding is:
##
##   add_state(unit, "position@half")   # Vector3: 12 B -> 6 B  (three IEEE-754 binary16 components)
##   add_state(unit, "basis@ss3")       # Quaternion/Basis: smallest-three, 16 B -> 8 B
##   add_state(unit, "hp")              # no suffix: lossless
##
## `@half` is valid for Vector3, Vector2 and f32 ONLY; `@ss3` for Quaternion and Basis ONLY. An invalid
## (quantizer, type) pairing does NOT error -- it SILENTLY falls back to lossless, so a suffix that looks
## like it is saving bytes may be saving none. That matters most for scalars, because of the next paragraph.
##
## A GDScript `float` is an f64 and a GDScript `int` is an i64 -- that is what the language actually stores,
## and the backend records them at full width deliberately, since narrowing a float here would round every
## replayed value and quietly break a bit-exact resimulation. There is therefore NO way to narrow a bare
## scalar from GDScript: `"hp@half"` is an f64 on the wire, 8 bytes, exactly as if the suffix were absent.
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
## The client half of interest culling: it stops the updates but never removes the node -- no spawner
## carries a spatial visibility filter -- so a client that wants to stop drawing a body frozen at its last pose
## has to notice for itself that the rows stopped. Compare against `Net.current_tick()`.
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

## Re-read the synchronizer's configuration after its state set changes.
func process_settings() -> void:
	if _sync != null:
		_sync.process_settings()

## This channel's stable replication id (0 when inert, or before process_settings() resolves a root inside the
## tree). See [method NetRollbackHandle.entity_id] -- same token, same caveat that it is a hash and not a
## quantity, and the same single consumer in [method Net.set_peer_anchor_entity].
func entity_id() -> int:
	if _sync == null or not _sync.has_method(&"get_entity_id"):
		return 0
	return _sync.get_entity_id()
