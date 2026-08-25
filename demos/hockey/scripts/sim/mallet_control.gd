extends RefCounted
class_name MalletControl
## How a mallet moves, as pure functions. No node, no tree, no wire.
##
## TWO STEP FUNCTIONS, AND THE SECOND ONE IS THE INTERESTING HALF.
##
## [method step_toward] is the real simulation: chase the player's requested point, capped in speed and
## acceleration, clamped to the player's own half. It is what the SERVER runs for every mallet and what the
## OWNING CLIENT runs for its own -- both have that mallet's input.
##
## [method step_coast] is dead reckoning, for a peer that has neither. Rollback INPUT travels client -> server
## only; it is never rebroadcast to the other clients, because that would be an O(N^2) input fan-out. So a peer
## watching somebody else's mallet holds an input frame that was never written, and chasing it would send the
## mallet toward a point the player left long ago -- the further the resim, the worse. Coasting on the last
## authoritative velocity is a better model of "what is it about to do" and it degrades toward standing still
## rather than toward a wrong place.
##
## Dead reckoning is deliberately NOT the same simulation as the authority's, and that is the whole reason the
## authority keeps sending rows.

## A mallet's simulation state. A class rather than two loose Vector3s so a step returns one value and a caller
## cannot pair this tick's position with last tick's velocity.
class State extends RefCounted:
	var position: Vector3 = Vector3.ZERO
	var velocity: Vector3 = Vector3.ZERO

	func _init(at: Vector3, moving: Vector3) -> void:
		position = at
		velocity = moving

## Chase `target`, capped by HockeyConfig.MALLET_MAX_SPEED and MALLET_ACCEL, clamped to `seat`'s own half.
static func step_toward(state: State, target: Vector3, seat: int, dt: float) -> State:
	if dt <= 0.0:
		return State.new(state.position, state.velocity)
	var desired: Vector3 = (TableGeometry.flatten(target) - state.position) / dt
	desired = _cap(desired, HockeyConfig.MALLET_MAX_SPEED)
	var change: Vector3 = _cap(desired - state.velocity, HockeyConfig.MALLET_ACCEL * dt)
	var moved: Vector3 = TableGeometry.clamp_to_half(
		state.position + (state.velocity + change) * dt, seat, HockeyConfig.MALLET_RADIUS)
	return State.new(moved, _velocity_of(state.position, moved, dt))

## Coast on the last known velocity, decaying toward rest. See the header for when this is the right model.
static func step_coast(state: State, seat: int, dt: float) -> State:
	if dt <= 0.0:
		return State.new(state.position, state.velocity)
	# pow(damping, dt) rather than a per-tick multiplier: the F1 lever changes the tick rate live, and a
	# per-tick decay would make the same mallet coast twice as far at 30 Hz as at 60.
	var decayed: Vector3 = state.velocity * pow(HockeyConfig.MALLET_COAST_DAMPING, dt)
	var moved: Vector3 = TableGeometry.clamp_to_half(
		state.position + decayed * dt, seat, HockeyConfig.MALLET_RADIUS)
	return State.new(moved, _velocity_of(state.position, moved, dt))

## Park a mallet at its seat's home spot, stationary. A vacant seat runs this every tick rather than once,
## because the rollback lane restores recorded history onto these properties -- a single write from outside the
## tick would be undone by the next restore, which is the most common OrbitNet bug and raises no error.
static func park(seat: int) -> State:
	return State.new(TableGeometry.home_point(seat), Vector3.ZERO)

# --- internals -------------------------------------------------------------------------------------
static func _cap(vector: Vector3, limit: float) -> Vector3:
	var length: float = vector.length()
	if length <= limit or length <= 0.0:
		return vector
	return vector * (limit / length)

# The velocity a mallet ACTUALLY has, derived from the displacement the clamp allowed rather than from the
# velocity that was asked for. Without this, a mallet held against the center line keeps the full speed it was
# driven at while standing still -- and hands all of it to the next puck that touches it.
static func _velocity_of(from: Vector3, to: Vector3, dt: float) -> Vector3:
	return (to - from) / dt
