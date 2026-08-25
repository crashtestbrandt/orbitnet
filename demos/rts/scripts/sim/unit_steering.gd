extends RefCounted
class_name UnitSteering
## The whole movement model for a unit, as a PURE function.
##
## No CharacterBody3D, no physics ray, no PhysicsServer, no SceneTree: a unit is a position, a velocity and a
## facing angle over a flat plane with a list of axis-aligned box obstacles. That is a deliberate constraint,
## not laziness -- it means the entire simulation step is unit-testable from plain data (see
## tests/unit/unit_steering_test.gd), so the sim can be verified without standing up a session, and the
## server's authoritative step has no hidden engine state a client could disagree about.
##
## It also removes an entire class of netcode confusion. When movement is a physics body, "why did the client
## see something different?" has two candidate answers (the network, or the physics solver). Here there is
## only one.
##
## DETERMINISM IS NOT REQUIRED and the demo does not pretend otherwise. The server is the only peer that ever
## runs this; clients receive positions. So there is no lockstep, no fixed-point arithmetic, and no
## cross-platform float discipline to maintain. See docs/rts-demo.md for why that trade is the right one for
## this architecture.

## One unit's kinematic state. Plain data, deliberately mutable-by-copy: step() returns a NEW State rather
## than mutating in place, so a test can hold the before and after side by side.
class State extends RefCounted:
	var position: Vector3 = Vector3.ZERO
	var velocity: Vector3 = Vector3.ZERO
	var facing: float = 0.0   # yaw, radians. Forward is Vector3(sin(facing), 0, cos(facing)).

	func _init(p: Vector3 = Vector3.ZERO, v: Vector3 = Vector3.ZERO, f: float = 0.0) -> void:
		position = p
		velocity = v
		facing = f

	func copy() -> State:
		return State.new(position, velocity, facing)

## Distance at which a unit starts braking for its goal. Larger looks lazier; smaller overshoots and orbits.
const ARRIVE_RADIUS: float = 2.0
## Below this speed the facing stops chasing velocity -- otherwise a stopped unit spins on numerical noise.
const FACING_EPSILON: float = 0.05

## Advance one unit by `dt`.
##
## `goal` is where it is trying to be. `obstacles` are world-space AABBs; only their X/Z extents matter (the
## plane is flat, and testing Y would make the result depend on how tall a decorative box happens to be).
## `arch` supplies speed/accel/radius/turn_rate.
##
## NON-FINITE INPUT IS ABSORBED, NOT PROPAGATED. A goal arriving from the wire can be NAN or INF -- that is a
## classic crash-and-corrupt vector, and once a NAN enters a position it spreads to every value that touches
## it and never leaves. A bad goal is treated as "no goal" (brake in place) and a state that is already
## non-finite is reset rather than stepped. The order validator rejects such payloads at the boundary too;
## this is the second line, because the boundary is not the only way a value gets here.
static func step(state: State, goal: Vector3, obstacles: Array[AABB], arch: RtsConfig.Archetype,
		dt: float) -> State:
	var out: State = state.copy()
	if not is_finite_vec(out.position):
		# Nothing sane to preserve: park it at the origin, stopped. Better a visible teleport than a
		# permanently poisoned entity that also poisons everything that reads its position.
		return State.new(Vector3.ZERO, Vector3.ZERO, 0.0)
	if not is_finite_vec(out.velocity):
		out.velocity = Vector3.ZERO
	if dt <= 0.0 or not is_finite(dt):
		return out

	var target: Vector3 = goal
	var has_goal: bool = is_finite_vec(goal)
	if not has_goal:
		target = out.position

	# --- desired velocity, with arrival braking ---------------------------------------------------
	var to_goal: Vector3 = Vector3(target.x - out.position.x, 0.0, target.z - out.position.z)
	var distance: float = to_goal.length()
	var desired: Vector3 = Vector3.ZERO
	if has_goal and distance > 0.0001:
		var speed: float = arch.max_speed
		if distance < ARRIVE_RADIUS:
			# Linear taper into the goal. Without it a unit overshoots by (max_speed * dt) every tick and
			# jitters around its destination, which reads on screen as a network problem and is not one.
			speed = arch.max_speed * (distance / ARRIVE_RADIUS)
		desired = (to_goal / distance) * speed

	out.velocity = out.velocity.move_toward(desired, arch.accel * dt)
	out.velocity.y = 0.0

	# --- integrate, then resolve --------------------------------------------------------------------
	out.position += out.velocity * dt
	out.position.y = 0.0
	out.position = resolve_obstacles(out.position, obstacles, arch.radius)
	out.position = clamp_to_field(out.position, arch.radius)

	# --- facing chases velocity ---------------------------------------------------------------------
	var planar_speed: float = Vector2(out.velocity.x, out.velocity.z).length()
	if planar_speed > FACING_EPSILON:
		var wanted: float = atan2(out.velocity.x, out.velocity.z)
		var delta: float = wrapf(wanted - out.facing, -PI, PI)
		var step_limit: float = arch.turn_rate * dt
		out.facing = wrapf(out.facing + clampf(delta, -step_limit, step_limit), -PI, PI)
	return out

## Push `position` out of any obstacle it overlaps, along the shallowest axis. `radius` inflates each box so a
## unit's body clears it rather than its center point.
##
## Shallowest-axis resolution is what makes a unit SLIDE along a wall instead of sticking to it: the component
## of motion into the wall is removed and the component along it survives. Boxes are resolved in list order;
## with a tight corner that can take two passes, which is why the loop runs twice.
static func resolve_obstacles(position: Vector3, obstacles: Array[AABB], radius: float) -> Vector3:
	var out: Vector3 = position
	for _pass: int in 2:
		for box: AABB in obstacles:
			var min_x: float = box.position.x - radius
			var max_x: float = box.position.x + box.size.x + radius
			var min_z: float = box.position.z - radius
			var max_z: float = box.position.z + box.size.z + radius
			if out.x <= min_x or out.x >= max_x or out.z <= min_z or out.z >= max_z:
				continue
			# Inside. Four candidate exits; take the cheapest.
			var left: float = out.x - min_x
			var right: float = max_x - out.x
			var back: float = out.z - min_z
			var front: float = max_z - out.z
			var smallest: float = minf(minf(left, right), minf(back, front))
			if smallest == left:
				out.x = min_x
			elif smallest == right:
				out.x = max_x
			elif smallest == back:
				out.z = min_z
			else:
				out.z = max_z
	return out

## Keep a unit inside the playable field, accounting for its radius.
static func clamp_to_field(position: Vector3, radius: float) -> Vector3:
	var limit_x: float = maxf(0.0, RtsConfig.FIELD_HALF_X - radius)
	var limit_z: float = maxf(0.0, RtsConfig.FIELD_HALF_Z - radius)
	return Vector3(clampf(position.x, -limit_x, limit_x), 0.0, clampf(position.z, -limit_z, limit_z))

## Whether every component of `v` is finite. `is_finite` is per-scalar in GDScript, so this is the missing
## vector form -- used here, by the order validator, and by the probe.
static func is_finite_vec(v: Vector3) -> bool:
	return is_finite(v.x) and is_finite(v.y) and is_finite(v.z)

## The unit forward vector for a yaw. The single definition of the facing convention; the renderer, the
## combat code and the wire packing all go through it so they cannot disagree about which way zero points.
static func forward_of(facing: float) -> Vector3:
	return Vector3(sin(facing), 0.0, cos(facing))
