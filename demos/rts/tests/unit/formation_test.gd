extends UnitTest
## Formation: giving every unit in an order its own destination.

func test_a_single_unit_goes_exactly_where_you_clicked() -> void:
	assert_vec_almost_eq(Formation.slot_offset(0, 1), Vector3.ZERO, 0.0001,
		"a one-unit order has no formation to speak of")

func test_an_odd_square_puts_its_middle_unit_exactly_on_the_click() -> void:
	# 25 units is a 5x5 block, so slot 12 IS the target. This is the strongest form of "something lands where
	# you pointed" that actually holds: a block with an EVEN column count has no centre slot, which is why the
	# guarantee is stated over the centroid rather than over any individual unit.
	assert_vec_almost_eq(Formation.slot_offset(12, 25), Vector3.ZERO, 0.0001,
		"the middle slot of a 5x5 block is the click")

func test_index_zero_is_a_real_slot_not_a_special_case() -> void:
	# Special-casing index 0 onto the target would hand it the same destination as the centre slot -- two
	# units ordered to one point, which is exactly what formations exist to prevent.
	assert_true(Formation.slot_offset(0, 25) != Formation.slot_offset(12, 25),
		"index 0 and the centre slot are different places")

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
