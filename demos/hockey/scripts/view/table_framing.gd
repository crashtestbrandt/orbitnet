extends RefCounted
class_name TableFraming
## Where the camera goes, solved from the table rather than tuned by hand. Pure.
##
## The framing is FIXED -- no pan, no zoom, no edge scroll, no orbit. Which means "the entire top surface is
## visible" has to be a property of the numbers rather than of somebody's last nudge, or the first person to
## change the table size or the tilt silently crops a rail off the window. So the camera distance is SOLVED:
## project every table corner into camera space and take the distance at which the tightest of them still sits
## inside the frustum with a margin. A unit test asserts the solve holds at a spread of aspect ratios.
##
## THE ONE THING THAT IS NOT FIXED is which end faces you, and it is chosen once when your seat is assigned.
## Playing from the top of the screen is not a camera control, it is a defect -- so a player on team 1 gets the
## board rotated a half turn about its own axis, which keeps the tilt pointing at the camera and puts their own
## goal nearest. Nothing about it moves during play.
##
## Godot's Camera3D defaults to KEEP_HEIGHT, so `fov` is VERTICAL and the horizontal half-angle grows with the
## aspect ratio. A narrow window therefore has LESS horizontal room, which is the case the solve has to cover:
## it takes the aspect as an argument and TableView re-solves whenever the viewport is resized.

## The board's basis for a player on `team`: the tilt, plus a half turn for the team whose end would otherwise
## be at the far edge.
##
## The half turn is applied on the RIGHT of the tilt, not the left. `Basis(UP, PI) * tilt` would rotate the
## tilt itself, so the table would lean AWAY from the camera and the surface would face the ceiling.
static func viewpoint_basis(team: int, tilt_rad: float) -> Basis:
	var basis: Basis = Basis(Vector3.RIGHT, tilt_rad)
	if team != 1:
		basis = basis * Basis(Vector3.UP, PI)
	return basis

## The four table corners, in the frame the camera is solved against.
static func corners(half_width: float, half_length: float, basis: Basis) -> PackedVector3Array:
	var out: PackedVector3Array = PackedVector3Array()
	for sx: int in 2:
		for sz: int in 2:
			var x: float = half_width if sx == 0 else -half_width
			var z: float = half_length if sz == 0 else -half_length
			out.push_back(basis * Vector3(x, 0.0, z))
	return out

## The corners of the shipped table, for the viewpoint of `team`.
static func table_corners(team: int) -> PackedVector3Array:
	return corners(HockeyConfig.HALF_WIDTH, HockeyConfig.HALF_LENGTH,
		viewpoint_basis(team, deg_to_rad(HockeyConfig.TABLE_TILT_DEGREES)))

## The camera-space height every point should be centered about, so the tilt's vertical asymmetry does not spend
## half the frustum on empty space above the table.
static func vertical_center(points: PackedVector3Array, pitch_rad: float) -> float:
	if points.is_empty():
		return 0.0
	var inverse: Basis = Basis(Vector3.RIGHT, -pitch_rad)
	var low: float = INF
	var high: float = -INF
	for point: Vector3 in points:
		var camera_space: Vector3 = inverse * point
		low = minf(low, camera_space.y)
		high = maxf(high, camera_space.y)
	return (low + high) * 0.5

## The smallest distance at which every point in `points` is inside the frustum with `margin` to spare.
##
## Analytic rather than iterative. A camera pitched by `pitch_rad` and pulled back by D along its own forward
## axis puts a world point at camera-space (x, y, z - D) where (x, y, z) is that point rotated into the
## camera's frame -- the rotation does not depend on D at all. So each frustum constraint solves directly for
## the D that satisfies it, and the answer is the largest of them.
static func min_distance(points: PackedVector3Array, pitch_rad: float, fov_rad: float, aspect: float,
		margin: float) -> float:
	if points.is_empty():
		return 1.0
	var tan_v: float = tan(clampf(fov_rad, 0.05, PI - 0.05) * 0.5) * (1.0 - clampf(margin, 0.0, 0.9))
	var tan_h: float = tan_v * maxf(0.05, aspect)
	var center: float = vertical_center(points, pitch_rad)
	var inverse: Basis = Basis(Vector3.RIGHT, -pitch_rad)
	var distance: float = 0.0
	for point: Vector3 in points:
		var camera_space: Vector3 = inverse * point
		distance = maxf(distance, camera_space.z + absf(camera_space.y - center) / tan_v)
		distance = maxf(distance, camera_space.z + absf(camera_space.x) / tan_h)
	return distance

## Whether `points` all sit inside the frustum at `distance`. The predicate [method min_distance] solves, kept
## separate so a test can assert the solve rather than re-derive it.
static func fits(points: PackedVector3Array, distance: float, pitch_rad: float, fov_rad: float, aspect: float,
		margin: float) -> bool:
	var tan_v: float = tan(clampf(fov_rad, 0.05, PI - 0.05) * 0.5) * (1.0 - clampf(margin, 0.0, 0.9))
	var tan_h: float = tan_v * maxf(0.05, aspect)
	var center: float = vertical_center(points, pitch_rad)
	var inverse: Basis = Basis(Vector3.RIGHT, -pitch_rad)
	for point: Vector3 in points:
		var camera_space: Vector3 = inverse * point
		var depth: float = distance - camera_space.z
		if depth <= 0.0:
			return false
		if absf(camera_space.y - center) > tan_v * depth + 0.000001:
			return false
		if absf(camera_space.x) > tan_h * depth + 0.000001:
			return false
	return true

## The camera's transform for a solved distance: pitched by `pitch_rad`, pulled back along its own forward
## axis, and slid along its own up axis so the table is vertically centered.
static func camera_transform(distance: float, pitch_rad: float, vertical_offset: float) -> Transform3D:
	var basis: Basis = Basis(Vector3.RIGHT, pitch_rad)
	var forward: Vector3 = basis * Vector3(0.0, 0.0, -1.0)
	var up: Vector3 = basis * Vector3(0.0, 1.0, 0.0)
	return Transform3D(basis, -forward * distance + up * vertical_offset)

## The whole solve for the shipped table, at a given viewport aspect ratio.
static func solve(team: int, aspect: float) -> Transform3D:
	var points: PackedVector3Array = table_corners(team)
	var pitch: float = deg_to_rad(HockeyConfig.CAMERA_PITCH_DEGREES)
	var fov: float = deg_to_rad(HockeyConfig.CAMERA_FOV_DEGREES)
	var distance: float = min_distance(points, pitch, fov, aspect, HockeyConfig.FRAMING_MARGIN)
	return camera_transform(distance, pitch, vertical_center(points, pitch))
