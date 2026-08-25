extends Node
class_name HockeyNet
## The session layer: bring a session up, seat the players, tear it down.
##
## THREE ORDERING RULES, and they are the whole reason this file exists rather than the calls being scattered
## through the boot path. Each one is a bug that is invisible until it is not.
##
##   1. THE WORLD IS BUILT BEFORE THE SOCKET BINDS, AND BOUND AFTER THE MODE IS SET. Two halves, because the
##      facade forces it: node paths are what entity ids are derived from, so the graph must exist before any
##      packet can arrive -- but `Net.make_state()` hands back an INERT handle while the facade is OFFLINE, and
##      the facade cannot leave OFFLINE until a peer is assigned. So the build is split (RinkDirector.build
##      then .bind_net_all) and the order is: build the graph, bind the socket, set the mode, register the
##      entities.
##
##   2. set_remote_resim(true) IS APPLIED BEFORE bind_net_all(). It is a per-entity exemption the backend
##      applies at REGISTRATION from the facade's stored flag, and `set_remote_resim` also sweeps the entities
##      that already exist -- so setting it first makes both paths agree. Setting it afterward would leave any
##      body registered in between exempt, which for this demo means the puck stops being predicted and the
##      whole point of it goes quiet with nothing erroring.
##
##   3. THE TEARDOWN PUTS IT BACK. remote_resim is a session-scoped setting on a process-wide facade. Leave it
##      set and the next offline launch inherits it, which is the kind of state leak that produces "it only
##      happens on the second game".
##
## NO set_net_tick_decoupled() ANYWHERE, and its absence is deliberate rather than an omission. This demo runs
## COUPLED: one net tick per physics frame at 60 Hz, configured in project.godot, with the clock pinned to a
## stretch of exactly 1.0 and error absorbed by whole-tick lead adjustments. The RTS demo's rule about setting
## the rate before the loop starts has nothing to apply to here.
##
## THE CLIENT BUILDS THE WORLD BEFORE IT KNOWS THE ROSTER. Every seat starts owned by peer 0 (nobody), and
## authority is re-pointed by set_seat_owner() when the roster arrives. That is what lets rule 1 hold for a
## CLIENT too -- the alternative, blocking the world build on a welcome packet, re-opens exactly the window
## rule 1 closes.
##
## DROP-IN AND DROP-OUT ARE THE SAME MECHANISM. A peer joining takes the lowest free seat on the thinner end; a
## peer leaving gives it up; the whole seat table is rebroadcast either way. No spawn, no despawn -- the mallet
## pool is static and only its ownership moves.
##
##   4. PLAYERS ARE SEATED ON Net.peer_joined, NOT ON multiplayer.peer_connected. The transport signal fires
##      when the socket comes up, which is BEFORE the OrbitNet handshake -- so the peer's SESSION IDENTITY is
##      not known yet, and identity is the only thing that can tell a returning player from a newcomer.
##      Seating on the transport signal is what makes a rejoiner land in whatever seat happens to be free,
##      which on this table means coming back onto the other team.
##
## RECONNECTION, AND THE RULE THIS DEMO TAKES. A dropped peer's seat is HELD for `Net.reconnect_grace()`
## seconds: the roster stops naming the peer and starts naming its IDENTITY, so the seat is taken but empty and
## the backend holds the mallet on the neutral input row with its state still broadcasting. It comes to rest
## where it was rather than freezing and then jumping when its owner returns.
##
## THE CONSERVATIVE RULE, AND WHY THIS DEMO TAKES IT WHERE THE RTS DEMO DOES NOT. `resumed_from` names a
## connection that MAY STILL BE UP, and a session identity is client-asserted and unauthenticated -- so a peer
## presenting an identity it watched someone else use takes that player's seat, and the original keeps its
## connection with no error. This demo therefore honors a reclaim only for an identity it already saw
## `Net.peer_dropped` report with `held = true`. The price is the one the facade names: a player whose old
## socket the transport has not yet noticed is gone comes back as a newcomer. On a 32-seat table that costs
## them their end of the rink, which is cheap; on the RTS's two-seat table it would cost them the game, which
## is why that demo takes the permissive rule and says so.
##
## Deliberately omitted, and listed as known gaps rather than hidden: a build/protocol version handshake (two
## incompatible peers will connect and misbehave rather than being refused with a reason), a join browser, and
## invites.

signal session_state_changed(state: State)
signal local_seat_changed(seat: int)

enum State { OFFLINE, CONNECTING, PLAYING, ERROR }

var rink: RinkDirector = null
var roster: TeamRoster = TeamRoster.new()

var _state: State = State.OFFLINE
var _local_seat: int = -1
var _error: String = ""
## SERVER-SIDE: the identities this layer has SEEN drop with their session held. The conservative rule reads
## exactly this set, and a reclaim by an identity that is not in it is refused -- which is what makes a forged
## identity worth no more than a fresh seat.
var _held_sessions: Dictionary[int, bool] = {}

func _init() -> void:
	# Named at construction: the roster RPC below routes by node path, so this node's name is part of the wire
	# contract and must not change after the node is in the tree.
	name = "HockeyNet"

func _ready() -> void:
	# The session-lifecycle signals, connected ONCE rather than per session: they are inert until a session is
	# hosted, and only ever fire on the authority. Connecting them here also means a listen host and a
	# dedicated server take exactly the same path, which is the property RULE 4 is protecting.
	if not Net.peer_joined.is_connected(_on_net_peer_joined):
		Net.peer_joined.connect(_on_net_peer_joined)
	if not Net.peer_dropped.is_connected(_on_net_peer_dropped):
		Net.peer_dropped.connect(_on_net_peer_dropped)
	if not Net.peer_session_expired.is_connected(_on_net_session_expired):
		Net.peer_session_expired.connect(_on_net_session_expired)

func state() -> State:
	return _state

func local_seat() -> int:
	return _local_seat

func error_message() -> String:
	return _error

# --- bring-up --------------------------------------------------------------------------------------
## Single player. No peer, no socket; the facade stays OFFLINE and every handle it hands out is inert, so the
## whole game runs on exactly the same code path with no networking spun up at all.
func start_offline() -> void:
	_teardown_rink()
	_local_seat = 0
	roster.assign(TeamRoster.SERVER_PEER)
	_build_rink()
	# Offline this registers nothing -- every handle comes back inert and the properties simply stick where
	# they are written. Calling it anyway is the point: one code path, whether or not there is a session.
	rink.bind_net_all()
	_set_state(State.PLAYING)
	local_seat_changed.emit(_local_seat)
	print("HOCKEY: offline (seat 0, no networking)")

## Listen server: authoritative server AND a local player.
func host_listen(port: int = NetTransport.DEFAULT_PORT) -> bool:
	return _host(port, false)

## Dedicated server: authoritative, no local player. Seats are handed out as peers connect.
func host_dedicated(port: int = NetTransport.DEFAULT_PORT) -> bool:
	return _host(port, true)

func _host(port: int, dedicated: bool) -> bool:
	_teardown_rink()
	if not dedicated:
		# The host takes its seat under its own peer id before the world is built, so its mallet is created
		# already owned rather than being re-authoritied a frame later.
		roster.assign(TeamRoster.SERVER_PEER)
		_local_seat = roster.seat_of_peer(TeamRoster.SERVER_PEER)
	# RULE 1, first half -- the node graph exists before anything can send it packets.
	_build_rink()

	# The peer cap is the seat pool. Past it ENet refuses the connection, which is the honest answer: a peer
	# with no mallet would receive the whole table and have every serve refused with no explanation.
	var peer: MultiplayerPeer = NetTransport.create_server(port, HockeyConfig.SEATS)
	if peer == null:
		_fail("could not bind a server peer on port %d" % port)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.SERVER if dedicated else Net.Mode.HOST)
	Net.set_remote_resim(true)   # RULE 2 -- before the entities register
	# RULE 1, second half -- now that the facade is out of OFFLINE, the handles it returns are live.
	rink.bind_net_all()
	_set_state(State.PLAYING)
	local_seat_changed.emit(_local_seat)
	_broadcast_roster()
	print("HOCKEY: %s on port %d (%s)" % [
		"dedicated" if dedicated else "hosting", port, NetTransport.preferred_kind_name()])
	return true

## Join a server. `target` is whatever the active transport accepts -- `ADDR` or `ADDR:PORT` for ENet, a lobby
## handle for Steam. The demo never learns which, which is the transport factory's whole job.
func join(target: String) -> bool:
	_teardown_rink()
	# RULE 1, first half. Seats are owned by nobody until the roster lands; see the header.
	_build_rink()

	var peer: MultiplayerPeer = NetTransport.create_client(target_address(target), target_port(target))
	if peer == null:
		_fail("could not create a client peer for '%s'" % target)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.CLIENT)
	Net.set_remote_resim(true)   # RULE 2
	rink.bind_net_all()          # RULE 1, second half
	_set_state(State.CONNECTING)
	print("HOCKEY: joining %s:%d (%s)" % [
		target_address(target), target_port(target), NetTransport.preferred_kind_name()])
	return true

# --- join targets ----------------------------------------------------------------------------------
# `ADDR` or `ADDR:PORT`, split here rather than by the caller.
#
# The host recipe takes a port, so without this a session hosted on anything but the default was unreachable:
# `NetTransport.create_client` takes the port as its own argument and would have handed ENet the whole
# "1.2.3.4:47900" string as a hostname to resolve. The flag's own documentation promised the suffix worked.
#
# A Steam target is a 64-bit Steam ID and carries no colon, so it falls through unchanged -- the demo still
# never learns which transport it is talking to.

## The address half of a join target.
static func target_address(target: String) -> String:
	var separator: int = _port_separator(target)
	return target if separator < 0 else target.substr(0, separator)

## The port half of a join target, or the transport's default when it carries none.
static func target_port(target: String) -> int:
	var separator: int = _port_separator(target)
	if separator < 0:
		return NetTransport.DEFAULT_PORT
	return clampi(target.substr(separator + 1).to_int(), 1, 65535)

# The index of the ':' that introduces a port, or -1. ONE rule, so the two accessors above can never disagree
# about where the split is and hand ENet an address and a port that came from different readings of the string.
static func _port_separator(target: String) -> int:
	var separator: int = target.rfind(":")
	if separator <= 0 or separator >= target.length() - 1:
		return -1
	if not target.substr(separator + 1).is_valid_int():
		return -1
	return separator

## Tear the session down and return the process to a clean OFFLINE state.
func leave() -> void:
	if multiplayer.multiplayer_peer != null:
		multiplayer.multiplayer_peer.close()
		multiplayer.multiplayer_peer = null
	Net.set_mode(Net.Mode.OFFLINE)
	Net.set_remote_resim(false)   # RULE 3 -- back to the facade's own default
	roster.clear()
	# The identities this layer watched leave describe a session that no longer exists. Carried into the next
	# one they would be a set of reclaims nobody in it ever earned.
	_held_sessions.clear()
	_local_seat = -1
	_teardown_rink()
	_set_state(State.OFFLINE)

# --- peer lifecycle --------------------------------------------------------------------------------
## The CLIENT-side transport signals only. The server's join and leave both come from `Net` -- see RULE 4: the
## transport's peer_connected fires before any identity is known, and its peer_disconnected cannot say whether
## the session is being held open.
func _connect_peer_signals() -> void:
	if not multiplayer.connected_to_server.is_connected(_on_connected_to_server):
		multiplayer.connected_to_server.connect(_on_connected_to_server)
	if not multiplayer.connection_failed.is_connected(_on_connection_failed):
		multiplayer.connection_failed.connect(_on_connection_failed)
	if not multiplayer.server_disconnected.is_connected(_on_server_disconnected):
		multiplayer.server_disconnected.connect(_on_server_disconnected)

## The seat assignment, on the OrbitNet handshake rather than the transport connect -- RULE 4. `session_id` is
## the identity this peer's handshake carried, and `resumed_from` is the peer id it held before it dropped, or
## 0 for a first-time joiner.
func _on_net_peer_joined(peer: int, session_id: int, resumed_from: int) -> void:
	if not Net.is_server():
		return
	# THE CONSERVATIVE RULE, and it is one line: an identity is worth a seat back only if this layer watched
	# that identity leave. `resumed_from` alone is the backend saying "somebody claimed this before", which a
	# forger can also make true.
	var reclaim: int = session_id if _held_sessions.has(session_id) else TeamRoster.NO_SESSION
	var seat: int = roster.assign(peer, reclaim)
	if seat < 0:
		# Every seat taken, held seats included. Refusing is the honest answer on a rink where a mallet is the
		# only thing to be: there is no spectator viewpoint in this demo to admit them to.
		print("HOCKEY: refusing peer %d -- every seat is taken or held" % peer)
		multiplayer.multiplayer_peer.disconnect_peer(peer)
		return
	if reclaim != TeamRoster.NO_SESSION:
		_held_sessions.erase(session_id)
		print("HOCKEY: peer %d resumed seat %d (was peer %d, team %d)" % [
			peer, seat, resumed_from, HockeyConfig.team_of_seat(seat)])
	else:
		if resumed_from > 0:
			# Worth saying out loud rather than seating them quietly: this is the conservative rule costing a
			# returning player their seat, and it looks identical to a forgery being refused.
			print("HOCKEY: peer %d claimed session %d, which was never seen to drop -- seating as new" % [
				peer, session_id])
		print("HOCKEY: peer %d seated at %d (team %d)" % [peer, seat, HockeyConfig.team_of_seat(seat)])
	_broadcast_roster()

## A peer's connection is gone. `held` is whether the backend is keeping its session open.
##
## A HELD SEAT IS NOT RELEASED, it changes hands from the peer to the IDENTITY. The mallet keeps its input
## authority pointed at a peer id that no longer exists, which is exactly what the backend's gap policy
## covers: it is written the neutral input row and its state keeps broadcasting, so the other players watch it
## come to rest rather than freeze.
func _on_net_peer_dropped(peer: int, session_id: int, held: bool) -> void:
	if not Net.is_server():
		return
	if held and session_id != TeamRoster.NO_SESSION:
		var kept: int = roster.hold(peer, session_id)
		if kept >= 0:
			_held_sessions[session_id] = true
			print("HOCKEY: peer %d dropped -- holding seat %d for %.0fs" % [
				peer, kept, Net.reconnect_grace()])
			_broadcast_roster()
			return
	# Not held, holding nothing, or claiming no identity. The third way in is the one worth knowing about: a
	# GHOST connection whose identity a returning player already took back, which a killed client leaves behind
	# until its keepalive times out. release() is a no-op there, because the seat already moved.
	roster.release(peer)
	print("HOCKEY: peer %d left (session %d, not held)" % [peer, session_id])
	_broadcast_roster()

## The grace window closed with nobody claiming the session: that player is not coming back. Free the seat and
## let the roster broadcast park the mallet.
func _on_net_session_expired(session_id: int, peer: int) -> void:
	if not Net.is_server():
		return
	_held_sessions.erase(session_id)
	roster.release_session(session_id)
	print("HOCKEY: seat released -- peer %d did not return" % peer)
	_broadcast_roster()

func _on_connected_to_server() -> void:
	_set_state(State.PLAYING)
	print("HOCKEY: connected as peer %d" % multiplayer.get_unique_id())

func _on_connection_failed() -> void:
	_fail("connection failed")

func _on_server_disconnected() -> void:
	# Tear down FIRST, then record the error: leave() ends at State.OFFLINE, so failing before it would have
	# the error state immediately overwritten and the player would be dropped with no explanation.
	leave()
	_fail("the server closed the session")

# --- roster replication ----------------------------------------------------------------------------
# One reliable broadcast of the whole seat table on every change, rather than per-seat deltas. The table is
# thirty-two ints; a delta protocol for it would be more code than the thing it encodes, and a full snapshot is
# self-healing -- a peer that missed one message is corrected by the next.
#
# It goes over a reliable RPC rather than the state lane on purpose: it is what re-points each MalletInput's
# multiplayer authority, and an authority change that arrived late (or in the wrong order relative to another)
# would have the backend rejecting a client's input frames as unauthorized.
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
	if rink == null:
		return
	var my_peer: int = 1 if Net.is_offline() else multiplayer.get_unique_id()
	var found_seat: int = -1
	for seat: int in mini(owners.size(), HockeyConfig.SEATS):
		rink.set_seat_owner(seat, owners[seat])
		if owners[seat] == my_peer:
			found_seat = seat
	if found_seat != _local_seat:
		_local_seat = found_seat
		local_seat_changed.emit(_local_seat)

func _seat_owners() -> PackedInt32Array:
	var owners: PackedInt32Array = PackedInt32Array()
	owners.resize(HockeyConfig.SEATS)
	for seat: int in HockeyConfig.SEATS:
		var peer: int = roster.peer_of_seat(seat)
		owners[seat] = peer if peer > 0 else 0
	return owners

# --- rink lifecycle --------------------------------------------------------------------------------
func _build_rink() -> void:
	rink = RinkDirector.new()
	# A child of the SCENE ROOT, not of this node: NetCommand routes by node path, so the rink's path must be
	# identical on every peer, and hanging it off a fixed, scene-defined parent is the simplest way to
	# guarantee that.
	get_parent().add_child(rink)
	rink.build(roster, _seat_owners())

func _teardown_rink() -> void:
	if rink != null:
		rink.queue_free()
		rink = null

# --- state -----------------------------------------------------------------------------------------
func _set_state(next: State) -> void:
	if next == _state:
		return
	_state = next
	session_state_changed.emit(next)

func _fail(reason: String) -> void:
	_error = reason
	push_warning("Hockey session error: %s" % reason)
	_set_state(State.ERROR)
