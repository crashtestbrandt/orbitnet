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
##   4. PLAYERS ARE SEATED ON Net.peer_joined, NOT ON multiplayer.peer_connected. The transport signal fires
##      when the socket comes up, which is before the OrbitNet handshake -- so the peer's SESSION IDENTITY is
##      not known yet, and identity is the only thing that can tell a reconnecting player from a newcomer.
##      Seating on the transport signal is what makes a rejoiner land in whatever seat happens to be free.
##
## RECONNECTION, AND WHAT THE SEAT DOES WHILE NOBODY IS IN IT. A dropped peer's seat is HELD: the roster keeps
## naming that peer, the commander keeps its input authority pointed at it, and the backend holds the body on
## the neutral input row with its state still broadcasting (Net.set_reconnect_grace). Nothing is re-pointed on
## the drop. When the player comes back, Net.peer_joined carries the identity that reclaims the seat and the
## roster broadcast re-points the commander at the new peer id. When the window closes instead,
## Net.peer_session_expired frees the seat and the commander goes back to the server.
##
## OBSERVERS: A PEER THAT DECLARES A CENTER INSTEAD OF DRIVING ONE. What a peer OBSERVES is not what its input
## CONTROLS, and until `Net.set_peer_anchor()` existed this demo could not say so: an interest center was read
## off the peer's own rollback body, so a peer with no body had no center, and a peer with no center was
## filtered in nowhere -- the backend falls open and sends it everything. That is why a seatless spectator used
## to be refused at the door rather than admitted.
##
## It is now a supported state. An observer holds no seat, drives no commander, and has its center and world
## DECLARED for it by the server, either at a ground point (`Net.set_peer_anchor`) or on an entity it follows
## (`Net.set_peer_anchor_entity`). Two consequences worth stating:
##
##   - A PEER ARRIVING AT A FULL TABLE IS ADMITTED AS AN OBSERVER, not disconnected. The old refusal was the
##     honest answer to "there is nothing I can do with you"; there is now something.
##   - THE DECLARATION IS THROTTLED, and ObserverDesk decides when. A panning observer moves its center every
##     frame, and one reliable message per frame to restate a center that slid 20 cm is how a spectator costs
##     more than a player.
##
## Deliberately omitted, and listed as known gaps rather than hidden: a build/protocol version handshake (two
## incompatible peers will connect and misbehave rather than being refused with a reason), a join browser, and
## invites. Each is real work in a shipping session layer and none of it teaches anything about the
## replication lanes.

signal session_state_changed(state: State)
signal local_seat_changed(seat: int)

enum State { OFFLINE, CONNECTING, PLAYING, ERROR }

var world: WorldDirector = null
var roster: SeatRoster = SeatRoster.new()
## THIS peer's view of where it is watching from, when it is observing. Client-side and pure; the server is
## told about it by `_observe_request`, and only the server ever calls the facade.
var observer: ObserverDesk = ObserverDesk.new()

var _state: State = State.OFFLINE
var _local_seat: int = -1
var _error: String = ""
var _observing: bool = false
## SERVER-SIDE: the peers currently observing. A value of `true` is the only value ever stored -- this is a
## set, and GDScript's Dictionary is what a typed set is spelled as here.
var _observers: Dictionary[int, bool] = {}

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
	# The session-lifecycle signals, connected once for the same reason: they are inert until a session is
	# hosted, and only ever fire on the authority.
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

	# Seats PLUS observer slots: a cap of SEATS refuses a spectator at the socket, before the session layer
	# ever sees a handshake to decide about.
	var peer: MultiplayerPeer = NetTransport.create_server(
		port, RtsConfig.SEATS + RtsConfig.OBSERVER_SLOTS)
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

	var peer: MultiplayerPeer = NetTransport.create_client(target_address(address), target_port(address))
	if peer == null:
		_fail("could not create a client peer for '%s'" % address)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.CLIENT)
	world.bind_net_all()                                 # RULE 1, second half
	_set_state(State.CONNECTING)
	print("RTS: joining %s:%d (%s)" % [
		target_address(address), target_port(address), NetTransport.preferred_kind_name()])
	return true

## Tear the session down and return the process to a clean OFFLINE state.
func leave() -> void:
	if multiplayer.multiplayer_peer != null:
		multiplayer.multiplayer_peer.close()
		multiplayer.multiplayer_peer = null
	Net.set_mode(Net.Mode.OFFLINE)
	Net.set_net_tick_coupled()   # RULE 3
	roster.clear()
	# The anchors went with the socket. What survives a teardown is this layer's memory of having SENT a
	# declaration, and leaving that set would have the next session's observer wait a resend interval before
	# telling a server that has never heard of it where it is watching from.
	_observers.clear()
	_observing = false
	observer.forget_sent()
	# Static state on a class the process shares. Leaving a previous session's cadences behind is the same
	# family of leak as leaving the tick decoupled -- see RULE 3.
	NetLagComp.reset_observed_interp()
	_local_seat = -1
	_teardown_world()
	_set_state(State.OFFLINE)

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

# --- the authoritative step ------------------------------------------------------------------------
func _on_pre_tick(_tick: int) -> void:
	if world != null and Net.is_server():
		world.net_step()
	_refresh_interp_estimates()

## SERVER-SIDE, once per net tick: hand NetLagComp each peer's measured send cadence.
##
## THE ADDON DOES NOT DO THIS FOR YOU. `NetLagComp`'s estimate is static state on a class the game owns, and
## the backend has no reason to write it -- a game with no rewind never reads it. Feeding it is three lines
## and they belong here, in the one place that already runs per tick on the authority.
##
## PER PEER, NOT POOLED. The byte budget is charged per peer and the send path rebuilds its candidate list per
## peer, so a peer with a small interest set gets its rows every tick while a peer in a dense part of the map
## waits several. Pooling them hands the first peer a window measured partly from the second: over-rewound
## above the mean, under-rewound below it. The pooled figure is still refreshed, because it is the fallback
## for a peer nothing has been measured about yet -- a fresh joiner is better served by the session's mean
## than by the one-tick floor, at exactly the moment its link is least settled.
func _refresh_interp_estimates() -> void:
	if not Net.is_server():
		return
	NetLagComp.refresh_observed_interp(Net.interarrival_all_ticks())
	for peer: int in multiplayer.get_peers():
		NetLagComp.refresh_observed_interp_for(peer, Net.interarrival_ticks(peer))

# --- peer lifecycle ------------------------------------------------------------------------------
## The CLIENT-side transport signals only. The server's join and leave both come from Net (RULE 4): the
## transport's peer_connected fires before any identity is known, and its peer_disconnected cannot say whether
## the session is being held open.
func _connect_peer_signals() -> void:
	if not multiplayer.connected_to_server.is_connected(_on_connected_to_server):
		multiplayer.connected_to_server.connect(_on_connected_to_server)
	if not multiplayer.connection_failed.is_connected(_on_connection_failed):
		multiplayer.connection_failed.connect(_on_connection_failed)
	if not multiplayer.server_disconnected.is_connected(_on_server_disconnected):
		multiplayer.server_disconnected.connect(_on_server_disconnected)

## The seat assignment, on the OrbitNet handshake rather than the transport connect -- see RULE 4. `resumed_from`
## is the peer id this player held before it dropped, or 0 for a newcomer.
##
## THIS DEMO TAKES THE PERMISSIVE RULE, DELIBERATELY. A session reclaims its seat on presentation of the
## identity alone, which is what makes a fast reconnect work at all -- the transport can take tens of seconds
## to report the old socket as gone, and until it does, a returning player would otherwise be refused as
## "every seat is taken". The price is that a forged identity takes a live player's seat rather than merely
## losing a future resume: the original keeps its connection and its commander simply stops answering it. That
## is stated as a limit in `README.md`. A game that wants the conservative rule honors `resumed_from` only for
## a session it already saw `Net.peer_dropped` report with `held = true`.
func _on_net_peer_joined(peer: int, session_id: int, resumed_from: int) -> void:
	if not Net.is_server():
		return
	var seat: int = roster.assign(peer, session_id)
	if seat < 0:
		# Every seat taken -- so this peer OBSERVES. Refusing used to be the honest answer, because a seatless
		# peer had no rollback body, therefore no interest center, therefore no filter: it would have received
		# the whole world and had every order rejected with no explanation. Declaring its center is what
		# changed, and the middle of the map is where a spectator with no preference is put.
		print("RTS: peer %d admitted as an observer -- every seat is taken" % peer)
		_apply_observe(peer, true, 0, Vector3.ZERO)
		return
	if resumed_from > 0:
		print("RTS: peer %d resumed seat %d (was peer %d)" % [peer, seat, resumed_from])
	else:
		print("RTS: peer %d seated at %d" % [peer, seat])
	# Re-points the commander at the new peer id on every peer, which is what makes the seat playable again.
	_broadcast_roster()

## A peer's connection is gone. `held` is whether the backend is keeping its session open.
##
## A HELD SEAT IS NOT TOUCHED. The roster keeps naming the departed peer, so the seat stays taken and the
## commander keeps its input authority pointed at an id that is no longer connected -- which is exactly the
## state the backend's gap policy covers: the body is held on the neutral input row and keeps broadcasting, so
## every other peer sees it come to rest rather than freeze. Releasing here instead would open the seat to the
## next arrival while its owner is still inside the grace window.
func _on_net_peer_dropped(peer: int, session_id: int, held: bool) -> void:
	if not Net.is_server():
		return
	# The backend drops a departed peer's anchor with its connection, so there is nothing to retract here --
	# only this layer's own bookkeeping to forget, and it must be forgotten either way. A held session keeps
	# its SEAT, not its viewpoint: an observer that returns declares where it is watching from again.
	_observers.erase(peer)
	# The cadence measured about a departed peer describes a link that no longer exists, and peer ids are
	# reused. Left behind, it would be handed to whoever arrives next under that id.
	NetLagComp.forget_peer_interp(peer)
	var seat: int = roster.seat_of_peer(peer)
	if held and seat >= 0:
		print("RTS: peer %d dropped -- holding seat %d for %.0fs" % [peer, seat, Net.reconnect_grace()])
		return
	# Not held, or holding nothing. Three ways to get here: the peer claimed no identity, it was refused
	# before it was ever seated, or it is a GHOST whose identity a returning player already took back -- the
	# connection a killed client leaves behind until its keepalive times out. release() is a no-op in that
	# last case, because assign() moved the seat to the new peer id when the player arrived.
	roster.release(peer)
	print("RTS: peer %d left (session %d, seat %d, not held)" % [peer, session_id, seat])
	_broadcast_roster()

## The grace window closed with nobody claiming the session: the player is not coming back. Free the seat, and
## let the roster broadcast hand the commander back to the server (owner 0 resolves to peer 1).
func _on_net_session_expired(session_id: int, peer: int) -> void:
	if not Net.is_server():
		return
	roster.release_session(session_id)
	print("RTS: seat released -- peer %d did not return" % peer)
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


# --- observers ------------------------------------------------------------------------------------
## Whether THIS peer is observing rather than playing.
func is_observing() -> bool:
	return _observing

## SERVER-SIDE: whether `peer` is observing. The HUD reads it to report how many are watching.
func observer_count() -> int:
	return _observers.size()

## Ask the server to hand this peer's seat back and watch instead, or to seat it again.
##
## A REQUEST, NOT A SETTING. The server owns seating and owns every anchor declaration -- a client that could
## set its own interest center could set it anywhere, which is the whole reason the call is server-side in the
## facade. `_observing` is updated when the request is sent rather than when it is answered, because the only
## thing it drives locally is which viewpoint this peer offers next, and offering the wrong one costs a
## message rather than correctness.
func request_observe(on: bool) -> void:
	if Net.is_offline():
		return
	if _observing == on:
		return
	_observing = on
	if not on:
		observer.forget_sent()
	if Net.is_server():
		# A listen host asking itself. There is no datagram, and its own peer id is the server's.
		_apply_observe(SeatRoster.SERVER_PEER, on, observer.tracked_entity(), observer.point())
		return
	_observe_request.rpc_id(SeatRoster.SERVER_PEER, on, observer.tracked_entity(), observer.point())

## Offer a ground point as this peer's viewpoint. Called every frame while observing; sends only when
## ObserverDesk says the declaration has moved enough to be worth a reliable message.
func observe_from(point: Vector3) -> void:
	observer.watch_point(point)
	_offer_viewpoint()

## Offer an entity to follow instead. `entity_id` comes from `entity_id()` on a handle; 0 is refused by the
## desk, because it is the facade's retraction value rather than an entity.
func observe_entity(entity_id: int) -> void:
	if observer.watch_entity(entity_id):
		_offer_viewpoint()

func _offer_viewpoint() -> void:
	if not _observing or Net.is_offline():
		return
	var now_s: float = float(Time.get_ticks_msec()) / 1000.0
	if not observer.due(now_s):
		return
	observer.mark_sent(now_s)
	if Net.is_server():
		_apply_observe(SeatRoster.SERVER_PEER, true, observer.tracked_entity(), observer.point())
		return
	_observe_request.rpc_id(SeatRoster.SERVER_PEER, true, observer.tracked_entity(), observer.point())

## CLIENT -> SERVER. `any_peer` because every client calls it; the sender id is read from the multiplayer
## layer rather than taken from the payload, which is the same rule every order in this demo is validated by.
@rpc("any_peer", "call_remote", "reliable")
func _observe_request(on: bool, entity_id: int, point: Vector3) -> void:
	if not Net.is_server():
		return
	_apply_observe(multiplayer.get_remote_sender_id(), on, entity_id, point)

## SERVER-SIDE. The only place in this demo that declares an anchor.
##
## THE ORDER MATTERS ON THE WAY IN AND ON THE WAY OUT. Starting to observe releases the seat FIRST and then
## declares, so the roster broadcast that hands the commander back to the server does not race a center that
## is about to be replaced anyway. Stopping retracts FIRST and then seats, so the peer is inferring from its
## own commander by the time it has one -- `clear_peer_anchor` hands both the center and the world back to
## inference, and inference needs the body to exist.
func _apply_observe(peer: int, on: bool, entity_id: int, point: Vector3) -> void:
	if not on:
		if not _observers.has(peer):
			return
		_observers.erase(peer)
		Net.clear_peer_anchor(peer)
		var seat: int = roster.assign(peer, Net.peer_session_id(peer))
		if seat < 0:
			# Somebody took the seat while this peer was watching. It stays a spectator rather than being
			# disconnected: it asked to play, not to leave.
			_observers[peer] = true
			Net.set_peer_anchor(peer, point, 0)
			print("RTS: peer %d asked to play, but every seat is taken -- still observing" % peer)
			return
		print("RTS: peer %d stopped observing and took seat %d" % [peer, seat])
		_broadcast_roster()
		return

	var was_seated: int = roster.seat_of_peer(peer)
	if was_seated >= 0:
		roster.release(peer)
	_observers[peer] = true
	if entity_id != 0:
		Net.set_peer_anchor_entity(peer, entity_id, 0)
	else:
		Net.set_peer_anchor(peer, point, 0)
	if was_seated >= 0:
		print("RTS: peer %d gave up seat %d to observe" % [peer, was_seated])
		_broadcast_roster()

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
