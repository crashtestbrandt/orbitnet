extends Node
class_name Scoreboard
## The match record: goals per team, and the last goal as a one-shot event. THE STATE LANE, and the reason is
## the single most common OrbitNet bug.
##
## A goal is discovered INSIDE the puck's `_rollback_tick`. It would be natural to keep the score on the puck
## alongside everything else the tick touches -- and it would be wrong, silently: the rollback lane restores
## recorded history onto its properties every tick, so the increment would be overwritten by the next restore
## and the score would sit at zero on the server with nothing erroring anywhere. `make_state()` promises
## exactly what is needed here and `register_rollback_body()` promises the opposite.
##
## So the score is written from inside the tick and stored on a lane that never restores. The direction that
## catches people out is the other one -- a value written OUTSIDE the tick landing on the rollback lane -- but
## it is the same rule and this is the same fix.
##
## THIS DEMO'S STATE LANE CARRIES ALMOST NOTHING, and that is the honest contrast with the RTS demo's 96 units.
## Air hockey is nearly all simulation, so nearly everything is on the rollback lane; what is left is the
## bookkeeping the simulation must not own.

## Packed goals: team 0 in the low 16 bits, team 1 in the next 16. Two counters in one i64 rather than two
## properties, because a state-lane block pays a per-property entry and these two always change together.
var net_score: int = 0
## Packed (scoring team, goal sequence). The sequence only ever needs to be COMPARED for change, so a client
## plays its goal flash exactly once by noticing the number moved -- no reliable event channel required.
var net_last_goal: int = 0

const _COUNTER_BITS: int = 16
const _COUNTER_MASK: int = (1 << _COUNTER_BITS) - 1
const _GOAL_SEQ_BITS: int = 16
const _GOAL_SEQ_MASK: int = (1 << _GOAL_SEQ_BITS) - 1

var _handle: NetStateHandle = null
var _sequence: int = 0

func _init() -> void:
	# Named at construction, before this node is ever added to a tree: its path is what the backend hashes into
	# this entity's id, so renaming it after the fact would silently re-key it.
	name = HockeyNames.SCORE_NODE

## Register the state lane. Called AFTER the node is in the tree at its final path.
func bind_net() -> void:
	_handle = Net.make_state(self)
	_handle.add_state(self, "net_score")
	_handle.add_state(self, "net_last_goal")
	_handle.process_settings()
	# No anchor and no interpolator, deliberately. An unanchored state channel is ALWAYS relevant, which is
	# right for a scoreboard; and interpolating a goal count between two integers would render halves of a
	# goal, which the interpolator would decline to do anyway (it steps non-blendable types) but which nobody
	# should have to find out by trying.

## Award a goal to `team`. Server-only (or offline), called from inside the puck's tick on the `is_fresh` pass.
func award(team: int) -> void:
	if team < 0 or team > 1:
		return
	_sequence = (_sequence % _GOAL_SEQ_MASK) + 1
	net_score = pack_score(
		goals(0) + (1 if team == 0 else 0),
		goals(1) + (1 if team == 1 else 0))
	net_last_goal = pack_goal(team, _sequence)

## Goals scored by `team`, on any peer.
func goals(team: int) -> int:
	return score_of(net_score, team)

## The team that scored last, or -1 before the first goal.
func last_scorer() -> int:
	return goal_team(net_last_goal)

## The last goal's sequence number, 0 before the first goal. A client plays its flash when this changes.
func last_sequence() -> int:
	return goal_sequence(net_last_goal)

## Clear the record (session teardown / a fresh world).
func reset() -> void:
	_sequence = 0
	net_score = 0
	net_last_goal = 0

# --- packing ---------------------------------------------------------------------------------------
# Static so the tests can exercise the packing directly with no node and no session.

static func pack_score(team0: int, team1: int) -> int:
	return ((team1 & _COUNTER_MASK) << _COUNTER_BITS) | (team0 & _COUNTER_MASK)

static func score_of(packed: int, team: int) -> int:
	if team == 1:
		return (packed >> _COUNTER_BITS) & _COUNTER_MASK
	return packed & _COUNTER_MASK

## `team` is stored offset by one so that a freshly zeroed entity reads as "no goal yet" rather than as a goal
## for team 0 -- the same trick the RTS demo uses for a unit's target id, for the same reason.
static func pack_goal(team: int, sequence: int) -> int:
	return ((sequence & _GOAL_SEQ_MASK) << 2) | ((team + 1) & 3)

static func goal_team(packed: int) -> int:
	return (packed & 3) - 1

static func goal_sequence(packed: int) -> int:
	return (packed >> 2) & _GOAL_SEQ_MASK
