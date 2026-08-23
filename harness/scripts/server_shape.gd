extends Node
## The SERVER-SHAPE scenario: one server-authoritative state channel, watched from a joining client, against
## each of the two server shapes.
##
## THE ASYMMETRY THIS EXISTS TO MEASURE. A listen server simulates a body of its own and is also a peer; a
## dedicated server holds none. Every send-path pass that walks "every entity" therefore walks a different set
## on each shape, and nothing in this repository ran either shape end to end before this file. The scenario is
## the same on both runs -- same node paths, same channels, same properties, same assertions -- so the only
## variable is what the server itself holds.
##
## Two roles, one script, launched as two processes by `tools/server-shape-probe.sh`:
##
##   --role=server --shape=dedicated   authoritative, no local player. Every seat is handed to a joiner.
##   --role=server --shape=listen      authoritative AND seat 0's local player.
##   --role=client --address=ADDR      joins, watches, and prints the readings.
##   --veto-own-status                 server-side negative control -- see `_veto_own_status`.
##
## WHAT THE CLIENT ASSERTS, AND WHY IT IS THE STATE LANE. Each seat body carries a `Status` child on the STATE
## lane -- server-authored values pushed each tick, no prediction. The client asserts that `last_known_state()`
## on **its own** seat's channel RISES, and reports the other seat's alongside it. A rising tick means encode,
## datagram and decode all ran for that channel; a flat one means no authoritative row arrived for the whole
## run.
##
## READING THE NUMBER BEFORE BELIEVING IT. `NetStateHandle.last_known_state()` FAILS OPEN: against a cdylib
## with no `get_last_known_state` it answers `Net.current_tick()`, which rises on every peer whether or not a
## row ever arrived. So the client prints `reports=` from `NetStateHandle.reports_last_known_state()` FIRST and
## the driver refuses a run where it is 0 -- otherwise the assertion is measuring the fallback. The rollback
## lane's `get_last_known_state()` does not fail open, so it is reported beside the state lane as a
## second, independent reading.
##
## THE ROLLBACK LANE IS HERE FOR THE SERVER'S SAKE, and it predicts on the client too. A seated peer's body
## is what gives the server an OWNED row -- the row the send path reads a peer's interest centre off, and the
## thing a listen server has one more of than a dedicated one. The client also predicts the seat it is given,
## which takes one non-obvious pair of calls (see `_bind_channels` and `_apply_owners`) because a client binds
## before it knows its seat. Both peers COUNT their rollback ticks and assert on the count: a client that
## silently sits out the loop still receives and applies every row, so no other reading in this file would
## notice.
##
## THE INTEREST PASS IS ON, DELIBERATELY. `aoi_radius` is set on the server, which is what makes the send path
## run its per-peer interest and priority passes at all -- the passes the two shapes disagree about. The radius
## is orders of magnitude larger than anything this world puts on the wire, so it culls nothing; the server
## prints `culled=` so a run that did cull is visible rather than being read as a starve.
##
## THE NEGATIVE CONTROL IS PART OF THE SCENARIO, not a thing to reach for when it fails. A run that can only
## ever pass proves nothing, so `--veto-own-status` makes the server withhold each joiner's own status channel
## and the driver asserts the client REPORTS THAT. Without it, "both shapes delivered rows" and "the assertion
## cannot see a channel that delivered none" are the same output.
##
## COUPLED, and no `--fixed-fps`. The harness project runs the net tick at the physics rate (`sync_to_physics`),
## which is the configuration the RTS probe does not cover. `--fixed-fps` would stall the clock sync's
## ping/pong, so the client would never finish its handshake and the run would time out looking like a netcode
## failure.

## Seats in the scenario world. Both exist on both shapes and on both peers, so the node paths -- and therefore
## the entity ids -- are identical everywhere. Only who OWNS them differs.
const SEATS: int = 2

## Metres of interest radius. Nothing in this world gets more than a few metres from the origin, so this culls
## nothing; it is set only to make the per-peer interest pass run.
const _AOI_RADIUS_M: float = 500.0
## Metres of band scale. Sized with the radius for the same reason -- one band, no reordering, the pass runs.
const _AOI_BAND_M: float = 250.0

## Where each peer stops sampling and prints its verdict, in seconds after the session starts. A coupled 60 Hz
## session runs a couple of hundred ticks in that time even on a loaded CI runner, against a threshold of eight
## -- the window is sized for the handshake and the clock sync to settle, not for the count.
const _REPORT_AFTER_S: float = 4.0
## The fewest forward moves of the state-lane tick that count as "rising". A single move could be one row that
## arrived by luck; this is a rate, over seconds.
const _MIN_RISES: int = 8

## The input carrier for a seat body. Its multiplayer authority is the OWNING peer's while the body's own stays
## with the server -- the server-authoritative split -- which is why input has to live on its own node.
class SeatInput extends Node:
	var nin_move: Vector3 = Vector3.ZERO

## One seat's body. The rollback lane, so a seated peer contributes an OWNED row: that row is what the send
## path reads a peer's interest centre off, and holding one is the whole of what a listen server has and a
## dedicated server does not.
class SeatBody extends Node3D:
	var sim_pos: Vector3 = Vector3.ZERO
	var input: SeatInput = null
	## How many times the backend has run this body's rollback tick on THIS peer. Counted rather than
	## assumed: whether a body joins the loop is `!exempt and (state_local or (input_local and
	## enable_prediction))` inside the backend, and none of those three are readable from here.
	var sims: int = 0

	## The rollback tick, run by the backend on the server for every body and on the owning client for its own.
	## One line of validation, which is all a scenario body needs: the authoritative position is the requested
	## one, clamped to the arena. It matters that there IS one -- a body with no `_rollback_tick` is not in the
	## backend's call list, so its lane never simulates and the shape difference this file measures would be
	## between two sets of inert entities.
	func _rollback_tick(_delta: float, _tick: int, _is_fresh: bool) -> void:
		sims += 1
		sim_pos = input.nin_move.clamp(Vector3(-8.0, 0.0, -8.0), Vector3(8.0, 0.0, 8.0))
		position = sim_pos

## One seat's server-authoritative snapshot channel: the thing under test. A separate node from the body so it
## is a STATE-lane entity of its own -- the lane a game puts health, loadout and status on, and the lane the
## reported symptom is about.
class SeatStatus extends Node3D:
	var status_value: int = 0

var _role: String = "server"
var _shape: String = "dedicated"
var _port: int = NetTransport.DEFAULT_PORT
var _address: String = "127.0.0.1"
## Seconds before this process exits. 0 leaves it running -- for a human watching one shape by hand.
var _run_seconds: float = 0.0
## SERVER-SIDE NEGATIVE CONTROL: withhold each joining peer's OWN status channel from it, with the per-peer
## visibility veto. The veto stops the rows and nothing else -- no despawn, the node stays, the entity id stays
## session-global -- which is precisely the symptom this scenario exists to detect. Running it proves the
## assertion is not vacuous: without it, a scenario that can only ever pass is indistinguishable from one that
## works.
var _veto_own_status: bool = false

var _world: Node = null
var _bodies: Array[SeatBody] = []
var _status: Array[SeatStatus] = []
var _body_handles: Array[NetRollbackHandle] = []
var _status_handles: Array[NetStateHandle] = []
## Seat -> owning peer id, 0 for unclaimed. Broadcast whole on every change (two ints; a delta protocol for it
## would be larger than the thing it encodes).
var _owners: PackedInt32Array = PackedInt32Array()

## Remote peers this server seated. Counted rather than inferred from `_owners`, because the listen shape
## seats ITSELF at boot -- so "a seat is taken" is true there before any client has connected, and a server
## that never saw a joiner would report PASS on its own occupancy.
var _seated_peers: int = 0

var _playing: bool = false
var _elapsed: float = 0.0
var _reported: bool = false
var _local_seat: int = -1

# The client's readings, per seat.
var _first_state: PackedInt32Array = PackedInt32Array()
var _last_state: PackedInt32Array = PackedInt32Array()
var _rises: PackedInt32Array = PackedInt32Array()

func _ready() -> void:
	# Named at construction: the roster RPC below routes by node path, so this node's name is part of the wire
	# contract and must match on both peers.
	name = "ServerShape"
	process_mode = Node.PROCESS_MODE_ALWAYS
	_parse_args()

	_owners.resize(SEATS)
	_first_state.resize(SEATS)
	_last_state.resize(SEATS)
	_rises.resize(SEATS)
	for seat: int in SEATS:
		_owners[seat] = 0
		_first_state[seat] = -2      # -2 = never sampled, so a real -1 reading is distinguishable
		_last_state[seat] = -2
		_rises[seat] = 0

	print("SHAPE-BOOT role=%s shape=%s godot=%s transport=%s" % [
		_role, _shape, Engine.get_version_info().get("string", "?"), NetTransport.preferred_kind_name()])

	# The listen host claims seat 0 BEFORE the world is built, so its body is created already owned rather than
	# being re-authoritied a frame later. A dedicated server claims nothing -- that is the shape.
	if _role == "server" and _shape == "listen":
		_owners[0] = 1

	# The build is split around the socket for the reason the RTS session layer states: entity ids are derived
	# from node paths, so the graph must exist before a packet can arrive -- but the facade hands back INERT
	# handles while it is OFFLINE, so the channels cannot be registered until a peer is assigned and the mode
	# is set. Build the graph, bind the socket, set the mode, then register.
	_build_world()
	if not _start_session():
		_finish(false, "the session did not start")
		# QUIT HERE rather than falling through to the `--run` timer below, which this return would skip.
		# A bind-port conflict would otherwise idle until the driver's watchdog fires, turning an instant
		# failure into a 60-second one on every affected run.
		get_tree().quit(1)
		return
	_bind_channels()
	_apply_owners()

	if not Net.pre_tick.is_connected(_on_pre_tick):
		Net.pre_tick.connect(_on_pre_tick)
	if not Net.peer_joined.is_connected(_on_peer_joined):
		Net.peer_joined.connect(_on_peer_joined)
	if not multiplayer.connected_to_server.is_connected(_on_connected):
		multiplayer.connected_to_server.connect(_on_connected)
	if not multiplayer.connection_failed.is_connected(_on_connection_failed):
		multiplayer.connection_failed.connect(_on_connection_failed)

	if _role == "server":
		_playing = true
		_broadcast_owners()

	if _run_seconds > 0.0:
		get_tree().create_timer(_run_seconds).timeout.connect(_quit)

## The run's own exit. A process that reported a verdict and then sat there would hold the port against the
## next shape's server, so the driver would read the previous run's bind failure as this run's.
func _quit() -> void:
	if not _reported:
		_finish(false, "the run ended before a verdict")
	get_tree().quit(0)

# --- the world -------------------------------------------------------------------------------------
## The same graph on every peer, with every name written out. Godot's auto-names are allocation-order
## dependent, and an entity id is an FNV hash of a node path: two peers that name a node differently agree
## about nothing, and nothing errors while they do.
func _build_world() -> void:
	_world = Node.new()
	_world.name = "World"
	add_child(_world)
	for seat: int in SEATS:
		var body: SeatBody = SeatBody.new()
		body.name = "Seat%d" % seat
		_world.add_child(body)
		var input: SeatInput = SeatInput.new()
		input.name = "Input"
		body.add_child(input)
		body.input = input
		var status: SeatStatus = SeatStatus.new()
		status.name = "Status"
		body.add_child(status)
		_bodies.push_back(body)
		_status.push_back(status)

## The second half of the build: register both lanes now that the facade is out of OFFLINE.
func _bind_channels() -> void:
	for seat: int in SEATS:
		var body: SeatBody = _bodies[seat]
		var status: SeatStatus = _status[seat]
		# The body's own authority stays with the server (peer 1, the default); the input node's moves to the
		# owner. Set before registration -- the backend reads the authority when it processes settings.
		var owner_peer: int = _owners[seat] if _owners[seat] > 0 else 1
		body.input.set_multiplayer_authority(owner_peer)
		# `predict` UNCONDITIONALLY, and this is the subtle part of the file.
		#
		# The obvious form is `Net.is_server() or owner_peer == multiplayer.get_unique_id()`. On a CLIENT
		# that is false for every seat, because this runs from _ready() and the roster has not arrived --
		# `_owners` is still all zeros, so every `owner_peer` reads 1. And `predict = false` does not merely
		# defer prediction, it EXEMPTS the body: net.gd sets `enable_prediction` here and nowhere else, and
		# `set_input_authority()` re-resolves only who owns the lanes. So the seat the client is about to be
		# given would sit out the rollback loop for the whole run, silently -- its received rows still land,
		# so every reading in this file would look normal.
		#
		# Passing `true` is safe for a seat this peer does not own, because the backend gates simulation on
		# `!exempt and (owns_state or (owns_input and enable_prediction))`: a client owns neither lane of a
		# body it was not given, so it still does not simulate. What `true` buys is that the moment
		# `set_input_authority()` points the input here, `owns_input` flips and the body starts predicting --
		# no re-registration. `_apply_owners()` re-establishes the display exemption; see there.
		_body_handles.push_back(Net.register_rollback_body(
			body, body.input, ["sim_pos"], ["nin_move"], true))

		var handle: NetStateHandle = Net.make_state(status)
		handle.add_state(status, "status_value")
		# The channel's world-space anchor. It is what makes the channel cullable, and therefore what puts it
		# through the per-peer interest pass rather than past it as always-relevant.
		handle.set_anchor("global_position")
		handle.process_settings()
		_status_handles.push_back(handle)

	if Net.is_server():
		# Server-side only, and the reason both are set: `aoi_radius` is what turns the interest pass on at
		# all, and the band scale is a separate number because it decides send ORDER rather than membership.
		Net.set_aoi_radius(_AOI_RADIUS_M)
		Net.set_aoi_band_radius(_AOI_BAND_M)

# --- session bring-up ------------------------------------------------------------------------------
func _start_session() -> bool:
	if _role == "client":
		var client: MultiplayerPeer = NetTransport.create_client(_address, _port)
		if client == null:
			printerr("SHAPE-FAIL could not create a client peer for '%s:%d'" % [_address, _port])
			return false
		multiplayer.multiplayer_peer = client
		Net.set_mode(Net.Mode.CLIENT)
		return true

	var server: MultiplayerPeer = NetTransport.create_server(_port, SEATS)
	if server == null:
		printerr("SHAPE-FAIL could not bind a server peer on port %d" % _port)
		return false
	multiplayer.multiplayer_peer = server
	Net.set_mode(Net.Mode.SERVER if _shape == "dedicated" else Net.Mode.HOST)
	return true

## A peer finished the OrbitNet handshake. Seated here rather than on the transport's `peer_connected`, which
## fires before any identity is known.
func _on_peer_joined(peer: int, _session_id: int, _resumed_from: int) -> void:
	if not Net.is_server():
		return
	for seat: int in SEATS:
		if _owners[seat] == 0:
			_owners[seat] = peer
			_seated_peers += 1
			print("SHAPE-SEAT role=server shape=%s peer=%d seat=%d" % [_shape, peer, seat])
			if _veto_own_status:
				Net.set_entity_hidden(peer, _status_handles[seat].entity_id(), true)
				print("SHAPE-VETO role=server shape=%s peer=%d seat=%d hidden=%s" % [
					_shape, peer, seat,
					"1" if Net.is_entity_hidden(peer, _status_handles[seat].entity_id()) else "0"])
			_broadcast_owners()
			return
	print("SHAPE-SEAT role=server shape=%s peer=%d seat=-1 (every seat taken)" % [_shape, peer])

func _on_connected() -> void:
	_playing = true
	print("SHAPE-READY role=client shape=%s peer=%d" % [_shape, multiplayer.get_unique_id()])

func _on_connection_failed() -> void:
	_finish(false, "the connection failed")

# --- seat ownership --------------------------------------------------------------------------------
func _broadcast_owners() -> void:
	if not Net.is_server():
		return
	_apply_owners()
	rpc(&"_owners_sync", _owners)

@rpc("authority", "call_remote", "reliable")
func _owners_sync(owners: PackedInt32Array) -> void:
	for seat: int in mini(owners.size(), SEATS):
		_owners[seat] = owners[seat]
	_apply_owners()

## Point every body's input at its seat's owner, on EVERY peer. Multiplayer authority is a property of a node
## on the peer that holds it and nothing here replicates, so a peer that skipped this disagrees about who owns
## the body -- and on the server that disagreement is what starts refusing the owner's input rows.
func _apply_owners() -> void:
	var me: int = multiplayer.get_unique_id()
	var found: int = -1
	for seat: int in SEATS:
		if _body_handles.size() > seat:
			_body_handles[seat].set_input_authority(_owners[seat] if _owners[seat] > 0 else 1)
		if _owners[seat] == me:
			found = seat
	# THE OTHER HALF OF THE `predict = true` ABOVE, and it must run AFTER the authority calls.
	#
	# Registering every body as predicting leaves each one un-exempt, and an un-exempt body owning neither
	# lane is what `net.remote_resim` turns on -- remote prediction, which this scenario does not want and
	# which is off by default. Re-asserting the lever walks the bodies and exempts exactly those owning
	# neither state nor input, which after the authority calls above is precisely the seats this peer was
	# not given. The seat it WAS given owns its input and is left alone, so it predicts.
	#
	# Client-side only: a server owns every body's state, so the lever finds nothing to exempt there.
	if not Net.is_server():
		Net.set_remote_resim(false)
	if found != _local_seat:
		_local_seat = found
		print("SHAPE-SEAT role=%s shape=%s seat=%d" % [_role, _shape, _local_seat])

# --- the authoritative step ------------------------------------------------------------------------
## The server writes each channel's row once per tick, before the backend records the tick. A value that moves
## every tick is what makes "no row arrived" distinguishable from "the row arrived and said nothing new".
func _on_pre_tick(tick: int) -> void:
	if not Net.is_server():
		return
	for seat: int in SEATS:
		_status[seat].status_value = tick * (seat + 1)
	# The owning peer authors its own body's input; on the listen shape the server owns seat 0 and does so
	# here. A body whose input lane is silent coasts on the neutral row, which is a different measurement.
	_write_local_input()

func _write_local_input() -> void:
	if _local_seat < 0:
		return
	var phase: float = Net.current_time()
	_bodies[_local_seat].input.nin_move = Vector3(sin(phase), 0.0, cos(phase)) * 4.0

# --- sampling + the verdict ------------------------------------------------------------------------
func _process(delta: float) -> void:
	if not _playing or _reported:
		return
	_elapsed += delta
	if _role == "client":
		_sample()
		_write_local_input()
	if _elapsed >= _REPORT_AFTER_S:
		_report()

## One sample per frame of every seat's state-lane tick. Counted as RISES rather than compared end to end: an
## end-to-end delta cannot tell one late row from a channel that was live throughout, and a channel that
## delivered exactly one row is the failure this scenario is about.
func _sample() -> void:
	for seat: int in SEATS:
		var now: int = _status_handles[seat].last_known_state()
		if _first_state[seat] == -2:
			_first_state[seat] = now
		elif now > _last_state[seat]:
			_rises[seat] += 1
		_last_state[seat] = now

func _report() -> void:
	if _role == "server":
		var bw: Dictionary[String, float] = Net.bandwidth_metrics()
		print("SHAPE-BW role=server shape=%s peers=%.0f interest_entities=%.0f admitted=%.2f deferred=%.2f culled=%.2f" % [
			_shape, bw["peers"], bw["interest_entities"],
			bw["blocks_admitted_s"], bw["blocks_deferred_s"], bw["blocks_culled_s"]])
		print("SHAPE-SIM role=server shape=%s seat0_sims=%d seat1_sims=%d" % [
			_shape, _bodies[0].sims, _bodies[1].sims])
		print("SHAPE-OWN role=server shape=%s seat0_owner=%d seat1_owner=%d local_seat=%d seated_peers=%d" % [
			_shape, _owners[0], _owners[1], _local_seat, _seated_peers])
		if _seated_peers <= 0:
			_finish(false, "no remote peer was ever seated")
			return
		# The server simulates EVERY body on both shapes -- that is what "authoritative" means here, and it
		# is the premise the whole comparison rests on. Asserted so the scenario cannot quietly degrade into
		# two sets of inert entities and still report a difference between the shapes.
		if _bodies[0].sims <= 0 or _bodies[1].sims <= 0:
			_finish(false, "the server ran %d and %d rollback ticks for its two seats -- it is not "
				% [_bodies[0].sims, _bodies[1].sims] + "simulating the world it is authoritative for")
			return
		_finish(true, "")
		return

	# THE BRANCH, PRINTED BEFORE THE READING. Everything below is the fail-open fallback when this is 0.
	print("SHAPE-BRANCH role=client shape=%s seat0_reports=%d seat1_reports=%d" % [
		_shape,
		1 if _status_handles[0].reports_last_known_state() else 0,
		1 if _status_handles[1].reports_last_known_state() else 0])
	var own: int = _local_seat
	var other: int = -1 if own < 0 else (own + 1) % SEATS
	print("SHAPE-STATE role=client shape=%s seat=%d own_first=%d own_last=%d own_rises=%d other_first=%d other_last=%d other_rises=%d" % [
		_shape, own,
		_reading(_first_state, own), _reading(_last_state, own), _reading(_rises, own),
		_reading(_first_state, other), _reading(_last_state, other), _reading(_rises, other)])
	print("SHAPE-BODY role=client shape=%s own_last=%d other_last=%d tick=%d" % [
		_shape,
		_body_last_state(own), _body_last_state(other), Net.current_tick()])
	print("SHAPE-SIM role=client shape=%s own_sims=%d other_sims=%d" % [
		_shape, _sims(own), _sims(other)])

	if own < 0:
		_finish(false, "this client was never seated")
		return
	if not _status_handles[own].reports_last_known_state():
		_finish(false, "the loaded backend cannot answer last_known_state -- the reading is the fallback")
		return
	if _rises[own] < _MIN_RISES:
		_finish(false, "the client's OWN seat-%d state channel advanced %d times in %.0fs" % [
			own, _rises[own], _elapsed])
		return
	# The rollback lane's own health, asserted rather than assumed. A client that does not simulate its own
	# body still receives and applies every row, so nothing above this line would notice -- which is exactly
	# how the seat this peer is given can sit out the whole rollback loop unremarked.
	if _sims(own) <= 0:
		_finish(false, "the client's OWN seat-%d body never ran a rollback tick -- it is exempt from the "
			% own + "loop, so this run exercised no owner prediction")
		return
	if other >= 0 and _sims(other) > 0:
		_finish(false, "the client simulated seat %d, which it does not own -- remote prediction is on and "
			% other + "this run is not the display-only shape it reports")
		return
	_finish(true, "")

func _reading(values: PackedInt32Array, seat: int) -> int:
	return -3 if seat < 0 else values[seat]

func _sims(seat: int) -> int:
	return -3 if seat < 0 else _bodies[seat].sims

func _body_last_state(seat: int) -> int:
	return -3 if seat < 0 else _body_handles[seat].get_last_known_state()

func _finish(passed: bool, reason: String) -> void:
	_reported = true
	if not passed and not reason.is_empty():
		printerr("SHAPE-FAIL role=%s shape=%s %s" % [_role, _shape, reason])
	print("SHAPE-RESULT role=%s shape=%s %s" % [_role, _shape, "PASS" if passed else "FAIL"])

# --- the command line ------------------------------------------------------------------------------
func _parse_args() -> void:
	_role = _flag("--role=", _role)
	_shape = _flag("--shape=", _shape)
	_address = _flag("--address=", _address)
	var port: String = _flag("--port=", "")
	if port.is_valid_int():
		_port = port.to_int()
	var run: String = _flag("--run=", "")
	if run.is_valid_float():
		_run_seconds = run.to_float()
	_veto_own_status = _has_flag("--veto-own-status")

func _has_flag(name: String) -> bool:
	return OS.get_cmdline_user_args().has(name)

func _flag(prefix: String, fallback: String) -> String:
	for arg: String in OS.get_cmdline_user_args():
		if arg.begins_with(prefix):
			return arg.substr(prefix.length())
	return fallback
