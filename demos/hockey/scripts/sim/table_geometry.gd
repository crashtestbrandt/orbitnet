extends RefCounted
class_name TableGeometry
## The table's shape, as pure functions over TABLE SPACE. No node, no tree, no tilt.
##
## The rink node is inclined so a fixed camera sees the whole surface in perspective, and NONE of that reaches
## here: the tilt is a rotation on the parent transform, so a body's local `position` is already its
## table-space coordinate. Every function below is therefore plain 2D geometry in the x/z plane with y pinned
## to 0, which is what lets the whole playfield be unit-tested from plain data.
##
## Mallets do not collide with each other -- two team-mates may overlap, and the renderer fades the nearer one
## rather than pushing it away. So the only containment rules are: a mallet stays inside ITS OWN HALF at full
## width, and the puck stays inside the rails except through a goal mouth.

## Flatten a point onto the table plane. Called on anything that came from a projection or the wire, where a y
## component is either meaningless or noise.
static func flatten(point: Vector3) -> Vector3:
	return Vector3(point.x, 0.0, point.z)

## Whether every component of `point` is finite. A wire- or pointer-decoded NAN propagates through every
## operation, never compares equal to anything, and surfaces far from where it entered -- so it is refused at
## the edge rather than clamped into something plausible.
static func is_finite_point(point: Vector3) -> bool:
	return is_finite(point.x) and is_finite(point.y) and is_finite(point.z)

## Clamp a body of `radius` inside the rails, ignoring the goal mouths. The puck reaches this only through the
## reflection path; a mallet is clamped to its own half instead.
static func clamp_to_table(point: Vector3, radius: float) -> Vector3:
	var limit_x: float = maxf(0.0, HockeyConfig.HALF_WIDTH - radius)
	var limit_z: float = maxf(0.0, HockeyConfig.HALF_LENGTH - radius)
	return Vector3(clampf(point.x, -limit_x, limit_x), 0.0, clampf(point.z, -limit_z, limit_z))

## Clamp a mallet of `radius` inside the half `seat` defends: the FULL width of the table, and from its own end
## rail up to the centre line.
##
## THIS IS THE VALIDATION MOMENT. It runs inside `_rollback_tick`, which the server runs for every mallet from
## the client's own requested point -- so a client that writes a target on the far side of the table gets a
## mallet on the centre line, on every peer, including its own after reconciliation. There is no separate
## server-side check to keep in step with the client's, because the clamp IS the simulation.
static func clamp_to_half(point: Vector3, seat: int, radius: float) -> Vector3:
	var limit_x: float = maxf(0.0, HockeyConfig.HALF_WIDTH - radius)
	var near: float = maxf(0.0, radius)                                       # the centre-line side
	var far: float = maxf(near, HockeyConfig.HALF_LENGTH - radius)            # the own-goal side
	var sign_z: float = HockeyConfig.end_sign(HockeyConfig.team_of_seat(seat))
	var z: float = clampf(point.z * sign_z, near, far) * sign_z
	return Vector3(clampf(point.x, -limit_x, limit_x), 0.0, z)

## Where a mallet stands when its seat is empty, and where it appears when someone takes the seat.
##
## Team-mates are spread centre-outwards across their end so the first players to join stand nearest the middle
## rather than in a corner, and no two seats share a slot. Deterministic, because every peer builds this pool
## and a random scatter would make the initial positions differ until the first state row arrived.
static func home_point(seat: int) -> Vector3:
	var team: int = HockeyConfig.team_of_seat(seat)
	var index: int = seat >> 1                         # this seat's index within its own team
	var slots: int = maxi(1, HockeyConfig.SEATS / 2)
	var span: float = maxf(0.0, HockeyConfig.HALF_WIDTH - HockeyConfig.MALLET_RADIUS)
	# 0, +1, -1, +2, -2 ... scaled so the outermost pair lands on the rail.
	var step: int = (index + 1) / 2
	var side: float = 1.0 if (index % 2) == 1 else -1.0
	var x: float = side * span * (float(step) / float(maxi(1, slots / 2)))
	var z: float = HockeyConfig.end_sign(team) * maxf(0.0,
		HockeyConfig.HALF_LENGTH - HockeyConfig.MALLET_RADIUS - 0.06)
	return Vector3(clampf(x, -span, span), 0.0, z)

## Whether `x` falls within a goal mouth, i.e. whether an end rail is open at that point.
static func is_in_goal_mouth(x: float) -> bool:
	return absf(x) <= HockeyConfig.GOAL_HALF_WIDTH

## The z of the goal line `team` defends.
static func goal_line_z(team: int) -> float:
	return HockeyConfig.end_sign(team) * HockeyConfig.HALF_LENGTH

## The team that SCORES when a puck centre reaches `z` through a mouth, or -1 when `z` is short of either line.
## Crossing the -z line is conceded by team 0, so team 1 scores.
static func scoring_team_at(z: float) -> int:
	if z <= -HockeyConfig.HALF_LENGTH:
		return 1
	if z >= HockeyConfig.HALF_LENGTH:
		return 0
	return -1

## Where the puck is placed for a face-off: the centre of the table.
static func centre_spot() -> Vector3:
	return Vector3.ZERO
