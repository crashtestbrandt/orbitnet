extends RefCounted
class_name ScoutPolicy
## One seat's view of the battlefield: which enemy units it can currently see, and what changed since last
## time it was asked.
##
## PURE. It takes arrays and returns indices; it never calls `Net`, never touches the tree, and does not know
## what an entity id is. The session layer turns the indices it reports into `Net.set_entity_hidden()` calls.
## That split is the only reason this is unit-testable at all -- the veto itself needs a live session.
##
## WHY A VETO AND NOT A RADIUS. Distance culling is a property of the ENTITY: one position, one answer, read
## the same way by every peer. Fog of war is a property of the PAIR -- seat 0 can see a unit that seat 1
## cannot, at the same instant, at the same distance. `Net.set_entity_hidden(peer, entity, true)` is the only
## call in the facade that can say a thing about one peer and one entity, which is what makes it the right
## one here and `set_aoi_radius()` the wrong one.
##
## WHAT A WITHHELD UNIT LOOKS LIKE, AND WHY THAT IS THE FEATURE. The rows stop; the node stays. A vetoed unit
## freezes on the watching peer at the last pose that arrived and does not despawn -- which is precisely the
## last-known-position ghost the genre already draws by hand. Nothing here has to fake it.
##
## HYSTERESIS, FOR THE SAME REASON THE INTEREST BAND HAS IT. A unit walking the vision edge would otherwise
## flip every tick, and each flip is a full block on the way back in: starting a veto clears the peer's delta
## bookkeeping, so a retraction cannot send a delta against a base the peer dropped. Enter at
## `VISION_RADIUS_M`, leave at `VISION_EXIT_M`.

## How far a unit sees, metres.
const VISION_RADIUS_M: float = 26.0
## ...and how far a seen unit must get before it is lost. 1.25x the entry radius, matching the band the
## backend's own interest filter uses.
const VISION_EXIT_M: float = VISION_RADIUS_M * 1.25

## Per-index visibility, 1 = this seat can see it. Sized to the unit pool on the first refresh and never
## resized after, because the pool is static.
var _visible: PackedByteArray = PackedByteArray()
var _primed: bool = false

## Recompute what `viewer_seat` can see, and return the indices whose answer CHANGED.
##
## Returning only the changes is what keeps the caller honest: `set_entity_hidden()` on an entity already in
## that state clears its delta bookkeeping again, so calling it every tick for every unit would hold every
## withheld entity permanently at "send a full block next".
##
## `seats`, `positions` and `alive` are parallel arrays over the whole unit pool. A seat's own units are
## never hidden from it, and a dead unit is treated as any other -- its liveness rides in the same rows the
## veto is stopping, so a unit that dies out of sight stays a live-looking ghost until it is seen again.
func refresh(viewer_seat: int, seats: PackedInt32Array, positions: PackedVector3Array,
		alive: PackedByteArray) -> PackedInt32Array:
	var count: int = mini(mini(seats.size(), positions.size()), alive.size())
	if _visible.size() != count:
		_visible.resize(count)
		# SEEDED VISIBLE, BECAUSE THAT IS WHERE THE BACKEND STARTS. No veto has been placed yet, so every
		# unit is reaching every peer. Seeding the mirror to 0 would make the first pass's diff report only
		# the units that ARE seen -- and those are precisely the ones needing no veto -- while the units to
		# withhold would compare equal to the seed and never be reported at all.
		_visible.fill(1)
		_primed = false

	var eyes: PackedVector3Array = PackedVector3Array()
	for index: int in count:
		if seats[index] == viewer_seat and alive[index] != 0:
			eyes.push_back(positions[index])

	var changed: PackedInt32Array = PackedInt32Array()
	for index: int in count:
		var seen: bool = _resolve(index, viewer_seat, seats, positions, eyes)
		var was: bool = _visible[index] != 0
		if seen == was and _primed:
			continue
		_visible[index] = 1 if seen else 0
		if seen != was:
			changed.push_back(index)
	# The first refresh REPORTS, and reports the units to withhold. The mirror was seeded to the backend's
	# own starting state (everything visible), so `changed` here holds exactly the indices that resolved to
	# not-seen -- the set the caller has to place a veto on. Returning nothing would leave every unit that
	# began the session out of vision replicating, while `hidden_count()` counted it as withheld.
	_primed = true
	return changed

## Whether `index` is currently visible to the seat this policy belongs to. Everything is visible before the
## first refresh, which is the same fail-open direction the interest filter takes.
func is_visible(index: int) -> bool:
	if index < 0 or index >= _visible.size():
		return true
	if not _primed:
		return true
	return _visible[index] != 0

## Every index currently NOT visible. The readout counts these; the session layer uses `refresh()`.
func hidden_count() -> int:
	if not _primed:
		return 0
	var hidden: int = 0
	for value: int in _visible:
		if value == 0:
			hidden += 1
	return hidden

## Forget everything, so the next refresh re-reports the whole set. Called when the fog is switched off and
## every veto is retracted: the policy's memory and the backend's state have to be cleared together, or the
## next refresh reports no change and leaves half the units withheld.
func clear() -> void:
	_visible.fill(1)
	_primed = false

func _resolve(index: int, viewer_seat: int, seats: PackedInt32Array, positions: PackedVector3Array,
		eyes: PackedVector3Array) -> bool:
	if seats[index] == viewer_seat:
		return true          # your own units are never withheld from you
	if eyes.is_empty():
		return false         # a seat with nothing alive sees nothing
	var reach: float = VISION_EXIT_M if (_primed and _visible[index] != 0) else VISION_RADIUS_M
	var reach_sq: float = reach * reach
	var at: Vector3 = positions[index]
	for eye: Vector3 in eyes:
		if eye.distance_squared_to(at) <= reach_sq:
			return true
	return false
