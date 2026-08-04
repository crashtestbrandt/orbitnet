extends UnitTest
## Formation: giving every unit in an order its own destination.

func test_a_single_unit_goes_exactly_where_you_clicked() -> void:
	assert_vec_almost_eq(Formation.slot_offset(0, 1), Vector3.ZERO, 0.0001,
		"a one-unit order has no formation to speak of")

func test_the_first_unit_is_always_on_the_click() -> void:
	for count: int in [1, 2, 5, 24, 48]:
		assert_vec_almost_eq(Formation.slot_offset(0, count), Vector3.ZERO, 0.0001,
			"index 0 lands on the target in a %d-unit order, so a click always sends SOMETHING exactly there"
			% count)

func test_slots_are_distinct() -> void:
	# The whole point: 24 units ordered to one place must not be 24 units ordered to the SAME place, or they
	# fight over it forever and the jitter reads as a network problem.
	var seen: Dictionary[String, bool] = {}
	var count: int = 24
	for index: int in count:
		var offset: Vector3 = Formation.slot_offset(index, count)
		var key: String = "%.3f,%.3f" % [offset.x, offset.z]
		assert_false(seen.has(key), "slot %d of %d is unique" % [index, count])
		seen[key] = true

func test_the_block_is_roughly_square_and_centred() -> void:
	var count: int = 25
	var min_x: float = 1e9
	var max_x: float = -1e9
	var sum: Vector3 = Vector3.ZERO
	for index: int in count:
		var offset: Vector3 = Formation.slot_offset(index, count)
		min_x = minf(min_x, offset.x)
		max_x = maxf(max_x, offset.x)
		sum += offset
	assert_almost_eq(sum.x / float(count), 0.0, 0.001, "the block is centred on the target in x")
	assert_almost_eq(sum.z / float(count), 0.0, 0.001, "and in z")
	assert_almost_eq(max_x - min_x, Formation.SLOT_SPACING * 4.0, 0.001,
		"25 units make a 5-wide block")

func test_offsets_stay_on_the_ground_plane() -> void:
	for index: int in 48:
		assert_almost_eq(Formation.slot_offset(index, 48).y, 0.0, 0.0001, "no formation slot leaves the plane")

func test_goals_are_clamped_inside_the_field() -> void:
	# An order against the map edge must not send the outer files into the wall forever.
	var edge: Vector3 = Vector3(RtsConfig.FIELD_HALF_X, 0.0, RtsConfig.FIELD_HALF_Z)
	for index: int in 25:
		var goal: Vector3 = Formation.goal_for(index, 25, edge)
		assert_true(absf(goal.x) <= RtsConfig.FIELD_HALF_X, "slot %d stays in bounds in x" % index)
		assert_true(absf(goal.z) <= RtsConfig.FIELD_HALF_Z, "slot %d stays in bounds in z" % index)

func test_a_negative_index_is_harmless() -> void:
	assert_vec_almost_eq(Formation.slot_offset(-3, 10), Vector3.ZERO, 0.0001,
		"a nonsense index degrades to the target rather than to a NaN")
