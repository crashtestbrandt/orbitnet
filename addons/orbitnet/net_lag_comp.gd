extends RefCounted
class_name NetLagComp
## Server-side hit resolution with a tick-indexed history ring. The authoritative weapon model
## resolves shots through here so lag compensation has ONE home. Each server tick the ring records the poses of
## the hittable bodies; a shot then RESOLVES against either:
##   * the PRESENT tick -- a live-space ray cast (NetRay), which is what S7 ships and is correct on a localhost
##     listen-server where RTT is ~0, OR
##   * a PAST tick reconstructed from the ring, so a high-ping shooter's shot is judged against where the targets
##     were on THEIR screen (the classic "favour the shooter" rewind). LIVE since 89c -- [method _resolve_rewound]
##     reconstructs the recorded per-region capsules and tests them analytically; only an un-recorded tick (or
##     the derived rewind depth is 0) still falls through to the present-tick cast.
##
## COST LIVES ON THE RESOLVE SIDE, and it is charged per SHOT, not per player: [method record] runs once a tick
## whatever is happening, while [method resolve_hit] runs once per LIVE PELLET TRACK per tick for the whole time
## of flight of every round in the world. Both halves are counted (perf_take_static / perf_take_resolve) and
## reported by `net.perf` -- read the resolve line during a firefight, since it is flat zero at rest.
##
## Pure Godot physics through [NetRay] -- no rollback-backend symbols (the `just net-check` gate). Owned by the
## SERVER's weapon authority; a client never instances one (it never resolves an authoritative hit). The hittable
## snapshot is supplied by a provider Callable so this stays decoupled from how the session enumerates players.

const _RING_SIZE: int = 128   # matches the backend history_limit default; the tick window a rewind could span

## Lag-comp rewind delay in MILLISECONDS -- how far into the past a shot is resolved: the server tests a
## traveling round against targets rewound by this much (the shooter's view / interpolation delay), not all the
## way to fire-tick. Small ⇒ targets stay near-live, so leading + dodging + tracer-coherence survive while the
## shooter's view lag is modestly compensated. 0 ⇒ present-tick (no comp).
##
## THIS IS THE FALLBACK, not the window every shot gets. A shot fired by a peer the server has an RTT estimate
## for is rewound by [method rewind_ticks_for_shooter] instead; this value is what a shot gets when there is no
## estimate to use -- offline, an AI's round, a peer in the first moments of its join, or [member per_shooter]
## off.
##
## **Milliseconds, not ticks, because a tick is not a fixed amount of time.** The obvious spelling of this knob
## is a tick count, documented against the rate the author happened to be running. Then the loop runs at another
## rate and the same line means something else: a window written as "3 ticks ≈ interpolation lag at 120 Hz" is
## worth 25 ms there, 50 ms at 60, and 100 ms at the 30 Hz decoupled tick a 100-player target wants -- doubling
## twice with nobody seeing a line change. A window denominated in time survives the rate change; one denominated
## in ticks is a different policy at every rate it is run at.
static var delay_ms: float = 50.0

## The ceiling on any rewind window, per-shooter or fallback. Read the number as **the deepest rewind this game
## will ever grant**. It defaults to 250 ms because that is the one-way delay of `worst_case` in netbench's
## profile catalog -- so a shooter sitting exactly on that design ceiling (500 ms round trip) asks for 250 ms of
## round trip plus a tick of interpolation and gets 250. The clamp binds AT the ceiling and nowhere below it,
## which is what makes it a bound on the hostile case rather than a cap on the honest one.
##
## Published figures for other engines are usually a ROUND TRIP and this is a rewind DEPTH; confusing them is how
## a ceiling ends up worth twice what its author meant.
##
## IT IS THE CONTAINMENT ON WHAT IS LEFT, so read what it is containing rather than assuming the backend has
## already handled it. The estimate is derived from acknowledgements the client chooses when to send. Three
## backend rules narrow that: an ack must carry the frame token the server minted for the tick it names, so a
## peer cannot acknowledge a frame that never reached it; an ack that did not ADVANCE takes no sample at all;
## and the estimate is the MINIMUM of a recent window. What survives all three is a peer that acknowledges a
## frame OLDER than the newest it holds. That reads as a slow link, is believed, and is indistinguishable from a
## player who put a traffic shaper in front of their connection and is honestly that far away. Neither case can
## be told apart, and neither needs to be: both get at most this many milliseconds of rewind, which is what the
## worst supported legitimate link already receives. Lowering this number is the only lever that narrows either.
## See `consume_ack` and `note_ack` in `orbit_net.rs` for the three rules and what they do not cover.
static var max_delay_ms: float = 250.0

## Whether a shot is rewound by its own shooter's measured round trip, or by the flat [member delay_ms] every
## shooter shares. An AUTHORITY knob: it changes how the server adjudicates, so a client setting it changes
## nothing. Off restores the flat-window behaviour exactly, which is what makes a feel regression an A/B rather
## than a bisect.
static var per_shooter: bool = true

## The shooter's VIEW LAG of what they are shooting at, in NET TICKS: how far behind the server's present the
## remote bodies on their screen are drawn.
##
## The pooled fallback, used by a shot that cannot name its shooter: an AI's round, a diagnostic re-deriving a
## depth, or a peer whose first accounting window has not closed yet. A shot that can name its shooter is built
## from [method observed_interp_for], which is per peer.
##
## **IT IS MEASURED, NOT ASSUMED.** The tempting constant is 1.0, on the argument that every remote body renders
## by interpolating between the last two RECEIVED poses and is therefore drawn exactly one net tick behind. The
## first half is true; the second does not follow, because a row does not arrive every tick. A peer's snapshot
## frame is one datagram of at most `MAX_FRAME_PAYLOAD`, and the send path admits entities into it by priority
## until the bytes run out -- so a body renders at the last row it RECEIVED, which is `interarrival` ticks old,
## not one. Measured against a real dedicated server on a two-dozen-entity arena from a LAN client: a mean of 3.4
## net ticks between rows for the near band, p95 of 8. At 7.5 m/s that is 0.42 m of travel the rewind did not
## account for, against a 0.40 m hit capsule -- a dead-centre shot resolving cleanly past the body it was aimed
## at. That is arithmetic rather than a feel judgement.
##
## The send path publishes the figure, and **what is read is the POOLED value across every band**, not the near
## band's. It is refreshed once per server tick -- the per-peer figure from [method Net.interarrival_ticks] into
## [method refresh_observed_interp_for], the pooled one from [method Net.interarrival_all_ticks] into
## [method refresh_observed_interp] -- rather than read per shot, because a nineteen-key dictionary per pellet
## per tick is the kind of cost this file's header exists to warn about.
##
## **THE POOLED FIGURE IS THE HONEST ONE FOR THIS CONSUMER.** This term is applied to EVERY shot at EVERY range,
## and the code that applies it does not know which band its target sits in. Feeding it the near figure would
## rewind a shot at a mid-band target short by exactly the error this exists to remove. Pooled is the figure a
## consumer that cannot see its subject's band should use, and if it is wrong it is wrong LONG -- a contested
## target carries more staleness x weight than the pool mean, so its real gap is a little shorter than the
## estimate -- while an over-deep rewind is bounded twice over, by [constant MAX_INTERP_TICKS] and again by
## [member max_delay_ms]. Per-TARGET rewind depth is the better answer and is a follow-up.
##
## The FLOOR is 1.0: a body cannot render fresher than the tick it arrived on, and a measurement that has not
## started yet must not shrink the window below what a flat one-tick assumption would have given.
static var observed_interp_ticks: float = 1.0

## The floor the measurement is clamped to, and the value used before any measurement exists.
const INTERP_TICKS: float = 1.0

## The ceiling on the MEASURED interpolation term, in ticks.
##
## A send path so starved that a body arrives every twentieth tick is broken in a way a deeper rewind does not
## fix -- it would only trade missed shots for shots that land on targets who had already taken cover. The clamp
## keeps a pathological measurement from turning the rewind into a time machine, and [member max_delay_ms] bounds
## the total independently.
const MAX_INTERP_TICKS: float = 8.0

## Re-read the send path's measured inter-arrival. SERVER ONLY, once per net tick.
##
## A zero or absent figure means the window has not published yet (the accounting is per second) or nothing was
## admitted at all; both leave the floor in place rather than inventing a number.
static func refresh_observed_interp(interarrival: float) -> void:
	if is_nan(interarrival) or interarrival <= 0.0:
		observed_interp_ticks = INTERP_TICKS
		return
	observed_interp_ticks = clampf(interarrival, INTERP_TICKS, MAX_INTERP_TICKS)

## The same measurement, per peer: how far behind the server's present the remote bodies on one peer's screen
## are drawn, in net ticks.
##
## Send cadence is a per-peer quantity by construction. The byte budget is charged per peer per frame and the
## send path rebuilds its candidate list per peer, so a peer with a small interest set gets its rows every tick
## while a peer in a dense part of the world waits several. The round-trip term this is added to
## ([method Net.peer_rtt_ms]) is already per peer, and pooling only this half granted a peer served every tick a
## window measured partly from peers served every eighth: over-rewound above the pool mean, under-rewound below
## it, up to the [constant MAX_INTERP_TICKS] ceiling. Under-rewind is the direction that costs a shooter a hit
## they saw land.
##
## A peer with no entry falls back to [member observed_interp_ticks] rather than to [constant INTERP_TICKS]. A fresh
## joiner has no cadence of its own yet, and the session's pooled mean is a better estimate of the one it is
## about to have than the one-tick floor -- which would hand it the shallowest window in the session at the
## moment its link is least settled.
##
## Read through [method observed_interp_for] rather than directly; the fallback lives there.
static var _peer_interp_ticks: Dictionary[int, float] = {}

## Re-read the send path's measured inter-arrival for ONE peer. SERVER ONLY, once per net tick per synced peer,
## from [method Net.interarrival_ticks].
##
## A zero, negative or NaN figure drops this peer's entry rather than pinning it to the floor. Those are the
## answers for a peer whose window admitted nothing and for a peer the backend does not know, and neither is a
## measurement of a one-tick cadence. Dropping returns that peer to the pooled fallback, which is what a peer
## with nothing measured about it should get.
static func refresh_observed_interp_for(peer: int, interarrival: float) -> void:
	if is_nan(interarrival) or interarrival <= 0.0:
		_peer_interp_ticks.erase(peer)
		return
	_peer_interp_ticks[peer] = clampf(interarrival, INTERP_TICKS, MAX_INTERP_TICKS)

## The interpolation term for one shooter, in net ticks: that peer's own measured cadence, or
## [member observed_interp_ticks] when nothing has been measured about it yet.
static func observed_interp_for(peer: int) -> float:
	return _peer_interp_ticks.get(peer, observed_interp_ticks)

## Drop one peer's measurement. A consuming game wires this to [signal Net.peer_dropped]; the store is not
## reached from the facade, for the same reason the hittable snapshot is not -- this file stays decoupled from
## how a session enumerates players.
##
## The store is keyed by peer id and peer ids are reused across a session's lifetime, so an entry left behind by
## a departed peer is the cadence a LATER peer would be rewound by. Nothing else clears it: the per-tick refresh
## only visits peers that are still connected.
static func forget_peer_interp(peer: int) -> void:
	_peer_interp_ticks.erase(peer)

## Put every measurement back to the floor and forget every peer. Called when a session ends: these are
## `static var`s, so they outlive the session whose send path they describe, and the next session must start
## from the floor rather than inherit it.
static func reset_observed_interp() -> void:
	observed_interp_ticks = INTERP_TICKS
	_peer_interp_ticks.clear()

## How many ticks of rewind `ms` is worth at `tick_hz`, clamped in MS first and then bounded by what the ring can
## actually hold. Pure -- the whole policy, unit-testable without a session.
static func rewind_ticks_for(ms: float, tick_hz: float, ring_size: int = _RING_SIZE) -> int:
	# NaN is rejected outright (it would survive clampf and poison the conversion); an INFINITE ask is not, because
	# clamping it is the whole job -- "as much rewind as you can give me" resolves to the ceiling, not to none.
	if is_nan(ms) or not is_finite(tick_hz) or tick_hz <= 0.0:
		return 0
	var clamped_ms: float = clampf(ms, 0.0, max_delay_ms)
	return clampi(roundi(clamped_ms * 0.001 * tick_hz), 0, maxi(ring_size - 1, 0))

## The rewind window ONE SHOOTER has earned, in milliseconds, given what the server measured about their round
## trip (`rtt_ms`, from [method Net.peer_rtt_ms]) at `tick_hz`. Pure, so the whole per-shooter policy is a unit
## test rather than a session.
##
## **interpolation + THE WHOLE ROUND TRIP.** The intuitive formula uses `rtt/2`, and half is the wrong half. The
## rewind is measured from the server's PRESENT tick back to the world as the shooter saw it, and that span is
## the sum of three legs, not two: the state left this server and took the downstream leg to reach the client;
## the client drew it `interp` ticks behind whatever it held; and the shot command then took the upstream leg to
## get back here. Down plus up is the whole round trip. Halving it answers "when did the client send this", which
## is a different question and not the one a rewind asks.
##
## A flat 50 ms window is, by this formula, correct for a shooter at roughly 33 ms RTT -- which is what a LAN
## playtest measures, and why the error hides there. `rtt/2` is BELOW the flat window it replaces for every
## shooter under about 67 ms, so adopting it ships as a regression for exactly the population it was meant to
## help.
##
## A NEGATIVE `interp_ticks` means "no per-peer figure supplied", and falls back to the pooled
## [member observed_interp_ticks]. A caller that can name its shooter passes
## [method observed_interp_for] instead, or calls [method rewind_ticks_for_peer_shot], which does.
##
## A NEGATIVE `rtt_ms` means "no estimate", which is not the same as zero and must not be treated as it: the
## caller falls back to [member delay_ms] rather than handing a fresh joiner the shallowest window in the session
## at the moment their link is least settled. Returns a negative here to say so, rather than silently
## substituting.
static func rewind_ms_for_shooter(rtt_ms: float, tick_hz: float, interp_ticks: float = -1.0) -> float:
	if rtt_ms < 0.0 or is_nan(rtt_ms) or not is_finite(tick_hz) or tick_hz <= 0.0:
		return -1.0
	var ticks: float = observed_interp_ticks if interp_ticks < 0.0 else interp_ticks
	if is_nan(ticks):
		ticks = INTERP_TICKS
	var interp_ms: float = clampf(ticks, INTERP_TICKS, MAX_INTERP_TICKS) * 1000.0 / tick_hz
	# An infinite measurement is not rejected, for the same reason an infinite `ms` is not rejected above: the
	# clamp in rewind_ticks_for is what answers it, and answering "the ceiling" is correct.
	return interp_ms + maxf(0.0, rtt_ms)

## The rewind depth in ticks for one shot, in the units [method resolve_hit] wants. `rtt_ms` negative (no
## estimate) or [member per_shooter] off both fall back to the flat [member delay_ms] window.
static func rewind_ticks_for_shooter(rtt_ms: float, tick_hz: float, interp_ticks: float = -1.0) -> int:
	if not per_shooter:
		return rewind_ticks_for(delay_ms, tick_hz)
	var ms: float = rewind_ms_for_shooter(rtt_ms, tick_hz, interp_ticks)
	if ms < 0.0:
		return rewind_ticks_for(delay_ms, tick_hz)
	return rewind_ticks_for(ms, tick_hz)

## The rewind depth in ticks for ONE SHOT, including the rule for a round the AUTHORITY itself fired.
##
## **THE AUTHORITY'S OWN SHOTS TAKE NO REWIND**, and that rule lives HERE rather than at the shot site, for two
## reasons that each go wrong when it does not:
##
##   * It is part of the per-shooter policy, so it has to answer to [member per_shooter] like the rest of it.
##     Written at the shot site it is unconditional, so turning per-shooter off restores the flat window for
##     every peer EXCEPT the host -- and a listen host is exactly where a developer runs the A/B this switch
##     exists for.
##   * Any diagnostic that re-derives the depth from [method rewind_ticks_for_shooter] does not know about the
##     authority, so it reports a window for the host's own body that no shot of that body would ever take. One
##     definition, two callers.
##
## A listen host does not render remote bodies from a replicated pose -- it renders the bodies it is simulating,
## live -- so its view lag is zero on both terms: no round trip to itself, and no interpolation delay to what it
## is drawing. Feeding it through the formula gives it a tick of interpolation it does not have, and a flat
## window gives it the fallback: the host would otherwise be the worst-compensated shooter in its own session.
static func rewind_ticks_for_shot(is_authority_shooter: bool, rtt_ms: float, tick_hz: float,
		interp_ticks: float = -1.0) -> int:
	if per_shooter and is_authority_shooter:
		return 0
	return rewind_ticks_for_shooter(rtt_ms, tick_hz, interp_ticks)

## The rewind depth in ticks for one shot fired by a named peer: [method rewind_ticks_for_shot] with both terms
## resolved for that peer.
##
## The call a weapon authority should make. Both halves of the window are per peer -- the round trip from
## [method Net.peer_rtt_ms], the interpolation term from [method observed_interp_for] -- and this is the one
## place that says so, so a shot site cannot pair one peer's round trip with the session's pooled cadence.
## `rtt_ms` is still a parameter rather than read here, because [NetLagComp] does not name [Net] and must not
## start.
static func rewind_ticks_for_peer_shot(is_authority_shooter: bool, peer: int, rtt_ms: float,
		tick_hz: float) -> int:
	return rewind_ticks_for_shot(is_authority_shooter, rtt_ms, tick_hz, observed_interp_for(peer))

## The rewind depth in ticks for the CURRENT session's FALLBACK window, derived from [member delay_ms] at the
## tick rate the loop is actually running at. [method Net.effective_tickrate] and not [method Net.tickrate]: the
## latter is the CONFIGURED rate and reads 60 while a physics-coupled loop runs at 120.
static func rewind_ticks(tick_hz: int) -> int:
	return rewind_ticks_for(delay_ms, float(tick_hz))

## How long the ring RETAINS a recorded tick, in slots.
##
## The ring is 128 slots, which is a duration only once a rate is fixed: 1.07 s at 120 Hz, 2.13 s at 60, and
## **4.27 s at 30**. That last one matters more than it looks. Game code that frees a body some seconds after its
## death typically argues the delay is safe because it exceeds the ring span, so a corpse has aged out of every
## slot before the node is freed. Halving the net rate re-arms that window: the ring would still hold a freed
## body's region capsules, and only `is_instance_valid` would stand between that and a use-after-free.
##
## Bounding RESIDENCY instead of trusting the slot count makes the guarantee rate-independent. Nothing older than
## the maximum window plus a margin can ever be rewound to, so nothing older needs keeping: at 60 Hz that is
## 15 + 8 = 23 slots (383 ms), at 30 Hz 8 + 8 = 16 (533 ms). Both are an order of magnitude inside any plausible
## linger, at every rate, without anyone having to check.
const _RETAIN_MARGIN_TICKS: int = 8
static func retain_ticks(tick_hz: int) -> int:
	return mini(_RING_SIZE - 1, rewind_ticks_for(max_delay_ms, float(tick_hz)) + _RETAIN_MARGIN_TICKS)

## One hittable collider's pose at a recorded tick (the unit the rewind reconstructs before resolving). For the 89c
## per-region model these are the individual region capsules: `collider` is the struck node (read back by the
## caller to resolve region + health), `transform` its recorded world pose, and `radius`/`height` the capsule
## descriptor the analytic rewind ray-tests against (a generic descriptor so this addon stays game-class-agnostic).
class Sample extends RefCounted:
	var collider: Object = null
	var transform: Transform3D = Transform3D.IDENTITY
	var radius: float = 0.0   # capsule radius (m); <= 0 = skip (no analytic shape recorded)
	var height: float = 0.0   # capsule total height (m), axis along the transform's local Y

# Fixed ring indexed by tick % _RING_SIZE. _ring_ticks[slot] is the tick that slot currently holds (-1 = empty),
# guarding against a stale wrap-around read; _ring_snaps[slot] is that tick's Sample list.
var _ring_ticks: PackedInt64Array = PackedInt64Array()
var _ring_snaps: Array[Array] = []

# perf instrumentation: the O(N^2) lag-comp cost made visible. `record()` runs once per hittable body per
# server tick and snapshots every OTHER body's region capsules into fresh Samples, so today's N per-body rings
# allocate ~N*(N-1)*regions Samples/tick. These counters are STATIC so they SUM across every ring into one
# server-wide figure the net.perf report reads, and they survive the shared-ring refactor (one ring -> the same
# counters, lower numbers). _perf_tick_lo/hi bracket the server-tick span the accumulation covers so the reader
# can derive per-tick figures. Read-and-reset via perf_take_static().
static var _perf_samples: int = 0
static var _perf_record_usec: int = 0
static var _perf_tick_lo: int = -1
static var _perf_tick_hi: int = -1
# The RESOLVE side of the same instrumentation, and the one that actually rides a FIREFIGHT. record() is paid once
# per tick whatever is happening; _resolve_rewound is paid once per LIVE PELLET TRACK per tick, and each call used
# to walk the entire ring slot -- so the server-side cost of shooting is O(rounds in flight x bodies x regions) and
# is invisible in the record-side numbers above. `calls` is rewound casts (~= live tracks), `tests` the ray-vs-
# capsule NARROW-phase tests that survived the broad-phase cull (the term that used to be calls x samples), `usec`
# the wall-clock both cost. Static for the same reason: one whole-server figure, read by net.perf.
static var _perf_resolve_calls: int = 0
static var _perf_resolve_tests: int = 0
static var _perf_resolve_usec: int = 0

## Read-and-reset the resolve-side counters (net.perf): rewound casts run, narrow-phase capsule tests they
## performed, and the usec they cost. All zero on a client / offline (only the server resolves authoritative hits).
static func perf_take_resolve() -> Dictionary[String, int]:
	var out: Dictionary[String, int] = {"calls": _perf_resolve_calls, "tests": _perf_resolve_tests, "usec": _perf_resolve_usec}
	_perf_resolve_calls = 0
	_perf_resolve_tests = 0
	_perf_resolve_usec = 0
	return out

## Read-and-reset the server-wide lag-comp perf counters (net.perf): total Samples recorded, total usec
## spent in record(), and the span of distinct server ticks those covered (0 if nothing recorded since the last
## call -- e.g. a pure client, which never records, or offline). The caller derives samples/tick + usec/tick.
static func perf_take_static() -> Dictionary[String, int]:
	var span: int = (_perf_tick_hi - _perf_tick_lo + 1) if _perf_tick_lo >= 0 else 0
	var out: Dictionary[String, int] = {"samples": _perf_samples, "usec": _perf_record_usec, "ticks": maxi(span, 0)}
	_perf_samples = 0
	_perf_record_usec = 0
	_perf_tick_lo = -1
	_perf_tick_hi = -1
	return out

# Returns Array[Sample]: the bodies a shot could hit AT THE TICK BEING RECORDED (the server passes the live player
# set minus the shooter). Optional -- without it the ring stays empty and only present-tick live casts resolve.
var hittable_provider: Callable = Callable()

func _init() -> void:
	_ring_ticks.resize(_RING_SIZE)
	_ring_snaps.resize(_RING_SIZE)
	for i: int in _RING_SIZE:
		_ring_ticks[i] = -1
		_ring_snaps[i] = []

## Record the hittable snapshot for `tick` into the ring (server, once per fresh tick). No-op without a provider.
##
## `retain` bounds how many ticks of history the ring KEEPS (see [retain_ticks]); 0 leaves the full 128 slots
## resident, which is what an unbounded ring does. It is a parameter rather than a lookup because this runs once per
## body per server tick and the caller already knows the session's tick rate -- and because it lets the ring be
## exercised without a session at all.
func record(tick: int, retain: int = 0) -> void:
	if not hittable_provider.is_valid():
		return
	var t0: int = Time.get_ticks_usec()
	var samples: Array[Sample] = hittable_provider.call()
	var slot: int = tick % _RING_SIZE
	_ring_ticks[slot] = tick
	_ring_snaps[slot] = samples
	# Evict the slot that has just aged past the retention window (see [retain_ticks]). Without this the ring's
	# residency is 128 ticks, which is a different DURATION at every tick rate -- and at 30 Hz it outlives the
	# corpse linger that keeps a freed body's region capsules from being rewound to.
	if retain > 0 and tick >= retain:
		var stale_slot: int = (tick - retain) % _RING_SIZE
		if stale_slot != slot and _ring_ticks[stale_slot] >= 0:
			_ring_ticks[stale_slot] = -1
			_ring_snaps[stale_slot] = []
	# perf: accumulate the server-side lag-comp cost. Static -- sums across every body's ring so net.perf
	# reads the whole-server figure; the wall-clock captures the Sample allocation cost the pooling step will cut.
	_perf_samples += samples.size()
	_perf_record_usec += Time.get_ticks_usec() - t0
	if _perf_tick_lo < 0 or tick < _perf_tick_lo:
		_perf_tick_lo = tick
	if tick > _perf_tick_hi:
		_perf_tick_hi = tick

## Drop every recorded snapshot. Called on session teardown: the ring outlives a session (the session layer owns ONE
## shared ring for the process), tick numbering restarts with the next session, and a slot still holding the
## previous session's samples would answer has_tick() for a colliding tick with references to long-freed bodies.
func clear() -> void:
	for i: int in _RING_SIZE:
		_ring_ticks[i] = -1
		_ring_snaps[i] = []


## Whether a recorded snapshot exists for `tick` (the reserved rewind path checks this before reconstructing).
func has_tick(tick: int) -> bool:
	if tick < 0:
		return false
	return _ring_ticks[tick % _RING_SIZE] == tick

## Resolve a shot. `at_tick` is the shooter's command tick and `present_tick` the server's current tick; when they
## match (or the snapshot is missing) this is a live present-tick cast. A PAST `at_tick` is the rewind case: the
## bits in `dynamic_mask` (the per-region hit layer) are reconstructed from the ring at `at_tick` and tested
## analytically, while the remaining (static) mask bits -- walls, tick-invariant -- are cast live, nearest wins.
## Returns a NetRay.Hit (valid=false on a miss). `space` must be queried where the physics space is unlocked (server net tick).
func resolve_hit(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID], mask: int, at_tick: int, present_tick: int, dynamic_mask: int = 0) -> NetRay.Hit:
	if at_tick >= 0 and at_tick < present_tick and has_tick(at_tick):
		return _resolve_rewound(space, origin, dir, dist, exclude, mask, at_tick, dynamic_mask)
	return NetRay.cast(space, origin, dir, dist, exclude, mask)

# Lag-comp rewind (89c): the dynamic (per-region) colliders are reconstructed from their `at_tick` ring poses and
# tested ANALYTICALLY (ray-vs-capsule) -- never by mutating the live physics world, so a concurrent projectile's
# present-tick cast in the same frame is never disturbed. The static remainder of the mask (world geometry, which
# does not move) is cast live at the present tick. The nearer of the two is the resolved hit; a struck region's
# Sample.collider is returned so the caller resolves region + health uniformly with the present-tick path.
func _resolve_rewound(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID], mask: int, at_tick: int, dynamic_mask: int) -> NetRay.Hit:
	var t0: int = Time.get_ticks_usec()
	_perf_resolve_calls += 1
	var static_mask: int = mask & ~dynamic_mask
	var best: NetRay.Hit = NetRay.cast(space, origin, dir, dist, exclude, static_mask)
	var best_dist: float = best.distance if best.valid else dist + 1.0
	# record() stores a typed Array[Sample] per slot; iterate it with a typed loop var (no untyped Array, no Variant
	# cast). A nested-typed field Array[Array[Sample]] would be cleaner but GDScript 4.x does not parse nested typed
	# collections, so the loop-var typing is the equivalent.
	for sample: Sample in _ring_snaps[at_tick % _RING_SIZE]:
		if sample == null or sample.radius <= 0.0:
			continue
		# BROAD PHASE, first because it is the cheapest test and the one that rejects nearly everything. The narrow
		# ray-vs-capsule below runs for EVERY recorded capsule in the WHOLE ZONE, for every live pellet track, every
		# tick -- so a firefight costs O(rounds in flight x bodies x regions) and a round crossing the midfield was
		# ray-testing every limb of every player and every AI in the arena. This rejects a capsule the segment
		# provably cannot reach: an intersection point lies within the capsule's bounding-sphere radius of its centre
		# AND on the segment, so if the segment's CLOSEST APPROACH to the centre already exceeds that radius there is
		# no intersection. Exact and conservative -- it can only skip capsules _ray_capsule would have missed anyway,
		# so no shot resolves differently.
		#
		# Ordered ahead of the liveness + exclude checks deliberately: both of those DEREFERENCE the collider (an
		# `as`-cast, a get_rid(), and a linear scan of the exclude set), while this reads only the Sample's own value
		# fields -- which stay valid whatever happened to the collider.
		var to_centre: Vector3 = sample.transform.origin - origin
		var along: float = clampf(to_centre.dot(dir), 0.0, dist)
		# Bounding-sphere radius of a capsule of total `height` and `radius` (axis through the centre): the caps
		# reach height/2 from the centre, and a degenerate height shorter than the caps leaves the radius itself.
		var bound: float = maxf(sample.height * 0.5, sample.radius)
		if to_centre.distance_squared_to(dir * along) > bound * bound:
			continue
		# A sample holds a RAW reference to the recorded collider, and a Node reference keeps nothing alive: a
		# death/respawn frees the body and its region hitboxes while the ring still carries the rewind window's
		# snapshots that named them. record() checks liveness at CAPTURE time; this is the only read of the ring,
		# so it must check again -- an EXPORTED build performs no liveness validation on a freed Object reference
		# (Variant's raw-pointer accessor; the "object was freed" guard is DEBUG_ENABLED-only), so `as`-casting a
		# dead sample, reading its RID, or handing it back as hit.collider all dereference freed memory. That is
		# the host crash that lands "right after somebody died": the shot resolves against a corpse's stale sample.
		if not is_instance_valid(sample.collider):
			continue
		# self-exclusion: the shared server ring records EVERY body (including the shooter), so drop any sample
		# whose region collider is in the caller's exclude set (projectile.gd passes shooter.region_rids()). Harmless
		# for a legacy per-body ring -- it never records self, so nothing here ever matches.
		var co: CollisionObject3D = sample.collider as CollisionObject3D
		if co != null and exclude.has(co.get_rid()):
			continue
		# A collider that has LEFT the queried layers since this sample was recorded is transparent to the
		# rewound cast too, exactly as it already is to the live cast above -- the ring must not resurrect a
		# target the present world says is not castable. The concrete case is a corpse: its hit capsules go to
		# layer 0 at death, and without this a shot rewound a few ticks could still spend itself on a body the
		# un-rewound shot beside it passes through. Read off the layer bits rather than any game-side notion of
		# "dead", so the ring stays as ignorant of gameplay as the physics query it stands in for.
		if co != null and (co.collision_layer & mask) == 0:
			continue
		_perf_resolve_tests += 1
		var t: float = _ray_capsule(origin, dir, dist, sample.transform, sample.radius, sample.height)
		if t >= 0.0 and t < best_dist:
			best_dist = t
			var hit: NetRay.Hit = NetRay.Hit.new()
			hit.valid = true
			hit.collider = sample.collider
			hit.position = origin + dir * t
			hit.normal = _capsule_normal(hit.position, sample.transform, sample.radius, sample.height)
			hit.distance = t
			best = hit
	_perf_resolve_usec += Time.get_ticks_usec() - t0
	return best

# The outward surface normal of a capsule (axis = local Y) at world point `pos`: radial from the NEAREST point on
# the capsule's axis segment (the cap centres clamp the height), which is correct for both the cylinder wall (purely
# radial) and the hemisphere caps (from the cap centre). The capsule-centre vector the naive form used is wrong for
# both; this matters for any future ricochet / penetration-angle / decal consumer of the rewound hit normal.
static func _capsule_normal(pos: Vector3, xform: Transform3D, radius: float, height: float) -> Vector3:
	var local_hit: Vector3 = xform.affine_inverse() * pos
	var half: float = maxf(0.0, height * 0.5 - radius)
	var axis_world: Vector3 = xform * Vector3(0.0, clampf(local_hit.y, -half, half), 0.0)
	return (pos - axis_world).normalized()

# Analytic ray-vs-capsule: the nearest entry distance along `dir` (unit) within [0, dist] of a capsule at `xform`
# (no scale) with the given radius + total height (axis = local Y), or -1.0 for a miss. Tests the infinite cylinder
# clipped to the cap centres plus the two hemisphere spheres, and returns the smallest valid entry.
static func _ray_capsule(origin: Vector3, dir: Vector3, dist: float, xform: Transform3D, radius: float, height: float) -> float:
	var inv: Transform3D = xform.affine_inverse()
	var lo: Vector3 = inv * origin          # ray origin in capsule-local space
	var ld: Vector3 = inv.basis * dir       # ray dir in capsule-local space (rotation only -> stays unit length)
	var half: float = maxf(0.0, height * 0.5 - radius)   # cap-centre offset along local Y
	var r2: float = radius * radius
	var best_t: float = -1.0
	# Side wall: infinite cylinder of radius r around local Y, clipped to the cap-centre span.
	var a: float = ld.x * ld.x + ld.z * ld.z
	if a > 0.000000001:
		var b: float = 2.0 * (lo.x * ld.x + lo.z * ld.z)
		var c: float = lo.x * lo.x + lo.z * lo.z - r2
		var disc: float = b * b - 4.0 * a * c
		if disc >= 0.0:
			var t: float = (-b - sqrt(disc)) / (2.0 * a)
			var y: float = lo.y + t * ld.y
			if t >= 0.0 and t <= dist and y >= -half and y <= half:
				best_t = t
	# The two hemisphere caps.
	best_t = _nearest_sphere(lo, ld, Vector3(0.0, half, 0.0), r2, dist, best_t)
	best_t = _nearest_sphere(lo, ld, Vector3(0.0, -half, 0.0), r2, dist, best_t)
	return best_t

# Nearest entry distance of a unit-dir ray vs a sphere (centre `c`, radius^2 `r2`) within [0, dist], or `current`
# if none is nearer. `ld` is assumed unit length (the capsule transform carries no scale).
static func _nearest_sphere(lo: Vector3, ld: Vector3, c: Vector3, r2: float, dist: float, current: float) -> float:
	var oc: Vector3 = lo - c
	var b: float = 2.0 * oc.dot(ld)
	var disc: float = b * b - 4.0 * (oc.dot(oc) - r2)
	if disc < 0.0:
		return current
	var t: float = (-b - sqrt(disc)) * 0.5
	if t < 0.0 or t > dist:
		return current
	if current < 0.0 or t < current:
		return t
	return current
