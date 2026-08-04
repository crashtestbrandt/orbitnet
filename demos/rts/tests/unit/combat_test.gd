extends UnitTest
## Combat: acquisition, range and damage.

func _positions(values: Array[Vector3]) -> PackedVector3Array:
	var out: PackedVector3Array = PackedVector3Array()
	for value: Vector3 in values:
		out.push_back(value)
	return out

# A world where every id is alive. Seat is derived from the id (seat-major layout), so the ids chosen below
# decide which side each unit is on -- which is exactly how the server derives ownership too.
func _alive(count: int) -> PackedByteArray:
	var out: PackedByteArray = PackedByteArray()
	out.resize(count)
	out.fill(1)
	return out

func _seat1(offset: int) -> int:
	return RtsConfig.first_id_of_seat(1) + offset

func test_acquires_the_nearest_enemy() -> void:
	var positions: PackedVector3Array = PackedVector3Array()
	positions.resize(RtsConfig.UNIT_COUNT)
	positions[0] = Vector3.ZERO                       # seat 0
	positions[_seat1(0)] = Vector3(10.0, 0.0, 0.0)    # seat 1, far
	positions[_seat1(1)] = Vector3(4.0, 0.0, 0.0)     # seat 1, near
	var target: int = Combat.nearest_enemy(Vector3.ZERO, 0, positions, _alive(RtsConfig.UNIT_COUNT), 20.0)
	assert_eq(target, _seat1(1), "the closer enemy is chosen")

func test_never_acquires_a_friendly() -> void:
	var positions: PackedVector3Array = PackedVector3Array()
	positions.resize(RtsConfig.UNIT_COUNT)
	positions[0] = Vector3.ZERO
	positions[1] = Vector3(1.0, 0.0, 0.0)             # a much closer FRIENDLY
	positions[_seat1(0)] = Vector3(9.0, 0.0, 0.0)
	var target: int = Combat.nearest_enemy(Vector3.ZERO, 0, positions, _alive(RtsConfig.UNIT_COUNT), 20.0)
	assert_eq(target, _seat1(0), "friendly fire is not merely discouraged, it is unreachable")

func test_never_acquires_a_corpse() -> void:
	var positions: PackedVector3Array = PackedVector3Array()
	positions.resize(RtsConfig.UNIT_COUNT)
	positions[0] = Vector3.ZERO
	positions[_seat1(0)] = Vector3(2.0, 0.0, 0.0)     # dead
	positions[_seat1(1)] = Vector3(8.0, 0.0, 0.0)     # alive
	var alive: PackedByteArray = _alive(RtsConfig.UNIT_COUNT)
	alive[_seat1(0)] = 0
	var target: int = Combat.nearest_enemy(Vector3.ZERO, 0, positions, alive, 20.0)
	assert_eq(target, _seat1(1), "a dead unit is not a target")

func test_range_is_respected() -> void:
	var positions: PackedVector3Array = PackedVector3Array()
	positions.resize(RtsConfig.UNIT_COUNT)
	positions[_seat1(0)] = Vector3(30.0, 0.0, 0.0)
	var target: int = Combat.nearest_enemy(Vector3.ZERO, 0, positions, _alive(RtsConfig.UNIT_COUNT), 20.0)
	assert_eq(target, -1, "an enemy beyond acquire range is not acquired")

func test_ties_break_on_the_lower_id() -> void:
	# Not arbitrary: with a float comparison, two equidistant targets would otherwise be picked by iteration
	# order -- stable today, and exactly the kind of implicit dependency that breaks when anything reorders.
	var positions: PackedVector3Array = PackedVector3Array()
	positions.resize(RtsConfig.UNIT_COUNT)
	positions[_seat1(3)] = Vector3(5.0, 0.0, 0.0)
	positions[_seat1(9)] = Vector3(5.0, 0.0, 0.0)
	var target: int = Combat.nearest_enemy(Vector3.ZERO, 0, positions, _alive(RtsConfig.UNIT_COUNT), 20.0)
	assert_eq(target, _seat1(3), "the lower id wins a tie")

func test_acquisition_ignores_height() -> void:
	# The sim is planar. A y difference must not affect distance, or a unit standing on a decorative box would
	# be harder to acquire than one beside it.
	var positions: PackedVector3Array = PackedVector3Array()
	positions.resize(RtsConfig.UNIT_COUNT)
	positions[_seat1(0)] = Vector3(5.0, 40.0, 0.0)
	var target: int = Combat.nearest_enemy(Vector3.ZERO, 0, positions, _alive(RtsConfig.UNIT_COUNT), 10.0)
	assert_eq(target, _seat1(0), "distance is measured on the plane")

# --- range and damage ------------------------------------------------------------------------------
func test_in_attack_range() -> void:
	assert_true(Combat.in_attack_range(Vector3.ZERO, Vector3(4.0, 0.0, 0.0), 5.0), "inside")
	assert_false(Combat.in_attack_range(Vector3.ZERO, Vector3(6.0, 0.0, 0.0), 5.0), "outside")
	assert_true(Combat.in_attack_range(Vector3.ZERO, Vector3(5.0, 0.0, 0.0), 5.0), "exactly at range counts")

func test_damage_scales_with_dt() -> void:
	var tank: RtsConfig.Archetype = RtsConfig.archetype(RtsConfig.Kind.TANK)
	assert_almost_eq(Combat.damage(tank, 1.0), tank.dps, 0.0001, "one second of fire is one dps")
	assert_almost_eq(Combat.damage(tank, 0.5), tank.dps * 0.5, 0.0001, "half a second is half")

func test_degenerate_dt_deals_nothing() -> void:
	var tank: RtsConfig.Archetype = RtsConfig.archetype(RtsConfig.Kind.TANK)
	assert_almost_eq(Combat.damage(tank, 0.0), 0.0, 0.0001, "no time, no damage")
	assert_almost_eq(Combat.damage(tank, -1.0), 0.0, 0.0001, "and time never runs backwards into healing")
	assert_almost_eq(Combat.damage(tank, NAN), 0.0, 0.0001, "a NaN dt deals nothing rather than NaN damage")

# --- stand-off -------------------------------------------------------------------------------------
func test_approach_stops_short_of_the_target() -> void:
	var goal: Vector3 = Combat.approach_goal(Vector3.ZERO, Vector3(20.0, 0.0, 0.0), 10.0)
	assert_almost_eq(goal.x, 20.0 - 8.5, 0.01,
		"a unit closes to 85% of its attack range, not to contact")

func test_a_unit_already_in_range_holds_position() -> void:
	var here: Vector3 = Vector3(3.0, 0.0, 0.0)
	var goal: Vector3 = Combat.approach_goal(here, Vector3(6.0, 0.0, 0.0), 10.0)
	assert_vec_almost_eq(goal, here, 0.0001,
		"the 85% stand-off is hysteresis: a unit that drifts slightly does not start chasing again")

func test_a_coincident_target_does_not_divide_by_zero() -> void:
	var goal: Vector3 = Combat.approach_goal(Vector3.ZERO, Vector3.ZERO, 10.0)
	assert_true(UnitSteering.is_finite_vec(goal), "two units at the same point produce a finite goal")

# --- archetypes ------------------------------------------------------------------------------------
func test_the_archetype_mix_is_deterministic_and_covers_all_three() -> void:
	# Both peers derive every unit's stats from its id without replicating them, so this mapping is part of
	# the wire contract in everything but name.
	var counts: Dictionary[int, int] = {}
	for index: int in RtsConfig.UNITS_PER_SEAT:
		var kind: int = RtsConfig.kind_for_index(index)
		counts[kind] = counts.get(kind, 0) + 1
	assert_eq(counts.size(), 3, "all three archetypes appear in a 48-unit army")
	assert_eq(counts[RtsConfig.Kind.TANK], 4, "4 Tanks per 48 (one per 12)")
	assert_eq(counts[RtsConfig.Kind.TROOPER], 20, "20 Troopers")
	assert_eq(counts[RtsConfig.Kind.SCOUT], 24, "24 Scouts")

func test_seat_is_pure_arithmetic_on_the_id() -> void:
	assert_eq(RtsConfig.seat_of(0), 0, "the first id belongs to seat 0")
	assert_eq(RtsConfig.seat_of(RtsConfig.UNITS_PER_SEAT - 1), 0, "as does the last of its block")
	assert_eq(RtsConfig.seat_of(RtsConfig.UNITS_PER_SEAT), 1, "the next block is seat 1")
	assert_eq(RtsConfig.seat_of(-1), -1, "an invalid id belongs to nobody")
	assert_eq(RtsConfig.seat_of(RtsConfig.UNIT_COUNT), -1, "and so does one past the end")
