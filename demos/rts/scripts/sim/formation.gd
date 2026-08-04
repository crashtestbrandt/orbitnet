extends RefCounted
class_name Formation
## Where each unit in an ordered group actually goes. Pure.
##
## WHY THIS EXISTS AT ALL: without it, an order to 24 units is 24 units ordered to the SAME point, which they
## all reach and then fight over, jittering forever because each one's arrival braking is fighting the others'
## presence. The obvious fix is neighbour separation -- but that is an O(n^2) force loop whose output depends
## on iteration order, and it would put a second, subtler source of "why do they look like that?" into a demo
## whose job is to make the NETWORK legible. Giving each unit its own destination removes the contention at
## the source, costs nothing per tick, and is trivially testable.
##
## The slot assignment is a pure function of (index within the order, group size), so the server computes it
## once when the order lands and never revisits it.

## Spacing between formation slots, in multiples of the largest unit radius. Loose enough that a Tank and a
## Scout in the same order do not occupy the same square.
const SLOT_SPACING: float = 2.4

## The offset from the order's target point for the `index`-th unit of a `count`-unit order.
##
## Lays out a centred grid, rows across X and columns along Z, widest-first: a 24-unit order becomes a 5x5
## block minus one, which reads as a formation rather than a queue. The grid is axis-aligned rather than
## rotated to the approach direction -- rotating it looks better and costs an extra input (which way is the
## group is coming from) that the server would have to derive from state the order does not carry.
##
## WHAT IS GUARANTEED IS THE CENTROID, NOT ANY INDIVIDUAL UNIT. It is tempting to special-case index 0 onto
## the click so that something always lands exactly where the player pointed -- and that is wrong twice. It
## collides with the grid's own centre slot, handing two units the same destination (which is the whole
## problem this function exists to avoid); and it cannot hold in general anyway, because a block with an even
## number of columns has no centre slot for anything to land on. The group centres on the click; with an odd
## square, the middle unit happens to sit exactly on it.
static func slot_offset(index: int, count: int) -> Vector3:
	# A negative index is nonsense input, not a slot -- degrade to the target rather than to a mirrored
	# position off the far side of the block. Index 0 is a REAL slot; see above.
	if count <= 1 or index < 0:
		return Vector3.ZERO
	# ceili, not int(ceil(...)): ceil() takes and returns Variant (it accepts vectors), so int(ceil(x))
	# passes a Variant to a typed constructor -- banned, and a parse error under this project's settings.
	var columns: int = ceili(sqrt(float(count)))
	if columns <= 0:
		columns = 1
	var row: int = index / columns
	var column: int = index % columns
	var rows: int = ceili(float(count) / float(columns))
	# Centre the block on the target: half a slot of offset per row/column either side of the middle.
	var x: float = (float(column) - (float(columns) - 1.0) * 0.5) * SLOT_SPACING
	var z: float = (float(row) - (float(rows) - 1.0) * 0.5) * SLOT_SPACING
	return Vector3(x, 0.0, z)

## The goal for the `index`-th unit of a `count`-unit order aimed at `target`, clamped inside the field so a
## formation ordered against the map edge does not send its outer files into the wall forever.
static func goal_for(index: int, count: int, target: Vector3) -> Vector3:
	var raw: Vector3 = target + slot_offset(index, count)
	return UnitSteering.clamp_to_field(raw, 0.0)
