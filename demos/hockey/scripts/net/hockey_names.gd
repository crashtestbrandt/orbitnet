extends RefCounted
class_name HockeyNames
## Deterministic node naming, and the world signature that proves it worked. Pure.
##
## OrbitNet does not assign entity ids -- it DERIVES them, as FNV-1a of the synchronizer root's node path. That
## is a good design (no id-assignment handshake, no per-entity RPC routing, a reconnecting client re-derives
## the same ids) and it has exactly one requirement: every peer must build the same node paths.
##
## Godot's automatic naming does not do that. `add_child(Node3D.new())` produces `@Node3D@27`, and the number
## is a per-process allocation counter -- it depends on how many nodes that process has ever created, which
## depends on whether a probe attached, whether the editor is running, which scene was loaded first. Two peers
## WILL disagree, no error is raised, and the symptom is that replication silently goes nowhere: the server
## broadcasts entity 0x8f3a..., the client listens for 0x21bc..., and the puck sits still.
##
## So every node that carries a synchronizer is named explicitly, from its seat index, through this file.
##
## The FNV implementation below is duplicated from the RTS demo's equivalent rather than shared. The two demos
## are separate Godot projects -- they have to be, because they disagree about the [orbitnet] settings block --
## and the addon is the only surface they share, which is netcode rather than demo instrumentation. Both copies
## are pinned against the same published test vectors.

## The scene-tree names the rink is built under. Fixed strings, because they are part of the path the entity id
## is derived from -- renaming one is a wire-compatibility break, not a refactor.
const RINK_ROOT: String = "Rink"
const MALLETS_ROOT: String = "Mallets"
const PUCK_NODE: String = "Puck"
const SCORE_NODE: String = "Score"
const SERVE_NODE: String = "Serve"
## The mallet's client-authority input child. Its path is part of the schema every peer must build identically.
const INPUT_NODE: String = "Input"

## The node name for the mallet of `seat`. Zero-padded and fixed width so the pool sorts in seat order in the
## remote scene-tree inspector, which is a real debugging convenience at 32 mallets.
static func mallet_node_name(seat: int) -> String:
	return "M%02d" % seat

## Recover the seat from a node name produced by [method mallet_node_name], or -1 if it is not one.
static func seat_from_name(name: String) -> int:
	if name.length() != 3 or not name.begins_with("M"):
		return -1
	var digits: String = name.substr(1)
	if not digits.is_valid_int():
		return -1
	return digits.to_int()

# --- the world signature --------------------------------------------------------------------------
# The direct gate on all of the above. Each peer hashes the sorted list of node paths it actually built and
# prints it; two peers printing different numbers built different worlds, and every symptom after that is
# downstream of it.
#
# This proves PATH EQUALITY, which is the property entity-id agreement is derived from -- it does not read the
# backend's ids (the facade deliberately does not expose them, and a demo should not need it to). If the paths
# match, the FNV ids match, because the ids are a pure function of the paths.

## FNV-1a, 64-bit.
##
## The offset basis is written as its signed two's-complement value because GDScript's `int` is a SIGNED 64-bit
## integer and 14695981039346656037 does not fit. Arithmetic wraps modulo 2^64, which is exactly what FNV
## wants, so the result matches the canonical algorithm bit for bit despite the sign.
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
