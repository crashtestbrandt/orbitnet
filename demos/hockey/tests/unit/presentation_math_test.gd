extends UnitTest
## The two pure numbers the presentation layer is built on: the team-mate fade, and the wire's own floor.

# --- the team-mate fade ----------------------------------------------------------------------------
# Mallets do not collide, so two team-mates can stand in the same place and the one in front would hide the one
# behind exactly when a player most needs to see their own.

func test_a_distant_mallet_is_solid() -> void:
	assert_almost_eq(MalletRenderer.fade_alpha(HockeyConfig.FADE_START), 1.0, 0.0001,
		"at the fade distance a mallet is fully drawn")
	assert_almost_eq(MalletRenderer.fade_alpha(HockeyConfig.HALF_LENGTH), 1.0, 0.0001,
		"and further away it stays that way")

func test_the_fade_is_monotonic() -> void:
	var previous: float = 0.0
	for step: int in 21:
		var distance: float = HockeyConfig.FADE_START * float(step) / 20.0
		var alpha: float = MalletRenderer.fade_alpha(distance)
		assert_true(alpha >= previous - 0.0001, "alpha never dips as the mallets separate")
		previous = alpha

func test_it_never_reaches_zero() -> void:
	# A mallet you cannot see at all is worse than one you can see through, because the puck still bounces off
	# it.
	assert_almost_eq(MalletRenderer.fade_alpha(0.0), HockeyConfig.FADE_FLOOR, 0.0001,
		"perfectly overlapped is the floor, not invisible")
	assert_true(HockeyConfig.FADE_FLOOR > 0.0, "and the floor is above nothing")

func test_a_negative_distance_is_clamped() -> void:
	assert_almost_eq(MalletRenderer.fade_alpha(-1.0), HockeyConfig.FADE_FLOOR, 0.0001,
		"a distance cannot be negative, and if one arrives it reads as fully overlapped")

# --- the wire's own floor ---------------------------------------------------------------------------
# `net_pos` rides as three IEEE-754 binary16s. A correction cannot be measured below their spacing, so the HUD
# prints the floor beside the number rather than letting a reader mistake it for noise.

func test_the_half_float_spacing_matches_the_format() -> void:
	# binary16 carries a 10-bit significand, so the spacing at a magnitude in [2^e, 2^(e+1)) is 2^(e-10).
	assert_almost_eq(HockeyHud.half_float_ulp_mm(1.0), 1000.0 / 1024.0, 0.0001, "at 1 m, just under a mm")
	assert_almost_eq(HockeyHud.half_float_ulp_mm(1.5), 1000.0 / 1024.0, 0.0001, "and anywhere in [1, 2)")
	assert_almost_eq(HockeyHud.half_float_ulp_mm(2.0), 2000.0 / 1024.0, 0.0001, "doubling at every octave")
	assert_almost_eq(HockeyHud.half_float_ulp_mm(0.5), 500.0 / 1024.0, 0.0001, "and halving below one")

func test_the_floor_is_smaller_than_the_puck() -> void:
	# The table scale is chosen so the quantization floor is not the number being reported. Ten times this
	# table and it would be.
	var floor_mm: float = HockeyHud.half_float_ulp_mm(HockeyConfig.HALF_LENGTH)
	assert_true(floor_mm < HockeyConfig.PUCK_RADIUS * 1000.0 * 0.1,
		"the wire's resolution is well under a tenth of the puck, so a correction worth seeing is measurable")

func test_zero_has_no_spacing_to_report() -> void:
	assert_almost_eq(HockeyHud.half_float_ulp_mm(0.0), 0.0, 0.0001,
		"log2(0) is not a number, so the origin answers zero rather than an infinity")
