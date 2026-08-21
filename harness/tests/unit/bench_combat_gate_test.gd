extends UnitTest
## The two bench gates that read what the GAME publishes: hit registration and the orientation arm.
##
## Both are static and pure, so they are exercised here rather than by a run. Each has a "not exercised"
## branch that stops a run reporting a healthy number it did not earn, and those branches are the ones worth
## pinning: a gate that reports PASS when it measured nothing is worse than an absent gate.

func _result() -> BenchGate.Result:
	return BenchGate.Result.new()

func _mentions(r: BenchGate.Result, needle: String) -> bool:
	for reason: String in r.reasons:
		if reason.findn(needle) >= 0:
			return true
	return false

# --- hit registration -----------------------------------------------------------------------------

func test_no_target_reports_not_exercised_rather_than_failing() -> void:
	# A policy that fires blind resolves no target. Its rounds say nothing about hit registration, so the
	# gate must not read them as a failure to land hits.
	var r: BenchGate.Result = BenchGate.evaluate_hit_registration(_result(), 500, 0, BenchSubject.TARGET_NONE)
	assert_true(r.passed, "no target must not fail the run")
	assert_true(_mentions(r, "NOT EXERCISED"), "and it must say the gate proved nothing")

func test_too_few_shots_cannot_conclude_the_mechanism_is_broken() -> void:
	# The shot floor gates the NEGATIVE conclusion only: "the server confirmed nothing" needs enough rounds
	# behind it to be evidence rather than luck.
	var r: BenchGate.Result = BenchGate.evaluate_hit_registration(
		_result(), 5, 0, BenchSubject.TARGET_MOVING, 120)
	assert_true(r.passed, "five shots and no confirms is not yet evidence")
	assert_true(_mentions(r, "NOT EXERCISED"), "and it must say so")

func test_zero_confirms_over_the_floor_is_a_failure() -> void:
	var r: BenchGate.Result = BenchGate.evaluate_hit_registration(
		_result(), 200, 0, BenchSubject.TARGET_MOVING, 120)
	assert_false(r.passed, "a firing peer that never lands a confirmed hit is a broken mechanism")

func test_one_confirmed_hit_passes_however_few_rounds_were_fired() -> void:
	# A single confirmation demonstrates the whole path -- aim, adjudicate, confirm -- and no number of
	# rounds makes that less true. It must pass without reaching the floor.
	var r: BenchGate.Result = BenchGate.evaluate_hit_registration(
		_result(), 3, 1, BenchSubject.TARGET_MOVING, 120)
	assert_true(r.passed, "one confirmed hit demonstrates the mechanism")

func test_the_hit_rate_is_reported_and_not_bounded() -> void:
	var r: BenchGate.Result = BenchGate.evaluate_hit_registration(
		_result(), 1000, 1, BenchSubject.TARGET_MOVING, 120)
	assert_true(r.passed, "a low rate is not a failure: the rate moves with scenario, seed and profile")
	assert_true(_mentions(r, "not bounded"), "and the line must say it is unbounded")

# --- the orientation arm --------------------------------------------------------------------------

func test_a_disabled_arm_reports_not_exercised() -> void:
	# With the arm off every counter is zero by construction, which is the exact signature of a perfectly
	# behaved arm. Reporting those zeroes plainly would read as "the defect is fixed".
	var r: BenchGate.Result = BenchGate.evaluate_orientation_arm(
		_result(), false, 0, 0, 0.0, 0.0, 500, 3.0)
	assert_true(r.passed, "a disabled arm is not a failure")
	assert_true(_mentions(r, "NOT EXERCISED"), "but it must not read as a healthy arm")

func test_an_armed_run_reports_the_standing_residual_beside_the_peak() -> void:
	var r: BenchGate.Result = BenchGate.evaluate_orientation_arm(
		_result(), true, 1307, 0, deg_to_rad(9.0), deg_to_rad(7.82), 500, 6.0)
	assert_true(_mentions(r, "STANDING"), "the standing figure is the gauge and must be named")
	assert_true(_mentions(r, "7.82"), "and reported in degrees")

func test_a_run_with_no_samples_reports_nothing() -> void:
	var r: BenchGate.Result = BenchGate.evaluate_orientation_arm(
		_result(), true, 0, 0, 0.0, 0.0, 0, 0.0)
	assert_eq(r.reasons.size(), 0, "no samples means no line, rather than a line of zeroes")
