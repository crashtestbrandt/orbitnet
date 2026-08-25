extends UnitTest
## TableGeometry: the half clamp, the goal mouths, and the home spots.

func test_a_mallet_gets_the_full_width_of_its_own_half() -> void:
	# The player's stated requirement, as an assertion: full left-right range, own end only.
	var limit: float = HockeyConfig.HALF_WIDTH - HockeyConfig.MALLET_RADIUS
	for seat: int in [0, 1]:
		var left: Vector3 = TableGeometry.clamp_to_half(
			Vector3(-99.0, 0.0, TableGeometry.home_point(seat).z), seat, HockeyConfig.MALLET_RADIUS)
		var right: Vector3 = TableGeometry.clamp_to_half(
			Vector3(99.0, 0.0, TableGeometry.home_point(seat).z), seat, HockeyConfig.MALLET_RADIUS)
		assert_almost_eq(left.x, -limit, 0.0001, "seat %d reaches the left rail" % seat)
		assert_almost_eq(right.x, limit, 0.0001, "seat %d reaches the right rail" % seat)

func test_no_mallet_crosses_the_center_line() -> void:
	# The clamp is the VALIDATION MOMENT: it runs on the server inside _rollback_tick, from the client's own
	# requested point. A client asking for the far end must get the center line, not the far end.
	for seat: int in [0, 1, 2, 3]:
		var sign_z: float = HockeyConfig.end_sign(HockeyConfig.team_of_seat(seat))
		var far_side: Vector3 = Vector3(0.0, 0.0, -sign_z * HockeyConfig.HALF_LENGTH * 2.0)
		var clamped: Vector3 = TableGeometry.clamp_to_half(far_side, seat, HockeyConfig.MALLET_RADIUS)
		assert_true(clamped.z * sign_z >= HockeyConfig.MALLET_RADIUS - 0.0001,
			"seat %d cannot reach past the center line, even by asking" % seat)
		assert_true(absf(clamped.z) <= HockeyConfig.HALF_LENGTH,
			"seat %d cannot be pushed through its own end rail either" % seat)

func test_the_clamp_flattens_and_is_idempotent() -> void:
	var raw: Vector3 = Vector3(0.2, 9.0, -0.4)
	var once: Vector3 = TableGeometry.clamp_to_half(raw, 0, HockeyConfig.MALLET_RADIUS)
	var twice: Vector3 = TableGeometry.clamp_to_half(once, 0, HockeyConfig.MALLET_RADIUS)
	assert_almost_eq(once.y, 0.0, 0.0001, "table space has no height")
	assert_vec_almost_eq(twice, once, 0.0001, "clamping an already-clamped point changes nothing")

func test_home_spots_are_distinct_and_inside_their_own_half() -> void:
	var seen: Dictionary[String, bool] = {}
	for seat: int in HockeyConfig.SEATS:
		var home: Vector3 = TableGeometry.home_point(seat)
		var key: String = "%.4f|%.4f" % [home.x, home.z]
		assert_false(seen.has(key), "seat %d parks somewhere no other seat does" % seat)
		seen[key] = true
		var clamped: Vector3 = TableGeometry.clamp_to_half(home, seat, HockeyConfig.MALLET_RADIUS)
		assert_vec_almost_eq(clamped, home, 0.0001, "seat %d's home spot is already legal" % seat)

func test_the_first_seats_stand_nearest_the_middle() -> void:
	# Center-out ordering, so a two-player game does not start with both mallets in opposite corners.
	assert_almost_eq(TableGeometry.home_point(0).x, 0.0, 0.0001, "the first seat on team 0 is central")
	assert_almost_eq(TableGeometry.home_point(1).x, 0.0, 0.0001, "and the first on team 1")
	assert_true(absf(TableGeometry.home_point(2).x) > absf(TableGeometry.home_point(0).x),
		"the second player on an end stands wider than the first")

func test_the_goal_mouth_is_the_only_gap_in_an_end_rail() -> void:
	assert_true(TableGeometry.is_in_goal_mouth(0.0), "dead center is a goal")
	assert_true(TableGeometry.is_in_goal_mouth(HockeyConfig.GOAL_HALF_WIDTH - 0.001), "just inside a post")
	assert_false(TableGeometry.is_in_goal_mouth(HockeyConfig.GOAL_HALF_WIDTH + 0.001), "just outside one")
	assert_false(TableGeometry.is_in_goal_mouth(HockeyConfig.HALF_WIDTH), "the corner is solid rail")

func test_crossing_a_goal_line_scores_for_the_other_end() -> void:
	assert_eq(TableGeometry.scoring_team_at(0.0), -1, "the middle of the table is not a goal")
	assert_eq(TableGeometry.scoring_team_at(HockeyConfig.HALF_LENGTH * 0.99), -1, "nor short of the line")
	assert_eq(TableGeometry.scoring_team_at(-HockeyConfig.HALF_LENGTH), 1,
		"team 0 defends -z, so a puck past that line is team 1's goal")
	assert_eq(TableGeometry.scoring_team_at(HockeyConfig.HALF_LENGTH), 0,
		"and the mirror image is team 0's")

func test_non_finite_points_are_refused_rather_than_clamped() -> void:
	# A wire- or pointer-decoded NAN propagates through every operation, never compares equal to anything, and
	# surfaces far from where it entered.
	assert_true(TableGeometry.is_finite_point(Vector3(0.1, 0.0, -0.2)), "an ordinary point is finite")
	assert_false(TableGeometry.is_finite_point(Vector3(NAN, 0.0, 0.0)), "NAN is not")
	assert_false(TableGeometry.is_finite_point(Vector3(0.0, 0.0, INF)), "nor is an infinity")
