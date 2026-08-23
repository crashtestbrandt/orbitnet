extends UnitTest
## ArenaGeometry: the rebasing, and the claim the whole demo rests on.

# --- the claim --------------------------------------------------------------------------------------
func test_the_same_local_point_is_the_same_point_in_every_arena() -> void:
	# THE DEMO'S CENTRAL FACT. What replicates is arena-local, so two fighters standing on the same spot in
	# different arenas are ZERO metres apart -- no radius can separate them, and a declared membership is the
	# only thing in the facade that can.
	var spot: Vector3 = Vector3(3.0, 0.0, -4.0)
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		assert_eq(ArenaGeometry.world_to_local(arena, ArenaGeometry.local_to_world(arena, spot)), spot,
			"arena %d round-trips its own local frame" % arena)

func test_rebasing_only_moves_the_world_position() -> void:
	var spot: Vector3 = Vector3(1.0, 0.0, 1.0)
	var first: Vector3 = ArenaGeometry.local_to_world(ArenaConfig.FIRST_ARENA_ID, spot)
	var second: Vector3 = ArenaGeometry.local_to_world(ArenaConfig.FIRST_ARENA_ID + 1, spot)
	assert_almost_eq(first.distance_to(second), ArenaConfig.ARENA_SPACING_M, 0.001,
		"the arenas are one spacing apart in WORLD space, which is presentation -- the interest pass never "
		+ "sees this distance")

func test_the_first_arena_sits_on_the_origin() -> void:
	assert_eq(ArenaGeometry.origin_of(ArenaConfig.FIRST_ARENA_ID), Vector3.ZERO,
		"so a single-arena reading of the demo needs no arithmetic at all")

# --- the floor --------------------------------------------------------------------------------------
func test_clamping_keeps_a_point_on_the_floor() -> void:
	var far_out: Vector3 = Vector3(1000.0, 5.0, -1000.0)
	var clamped: Vector3 = ArenaGeometry.clamp_local(far_out)
	assert_almost_eq(clamped.x, ArenaConfig.ARENA_HALF_X, 0.001, "clamped to the +x wall")
	assert_almost_eq(clamped.z, -ArenaConfig.ARENA_HALF_Z, 0.001, "and the -z one")
	assert_almost_eq(clamped.y, 0.0, 0.001, "and flattened onto the floor plane")

func test_a_point_inside_is_left_alone() -> void:
	var inside: Vector3 = Vector3(1.0, 0.0, 2.0)
	assert_eq(ArenaGeometry.clamp_local(inside), inside, "clamping is a boundary, not a projection")

# --- spawns -----------------------------------------------------------------------------------------
func test_teams_spawn_on_opposite_ends() -> void:
	var team_zero: Vector3 = ArenaGeometry.home_local(0)
	var team_one: Vector3 = ArenaGeometry.home_local(1)
	assert_true(team_zero.z * team_one.z < 0.0,
		"the two teams start on opposite sides of the floor, facing each other across the cover")

func test_every_spawn_is_on_the_floor() -> void:
	for seat: int in ArenaConfig.SEAT_COUNT:
		var home: Vector3 = ArenaGeometry.home_local(seat)
		assert_eq(ArenaGeometry.clamp_local(home), home, "seat %d spawns inside the floor" % seat)

func test_no_two_seats_in_one_arena_share_a_spawn() -> void:
	var seen: PackedVector3Array = PackedVector3Array()
	for step: int in ArenaConfig.SEATS_PER_ARENA:
		var home: Vector3 = ArenaGeometry.home_local(step)
		assert_false(seen.has(home), "seat %d's spawn is its own" % step)
		seen.push_back(home)

func test_spawns_repeat_across_arenas() -> void:
	# Not a bug -- the point. Every arena is the same arena, so seat 0 and the first seat of arena 2 stand on
	# the same local spot, and only the membership tells them apart.
	assert_eq(ArenaGeometry.home_local(0), ArenaGeometry.home_local(ArenaConfig.SEATS_PER_ARENA),
		"the first seat of every arena spawns at the same LOCAL point")

# --- cover and props ---------------------------------------------------------------------------------
func test_cover_stands_on_the_floor_and_has_volume() -> void:
	for index: int in ArenaConfig.COVER_PER_ARENA:
		var box: AABB = ArenaGeometry.cover_local(index)
		assert_true(box.size.x > 0.0 and box.size.y > 0.0 and box.size.z > 0.0,
			"cover %d has volume, so a shot can stop on it" % index)
		assert_almost_eq(box.position.y, 0.0, 0.001, "and its base is on the floor")

func test_cover_blocks_a_line_through_it() -> void:
	var box: AABB = ArenaGeometry.cover_local(0)
	var centre: Vector3 = box.get_center()
	var eye: float = ArenaConfig.FIGHTER_HEIGHT * 0.5
	var from_point: Vector3 = Vector3(centre.x - 8.0, 0.0, centre.z)
	var to_point: Vector3 = Vector3(centre.x + 8.0, 0.0, centre.z)
	# The box is centred at eye height only if it is tall enough; assert against the geometry rather than
	# assuming it.
	if box.position.y <= eye and box.position.y + box.size.y >= eye:
		assert_true(ArenaGeometry.cover_blocks(from_point, to_point), "a line straight through cover is blocked")

func test_props_are_all_on_the_floor_and_distinct() -> void:
	var seen: PackedVector3Array = PackedVector3Array()
	for index: int in ArenaConfig.PROPS_PER_ARENA:
		var at: Vector3 = ArenaGeometry.prop_local(index)
		assert_eq(ArenaGeometry.clamp_local(at), at, "prop %d is inside the floor" % index)
		assert_false(seen.has(at), "and no two props share a spot")
		seen.push_back(at)
