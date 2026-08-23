extends RefCounted
class_name HitResolver
## The authoritative shot: one rewind ring for the whole session, and the per-target depth that makes a
## contested target and one across the arena rewind by different amounts.
##
## ONE RING, NOT ONE PER FIGHTER. `NetLagComp` records every hittable body's capsule into one snapshot per
## tick, and the resolve walks that slot. A ring per body would record every OTHER body into every ring,
## which is the N-squared the shared ring exists to avoid.
##
## THE REWIND IS ANALYTIC, NOT A MUTATION OF THE WORLD. Recorded capsules are ray-tested where they were; the
## live physics world is never moved. That is what lets the STATIC half of the mask -- cover, which is the
## same cover at every tick -- be cast live at the present tick in the same call, with the nearer of the two
## winning.
##
## THE PER-TARGET DEPTH IS THE WHOLE POINT OF `band_ticks`. One shot, three depths, indexed by the band each
## TARGET sits in relative to the SHOOTER'S BODY -- not the ray origin, which walks toward the target and
## would band a round differently on each tick of its own flight. A near target's rows arrive every tick while
## a far one's wait several, so rewinding both by the far figure over-rewinds the near one and rewinding both
## by the near figure costs the far shot a hit the shooter saw land.
##
## THE AUTHORITY REWINDS NOTHING. A listen host renders the bodies it is simulating, live: no round trip to
## itself and no interpolation delay to what it is drawing. The rule lives inside `NetLagComp`, not here, so
## that turning per-shooter compensation off restores the flat window for the host too.

## What one resolved shot did, for the readout and for the probe.
class Shot extends RefCounted:
	## The seat that was hit, or -1 for a miss (including a shot that stopped on cover).
	var hit_seat: int = -1
	## Whether the shot stopped on static cover rather than reaching anything.
	var stopped_on_cover: bool = false
	## Where it ended, world space.
	var point: Vector3 = Vector3.ZERO
	## The base tick the shot was resolved around, and how many ticks back that was.
	var at_tick: int = 0
	var base_rewind_ticks: int = 0
	## The three absolute ticks the three bands resolved at, NEAR first.
	var band_ticks: PackedInt64Array = PackedInt64Array()
	## The band the struck target sat in, and how many ticks IT was actually rewound. -1 when nothing was hit.
	var target_band: int = -1
	var target_rewind_ticks: int = -1
	## WHETHER THE HIT WAS FATAL IS NOT ANSWERED HERE, and that is a consequence of the lane rather than an
	## omission. Damage is queued and applied inside the tick, so at resolve time nobody knows yet -- the
	## director reads the answer off the fighters afterwards. See FighterBody._drain_pending().

var lag: NetLagComp = NetLagComp.new()

var _fighters: Array[FighterBody] = []

func configure(pool: Array[FighterBody]) -> void:
	_fighters = pool
	lag.hittable_provider = _snapshot

## Record this tick's hittable poses. SERVER-SIDE, once per fresh net tick, BEFORE any shot for that tick is
## resolved -- a shot rewinding to a tick the ring never recorded is resolved live instead, which silently
## turns lag compensation off for the shooter it happens to.
##
## `retain_ticks` bounds RESIDENCY by duration rather than by slot count. The ring is 128 slots, which is
## 4.27 s at this demo's 30 Hz -- long enough that a freed body's capsules could still be in it.
func record(tick: int) -> void:
	lag.record(tick, NetLagComp.retain_ticks(ArenaConfig.NET_TICK_HZ))

func clear() -> void:
	lag.clear()

## Resolve one shot. SERVER-SIDE, and it must run where the physics space is unlocked -- inside the net tick.
##
## `at_tick` is the shot's BASE depth and still decides whether a rewind happens at all; the per-band ticks
## refine around it, and a target whose own band tick is not in the ring falls back to it.
func resolve(space: PhysicsDirectSpaceState3D, shooter: FighterBody, peer: int, rtt_ms: float,
		present_tick: int, is_authority_shooter: bool) -> Shot:
	var out: Shot = Shot.new()
	var origin: Vector3 = shooter.muzzle_world()
	var dir: Vector3 = FighterMotion.clamp_aim(shooter.net_aim)
	var shooter_pos: Vector3 = shooter.global_position

	var base_ticks: int = NetLagComp.rewind_ticks_for_peer_shot(
		is_authority_shooter, peer, rtt_ms, float(ArenaConfig.NET_TICK_HZ))
	out.base_rewind_ticks = base_ticks
	out.at_tick = present_tick - base_ticks
	out.band_ticks = NetLagComp.rewind_band_ticks(
		present_tick, is_authority_shooter, peer, rtt_ms, float(ArenaConfig.NET_TICK_HZ))

	var exclude: Array[RID] = [shooter.hitbox_rid()]
	var hit: NetRay.Hit = lag.resolve_hit(space, origin, dir, ArenaConfig.SHOT_RANGE_M, exclude,
		ArenaConfig.SHOT_MASK, out.at_tick, present_tick, ArenaConfig.SHOT_DYNAMIC_MASK,
		out.band_ticks, shooter_pos)
	if not hit.valid:
		out.point = origin + dir * ArenaConfig.SHOT_RANGE_M
		return out
	out.point = hit.position

	var struck: FighterBody = _fighter_of(hit.collider)
	if struck == null:
		# The static half of the mask answered first: the shot stopped on cover. Nothing to rewind, nothing to
		# damage, and the tracer stops where the wall is.
		out.stopped_on_cover = true
		return out
	out.hit_seat = struck.seat
	# Reported rather than recomputed at the call site: which band a target sat in is decided from the
	# SHOOTER'S body, and re-deriving it from the ray would put a target that the resolve banded MID into NEAR
	# whenever the round happened to be resolved close to it.
	var band: NetLagComp.Band = NetLagComp.band_for(shooter_pos, struck.global_position,
		NetLagComp.band_scale_m)
	out.target_band = band
	out.target_rewind_ticks = present_tick - out.band_ticks[band]
	return out

# --- the snapshot ------------------------------------------------------------------------------------
## Every fighter a shot could hit, at the tick being recorded.
##
## THE SHOOTER IS RECORDED TOO. One shared ring records everybody, and self-exclusion happens at resolve time
## through the caller's exclude set -- which is the only place that knows who is shooting.
##
## A DEAD FIGHTER IS SKIPPED, and skipping it here is what stops a shot rewound a few ticks from spending
## itself on a corpse the un-rewound shot beside it passes through.
func _snapshot() -> Array[NetLagComp.Sample]:
	var out: Array[NetLagComp.Sample] = []
	for fighter: FighterBody in _fighters:
		if fighter == null or not fighter.is_alive():
			continue
		var sample: NetLagComp.Sample = NetLagComp.Sample.new()
		sample.collider = fighter.hitbox()
		sample.transform = fighter.hitbox_transform()
		sample.radius = ArenaConfig.FIGHTER_RADIUS
		sample.height = ArenaConfig.FIGHTER_HEIGHT
		out.push_back(sample)
	return out

## The fighter a struck collider belongs to. The rewind hands back the recorded COLLIDER -- the hit capsule --
## and the fighter is its parent, which is why the capsule is a named child rather than an anonymous one.
func _fighter_of(collider: Object) -> FighterBody:
	if not is_instance_valid(collider):
		return null
	var node: Node = collider as Node
	if node == null:
		return null
	var parent: Node = node.get_parent()
	if parent == null:
		return null
	var fighter: FighterBody = parent as FighterBody
	return fighter
