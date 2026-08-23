extends RefCounted
class_name FighterMotion
## How a fighter moves. Pure: plain data in, plain data out, no nodes and no physics server.
##
## PURE BECAUSE THE ROLLBACK LOOP REPLAYS IT. A tick is restored and re-simulated, potentially many times in
## one frame, and Godot's physics server cannot be rewound and re-stepped -- a body whose motion came out of
## it would resimulate to a different answer than the one it recorded. Integrating by hand is what makes the
## replay deterministic, and it is also what makes this file a unit test rather than a session.
##
## THE INTENT IS CLAMPED, NOT TRUSTED. The backend checks WHO wrote an input row, never what is in it: a row
## that decodes at the right stride is stored as-is. So a client sending a move vector of length 40 would move
## forty times as fast unless somebody clamps it, and the only place that can is inside the tick.

class State extends RefCounted:
	var position: Vector3 = Vector3.ZERO
	var velocity: Vector3 = Vector3.ZERO

	func _init(at: Vector3 = Vector3.ZERO, vel: Vector3 = Vector3.ZERO) -> void:
		position = at
		velocity = vel

## One step, in ARENA-LOCAL coordinates. `intent` is the replicated move vector, in the fighter's own frame,
## nominally within the unit disc.
static func step(state: State, intent: Vector3, dt: float) -> State:
	var wish: Vector3 = clamp_intent(intent) * ArenaConfig.MOVE_SPEED
	var velocity: Vector3 = state.velocity.move_toward(wish, ArenaConfig.MOVE_ACCEL * dt)
	velocity *= 1.0 - clampf(ArenaConfig.MOVE_DAMPING * dt * float(ArenaConfig.NET_TICK_HZ), 0.0, 1.0)
	var moved: Vector3 = ArenaGeometry.clamp_local(state.position + velocity * dt)
	# Clamping the POSITION without clamping the VELOCITY leaves a fighter pressed into a wall accumulating
	# speed it cannot spend, which it then spends all at once the moment it turns around.
	if not is_equal_approx(moved.x, state.position.x + velocity.x * dt):
		velocity.x = 0.0
	if not is_equal_approx(moved.z, state.position.z + velocity.z * dt):
		velocity.z = 0.0
	return State.new(moved, velocity)

## A dead fighter's step: parked, and re-asserted EVERY tick rather than once. The rollback lane restores
## recorded history onto these properties, so a single write from outside the tick would be undone by the next
## restore.
static func park(home: Vector3) -> State:
	return State.new(home, Vector3.ZERO)

## The move intent, flattened to the floor plane and bounded to the unit disc.
static func clamp_intent(intent: Vector3) -> Vector3:
	var flat: Vector3 = Vector3(intent.x, 0.0, intent.z)
	if not flat.is_finite():
		return Vector3.ZERO
	return flat if flat.length_squared() <= 1.0 else flat.normalized()

## The aim direction, normalized and flattened. A zero or non-finite aim answers +z rather than a zero vector:
## a shot needs a direction, and normalizing nothing produces NaN that would reach the physics query.
static func clamp_aim(aim: Vector3) -> Vector3:
	var flat: Vector3 = Vector3(aim.x, 0.0, aim.z)
	if not flat.is_finite() or flat.length_squared() < 0.000001:
		return Vector3(0.0, 0.0, 1.0)
	return flat.normalized()

## Where a shot leaves a fighter standing at `local`: the middle of the capsule, not its feet.
static func muzzle(local: Vector3) -> Vector3:
	return local + Vector3(0.0, ArenaConfig.FIGHTER_HEIGHT * 0.5, 0.0)
