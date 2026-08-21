extends UnitTest
## Scene-free coverage of [NetLagComp]: the analytic ray-vs-capsule math, the tick ring's bookkeeping and
## residency, and the rewind policy -- the depth a shot is resolved at.
##
## `resolve_hit` IS reached here, with a null `space`, which [NetRay.cast] answers with a miss -- so what these
## assertions read is purely the analytic capsule half. What a null space cannot reach is the static-mask
## remainder (walls, cast live against a real physics world), and that dispatch stays covered by
## a scene-bound probe against a real physics world. A consuming game should keep one, and should mirror this
## case geometry in it so the two agree on the same numbers.

## The collision layer a game puts its per-region hitboxes on. Any single bit works here: the suite only needs
## the resolver to be handed a mask it can match, and a plain [Node3D] to stand in for the collider a game
## would really record -- [member NetLagComp.Sample.collider] is an [Object] precisely so the backend never
## names a game's type.
const _HITBOX_LAYER: int = 1 << 4

func test_ray_hits_the_cylinder_wall_front_face() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3(10.0, 0.0, 0.0))
	var t: float = NetLagComp._ray_capsule(Vector3(10.0, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 10.0, xf, 0.3, 1.0)
	assert_almost_eq(t, 4.7, 1e-4, "front-face cylinder-wall entry distance")

func test_ray_tracks_a_moved_capsule() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3(10.0, 0.0, -2.0))
	var t: float = NetLagComp._ray_capsule(Vector3(10.0, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 10.0, xf, 0.3, 1.0)
	assert_almost_eq(t, 6.7, 1e-4, "the recorded pose's transform, not a live position, drives the hit")

func test_ray_misses_an_off_axis_capsule() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3(10.0, 0.0, 0.0))
	var t: float = NetLagComp._ray_capsule(Vector3(10.0, 5.0, 5.0), Vector3(0.0, 0.0, -1.0), 10.0, xf, 0.3, 1.0)
	assert_true(t < 0.0, "a ray 5 m off the capsule's axis clears both the cylinder and the hemisphere caps")

func test_ray_hits_the_top_hemisphere_cap() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3.ZERO)
	# Straight down the axis: a > 0 degenerates for the cylinder test, so only the caps can register.
	var t: float = NetLagComp._ray_capsule(Vector3(0.0, 2.0, 0.0), Vector3(0.0, -1.0, 0.0), 10.0, xf, 0.3, 1.0)
	assert_almost_eq(t, 1.5, 1e-4, "entry at the top cap's apex: half-height (0.2) + radius (0.3) below the origin")

func test_ray_beyond_dist_does_not_register() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3(10.0, 0.0, 0.0))
	var t: float = NetLagComp._ray_capsule(Vector3(10.0, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 4.0, xf, 0.3, 1.0)
	assert_true(t < 0.0, "the true entry (4.7) is past the query's max distance (4.0) -- must miss, not clip")

func test_capsule_normal_on_the_cylinder_wall() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3(10.0, 0.0, 0.0))
	var hit_pos: Vector3 = Vector3(10.0, 0.0, 0.3)   # the case-1 entry point
	var n: Vector3 = NetLagComp._capsule_normal(hit_pos, xf, 0.3, 1.0)
	assert_vec_almost_eq(n, Vector3(0.0, 0.0, 1.0), 1e-4, "radial from the nearest axis point, not from the capsule centre")

func test_capsule_normal_on_the_hemisphere_cap() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3.ZERO)
	var hit_pos: Vector3 = Vector3(0.0, 0.5, 0.0)   # the case-4 apex entry point
	var n: Vector3 = NetLagComp._capsule_normal(hit_pos, xf, 0.3, 1.0)
	assert_vec_almost_eq(n, Vector3(0.0, 1.0, 0.0), 1e-4, "from the clamped cap centre, not the capsule centre")

# --- tick ring bookkeeping (record/has_tick; pure -- no physics space involved) -------------------------
func test_ring_records_exactly_the_queried_ticks() -> void:
	var lc: NetLagComp = NetLagComp.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return []
	lc.record(5)
	lc.record(6)
	assert_true(lc.has_tick(5), "tick 5 was recorded")
	assert_true(lc.has_tick(6), "tick 6 was recorded")
	assert_false(lc.has_tick(7), "tick 7 was never recorded")
	assert_false(lc.has_tick(-1), "a negative tick is never valid")

func test_record_without_a_provider_is_a_noop() -> void:
	var lc: NetLagComp = NetLagComp.new()
	lc.record(5)   # hittable_provider left unset (Callable())
	assert_false(lc.has_tick(5), "no provider -> the ring stays empty")

func test_ring_wraparound_guards_a_stale_slot_read() -> void:
	var lc: NetLagComp = NetLagComp.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return []
	lc.record(5)
	lc.record(5 + 128)   # _RING_SIZE (documented on NetLagComp) -- same slot, a different tick
	assert_false(lc.has_tick(5), "the slot now holds a different tick; the stale read is guarded, not stale-hit")
	assert_true(lc.has_tick(5 + 128), "the fresh tick in that slot reads back correctly")

# --- freed-collider safety (the death/respawn host crash) -----------------------------------------------
# `Sample.collider` is a RAW reference and a Node reference keeps nothing alive, so the rewind window's snapshots
# that named a body outlive its despawn: for a few ticks after every death the ring holds dangling pointers.
# record() checks liveness at CAPTURE time; _resolve_rewound is the only READ of the ring and must check again,
# because an EXPORTED build performs no liveness validation when a freed Object reference is dereferenced
# (Variant hands back the stored pointer; the "object was freed" guard is DEBUG_ENABLED-only). Without the guard,
# `as`-casting the sample, reading its RID, or returning it as hit.collider -- whose caller then reads `.name` and
# applies damage through `.health` -- all touch freed memory on the host. `space` is null in these cases, which
# NetRay.cast answers with a miss, so the analytic capsule half is what the assertions read.
func _capsule_sample(collider: Object, origin: Vector3) -> NetLagComp.Sample:
	var sample: NetLagComp.Sample = NetLagComp.Sample.new()
	sample.collider = collider
	sample.transform = Transform3D(Basis.IDENTITY, origin)
	sample.radius = 0.5
	sample.height = 2.0
	return sample

# A ring holding `samples` at tick 10, ready to be rewound to from tick 13.
func _recorded_ring(samples: Array[NetLagComp.Sample]) -> NetLagComp:
	var lc: NetLagComp = NetLagComp.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return samples
	lc.record(10)
	return lc

# A ray from +Z straight down -Z, which passes through a capsule sitting at the origin.
func _rewind(lc: NetLagComp) -> NetRay.Hit:
	return lc.resolve_hit(null, Vector3(0.0, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 20.0, [],
		0xFFFFFFFF, 10, 13, _HITBOX_LAYER)

func test_a_freed_collider_is_dropped_from_the_rewind() -> void:
	var doomed: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([_capsule_sample(doomed, Vector3.ZERO)])
	doomed.free()   # the despawn: the hitbox goes, the recorded slot still names it
	assert_false(_rewind(lc).valid, "a sample whose collider was freed resolves to a miss, never a dangling hit")

func test_a_live_collider_still_resolves() -> void:
	var live: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([_capsule_sample(live, Vector3.ZERO)])
	var hit: NetRay.Hit = _rewind(lc)
	assert_true(hit.valid, "a live recorded capsule is still struck by the rewound ray")
	assert_true(hit.collider == live, "the struck sample's collider is handed back to the caller")
	live.free()

# The freed capsule sits NEARER the muzzle, so without the guard it wins the distance comparison and is the one
# returned -- this pins that liveness is tested before a dead sample can beat a live hit.
func test_a_nearer_freed_collider_never_beats_a_live_one() -> void:
	var doomed: Node3D = Node3D.new()
	var live: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([
		_capsule_sample(doomed, Vector3(0.0, 0.0, 2.0)),
		_capsule_sample(live, Vector3.ZERO),
	])
	doomed.free()
	var hit: NetRay.Hit = _rewind(lc)
	assert_true(hit.valid, "the live capsule behind the freed one still resolves")
	assert_true(hit.collider == live, "the nearer freed sample is skipped rather than returned")
	live.free()

func test_clear_drops_every_recorded_slot() -> void:
	var live: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([_capsule_sample(live, Vector3.ZERO)])
	assert_true(lc.has_tick(10), "the recorded tick is in the ring")
	lc.clear()
	assert_false(lc.has_tick(10), "clear() drops the recorded tick")
	assert_false(_rewind(lc).valid, "a cleared ring resolves as a present-tick miss, not against stale samples")
	live.free()

# The shared ring outlives a session (a host holds one for the process) and the next session restarts tick
# numbering, so a slot left describing the previous session's freed world would answer a colliding tick.
func test_a_cleared_ring_survives_tick_reuse_across_sessions() -> void:
	var first: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([_capsule_sample(first, Vector3.ZERO)])
	lc.clear()
	first.free()   # session teardown freed the world that ring described
	var second: Node3D = Node3D.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return [_capsule_sample(second, Vector3.ZERO)]
	lc.record(10)   # the new session reuses tick 10
	assert_true(_rewind(lc).collider == second, "the reused tick resolves against the NEW session's sample")
	second.free()

# --- broad-phase cull (the firefight cost) --------------------------------------------------------------
# _resolve_rewound runs once per LIVE PELLET TRACK per tick and used to narrow-phase every recorded capsule in the
# zone, so a firefight cost O(rounds in flight x bodies x regions) on the server. The segment-vs-bounding-sphere
# reject in front of it is only sound if it NEVER eats a hit the narrow phase would have found, so these pin the
# side that a too-tight bound breaks -- a capsule the ray reaches only via its far end. `space` is null throughout,
# which NetRay.cast answers with a miss, so what the assertions read is purely the analytic half.
func _sample_at(collider: Object, xform: Transform3D, radius: float, height: float) -> NetLagComp.Sample:
	var sample: NetLagComp.Sample = NetLagComp.Sample.new()
	sample.collider = collider
	sample.transform = xform
	sample.radius = radius
	sample.height = height
	return sample

# A capsule lying ACROSS the ray (axis rotated onto world X) that the ray clips well off its centre. The centre is
# 0.6 m off the ray line -- twice the capsule's 0.3 radius -- so a cull bounded by the RADIUS rather than the
# capsule's half-HEIGHT would reject it, and a real torso shot along the body's long axis would stop registering.
func test_the_cull_keeps_a_capsule_the_ray_reaches_end_on() -> void:
	var live: Node3D = Node3D.new()
	var across: Transform3D = Transform3D(Basis(Vector3.FORWARD, PI * 0.5), Vector3(10.0, 0.0, 0.0))
	var lc: NetLagComp = _recorded_ring([_sample_at(live, across, 0.3, 2.0)])
	var hit: NetRay.Hit = lc.resolve_hit(null, Vector3(10.6, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 20.0, [],
		0xFFFFFFFF, 10, 13, _HITBOX_LAYER)
	assert_true(hit.valid, "a capsule struck 0.6 m off its centre, along its own axis, survives the cull")
	assert_almost_eq(hit.distance, 4.7, 1e-4, "and resolves at the same entry the narrow phase alone would give")
	live.free()

func test_the_cull_drops_a_capsule_the_segment_stops_short_of() -> void:
	var live: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([_capsule_sample(live, Vector3.ZERO)])
	# The true entry is at 4.5 (5.0 - radius 0.5); a 4.0 m segment never gets there.
	var hit: NetRay.Hit = lc.resolve_hit(null, Vector3(0.0, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 4.0, [],
		0xFFFFFFFF, 10, 13, _HITBOX_LAYER)
	assert_false(hit.valid, "a capsule past the segment's end is culled, not clipped to the end")
	live.free()

func test_the_cull_drops_a_capsule_behind_the_muzzle() -> void:
	var live: Node3D = Node3D.new()
	var lc: NetLagComp = _recorded_ring([_capsule_sample(live, Vector3(0.0, 0.0, 10.0))])
	# The capsule sits 5 m BEHIND the origin along the ray; the closest approach clamps to the segment's start.
	var hit: NetRay.Hit = lc.resolve_hit(null, Vector3(0.0, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 20.0, [],
		0xFFFFFFFF, 10, 13, _HITBOX_LAYER)
	assert_false(hit.valid, "a capsule behind the muzzle is culled (the closest approach clamps to t=0)")
	live.free()

# --- perf counters (static, the whole-server figure a diagnostics report reads) -------------------------
func test_perf_counters_sum_samples_and_bracket_the_tick_span() -> void:
	NetLagComp.perf_take_static()   # clear any accumulation the ring-bookkeeping tests above left behind
	var lc: NetLagComp = NetLagComp.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return [NetLagComp.Sample.new(), NetLagComp.Sample.new()]
	lc.record(10)
	lc.record(11)   # two ticks, two Samples each
	var perf: Dictionary[String, int] = NetLagComp.perf_take_static()
	assert_eq(perf["samples"], 4, "both records' Samples sum into the static counter")
	assert_eq(perf["ticks"], 2, "tick span brackets [10, 11] inclusive")
	assert_true(perf["usec"] >= 0, "record() wall-clock accumulates (>= 0)")

func test_perf_take_static_resets_after_read() -> void:
	NetLagComp.perf_take_static()
	var lc: NetLagComp = NetLagComp.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return [NetLagComp.Sample.new()]
	lc.record(20)
	var _first: Dictionary[String, int] = NetLagComp.perf_take_static()
	var second: Dictionary[String, int] = NetLagComp.perf_take_static()
	assert_eq(second["samples"], 0, "read-and-reset: a second take sees no samples")
	assert_eq(second["ticks"], 0, "read-and-reset: the tick span clears too")

# --- the rewind policy: the window is a duration, and a tick is not one --------------------------------
# Nothing anywhere asserted this before. `delay_ticks = 3` shipped documented as "~3 ~= interp/render lag at
# 120 Hz" while the networked loop ran at 60 -- so the shipped value was worth 50 ms, not the 25 ms its own
# comment implied, and flipping to the 30 Hz decoupled tick the 100-player target needs would have doubled it
# again to 100 ms with no line changing. These cases are what makes that impossible to do silently.
func test_the_rewind_window_is_the_same_duration_at_every_tick_rate() -> void:
	assert_eq(NetLagComp.rewind_ticks_for(50.0, 120.0), 6, "50 ms at 120 Hz")
	assert_eq(NetLagComp.rewind_ticks_for(50.0, 60.0), 3, "50 ms at 60 Hz -- what the old delay_ticks = 3 was")
	assert_eq(NetLagComp.rewind_ticks_for(50.0, 30.0), 2,
		"50 ms at 30 Hz rounds to 2 ticks (1.5), NOT the 3 a tick-denominated window would have kept")

func test_zero_ms_is_the_present_tick_boundary() -> void:
	# A scene-bound rewind probe asserts D = 0 => a pure present-tick live cast. That boundary must survive
	# the change of unit, or the one gate covering the dispatch is testing a case that can no longer be reached.
	for hz: float in [30.0, 60.0, 120.0]:
		assert_eq(NetLagComp.rewind_ticks_for(0.0, hz), 0, "0 ms is present-tick at %d Hz" % int(hz))

func test_the_window_is_clamped_in_milliseconds_before_it_becomes_ticks() -> void:
	var ceiling: float = NetLagComp.max_delay_ms
	assert_almost_eq(ceiling, 250.0, 1e-6, "the design ceiling netbench's worst_case profile is calibrated to")
	# Clamping in ms and not in ticks is what makes the ceiling mean the same thing at every rate.
	assert_eq(NetLagComp.rewind_ticks_for(10_000.0, 60.0), NetLagComp.rewind_ticks_for(ceiling, 60.0),
		"an absurd ask clamps to the ceiling at 60 Hz")
	assert_eq(NetLagComp.rewind_ticks_for(10_000.0, 30.0), NetLagComp.rewind_ticks_for(ceiling, 30.0),
		"...and to the SAME duration at 30 Hz, which is what clamping in ms rather than ticks buys")
	assert_eq(NetLagComp.rewind_ticks_for(-5.0, 60.0), 0, "a negative ask is present-tick, never a wrap")

func test_the_window_can_never_outrun_the_ring() -> void:
	# A ring read past its own span returns a slot holding a DIFFERENT tick, which `has_tick` catches -- but
	# only because the depth is bounded here first. Even an absurd rate cannot ask for more than the ring holds.
	assert_true(NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, 100_000.0) <= 127,
		"even an absurd tick rate cannot ask for more history than the ring holds")
	assert_eq(NetLagComp.rewind_ticks_for(100.0, 60.0, 4), 3, "bounded by the ring passed in, not by 128")

func test_degenerate_rates_and_windows_resolve_to_present_tick() -> void:
	for hz: float in [0.0, -60.0, NAN, INF]:
		assert_eq(NetLagComp.rewind_ticks_for(50.0, hz), 0, "an unusable tick rate means no rewind")
	for ms: float in [NAN, -INF]:
		assert_eq(NetLagComp.rewind_ticks_for(ms, 60.0), 0, "an unusable window means no rewind")
	assert_eq(NetLagComp.rewind_ticks_for(INF, 60.0), NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, 60.0),
		"an infinite ask clamps to the ceiling rather than falling back to zero")

# --- ring RESIDENCY: the corpse guarantee, made rate-independent ----------------------------------------
# A game that lingers a corpse before despawning it argues the linger is safe because it exceeds the ring's
# 2.13 s span at 60 Hz, and warns
# in as many words that a tick-rate change would re-arm the window. At 30 Hz a 128-slot ring spans 4.27 s, which
# re-arms it exactly: the ring would still hold a freed body's region capsules. Bounding RESIDENCY by the maximum
# rewindable window instead makes the margin hold at every rate without anyone re-deriving it.
func test_ring_residency_stays_far_inside_the_corpse_linger_at_every_rate() -> void:
	const DEATH_LINGER_S: float = 2.5   # a representative corpse-linger, in seconds
	for hz: int in [30, 60, 120]:
		var retained: int = NetLagComp.retain_ticks(hz)
		var span_s: float = float(retained) / float(hz)
		assert_true(span_s < DEATH_LINGER_S * 0.5,
			"at %d Hz the ring retains %d ticks = %.2fs, which must stay well inside the %.1fs linger" % [
				hz, retained, span_s, DEATH_LINGER_S])
		assert_true(retained > NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, float(hz)),
			"residency must exceed the deepest window a shot can ask for, or a legal rewind finds an empty slot")

func test_the_ring_evicts_ticks_older_than_the_retention_window() -> void:
	var lc: NetLagComp = NetLagComp.new()
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return []
	var retain: int = NetLagComp.retain_ticks(60)
	for tick: int in range(0, retain + 3):
		lc.record(tick, retain)
	assert_true(lc.has_tick(retain + 2), "the newest tick is resident")
	assert_true(lc.has_tick(retain), "a tick inside the window is resident")
	assert_false(lc.has_tick(0), "a tick past the retention window has been evicted, not merely overwritten later")

# --- the per-shooter rewind policy: one session, two shooters, two windows ------------------------------
# Before this, every shot in a session was rewound by the same flat `delay_ms` -- a LAN player and a peer at the
# design ceiling alike. The depth is now the SHOOTER'S OWN view lag, and these are the cases that pin what that
# means. Everything here is pure arithmetic over `rewind_ms_for_shooter` / `rewind_ticks_for_shooter`; the ring,
# `resolve_hit` and `_resolve_rewound` are untouched by the change and are covered above.
#
# `per_shooter` and `delay_ms` are process-wide statics on a RefCounted class and `just test` is ONE Godot
# process, so a suite that writes them leaks into every later suite. Each case that writes one restores it.

func test_the_window_is_interpolation_plus_the_WHOLE_round_trip() -> void:
	# The rewind is measured from the server's PRESENT tick back to the world as the shooter saw it, and that
	# span has three legs: the state took the downstream leg to reach them, they drew it `interp` ticks behind
	# whatever they held, and the shot command took the upstream leg to get back here. Down plus up is the whole
	# round trip. 100 ms round trip at 60 Hz with a one-tick interpolation term: 16.67 + 100.
	#
	# `rtt/2` is the arithmetic for "when did the client send this" -- a different question from this one.
	# It put the window BELOW the flat 50 ms it replaced for every shooter under about 67 ms round trip, so the
	# change was a regression for exactly the population it existed to help.
	assert_almost_eq(NetLagComp.rewind_ms_for_shooter(100.0, 60.0, 1.0), 1000.0 / 60.0 + 100.0, 1e-4,
		"one tick of interpolation plus the WHOLE round trip, not half of it")

func test_the_interpolation_term_is_the_send_paths_measured_inter_arrival() -> void:
	# The term that was a constant. A body renders at the last row it RECEIVED, and a peer's snapshot frame is
	# one datagram -- so when the byte budget cannot carry every entity every tick, the newest row is older than
	# one tick and the rewind must know by how much. Measured on a 24-drone arena against a real dedicated
	# server: a mean of 3.4 ticks for the near band.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.refresh_observed_interp(3.4)
	assert_almost_eq(NetLagComp.observed_interp_ticks, 3.4, 1e-4, "the measurement is adopted")
	assert_almost_eq(NetLagComp.rewind_ms_for_shooter(100.0, 60.0), 3.4 * 1000.0 / 60.0 + 100.0, 1e-4,
		"...and the window is built from it rather than from 1.0")
	# The FLOOR: a body cannot render fresher than the tick it arrived on, and a measurement that has not
	# started yet must not shrink the window below the flat fallback.
	NetLagComp.refresh_observed_interp(0.0)
	assert_almost_eq(NetLagComp.observed_interp_ticks, NetLagComp.INTERP_TICKS, 1e-4,
		"no measurement yet leaves the floor in place rather than inventing a number")
	NetLagComp.refresh_observed_interp(0.25)
	assert_almost_eq(NetLagComp.observed_interp_ticks, NetLagComp.INTERP_TICKS, 1e-4,
		"...and a sub-tick measurement cannot buy a window shallower than one tick")
	# The CEILING: a send path so starved that a body arrives every twentieth tick is broken in a way a deeper
	# rewind does not fix -- it would trade missed shots for shots landing on targets already behind cover.
	NetLagComp.refresh_observed_interp(50.0)
	assert_almost_eq(NetLagComp.observed_interp_ticks, NetLagComp.MAX_INTERP_TICKS, 1e-4,
		"a pathological measurement clamps rather than turning the rewind into a time machine")
	NetLagComp.observed_interp_ticks = saved
	assert_almost_eq(NetLagComp.rewind_ms_for_shooter(0.0, 60.0), 1000.0 / 60.0, 1e-4,
		"a shooter on a perfect link still gets the interpolation term -- their screen is a tick behind regardless")

func test_the_interpolation_term_is_a_duration_at_every_tick_rate() -> void:
	# INTERP_TICKS is a constant in TICKS and the term it produces is a DURATION, so it has to shrink as the rate
	# rises. A remote body renders one tick behind whatever a tick currently is.
	for hz: float in [30.0, 60.0, 120.0]:
		assert_almost_eq(NetLagComp.rewind_ms_for_shooter(0.0, hz), 1000.0 / hz, 1e-4,
			"the interp term is one tick at %d Hz" % int(hz))

func test_a_lan_shooter_is_rewound_less_than_a_ceiling_shooter_in_the_same_session() -> void:
	# The point of per-shooter compensation, as arithmetic. Same tick rate, same policy, two peers, two depths.
	var lan: int = NetLagComp.rewind_ticks_for_shooter(1.0, 60.0)
	var congested: int = NetLagComp.rewind_ticks_for_shooter(100.0, 60.0)
	var ceiling: int = NetLagComp.rewind_ticks_for_shooter(500.0, 60.0)
	assert_true(lan < congested and congested < ceiling,
		"rewind depth rises with the shooter's measured lag (%d < %d < %d ticks)" % [lan, congested, ceiling])
	assert_eq(lan, 1, "a LAN shooter is rewound one tick -- their view lag IS the interpolation term")

func test_no_estimate_is_not_zero_and_falls_back_to_the_flat_window() -> void:
	# A peer the server has heard no acknowledgement from is a DIFFERENT state from one on a perfect link.
	# Treating "we do not know" as 0 would hand a fresh joiner the shallowest window in the session at exactly
	# the moment their link is least settled, so it falls back to the flat window instead.
	assert_almost_eq(NetLagComp.rewind_ms_for_shooter(-1.0, 60.0), -1.0, 1e-6,
		"a negative measurement reports 'no estimate' rather than resolving to a window")
	assert_eq(NetLagComp.rewind_ticks_for_shooter(-1.0, 60.0), NetLagComp.rewind_ticks(60),
		"...and the depth falls back to the flat delay_ms window")
	assert_true(NetLagComp.rewind_ticks_for_shooter(-1.0, 60.0) > NetLagComp.rewind_ticks_for_shooter(0.0, 60.0),
		"the fallback is deeper than a measured-perfect link, which is the point of telling them apart")

func test_the_per_shooter_window_is_clamped_in_milliseconds() -> void:
	# The clamp is load-bearing only once the depth is per-shooter: before that nothing a client did could
	# influence it. Now a peer can
	# inflate the figure the estimate is drawn from by going quiet, and this is the containment.
	var ceiling_ticks: int = NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, 60.0)
	assert_eq(NetLagComp.rewind_ticks_for_shooter(10_000.0, 60.0), ceiling_ticks,
		"an absurd round trip clamps to the ceiling rather than indexing past the ring")
	assert_eq(NetLagComp.rewind_ticks_for_shooter(INF, 60.0), ceiling_ticks,
		"...and so does an infinite one, which is what clamping is for")
	# The ceiling is the same DURATION at every rate, which is what clamping in ms rather than ticks buys.
	for hz: float in [30.0, 60.0, 120.0]:
		var ms: float = float(NetLagComp.rewind_ticks_for_shooter(10_000.0, hz)) * 1000.0 / hz
		assert_almost_eq(ms, NetLagComp.max_delay_ms, 1000.0 / hz,
			"the clamped window is %.0f ms at %d Hz, within one tick" % [NetLagComp.max_delay_ms, int(hz)])

func test_the_design_ceiling_shooter_is_answered_by_the_clamp() -> void:
	# netbench's `worst_case` is 250 ms ONE-WAY, a 500 ms round trip. With the whole round trip in the formula
	# such a shooter asks for 500 ms plus their interpolation and receives `max_delay_ms` -- the clamp is what
	# answers, which is the containment the ceiling exists to be. What CHANGED with the halving fix is where the
	# clamp starts binding: it now binds from a 250 ms round trip up rather than from 500, so the deepest rewind
	# this game grants is reached by a genuinely bad link rather than only by the design ceiling.
	var asked: float = NetLagComp.rewind_ms_for_shooter(500.0, 60.0, 1.0)
	assert_almost_eq(asked, 500.0 + 1000.0 / 60.0, 1e-4,
		"the ask is the whole round trip plus the interpolation term")
	assert_true(asked > NetLagComp.max_delay_ms, "so the clamp is what answers it")
	assert_eq(NetLagComp.rewind_ticks_for_shooter(500.0, 60.0, 1.0),
		NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, 60.0),
		"...and what it receives is exactly the ceiling")

func test_a_degenerate_measurement_or_rate_reports_no_estimate() -> void:
	for hz: float in [0.0, -60.0, NAN, INF]:
		assert_almost_eq(NetLagComp.rewind_ms_for_shooter(50.0, hz), -1.0, 1e-6,
			"an unusable tick rate cannot produce a window")
	assert_almost_eq(NetLagComp.rewind_ms_for_shooter(NAN, 60.0), -1.0, 1e-6,
		"a NaN measurement is no estimate, never a window derived from NaN")

func test_turning_the_policy_off_reproduces_the_flat_window_exactly() -> void:
	# The A/B that makes a feel regression one setting rather than a bisect: off must be bit-identical to the flat
	# for every shooter, including one the server has a perfectly good estimate for -- AND including the AUTHORITY
	# ITSELF, which is the shooter a developer running that A/B on a listen host actually fires. The zero-rewind
	# rule for the authority shipped at the shot site instead of on the policy, so it was unconditional: `off`
	# restored the flat window for every peer except the one holding the mouse.
	var was: bool = NetLagComp.per_shooter
	NetLagComp.per_shooter = false
	for hz: float in [30.0, 60.0, 120.0]:
		assert_eq(NetLagComp.rewind_ticks_for_shooter(500.0, hz), NetLagComp.rewind_ticks(int(hz)),
			"per_shooter off: a 500 ms shooter gets the flat window at %d Hz" % int(hz))
		assert_eq(NetLagComp.rewind_ticks_for_shooter(0.0, hz), NetLagComp.rewind_ticks(int(hz)),
			"per_shooter off: a LAN shooter gets it too")
		assert_eq(NetLagComp.rewind_ticks_for_shot(true, 0.0, hz), NetLagComp.rewind_ticks(int(hz)),
			"per_shooter off: the AUTHORITY's own shot gets the flat window too, at %d Hz" % int(hz))
	NetLagComp.per_shooter = was

func test_the_authority_takes_no_rewind_while_the_policy_is_on() -> void:
	# ...and with the policy on it does take zero: a listen host renders the bodies
	# it simulates, live, so it has neither a round trip to itself nor an interpolation delay to what it draws.
	# Pinned as a depth of exactly 0 rather than "less than a remote's", because a host that merely got a SHORTER
	# window would still be resolving its own shots against a world it never saw.
	var was: bool = NetLagComp.per_shooter
	NetLagComp.per_shooter = true
	for hz: float in [30.0, 60.0, 120.0]:
		assert_eq(NetLagComp.rewind_ticks_for_shot(true, 0.0, hz), 0,
			"the authority's own shot takes no rewind at %d Hz" % int(hz))
		assert_eq(NetLagComp.rewind_ticks_for_shot(true, 250.0, hz), 0,
			"...whatever RTT is reported for it, which for itself is meaningless")
		assert_true(NetLagComp.rewind_ticks_for_shot(false, 250.0, hz) > 0,
			"...while a remote shooter on the same server still gets one")
	NetLagComp.per_shooter = was

func test_no_shooter_can_ask_for_more_history_than_the_ring_retains() -> void:
	# The silent failure this guards: a depth past `retain_ticks` finds an evicted slot, `has_tick` answers false,
	# and the shot resolves at the PRESENT tick with no error anywhere -- a feel bug with no symptom to grep for.
	# It holds only because rewind_ticks_for_shooter routes through the same ms clamp retain_ticks is derived from.
	for hz: int in [30, 60, 120]:
		var deepest: int = NetLagComp.rewind_ticks_for_shooter(INF, float(hz))
		assert_true(deepest < NetLagComp.retain_ticks(hz),
			"at %d Hz the deepest window (%d ticks) stays inside the ring's residency (%d)" % [
				hz, deepest, NetLagComp.retain_ticks(hz)])
