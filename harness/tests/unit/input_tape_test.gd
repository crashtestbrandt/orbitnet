extends UnitTest
## InputTape: the record/replay codec.
##
## A tape is a regression asset -- record a session once, replay it under every impairment profile forever.
## That only works if the round trip is lossless and if a tape written before a vocabulary change still loads.

func _frame(x: float, fire: bool) -> Dictionary:
	return {
		BenchSubject.KEY_TRANSLATE: Vector3(x, 0.0, -x),
		BenchSubject.KEY_ROTATE: Vector3(0.0, x * 0.5, 0.0),
		BenchSubject.KEY_AIM_DIR: Vector3(0.0, 0.0, -1.0),
		BenchSubject.KEY_AIM_HELD: not fire,
		BenchSubject.KEY_FIRE: fire,
	}

func test_a_tape_round_trips_losslessly() -> void:
	var tape: InputTape = InputTape.new()
	for index: int in 32:
		tape.record(_frame(float(index) * 0.1, index % 3 == 0))
	var bytes: PackedByteArray = tape.encode()

	var restored: InputTape = InputTape.new()
	assert_true(restored.decode(bytes), "a well-formed tape decodes")
	assert_eq(restored.length(), tape.length(), "with every frame")
	for index: int in tape.length():
		var before: Dictionary = tape.frame_at(index)
		var after: Dictionary = restored.frame_at(index)
		assert_vec_almost_eq(BenchSubject.vec3_field(after, BenchSubject.KEY_TRANSLATE),
			BenchSubject.vec3_field(before, BenchSubject.KEY_TRANSLATE), 0.0,
			"frame %d's translation survives EXACTLY -- a tape must not quantize" % index)
		assert_eq(BenchSubject.bool_field(after, BenchSubject.KEY_FIRE),
			BenchSubject.bool_field(before, BenchSubject.KEY_FIRE), "frame %d's fire flag survives" % index)

func test_recording_copies_rather_than_aliasing() -> void:
	# The common shape on the capture side is a body that reuses ONE input buffer every tick. If record()
	# stored the reference, every frame in the tape would end up identical to the last one -- and the tape
	# would look plausible while being useless.
	var tape: InputTape = InputTape.new()
	var live: Dictionary = _frame(1.0, false)
	tape.record(live)
	live[BenchSubject.KEY_TRANSLATE] = Vector3(99.0, 0.0, 0.0)
	tape.record(live)
	assert_vec_almost_eq(BenchSubject.vec3_field(tape.frame_at(0), BenchSubject.KEY_TRANSLATE),
		Vector3(1.0, 0.0, -1.0), 0.0001,
		"mutating the source frame cannot retroactively rewrite what was already recorded")

func test_an_empty_frame_is_not_recorded() -> void:
	# An empty frame is the vocabulary's RELEASE signal, not input. Recording it would put a spurious "stop
	# driving" frame in the middle of a tape.
	var tape: InputTape = InputTape.new()
	tape.record({})
	assert_eq(tape.length(), 0, "the release signal is not input")

func test_an_exhausted_tape_reads_as_empty() -> void:
	# The replay driver treats empty as "tape finished", which is ALSO the release signal -- so a tape running
	# out hands the body back to live input by construction rather than by a special case.
	var tape: InputTape = InputTape.new()
	tape.record(_frame(1.0, false))
	assert_false(tape.frame_at(0).is_empty(), "frame 0 exists")
	assert_true(tape.frame_at(1).is_empty(), "one past the end is empty")
	assert_true(tape.frame_at(-1).is_empty(), "and so is a negative index")

func test_a_forward_compatible_frame_survives() -> void:
	# A key the vocabulary does not know yet must ride through untouched, so a tape recorded by a newer build
	# is still replayable by an older one (the unknown key is simply ignored by apply_input).
	var tape: InputTape = InputTape.new()
	tape.record({BenchSubject.KEY_FIRE: true, "future_field": 42})
	var restored: InputTape = InputTape.new()
	assert_true(restored.decode(tape.encode()), "it decodes")
	assert_true(restored.frame_at(0).has("future_field"), "and the unknown key is preserved, not dropped")

# --- rejecting junk -------------------------------------------------------------------------------
func test_junk_is_rejected_rather_than_misparsed() -> void:
	var tape: InputTape = InputTape.new()
	assert_false(tape.decode(PackedByteArray()), "empty bytes are not a tape")
	assert_eq(tape.length(), 0, "and leave the tape empty")

func test_a_non_tape_blob_is_rejected() -> void:
	# The magic key exists so a stray file -- a screenshot, a save, half a download -- is REFUSED rather than
	# silently decoded into a plausible-looking empty tape.
	var tape: InputTape = InputTape.new()
	assert_false(tape.decode(var_to_bytes({"hello": "world"})), "a dictionary without the magic is not a tape")
	assert_false(tape.decode(var_to_bytes([1, 2, 3])), "nor is an array")
	assert_false(tape.decode(var_to_bytes("just a string")), "nor a string")

func test_a_tape_with_a_corrupt_frame_list_is_rejected() -> void:
	var tape: InputTape = InputTape.new()
	assert_false(tape.decode(var_to_bytes({"magic": "obnt", "version": 2, "frames": "not a list"})),
		"a frames field that is not an array is corrupt")

func test_non_dictionary_entries_are_skipped() -> void:
	var tape: InputTape = InputTape.new()
	var blob: Dictionary = {"magic": "obnt", "version": 2, "frames": [{"a": 1}, 7, "x", {"b": 2}]}
	assert_true(tape.decode(var_to_bytes(blob)), "the tape as a whole is well-formed")
	assert_eq(tape.length(), 2, "and only the real frames survive")
