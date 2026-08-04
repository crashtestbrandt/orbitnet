extends RefCounted
class_name RtsNames
## Deterministic node naming, and the world signature that proves it worked. Pure.
##
## THIS IS THE SINGLE MOST LOAD-BEARING FILE IN THE DEMO and it is fifty lines of string formatting.
##
## OrbitNet does not assign entity ids -- it DERIVES them, as FNV-1a of the synchronizer root's node path.
## That is a good design (no id-assignment handshake, no per-entity RPC routing, a client that reconnects
## re-derives the same ids), and it has exactly one requirement: every peer must build the same node paths.
##
## Godot's automatic naming does not do that. `add_child(Node3D.new())` produces `@Node3D@27`, and the number
## is a per-process allocation counter -- it depends on how many nodes that process has ever created, which
## depends on the menu the player passed through, whether a probe attached, whether the editor is running.
## Two peers WILL disagree, no error is raised, and the symptom is that replication silently goes nowhere:
## the server broadcasts entity 0x8f3a..., the client is listening for 0x21bc..., and every unit sits still.
##
## So every node that carries a synchronizer is named explicitly, from its id, through this file.

## The scene-tree names the world is built under. Fixed strings, because they are part of the path the entity
## id is derived from -- renaming one is a wire-compatibility break, not a refactor.
const WORLD_ROOT: String = "World"
const UNITS_ROOT: String = "Units"
const COMMANDERS_ROOT: String = "Commanders"

## The node name for unit `id`. Zero-padded and fixed-width so the name sorts in id order in the remote scene
## tree inspector, which is a real debugging convenience at 96 units.
static func unit_node_name(id: int) -> String:
	return "U%08d" % id

## The node name for the commander avatar of `seat`.
static func commander_node_name(seat: int) -> String:
	return "C%02d" % seat

## The node name for a seat's order channel. One NetCommand per SEAT, not per unit: a NetCommand is a Node
## that routes by node path, so per-unit channels would mean 96 nodes and 96 registrations for a channel that
## is naturally batched -- an order names its units in the payload.
static func orders_node_name(seat: int) -> String:
	return "Orders%02d" % seat

## Recover the unit id from a node name produced by [method unit_node_name], or -1 if it is not one.
static func unit_id_from_name(name: String) -> int:
	if name.length() != 9 or not name.begins_with("U"):
		return -1
	var digits: String = name.substr(1)
	if not digits.is_valid_int():
		return -1
	return digits.to_int()

# --- the world signature --------------------------------------------------------------------------
# The direct gate on all of the above. Each peer hashes the sorted list of node paths it actually built and
# prints it; the probe asserts the two peers printed the same number.
#
# This proves PATH EQUALITY, which is the property entity-id agreement is derived from -- it does not read
# the backend's ids (the facade deliberately does not expose them, and a demo should not need to). If the
# paths match, the FNV ids match, because the ids are a pure function of the paths. If the paths ever stop
# matching, this number changes and the probe fails, which is the regression worth catching.

## FNV-1a, 64-bit.
##
## The offset basis is written as its signed two's-complement value because GDScript's `int` is a SIGNED
## 64-bit integer and 14695981039346656037 does not fit. Arithmetic wraps modulo 2^64, which is exactly what
## FNV wants, so the result matches the canonical algorithm bit for bit despite the sign.
static func fnv1a_64(text: String) -> int:
	const OFFSET_BASIS: int = -3750763034362895579   # 14695981039346656037 as signed 64-bit
	const PRIME: int = 1099511628211
	var hash: int = OFFSET_BASIS
	for byte: int in text.to_utf8_buffer():
		hash ^= byte
		hash *= PRIME
	return hash

## Hash a set of node paths into one comparable number. Sorted first, so the signature depends on WHICH paths
## exist and not on the order they happened to be collected in -- a peer that builds the same world in a
## different order is not a bug, and this must not report one.
static func world_signature(paths: PackedStringArray) -> int:
	var sorted: PackedStringArray = paths.duplicate()
	sorted.sort()
	return fnv1a_64("\n".join(sorted))
