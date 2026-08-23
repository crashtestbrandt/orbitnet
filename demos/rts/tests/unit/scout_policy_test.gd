extends UnitTest
## ScoutPolicy: which enemy units a seat can see, and what it reports as having changed.

const SEAT_A: int = 0
const SEAT_B: int = 1

## Two seat-0 units and one seat-1 unit, refreshed from seat 0. `range_m` is the distance from the FORWARD
## eye, which is the one that decides -- vision is the nearest eye's answer, so measuring from the origin
## would silently understate every distance by the two metres the second friendly unit stands ahead.
##
## Typed locals throughout rather than an Array of arrays: `Array` indexing yields a Variant, and this project
## promotes every Variant that reaches a typed parameter to an error.
const FORWARD_EYE_X: float = 2.0

func _refresh(policy: ScoutPolicy, range_m: float) -> PackedInt32Array:
	var seats: PackedInt32Array = PackedInt32Array([SEAT_A, SEAT_A, SEAT_B])
	var positions: PackedVector3Array = PackedVector3Array([
		Vector3.ZERO,
		Vector3(FORWARD_EYE_X, 0.0, 0.0),
		Vector3(FORWARD_EYE_X + range_m, 0.0, 0.0)])
	var alive: PackedByteArray = PackedByteArray([1, 1, 1])
	return policy.refresh(SEAT_A, seats, positions, alive)

# --- the first pass ---------------------------------------------------------------------------------
func test_the_first_refresh_reports_the_units_to_withhold() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	var changed: PackedInt32Array = _refresh(policy, 500.0)
	assert_eq(changed.size(), 1,
		"the far enemy unit starts out of vision, and the first pass is the only chance to withhold it")
	assert_eq(changed[0], 2, "and it is the seat-1 unit, not either of seat 0's own")

func test_the_first_refresh_reports_nothing_when_everything_is_already_visible() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	var changed: PackedInt32Array = _refresh(policy, ScoutPolicy.VISION_RADIUS_M * 0.5)
	assert_eq(changed.size(), 0,
		"every unit is inside vision and the backend holds no veto, so nothing has to change")

func test_your_own_units_are_never_hidden() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 500.0)
	assert_true(policy.is_visible(0), "seat 0 sees its own unit")
	assert_true(policy.is_visible(1), "and its other one")

# --- entering and leaving vision --------------------------------------------------------------------
func test_a_unit_inside_the_radius_is_visible() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, ScoutPolicy.VISION_RADIUS_M * 0.5)
	assert_true(policy.is_visible(2), "well inside vision")

func test_a_unit_beyond_the_radius_is_hidden() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 500.0)
	assert_false(policy.is_visible(2), "far away and never seen")
	assert_eq(policy.hidden_count(), 1, "one unit is being withheld")

func test_crossing_into_vision_is_reported_once() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 500.0)
	var changed: PackedInt32Array = _refresh(policy, 1.0)
	assert_eq(changed.size(), 1, "one unit changed")
	assert_eq(changed[0], 2, "and it is the enemy that walked in")
	assert_eq(_refresh(policy, 1.0).size(), 0,
		"standing still is not a change -- re-vetoing an entity already in that state resets its delta base")

# --- the hysteresis band ----------------------------------------------------------------------------
func test_a_seen_unit_is_kept_past_the_entry_radius() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 1.0)
	var between: float = (ScoutPolicy.VISION_RADIUS_M + ScoutPolicy.VISION_EXIT_M) * 0.5
	_refresh(policy, between)
	assert_true(policy.is_visible(2),
		"inside the band a unit that was already seen stays seen, so an edge-walker does not flip every tick")

func test_a_unit_past_the_exit_radius_is_lost() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 1.0)
	_refresh(policy, ScoutPolicy.VISION_EXIT_M + 1.0)
	assert_false(policy.is_visible(2), "past the exit radius it is withheld again")

func test_an_unseen_unit_is_not_admitted_inside_the_band() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 500.0)
	var between: float = (ScoutPolicy.VISION_RADIUS_M + ScoutPolicy.VISION_EXIT_M) * 0.5
	_refresh(policy, between)
	assert_false(policy.is_visible(2),
		"the band only RETAINS; entering still costs the full vision radius")

# --- the degenerate cases ---------------------------------------------------------------------------
func test_a_seat_with_nothing_alive_sees_no_enemy() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	var seats: PackedInt32Array = PackedInt32Array([SEAT_A, SEAT_B])
	var positions: PackedVector3Array = PackedVector3Array([Vector3.ZERO, Vector3(1.0, 0.0, 0.0)])
	policy.refresh(SEAT_A, seats, positions, PackedByteArray([1, 1]))
	assert_true(policy.is_visible(1), "an enemy a metre away is seen")
	policy.refresh(SEAT_A, seats, positions, PackedByteArray([0, 1]))
	assert_false(policy.is_visible(1),
		"with its last eye dead the seat sees nothing, however close the enemy is standing")

func test_clear_forgets_everything() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 500.0)
	assert_eq(policy.hidden_count(), 1, "one withheld")
	policy.clear()
	assert_eq(policy.hidden_count(), 0, "and after a clear, nothing is withheld")
	assert_true(policy.is_visible(2),
		"because the backend's vetoes were retracted at the same moment; the two memories clear together")
	assert_eq(_refresh(policy, 500.0).size(), 1,
		"a first refresh again: the vetoes went with them, so a unit still out of vision is reported anew")

func test_an_out_of_range_index_is_visible() -> void:
	var policy: ScoutPolicy = ScoutPolicy.new()
	_refresh(policy, 1.0)
	assert_true(policy.is_visible(99), "an index the policy has never sized for fails OPEN, never closed")
