extends UnitTest
## The netbench seam: BenchSubject's contract and BenchPolicy's purity.
##
## netbench used to name a specific game's classes -- a player body, a session manager, a game autoload. It now
## reaches a game through BenchSubject and nothing else. These cases pin the properties that decoupling
## depends on, because they are the ones that would quietly regress the moment someone "just" reached for a
## concrete type again.

# --- the vocabulary -------------------------------------------------------------------------------
func test_the_field_helpers_read_typed_values() -> void:
	var frame: Dictionary = {
		BenchSubject.KEY_TRANSLATE: Vector3(1.0, 0.0, -1.0),
		BenchSubject.KEY_FIRE: true,
		BenchSubject.KEY_RECONCILE_ERROR: 0.25,
	}
	assert_vec_almost_eq(BenchSubject.vec3_field(frame, BenchSubject.KEY_TRANSLATE), Vector3(1.0, 0.0, -1.0),
		0.0001, "a Vector3 field reads back")
	assert_true(BenchSubject.bool_field(frame, BenchSubject.KEY_FIRE), "a bool field reads back")
	assert_almost_eq(BenchSubject.float_field(frame, BenchSubject.KEY_RECONCILE_ERROR), 0.25, 0.0001,
		"a float field reads back")

func test_missing_fields_fall_back_rather_than_erroring() -> void:
	# A frame from an OLDER tape will not carry a key added since. That must keep the subject's own default,
	# not crash the replay -- which is what makes a recorded tape a durable regression asset.
	var empty: Dictionary = {}
	assert_vec_almost_eq(BenchSubject.vec3_field(empty, BenchSubject.KEY_TRANSLATE, Vector3.UP), Vector3.UP,
		0.0001, "an absent Vector3 falls back")
	assert_true(BenchSubject.bool_field(empty, BenchSubject.KEY_FIRE, true), "an absent bool falls back")
	assert_almost_eq(BenchSubject.float_field(empty, "nope", 3.5), 3.5, 0.0001, "an absent float falls back")

func test_wrong_typed_fields_fall_back_rather_than_erroring() -> void:
	# A frame can arrive from a file on disk. A corrupt or hand-edited value must degrade, not propagate.
	var junk: Dictionary = {
		BenchSubject.KEY_TRANSLATE: "north",
		BenchSubject.KEY_FIRE: 17,
		BenchSubject.KEY_RECONCILE_ERROR: Vector3.ZERO,
	}
	assert_vec_almost_eq(BenchSubject.vec3_field(junk, BenchSubject.KEY_TRANSLATE), Vector3.ZERO, 0.0001,
		"a string is not a Vector3")
	assert_false(BenchSubject.bool_field(junk, BenchSubject.KEY_FIRE), "an int is not a bool")
	assert_almost_eq(BenchSubject.float_field(junk, BenchSubject.KEY_RECONCILE_ERROR), 0.0, 0.0001,
		"a Vector3 is not a float")

func test_an_int_is_accepted_where_a_float_is_wanted() -> void:
	# `{"x": 0}` in a literal is an int, and so is a round-tripped 0.0 in some encodings. Refusing it would
	# make a perfectly good sample read as zero for a reason nobody would ever guess.
	assert_almost_eq(BenchSubject.float_field({"x": 4}, "x"), 4.0, 0.0001, "an int reads as a float")

func test_the_base_subject_is_inert_but_safe() -> void:
	# The default implementations must be callable. BenchProbe and BenchBot call every one of them before a
	# game has necessarily provided anything, and a base class that errored would make "attach the bench
	# first, implement the subject second" impossible.
	var subject: BenchSubject = BenchSubject.new()
	assert_false(subject.is_ready(), "a bare subject is never ready")
	assert_true(subject.local_body() == null, "and has no body")
	assert_true(subject.capture_input().is_empty(), "recording it yields nothing")
	assert_true(subject.sample(null).is_empty(), "and it contributes no metrics")
	subject.apply_input({BenchSubject.KEY_FIRE: true})   # must not error
	subject.release()

# --- policy purity --------------------------------------------------------------------------------
func test_a_policy_is_a_pure_function() -> void:
	var a: Dictionary = BenchPolicy.frame(BenchPolicy.Policy.WANDER, 3.25, 7)
	var b: Dictionary = BenchPolicy.frame(BenchPolicy.Policy.WANDER, 3.25, 7)
	assert_vec_almost_eq(BenchSubject.vec3_field(a, BenchSubject.KEY_TRANSLATE),
		BenchSubject.vec3_field(b, BenchSubject.KEY_TRANSLATE), 0.0,
		"the same (policy, t, seed) yields a BIT-IDENTICAL frame -- no RNG state carried between calls")

func test_seeds_de_correlate_a_fleet() -> void:
	# Without a per-seed phase offset, N bots are one correlated waveform and the bench measures a synchronized
	# burst rather than N independent clients.
	var a: Vector3 = BenchSubject.vec3_field(
		BenchPolicy.frame(BenchPolicy.Policy.STRAFE, 1.0, 1), BenchSubject.KEY_TRANSLATE)
	var b: Vector3 = BenchSubject.vec3_field(
		BenchPolicy.frame(BenchPolicy.Policy.STRAFE, 1.0, 2), BenchSubject.KEY_TRANSLATE)
	assert_true(a.distance_to(b) > 0.001, "two seeds diverge at the same instant")

func test_idle_really_is_idle() -> void:
	var frame: Dictionary = BenchPolicy.frame(BenchPolicy.Policy.IDLE, 12.0, 3)
	assert_vec_almost_eq(BenchSubject.vec3_field(frame, BenchSubject.KEY_TRANSLATE), Vector3.ZERO, 0.0,
		"the idle baseline authors no translation")
	assert_false(BenchSubject.bool_field(frame, BenchSubject.KEY_FIRE), "and never fires")

func test_every_policy_emits_the_full_vocabulary() -> void:
	# A frame missing a key would silently take the subject's default rather than the policy's intent, which
	# is the sort of thing that makes one policy quietly behave like another.
	for name: String in BenchPolicy.names():
		var frame: Dictionary = BenchPolicy.frame(BenchPolicy.policy_from_name(name), 2.0, 5)
		assert_true(frame.has(BenchSubject.KEY_TRANSLATE), "%s sets translate" % name)
		assert_true(frame.has(BenchSubject.KEY_ROTATE), "%s sets rotate" % name)
		assert_true(frame.has(BenchSubject.KEY_AIM_DIR), "%s sets aim_dir" % name)
		assert_true(frame.has(BenchSubject.KEY_FIRE), "%s sets fire" % name)

func test_translation_stays_bounded() -> void:
	# A policy authors INTENT in [-1, 1]; a game scales it. A policy that exceeded that would silently give
	# bots superhuman input and make every bench number optimistic.
	for name: String in BenchPolicy.names():
		var policy: BenchPolicy.Policy = BenchPolicy.policy_from_name(name)
		for step: int in 200:
			var frame: Dictionary = BenchPolicy.frame(policy, float(step) * 0.05, 11)
			var translate: Vector3 = BenchSubject.vec3_field(frame, BenchSubject.KEY_TRANSLATE)
			assert_true(absf(translate.x) <= 1.001 and absf(translate.z) <= 1.001,
				"%s keeps translation intent within [-1, 1]" % name)

func test_unknown_policy_names_default_rather_than_failing() -> void:
	assert_eq(BenchPolicy.policy_from_name("nonsense"), BenchPolicy.Policy.STRAFE,
		"an unknown name falls back to the workhorse policy")
	assert_false(BenchPolicy.has_policy("nonsense"), "but has_policy still reports the truth, so a harness can validate first")
	assert_true(BenchPolicy.has_policy("strafe_fire"), "a real name is recognized")
	assert_eq(BenchPolicy.policy_from_name("  FIRE  "), BenchPolicy.Policy.STRAFE_FIRE,
		"names are trimmed and case-folded, and 'fire' is an alias")
