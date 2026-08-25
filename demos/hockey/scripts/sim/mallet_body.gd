extends Node3D
class_name MalletBody
## One seat's mallet: a rollback body with server-owned state and client-owned input.
##
## THE FAMILIAR HALF of the rollback lane -- an entity whose owner authors continuous per-tick input and
## predicts locally. `net_pos` is registered FIRST and as a Vector3 because the backend takes a peer's first
## Vec3 state property as its interest anchor; this demo never turns interest management on (a 2 m table has
## nothing to cull) but a body whose anchor would be a velocity or a local offset is a trap worth not setting.
##
## THE VELOCITY IS ON THE WIRE, and that is not an optimization. The rollback lane's state set has to be the
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

## The two lanes' entries, in the order the bulk hooks fill their arrays. See PuckBody.STATE_PROPS for why
## they are named once rather than spelled at the registration.
const STATE_PROPS: Array[String] = ["net_pos@half", "net_vel@half"]
const INPUT_PROPS: Array[String] = ["nin_target@half"]

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
		STATE_PROPS,   # server-authored
		INPUT_PROPS,   # client-authored, validated by node authority
		predict)
	set_bulk_marshalling(true)

# --- bulk marshalling ------------------------------------------------------------------------------
## Marshal a whole lane in one crossing. See PuckBody for the arithmetic; the mallets multiply it, because
## there are up to 32 of them and every one a peer predicts replays with the puck.
##
## BOTH LANES GO THROUGH ONE METHOD, dispatched on `lane`. The input entry lives on the CHILD input node while
## the hook is resolved on the body's ROOT, so the state half reads this node's own fields and the input half
## reaches through `input`. Where the value is stored is the game's business; the hook only has to supply it.
func _net_marshal_out(lane: int, values: Array) -> void:
	if lane == NetRollbackHandle.LANE_INPUT:
		values[0] = input.nin_target
		return
	values[0] = net_pos
	values[1] = net_vel

func _net_marshal_in(lane: int, values: Array) -> void:
	if lane == NetRollbackHandle.LANE_INPUT:
		input.nin_target = values[0]
		return
	net_pos = values[0]
	net_vel = values[1]

## Declare the hooks, or take them away. See PuckBody.set_bulk_marshalling().
func set_bulk_marshalling(on: bool) -> void:
	if _handle == null:
		return
	_handle.set_bulk_capture(HockeyConfig.MARSHAL_OUT if on else "")
	_handle.set_bulk_restore(HockeyConfig.MARSHAL_IN if on else "")
	# The apply direction shares the restore method, which is safe only because this body declares no cosmetic
	# entries -- an apply hook reads the CAPTURE slots, and those are the restore slots plus the cosmetics.
	_handle.set_bulk_apply(HockeyConfig.MARSHAL_IN if on else "")
	_handle.process_settings()

## Whether each lane is actually marshalling in bulk, state first. Reported per lane rather than as one
## answer, because the two resolve independently and the input half is the one that can quietly fail.
func uses_bulk_marshalling() -> bool:
	return _handle != null and _handle.uses_bulk_capture(NetRollbackHandle.LANE_STATE)

func uses_bulk_input_marshalling() -> bool:
	return _handle != null and _handle.uses_bulk_capture(NetRollbackHandle.LANE_INPUT)

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
##
## PREDICTION IS RE-DECLARED HERE, and it is the half an authority write does not cover. A client builds its
## rink before the roster arrives, so every mallet registers with `predict = false` -- which does not merely
## defer prediction, it EXEMPTS the mallet from the rollback loop. Without `set_predicted()` the seat this
## player is about to be given would sit out the loop for the whole session: its authoritative rows would
## still land, so the mallet would move and every readout would look ordinary, while the player's own mouse
## took a full round trip to reach it. The PUCK hides this, because it is registered `predict = true` on every
## peer and is the number this demo reports.
func set_owner_peer(peer: int) -> void:
	owner_peer = peer
	if peer > 0:
		# Re-seating puts the mallet back on its home spot rather than wherever the last player left it. Only
		# the authority's write survives; a client's is corrected on the next row, which is exactly right.
		input.nin_target = TableGeometry.home_point(seat)
	if Net.is_offline() or _handle == null:
		return
	_handle.set_input_authority(peer if peer > 0 else 1)
	_handle.set_predicted(Net.is_server() or (peer > 0 and peer == multiplayer.get_unique_id()))

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

## Whether THIS peer simulates this mallet in its rollback loop, as opposed to applying the rows it receives.
##
## Not the same question as [method is_predicting], which asks whether the owner is currently MISPREDICTING.
## This one is the switch, and it is the one that is silent when it is wrong: a mallet left out of the loop
## still applies its authoritative rows, so it moves and every other readout looks ordinary while its own
## player's mouse takes a full round trip to reach it. See [method set_owner_peer].
func is_predicted() -> bool:
	return _handle != null and _handle.is_predicted()

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
