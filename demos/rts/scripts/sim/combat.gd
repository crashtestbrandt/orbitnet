extends RefCounted
class_name Combat
## Target acquisition and damage. Pure, and deliberately the simplest thing that produces a sustained fight.
##
## NO PROJECTILES. A projectile is a spawned, replicated, short-lived entity -- which is a genuinely
## interesting netcode problem and completely the wrong one for this demo to also be about. Damage here is
## continuous and instantaneous: while an enemy is inside attack_range, hp falls at dps. What the viewer sees
## is health bars draining and units dying, driven entirely by REPLICATED state, which is the point.
##
## Everything is O(units^2) per tick by construction (each unit scans every other). At 96 units and 20 Hz that
## is ~184k distance comparisons a second, which is nothing, and a spatial index would be a third system to
## explain. If UNITS_PER_SEAT is raised far enough for that to matter, the fix is a grid -- and the fact that
## it has not been needed is itself informative about where the real cost in a networked RTS is.

## The nearest living enemy of `seat` within `max_range` of `from`, or -1.
##
## `positions` and `alive` are parallel arrays indexed by unit id -- flat arrays rather than a node scan
## because this runs for every unit every tick, and because it keeps the function pure enough to unit-test
## from literals.
##
## Ties are broken by the LOWER id. That is not arbitrary: with a float comparison, two equidistant targets
## would otherwise be chosen by array iteration order, which is stable here but is exactly the sort of
## implicit dependency that turns into a divergence the first time anything reorders.
static func nearest_enemy(from: Vector3, seat: int, positions: PackedVector3Array, alive: PackedByteArray,
		max_range: float) -> int:
	var best_id: int = -1
	var best_distance_sq: float = max_range * max_range
	var count: int = mini(positions.size(), alive.size())
	for id: int in count:
		if alive[id] == 0:
			continue
		if RtsConfig.seat_of(id) == seat:
			continue
		var delta: Vector3 = positions[id] - from
		var distance_sq: float = delta.x * delta.x + delta.z * delta.z
		if distance_sq < best_distance_sq:
			best_distance_sq = distance_sq
			best_id = id
	return best_id

## Whether `attacker` at `from` can currently shoot `target` at `to` given its attack range.
static func in_attack_range(from: Vector3, to: Vector3, attack_range: float) -> bool:
	var delta: Vector3 = to - from
	return (delta.x * delta.x + delta.z * delta.z) <= attack_range * attack_range

## Damage dealt over `dt` seconds by an archetype. Trivial, but it exists so the number has ONE definition
## that both the sim and the tests read -- a duplicated `dps * dt` is how a balance change ends up applied in
## one place and asserted in another.
static func damage(arch: RtsConfig.Archetype, dt: float) -> float:
	if dt <= 0.0 or not is_finite(dt):
		return 0.0
	return arch.dps * dt

## Where a unit should stand to attack `target`: close to within attack range, then hold. Returning a goal
## rather than a boolean keeps the movement model the single mover -- combat never writes a position.
##
## The stand-off point is on the line to the target at 85% of attack range, so a unit that drifts slightly
## does not immediately fall out of range and start chasing again (the RTS equivalent of hysteresis).
static func approach_goal(from: Vector3, target_position: Vector3, attack_range: float) -> Vector3:
	var delta: Vector3 = Vector3(target_position.x - from.x, 0.0, target_position.z - from.z)
	var distance: float = delta.length()
	var stand_off: float = attack_range * 0.85
	if distance <= stand_off or distance <= 0.0001:
		return from
	return target_position - (delta / distance) * stand_off
