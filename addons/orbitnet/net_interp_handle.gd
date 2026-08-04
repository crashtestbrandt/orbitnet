extends RefCounted
class_name NetInterpolatorHandle
## Opaque handle around the backend's render interpolator, created by orbitnet/net.gd. Presentation code
## smooths a state-lane entity's replicated properties between net ticks THROUGH this handle, so it never
## names the backend (the net-check gate). The wrapped interpolator is held as a plain Node.
##
## WHY THIS EXISTS. Every other lane already had a typed handle -- [NetRollbackHandle], [NetStateHandle] --
## and [method Net.make_interpolator] was the one hole: it returned a bare `Node`, so the only way to reach
## `add_property()` was an untyped method call on a `Node`. Under the typed-GDScript rules this addon is
## written to (and which any project that promotes `unsafe_method_access` to an error inherits), that is a
## compile ERROR, not a warning -- so the interpolator was unreachable from a strictly-typed consumer
## without an `@warning_ignore`. The RTS demo is what surfaced it: every unit it replicates lives on the
## state lane at a 20 Hz net tick, which is exactly the configuration interpolation exists for.
##
## WHEN TO USE IT. Rollback-lane bodies do NOT want this: under the coupled path the net tick IS the physics
## tick, the body writes its pose every physics tick, and Godot's own physics interpolation renders it -- an
## interpolator would fight it. Reach for this on non-rollback replicated objects, and whenever the net tick
## runs slower than the frame rate (see [method Net.set_net_tick_decoupled]), where without it a replicated
## entity visibly steps at the net rate.
##
## LOCAL ONLY. Nothing here touches the wire. The interpolator rotates the declared properties' recorded
## values at each tick boundary and blends between them every frame at the tick clock's sub-tick factor.
## Feed it the SAME properties the state lane replicates.
##
## OFFLINE / no interpolator: _interp is null and every method no-ops, so callers wire the same code path
## whether or not networking is live -- offline, the replicated props are simply written directly and stick.

var _interp: Node = null   # the backend interpolator node (created + owned by orbitnet/net.gd), or null OFFLINE

func _init(interp: Node) -> void:
	_interp = interp

## Whether a real interpolator backs this handle (false OFFLINE / inert).
func is_active() -> bool:
	return _interp != null

## Register a property to interpolate. `node` is the object owning the property; `property` its name.
## Call [method process_settings] once the set is complete.
##
## Only value types the backend knows how to blend are smoothed -- float, Vector2/3, Quaternion,
## Transform3D. Anything else (an int, a bool, a PackedArray) is applied as a step function at the tick
## boundary, which is the correct behaviour for a discrete value and is NOT an error.
func add_property(node: Object, property: String) -> void:
	if _interp != null:
		_interp.add_property(node, property)

## Re-read the interpolator's configuration after its property set changes. Must be called once after the
## [method add_property] calls, exactly like the other lanes' handles.
func process_settings() -> void:
	if _interp != null:
		_interp.process_settings()

## Snap both interpolation endpoints to the live values, so the next frames do NOT smooth across the jump.
## Call this after any discontinuity the sim intends -- a spawn, a respawn, a teleport, a world rebuild.
## Without it a unit that respawns across the map visibly flies there over one net tick.
func teleport() -> void:
	if _interp != null:
		_interp.teleport()

## Whether interpolation is currently running. False when inert.
func is_enabled() -> bool:
	if _interp == null:
		return false
	var on: bool = _interp.enabled
	return on

## Turn interpolation on/off live. Off leaves the replicated values exactly as the wire delivered them, which
## is what makes the difference visible: a demo can bind this to a key and watch entities step at the net
## tick. Inert when there is no interpolator.
func set_enabled(on: bool) -> void:
	if _interp != null:
		_interp.enabled = on
