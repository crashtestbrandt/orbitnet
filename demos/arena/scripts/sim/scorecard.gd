extends Node
class_name Scorecard
## One arena's score. A STATE channel with a membership and NO anchor, which is the third shape of interest
## this demo has to show.
##
## MEMBERSHIP IS WHAT A POSITIONLESS CHANNEL HAS INSTEAD OF A RADIUS. This node replicates two integers and no
## position, so there is no distance for a radius to work with and `set_anchor()` would have nothing to name.
## Declaring nothing at all is the fail-open default -- every peer in every arena receives every arena's
## score -- and `set_membership("arena_id")` is what bounds it to the one arena it is about while leaving it
## uncullable inside that arena, which is what a scoreboard should be.
##
## THE STATE LANE, NOT THE ROLLBACK LANE. A kill is discovered OUTSIDE the tick, in the shot resolution that a
## command triggers. On the rollback lane the next restore would write the recorded history back over it and
## the score would silently stay at zero on the server. `make_state()` promises exactly what is needed here
## and `register_rollback_body()` promises the opposite.

var arena_id: int = 0

## Kills per team, packed one per byte so the pair costs one i64 rather than two.
var net_score: int = 0
## The seat that scored last and the sequence, so a client can announce a kill without a second channel.
var net_last_kill: int = 0

var _handle: NetStateHandle = null
var _sequence: int = 0

func configure(arena: int) -> void:
	arena_id = arena
	name = ArenaNames.scorecard_node_name(arena)

func bind_net() -> void:
	_handle = Net.make_state(self)
	_handle.add_state(self, "net_score")
	_handle.add_state(self, "net_last_kill")
	# NO set_anchor(). There is no position here to be culled by, and naming one that was not a world position
	# would park this channel at the origin and cull it for everybody.
	_handle.set_membership("arena_id")
	_handle.process_settings()

func entity_id() -> int:
	return 0 if _handle == null else _handle.entity_id()

## The tick of the newest authoritative row this peer holds for this scorecard.
func last_known_state() -> int:
	return -1 if _handle == null else _handle.last_known_state()

# --- scoring -------------------------------------------------------------------------------------------
## SERVER-SIDE. Credit `team` with a kill by `seat`.
func credit(team: int, seat: int) -> void:
	var scores: PackedInt32Array = teams()
	if team < 0 or team >= scores.size():
		return
	scores[team] = mini(scores[team] + 1, 0xFFFF)
	net_score = scores[0] | (scores[1] << 16)
	_sequence = (_sequence + 1) & 0xFFFF
	net_last_kill = (seat & 0xFFFF) | (_sequence << 16)

## The two teams' scores, team 0 first.
func teams() -> PackedInt32Array:
	return PackedInt32Array([net_score & 0xFFFF, (net_score >> 16) & 0xFFFF])

## The seat that scored last, and the kill sequence. A client redraws its feed when the sequence moves.
func last_kill_seat() -> int:
	return net_last_kill & 0xFFFF

func kill_sequence() -> int:
	return (net_last_kill >> 16) & 0xFFFF
