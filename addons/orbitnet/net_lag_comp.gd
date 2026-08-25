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
## THE REWIND DEPTH IS PER TARGET, not per shot. A shot's window is its shooter's interpolation delay plus their
## round trip, and the interpolation half is how stale the target's last received row is -- which is a property of
## the TARGET's distance from the shooter, because the send path admits rows by priority and bands that priority by
## distance. A contested body a few metres away and one across the map are not the same age. [method resolve_hit]
## takes three ticks from [method rewind_band_ticks] and reconstructs each target at the one its own band earned;
## a caller that passes none gets one depth for the whole cast, as before.
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
## be told apart, and neither needs to be. See `consume_ack` and `note_ack` in `orbit_net.rs` for the three
## rules and what they do not cover.
##
## THERE ARE TWO CEILINGS, AND THEY BOUND DIFFERENT THINGS.
##
##   * THIS ONE BOUNDS THE REWIND DEPTH: the deepest window any shot in this game is resolved at, and -- through
##     [method retain_ticks] -- how long the ring keeps a recorded tick, which is what makes the corpse-linger
##     margin hold at every tick rate. It is applied HERE, to a window already built, and it applies whatever
##     the round trip handed in was.
##   * THE BACKEND ONE BOUNDS WHAT THE SERVER BELIEVES ABOUT A LINK: `rtt_believed_max_ms`, the cap on the
##     round-trip figure the server reports for a peer at all. Every consumer of that figure gets the bounded
##     one -- this rewind, a diagnostic, a game's own matchmaking or routing rule -- not only the shots
##     resolved through here.
##
## Neither subsumes the other. Lowering only this one still lets a fabricated round trip reach everything else
## that asks the server what a peer's link is doing; lowering only the backend one still lets a shot ask for
## more history than the ring retains. THEY DO NOT COMPOUND: the backend caps the figure, this caps the window
## built from it, and a shooter under both is touched by neither.
##
## [NetLagComp] does not set the backend ceiling and does not read it. That knob lives on the session facade,
## for the same reason `rtt_ms` is a parameter here rather than a lookup: this file names no session type.
##
## WHAT NEITHER CEILING CLOSES, plainly: a client that advances its ack at full rate behind a constant lag
## still reads as a slow link, up to whichever ceiling binds first. No wire field closes that -- the round trip
## is the only quantity the server can derive, so a deliberate lag and an honest one are the same measurement.
## Lowering either number narrows the residual and narrows the honest slow link by exactly as much, which is
## why both default to the delay the worst supported legitimate link already receives.
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
## **POOLED ACROSS BANDS, and that is a fallback rather than the answer.** A consumer that cannot see which
## band its subject sits in must use the pooled figure: feeding it the near one would rewind a shot at a
## mid-band target short by exactly the error this exists to remove. The resolve path CAN see its subject, and
## scales this term per target through [method observed_interp_for_band]. This value is what is left when it
## cannot -- an unconfigured band scale, a band that has published no measurement, or a caller that does not
## reach [method resolve_hit] at all.
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

## The distance band a TARGET sits in, for the shooter shooting at it. The send path's three bands, in the
## order the backend accumulates them.
enum Band {
	NEAR = 0,
	MID = 1,
	FAR = 2,
}

## The send path's measured inter-arrival for ONE band, in net ticks, raw and unclamped, refreshed once per
## server tick from [method Net.interarrival_near_ticks] and its two siblings. 0.0 = that band published no
## measurement.
##
## RAW because these are the terms of a RATIO, not a window: what the rewind wants from them is how much
## staler a band's rows are than the session's average row, and clamping the numerator and denominator
## separately would distort that. [method observed_interp_for_band] is where the window's floor and ceiling
## are applied, once, to the product.
static var observed_interp_near_ticks: float = 0.0
static var observed_interp_mid_ticks: float = 0.0
static var observed_interp_far_ticks: float = 0.0

## The pooled inter-arrival across every band, raw and unclamped -- the DENOMINATOR of the band ratio, and the
## same figure [method refresh_observed_interp] receives before clamping it into [member observed_interp_ticks].
## It is not the mean of the three band figures: the backend derives it as total band members over total band
## sends, so the near band, which supplies most of the sends, dominates it.
static var _pooled_interp_raw: float = 0.0

## The band scale the session is running, in metres -- [method Net.aoi_band_radius], refreshed once per server
## tick. Edges at `scale/3` and `2*scale/3`, the same test the send path bands a row by.
##
## **0 MAKES THE PER-TARGET SPLIT INERT, and that is the required behaviour rather than a degenerate one.** The
## backend defaults the scale to 0 and reports the near band for every row at a non-positive scale, so an
## unconfigured session has no band information about anything. Every band then scales by 1.0 and every target
## is rewound by the pooled figure, which is what a consumer that cannot see its subject's band should get --
## not the near figure applied to the whole world.
static var band_scale_m: float = 0.0

## Re-read the send path's per-band measurements and the session's band scale. SERVER ONLY, once per net tick,
## alongside [method refresh_observed_interp].
##
## FIVE TERMS IN ONE CALL because they are one window's worth of evidence and a ratio built from two windows
## describes neither. `pooled` is the same figure [method refresh_observed_interp] takes; it is passed again
## rather than read back from [member observed_interp_ticks] because that one is clamped and this one is the
## ratio's denominator.
##
## A per-tick refresh into scalars rather than a read per shot, for the reason the pooled figure is refreshed
## that way: [method Net.bandwidth_metrics] allocates a nineteen-key dictionary, and a pellet track resolves
## once per tick for its whole time of flight.
static func refresh_band_interp(near: float, mid: float, far: float, pooled: float,
		band_scale: float) -> void:
	observed_interp_near_ticks = _sane_measurement(near)
	observed_interp_mid_ticks = _sane_measurement(mid)
	observed_interp_far_ticks = _sane_measurement(far)
	_pooled_interp_raw = _sane_measurement(pooled)
	band_scale_m = band_scale if is_finite(band_scale) and band_scale > 0.0 else 0.0

# A measurement, or 0.0 for "no measurement". NaN, infinity and a non-positive figure are all the backend
# saying it has nothing, and none of them may reach a ratio.
static func _sane_measurement(value: float) -> float:
	if not is_finite(value) or value <= 0.0:
		return 0.0
	return value

## How much staler one band's rows are than the session's average row: that band's inter-arrival over the
## pooled one, or 1.0 when either figure is missing.
##
## **1.0 IS THE ANSWER WHENEVER THE EVIDENCE IS NOT THERE**, and there are three ways for it not to be: no band
## scale is configured (the backend bands every row near, so the split describes nothing), the band published
## no measurement, or no pooled measurement exists to divide by. Each of those leaves the target on the pooled
## figure, which is the fallback [member observed_interp_ticks] exists to be.
static func band_interp_scale(band: Band) -> float:
	if band_scale_m <= 0.0 or _pooled_interp_raw <= 0.0:
		return 1.0
	var measured: float = 0.0
	match band:
		Band.NEAR:
			measured = observed_interp_near_ticks
		Band.MID:
			measured = observed_interp_mid_ticks
		Band.FAR:
			measured = observed_interp_far_ticks
	if measured <= 0.0:
		return 1.0
	return measured / _pooled_interp_raw

## The interpolation term for one shooter's shot at a target in `band`, in net ticks: that peer's own measured
## cadence, scaled by how much staler that band is than the session's average row.
##
## **THE TWO MEASUREMENTS ARE DIFFERENT MARGINS OF THE SAME TABLE, so they multiply rather than replace each
## other.** [method observed_interp_for] is one peer's cadence pooled across bands; the band figures are one
## band's cadence pooled across peers. Neither alone is the cell the rewind wants -- the cadence of THIS peer's
## rows for THIS band -- and the backend does not publish that cell, because a per-peer-per-band accumulator is
## a hash lookup per candidate row on the send path. The product of the two margins over the pooled total is
## the estimate the two margins support, and it degenerates correctly: with no band evidence the scale is 1.0
## and this is exactly [method observed_interp_for].
##
## Clamped ONCE, at the end, to the same floor and ceiling every other window term takes. A band figure drawn
## from a handful of sends can be an arbitrary multiple of the pooled one, and [constant MAX_INTERP_TICKS] is
## what stops that turning the rewind into a time machine -- with [member max_delay_ms] bounding the total
## independently after it.
static func observed_interp_for_band(peer: int, band: Band) -> float:
	return clampf(observed_interp_for(peer) * band_interp_scale(band), INTERP_TICKS, MAX_INTERP_TICKS)

## The band a target at `target_pos` sits in for a shooter at `shooter_pos`, with band edges derived from
## `band_scale` at `scale/3` and `2*scale/3`. Pure -- the whole banding rule is a unit test.
##
## **THE SHOOTER'S OWN POSITION, NOT THE RAY ORIGIN.** The send path bands a row by the distance from the
## PEER'S INTEREST ANCHOR to the row's anchor, so the shooter's body is the matching proxy. The ray origin the
## resolve path already holds is the round's current segment start, which walks toward the target over the
## round's time of flight -- band by it and a pellet crosses band edges in mid-air and is rewound to a
## different depth on each tick of its own flight.
##
## A non-positive or non-finite scale reports [enum Band].NEAR, matching `band_of` in the backend's
## `priority.rs`: the rule is duplicated here rather than called because it has no GDScript binding, and the
## two must not drift.
static func band_for(shooter_pos: Vector3, target_pos: Vector3, band_scale: float) -> Band:
	if not is_finite(band_scale) or band_scale <= 0.0:
		return Band.NEAR
	var dist_sq: float = shooter_pos.distance_squared_to(target_pos)
	if not is_finite(dist_sq) or dist_sq <= 0.0:
		return Band.NEAR
	var near_edge: float = band_scale / 3.0
	if dist_sq <= near_edge * near_edge:
		return Band.NEAR
	var mid_edge: float = band_scale * (2.0 / 3.0)
	if dist_sq <= mid_edge * mid_edge:
		return Band.MID
	return Band.FAR

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
##
## The band figures and the band scale go with them. A scale left behind by the previous session would band
## the next session's targets against a world of a different size, and the ratio it feeds would be built from
## a send path that no longer exists.
static func reset_observed_interp() -> void:
	observed_interp_ticks = INTERP_TICKS
	_peer_interp_ticks.clear()
	observed_interp_near_ticks = 0.0
	observed_interp_mid_ticks = 0.0
	observed_interp_far_ticks = 0.0
	_pooled_interp_raw = 0.0
	band_scale_m = 0.0

## How many ticks of rewind `ms` is worth at `tick_hz`, clamped in MS first and then bounded by what the ring can
## actually hold. Pure -- the whole policy, unit-testable without a session.
static func rewind_ticks_for(ms: float, tick_hz: float, ring_size: int = _RING_SIZE) -> int:
	# NaN is rejected outright (it would survive clampf and poison the conversion); an INFINITE ask is not, because
	# clamping it is the whole job -- "as much rewind as you can give me" resolves to the ceiling, not to none.
	if is_nan(ms) or not is_finite(tick_hz) or tick_hz <= 0.0:
		return 0
	var clamped_ms: float = clampf(ms, 0.0, max_delay_ms)
	return clampi(roundi(clamped_ms * 0.001 * tick_hz), 0, maxi(ring_size - 1, 0))

## The rewind window ONE SHOOTER has earned, in milliseconds, given what the server BELIEVES about their round
## trip (`rtt_ms`, from [method Net.peer_rtt_ms]) at `tick_hz`. Pure, so the whole per-shooter policy is a unit
## test rather than a session.
##
## `rtt_ms` ARRIVES ALREADY BOUNDED by the backend's belief ceiling -- see [member max_delay_ms] for the two
## ceilings and what each one bounds. Nothing here depends on that: this formula answers any figure it is
## handed, and [member max_delay_ms] answers an absurd one whether or not the backend saw it first.
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

## The rewind depth in ticks for one shot at a target in `band`: [method rewind_ticks_for_peer_shot] with the
## interpolation term scaled for that band.
##
## It answers to every switch the flat call does, because it is the same call with a different term.
## [member per_shooter] off returns the flat window whatever the bands measured, and the authority's own shot
## still takes no rewind -- both rules live in [method rewind_ticks_for_shot] and neither is re-stated here.
static func rewind_ticks_for_peer_shot_band(is_authority_shooter: bool, peer: int, rtt_ms: float,
		tick_hz: float, band: Band) -> int:
	return rewind_ticks_for_shot(is_authority_shooter, rtt_ms, tick_hz, observed_interp_for_band(peer, band))

## The three ABSOLUTE ticks one shot is resolved at, indexed by [enum Band] -- the array [method resolve_hit]
## takes, and the only thing a shot site has to build to get a per-target rewind.
##
## **BUILD IT ONCE PER SHOT PER TICK, not once per pellet.** Every term is a property of the shooter and the
## session, so a shotgun's whole pattern shares one array, and a pellet track that lives for thirty ticks
## rebuilds it thirty times rather than once per resolve call. That is the same accounting that keeps the band
## measurements in scalars.
##
## A tick BELOW ZERO is left as it falls rather than clamped to 0. It means the session has not run long enough
## to hold that band's depth, and [method resolve_hit] answers an unrecorded tick by resolving that target at
## the shot's base depth -- clamping to 0 would instead point it at whatever tick 0 happens to hold.
static func rewind_band_ticks(present_tick: int, is_authority_shooter: bool, peer: int, rtt_ms: float,
		tick_hz: float) -> PackedInt64Array:
	var out: PackedInt64Array = PackedInt64Array()
	out.resize(3)
	out[Band.NEAR] = present_tick - rewind_ticks_for_peer_shot_band(is_authority_shooter, peer, rtt_ms,
		tick_hz, Band.NEAR)
	out[Band.MID] = present_tick - rewind_ticks_for_peer_shot_band(is_authority_shooter, peer, rtt_ms,
		tick_hz, Band.MID)
	out[Band.FAR] = present_tick - rewind_ticks_for_peer_shot_band(is_authority_shooter, peer, rtt_ms,
		tick_hz, Band.FAR)
	return out

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
##
## `band_ticks` and `shooter_pos` turn on the PER-TARGET depth: three ticks from [method rewind_band_ticks]
## indexed by [enum Band], and the SHOOTER'S BODY POSITION the targets are banded against. Each target is then
## reconstructed at the tick its own band earned rather than at `at_tick`, so a contested body and one across
## the map are not rewound by the same amount. Leave `band_ticks` empty -- the default -- and every target is
## reconstructed at `at_tick`, which is what every caller got before the split existed.
##
## `at_tick` STILL DECIDES WHETHER A REWIND HAPPENS AT ALL, and stays the base depth the per-band ticks refine
## around. Pass the shot's flat depth ([method rewind_ticks_for_peer_shot]) as it always was: it is the tick
## this guard proves is recorded, and therefore the one a target whose own band tick is not in the ring falls
## back to. The authority's zero-rewind rule and [member per_shooter] off both land here as `at_tick ==
## present_tick`, a live cast, with the band array never read.
func resolve_hit(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID], mask: int, at_tick: int, present_tick: int, dynamic_mask: int = 0, band_ticks: PackedInt64Array = PackedInt64Array(), shooter_pos: Vector3 = Vector3.ZERO) -> NetRay.Hit:
	if at_tick >= 0 and at_tick < present_tick and has_tick(at_tick):
		return _resolve_rewound(space, origin, dir, dist, exclude, mask, at_tick, dynamic_mask, band_ticks,
			shooter_pos)
	return NetRay.cast(space, origin, dir, dist, exclude, mask)

# Lag-comp rewind (89c): the dynamic (per-region) colliders are reconstructed from their `at_tick` ring poses and
# tested ANALYTICALLY (ray-vs-capsule) -- never by mutating the live physics world, so a concurrent projectile's
# present-tick cast in the same frame is never disturbed. The static remainder of the mask (world geometry, which
# does not move) is cast live at the present tick. The nearer of the two is the resolved hit; a struck region's
# Sample.collider is returned so the caller resolves region + health uniformly with the present-tick path.
func _resolve_rewound(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID], mask: int, at_tick: int, dynamic_mask: int, band_ticks: PackedInt64Array = PackedInt64Array(), shooter_pos: Vector3 = Vector3.ZERO) -> NetRay.Hit:
	var t0: int = Time.get_ticks_usec()
	_perf_resolve_calls += 1
	var static_mask: int = mask & ~dynamic_mask
	var best: NetRay.Hit = NetRay.cast(space, origin, dir, dist, exclude, static_mask)
	var best_dist: float = best.distance if best.valid else dist + 1.0
	var banded: bool = band_ticks.size() == 3
	# ONE slot still drives the loop, and the per-target depth is a SWAP rather than a second pass. The obvious
	# spelling of per-target rewind is a pass per band -- three slot iterations where there was one, on the call
	# this file's header warns is charged per live pellet track per tick. It is also WRONG in a way that costs a
	# hit: a body's band is read off its recorded pose, so a body straddling a band edge can read NEAR in the
	# slot the near pass walks and MID in the slot the mid pass walks, and be rejected by both. Banding each
	# target ONCE, from the base slot, then fetching that target's counterpart out of its own band's slot,
	# resolves both at once -- every target is banded exactly once and tested exactly once.
	var base: Array = _ring_snaps[at_tick % _RING_SIZE]
	# record() stores a typed Array[Sample] per slot; a nested-typed field Array[Array[Sample]] would be cleaner
	# but GDScript 4.x does not parse nested typed collections, so the typed local is the equivalent (assignment
	# converts, which is the allowed form; an `as`-cast of a Variant is not).
	for idx: int in base.size():
		var sample: Sample = base[idx]
		if sample == null or sample.radius <= 0.0:
			continue
		# The per-target depth, ahead of everything else in the body because it decides WHICH SAMPLE the rest of
		# the body is about. It reads only the Sample's own value fields, so it is as safe on a stale entry as
		# the broad phase below and cheaper than it.
		if banded:
			var want: int = band_ticks[band_for(shooter_pos, sample.transform.origin, band_scale_m)]
			if want != at_tick:
				var alt: Sample = _band_counterpart(want, idx, sample.collider)
				if alt != null and alt.radius > 0.0:
					sample = alt
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

# The same target's sample in ANOTHER recorded slot: the entry at the SAME INDEX of `tick`'s snapshot, if that
# entry names the same collider. `null` for anything else, which the caller answers by keeping the base slot's
# sample -- so a target whose counterpart cannot be identified is resolved at the shot's base depth, never
# dropped.
#
# AN INDEX PROBE, NOT A SEARCH, and that is the whole reason the per-target depth is affordable. The hittable
# provider walks the same bodies and the same regions every tick, so slot N's entry i and slot M's entry i are
# the same collider whenever the hittable set did not change between them -- which is every tick but the ones a
# body spawned or died on. Searching instead would be a scan per sample per pellet per tick, the O(N^2) the
# broad phase above exists to avoid; a dictionary keyed by collider would be an allocation per resolve, the cost
# the band measurements are kept in scalars to avoid. The identity compare is what makes the guess safe: it
# fails closed for the ticks after a spawn or a death, and those targets take the base depth for a few ticks.
func _band_counterpart(tick: int, idx: int, collider: Object) -> Sample:
	if not has_tick(tick):
		return null
	var snap: Array = _ring_snaps[tick % _RING_SIZE]
	if idx >= snap.size():
		return null
	var alt: Sample = snap[idx]
	if alt == null or alt.collider != collider:
		return null
	return alt

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
