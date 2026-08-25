extends UnitTest
## UnitSteering: the whole movement model, exercised from plain data. No scene, no physics server, no session.
##
## This suite exists in this shape because the sim was deliberately written as a pure function. If movement
## lived on a CharacterBody3D, every case below would need a live SceneTree and a PhysicsServer, would take
## seconds instead of microseconds, and the NaN cases would be untestable without provoking real corruption.

const _EPS: float = 0.0001

func _scout() -> RtsConfig.Archetype:
	return RtsConfig.archetype(RtsConfig.Kind.SCOUT)

func _at(x: float, z: float) -> UnitSteering.State:
	return UnitSteering.State.new(Vector3(x, 0.0, z), Vector3.ZERO, 0.0)

# An empty obstacle list, TYPED. A bare `[]` literal is an untyped Array, and passing one where
# `Array[AABB]` is declared is an unsafe call argument -- which this project promotes to an error.
func _no_obstacles() -> Array[AABB]:
	var none: Array[AABB] = []
	return none

func _one_box(box: AABB) -> Array[AABB]:
	var boxes: Array[AABB] = [box]
	return boxes

# --- basic motion --------------------------------------------------------------------------------
func test_moves_toward_goal() -> void:
	var state: UnitSteering.State = _at(0.0, 0.0)
	var goal: Vector3 = Vector3(20.0, 0.0, 0.0)
	var out: UnitSteering.State = UnitSteering.step(state, goal, _no_obstacles(), _scout(), 0.05)
	assert_true(out.position.x > 0.0, "a unit steps toward its goal")
	assert_almost_eq(out.position.z, 0.0, _EPS, "and does not drift off the axis it is traveling along")
	assert_almost_eq(out.position.y, 0.0, _EPS, "movement stays on the ground plane")

func test_arrives_and_settles() -> void:
	var state: UnitSteering.State = _at(0.0, 0.0)
	var goal: Vector3 = Vector3(6.0, 0.0, 0.0)
	# Twelve seconds is far longer than the trip; the point is that it STOPS rather than orbiting the goal.
	for _i: int in 240:
		state = UnitSteering.step(state, goal, _no_obstacles(), _scout(), 0.05)
	assert_almost_eq(state.position.x, goal.x, 0.25, "a unit ends up at its goal")
	assert_true(state.velocity.length() < 0.5, "and has braked, rather than circling it forever")

func test_speed_is_capped_by_the_archetype() -> void:
	var state: UnitSteering.State = _at(0.0, 0.0)
	var goal: Vector3 = Vector3(500.0, 0.0, 0.0)
	var arch: RtsConfig.Archetype = _scout()
	for _i: int in 100:
		state = UnitSteering.step(state, goal, _no_obstacles(), arch, 0.05)
	assert_true(state.velocity.length() <= arch.max_speed + 0.01,
		"velocity never exceeds the archetype's max_speed")

func test_no_goal_brakes_in_place() -> void:
	var state: UnitSteering.State = UnitSteering.State.new(Vector3.ZERO, Vector3(8.0, 0.0, 0.0), 0.0)
	# A goal at the current position means "nothing to do"; the unit must shed speed, not coast.
	for _i: int in 60:
		state = UnitSteering.step(state, state.position, _no_obstacles(), _scout(), 0.05)
	assert_true(state.velocity.length() < 0.5, "with no destination a unit comes to rest")

# --- obstacles -----------------------------------------------------------------------------------
func test_does_not_end_inside_an_obstacle() -> void:
	var box: AABB = AABB(Vector3(4.0, 0.0, -3.0), Vector3(4.0, 3.0, 6.0))
	var obstacles: Array[AABB] = _one_box(box)
	var state: UnitSteering.State = _at(0.0, 0.0)
	var goal: Vector3 = Vector3(20.0, 0.0, 0.0)
	for _i: int in 200:
		state = UnitSteering.step(state, goal, obstacles, _scout(), 0.05)
		var inside: bool = state.position.x > box.position.x and state.position.x < box.position.x + box.size.x \
			and state.position.z > box.position.z and state.position.z < box.position.z + box.size.z
		assert_false(inside, "a unit is never left inside an obstacle")

func test_resolve_pushes_out_along_the_shallowest_axis() -> void:
	# A point just inside the left face must exit LEFT, not through the far side -- that is what makes a unit
	# slide along a wall rather than teleport through it.
	var box: AABB = AABB(Vector3(0.0, 0.0, 0.0), Vector3(10.0, 3.0, 10.0))
	var out: Vector3 = UnitSteering.resolve_obstacles(Vector3(0.4, 0.0, 5.0), _one_box(box), 0.0)
	assert_almost_eq(out.x, 0.0, _EPS, "a shallow left overlap exits through the left face")
	assert_almost_eq(out.z, 5.0, _EPS, "and does not move along the axis it was not penetrating")

# --- bounds --------------------------------------------------------------------------------------
func test_clamped_inside_the_field() -> void:
	var far: Vector3 = Vector3(RtsConfig.FIELD_HALF_X * 4.0, 0.0, RtsConfig.FIELD_HALF_Z * 4.0)
	var clamped: Vector3 = UnitSteering.clamp_to_field(far, 1.0)
	assert_true(absf(clamped.x) <= RtsConfig.FIELD_HALF_X, "x is clamped to the field")
	assert_true(absf(clamped.z) <= RtsConfig.FIELD_HALF_Z, "z is clamped to the field")

# --- facing --------------------------------------------------------------------------------------
func test_facing_chases_velocity() -> void:
	var state: UnitSteering.State = _at(0.0, 0.0)
	var goal: Vector3 = Vector3(0.0, 0.0, 30.0)   # +Z is yaw 0 under the convention
	for _i: int in 60:
		state = UnitSteering.step(state, goal, _no_obstacles(), _scout(), 0.05)
	assert_almost_eq(state.facing, 0.0, 0.05, "traveling +Z settles at yaw 0")
	var forward: Vector3 = UnitSteering.forward_of(state.facing)
	assert_vec_almost_eq(forward, Vector3(0.0, 0.0, 1.0), 0.05, "and forward_of agrees with it")

func test_facing_turn_rate_is_respected() -> void:
	var arch: RtsConfig.Archetype = _scout()
	var state: UnitSteering.State = UnitSteering.State.new(Vector3.ZERO, Vector3(0.0, 0.0, 6.0), PI)
	var before: float = state.facing
	state = UnitSteering.step(state, Vector3(0.0, 0.0, 30.0), _no_obstacles(), arch, 0.05)
	var turned: float = absf(wrapf(state.facing - before, -PI, PI))
	assert_true(turned <= arch.turn_rate * 0.05 + _EPS,
		"facing turns no faster than the archetype's turn_rate")

# --- degenerate input ------------------------------------------------------------------------------
# A wire-decoded NAN or INF is a real attack surface and a real bug source: it propagates through every
# arithmetic operation, never compares equal to anything, and surfaces far from where it entered. The
# validator rejects it at the boundary; this is the second line.
func test_nan_goal_is_absorbed() -> void:
	var state: UnitSteering.State = _at(3.0, 4.0)
	var out: UnitSteering.State = UnitSteering.step(state, Vector3(NAN, NAN, NAN), _no_obstacles(), _scout(), 0.05)
	assert_true(UnitSteering.is_finite_vec(out.position), "a NAN goal never reaches the position")
	assert_true(UnitSteering.is_finite_vec(out.velocity), "nor the velocity")

func test_infinite_goal_is_absorbed() -> void:
	var state: UnitSteering.State = _at(3.0, 4.0)
	var out: UnitSteering.State = UnitSteering.step(state, Vector3(INF, 0.0, 0.0), _no_obstacles(), _scout(), 0.05)
	assert_true(UnitSteering.is_finite_vec(out.position), "an INF goal never reaches the position")

func test_poisoned_state_is_reset_not_propagated() -> void:
	var state: UnitSteering.State = UnitSteering.State.new(Vector3(NAN, 0.0, 0.0), Vector3.ZERO, 0.0)
	var out: UnitSteering.State = UnitSteering.step(state, Vector3(5.0, 0.0, 0.0), _no_obstacles(), _scout(), 0.05)
	assert_true(UnitSteering.is_finite_vec(out.position),
		"a unit whose position is already NAN is reset rather than stepped")

func test_zero_and_negative_dt_are_no_ops() -> void:
	var state: UnitSteering.State = _at(2.0, 2.0)
	var zero: UnitSteering.State = UnitSteering.step(state, Vector3(20.0, 0.0, 0.0), _no_obstacles(), _scout(), 0.0)
	assert_vec_almost_eq(zero.position, state.position, _EPS, "dt = 0 moves nothing")
	var negative: UnitSteering.State = UnitSteering.step(state, Vector3(20.0, 0.0, 0.0), _no_obstacles(), _scout(), -0.05)
	assert_vec_almost_eq(negative.position, state.position, _EPS, "a negative dt never runs the sim backward")

func test_is_finite_vec() -> void:
	assert_true(UnitSteering.is_finite_vec(Vector3(1.0, 2.0, 3.0)), "an ordinary vector is finite")
	assert_false(UnitSteering.is_finite_vec(Vector3(1.0, NAN, 3.0)), "one NAN component is enough")
	assert_false(UnitSteering.is_finite_vec(Vector3(INF, 0.0, 0.0)), "so is one INF component")
