extends RefCounted
class_name PuckPhysics
## The puck, as pure functions over table space. No node, no tree, no PhysicsServer.
##
## THIS IS THE FUNCTION EVERY PEER RUNS. The server runs it to produce the authoritative puck; every client
## runs the identical call to predict it, and again on every replayed tick when a correction arrives. It takes
## plain data -- a state, two parallel arrays of mallet poses, a dt -- so the whole of it is exercisable from a
## unit test with no session, which is the only practical way to be confident two processes agree.
##
## NO PHYSICS SERVER, and not as a preference. The rollback loop REPLAYS a tick; Godot's physics server cannot
## be rewound and re-stepped, so a puck driven by it would resimulate to a different answer than the one it
## recorded, on every correction, forever.
##
## SUBSTEPS. At HockeyConfig.PUCK_MAX_SPEED a single 60 Hz step moves the puck 100 mm -- more than its 64 mm
## diameter -- so a one-shot overlap test would let a fast puck pass straight through a mallet. That is a
## SIMULATION bug that looks exactly like a netcode bug, which is why the substep count is derived from the
## speed cap rather than picked.

## The puck's simulation state. One value, so a caller cannot pair this tick's position with last tick's
## velocity -- which under rollback is not a hypothetical mistake but the default one.
class State extends RefCounted:
	var position: Vector3 = Vector3.ZERO
	var velocity: Vector3 = Vector3.ZERO
	## How many rail or mallet contacts this step resolved.
	##
	## Carried out of the simulation because the VIEW needs it. A correction arriving from the server is
	## absorbed by blending the puck toward its new position; a bounce is a discontinuity the simulation
	## intended, and blending that one would render the puck passing through the rail and sliding back. The sim
	## is the only thing that knows which just happened, so it says.
	var contacts: int = 0

	func _init(at: Vector3, moving: Vector3, touched: int = 0) -> void:
		position = at
		velocity = moving
		contacts = touched

## Advance the puck one net tick against the mallets in `mallet_positions` / `mallet_velocities` (parallel
## arrays, OCCUPIED mallets only -- a vacant seat's mallet is not on the table as far as the puck is
## concerned).
##
## The puck is NOT contained at a goal mouth: it travels straight through and the caller reads
## [method TableGeometry.scoring_team_at] off the result. Reflecting it there and reporting a goal separately
## would be two rules that have to agree about the same edge.
static func step(state: State, mallet_positions: PackedVector3Array, mallet_velocities: PackedVector3Array,
		dt: float) -> State:
	if dt <= 0.0:
		return State.new(state.position, state.velocity)
	var substeps: int = maxi(1, HockeyConfig.PUCK_SUBSTEPS)
	var h: float = dt / float(substeps)
	var current: State = State.new(state.position, state.velocity)
	var contacts: int = 0
	for _index: int in substeps:
		# pow(damping, h) per substep is exactly pow(damping, dt) across the tick, so the substep count
		# changes the collision resolution and nothing else about how the puck decays.
		var slowed: Vector3 = cap_speed(current.velocity * pow(HockeyConfig.PUCK_DAMPING, h))
		current = State.new(current.position + slowed * h, slowed)
		current = resolve_mallets(current, mallet_positions, mallet_velocities)
		contacts += current.contacts
		current = resolve_rails(current)
		contacts += current.contacts
	return State.new(current.position, current.velocity, contacts)

## Separate the puck from every mallet it overlaps and bounce it off them.
##
## The mallet is treated as INFINITELY MASSIVE -- it is a hand, not a free body -- so the puck's velocity is
## reflected in the mallet's frame and the mallet's own motion is added back. `MALLET_TRANSFER` above 1.0 adds
## extra drive along the contact normal, which is what makes a rally sustain instead of decaying to a stop.
static func resolve_mallets(state: State, mallet_positions: PackedVector3Array,
		mallet_velocities: PackedVector3Array) -> State:
	var count: int = mini(mallet_positions.size(), mallet_velocities.size())
	var contact: float = HockeyConfig.PUCK_RADIUS + HockeyConfig.MALLET_RADIUS
	var position: Vector3 = state.position
	var velocity: Vector3 = state.velocity
	var contacts: int = 0
	for index: int in count:
		var mallet: Vector3 = mallet_positions[index]
		var offset: Vector3 = Vector3(position.x - mallet.x, 0.0, position.z - mallet.z)
		var distance: float = offset.length()
		if distance >= contact:
			continue
		# Exactly concentric has no contact normal. Bounce the puck back the way it came, and along +z when it
		# is not moving either -- an arbitrary answer, but the SAME arbitrary answer on every peer, which is
		# the only property that matters here.
		var normal: Vector3 = Vector3(0.0, 0.0, 1.0)
		if distance > 0.000001:
			normal = offset / distance
		elif velocity.length() > 0.000001:
			normal = -velocity.normalized()
		position = mallet + normal * contact
		var driver: Vector3 = mallet_velocities[index]
		var relative: Vector3 = velocity - driver
		var approach: float = relative.dot(normal)
		if approach < 0.0:
			relative -= normal * approach * (1.0 + HockeyConfig.MALLET_RESTITUTION)
		velocity = relative + driver
		var drive: float = maxf(0.0, driver.dot(normal))
		velocity += normal * drive * (HockeyConfig.MALLET_TRANSFER - 1.0)
		velocity = cap_speed(velocity)
		contacts += 1
	return State.new(TableGeometry.flatten(position), TableGeometry.flatten(velocity), contacts)

## Bounce the puck off the side rails, and off an end rail EXCEPT across a goal mouth.
static func resolve_rails(state: State) -> State:
	var position: Vector3 = state.position
	var velocity: Vector3 = state.velocity
	var limit_x: float = maxf(0.0, HockeyConfig.HALF_WIDTH - HockeyConfig.PUCK_RADIUS)
	var limit_z: float = maxf(0.0, HockeyConfig.HALF_LENGTH - HockeyConfig.PUCK_RADIUS)
	var contacts: int = 0
	if absf(position.x) > limit_x:
		position.x = clampf(position.x, -limit_x, limit_x)
		velocity.x = -velocity.x * HockeyConfig.RAIL_RESTITUTION
		contacts += 1
	if absf(position.z) > limit_z and not TableGeometry.is_in_goal_mouth(position.x):
		position.z = clampf(position.z, -limit_z, limit_z)
		velocity.z = -velocity.z * HockeyConfig.RAIL_RESTITUTION
		contacts += 1
	return State.new(TableGeometry.flatten(position), TableGeometry.flatten(velocity), contacts)

## The velocity a face-off serves the puck at, toward the end `to_team` defends.
##
## Deterministic in `sequence` and free of any RNG, so a client PREDICTING the face-off serves the puck exactly
## as the server does. A random serve would mispredict every restart by the full serve speed, which is the
## largest correction the demo could possibly manufacture for itself.
static func serve_velocity(to_team: int, sequence: int) -> Vector3:
	# A cheap integer hash folded into 0..1. Knuth's multiplicative constant; GDScript ints are signed 64-bit
	# and the product of a 16-bit sequence with it cannot overflow, so the modulo is exact.
	var mixed: int = absi((sequence * 2654435761) % 1024)
	var spread: float = (float(mixed) / 1024.0) * 2.0 - 1.0
	var angle: float = spread * HockeyConfig.SERVE_SPREAD_RAD
	var forward: float = HockeyConfig.end_sign(to_team)
	return Vector3(sin(angle) * HockeyConfig.SERVE_SPEED, 0.0,
		cos(angle) * HockeyConfig.SERVE_SPEED * forward)

## Whether the puck is slow enough to count as dead. The serve validator's precondition, and the reason this
## demo needs no token bucket on its command channel.
static func is_at_rest(velocity: Vector3) -> bool:
	return velocity.length() <= HockeyConfig.PUCK_REST_SPEED

## Clamp a velocity to HockeyConfig.PUCK_MAX_SPEED. Public because it is what makes the substep count
## sufficient, so a test that changes the speed cap should be reading the same function the sim does.
static func cap_speed(velocity: Vector3) -> Vector3:
	var speed: float = velocity.length()
	if speed <= HockeyConfig.PUCK_MAX_SPEED or speed <= 0.0:
		return velocity
	return velocity * (HockeyConfig.PUCK_MAX_SPEED / speed)
