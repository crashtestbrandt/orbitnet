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
	assert_vec_almost_eq(n, Vector3(0.0, 0.0, 1.0), 1e-4, "radial from the nearest axis point, not from the capsule center")

func test_capsule_normal_on_the_hemisphere_cap() -> void:
	var xf: Transform3D = Transform3D(Basis.IDENTITY, Vector3.ZERO)
	var hit_pos: Vector3 = Vector3(0.0, 0.5, 0.0)   # the case-4 apex entry point
	var n: Vector3 = NetLagComp._capsule_normal(hit_pos, xf, 0.3, 1.0)
	assert_vec_almost_eq(n, Vector3(0.0, 1.0, 0.0), 1e-4, "from the clamped cap center, not the capsule center")

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

# A capsule lying ACROSS the ray (axis rotated onto world X) that the ray clips well off its center. The center is
# 0.6 m off the ray line -- twice the capsule's 0.3 radius -- so a cull bounded by the RADIUS rather than the
# capsule's half-HEIGHT would reject it, and a real torso shot along the body's long axis would stop registering.
func test_the_cull_keeps_a_capsule_the_ray_reaches_end_on() -> void:
	var live: Node3D = Node3D.new()
	var across: Transform3D = Transform3D(Basis(Vector3.FORWARD, PI * 0.5), Vector3(10.0, 0.0, 0.0))
	var lc: NetLagComp = _recorded_ring([_sample_at(live, across, 0.3, 2.0)])
	var hit: NetRay.Hit = lc.resolve_hit(null, Vector3(10.6, 0.0, 5.0), Vector3(0.0, 0.0, -1.0), 20.0, [],
		0xFFFFFFFF, 10, 13, _HITBOX_LAYER)
	assert_true(hit.valid, "a capsule struck 0.6 m off its center, along its own axis, survives the cull")
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

func test_the_interpolation_term_is_scoped_to_the_peer_that_fired() -> void:
	# The defect: the round-trip half of the window was per peer and the interpolation half was one
	# process-wide figure pooled across every peer. Send cadence is per peer by construction -- the byte budget
	# is charged per peer per frame and the candidate list is rebuilt per peer -- so a shooter whose own rows
	# arrive every tick was granted a window measured partly from peers whose rows arrive every eighth.
	#
	# Two shooters on identical links, differing only in what the send path measured about each: the depths
	# must differ, and each must be the one its own cadence earns.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp(4.0)          # the session's pooled mean
	NetLagComp.refresh_observed_interp_for(2, 1.0)   # served every tick
	NetLagComp.refresh_observed_interp_for(3, 8.0)   # served every eighth
	assert_almost_eq(NetLagComp.observed_interp_for(2), 1.0, 1e-4, "peer 2 is charged its own cadence")
	assert_almost_eq(NetLagComp.observed_interp_for(3), 8.0, 1e-4, "peer 3 is charged its own")
	var fast: int = NetLagComp.rewind_ticks_for_peer_shot(false, 2, 100.0, 60.0)
	var slow: int = NetLagComp.rewind_ticks_for_peer_shot(false, 3, 100.0, 60.0)
	assert_true(slow > fast,
		"the same link at two cadences is two windows, not one pooled window applied to both")
	assert_eq(fast, NetLagComp.rewind_ticks_for_shooter(100.0, 60.0, 1.0),
		"and each is exactly the depth that peer's own measurement earns")
	assert_eq(slow, NetLagComp.rewind_ticks_for_shooter(100.0, 60.0, 8.0), "...on both sides of the pool mean")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_a_peer_with_no_measurement_falls_back_to_the_pooled_figure() -> void:
	# NOT to the one-tick floor. A fresh joiner has no cadence of its own yet, and the session's pooled mean is
	# a better estimate of the one it is about to have -- the floor would hand it the shallowest window in the
	# session at the moment its link is least settled.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp(3.4)
	assert_almost_eq(NetLagComp.observed_interp_for(7), 3.4, 1e-4,
		"an unmeasured peer takes the pooled mean, not the floor")
	assert_eq(NetLagComp.rewind_ticks_for_peer_shot(false, 7, 100.0, 60.0),
		NetLagComp.rewind_ticks_for_shooter(100.0, 60.0), "...which is the window the pooled figure produced")
	# And with nothing measured at all, the pooled figure is itself the floor.
	NetLagComp.reset_observed_interp()
	assert_almost_eq(NetLagComp.observed_interp_for(7), NetLagComp.INTERP_TICKS, 1e-4,
		"no measurement anywhere leaves the floor in place rather than inventing a number")
	NetLagComp.observed_interp_ticks = saved

func test_a_peers_measurement_is_clamped_like_the_pooled_one() -> void:
	# The same floor and ceiling, for the same reasons: a body cannot render fresher than the tick it arrived
	# on, and a send path so starved that a row arrives every twentieth tick is broken in a way a deeper rewind
	# does not fix.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(4, 0.25)
	assert_almost_eq(NetLagComp.observed_interp_for(4), NetLagComp.INTERP_TICKS, 1e-4,
		"a sub-tick measurement cannot buy a window shallower than one tick")
	NetLagComp.refresh_observed_interp_for(4, 50.0)
	assert_almost_eq(NetLagComp.observed_interp_for(4), NetLagComp.MAX_INTERP_TICKS, 1e-4,
		"a pathological measurement clamps rather than turning the rewind into a time machine")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_an_absent_measurement_drops_the_peer_rather_than_pinning_it() -> void:
	# The backend answers 0.0 for a peer whose window admitted nothing and for a peer it does not know, and
	# neither is a measurement of a one-tick cadence. Pinning the floor there would make an idle window read as
	# the fastest possible link; dropping the entry returns the peer to the pooled fallback.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp(5.0)
	NetLagComp.refresh_observed_interp_for(9, 2.0)
	for absent: float in [0.0, -1.0, NAN]:
		NetLagComp.refresh_observed_interp_for(9, 2.0)
		NetLagComp.refresh_observed_interp_for(9, absent)
		assert_almost_eq(NetLagComp.observed_interp_for(9), 5.0, 1e-4,
			"an absent figure returns the peer to the pooled fallback, not to the floor")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_a_departed_peers_measurement_does_not_outlive_it() -> void:
	# Peer ids are reused within a session's lifetime and the per-tick refresh only visits peers that are still
	# connected, so an entry left behind by a departed peer is the cadence a LATER peer would be rewound by.
	# The session-scoped reset covers the same hazard across sessions: these are `static var`s.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp(4.0)
	NetLagComp.refresh_observed_interp_for(5, 8.0)
	NetLagComp.forget_peer_interp(5)
	assert_almost_eq(NetLagComp.observed_interp_for(5), 4.0, 1e-4,
		"the id is back to the pooled fallback the moment its peer is gone")
	NetLagComp.refresh_observed_interp_for(5, 8.0)
	NetLagComp.reset_observed_interp()
	assert_almost_eq(NetLagComp.observed_interp_for(5), NetLagComp.INTERP_TICKS, 1e-4,
		"and a session teardown clears every peer, not only the pooled scalar")
	NetLagComp.observed_interp_ticks = saved

func test_the_peer_aware_shot_answers_to_the_same_policy_switches() -> void:
	# It is `rewind_ticks_for_shot` with both terms resolved for one peer, so it must inherit the whole policy:
	# the authority's own shot takes no rewind, and `per_shooter` off restores the flat window for everyone --
	# including a peer with a deep measured cadence, which is what would otherwise survive the switch.
	var was: bool = NetLagComp.per_shooter
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(6, 8.0)
	NetLagComp.per_shooter = true
	for hz: float in [30.0, 60.0, 120.0]:
		assert_eq(NetLagComp.rewind_ticks_for_peer_shot(true, 6, 0.0, hz), 0,
			"the authority's own shot takes no rewind at %d Hz, whatever was measured about it" % int(hz))
		assert_eq(NetLagComp.rewind_ticks_for_peer_shot(false, 6, 100.0, hz),
			NetLagComp.rewind_ticks_for_shot(false, 100.0, hz, 8.0),
			"a remote shot is the shot policy with that peer's own term at %d Hz" % int(hz))
	NetLagComp.per_shooter = false
	for hz: float in [30.0, 60.0, 120.0]:
		assert_eq(NetLagComp.rewind_ticks_for_peer_shot(false, 6, 100.0, hz), NetLagComp.rewind_ticks(int(hz)),
			"per_shooter off: the flat window at %d Hz, measured cadence and all" % int(hz))
		assert_eq(NetLagComp.rewind_ticks_for_peer_shot(true, 6, 0.0, hz), NetLagComp.rewind_ticks(int(hz)),
			"per_shooter off: the authority gets it too at %d Hz" % int(hz))
	NetLagComp.per_shooter = was
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

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
	# A peer the server has heard no acknowledgment from is a DIFFERENT state from one on a perfect link.
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

# --- the two ceilings: what the server BELIEVES, and how deep a shot REWINDS ----------------------------
# The backend now caps the round trip it reports for a peer (`rtt_believed_max_ms`, 250 ms by default) as well
# as storing it, so the figure reaching `rewind_ticks_for_shooter` is already bounded. `max_delay_ms` is the
# OTHER ceiling and bounds the window built from that figure. Both default to 250 ms, which is exactly the
# arrangement that could quietly halve a window if the two stacked -- so what these pin is that they do not,
# and that a shooter under both is touched by neither.
#
# `NetLagComp` names no session type, so the backend cap is modeled here as `minf(raw, ceiling)` -- which is
# what the backend's `peer_rtt_ms` now does to the raw estimate before a shot site ever sees it.
const _BELIEVED_CEILING_MS: float = 250.0   # the backend's rtt_believed_max_ms default

func _believed(raw_ms: float) -> float:
	return minf(raw_ms, _BELIEVED_CEILING_MS)

func test_the_two_ceilings_do_not_compound() -> void:
	# A shooter whose RAW estimate is absurd arrives here already believed at 250 ms, and 250 ms of round trip
	# plus a tick of interpolation is still over `max_delay_ms` -- so the rewind clamp is what answers, exactly
	# as it did before the backend cap existed. The depth must be the ceiling depth and not a hair less: two
	# 250 ms bounds applied in series must land on one 250 ms window, never on 250 minus a tick.
	for hz: float in [30.0, 60.0, 120.0]:
		var ceiling_ticks: int = NetLagComp.rewind_ticks_for(NetLagComp.max_delay_ms, hz)
		assert_eq(NetLagComp.rewind_ticks_for_shooter(_believed(10_000.0), hz), ceiling_ticks,
			"a believed-capped shooter still receives the full ceiling depth at %d Hz" % int(hz))
		assert_eq(NetLagComp.rewind_ticks_for_shooter(_believed(10_000.0), hz),
			NetLagComp.rewind_ticks_for_shooter(10_000.0, hz),
			"...the same depth an uncapped figure earns at %d Hz -- the two caps do not stack" % int(hz))
	# And the depth the pair produces is still inside what the ring retains, which is the property retain_ticks
	# is derived from and the one a silently-shallower window would not have broken.
	for hz: int in [30, 60, 120]:
		assert_true(NetLagComp.rewind_ticks_for_shooter(_believed(INF), float(hz)) < NetLagComp.retain_ticks(hz),
			"at %d Hz the believed-capped deepest window still fits the ring's residency" % hz)

func test_no_shooter_under_the_believed_ceiling_changes() -> void:
	# The cap binds AT the ceiling and nowhere below it, so every honest shooter is rewound exactly as before.
	#
	# ASSERTED ON THE BELIEVED FIGURE ITSELF, not on the depth it produces. Comparing
	# `rewind_ticks_for_shooter(_believed(x))` against `rewind_ticks_for_shooter(x)` for an `x` below the
	# ceiling is `f(x) == f(x)`, because `_believed` is the identity there -- it would hold whatever
	# `rewind_ticks_for_shooter` did, including nothing. The statement that carries content is that the cap
	# does not move the input below itself and does move it above, so both halves are pinned here.
	for raw_ms: float in [0.0, 1.0, 16.0, 50.0, 100.0, 200.0, 249.0, 250.0]:
		assert_eq(_believed(raw_ms), raw_ms,
			"a %.0f ms estimate is under the ceiling and reaches the shot site unchanged" % raw_ms)
	for raw_ms: float in [251.0, 400.0, 10_000.0]:
		assert_eq(_believed(raw_ms), _BELIEVED_CEILING_MS,
			"a %.0f ms estimate is over the ceiling and reaches the shot site as the ceiling" % raw_ms)
	# And the depth an unchanged figure earns is still the depth it earned, at every rate.
	for raw_ms: float in [0.0, 1.0, 16.0, 50.0, 100.0, 200.0, 249.0, 250.0]:
		for hz: float in [30.0, 60.0, 120.0]:
			assert_eq(NetLagComp.rewind_ticks_for_shooter(_believed(raw_ms), hz),
				NetLagComp.rewind_ticks_for_shooter(raw_ms, hz),
				"a %.0f ms shooter is unchanged by the belief ceiling at %d Hz" % [raw_ms, int(hz)])
	# ...and shooters under it are still told apart from each other, which is what measuring is for.
	var lan: int = NetLagComp.rewind_ticks_for_shooter(_believed(1.0), 60.0)
	var mid: int = NetLagComp.rewind_ticks_for_shooter(_believed(100.0), 60.0)
	var slow: int = NetLagComp.rewind_ticks_for_shooter(_believed(200.0), 60.0)
	assert_true(lan < mid and mid < slow,
		"three links under the ceiling are still three depths (%d < %d < %d ticks)" % [lan, mid, slow])
	assert_true(slow < NetLagComp.rewind_ticks_for_shooter(_believed(10_000.0), 60.0),
		"and every one of them is shallower than the capped shooter, which is what the ceiling is for")

# --- the per-TARGET rewind depth: one cast, three depths -----------------------------------------------
# The window is the shooter's view lag of WHAT THEY ARE SHOOTING AT, and the interpolation half of that is how
# stale the target's last received row is. That is a property of the TARGET's distance, because the send path
# admits rows by priority and bands priority by distance -- so one depth for a whole cast errs long for a
# contested body and short for one across the map. These pin the banding rule, how the band evidence composes
# with the per-peer figure, and that the resolve path reconstructs each target at its own band's tick.
#
# `band_scale_m` and the three band figures are process-wide statics on a RefCounted class and `just test` is
# ONE Godot process, so every case that writes them restores them.

func test_the_band_edges_are_the_send_paths_own() -> void:
	# scale/3 and 2*scale/3 against the distance from the SHOOTER to the target, which is the test
	# `priority.rs` bands a row by -- duplicated in GDScript because `band_of` has no binding, so the two must
	# agree on every edge or a target is rewound by a band it was never sent in.
	var origin: Vector3 = Vector3.ZERO
	assert_eq(NetLagComp.band_for(origin, Vector3(29.0, 0.0, 0.0), 90.0), NetLagComp.Band.NEAR,
		"inside scale/3 is the near band")
	assert_eq(NetLagComp.band_for(origin, Vector3(30.0, 0.0, 0.0), 90.0), NetLagComp.Band.NEAR,
		"the near edge itself is still near -- the comparison is <=, as it is in the backend")
	assert_eq(NetLagComp.band_for(origin, Vector3(31.0, 0.0, 0.0), 90.0), NetLagComp.Band.MID,
		"past scale/3 is the mid band")
	assert_eq(NetLagComp.band_for(origin, Vector3(60.0, 0.0, 0.0), 90.0), NetLagComp.Band.MID,
		"the mid edge itself is still mid")
	assert_eq(NetLagComp.band_for(origin, Vector3(61.0, 0.0, 0.0), 90.0), NetLagComp.Band.FAR,
		"past 2*scale/3 is the far band")
	# The shooter is not the world origin, so the distance is between the two positions and not a magnitude.
	assert_eq(NetLagComp.band_for(Vector3(100.0, 0.0, 0.0), Vector3(120.0, 0.0, 0.0), 90.0),
		NetLagComp.Band.NEAR, "the distance is shooter-to-target, never the target's distance from the origin")

func test_an_unconfigured_band_scale_bands_nothing() -> void:
	# `aoi_band_radius` defaults to 0 and the backend reports the near band for every row at a non-positive
	# scale, so an unconfigured session has no band evidence about anything. Reporting NEAR here is what routes
	# such a session back to the pooled figure -- the near FIGURE applied to the whole world is the error this
	# split exists to remove, and turning the split on by default would have been exactly that.
	for scale: float in [0.0, -90.0, NAN, INF]:
		assert_eq(NetLagComp.band_for(Vector3.ZERO, Vector3(5000.0, 0.0, 0.0), scale), NetLagComp.Band.NEAR,
			"a scale of %f bands every target near" % scale)

func test_the_band_term_is_the_peers_own_cadence_scaled_by_its_bands_staleness() -> void:
	# The two measurements are different MARGINS of the same table: one peer's cadence pooled across bands, and
	# one band's cadence pooled across peers. Neither alone is the cell -- this peer's rows for this band -- and
	# the backend does not publish that cell, because a per-peer-per-band accumulator is a hash lookup per
	# candidate row on the send path. The product of the margins over the pooled total is what they support.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(11, 3.0)
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 3.0, 90.0)   # near fresher than pooled, far staler
	assert_almost_eq(NetLagComp.observed_interp_for_band(11, NetLagComp.Band.NEAR), 2.0, 1e-4,
		"a near target: the peer's 3.0 scaled by 2/3")
	assert_almost_eq(NetLagComp.observed_interp_for_band(11, NetLagComp.Band.MID), 4.0, 1e-4,
		"a mid target: scaled by 4/3")
	assert_almost_eq(NetLagComp.observed_interp_for_band(11, NetLagComp.Band.FAR), 6.0, 1e-4,
		"a far target: scaled by 6/3 -- three depths from one shooter, which is the whole point")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_no_band_evidence_leaves_every_target_on_the_pooled_figure() -> void:
	# Three ways for the evidence not to be there, and each must land on exactly what the flat call gives --
	# not on the near figure, and not on the floor.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(12, 3.0)
	var flat: float = NetLagComp.observed_interp_for(12)
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 3.0, 0.0)   # measured, but no band scale configured
	for band: NetLagComp.Band in [NetLagComp.Band.NEAR, NetLagComp.Band.MID, NetLagComp.Band.FAR]:
		assert_almost_eq(NetLagComp.observed_interp_for_band(12, band), flat, 1e-4,
			"an unconfigured band scale leaves band %d on the pooled figure" % band)
	NetLagComp.refresh_band_interp(2.0, 0.0, 0.0, 3.0, 90.0)   # only the near band published anything
	assert_almost_eq(NetLagComp.observed_interp_for_band(12, NetLagComp.Band.MID), flat, 1e-4,
		"a band that published no measurement stays on the pooled figure")
	assert_almost_eq(NetLagComp.observed_interp_for_band(12, NetLagComp.Band.FAR), flat, 1e-4,
		"...both of them")
	assert_almost_eq(NetLagComp.observed_interp_for_band(12, NetLagComp.Band.NEAR), 2.0, 1e-4,
		"...while the band that did publish one is still scaled by it")
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 0.0, 90.0)   # nothing pooled to divide by
	for band: NetLagComp.Band in [NetLagComp.Band.NEAR, NetLagComp.Band.MID, NetLagComp.Band.FAR]:
		assert_almost_eq(NetLagComp.observed_interp_for_band(12, band), flat, 1e-4,
			"no pooled measurement is no ratio, so band %d takes the pooled fallback" % band)
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_a_degenerate_band_measurement_is_no_measurement() -> void:
	# NaN, infinity and a non-positive figure are all the backend saying it has nothing, and none of them may
	# reach a ratio: a NaN numerator poisons the window and an infinite one turns it into the ceiling.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(13, 3.0)
	var flat: float = NetLagComp.observed_interp_for(13)
	for junk: float in [NAN, INF, -INF, -1.0, 0.0]:
		NetLagComp.refresh_band_interp(junk, junk, junk, 3.0, 90.0)
		assert_almost_eq(NetLagComp.observed_interp_for_band(13, NetLagComp.Band.FAR), flat, 1e-4,
			"a band figure of %f is no measurement, not a window built from it" % junk)
		NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, junk, 90.0)
		assert_almost_eq(NetLagComp.observed_interp_for_band(13, NetLagComp.Band.FAR), flat, 1e-4,
			"...and neither is a pooled denominator of %f" % junk)
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_the_band_term_takes_the_same_floor_and_ceiling_as_every_other() -> void:
	# The band figures are raw so the ratio is undistorted, which means the product can be anything -- a far
	# band drawn from a handful of sends is an arbitrary multiple of the pooled one. The clamp is applied ONCE,
	# to the product, and is the same floor and ceiling the flat term takes.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(14, 8.0)
	NetLagComp.refresh_band_interp(1.0, 1.0, 40.0, 2.0, 90.0)   # a far band 20x the pooled figure
	assert_almost_eq(NetLagComp.observed_interp_for_band(14, NetLagComp.Band.FAR),
		NetLagComp.MAX_INTERP_TICKS, 1e-4,
		"a pathological band ratio clamps rather than turning the rewind into a time machine")
	NetLagComp.refresh_observed_interp_for(14, 1.0)
	NetLagComp.refresh_band_interp(0.1, 1.0, 1.0, 10.0, 90.0)   # a near band a hundredth of the pooled figure
	assert_almost_eq(NetLagComp.observed_interp_for_band(14, NetLagComp.Band.NEAR),
		NetLagComp.INTERP_TICKS, 1e-4,
		"and a fresh band cannot buy a window shallower than the tick a body arrived on")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_a_session_teardown_forgets_the_bands_and_the_scale() -> void:
	# These are `static var`s and outlive the session whose send path they describe. A scale left behind would
	# band the next session's targets against a world of a different size.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.refresh_observed_interp_for(15, 3.0)
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 3.0, 90.0)
	NetLagComp.reset_observed_interp()
	assert_almost_eq(NetLagComp.band_scale_m, 0.0, 1e-6, "the band scale is gone")
	assert_almost_eq(NetLagComp.band_interp_scale(NetLagComp.Band.FAR), 1.0, 1e-6,
		"and every band is back to the pooled figure rather than the previous session's ratio")
	NetLagComp.observed_interp_ticks = saved

func test_two_peers_at_two_ranges_are_four_windows() -> void:
	# The composition, as depths rather than terms: the per-peer split and the per-band split are independent
	# axes, and collapsing either one is the defect each was written to remove.
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(21, 1.0)   # served every tick
	NetLagComp.refresh_observed_interp_for(22, 4.0)   # served every fourth
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 3.0, 90.0)
	var fast_near: int = NetLagComp.rewind_ticks_for_peer_shot_band(false, 21, 100.0, 60.0, NetLagComp.Band.NEAR)
	var fast_far: int = NetLagComp.rewind_ticks_for_peer_shot_band(false, 21, 100.0, 60.0, NetLagComp.Band.FAR)
	var slow_near: int = NetLagComp.rewind_ticks_for_peer_shot_band(false, 22, 100.0, 60.0, NetLagComp.Band.NEAR)
	var slow_far: int = NetLagComp.rewind_ticks_for_peer_shot_band(false, 22, 100.0, 60.0, NetLagComp.Band.FAR)
	assert_true(fast_near < fast_far, "one shooter's far target is rewound deeper than their near one")
	assert_true(slow_near < slow_far, "...for both shooters")
	assert_true(fast_far < slow_far, "and the slower peer is still rewound deeper at the same range")
	assert_eq(fast_far, NetLagComp.rewind_ticks_for_shot(false, 100.0, 60.0,
		NetLagComp.observed_interp_for_band(21, NetLagComp.Band.FAR)),
		"each depth is exactly the shot policy with that peer's own band term")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_the_band_depth_answers_to_the_same_policy_switches() -> void:
	# `per_shooter` off must restore the flat window EXACTLY -- a band ratio surviving the switch is what would
	# otherwise make the A/B this knob exists for a three-way comparison -- and the authority's own shots must
	# still take no rewind at all, at every range.
	var was: bool = NetLagComp.per_shooter
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(23, 4.0)
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 3.0, 90.0)
	NetLagComp.per_shooter = true
	for band: NetLagComp.Band in [NetLagComp.Band.NEAR, NetLagComp.Band.MID, NetLagComp.Band.FAR]:
		assert_eq(NetLagComp.rewind_ticks_for_peer_shot_band(true, 23, 0.0, 60.0, band), 0,
			"the authority's own shot takes no rewind at band %d, whatever that band measured" % band)
	NetLagComp.per_shooter = false
	for band: NetLagComp.Band in [NetLagComp.Band.NEAR, NetLagComp.Band.MID, NetLagComp.Band.FAR]:
		assert_eq(NetLagComp.rewind_ticks_for_peer_shot_band(false, 23, 100.0, 60.0, band),
			NetLagComp.rewind_ticks(60), "per_shooter off: the flat window at band %d, band ratio and all" % band)
	NetLagComp.per_shooter = was
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

func test_the_band_tick_array_is_the_present_tick_less_each_bands_depth() -> void:
	var saved: float = NetLagComp.observed_interp_ticks
	NetLagComp.reset_observed_interp()
	NetLagComp.refresh_observed_interp_for(24, 3.0)
	NetLagComp.refresh_band_interp(2.0, 4.0, 6.0, 3.0, 90.0)
	var ticks: PackedInt64Array = NetLagComp.rewind_band_ticks(500, false, 24, 100.0, 60.0)
	assert_eq(ticks.size(), 3, "three ticks, one per band, indexed by the Band enum")
	for band: NetLagComp.Band in [NetLagComp.Band.NEAR, NetLagComp.Band.MID, NetLagComp.Band.FAR]:
		assert_eq(int(ticks[band]), 500 - NetLagComp.rewind_ticks_for_peer_shot_band(false, 24, 100.0, 60.0, band),
			"band %d is the present tick less its own depth" % band)
	assert_true(ticks[NetLagComp.Band.FAR] < ticks[NetLagComp.Band.NEAR],
		"the far band reaches further back, which is the ordering the split exists to produce")
	# A session too young to hold the depth leaves the tick NEGATIVE rather than clamped to 0: `resolve_hit`
	# answers an unrecorded tick by resolving that target at the shot's base depth, while a clamp to 0 would
	# point it at whatever tick 0 happens to hold.
	var young: PackedInt64Array = NetLagComp.rewind_band_ticks(1, false, 24, 100.0, 60.0)
	assert_true(young[NetLagComp.Band.FAR] < 0, "a depth the session cannot reach stays negative, not clamped")
	NetLagComp.reset_observed_interp()
	NetLagComp.observed_interp_ticks = saved

# --- the resolve path: each target reconstructed at its OWN band's tick ---------------------------------
# One shooter at the world origin, three targets laid out so exactly one ray reaches each, and a ring whose
# recorded poses encode the tick they were recorded at: every body sits one meter further out per tick into the
# past, so the resolved entry distance names the slot the resolver read. `space` is null throughout, which
# NetRay.cast answers with a miss, so the analytic capsule half is what the assertions read.
#
#   near target   x = 10 + (19 - tick), y = 0     10.0 m from the shooter -- inside scale/3 (30)
#   mid target    x = 45 + (19 - tick), y = -10   46.1 m from the shooter -- between 30 and 60
#   far target    x = 80 + (19 - tick), y = 10    83.6 m from the shooter -- past 2*scale/3 (60)
#
# The rays are cast from the target's own Y so each reaches one body and the broad phase rejects the other two.
# THEY DO NOT START AT THE SHOOTER, which is the point: a round's segment start walks toward its target over the
# time of flight, and banding by it would rewind a pellet to a different depth on every tick of its own flight.
const _BAND_SCALE: float = 90.0        # edges at 30 and 60
const _SHOOTER: Vector3 = Vector3.ZERO
const _BASE_TICK: int = 18             # the shot's flat depth, and the fallback for any band the ring lacks
const _PRESENT_TICK: int = 20

func _band_row(tick: int) -> Array[NetLagComp.Sample]:
	var age: float = float(19 - tick)
	var out: Array[NetLagComp.Sample] = []
	out.push_back(_sample_at(_band_bodies[0], Transform3D(Basis.IDENTITY,
		Vector3(10.0 + age, 0.0, 0.0)), 0.5, 2.0))
	out.push_back(_sample_at(_band_bodies[1], Transform3D(Basis.IDENTITY,
		Vector3(45.0 + age, -10.0, 0.0)), 0.5, 2.0))
	out.push_back(_sample_at(_band_bodies[2], Transform3D(Basis.IDENTITY,
		Vector3(80.0 + age, 10.0, 0.0)), 0.5, 2.0))
	return out

var _band_bodies: Array[Node3D] = []

func _band_ring() -> NetLagComp:
	_band_bodies = [Node3D.new(), Node3D.new(), Node3D.new()]
	var lc: NetLagComp = NetLagComp.new()
	for tick: int in [16, 17, 18, 19]:
		lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return _band_row(tick)
		lc.record(tick)
	return lc

func _free_band_bodies() -> void:
	for body: Node3D in _band_bodies:
		body.free()
	_band_bodies = []

# The ray that reaches one band's target, from that target's own Y so the other two are culled.
func _band_ray(lc: NetLagComp, y: float, band_ticks: PackedInt64Array) -> NetRay.Hit:
	return lc.resolve_hit(null, Vector3(0.0, y, 0.0), Vector3(1.0, 0.0, 0.0), 200.0, [], 0xFFFFFFFF,
		_BASE_TICK, _PRESENT_TICK, _HITBOX_LAYER, band_ticks, _SHOOTER)

func test_one_cast_resolves_three_targets_at_three_ticks() -> void:
	# The headline: a contested body a few meters away and one across the map are not the same age, so they are
	# not rewound by the same amount. Near reads tick 19, mid tick 17, far tick 16 -- and the entry distance
	# names which, because each body sits one meter further out per tick into the past.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = _BAND_SCALE
	var lc: NetLagComp = _band_ring()
	var ticks: PackedInt64Array = PackedInt64Array([19, 17, 16])
	assert_almost_eq(_band_ray(lc, 0.0, ticks).distance, 9.5, 1e-4,
		"the near target is reconstructed at tick 19 (x = 10, entry 9.5)")
	assert_almost_eq(_band_ray(lc, -10.0, ticks).distance, 46.5, 1e-4,
		"the mid target at tick 17 (x = 47, entry 46.5)")
	assert_almost_eq(_band_ray(lc, 10.0, ticks).distance, 82.5, 1e-4,
		"the far target at tick 16 (x = 83, entry 82.5) -- three depths in one cast")
	_free_band_bodies()
	NetLagComp.band_scale_m = was_scale

func test_no_band_ticks_restores_the_single_slot_rewind_exactly() -> void:
	# The default, and every caller that predates the split: one tick for the whole cast. The band scale is
	# left configured on purpose -- an empty array, not an unbanded session, is what turns the refinement off.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = _BAND_SCALE
	var lc: NetLagComp = _band_ring()
	var flat: PackedInt64Array = PackedInt64Array()
	assert_almost_eq(_band_ray(lc, 0.0, flat).distance, 10.5, 1e-4, "near at the base tick 18 (x = 11)")
	assert_almost_eq(_band_ray(lc, -10.0, flat).distance, 45.5, 1e-4, "mid at the base tick 18 (x = 46)")
	assert_almost_eq(_band_ray(lc, 10.0, flat).distance, 80.5, 1e-4, "far at the base tick 18 (x = 81)")
	_free_band_bodies()
	NetLagComp.band_scale_m = was_scale

func test_an_unconfigured_band_scale_puts_every_target_on_the_near_tick() -> void:
	# With no scale the banding rule reports NEAR for everything, so every target takes band_ticks[NEAR]. That
	# is not a policy accident: `rewind_band_ticks` derives all three from the pooled figure when there is no
	# band evidence, so the three ticks are equal and this is the flat window.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = 0.0
	var lc: NetLagComp = _band_ring()
	var ticks: PackedInt64Array = PackedInt64Array([19, 17, 16])
	assert_almost_eq(_band_ray(lc, 10.0, ticks).distance, 79.5, 1e-4,
		"a target 80 m out is banded NEAR without a scale, and takes the near tick 19 (x = 80)")
	_free_band_bodies()
	NetLagComp.band_scale_m = was_scale

func test_a_band_tick_the_ring_never_recorded_falls_back_to_the_base_depth() -> void:
	# Never to nothing. A session too young to hold a band's depth, or a depth past the ring's residency, must
	# resolve that target at the shot's base tick -- which the dispatch guard has already proved is recorded.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = _BAND_SCALE
	var lc: NetLagComp = _band_ring()
	for missing: int in [5, -3]:
		var ticks: PackedInt64Array = PackedInt64Array([19, 17, missing])
		assert_almost_eq(_band_ray(lc, 10.0, ticks).distance, 80.5, 1e-4,
			"an unrecorded far tick (%d) resolves the far target at the base tick 18, not at no tick" % missing)
	_free_band_bodies()
	NetLagComp.band_scale_m = was_scale

func test_a_target_whose_counterpart_cannot_be_identified_keeps_the_base_depth() -> void:
	# The counterpart lookup is an INDEX PROBE, not a search: slot N's entry i and slot M's entry i are the same
	# collider whenever the hittable set did not change between them. The identity compare is what makes that
	# guess safe -- on the ticks after a spawn or a death it fails, and the target takes the base depth for a few
	# ticks rather than being resolved against a DIFFERENT BODY's pose, which is the failure worth having a
	# compare for.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = _BAND_SCALE
	var lc: NetLagComp = _band_ring()
	var stranger: Node3D = Node3D.new()
	var reordered: Array[NetLagComp.Sample] = _band_row(16)
	reordered[2].collider = stranger   # tick 16's far entry now names a body that was not there at tick 18
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return reordered
	lc.record(16)
	var hit: NetRay.Hit = _band_ray(lc, 10.0, PackedInt64Array([19, 17, 16]))
	assert_almost_eq(hit.distance, 80.5, 1e-4,
		"the far target keeps the base tick 18 (x = 81) rather than taking a stranger's pose")
	assert_true(hit.collider == _band_bodies[2], "and it is still the target that was struck")
	stranger.free()
	_free_band_bodies()
	NetLagComp.band_scale_m = was_scale

func test_a_shorter_band_slot_never_indexes_past_its_end() -> void:
	# A tick recorded while fewer bodies were hittable has a shorter sample list, so the index probe would read
	# past its end. It must answer "no counterpart" rather than trip an out-of-bounds read on the resolve path.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = _BAND_SCALE
	var lc: NetLagComp = _band_ring()
	var short_row: Array[NetLagComp.Sample] = [_band_row(16)[0]]
	lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return short_row
	lc.record(16)
	assert_almost_eq(_band_ray(lc, 10.0, PackedInt64Array([19, 17, 16])).distance, 80.5, 1e-4,
		"the far target falls back to the base tick rather than reading past the shorter slot")
	_free_band_bodies()
	NetLagComp.band_scale_m = was_scale

func test_every_target_is_banded_once_so_none_can_be_dropped() -> void:
	# The failure the swap-not-a-pass shape exists to prevent. A pass per band reads each band's slot and
	# rejects the samples that do not belong to it -- so a body straddling a band edge, which reads NEAR in the
	# slot the near pass walks and MID in the slot the mid pass walks, is rejected by BOTH and silently stops
	# being hittable. Here the mid target sits one meter inside the near edge at the near tick and one meter
	# outside it at the mid tick; it must still resolve.
	var was_scale: float = NetLagComp.band_scale_m
	NetLagComp.band_scale_m = _BAND_SCALE
	var straddler: Node3D = Node3D.new()
	var lc: NetLagComp = NetLagComp.new()
	# x = 29 at tick 19 (near, 29 < 30) and x = 31 at tick 17 (mid, 31 > 30): the two sides of the edge.
	for tick: int in [17, 19]:
		var x: float = 29.0 if tick == 19 else 31.0
		lc.hittable_provider = func() -> Array[NetLagComp.Sample]: return [
			_sample_at(straddler, Transform3D(Basis.IDENTITY, Vector3(x, 0.0, 0.0)), 0.5, 2.0)] as Array[NetLagComp.Sample]
		lc.record(tick)
	var hit: NetRay.Hit = lc.resolve_hit(null, Vector3.ZERO, Vector3(1.0, 0.0, 0.0), 200.0, [], 0xFFFFFFFF,
		19, _PRESENT_TICK, _HITBOX_LAYER, PackedInt64Array([19, 17, 17]), _SHOOTER)
	assert_true(hit.valid, "a target straddling a band edge between two slots is still resolved, never dropped")
	assert_true(hit.collider == straddler, "and it is the straddler that was struck")
	straddler.free()
	NetLagComp.band_scale_m = was_scale
