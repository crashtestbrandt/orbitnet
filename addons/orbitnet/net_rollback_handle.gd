extends RefCounted
class_name NetRollbackHandle
## Opaque handle around the vendored rollback synchronizer, created by orbitnet/net.gd. Game code drives owner
## prediction + reconciliation (#63) THROUGH this handle so it never names the backend (the net-check gate). The
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

## Register a synchronized STATE property (e.g. the fields of the body's serialized simulation state) -- #63.
func add_state(node: Object, property: String) -> void:
	if _sync != null:
		_sync.add_state(node, property)

## Register a synchronized INPUT property (the per-tick input frame the owning client authored) -- #63.
func add_input(node: Object, property: String) -> void:
	if _sync != null:
		_sync.add_input(node, property)

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

## Record a per-tick memo value keyed (tick, key) -- the backend-owned resim log (#103/#67). Record on the
## is_fresh pass; every replayed pass reads the same value back, so a resim resolves against what the fresh
## pass saw (e.g. the weapon category held at the fire tick) even if live state changed since. Trimmed with
## rollback history. No-op when inert (offline reads fall through to the caller's live value).
func memo_set(tick: int, key: int, value: int) -> void:
	if _sync != null:
		_sync.memo_set(tick, key, value)

## Read a per-tick memo value recorded by memo_set, or `fallback` when none was recorded (or inert).
func memo_get(tick: int, key: int, fallback: int) -> int:
	return _sync.memo_get(tick, key, fallback) if _sync != null else fallback
