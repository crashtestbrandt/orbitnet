extends UnitTest
## BenchGate.evaluate_seated: a client the session never seated fails, with a reason that says so.
##
## Such a client still samples the link, so every other gate can pass on a run that drove nothing. The seated
## gate is what turns it into a FAIL with a reason, rather than a client the harness has to kill.

func _mentions(r: BenchGate.Result, needle: String) -> bool:
	for reason: String in r.reasons:
		if reason.findn(needle) >= 0:
			return true
	return false

func test_a_seated_client_passes() -> void:
	var r: BenchGate.Result = BenchGate.evaluate_seated(BenchGate.Result.new(), true, 80.0)
	assert_true(r.passed, "a client with an owned body passes this gate")
	assert_true(_mentions(r, "PASS seated"), "and the line says so")

func test_an_unseated_client_fails_and_says_why() -> void:
	var r: BenchGate.Result = BenchGate.evaluate_seated(BenchGate.Result.new(), false, 80.0)
	assert_false(r.passed, "a client the session never seated fails")
	assert_true(_mentions(r, "no owned body within 80s"), "the reason names the wait")
	assert_true(_mentions(r, "observer"), "and the likely cause")

func test_an_unseated_client_fails_even_when_every_other_gate_passed() -> void:
	# One confirmed hit passes the hit-registration gate, so the result is passing before the seated gate runs.
	var r: BenchGate.Result = BenchGate.evaluate_hit_registration(
		BenchGate.Result.new(), 3, 1, BenchSubject.TARGET_MOVING, 120)
	assert_true(r.passed, "the run passes before the seated gate")
	r = BenchGate.evaluate_seated(r, false, 30.0)
	assert_false(r.passed, "the passing link gates cannot carry a run that drove nothing")
