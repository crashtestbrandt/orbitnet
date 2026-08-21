extends Node3D
class_name MalletBody
## One seat's mallet: a rollback body with server-owned state and client-owned input.
##
## THE FAMILIAR HALF of the rollback lane -- an entity whose owner authors continuous per-tick input and
## predicts locally. `net_pos` is registered FIRST and as a Vector3 because the backend takes a peer's first
## Vec3 state property as its interest anchor; this demo never turns interest management on (a 2 m table has
## nothing to cull) but a body whose anchor would be a velocity or a local offset is a trap worth not setting.
##
## THE VELOCITY IS ON THE WIRE, and that is not an optimisation. The rollback lane's state set has to be the
## COMPLETE simulation state: a restore that returns position without velocity resumes the resim from the wrong
## basis and diverges immediately, in a way that looks like a physics bug rather than a schema one. The RTS
## demo never had to make this point -- its units are display-only and its cursor has no momentum.

## STATE, server-authored: the authoritative pose, after the server has clamped the client's request.
var net_pos: Vector3 = Vector3.ZERO
## STATE, server-authored: the velocity the mallet actually has, which is what strikes the puck.
var net_vel: Vector3 = Vector3.ZERO

var seat: int = -1
var owner_peer: int = 0

var input: MalletInput = null
var _handle: NetRollbackHandle = null

## Configure identity. Called BEFORE the node enters the tree: the name feeds the node path, and the node path
## is what the backend hashes into this entity's id.
func configure(seat_index: int, peer: int) -> void:
	seat = seat_index
	owner_peer = peer
	name = HockeyNames.mallet_node_name(seat_index)
	net_pos = TableGeometry.home_point(seat_index)
	net_vel = Vector3.ZERO
	position = net_pos
	input = MalletInput.new()
	# A STABLE child name for the same reason: the input node's path is part of the schema every peer must
	# build identically.
	input.name = HockeyNames.INPUT_NODE
	input.nin_target = net_pos
	add_child(input)

## Register the rollback lane. Called AFTER the mallet is in the tree at its final path.
func bind_net() -> void:
	if Net.is_offline():
		# Offline the handle is inert and every property simply sticks where it is written -- the same code
		# path, which is the point of the facade's OFFLINE contract. RinkDirector drives advance() directly.
		_handle = Net.register_rollback_body(self, input, [], [], false, [])
		return
	# The mallet's authority stays with the server (peer 1, the default); the INPUT node's moves to the owner.
	# Setting it before registration matters: the backend reads the authority when it processes settings.
	input.set_multiplayer_authority(owner_peer if owner_peer > 0 else 1)
	var predict: bool = Net.is_server() or owner_peer == multiplayer.get_unique_id()
	_handle = Net.register_rollback_body(
		self,
		input,
		["net_pos@half", "net_vel@half"],   # STATE  -- server-authored
		["nin_target@half"],                # INPUT  -- client-authored, validated by node authority
		predict)

## One simulation step. Called by the backend as `_rollback_tick` when networked, and by RinkDirector's offline
## accumulator when there is no session -- one body, two clocks, so "it behaves differently offline" cannot
## happen quietly.
func advance(delta: float) -> void:
	var state: MalletControl.State = MalletControl.State.new(net_pos, net_vel)
	var next: MalletControl.State = null
	if owner_peer <= 0:
		# A vacant seat is parked and inert, re-asserted EVERY tick rather than once. The rollback lane
		# restores recorded history onto these properties, so a single write from outside the tick would be
		# undone by the next restore.
		next = MalletControl.park(seat)
	elif _receives_input():
		next = MalletControl.step_toward(state, input.nin_target, seat, delta)
	else:
		next = MalletControl.step_coast(state, seat, delta)
	net_pos = next.position
	net_vel = next.velocity
	# The transform is not replicated -- the renderer reads net_pos, because an exempt remote mallet never runs
	# this function at all. Keeping the node where its mallet is costs one assignment and makes the remote
	# scene-tree view legible while debugging.
	position = net_pos

func _rollback_tick(delta: float, _tick: int, _is_fresh: bool) -> void:
	advance(delta)

## Re-point this mallet at a different owning peer. Called on EVERY peer whenever the roster changes, so
## authority agrees everywhere: a peer that missed the update would keep predicting (or keep refusing to
## predict) the wrong body, and the backend would start rejecting its input frames as unauthorized. Peer 0
## means "nobody", which parks the mallet and resolves authority to the server.
func set_owner_peer(peer: int) -> void:
	owner_peer = peer
	if peer > 0:
		# Re-seating puts the mallet back on its home spot rather than wherever the last player left it. Only
		# the authority's write survives; a client's is corrected on the next row, which is exactly right.
		input.nin_target = TableGeometry.home_point(seat)
	if Net.is_offline() or _handle == null:
		return
	input.set_multiplayer_authority(peer if peer > 0 else 1)
	_handle.process_authority()

## Write this frame's requested point. Called only on the peer that OWNS this mallet; on anyone else the input
## node is not writable and the backend would ignore it anyway.
func set_local_target(point: Vector3) -> void:
	input.nin_target = TableGeometry.flatten(point)

## Whether somebody is sitting here. Derived from the roster, which every peer receives reliably, so this is
## the same answer everywhere -- it is deliberately NOT replicated state.
func is_occupied() -> bool:
	return owner_peer > 0

## This mallet's team, derived from its seat index.
func team() -> int:
	return HockeyConfig.team_of_seat(seat)

## Whether this mallet's owner is currently mis-predicting. Diagnostics for the HUD.
func is_predicting() -> bool:
	return _handle != null and _handle.is_predicting()

# --- internals -------------------------------------------------------------------------------------
# Whether THIS peer has this mallet's input frames at all.
#
# The server receives every client's, and the owning client authors its own -- both run the real simulation.
# Any OTHER peer has neither: rollback input travels client -> server only and is never rebroadcast, because
# that would be an O(N^2) input fan-out. Chasing an input frame that was never written would send the mallet
# toward a point the player left long ago, so those peers dead-reckon on the last authoritative velocity
# instead. See MalletControl for why that is a different simulation on purpose.
func _receives_input() -> bool:
	return Net.is_offline() or Net.is_server() or input.is_multiplayer_authority()
