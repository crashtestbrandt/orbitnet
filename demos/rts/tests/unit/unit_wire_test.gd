extends UnitTest
## The unit wire schema: the net_meta bitfield and the net_aux (sin, cos, hp01) packing.
##
## These are static functions on UnitBody, so the whole schema is testable with no node, no scene and no
## session -- which matters because a packing bug is invisible at runtime. A target index shifted by one bit
## does not crash; it makes units shoot at the wrong enemy on clients only.

func test_meta_round_trips() -> void:
	var meta: int = UnitBody.pack_meta(true, 42, 1234)
	assert_true(UnitBody.meta_alive(meta), "liveness survives")
	assert_eq(UnitBody.meta_target(meta), 42, "the target survives")
	assert_eq(UnitBody.meta_seq(meta), 1234, "the order sequence survives")

func test_no_target_is_distinct_from_target_zero() -> void:
	# The whole reason the target is stored offset by one. A freshly zeroed entity (or a decode of an all-zero
	# row) must read as "no target", not as "targeting unit 0" -- otherwise every unit that has not acquired
	# anything yet appears to be aiming at the first unit in the pool.
	var none: int = UnitBody.pack_meta(true, -1, 0)
	var zero: int = UnitBody.pack_meta(true, 0, 0)
	assert_eq(UnitBody.meta_target(none), -1, "-1 means no target")
	assert_eq(UnitBody.meta_target(zero), 0, "and unit 0 is a real, distinguishable target")
	assert_true(none != zero, "the two encodings differ")

func test_dead_and_alive_differ_only_in_the_liveness_bit() -> void:
	var alive: int = UnitBody.pack_meta(true, 7, 99)
	var dead: int = UnitBody.pack_meta(false, 7, 99)
	assert_false(UnitBody.meta_alive(dead), "the dead encoding reads as dead")
	assert_eq(UnitBody.meta_target(dead), 7, "without disturbing the target field")
	assert_eq(UnitBody.meta_seq(dead), 99, "or the sequence field")

func test_fields_do_not_bleed_into_each_other() -> void:
	# Every field at its maximum at once. If any width is wrong, one of these reads back changed.
	var meta: int = UnitBody.pack_meta(true, RtsConfig.UNIT_COUNT - 1, 65535)
	assert_true(UnitBody.meta_alive(meta), "liveness is above both fields")
	assert_eq(UnitBody.meta_target(meta), RtsConfig.UNIT_COUNT - 1, "the largest valid target fits")
	assert_eq(UnitBody.meta_seq(meta), 65535, "the largest sequence fits")

func test_the_sequence_wraps_rather_than_corrupting_neighbors() -> void:
	# A sequence is only ever COMPARED for change, never ordered globally, so wrapping is fine -- but it must
	# wrap inside its own field rather than carrying into the liveness bit.
	var meta: int = UnitBody.pack_meta(true, 3, 65536 + 5)
	assert_eq(UnitBody.meta_seq(meta), 5, "the sequence wraps within 16 bits")
	assert_eq(UnitBody.meta_target(meta), 3, "and the target is untouched")
	assert_true(UnitBody.meta_alive(meta), "as is liveness")

# --- the facing packing ----------------------------------------------------------------------------
# Facing goes as (sin, cos), not as an angle. Two reasons, and this suite pins both.
func test_facing_round_trips_through_the_aux_vector() -> void:
	for angle: float in [0.0, 0.5, 1.5, PI * 0.5, -PI * 0.5, 2.5, -2.5]:
		var aux: Vector3 = Vector3(sin(angle), cos(angle), 1.0)
		assert_almost_eq(UnitBody.aux_facing(aux), angle, 0.0001, "facing %f survives the packing" % angle)

func test_facing_survives_the_pi_wrap() -> void:
	# The reason an angle scalar would be wrong: interpolating from +3.13 to -3.13 as NUMBERS sweeps the long
	# way round through zero -- a unit facing roughly south spins a full turn every time it wobbles. As a
	# point on a circle, the same two values are adjacent.
	var a: float = PI - 0.01
	var b: float = -PI + 0.01
	var aux_a: Vector3 = Vector3(sin(a), cos(a), 1.0)
	var aux_b: Vector3 = Vector3(sin(b), cos(b), 1.0)
	assert_true(aux_a.distance_to(aux_b) < 0.05,
		"two nearly-identical headings either side of the wrap are NEIGHBORS on the unit circle")
	assert_true(absf(a - b) > 6.0,
		"...while as raw angles they are almost a full rotation apart, which is what would be interpolated")

func test_half_precision_keeps_facing_usable() -> void:
	# @half is IEEE-754 binary16: ~3 decimal digits over [-1, 1]. Simulate the quantization and check the
	# recovered angle is still far tighter than anything a viewer could notice.
	for angle: float in [0.0, 0.7, -2.2, 3.0]:
		var quantized: Vector3 = Vector3(_to_half(sin(angle)), _to_half(cos(angle)), 1.0)
		var recovered: float = UnitBody.aux_facing(quantized)
		assert_almost_eq(wrapf(recovered - angle, -PI, PI), 0.0, 0.01,
			"half precision costs well under a degree of facing at %f" % angle)

func test_hp_survives_the_third_component() -> void:
	for hp: float in [0.0, 0.25, 0.5, 1.0]:
		assert_almost_eq(UnitBody.aux_hp01(Vector3(0.0, 1.0, hp)), hp, 0.0001, "hp %f survives" % hp)
	assert_almost_eq(UnitBody.aux_hp01(Vector3(0.0, 1.0, 4.0)), 1.0, 0.0001,
		"a wire value outside [0,1] is clamped rather than driving a health bar off the end of itself")

# Round a float to the nearest IEEE-754 binary16 value, using only the mantissa truncation that matters over
# [-1, 1]. Enough to model the quantization error the wire actually introduces.
func _to_half(value: float) -> float:
	if value == 0.0:
		return 0.0
	var magnitude: float = absf(value)
	var exponent: int = floori(log(magnitude) / log(2.0))
	var step: float = pow(2.0, float(exponent - 10))   # binary16 carries 10 explicit mantissa bits
	return signf(value) * round(magnitude / step) * step
