extends UnitTest
## The per-target rewind policy, over NetLagComp's static surface. The demo's signature feature, and it is
## entirely pure -- band selection and depth arithmetic need no session.
##
## THESE STATICS ARE PROCESS-WIDE, so every test that writes one restores it. A suite that left a band scale
## behind would change what the next suite measured, and the failure would land somewhere else.

const TICK_HZ: float = float(ArenaConfig.NET_TICK_HZ)
const PRESENT: int = 5000

func _reset() -> void:
	NetLagComp.reset_observed_interp()

# --- band selection ------------------------------------------------------------------------------------
func test_band_edges_are_thirds_of_the_scale() -> void:
	var scale: float = 90.0
	var origin: Vector3 = Vector3.ZERO
	assert_eq(NetLagComp.band_for(origin, Vector3(1.0, 0.0, 0.0), scale), NetLagComp.Band.NEAR,
		"a target in your face is NEAR")
	assert_eq(NetLagComp.band_for(origin, Vector3(45.0, 0.0, 0.0), scale), NetLagComp.Band.MID,
		"past scale/3 is MID")
	assert_eq(NetLagComp.band_for(origin, Vector3(89.0, 0.0, 0.0), scale), NetLagComp.Band.FAR,
		"past 2*scale/3 is FAR")

func test_an_unconfigured_scale_bands_everything_near() -> void:
	# The rule is duplicated from the backend's own `band_of`, and 0 means the backend bands every row near --
	# so the split describes nothing and the per-target rewind correctly degenerates to the flat window.
	assert_eq(NetLagComp.band_for(Vector3.ZERO, Vector3(500.0, 0.0, 0.0), 0.0), NetLagComp.Band.NEAR,
		"with no scale there are no bands")
	assert_eq(NetLagComp.band_for(Vector3.ZERO, Vector3(500.0, 0.0, 0.0), -3.0), NetLagComp.Band.NEAR,
		"and a negative one is not a scale either")

func test_the_configured_scale_bands_an_arena_meaningfully() -> void:
	# Sized to an ARENA rather than to the session: a scale large enough to span three arenas would put every
	# body in one band and the per-band measurements would all read the same.
	var scale: float = ArenaConfig.BAND_SCALE_M
	var across: Vector3 = Vector3(ArenaConfig.ARENA_HALF_X * 2.0, 0.0, 0.0)
	assert_eq(NetLagComp.band_for(Vector3.ZERO, across, scale), NetLagComp.Band.FAR,
		"a shot across the whole arena is a FAR shot")
	assert_eq(NetLagComp.band_for(Vector3.ZERO, Vector3(2.0, 0.0, 0.0), scale), NetLagComp.Band.NEAR,
		"and one at two meters is NEAR")

# --- the window ------------------------------------------------------------------------------------------
func test_the_authority_rewinds_nothing() -> void:
	# A listen host renders the bodies it is simulating, live: no round trip to itself and no interpolation
	# delay to what it is drawing. Feeding it through the formula would give it a tick of interpolation it
	# does not have.
	_reset()
	assert_eq(NetLagComp.rewind_ticks_for_peer_shot(true, 1, 40.0, TICK_HZ), 0,
		"the host takes no rewind on its own shots")

func test_a_remote_shooter_earns_a_window_from_its_round_trip() -> void:
	_reset()
	var shallow: int = NetLagComp.rewind_ticks_for_peer_shot(false, 2, 20.0, TICK_HZ)
	var deep: int = NetLagComp.rewind_ticks_for_peer_shot(false, 3, 160.0, TICK_HZ)
	assert_true(deep > shallow, "a worse link earns a deeper rewind, which is the whole point of per-shooter")

func test_the_window_is_bounded() -> void:
	_reset()
	var absurd: int = NetLagComp.rewind_ticks_for_peer_shot(false, 2, 100000.0, TICK_HZ)
	var cap: int = NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, TICK_HZ)
	assert_eq(absurd, cap,
		"max_delay_ms bounds the deepest rewind the game will grant anyone, however bad a link claims to be")

func test_no_round_trip_estimate_falls_back_rather_than_reading_zero() -> void:
	_reset()
	var unknown: int = NetLagComp.rewind_ticks_for_peer_shot(false, 2, -1.0, TICK_HZ)
	assert_eq(unknown, NetLagComp.rewind_ticks_for(NetLagComp.delay_ms, TICK_HZ),
		"a fresh joiner gets the flat window, not the shallowest one in the session at the moment its link "
		+ "is least settled")

# --- the three ticks a shot resolves at ------------------------------------------------------------------
func test_the_band_array_has_one_entry_per_band() -> void:
	_reset()
	var ticks: PackedInt64Array = NetLagComp.rewind_band_ticks(PRESENT, false, 2, 50.0, TICK_HZ)
	assert_eq(ticks.size(), 3, "resolve_hit() reads exactly three, indexed by Band")

func test_the_ticks_are_absolute_and_at_or_before_the_present() -> void:
	_reset()
	var ticks: PackedInt64Array = NetLagComp.rewind_band_ticks(PRESENT, false, 2, 50.0, TICK_HZ)
	for band: int in 3:
		assert_true(ticks[band] <= PRESENT, "band %d resolves at or before the present" % band)

func test_the_authority_resolves_every_band_at_the_present() -> void:
	_reset()
	var ticks: PackedInt64Array = NetLagComp.rewind_band_ticks(PRESENT, true, 1, 40.0, TICK_HZ)
	for band: int in 3:
		assert_eq(ticks[band], PRESENT, "band %d is a live cast for the host" % band)

func test_with_no_band_measurement_the_three_depths_agree() -> void:
	# The degenerate case, and it is the CORRECT one: with no evidence the scale is 1.0 and the per-target
	# rewind is exactly the flat per-shooter window. It must not invent a spread.
	_reset()
	var ticks: PackedInt64Array = NetLagComp.rewind_band_ticks(PRESENT, false, 2, 60.0, TICK_HZ)
	assert_eq(ticks[NetLagComp.Band.NEAR], ticks[NetLagComp.Band.FAR],
		"no band measurement, no spread")

func test_a_staler_far_band_earns_a_deeper_rewind_than_near() -> void:
	_reset()
	# The send path measured the far band arriving four times less often than the pooled average and the near
	# band arriving as often as it. That is the evidence a per-target rewind exists to act on.
	NetLagComp.refresh_band_interp(1.0, 2.0, 4.0, 1.0, ArenaConfig.BAND_SCALE_M)
	var ticks: PackedInt64Array = NetLagComp.rewind_band_ticks(PRESENT, false, 2, 60.0, TICK_HZ)
	assert_true(ticks[NetLagComp.Band.FAR] < ticks[NetLagComp.Band.NEAR],
		"the far target is reconstructed FURTHER back, because its rows are staler on the shooter's screen")
	assert_true(ticks[NetLagComp.Band.MID] <= ticks[NetLagComp.Band.NEAR],
		"and the mid band sits between them")
	_reset()

func test_a_band_with_no_measurement_falls_back_to_the_pooled_figure() -> void:
	_reset()
	NetLagComp.refresh_band_interp(1.0, 0.0, 4.0, 1.0, ArenaConfig.BAND_SCALE_M)
	assert_almost_eq(NetLagComp.band_interp_scale(NetLagComp.Band.MID), 1.0, 0.0001,
		"a band that published nothing is not evidence of a fast band")
	_reset()

func test_the_retained_ring_is_bounded_by_duration_not_slots() -> void:
	# The ring is 128 slots, which is a different DURATION at every rate -- 4.27 s at this demo's 30 Hz.
	# Bounding residency instead is what makes a freed body's capsules provably gone.
	var retain: int = NetLagComp.retain_ticks(ArenaConfig.NET_TICK_HZ)
	assert_true(retain > 0, "something is retained")
	assert_true(retain < 128, "but not the whole ring, at this rate")
	assert_true(retain >= NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, TICK_HZ),
		"and never less than the deepest window the policy can grant, or a legal rewind would find no slot")
