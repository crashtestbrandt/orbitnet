extends RefCounted
class_name NetRollbackHandle
## Opaque handle around the vendored rollback synchronizer, created by orbitnet/net.gd. Game code drives owner
## prediction + reconciliation THROUGH this handle so it never names the backend (the net-check gate). The
## wrapped synchronizer is held as a plain Node; method calls onto it are method NAMES (not backend symbols), so
## this file stays clear of the facade boundary.
##
## OFFLINE / no synchronizer: _sync is null and every method no-ops, so callers wire the same code path whether
## or not networking is live.

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
## THIS ALSO SETS THE OWNING PEER'S OWN WORLD. A peer's world is read off the body that anchors its interest
## radius -- the lowest-id body whose input authority is that peer. That body's membership is the world every
## other entity is then filtered against for that peer, so declaring it on player bodies is what makes the
## feature work at all; declaring it only on scenery filters nothing. A peer with no rollback body has no
## anchor and no world, and sees every world (the same fail-open as its radius).
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

## Re-read the synchronizer's configuration after its state/input sets change (the backend re-resolves its schema here).
func process_settings() -> void:
	if _sync != null:
		_sync.process_settings()

## Re-evaluate which peer owns prediction for this body after an authority change.
func process_authority() -> void:
	if _sync != null:
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
