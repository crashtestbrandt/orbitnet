extends Node
class_name RtsNet
## The session layer: bring a session up, seat the players, tear it down. About as small as a real one gets.
##
## THREE ORDERING RULES, and they are the whole reason this file exists rather than the calls being scattered
## through the boot path. Each one is a bug that is invisible until it is not.
##
##   1. THE WORLD IS BUILT BEFORE THE SOCKET BINDS, AND BOUND AFTER THE MODE IS SET. Two halves, because the
##      facade forces it: node paths are what entity ids are derived from, so the graph must exist before any
##      packet can arrive -- but `Net.make_state()` hands back an INERT handle while the facade is OFFLINE,
##      and the facade cannot leave OFFLINE until a peer is assigned. So the build is split
##      (WorldDirector.build then .bind_net_all) and the order is: build the graph, bind the socket, set the
##      mode, register the entities. Collapsing the two halves means registering entities after the socket is
##      already live, which is the window this rule exists to close.
##
##   2. set_net_tick_decoupled(20) IS APPLIED BEFORE Net.set_mode(). set_mode starts the tick loop; changing
##      the tickrate after it has started means the first ticks run at the project default and the clock
##      re-paces underneath them. Set the rate, then start.
##
##   3. set_net_tick_coupled() ON TEARDOWN. The decouple is a session-scoped setting on a process-wide
##      facade. Leave it set and the next offline launch -- or the next hosted session at a different rate --
##      inherits it, which is the kind of state leak that produces "it only happens on the second game".
##
## THE CLIENT BUILDS THE WORLD BEFORE IT KNOWS THE ROSTER. Every seat starts owned by peer 0 (nobody, which
## resolves to the server), and authority is re-pointed by set_seat_owner() when the roster arrives. That is
## what lets rule 1 hold for a CLIENT too -- the alternative, blocking the world build on a welcome packet,
## re-opens exactly the window rule 1 closes.
##
## Deliberately omitted, and listed as known gaps rather than hidden: a build/protocol version handshake (two
## incompatible peers will connect and misbehave rather than being refused with a reason), a join browser,
## invites, and reconnection with seat retention. Each is real work in a shipping session layer and none of it
## teaches anything about the replication lanes.

signal session_state_changed(state: State)
signal local_seat_changed(seat: int)

enum State { OFFLINE, CONNECTING, PLAYING, ERROR }

var world: WorldDirector = null
var roster: SeatRoster = SeatRoster.new()

var _state: State = State.OFFLINE
var _local_seat: int = -1
var _error: String = ""

func _init() -> void:
	# Named at construction: the roster RPC below routes by node path, so this node's name is part of the
	# wire contract and must not change after the node is in the tree.
	name = "RtsNet"

func _ready() -> void:
	# pre_tick fires once per net tick, BEFORE the backend records that tick's input -- so the server's
	# authoritative step lands in the same tick its results are captured and broadcast in. Connecting once
	# here rather than per-session keeps the wiring in one place; it is inert OFFLINE (no loop runs).
	if not Net.pre_tick.is_connected(_on_pre_tick):
		Net.pre_tick.connect(_on_pre_tick)

func state() -> State:
	return _state

func local_seat() -> int:
	return _local_seat

func error_message() -> String:
	return _error

# --- bring-up ------------------------------------------------------------------------------------
## Single player. No peer, no socket; the facade stays OFFLINE and every handle it hands out is inert, so the
## whole game runs on exactly the same code path with no networking spun up at all.
func start_offline() -> void:
	_teardown_world()
	_local_seat = 0
	_build_world()
	# Offline this registers nothing -- every handle comes back inert and the properties simply stick where
	# they are written. Calling it anyway is the point: one code path, whether or not there is a session.
	world.bind_net_all()
	_set_state(State.PLAYING)
	local_seat_changed.emit(_local_seat)
	print("RTS: offline (seat 0, no networking)")

## Listen server: authoritative server AND a local player.
func host_listen(port: int = NetTransport.DEFAULT_PORT) -> bool:
	return _host(port, false)

## Dedicated server: authoritative, no local player. Seats start empty and are handed out as peers connect.
func host_dedicated(port: int = NetTransport.DEFAULT_PORT) -> bool:
	return _host(port, true)

func _host(port: int, dedicated: bool) -> bool:
	_teardown_world()
	# RULE 2 — before set_mode(), which starts the loop.
	Net.set_net_tick_decoupled(RtsConfig.NET_TICK_HZ)
	if not dedicated:
		# The host takes seat 0 under its own peer id before the world is built, so its commander is created
		# already owned rather than being re-authoritied a frame later.
		roster.assign(SeatRoster.SERVER_PEER)
		_local_seat = roster.seat_of_peer(SeatRoster.SERVER_PEER)
	# RULE 1, first half — the node graph exists before anything can send it packets.
	_build_world()

	var peer: MultiplayerPeer = NetTransport.create_server(port, RtsConfig.SEATS)
	if peer == null:
		_fail("could not bind a server peer on port %d" % port)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.SERVER if dedicated else Net.Mode.HOST)
	# RULE 1, second half — now that the facade is out of OFFLINE, the handles it returns are live.
	world.bind_net_all()
	_set_state(State.PLAYING)
	local_seat_changed.emit(_local_seat)
	_broadcast_roster()
	print("RTS: %s on port %d (%s)" % [
		"dedicated" if dedicated else "hosting", port, NetTransport.preferred_kind_name()])
	return true

## Join a server. `address` is whatever the active transport accepts -- an IP:port for ENet, a lobby handle
## for Steam. The demo never learns which, which is the transport factory's whole job.
func join(address: String) -> bool:
	_teardown_world()
	Net.set_net_tick_decoupled(RtsConfig.NET_TICK_HZ)   # RULE 2
	# RULE 1, first half. Seats are owned by nobody until the roster lands; see the header.
	_build_world()

	var peer: MultiplayerPeer = NetTransport.create_client(address)
	if peer == null:
		_fail("could not create a client peer for '%s'" % address)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.CLIENT)
	world.bind_net_all()                                 # RULE 1, second half
	_set_state(State.CONNECTING)
	print("RTS: joining %s (%s)" % [address, NetTransport.preferred_kind_name()])
	return true

## Tear the session down and return the process to a clean OFFLINE state.
func leave() -> void:
	if multiplayer.multiplayer_peer != null:
		multiplayer.multiplayer_peer.close()
		multiplayer.multiplayer_peer = null
	Net.set_mode(Net.Mode.OFFLINE)
	Net.set_net_tick_coupled()   # RULE 3
	roster.clear()
	_local_seat = -1
	_teardown_world()
	_set_state(State.OFFLINE)

# --- the authoritative step ------------------------------------------------------------------------
func _on_pre_tick(_tick: int) -> void:
	if world != null and Net.is_server():
		world.net_step()

# --- peer lifecycle ------------------------------------------------------------------------------
func _connect_peer_signals() -> void:
	if not multiplayer.peer_connected.is_connected(_on_peer_connected):
		multiplayer.peer_connected.connect(_on_peer_connected)
	if not multiplayer.peer_disconnected.is_connected(_on_peer_disconnected):
		multiplayer.peer_disconnected.connect(_on_peer_disconnected)
	if not multiplayer.connected_to_server.is_connected(_on_connected_to_server):
		multiplayer.connected_to_server.connect(_on_connected_to_server)
	if not multiplayer.connection_failed.is_connected(_on_connection_failed):
		multiplayer.connection_failed.connect(_on_connection_failed)
	if not multiplayer.server_disconnected.is_connected(_on_server_disconnected):
		multiplayer.server_disconnected.connect(_on_server_disconnected)

func _on_peer_connected(peer: int) -> void:
	if not Net.is_server():
		return
	var seat: int = roster.assign(peer)
	if seat < 0:
		# Every seat taken. Refusing here is the honest answer; silently admitting a seatless spectator would
		# let them connect, receive the whole world, and have every order rejected with no explanation.
		print("RTS: refusing peer %d -- every seat is taken" % peer)
		multiplayer.multiplayer_peer.disconnect_peer(peer)
		return
	print("RTS: peer %d seated at %d" % [peer, seat])
	_broadcast_roster()

func _on_peer_disconnected(peer: int) -> void:
	if not Net.is_server():
		return
	roster.release(peer)
	print("RTS: peer %d left" % peer)
	_broadcast_roster()

func _on_connected_to_server() -> void:
	_set_state(State.PLAYING)
	print("RTS: connected as peer %d" % multiplayer.get_unique_id())

func _on_connection_failed() -> void:
	_fail("connection failed")

func _on_server_disconnected() -> void:
	# Tear down FIRST, then record the error: leave() ends at State.OFFLINE, so failing before it would have
	# the error state immediately overwritten and the player would be dropped to a menu with no explanation.
	leave()
	_fail("the server closed the session")

# --- roster replication ---------------------------------------------------------------------------
# One reliable broadcast of the whole seat table on every change, rather than per-seat deltas. The table is
# two ints; a delta protocol for it would be more code than the thing it encodes, and a full snapshot is
# self-healing -- a peer that missed one message is corrected by the next.
func _broadcast_roster() -> void:
	if not Net.is_server():
		return
	var owners: PackedInt32Array = _seat_owners()
	_apply_roster(owners)
	rpc(&"_roster_sync", owners)

@rpc("authority", "call_remote", "reliable")
func _roster_sync(owners: PackedInt32Array) -> void:
	_apply_roster(owners)

func _apply_roster(owners: PackedInt32Array) -> void:
	if world == null:
		return
	var my_peer: int = 1 if Net.is_offline() else multiplayer.get_unique_id()
	var found_seat: int = -1
	for seat: int in mini(owners.size(), RtsConfig.SEATS):
		world.set_seat_owner(seat, owners[seat])
		if owners[seat] == my_peer:
			found_seat = seat
	if found_seat != _local_seat:
		_local_seat = found_seat
		local_seat_changed.emit(_local_seat)

func _seat_owners() -> PackedInt32Array:
	var owners: PackedInt32Array = PackedInt32Array()
	owners.resize(RtsConfig.SEATS)
	for seat: int in RtsConfig.SEATS:
		var peer: int = roster.peer_of_seat(seat)
		owners[seat] = peer if peer > 0 else 0
	return owners

# --- world lifecycle ------------------------------------------------------------------------------
func _build_world() -> void:
	world = WorldDirector.new()
	# A child of the SCENE ROOT, not of this node: NetCommand routes by node path, so the world's path must be
	# identical on every peer, and hanging it off a fixed, scene-defined parent is the simplest way to
	# guarantee that.
	get_parent().add_child(world)
	world.build(roster, _seat_owners())

func _teardown_world() -> void:
	if world != null:
		world.queue_free()
		world = null

# --- state ---------------------------------------------------------------------------------------
func _set_state(next: State) -> void:
	if next == _state:
		return
	_state = next
	session_state_changed.emit(next)

func _fail(reason: String) -> void:
	_error = reason
	push_warning("RTS session error: %s" % reason)
	_set_state(State.ERROR)
