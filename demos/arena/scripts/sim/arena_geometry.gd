extends RefCounted
class_name ArenaGeometry
## Where the arenas are, where a seat starts, and where the cover stands. Pure: constants and arithmetic, no
## nodes, so the whole layout is a unit test.
##
## EVERY ARENA IS THE SAME ARENA, REBASED. `local_to_world()` is the only place the offset is applied, and
## every other function here answers in ARENA-LOCAL coordinates. That is what makes the demo's central claim
## true by construction rather than by arrangement: two fighters at the same local point in different arenas
## are at the same local point, and only the rebasing separates them -- by 1200 m, which is exactly the
## separation a distance filter would have to work with if membership did not exist.
##
## THE REBASING IS ALONG ONE AXIS on purpose. Laid out on a grid the arenas would be at varying distances from
## each other, and a reader checking whether a cull was distance or membership would have to do arithmetic. On
## a line every arena is `ARENA_SPACING_M` from its neighbour and no further reasoning is needed.

## The world-space origin of `arena_id`.
static func origin_of(arena_id: int) -> Vector3:
	var index: int = arena_id - ArenaConfig.FIRST_ARENA_ID
	return Vector3(float(index) * ArenaConfig.ARENA_SPACING_M, 0.0, 0.0)

## An arena-local point in world space.
static func local_to_world(arena_id: int, local: Vector3) -> Vector3:
	return origin_of(arena_id) + local

## A world point back into its arena's local frame.
static func world_to_local(arena_id: int, world: Vector3) -> Vector3:
	return world - origin_of(arena_id)

## Clamp an arena-local point inside the floor.
static func clamp_local(local: Vector3) -> Vector3:
	return Vector3(
		clampf(local.x, -ArenaConfig.ARENA_HALF_X, ArenaConfig.ARENA_HALF_X),
		0.0,
		clampf(local.z, -ArenaConfig.ARENA_HALF_Z, ArenaConfig.ARENA_HALF_Z))

## Where a seat spawns, in its own arena's local frame. Teams start on opposite ends and spread along x, so a
## fresh arena opens with the two sides facing each other across the cover.
static func home_local(seat: int) -> Vector3:
	var within: int = seat % ArenaConfig.SEATS_PER_ARENA
	var team: int = ArenaConfig.team_of_seat(seat)
	var rank: int = within / ArenaConfig.TEAMS
	var per_team: int = maxi(1, ArenaConfig.SEATS_PER_ARENA / ArenaConfig.TEAMS)
	var spread: float = ArenaConfig.ARENA_HALF_X * 1.4 / float(per_team)
	var x: float = (float(rank) - float(per_team - 1) * 0.5) * spread
	var z: float = ArenaConfig.ARENA_HALF_Z * (-0.82 if team == 0 else 0.82)
	return Vector3(x, 0.0, z)

## Where a seat spawns in world space.
static func home_world(seat: int) -> Vector3:
	return local_to_world(ArenaConfig.arena_of_seat(seat), home_local(seat))

## Where the cloak pickup sits, arena-local. The middle, because a pickup that withholds a fighter from the
## other team should cost that fighter the walk into the open to take it.
static func cloak_local() -> Vector3:
	return Vector3.ZERO

## One cover block's arena-local box, `index` in [0, COVER_PER_ARENA). Two staggered rows across the middle,
## fixed rather than random: every peer needs the same map, and a seed would be one more thing to agree on.
static func cover_local(index: int) -> AABB:
	var per_row: int = maxi(1, ArenaConfig.COVER_PER_ARENA / 2)
	var row: int = index / per_row
	var column: int = index % per_row
	var spread: float = ArenaConfig.ARENA_HALF_X * 1.6 / float(per_row)
	var x: float = (float(column) - float(per_row - 1) * 0.5) * spread + (0.0 if row == 0 else spread * 0.5)
	var z: float = ArenaConfig.ARENA_HALF_Z * (-0.28 if row == 0 else 0.28)
	var size: Vector3 = Vector3(2.4, 2.2, 1.2)
	return AABB(Vector3(x, 0.0, z) - size * 0.5 + Vector3(0.0, size.y * 0.5, 0.0), size)

## One state-lane prop's arena-local position, `index` in [0, PROPS_PER_ARENA). Spread over the floor on a
## deterministic lattice: they replicate a position and nothing else, and their job is to put real entities in
## the slot table and real candidates in the interest pass.
static func prop_local(index: int) -> Vector3:
	var columns: int = maxi(1, int(ceil(sqrt(float(ArenaConfig.PROPS_PER_ARENA)))))
	var row: int = index / columns
	var column: int = index % columns
	var step_x: float = ArenaConfig.ARENA_HALF_X * 2.0 / float(columns + 1)
	var step_z: float = ArenaConfig.ARENA_HALF_Z * 2.0 / float(columns + 1)
	return Vector3(
		-ArenaConfig.ARENA_HALF_X + step_x * float(column + 1),
		0.0,
		-ArenaConfig.ARENA_HALF_Z + step_z * float(row + 1))

## Whether an arena-local segment is blocked by cover. Used by the bots and by the readout, NEVER by the
## authoritative hit resolution -- that casts the real static geometry through the physics space, because a
## second implementation of the same question is a second answer waiting to disagree with the first.
static func cover_blocks(from_local: Vector3, to_local: Vector3) -> bool:
	var eye: Vector3 = Vector3(0.0, ArenaConfig.FIGHTER_HEIGHT * 0.5, 0.0)
	for index: int in ArenaConfig.COVER_PER_ARENA:
		if cover_local(index).intersects_segment(from_local + eye, to_local + eye):
			return true
	return false
