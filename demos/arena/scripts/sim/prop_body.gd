extends Node3D
class_name PropBody
## One piece of arena scenery on the STATE lane. It replicates a position, declares BOTH interest axes, and
## exists for two reasons that are both about scale rather than about play.
##
##   * THE SLOT TABLE. A block names its entity by a 16-bit session slot rather than a 64-bit id, and the
##     table binding slots to ids is distributed by the entity manifest as a whole table each time it changes.
##     A session with hundreds of entities is where that stops being free, and a demo with three fighters is
##     not one.
##   * THE INTEREST PASS. The pass is charged per candidate per peer per tick, and `interest_ms` in
##     `Net.bandwidth_metrics()` is the number it moves. A handful of entities cannot move it.
##
## IT DECLARES BOTH AXES, WHICH IS THE COMPOSITION WORTH SEEING. `set_anchor()` alone would cull by distance
## across arenas that overlap in coordinates -- the props of all three arenas sit on top of each other, so the
## radius would admit every one of them. `set_membership()` alone would send a whole arena's props to every
## peer in it whatever the radius said. Together the channel is culled by distance WITHIN its own arena, which
## is what it should be.
##
## NOTHING WRITES IT AFTER THE BUILD, and that is deliberate rather than lazy: a static value on a delta-coded
## lane costs a full block once and then nothing, which is what a byte budget should spend on scenery.

var arena_id: int = 0
## Arena-local position. The declared anchor.
var prop_pos: Vector3 = Vector3.ZERO

var _handle: NetStateHandle = null

func configure(arena: int, index: int) -> void:
	arena_id = arena
	name = ArenaNames.prop_node_name(arena, index)
	prop_pos = ArenaGeometry.prop_local(index)
	position = ArenaGeometry.local_to_world(arena, prop_pos)

func bind_net() -> void:
	_handle = Net.make_state(self)
	_handle.add_state(self, "prop_pos@half")
	# `"prop_pos"`, not `"prop_pos@half"`: the anchor names a live Vector3 read on the AUTHORITY to compute
	# relevancy. It is not a wire entry, so it takes no quantization suffix and costs no bytes.
	_handle.set_anchor("prop_pos")
	_handle.set_membership("arena_id")
	_handle.process_settings()

func entity_id() -> int:
	return 0 if _handle == null else _handle.entity_id()

## The tick of the newest authoritative row this peer holds for this prop. See FighterBody.last_known_state().
func last_known_state() -> int:
	return -1 if _handle == null else _handle.last_known_state()

## The tick of the newest row this peer DECODED for this prop, or -1 if none ever arrived.
##
## THE RECEIPT, NOT THE FRONTIER, and it is what a "still being sent this" question wants. See
## FighterBody.last_received_state().
func last_received_state() -> int:
	return -1 if _handle == null else _handle.last_received_state()

## Whether this peer is still being sent this prop's rows. Fails open -- see FighterBody.is_receiving().
func is_receiving(within_ticks: int = InterestMeter.STALE_TICKS) -> bool:
	return _handle == null or _handle.is_receiving(within_ticks)
