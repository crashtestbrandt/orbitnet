extends RefCounted
class_name NetStateHandle
## Opaque handle around the vendored state synchronizer, created by orbitnet/net.gd. Server-authoritative, non-
## predicted state (remote avatars, replicated events) is driven THROUGH this handle so game code never names the
## backend (the net-check gate). The wrapped synchronizer is held as a plain Node.
##
## OFFLINE / no synchronizer: _sync is null and every method no-ops.

var _sync: Node = null   # the backend state synchronizer node (created + owned by orbitnet/net.gd), or null OFFLINE

func _init(sync: Node) -> void:
	_sync = sync

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

## Re-read the synchronizer's configuration after its state set changes.
func process_settings() -> void:
	if _sync != null:
		_sync.process_settings()
