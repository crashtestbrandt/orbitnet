extends UnitTest
## MalletControl: chasing the pointer, dead reckoning, and the velocity a clamped mallet actually has.

const DT: float = 1.0 / 60.0

func _state(at: Vector3, moving: Vector3 = Vector3.ZERO) -> MalletControl.State:
	return MalletControl.State.new(at, moving)

func test_a_mallet_moves_toward_its_target() -> void:
	var start: Vector3 = TableGeometry.home_point(0)
	var target: Vector3 = Vector3(0.2, 0.0, start.z)
	var next: MalletControl.State = MalletControl.step_toward(_state(start), target, 0, DT)
	assert_true(next.position.distance_to(target) < start.distance_to(target),
		"one tick of chasing closes some of the gap")
	assert_true(next.velocity.length() > 0.0, "and gives the mallet a velocity to strike with")

func test_the_speed_cap_holds() -> void:
	var start: Vector3 = Vector3(0.0, 0.0, -0.5)
	var far: Vector3 = Vector3(0.0, 0.0, -0.05)
	var state: MalletControl.State = _state(start)
	for _tick: int in 60:
		state = MalletControl.step_toward(state, far, 0, DT)
		assert_true(state.velocity.length() <= HockeyConfig.MALLET_MAX_SPEED + 0.0001,
			"a mallet never exceeds its speed cap, however far away the pointer is")

func test_acceleration_is_finite() -> void:
	# A mallet that teleported onto the pointer would have no defined speed and could place the puck anywhere.
	var state: MalletControl.State = _state(Vector3(0.0, 0.0, -0.9))
	var first: MalletControl.State = MalletControl.step_toward(state, Vector3(0.0, 0.0, -0.1), 0, DT)
	assert_true(first.velocity.length() <= HockeyConfig.MALLET_ACCEL * DT + 0.0001,
		"the first tick of a flick is bounded by the acceleration, not by the distance")

func test_a_mallet_pressed_into_the_centre_line_reports_no_speed() -> void:
	# The bug this guards: a mallet held against the line keeps the velocity it was DRIVEN at while standing
	# still, and hands all of it to the next puck that touches it.
	var line: Vector3 = TableGeometry.clamp_to_half(
		Vector3(0.0, 0.0, 0.0), 0, HockeyConfig.MALLET_RADIUS)
	var state: MalletControl.State = _state(line, Vector3(0.0, 0.0, 3.0))
	for _tick: int in 5:
		state = MalletControl.step_toward(state, Vector3(0.0, 0.0, 5.0), 0, DT)
	assert_almost_eq(state.velocity.z, 0.0, 0.0001,
		"a mallet that cannot move has no velocity, whatever it was asked to do")

func test_dead_reckoning_coasts_and_decays() -> void:
	# What a peer runs for a mallet whose input it never receives. It must degrade toward standing still
	# rather than toward a wrong place.
	var state: MalletControl.State = _state(Vector3(0.0, 0.0, -0.5), Vector3(0.6, 0.0, 0.0))
	var first: MalletControl.State = MalletControl.step_coast(state, 0, DT)
	assert_true(first.position.x > 0.0, "it keeps going the way it was going")
	assert_true(first.velocity.length() < 0.6, "and loses speed doing it")
	var settled: MalletControl.State = first
	for _tick: int in 120:
		settled = MalletControl.step_coast(settled, 0, DT)
	assert_true(settled.velocity.length() < 0.01, "left alone it comes to rest rather than drifting forever")

func test_coasting_respects_the_half_clamp() -> void:
	var state: MalletControl.State = _state(Vector3(0.0, 0.0, -0.2), Vector3(0.0, 0.0, 40.0))
	var next: MalletControl.State = MalletControl.step_coast(state, 0, DT)
	assert_true(next.position.z <= -HockeyConfig.MALLET_RADIUS + 0.0001,
		"dead reckoning cannot push a mallet across the centre line either")

func test_a_zero_delta_changes_nothing() -> void:
	# The tick dt is 0 while OFFLINE, and dividing by it would produce a mallet at NAN.
	var state: MalletControl.State = _state(Vector3(0.1, 0.0, -0.3), Vector3(1.0, 0.0, 0.0))
	var held: MalletControl.State = MalletControl.step_toward(state, Vector3.ZERO, 0, 0.0)
	assert_vec_almost_eq(held.position, state.position, 0.0001, "no time, no motion")
	assert_vec_almost_eq(held.velocity, state.velocity, 0.0001, "and no divide by zero")
	var coasted: MalletControl.State = MalletControl.step_coast(state, 0, 0.0)
	assert_vec_almost_eq(coasted.position, state.position, 0.0001, "the same for the coast path")

func test_parking_puts_a_vacant_seat_on_its_home_spot() -> void:
	for seat: int in [0, 1, 5, HockeyConfig.SEATS - 1]:
		var parked: MalletControl.State = MalletControl.park(seat)
		assert_vec_almost_eq(parked.position, TableGeometry.home_point(seat), 0.0001,
			"seat %d parks at home" % seat)
		assert_vec_almost_eq(parked.velocity, Vector3.ZERO, 0.0001, "and stationary, so it strikes nothing")
