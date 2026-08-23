extends RefCounted
class_name InterestMeter
## What this peer is actually being sent, against what exists. The client half of interest filtering, made
## into a number.
##
## A CLIENT DIAGNOSTIC, AND ONLY A CLIENT ONE. Interest runs where state authority is, so a server holds every
## entity by construction and this would report "everything, always" there -- which is true and says nothing.
## The facade does not publish the server's interest sets, and a demo should not need it to: the client can
## see for itself that an entity's rows stopped.
##
## ALL THREE AXES LOOK IDENTICAL FROM HERE, and that is the honest situation rather than a gap. A distance
## cull, a membership that does not match and a per-peer veto all stop the rows and leave the node frozen at
## its last pose. What an entity that stopped updating MEANS is the game's decision -- this demo draws a
## culled fighter faded and keeps drawing it, because a shooter that deleted opponents at the AOI edge would
## be a shooter where cover is a render distance.
##
## THE THRESHOLD IS IN TICKS, NOT SECONDS, so the reading is the same on a fast desktop and a loaded runner.

## How many ticks of silence count as "not being sent this". Generous rather than tight: a body at the far
## edge of the send rota can legitimately wait several ticks for its turn under a byte budget, and calling
## that a cull would report the rota as a filter.
const STALE_TICKS: int = 24

## One reading, so a caller gets a consistent set rather than three walks that each saw a different tick.
class Reading extends RefCounted:
	## Entities of each kind this peer is currently receiving, and how many exist at all.
	var fighters_fresh: int = 0
	var fighters_total: int = 0
	var props_fresh: int = 0
	var props_total: int = 0
	var cards_fresh: int = 0
	var cards_total: int = 0
	## Fighters being received, broken down by arena, indexed from ArenaConfig.FIRST_ARENA_ID.
	var fighters_by_arena: PackedInt32Array = PackedInt32Array()

	func total_fresh() -> int:
		return fighters_fresh + props_fresh + cards_fresh

	func total() -> int:
		return fighters_total + props_total + cards_total

## Whether a row that last arrived at `last_tick` still counts as being received at `now_tick`.
##
## A NEGATIVE `last_tick` MEANS NOTHING HAS EVER ARRIVED, which is not staleness -- it is a body this peer has
## never been sent. Both answer false, but only one of them is a cull, and a caller counting "never arrived"
## as stale would report a fresh join as a session-wide outage.
static func is_fresh(last_tick: int, now_tick: int) -> bool:
	if last_tick < 0:
		return false
	return now_tick - last_tick <= STALE_TICKS

## Read the whole world. `now_tick` is the session's present.
static func read(world: MatchDirector, now_tick: int) -> Reading:
	var out: Reading = Reading.new()
	out.fighters_by_arena.resize(ArenaConfig.ARENAS)
	if world == null:
		return out
	for fighter: FighterBody in world.fighters:
		if fighter == null:
			continue
		out.fighters_total += 1
		if not is_fresh(fighter.last_known_state(), now_tick):
			continue
		out.fighters_fresh += 1
		var slot: int = fighter.arena_id - ArenaConfig.FIRST_ARENA_ID
		if slot >= 0 and slot < out.fighters_by_arena.size():
			out.fighters_by_arena[slot] += 1
	for prop: PropBody in world.props:
		if prop == null:
			continue
		out.props_total += 1
		if is_fresh(prop.last_known_state(), now_tick):
			out.props_fresh += 1
	for card: Scorecard in world.scorecards:
		if card == null:
			continue
		out.cards_total += 1
		if is_fresh(card.last_known_state(), now_tick):
			out.cards_fresh += 1
	return out
