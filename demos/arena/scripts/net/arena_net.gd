extends Node
class_name ArenaNet
## The session layer: bring a session up, seat the players, feed the lag-compensation estimates, tear it down.
##
## FOUR ORDERING RULES. Each one is a bug that is invisible until it is not.
##
##   1. THE WORLD IS BUILT BEFORE THE SOCKET BINDS, AND BOUND AFTER THE MODE IS SET. Node paths are what
##      entity ids are derived from, so the graph must exist before any packet can arrive -- but
##      `Net.make_state()` hands back an INERT handle while the facade is OFFLINE, and the facade cannot leave
##      OFFLINE until a peer is assigned. So the build is split (MatchDirector.build then .bind_net_all) and
##      the order is: build the graph, bind the socket, set the mode, register the entities.
##
##   2. set_net_tick_decoupled(30) IS APPLIED BEFORE Net.set_mode(). set_mode starts the tick loop; changing
##      the rate after it has started means the first ticks run at the project default and the clock re-paces
##      underneath them.
##
##   3. set_net_tick_coupled() ON TEARDOWN, and the same for every other session-scoped setting written here:
##      the interest radius, the band scale, the lag-comp estimates. They live on a process-wide facade and on
##      static class state, so leaving them set is the kind of leak that produces "it only happens on the
##      second game".
##
##   4. PLAYERS ARE SEATED ON Net.peer_joined, NOT ON multiplayer.peer_connected. The transport signal fires
##      when the socket comes up, which is BEFORE the OrbitNet handshake -- so the peer's SESSION IDENTITY is
##      not known yet, and identity is the only thing that can tell a returning player from a newcomer.
##
## THE LAG-COMPENSATION ESTIMATES ARE FED FROM HERE, AND THE ADDON DOES NOT DO IT FOR YOU. `NetLagComp` holds
## static state a game owns; the backend has no reason to write it, because a game with no rewind never reads
## it. Two refreshes per tick on the authority -- the per-peer cadence and the per-band one -- and both are
## what turns a flat rewind window into a per-shooter, per-target one.
##
## OBSERVERS DECLARE WHERE THEY WATCH FROM. A peer with no seat has no body to infer an interest centre from,
## and a peer with no centre is filtered in nowhere -- the backend falls open and sends it everything. So a
## seatless peer here is given a centre and a world with `Net.set_peer_anchor()`, which also fixes WHICH ARENA
## it is watching: an observer without one would be in every arena at once.
##
## THE CONSERVATIVE RESUME RULE. `resumed_from` names a connection that may still be up, and a session
## identity is client-asserted and unauthenticated -- so a peer presenting an identity it watched someone else
## use takes that player's seats, and the original keeps its connection with no error. This demo honours a
## reclaim only for an identity it already saw `Net.peer_dropped` report with `held = true`. The price is the
## one the facade names: a player whose old socket the transport has not yet noticed is gone comes back as a
## newcomer.

signal session_state_changed(state: State)
signal local_seats_changed(seats: PackedInt32Array)

enum State { OFFLINE, CONNECTING, PLAYING, ERROR }

var world: MatchDirector = null
var roster: SeatRoster = SeatRoster.new()
## THIS peer's view of where it is watching from, when it is observing.
var observer: ObserverDesk = ObserverDesk.new()

## How many seats this peer asks for, and whether it wants them in different arenas. Read at join time.
var wanted_seats: int = 1
var spread_seats: bool = false
var props_per_arena: int = -1

var _state: State = State.OFFLINE
var _local_seats: PackedInt32Array = PackedInt32Array()
var _error: String = ""
var _observing: bool = false
## SERVER-SIDE: the peers currently observing, and the identities seen to drop with their session held.
var _observers: Dictionary[int, bool] = {}
var _held_sessions: Dictionary[int, bool] = {}

func _init() -> void:
	# Named at construction: the roster RPC below routes by node path, so this node's name is part of the wire
	# contract and must not change after the node is in the tree.
	name = "ArenaNet"

func _ready() -> void:
	if not Net.pre_tick.is_connected(_on_pre_tick):
		Net.pre_tick.connect(_on_pre_tick)
	if not Net.post_tick.is_connected(_on_post_tick):
		Net.post_tick.connect(_on_post_tick)
	if not Net.peer_joined.is_connected(_on_net_peer_joined):
		Net.peer_joined.connect(_on_net_peer_joined)
	if not Net.peer_dropped.is_connected(_on_net_peer_dropped):
		Net.peer_dropped.connect(_on_net_peer_dropped)
	if not Net.peer_session_expired.is_connected(_on_net_session_expired):
		Net.peer_session_expired.connect(_on_net_session_expired)

func state() -> State:
	return _state

func local_seats() -> PackedInt32Array:
	return _local_seats

func error_message() -> String:
	return _error

# --- bring-up --------------------------------------------------------------------------------------
## Single player. No peer, no socket; the facade stays OFFLINE and every handle it hands out is inert, so the
## whole game runs on exactly the same code path with no networking spun up at all.
func start_offline() -> void:
	_teardown_world()
	_local_seats = PackedInt32Array([0])
	roster.assign(SeatRoster.SERVER_PEER, wanted_seats, SeatRoster.NO_SESSION, spread_seats)
	_build_world()
	world.bind_net_all()
	_set_state(State.PLAYING)
	local_seats_changed.emit(_local_seats)
	print("ARENA: offline (seat 0, no networking)")

func host_listen(port: int = NetTransport.DEFAULT_PORT) -> bool:
	return _host(port, false)

func host_dedicated(port: int = NetTransport.DEFAULT_PORT) -> bool:
	return _host(port, true)

func _host(port: int, dedicated: bool) -> bool:
	_teardown_world()
	Net.set_net_tick_decoupled(ArenaConfig.NET_TICK_HZ)   # RULE 2 -- before set_mode()
	if not dedicated:
		_local_seats = roster.assign(
			SeatRoster.SERVER_PEER, wanted_seats, SeatRoster.NO_SESSION, spread_seats)
	_build_world()                                        # RULE 1, first half

	# Seats PLUS observer slots: a cap of SEAT_COUNT refuses a spectator at the socket, before the session
	# layer ever sees a handshake to decide about.
	var peer: MultiplayerPeer = NetTransport.create_server(
		port, ArenaConfig.SEAT_COUNT + ArenaConfig.OBSERVER_SLOTS)
	if peer == null:
		_fail("could not bind a server peer on port %d" % port)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.SERVER if dedicated else Net.Mode.HOST)
	_apply_interest_settings()
	world.bind_net_all()                                  # RULE 1, second half
	_set_state(State.PLAYING)
	local_seats_changed.emit(_local_seats)
	_broadcast_roster()
	print("ARENA: %s on port %d (%s)" % [
		"dedicated" if dedicated else "hosting", port, NetTransport.preferred_kind_name()])
	return true

## Join a server. `target` is whatever the active transport accepts -- `ADDR` or `ADDR:PORT` for ENet, a lobby
## handle for Steam. The demo never learns which, which is the transport factory's whole job.
func join(target: String) -> bool:
	_teardown_world()
	Net.set_net_tick_decoupled(ArenaConfig.NET_TICK_HZ)   # RULE 2
	_build_world()                                        # RULE 1, first half. Seats are owned by nobody yet.

	var peer: MultiplayerPeer = NetTransport.create_client(_address_of(target), _port_of(target))
	if peer == null:
		_fail("could not create a client peer for '%s'" % target)
		return false
	multiplayer.multiplayer_peer = peer
	_connect_peer_signals()
	Net.set_mode(Net.Mode.CLIENT)
	world.bind_net_all()                                  # RULE 1, second half
	_set_state(State.CONNECTING)
	print("ARENA: joining %s:%d (%s)" % [
		_address_of(target), _port_of(target), NetTransport.preferred_kind_name()])
	return true

## Tear the session down and return the process to a clean OFFLINE state.
func leave() -> void:
	if multiplayer.multiplayer_peer != null:
		multiplayer.multiplayer_peer.close()
		multiplayer.multiplayer_peer = null
	Net.set_mode(Net.Mode.OFFLINE)
	Net.set_net_tick_coupled()   # RULE 3
	roster.clear()
	_observers.clear()
	_held_sessions.clear()
	_observing = false
	observer.forget_sent()
	# Static state on classes the PROCESS shares, not the session. The ring would otherwise answer has_tick()
	# for a colliding tick of the next session with references to long-freed bodies.
	NetLagComp.reset_observed_interp()
	if world != null:
		world.resolver.clear()
	_local_seats = PackedInt32Array()
	_teardown_world()
	_set_state(State.OFFLINE)

## The interest settings this session runs at. SERVER-SIDE and applied once the mode is set.
func _apply_interest_settings() -> void:
	Net.set_aoi_radius(ArenaConfig.AOI_RADIUS_M)
	# The band scale is a SEPARATE number from the radius, and by two orders of magnitude here: the radius
	# decides whether a body is sent at all, the band scale how often relative to everything else. Sized to an
	# arena rather than to the session, because a scale large enough to span three would put every body in one
	# band and the per-band interpolation measurements would all read the same.
	Net.set_aoi_band_radius(ArenaConfig.BAND_SCALE_M)

# --- the authoritative step --------------------------------------------------------------------------
## BEFORE the rollback loop. The veto pass has to run here: a cloak is applied inside the loop, from the same
## values this tick's rows are built from, so a pass that ran afterwards would first see the flag on the tick
## whose row already carried it. See MatchDirector.pre_tick().
func _on_pre_tick(_tick: int) -> void:
	if world != null and Net.is_server():
		world.pre_tick()

## AFTER the rollback loop, where every fighter's pose is the authoritative present-tick value -- which is what
## the rewind ring must record, and what a cloak pickup must be decided against.
func _on_post_tick() -> void:
	if world != null and Net.is_server():
		world.post_tick(Net.current_tick())
	_refresh_lag_estimates()

## SERVER-SIDE, once per net tick: hand NetLagComp both halves of what it needs to size a rewind.
##
## PER PEER, because the byte budget is charged per peer and the send path rebuilds its candidate list per
## peer -- a peer watching a quiet arena gets its rows every tick while one in a firefight waits several.
## Pooling them hands the first a window measured partly from the second: over-rewound above the mean,
## under-rewound below it, and under-rewind is the direction that costs a shooter a hit they saw land.
##
## PER BAND, because the same is true across distance: a near row and a far one are sent at different
## cadences, so a target across the arena is staler than one in your face by a factor the send path already
## measures. The two margins multiply -- one peer's cadence pooled across bands, one band's pooled across
## peers -- and the product is the estimate the evidence supports.
func _refresh_lag_estimates() -> void:
	if not Net.is_server():
		return
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	NetLagComp.refresh_observed_interp(wire["interarrival_all"])
	NetLagComp.refresh_band_interp(
		wire["interarrival_near"], wire["interarrival_mid"], wire["interarrival_far"],
		wire["interarrival_all"], Net.aoi_band_radius())
	for peer: int in multiplayer.get_peers():
		NetLagComp.refresh_observed_interp_for(peer, Net.interarrival_ticks(peer))

# --- peer lifecycle ----------------------------------------------------------------------------------
## The CLIENT-side transport signals only. The server's join and leave both come from `Net` -- see RULE 4.
func _connect_peer_signals() -> void:
	if not multiplayer.connected_to_server.is_connected(_on_connected_to_server):
		multiplayer.connected_to_server.connect(_on_connected_to_server)
	if not multiplayer.connection_failed.is_connected(_on_connection_failed):
		multiplayer.connection_failed.connect(_on_connection_failed)
	if not multiplayer.server_disconnected.is_connected(_on_server_disconnected):
		multiplayer.server_disconnected.connect(_on_server_disconnected)

func _on_net_peer_joined(peer: int, session_id: int, resumed_from: int) -> void:
	if not Net.is_server():
		return
	# THE CONSERVATIVE RULE, and it is one line: an identity is worth seats back only if this layer watched
	# that identity leave. `resumed_from` alone is the backend saying "somebody claimed this before", which a
	# forger can also make true.
	var reclaim: int = session_id if _held_sessions.has(session_id) else SeatRoster.NO_SESSION
	var seats: PackedInt32Array = roster.assign(peer, ArenaConfig.MAX_SEATS_PER_PEER, reclaim, true)
	if seats.is_empty():
		# Every seat taken -- so this peer OBSERVES. Refusing used to be the only honest answer to a seatless
		# peer, because it had no body, therefore no interest centre, therefore no filter. Declaring its
		# centre and its arena is what changed.
		print("ARENA: peer %d admitted as an observer -- every seat is taken" % peer)
		_apply_observe(peer, true, 0, Vector3.ZERO, ArenaConfig.FIRST_ARENA_ID)
		return
	if reclaim != SeatRoster.NO_SESSION:
		_held_sessions.erase(session_id)
		print("ARENA: peer %d resumed seats %s (was peer %d)" % [peer, seats, resumed_from])
	else:
		if resumed_from > 0:
			# Worth saying out loud rather than seating them quietly: this is the conservative rule costing a
			# returning player their seats, and it looks identical to a forgery being refused.
			print("ARENA: peer %d claimed session %d, never seen to drop -- seating as new" % [
				peer, session_id])
		print("ARENA: peer %d seated at %s (arena %d)" % [
			peer, seats, ArenaConfig.arena_of_seat(seats[0])])
	_broadcast_roster()

func _on_net_peer_dropped(peer: int, session_id: int, held: bool) -> void:
	if not Net.is_server():
		return
	# The backend drops a departed peer's anchor AND its vetoes with its connection, so there is nothing to
	# retract -- only this layer's own bookkeeping to forget. The cadence measured about it describes a link
	# that no longer exists, and peer ids are reused.
	_observers.erase(peer)
	NetLagComp.forget_peer_interp(peer)
	if world != null:
		world.forget_peer(peer)
	if held and session_id != SeatRoster.NO_SESSION:
		var kept: PackedInt32Array = roster.hold(peer, session_id)
		if not kept.is_empty():
			_held_sessions[session_id] = true
			print("ARENA: peer %d dropped -- holding seats %s for %.0fs" % [
				peer, kept, Net.reconnect_grace()])
			_broadcast_roster()
			return
	# Not held, holding nothing, or claiming no identity. The third way in is the one worth knowing about: a
	# GHOST connection whose identity a returning player already took back, which a killed client leaves
	# behind until its keepalive times out. release() is a no-op there, because the seats already moved.
	roster.release(peer)
	print("ARENA: peer %d left (session %d, not held)" % [peer, session_id])
	_broadcast_roster()

func _on_net_session_expired(session_id: int, peer: int) -> void:
	if not Net.is_server():
		return
	_held_sessions.erase(session_id)
	roster.release_session(session_id)
	print("ARENA: seats released -- peer %d did not return" % peer)
	_broadcast_roster()

func _on_connected_to_server() -> void:
	_set_state(State.PLAYING)
	print("ARENA: connected as peer %d" % multiplayer.get_unique_id())

func _on_connection_failed() -> void:
	_fail("connection failed")

func _on_server_disconnected() -> void:
	# Tear down FIRST, then record the error: leave() ends at State.OFFLINE, so failing before it would have
	# the error state immediately overwritten and the player dropped with no explanation.
	leave()
	_fail("the server closed the session")

# --- observers ---------------------------------------------------------------------------------------
func is_observing() -> bool:
	return _observing

func observer_count() -> int:
	return _observers.size()

## Ask the server to hand this peer's seats back and watch instead, or to seat it again.
##
## A REQUEST, NOT A SETTING. The server owns seating and owns every anchor declaration -- a client that could
## set its own interest centre could set it anywhere, which is why the call is server-side in the facade.
func request_observe(on: bool) -> void:
	if Net.is_offline() or _observing == on:
		return
	_observing = on
	if not on:
		observer.forget_sent()
	if Net.is_server():
		_apply_observe(SeatRoster.SERVER_PEER, on, observer.tracked_entity(), observer.point(),
			observer.arena())
		return
	_observe_request.rpc_id(SeatRoster.SERVER_PEER, on, observer.tracked_entity(), observer.point(),
		observer.arena())

## Offer a ground point in `arena_id` as this peer's viewpoint. Called every frame while observing; sends only
## when ObserverDesk says the declaration has moved enough to be worth a reliable message.
func observe_from(point: Vector3, arena_id: int) -> void:
	observer.watch_point(point, arena_id)
	_offer_viewpoint()

## Offer a fighter to follow instead. `entity_id` comes from `entity_id()` on a handle; 0 is refused by the
## desk, because it is the facade's retraction value rather than an entity.
func observe_entity(entity_id: int, arena_id: int) -> void:
	if observer.watch_entity(entity_id, arena_id):
		_offer_viewpoint()

func _offer_viewpoint() -> void:
	if not _observing or Net.is_offline():
		return
	var now_s: float = float(Time.get_ticks_msec()) / 1000.0
	if not observer.due(now_s):
		return
	observer.mark_sent(now_s)
	if Net.is_server():
		_apply_observe(SeatRoster.SERVER_PEER, true, observer.tracked_entity(), observer.point(),
			observer.arena())
		return
	_observe_request.rpc_id(SeatRoster.SERVER_PEER, true, observer.tracked_entity(), observer.point(),
		observer.arena())

## CLIENT -> SERVER. `any_peer` because every client calls it; the sender id is read from the multiplayer
## layer rather than taken from the payload, which is the same rule every shot is validated by.
@rpc("any_peer", "call_remote", "reliable")
func _observe_request(on: bool, entity_id: int, point: Vector3, arena_id: int) -> void:
	if not Net.is_server():
		return
	_apply_observe(multiplayer.get_remote_sender_id(), on, entity_id, point, arena_id)

## SERVER-SIDE. The only place in this demo that declares an anchor.
##
## A DECLARATION REPLACES INFERENCE ON BOTH AXES AT ONCE -- the centre AND the world. That is what makes it
## the right call for an observer here rather than a nice-to-have: an observer has no body, so it has no
## inferred arena either, and a peer in no arena is in every arena.
##
## THE ORDER MATTERS BOTH WAYS. Starting to observe releases the seats FIRST and then declares, so the roster
## broadcast that parks the fighters does not race a centre about to be replaced. Stopping retracts FIRST and
## then seats, because `clear_peer_anchor()` hands both axes back to inference and inference needs a body.
func _apply_observe(peer: int, on: bool, entity_id: int, point: Vector3, arena_id: int) -> void:
	if not on:
		if not _observers.has(peer):
			return
		_observers.erase(peer)
		Net.clear_peer_anchor(peer)
		var seats: PackedInt32Array = roster.assign(
			peer, ArenaConfig.MAX_SEATS_PER_PEER, Net.peer_session_id(peer), true)
		if seats.is_empty():
			# Somebody took the seats while this peer watched. It stays a spectator rather than being
			# disconnected: it asked to play, not to leave.
			_observers[peer] = true
			Net.set_peer_anchor(peer, point, _sane_arena(arena_id))
			print("ARENA: peer %d asked to play, but every seat is taken -- still observing" % peer)
			return
		print("ARENA: peer %d stopped observing and took seats %s" % [peer, seats])
		_broadcast_roster()
		return

	var had: PackedInt32Array = roster.seats_of_peer(peer)
	if not had.is_empty():
		roster.release(peer)
	_observers[peer] = true
	if entity_id != 0:
		Net.set_peer_anchor_entity(peer, entity_id, _sane_arena(arena_id))
	else:
		Net.set_peer_anchor(peer, point, _sane_arena(arena_id))
	if not had.is_empty():
		print("ARENA: peer %d gave up seats %s to observe" % [peer, had])
		_broadcast_roster()

## An arena id, or the first arena. Never 0: `0` is the facade's EVERY-WORLD membership, so an observer
## declared into it would be watching all three arenas at once -- which is the fail-open answer the
## declaration exists to replace.
func _sane_arena(arena_id: int) -> int:
	return arena_id if ArenaConfig.is_arena(arena_id) else ArenaConfig.FIRST_ARENA_ID

# --- roster replication --------------------------------------------------------------------------------
# One reliable broadcast of the whole seat table on every change, rather than per-seat deltas. The table is
# one int per seat; a delta protocol for it would be more code than the thing it encodes, and a full snapshot
# is self-healing -- a peer that missed one message is corrected by the next.
func _broadcast_roster() -> void:
	if not Net.is_server():
		return
	var owners: PackedInt32Array = roster.seat_owners()
	_apply_roster(owners)
	rpc(&"_roster_sync", owners)

@rpc("authority", "call_remote", "reliable")
func _roster_sync(owners: PackedInt32Array) -> void:
	_apply_roster(owners)

## Re-point every seat, and work out which of them are this peer's.
##
## THE SEAT INDEX IS PER CONNECTION. A fighter's `set_seat()` index says which of its OWNING connection's
## bodies it is, not which of the session's -- so it is derived here by counting that owner's seats in order,
## which every peer can do from the same table without being told.
func _apply_roster(owners: PackedInt32Array) -> void:
	if world == null:
		return
	var my_peer: int = SeatRoster.SERVER_PEER if Net.is_offline() else multiplayer.get_unique_id()
	var seen: Dictionary[int, int] = {}
	var mine: PackedInt32Array = PackedInt32Array()
	for seat: int in mini(owners.size(), ArenaConfig.SEAT_COUNT):
		var owner_peer: int = owners[seat]
		var index: int = seen.get(owner_peer, 0)
		seen[owner_peer] = index + 1
		world.set_seat_owner(seat, owner_peer, index if owner_peer > 0 else 0)
		if owner_peer == my_peer:
			mine.push_back(seat)
	if mine != _local_seats:
		_local_seats = mine
		local_seats_changed.emit(_local_seats)

# --- join targets ----------------------------------------------------------------------------------
static func _address_of(target: String) -> String:
	var separator: int = _port_separator(target)
	return target if separator < 0 else target.substr(0, separator)

static func _port_of(target: String) -> int:
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

# --- internals -------------------------------------------------------------------------------------
func _build_world() -> void:
	world = MatchDirector.new()
	add_child(world)
	world.build(roster, roster.seat_owners(), props_per_arena)

func _teardown_world() -> void:
	if world == null:
		return
	remove_child(world)
	world.queue_free()
	world = null

func _set_state(next: State) -> void:
	if _state == next:
		return
	_state = next
	session_state_changed.emit(_state)

func _fail(message: String) -> void:
	_error = message
	push_warning("Arena session error: %s" % message)
	_set_state(State.ERROR)
