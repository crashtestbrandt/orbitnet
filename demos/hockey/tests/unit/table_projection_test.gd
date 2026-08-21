extends UnitTest
## TableProjection: pointer -> tilted plane -> table space.
##
## Taking a Transform3D rather than a Node3D is what makes this testable: the caller does the projection, which
## needs a live Camera3D, and hands over plain values.

func _board(team: int = 0) -> Transform3D:
	return Transform3D(
		TableFraming.viewpoint_basis(team, deg_to_rad(HockeyConfig.TABLE_TILT_DEGREES)), Vector3.ZERO)

func test_a_ray_at_the_centre_lands_on_the_centre_spot() -> void:
	var board: Transform3D = _board()
	var above: Vector3 = board.basis.y * 2.0
	var hit: Vector3 = TableProjection.table_point(above, -board.basis.y, board)
	assert_vec_almost_eq(hit, Vector3.ZERO, 0.0001, "straight down onto the middle of the table")

func test_table_space_is_recovered_whichever_way_the_board_faces() -> void:
	# The incline AND the half turn both live in the board's transform, so inverting it is what turns a screen
	# ray into the coordinates the simulation and the wire use. Nothing downstream knows the table is tilted.
	for team: int in [0, 1]:
		var board: Transform3D = _board(team)
		for point: Vector3 in [Vector3(0.3, 0.0, -0.7), Vector3(-0.45, 0.0, 0.9), Vector3(0.0, 0.0, 0.2)]:
			var world: Vector3 = board * point
			var origin: Vector3 = world + board.basis.y * 1.5
			var hit: Vector3 = TableProjection.table_point(origin, -board.basis.y, board)
			assert_vec_almost_eq(hit, point, 0.0001,
				"team %d recovers %s from the world point above it" % [team, point])

func test_the_result_is_flattened() -> void:
	var board: Transform3D = _board()
	var world: Vector3 = board * Vector3(0.2, 0.0, 0.4)
	var hit: Vector3 = TableProjection.table_point(world + board.basis.y, -board.basis.y, board)
	assert_almost_eq(hit.y, 0.0, 0.0001, "table space has no height, whatever the arithmetic produced")

func test_a_ray_aimed_away_returns_the_fallback() -> void:
	# A pointer above the horizon. Returning the caller's last good point keeps the mallet where it was rather
	# than flinging it at the origin.
	var board: Transform3D = _board()
	var fallback: Vector3 = Vector3(0.11, 0.0, -0.22)
	var hit: Vector3 = TableProjection.table_point(board.basis.y * 2.0, board.basis.y, board, fallback)
	assert_vec_almost_eq(hit, fallback, 0.0001, "a ray pointing away from the plane never meets it")

func test_a_degenerate_board_returns_the_fallback() -> void:
	var fallback: Vector3 = Vector3(0.3, 0.0, 0.4)
	var flat: Transform3D = Transform3D(Basis().scaled(Vector3.ZERO), Vector3.ZERO)
	assert_vec_almost_eq(TableProjection.table_point(Vector3.UP, Vector3.DOWN, flat, fallback), fallback,
		0.0001, "a board with no normal has no plane to intersect")

func test_an_offset_board_is_handled() -> void:
	# The board sits at the scene origin today. Taking its origin into account rather than assuming zero means
	# moving it later is a scene change rather than a bug.
	var board: Transform3D = Transform3D(
		TableFraming.viewpoint_basis(0, deg_to_rad(HockeyConfig.TABLE_TILT_DEGREES)),
		Vector3(3.0, 1.0, -2.0))
	var point: Vector3 = Vector3(0.25, 0.0, -0.5)
	var world: Vector3 = board * point
	var hit: Vector3 = TableProjection.table_point(world + board.basis.y * 2.0, -board.basis.y, board)
	assert_vec_almost_eq(hit, point, 0.0001, "the board's own origin is subtracted, not assumed away")
