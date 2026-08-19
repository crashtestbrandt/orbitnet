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
