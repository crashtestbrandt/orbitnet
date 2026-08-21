extends UnitTest
## TableFraming: the fixed camera, solved rather than tuned.
##
## "The entire top surface is visible" has to be a property of the numbers. Tuned by hand, the first person to
## change the table size or the tilt would silently crop a rail off the window and nothing would say so.

const ASPECTS: Array[float] = [21.0 / 9.0, 16.0 / 9.0, 16.0 / 10.0, 4.0 / 3.0, 1.0, 3.0 / 4.0, 9.0 / 16.0]

func _pitch() -> float:
	return deg_to_rad(HockeyConfig.CAMERA_PITCH_DEGREES)

func _fov() -> float:
	return deg_to_rad(HockeyConfig.CAMERA_FOV_DEGREES)

func test_the_whole_table_fits_at_every_aspect() -> void:
	for team: int in [0, 1]:
		var corners: PackedVector3Array = TableFraming.table_corners(team)
		for aspect: float in ASPECTS:
			var distance: float = TableFraming.min_distance(
				corners, _pitch(), _fov(), aspect, HockeyConfig.FRAMING_MARGIN)
			assert_true(TableFraming.fits(corners, distance, _pitch(), _fov(), aspect,
				HockeyConfig.FRAMING_MARGIN),
				"team %d's view frames every corner at aspect %.2f" % [team, aspect])

func test_the_solve_is_tight() -> void:
	# Not merely sufficient: pulling back further than necessary would waste the window, so the answer must be
	# the SMALLEST distance that works.
	var corners: PackedVector3Array = TableFraming.table_corners(0)
	var distance: float = TableFraming.min_distance(
		corners, _pitch(), _fov(), 16.0 / 9.0, HockeyConfig.FRAMING_MARGIN)
	assert_false(TableFraming.fits(corners, distance - 0.05, _pitch(), _fov(), 16.0 / 9.0,
		HockeyConfig.FRAMING_MARGIN),
		"five centimetres closer and a corner leaves the frustum")

func test_a_narrower_window_needs_more_distance() -> void:
	# Godot's Camera3D defaults to KEEP_HEIGHT, so `fov` is VERTICAL and a narrow window has LESS horizontal
	# room. This is the case that makes the framing re-solve on resize rather than being computed once.
	var corners: PackedVector3Array = TableFraming.table_corners(0)
	var wide: float = TableFraming.min_distance(corners, _pitch(), _fov(), 16.0 / 9.0,
		HockeyConfig.FRAMING_MARGIN)
	var narrow: float = TableFraming.min_distance(corners, _pitch(), _fov(), 9.0 / 16.0,
		HockeyConfig.FRAMING_MARGIN)
	assert_true(narrow > wide, "a tall window pulls the camera back rather than cropping the rails")

func test_each_team_looks_at_its_own_end() -> void:
	# Playing from the top of the screen is not a camera control, it is a defect. The half turn is the ONE
	# thing about the framing that is not fixed, and it is chosen once when a seat is assigned.
	for team: int in [0, 1]:
		var basis: Basis = TableFraming.viewpoint_basis(
			team, deg_to_rad(HockeyConfig.TABLE_TILT_DEGREES))
		var distance: float = TableFraming.min_distance(TableFraming.table_corners(team), _pitch(), _fov(),
			16.0 / 9.0, HockeyConfig.FRAMING_MARGIN)
		var camera: Transform3D = TableFraming.camera_transform(distance, _pitch(),
			TableFraming.vertical_center(TableFraming.table_corners(team), _pitch()))
		var mine: Vector3 = basis * Vector3(0.0, 0.0, TableGeometry.goal_line_z(team))
		var theirs: Vector3 = basis * Vector3(0.0, 0.0, TableGeometry.goal_line_z(1 - team))
		assert_true(camera.origin.distance_to(mine) < camera.origin.distance_to(theirs),
			"team %d's own goal is the near one" % team)

func test_the_table_leans_toward_the_camera() -> void:
	# The half turn is applied on the RIGHT of the tilt. Applied on the left it would rotate the tilt itself,
	# and the surface would lean away from the camera and face the ceiling.
	for team: int in [0, 1]:
		var basis: Basis = TableFraming.viewpoint_basis(
			team, deg_to_rad(HockeyConfig.TABLE_TILT_DEGREES))
		assert_true(basis.y.z > 0.0,
			"team %d's board faces the camera, which sits on +z" % team)

func test_the_corner_set_is_the_same_either_way_round() -> void:
	# Which is why one distance solve serves both viewpoints.
	var a: PackedVector3Array = TableFraming.table_corners(0)
	var b: PackedVector3Array = TableFraming.table_corners(1)
	assert_eq(a.size(), 4, "a rectangle has four corners")
	assert_eq(b.size(), 4, "either way round")
	for corner: Vector3 in a:
		var matched: bool = false
		for other: Vector3 in b:
			if corner.distance_to(other) < 0.000001:
				matched = true
		assert_true(matched, "every corner of one viewpoint is a corner of the other")

func test_an_empty_point_set_does_not_divide_by_anything() -> void:
	var none: PackedVector3Array = PackedVector3Array()
	assert_true(TableFraming.min_distance(none, _pitch(), _fov(), 1.0, 0.1) > 0.0,
		"no points still yields a usable distance rather than a zero or a NAN")
	assert_almost_eq(TableFraming.vertical_center(none, _pitch()), 0.0, 0.0001, "and a centre of zero")
