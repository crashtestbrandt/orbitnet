extends UnitTest
## FighterMotion: the integrator the rollback loop replays, and the clamps that make a client's input safe.

# --- the clamps, which are the security-relevant half --------------------------------------------------
func test_an_oversized_move_intent_is_bounded_to_the_unit_disc() -> void:
	# The backend checks WHO wrote an input row, never what is in it -- a row that decodes at the right
	# stride is stored as-is. A client sending a move vector of length 40 moves forty times as fast unless
	# somebody clamps it, and the only place that can is inside the tick.
	var huge: Vector3 = Vector3(40.0, 0.0, 30.0)
	assert_almost_eq(FighterMotion.clamp_intent(huge).length(), 1.0, 0.0001,
		"a 50-long intent is bounded to 1, not refused: refusing would drop a frame, clamping keeps playing")

func test_a_small_intent_is_left_alone() -> void:
	var gentle: Vector3 = Vector3(0.3, 0.0, 0.4)
	assert_vec_almost_eq(FighterMotion.clamp_intent(gentle), gentle, 0.0001,
		"inside the disc the clamp is the identity, so analog input keeps its magnitude")

func test_vertical_intent_is_discarded() -> void:
	assert_almost_eq(FighterMotion.clamp_intent(Vector3(0.0, 9.0, 0.0)).length(), 0.0, 0.0001,
		"the floor is a plane; a y component is a client asking to fly")

func test_a_non_finite_intent_is_refused() -> void:
	assert_eq(FighterMotion.clamp_intent(Vector3(NAN, 0.0, 0.0)), Vector3.ZERO,
		"NaN survives arithmetic and would reach a physics query as a poisoned position")
	assert_eq(FighterMotion.clamp_intent(Vector3(INF, 0.0, 0.0)), Vector3.ZERO, "and so would infinity")

func test_a_zero_aim_answers_a_direction_rather_than_nothing() -> void:
	var aim: Vector3 = FighterMotion.clamp_aim(Vector3.ZERO)
	assert_almost_eq(aim.length(), 1.0, 0.0001,
		"a shot needs a direction, and normalizing a zero vector produces NaN that would reach the ray cast")

func test_aim_is_flattened_and_normalized() -> void:
	var aim: Vector3 = FighterMotion.clamp_aim(Vector3(3.0, 7.0, 4.0))
	assert_almost_eq(aim.length(), 1.0, 0.0001, "normalized")
	assert_almost_eq(aim.y, 0.0, 0.0001, "and flat, because the arena is")

# --- the integrator ------------------------------------------------------------------------------------
func test_a_fighter_accelerates_toward_its_intent() -> void:
	var state: FighterMotion.State = FighterMotion.State.new(Vector3.ZERO, Vector3.ZERO)
	var next: FighterMotion.State = FighterMotion.step(state, Vector3(0.0, 0.0, 1.0), ArenaConfig.NET_TICK_DT)
	assert_true(next.velocity.z > 0.0, "it starts moving the way it asked to")
	assert_true(next.velocity.z <= ArenaConfig.MOVE_SPEED,
		"and never past the speed cap, however long the intent is held")

func test_a_released_intent_bleeds_off() -> void:
	var moving: FighterMotion.State = FighterMotion.State.new(
		Vector3.ZERO, Vector3(0.0, 0.0, ArenaConfig.MOVE_SPEED))
	var next: FighterMotion.State = FighterMotion.step(moving, Vector3.ZERO, ArenaConfig.NET_TICK_DT)
	assert_true(next.velocity.z < ArenaConfig.MOVE_SPEED, "letting go slows it down")

func test_the_step_is_deterministic() -> void:
	# THE PROPERTY THE ROLLBACK LOOP DEPENDS ON. A tick is restored and re-simulated, potentially many times
	# in one frame, and a step that answered differently the second time would reconcile forever.
	var start: FighterMotion.State = FighterMotion.State.new(Vector3(1.0, 0.0, -2.0), Vector3(0.4, 0.0, 0.1))
	var intent: Vector3 = Vector3(0.6, 0.0, -0.3)
	var first: FighterMotion.State = FighterMotion.step(start, intent, ArenaConfig.NET_TICK_DT)
	var second: FighterMotion.State = FighterMotion.step(start, intent, ArenaConfig.NET_TICK_DT)
	assert_eq(first.position, second.position, "the same input from the same state gives the same position")
	assert_eq(first.velocity, second.velocity, "and the same velocity, bit for bit")

func test_a_replayed_run_lands_where_the_first_one_did() -> void:
	var intent: Vector3 = Vector3(0.8, 0.0, 0.6)
	var live: FighterMotion.State = FighterMotion.State.new(Vector3.ZERO, Vector3.ZERO)
	for _step: int in 30:
		live = FighterMotion.step(live, intent, ArenaConfig.NET_TICK_DT)
	var replay: FighterMotion.State = FighterMotion.State.new(Vector3.ZERO, Vector3.ZERO)
	for _step: int in 30:
		replay = FighterMotion.step(replay, intent, ArenaConfig.NET_TICK_DT)
	assert_eq(live.position, replay.position, "thirty ticks replayed reach the same place")

func test_a_fighter_is_kept_on_the_floor() -> void:
	var state: FighterMotion.State = FighterMotion.State.new(
		Vector3(ArenaConfig.ARENA_HALF_X - 0.1, 0.0, 0.0), Vector3.ZERO)
	for _step: int in 60:
		state = FighterMotion.step(state, Vector3(1.0, 0.0, 0.0), ArenaConfig.NET_TICK_DT)
	assert_almost_eq(state.position.x, ArenaConfig.ARENA_HALF_X, 0.001, "it stops at the wall")

func test_speed_against_a_wall_does_not_accumulate() -> void:
	# Clamping the POSITION without clamping the VELOCITY leaves a fighter pressed into a wall banking speed
	# it cannot spend, which it then spends all at once the moment it turns around.
	var state: FighterMotion.State = FighterMotion.State.new(
		Vector3(ArenaConfig.ARENA_HALF_X, 0.0, 0.0), Vector3.ZERO)
	for _step: int in 60:
		state = FighterMotion.step(state, Vector3(1.0, 0.0, 0.0), ArenaConfig.NET_TICK_DT)
	assert_almost_eq(state.velocity.x, 0.0, 0.0001, "the wall took the velocity, not just the position")

func test_a_parked_fighter_sits_on_its_home_point() -> void:
	var home: Vector3 = ArenaGeometry.home_local(3)
	var parked: FighterMotion.State = FighterMotion.park(home)
	assert_eq(parked.position, home, "a vacant or dead seat is parked at home")
	assert_eq(parked.velocity, Vector3.ZERO, "and holds still")

func test_the_muzzle_is_above_the_feet() -> void:
	assert_true(FighterMotion.muzzle(Vector3.ZERO).y > 0.0,
		"a shot from the floor would stop on the floor")
