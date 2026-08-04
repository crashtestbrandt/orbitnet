extends Node3D
class_name CommanderAvatar
## One player's presence in the world: their command cursor. This is the demo's ONLY rollback entity, exactly
## one per player, and it is here for two independent reasons.
##
## 1. IT IS THE ONLY THING AN RTS CONTINUOUSLY AUTHORS. The rollback lane exists for input that arrives every
##    tick and can be predicted from the last one. A unit's orders are sparse and unpredictable; a cursor is a
##    smooth stream. So the lane split falls out of the game, not out of a preference: cursors roll back,
##    units do not.
##
## 2. IT IS THE AOI ANCHOR, AND WITHOUT IT AOI CANNOT FUNCTION AT ALL. The backend computes a peer's interest
##    set by finding the rollback entity whose INPUT authority is that peer, taking its first Vec3 state
##    property as the centre, and culling other rollback entities against it. A peer with no rollback body
##    has no centre, and the backend correctly falls back to "everything stays in interest". So `cmd_cursor`
##    being the first registered Vec3 state property is not incidental -- it is what `Net.set_aoi_radius()`
##    reads.
##
## And it demonstrates the server-authoritative split on a NON-CHARACTER entity, which is the API shape people
## most often get wrong: state authority on this node (the server), input authority on the child
## (the owning client), and a `_rollback_tick` that turns one into the other under server validation.

## Server-owned STATE: the authoritative cursor, after the server has validated the client's request.
## Registered FIRST and as a Vector3 because the AOI anchor is the first Vec3 state property -- see above.
var cmd_cursor: Vector3 = Vector3.ZERO

## COSMETIC: how many units this player currently has selected. Replicated so an opponent's HUD can show it,
## never restored during rollback, and never counted as a misprediction -- which is exactly right for a value
## that is presentation-only and whose exact tick nobody cares about.
var cmd_sel_count: int = 0

## COSMETIC: the live drag box on the ground, as (centre x, centre z, half-extent). Zero half-extent means no
## drag in progress. A square rather than a rectangle so it fits one Vector3 and quantizes as one @half --
## the box is a hint that a player is selecting, not a UI element anyone measures.
var cmd_drag: Vector3 = Vector3.ZERO

var seat: int = -1
var owner_peer: int = 0

var input: CommanderInput = null
var _handle: NetRollbackHandle = null

## Configure identity. Called BEFORE the node enters the tree: the name feeds the node path, and the node path
## is what the backend hashes into this entity's id.
func configure(seat_index: int, peer: int) -> void:
	seat = seat_index
	owner_peer = peer
	name = RtsNames.commander_node_name(seat_index)
	cmd_cursor = RtsConfig.spawn_center(seat_index)
	input = CommanderInput.new()
	# A STABLE child name for the same reason: the input node's path is part of the schema every peer must
	# build identically.
	input.name = "Input"
	input.nin_cursor = cmd_cursor
	add_child(input)

## Register the rollback lane. Called AFTER the avatar is in the tree at its final path.
func bind_net() -> void:
	if Net.is_offline():
		# Offline the handle is inert and every property simply sticks where it is written -- the same code
		# path, which is the point of the facade's OFFLINE contract.
		_handle = Net.register_rollback_body(self, input, [], [], false, [])
		return
	# The avatar's authority stays with the server (peer 1, the default); the INPUT node's moves to the owner.
	# Setting it before registration matters: the backend reads the authority when it processes settings.
	input.set_multiplayer_authority(owner_peer)

	var predict: bool = Net.is_server() or owner_peer == multiplayer.get_unique_id()
	_handle = Net.register_rollback_body(
		self,
		input,
		["cmd_cursor@half"],          # STATE  — server-authored, and the AOI anchor
		["nin_cursor@half"],          # INPUT  — client-authored, validated by node authority
		predict,
		["cmd_sel_count", "cmd_drag@half"])   # COSMETIC — replicated, never restored, never a misprediction

## The rollback tick. Called by the backend on this node with (dt, tick, is_fresh) -- on the server for every
## commander, and on the owning client for its own (that is local prediction).
##
## The "simulation" is deliberately one line of validation: the authoritative cursor is the client's requested
## cursor, CLAMPED to the field. That is small but it is not trivial -- it is the moment a client-authored
## value becomes server-owned state, and it is where a bounds check belongs. A client that writes a cursor a
## kilometre off the map gets a cursor at the map edge, on every peer, including its own after reconciliation.
func _rollback_tick(_delta: float, _tick: int, _is_fresh: bool) -> void:
	cmd_cursor = UnitSteering.clamp_to_field(input.nin_cursor, 0.0)
	# Keep the transform on the cursor as well. Nothing replicates it (the transform is not a registered
	# property), but a node that sits where its cursor is makes the remote-scene-tree view legible while
	# debugging, and costs one assignment per tick.
	position = cmd_cursor

## Re-point this commander at a different owning peer, and re-evaluate prediction.
##
## Called on EVERY peer whenever the roster changes -- a player joining, leaving, or reconnecting. Authority
## is a property of a node, so all peers must be told; a peer that missed the update would keep predicting
## (or keep refusing to predict) the wrong body, and the backend would start rejecting its input frames as
## unauthorized. Peer 0 means "nobody", which resolves to the server holding the seat.
func set_owner_peer(peer: int) -> void:
	owner_peer = peer
	if Net.is_offline() or _handle == null:
		return
	input.set_multiplayer_authority(peer if peer > 0 else 1)
	_handle.process_authority()

## Write this frame's desired cursor. Called only on the peer that OWNS this commander; on anyone else the
## input node is not writable and the backend would ignore it anyway.
func set_local_cursor(point: Vector3) -> void:
	input.nin_cursor = point

## Publish the presentation-only selection state onto the cosmetic channel.
func set_selection_hint(count: int, drag_centre: Vector3, drag_half_extent: float) -> void:
	cmd_sel_count = count
	cmd_drag = Vector3(drag_centre.x, drag_centre.z, drag_half_extent)

## Whether this avatar's owner is currently mis-predicting. Diagnostics for the HUD -- with a one-line sim
## this should essentially never be true, and if it is, something is wrong upstream of the demo.
func is_predicting() -> bool:
	return _handle != null and _handle.is_predicting()
