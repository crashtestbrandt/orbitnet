extends RefCounted
class_name ArenaNames
## Every node name the wire depends on, in one place.
##
## AN ENTITY ID IS A HASH OF A NODE PATH. Two peers that build the same world under different names build
## different entity ids, and nothing errors -- the rows simply go nowhere. Godot's automatic names are
## allocation-order dependent, so every replicated node in this demo is named explicitly, from here, before it
## enters the tree. `world_signature()` is what turns a disagreement into a caught failure: both peers print
## it, and the probe compares them.

const WORLD_ROOT: StringName = &"Match"
const ARENAS_ROOT: StringName = &"Arenas"
const FIGHTERS_ROOT: StringName = &"Fighters"
const PROPS_ROOT: StringName = &"Props"
const SCORECARDS_ROOT: StringName = &"Scorecards"
## The input node under a fighter. Its path is part of the schema every peer must build identically.
const INPUT_NODE: StringName = &"Input"
## The hit capsule under a fighter. Not replicated -- but it is what a rewound shot names, so it is named.
const HITBOX_NODE: StringName = &"Hit"

static func arena_node_name(arena_id: int) -> String:
	return "Arena%02d" % arena_id

static func fighter_node_name(seat: int) -> String:
	return "Fighter%03d" % seat

static func prop_node_name(arena_id: int, index: int) -> String:
	return "Prop%02d_%04d" % [arena_id, index]

static func scorecard_node_name(arena_id: int) -> String:
	return "Score%02d" % arena_id

## An order-independent digest of every replicated node path in the world.
##
## ORDER-INDEPENDENT ON PURPOSE. The two peers build the same set; requiring them to build it in the same
## sequence would make the signature a test of the build loop rather than of the names. Summing a per-path
## hash answers the question actually being asked -- is this the same SET of paths -- and a mismatched set is
## what breaks replication.
static func world_signature(paths: PackedStringArray) -> int:
	var sum: int = 0
	for path: String in paths:
		sum += hash(path)
	return sum
