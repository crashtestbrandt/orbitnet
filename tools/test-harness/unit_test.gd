extends RefCounted
class_name UnitTest
## Base class for the hand-rolled GDScript unit suites. A suite extends this and defines `test_*` methods;
## the runner (tests/support/run_unit_tests.gd) discovers suites under tests/unit/ and calls every `test_*`
## method by reflection -- no scene tree, no physics, no windowing, so a whole suite runs in milliseconds
## under `godot --headless --script tests/support/run_unit_tests.gd`.
##
## CANONICAL COPY: tools/test-harness/. Every Godot project in this repo gets a mirror of it from
## `just sync-addons`, and `just addon-drift` fails if a copy is edited instead of this one.
##
## Failures ACCUMULATE (never the engine's `assert()`, which aborts the current call on first hit) so one
## run reports every mismatch, matching the existing probes' PASS/FAIL-with-a-list style. A suite targets a
## PURE function -- no scene dependency -- construct fixtures directly and call the function; if a case
## needs a live scene/physics/network, it belongs in a tools/instr/*_probe.gd gate instead.

var _failures: PackedStringArray = PackedStringArray()
var _checks: int = 0

func failures() -> PackedStringArray:
	return _failures

func check_count() -> int:
	return _checks

func assert_true(ok: bool, label: String) -> void:
	_checks += 1
	if not ok:
		_failures.push_back(label)

func assert_false(ok: bool, label: String) -> void:
	assert_true(not ok, label)

func assert_eq(actual: Variant, expected: Variant, label: String) -> void:
	_checks += 1
	if actual != expected:
		_failures.push_back("%s (expected %s, got %s)" % [label, expected, actual])

func assert_almost_eq(actual: float, expected: float, eps: float, label: String) -> void:
	_checks += 1
	if absf(actual - expected) > eps:
		_failures.push_back("%s (expected ~%s +/- %s, got %s)" % [label, expected, eps, actual])

func assert_vec_almost_eq(actual: Vector3, expected: Vector3, eps: float, label: String) -> void:
	_checks += 1
	if (actual - expected).length() > eps:
		_failures.push_back("%s (expected ~%s +/- %s, got %s)" % [label, expected, eps, actual])

## Sign-agnostic quaternion comparison -- q and -q are the same rotation.
func assert_quat_almost_eq(actual: Quaternion, expected: Quaternion, eps: float, label: String) -> void:
	_checks += 1
	var d_pos: float = _quat_max_component_diff(actual, expected)
	var d_neg: float = _quat_max_component_diff(actual, -expected)
	if minf(d_pos, d_neg) > eps:
		_failures.push_back("%s (expected ~%s +/- %s, got %s)" % [label, expected, eps, actual])

func _quat_max_component_diff(a: Quaternion, b: Quaternion) -> float:
	return maxf(maxf(absf(a.x - b.x), absf(a.y - b.y)), maxf(absf(a.z - b.z), absf(a.w - b.w)))
