extends SceneTree
## Fast, isolated unit-test runner: discovers every `*_test.gd` suite under res://tests/unit/, loads it as a
## script (each suite extends [UnitTest], a RefCounted -- no scene tree, no physics, no windowing), calls
## every `test_*` method by reflection, and aggregates PASS/FAIL. No --path bring-up of the main scene, so a
## full run completes in about a second versus the tens of seconds a scene-based probe pays.
##
## Exits non-zero on any failure, AND on finding no suites or no test_* methods -- a silently empty run is a
## red flag, not a pass. That check has caught more real breakage than most of the assertions: a renamed
## directory or a parse error that stops discovery would otherwise report success.
##
## CANONICAL COPY: tools/test-harness/. Mirrored into every project by `just sync-addons`.

const _SUITE_DIR: String = "res://tests/unit"

func _initialize() -> void:
	var suite_paths: PackedStringArray = _discover_suites()
	if suite_paths.is_empty():
		printerr("run_unit_tests: no *_test.gd suites found under %s" % _SUITE_DIR)
		quit(1)
		return

	var suite_count: int = 0
	var total_checks: int = 0
	var total_failures: PackedStringArray = PackedStringArray()

	print("\n===== UNIT TESTS (%s) =====" % _SUITE_DIR)
	for path: String in suite_paths:
		var suite_name: String = path.get_file()
		var script: GDScript = load(path) as GDScript
		if script == null:
			total_failures.push_back("%s: failed to load as a GDScript" % suite_name)
			continue
		var instance: Variant = script.new()
		if not is_instance_of(instance, UnitTest):
			total_failures.push_back("%s: does not extend UnitTest" % suite_name)
			continue
		var suite: UnitTest = instance
		var test_methods: Array[String] = _test_methods(suite)
		if test_methods.is_empty():
			total_failures.push_back("%s: no test_* methods found" % suite_name)
			continue
		suite_count += 1
		for method_name: String in test_methods:
			suite.callv(method_name, [])
		total_checks += suite.check_count()
		for failure: String in suite.failures():
			var line: String = "%s: %s" % [suite_name, failure]
			total_failures.push_back(line)
			print("  FAIL " + line)

	var verdict: String = "ALL PASS" if total_failures.is_empty() else "FAILURES"
	print("  %d suite(s), %d check(s), %d failure(s) -- %s" % [suite_count, total_checks, total_failures.size(), verdict])
	print("===================================\n")
	quit(0 if total_failures.is_empty() else 1)

func _discover_suites() -> PackedStringArray:
	var out: PackedStringArray = PackedStringArray()
	var dir: DirAccess = DirAccess.open(_SUITE_DIR)
	if dir == null:
		return out
	dir.list_dir_begin()
	var entry: String = dir.get_next()
	while entry != "":
		if not dir.current_is_dir() and entry.ends_with("_test.gd"):
			out.push_back("%s/%s" % [_SUITE_DIR, entry])
		entry = dir.get_next()
	dir.list_dir_end()
	out.sort()
	return out

func _test_methods(suite: UnitTest) -> Array[String]:
	var out: Array[String] = []
	for method_info: Dictionary in suite.get_method_list():
		var method_name: String = method_info.get("name", "")
		if method_name.begins_with("test_"):
			out.push_back(method_name)
	out.sort()
	return out
