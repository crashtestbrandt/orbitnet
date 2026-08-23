extends RefCounted
class_name CloakPolicy
## Who must not be sent a cloaked fighter. Pure: it takes arrays and returns seat indices; it never calls
## `Net`, never touches the tree, and does not know what an entity id is. The session layer turns the indices
## it reports into `Net.set_entity_hidden()` calls.
##
## WHY A VETO AND NOT A MEMBERSHIP. Membership scopes a whole CLASS of entities by a declared key -- it is a
## property of the entity, one answer, read the same way by every peer. A cloak is a fact about a PAIR: the
## cloaked fighter's own team still sees it, at the same instant, at the same distance. `set_entity_hidden()`
## is the only call in the facade that can say something about one peer and one entity, which is what makes it
## the right one here and every other axis the wrong one.
##
## WHAT A WITHHELD FIGHTER LOOKS LIKE, AND WHY THAT IS THE FEATURE. The rows stop; the node stays. A vetoed
## fighter freezes on the watching peer at the last pose that arrived and does not despawn -- so a cloak reads
## as an opponent who kept running in the direction they were last seen going, which is a better cloak than
## invisibility and costs no code at all.
##
## THE VETO IS PER CONNECTION, NOT PER SEAT. A datagram is per connection and every seat behind it shares one,
## so a veto applies to all of a connection's seats -- including one that joins later. A split-screen player
## whose two fighters are on opposite teams therefore cannot see the cloaked one on either half of the screen.
## That is a real limit and it is the datagram's, not this policy's; this file reports per seat and the
## session layer folds the seats of one connection together.

## Whether `viewer_team` may be shown a fighter on `target_team` that is currently cloaked.
static func may_see(viewer_team: int, target_team: int, cloaked: bool) -> bool:
	return (not cloaked) or viewer_team == target_team

## Recompute, for one viewing CONNECTION, which seats' fighters it must not receive.
##
## `viewer_teams` is every team that connection has a seat on -- the plural matters, because a connection
## whose two seats are on opposite teams may see both teams' cloaks, and a veto it does not deserve on one
## seat would blind the other. `cloaked` and `teams` are parallel arrays over the whole seat pool.
##
## Returns the seats to WITHHOLD, which the caller diffs against what it already withheld.
static func hidden_seats(viewer_teams: PackedInt32Array, teams: PackedInt32Array,
		cloaked: PackedByteArray) -> PackedInt32Array:
	var out: PackedInt32Array = PackedInt32Array()
	var count: int = mini(teams.size(), cloaked.size())
	for seat: int in count:
		if cloaked[seat] == 0:
			continue
		var visible: bool = false
		for team: int in viewer_teams:
			if may_see(team, teams[seat], true):
				visible = true
				break
		if not visible:
			out.push_back(seat)
	return out

## The seats whose veto state CHANGED between two withheld sets, so the caller places and retracts exactly the
## vetoes that moved.
##
## RE-VETOING AN ENTITY ALREADY IN THAT STATE IS NOT FREE, which is why this exists rather than the caller
## simply asserting the whole set every tick. Starting a veto drops the entity from that peer's interest and
## CLEARS ITS DELTA BOOKKEEPING, so a later retraction sends a full block rather than a delta against a base
## the peer dropped. Asserting it every tick would hold every withheld entity permanently at "send a full
## block next".
static func changes(previous: PackedInt32Array, current: PackedInt32Array) -> PackedInt32Array:
	var changed: PackedInt32Array = PackedInt32Array()
	for seat: int in current:
		if not previous.has(seat):
			changed.push_back(seat)
	for seat: int in previous:
		if not current.has(seat):
			changed.push_back(seat)
	return changed
