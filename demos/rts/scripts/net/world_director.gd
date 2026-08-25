extends Node3D
class_name WorldDirector
## Builds the world, owns the unit pool, runs the authoritative step, and adjudicates every order.
##
## A STATIC UNIT POOL, NOT SPAWN/DESPAWN. Every peer creates all RtsConfig.UNIT_COUNT unit nodes at world
## build, with identical names, and the node set NEVER changes afterward. Death sets a replicated liveness
## bit; the respawn drip clears it. That is a deliberate departure from "the server queue_free()s a dead
## unit", and the reason is entity identity:
##
##   OrbitNet derives an entity id from its synchronizer root's NODE PATH. Freeing and re-creating a node
##   means re-creating it at exactly the same path on every peer, in the same order, or the ids diverge and
##   replication silently goes nowhere. Doing that correctly needs a spawn-replication mechanism -- which is a
##   real and interesting problem, and completely the wrong one for a demo about replication LANES to also be
##   about. A fixed pool makes path agreement true by construction, so the entity-id gate in the probe is
##   asserting something the design guarantees rather than something the code happens to get right today.
##
##   It also costs nothing here: 96 nodes is 96 nodes whether or not they are alive, and a dead unit's
##   properties stop changing, so the state lane's dirty-tracking stops sending it. A dead army is free.
##
## Everything below runs on the SERVER (or offline, which is its own server). Clients build the identical
## world so the paths match, then receive.

## Emitted once the node graph exists and every entity is registered. RtsNet waits for this before binding the
## transport -- the world must exist BEFORE the socket does, or the first packets arrive with nowhere to land.
signal world_built()

## Emitted on the server when an order is accepted, for the HUD's order log.
signal order_applied(seat: int, verb: StringName, count: int, sequence: int)

var units: Array[UnitBody] = []
var commanders: Array[CommanderAvatar] = []
var roster: SeatRoster = null
var obstacles: Array[AABB] = []

var _units_root: Node3D = null
var _commanders_root: Node3D = null
var _order_channels: Array[NetCommand] = []
var _throttle: CommandThrottle = null

# Flat, id-indexed mirrors of the pool, rebuilt at the top of every step. Flat arrays rather than repeated
# node property reads because the combat pass is O(n^2) over them and a PackedVector3Array read is a memory
# fetch where a node property read is a Variant round-trip.
var _positions: PackedVector3Array = PackedVector3Array()
var _alive: PackedByteArray = PackedByteArray()
## Each unit's seat, parallel to the two mirrors above. Derived once from the id and never changed -- the
## pool is static -- so the fog pass reads an array instead of calling RtsConfig.seat_of() per unit per tick.
var _unit_seats: PackedInt32Array = PackedInt32Array()

## SERVER-SIDE fog of war: one ScoutPolicy per seat, and the per-peer vetoes they drive.
##
## OFF BY DEFAULT, like every other lever in this demo. The plain configuration is the one a reader should
## see first, and fog is a rule about this GAME rather than a property of the netcode -- what the netcode
## supplies is `Net.set_entity_hidden()`, and a demo that turned it on unasked would make a policy look like
## a default.
var _fog: Array[ScoutPolicy] = []
var _fog_on: bool = false
var _fog_ticks: int = 0

var _clock: float = 0.0          # seconds of simulated time, server-side
var _offline_accumulator: float = 0.0
var _next_sequence: int = 1
var _built: bool = false
var _bound: bool = false

func _init() -> void:
	# The name is set at CONSTRUCTION, before this node is ever added to a tree. Every entity id under it is
	# a hash of a path that starts with this name, so renaming it after the children exist would silently
	# re-key the whole world.
	name = RtsNames.WORLD_ROOT
	_throttle = CommandThrottle.new(RtsConfig.ORDER_RATE_PER_S, RtsConfig.ORDER_BURST)

# --- world build ---------------------------------------------------------------------------------
## Build the world. `seat_owners` maps seat index -> peer id (0 where a seat is empty); it must be the SAME on
## every peer, which is why RtsNet only calls this on a client after the server has told it the roster.
func build(seat_roster: SeatRoster, seat_owners: PackedInt32Array) -> void:
	if _built:
		return
	_built = true
	roster = seat_roster
	obstacles = build_obstacles()

	_units_root = Node3D.new()
	_units_root.name = RtsNames.UNITS_ROOT
	add_child(_units_root)

	_commanders_root = Node3D.new()
	_commanders_root.name = RtsNames.COMMANDERS_ROOT
	add_child(_commanders_root)

	# Units. configure() before add_child, because the NAME is part of the path the entity id is hashed from.
	# Registration is a SEPARATE pass -- see bind_net_all().
	units.resize(RtsConfig.UNIT_COUNT)
	_unit_seats.resize(RtsConfig.UNIT_COUNT)
	for id: int in RtsConfig.UNIT_COUNT:
		var unit: UnitBody = UnitBody.new()
		unit.configure(id)
		_units_root.add_child(unit)
		unit.place(spawn_position(id), spawn_facing(id))
		units[id] = unit
		_unit_seats[id] = unit.seat

	# One visibility policy per seat, built here rather than lazily so the fog lever has something to switch
	# on. They are inert until set_fog(true), and on a client they are never refreshed at all -- a client does
	# not decide what it may receive.
	_fog.resize(RtsConfig.SEATS)
	for seat: int in RtsConfig.SEATS:
		_fog[seat] = ScoutPolicy.new()

	# Commanders: one per SEAT, always all of them, whether or not anyone is sitting there. Creating them
	# lazily as players join would mean the two peers build different node sets and therefore different
	# entity ids -- the exact failure the static pool exists to avoid.
	commanders.resize(RtsConfig.SEATS)
	for seat: int in RtsConfig.SEATS:
		var owner_peer: int = seat_owners[seat] if seat < seat_owners.size() else 0
		var commander: CommanderAvatar = CommanderAvatar.new()
		commander.configure(seat, owner_peer)
		_commanders_root.add_child(commander)
		commanders[seat] = commander

	# One order channel per seat. NetCommand routes by node path, so every peer builds all of them; a peer
	# only ever submits on its own.
	_order_channels.resize(RtsConfig.SEATS)
	for seat: int in RtsConfig.SEATS:
		_order_channels[seat] = _build_order_channel(seat)

	_refresh_mirrors()
	world_built.emit()
	print("RTS world built: %d units, %d seats, sig=%d" % [units.size(), RtsConfig.SEATS, world_signature()])

## Register every entity with the facade. A SEPARATE pass from build(), and the split is not cosmetic.
##
## `Net.make_state()` / `Net.register_rollback_body()` return INERT handles while the facade is OFFLINE --
## that is the contract that lets a single-player launch run the same code with no networking. Which means
## registration cannot happen until after `Net.set_mode()`, and `set_mode()` cannot happen until a peer is
## assigned. If build and bind were one call, a session would have to build its world after the socket was
## already live, re-opening the window RtsNet's rule 1 exists to close.
##
## Splitting them closes it: the node graph -- every path the entity ids are derived from -- exists before the
## socket does, and only the registration waits for the mode. RtsNet calls these in that order.
func bind_net_all() -> void:
	if not _built or _bound:
		return
	_bound = true
	for unit: UnitBody in units:
		if unit != null:
			unit.bind_net()
	for commander: CommanderAvatar in commanders:
		if commander != null:
			commander.bind_net()
	print("RTS world bound to the %s lane set (%d entities)" % [
		Net.mode_name(Net.current_mode()), units.size() + commanders.size()])
	# ASKED OF THE BACKEND, NOT ECHOED. A hook is resolved by name on the channel's root, and a name that did
	# not resolve leaves the channel on the per-property walk while the call site reports nothing.
	print("RTS-MARSHAL units=%d/%d (one crossing per row in each direction, %d props)" % [
		bulk_units(), units.size(), 3])

## How many unit channels are actually marshalling in bulk, asked of the backend.
func bulk_units() -> int:
	var count: int = 0
	for unit: UnitBody in units:
		if unit != null and unit.uses_bulk_marshalling():
			count += 1
	return count

## Re-point a seat's commander at a new owning peer, and re-evaluate prediction. Called on every peer when the
## roster changes, so authority agrees everywhere.
func set_seat_owner(seat: int, peer: int) -> void:
	if seat < 0 or seat >= commanders.size():
		return
	var commander: CommanderAvatar = commanders[seat]
	if commander != null:
		commander.set_owner_peer(peer)
	# THE FOG IS KEYED ON THE PEER, AND THE PEER JUST CHANGED. A veto is dropped when its peer disconnects, so
	# the arriving peer holds none -- while this seat's policy still remembers what the DEPARTED one could
	# see. Left alone, the next refresh would report only the difference against that stale memory and leave
	# the newcomer permanently able to see whatever its predecessor could.
	if seat < _fog.size():
		_fog[seat].clear()

## The signature of the world this peer built -- see RtsNames.world_signature(). Printed at build and asserted
## equal across peers by the probe: it is the direct gate on deterministic naming, and therefore on entity-id
## agreement.
func world_signature() -> int:
	var paths: PackedStringArray = PackedStringArray()
	for unit: UnitBody in units:
		if unit != null:
			paths.push_back(String(unit.get_path()))
	for commander: CommanderAvatar in commanders:
		if commander != null:
			paths.push_back(String(commander.get_path()))
	return RtsNames.world_signature(paths)

## The obstacle field. A fixed list, not a random one: both peers need the same map, and a seed would be one
## more thing to agree on for no gain. The same AABBs feed the steering AND the visuals, so what you see is
## provably what units collide with.
static func build_obstacles() -> Array[AABB]:
	var out: Array[AABB] = []
	# A central spine, offset either side, plus two flanking blocks. Enough to make pathing visible without
	# turning the demo into a navmesh exercise (there is no pathfinding here -- units slide along boxes).
	out.push_back(AABB(Vector3(-4.0, 0.0, -26.0), Vector3(8.0, 4.0, 18.0)))
	out.push_back(AABB(Vector3(-4.0, 0.0, 8.0), Vector3(8.0, 4.0, 18.0)))
	out.push_back(AABB(Vector3(-30.0, 0.0, -6.0), Vector3(10.0, 3.0, 12.0)))
	out.push_back(AABB(Vector3(20.0, 0.0, -6.0), Vector3(10.0, 3.0, 12.0)))
	out.push_back(AABB(Vector3(-46.0, 0.0, 18.0), Vector3(6.0, 3.0, 14.0)))
	out.push_back(AABB(Vector3(40.0, 0.0, -32.0), Vector3(6.0, 3.0, 14.0)))
	return out

## Where unit `id` starts and respawns. A deterministic scatter around its seat's spawn center -- deterministic
## because both peers build the world, and a random scatter would make the initial positions differ until the
## first state packet arrived (a visible pop on every join).
static func spawn_position(id: int) -> Vector3:
	var index: int = id % RtsConfig.UNITS_PER_SEAT
	var seat: int = RtsConfig.seat_of(id)
	# A phyllotaxis spiral: even coverage of a disc with no clumping, from one integer, with no RNG.
	var golden_angle: float = 2.39996323
	var angle: float = float(index) * golden_angle
	var radius: float = RtsConfig.SPAWN_SPREAD * sqrt(float(index) / float(maxi(1, RtsConfig.UNITS_PER_SEAT)))
	var center: Vector3 = RtsConfig.spawn_center(seat)
	var at: Vector3 = center + Vector3(cos(angle) * radius, 0.0, sin(angle) * radius)
	return UnitSteering.clamp_to_field(at, 1.0)

## Which way a unit faces at spawn: toward the middle of the map, i.e. toward the other seat.
static func spawn_facing(id: int) -> float:
	var seat: int = RtsConfig.seat_of(id)
	return 0.0 if seat == 1 else PI
	# yaw 0 is +Z and yaw PI is -Z under UnitSteering's convention; either way they end up looking inward
	# once they move, since facing chases velocity.

# --- the authoritative step ------------------------------------------------------------------------
func _ready_to_step() -> bool:
	return _built and (Net.is_offline() or Net.is_server())

## Networked: driven from Net.pre_tick by RtsNet, once per net tick.
func net_step() -> void:
	if not _ready_to_step():
		return
	step(RtsConfig.NET_TICK_DT)
	# AFTER the step, so the vetoes are decided from the poses this tick will actually broadcast. Before it,
	# every visibility answer would be one tick stale -- which is survivable, and is still the wrong tick to
	# have withheld a row from.
	_fog_pass()

func _physics_process(delta: float) -> void:
	if not _built:
		return
	if Net.is_offline():
		# OFFLINE the net tick loop does not run, so the sim is paced by a fixed accumulator at exactly the
		# same dt the networked path uses. One _step() body, two clocks -- so "it behaves differently
		# offline" cannot happen quietly.
		_offline_accumulator += delta
		var guard: int = 0
		while _offline_accumulator >= RtsConfig.NET_TICK_DT and guard < 4:
			_offline_accumulator -= RtsConfig.NET_TICK_DT
			guard += 1
			step(RtsConfig.NET_TICK_DT)
		if guard >= 4:
			# A long stall (a breakpoint, a window drag) must not turn into a burst of catch-up ticks; drop
			# the backlog instead. The networked path gets the same protection from the tick clock itself.
			_offline_accumulator = 0.0
	elif not Net.is_server():
		# A receiving client does not simulate, but it still needs an up-to-date view of the world for
		# selection and the HUD -- and it has to notice a respawn so interpolation does not smooth a unit
		# across the map.
		for unit: UnitBody in units:
			if unit != null:
				unit.note_liveness_for_interp()
		_refresh_mirrors()

## One authoritative simulation tick. Server-only (or offline).
func step(dt: float) -> void:
	_clock += dt
	_refresh_mirrors()

	# Movement, from ONE snapshot of the world. Every unit reads the same positions array, so the result does
	# not depend on iteration order -- which is not required for correctness here (there is no lockstep) but
	# makes the sim reproducible enough to debug.
	for unit: UnitBody in units:
		if unit != null and unit.is_alive():
			unit.sim_step(_positions, _alive, obstacles, dt)

	_refresh_mirrors()
	_combat_pass(dt)
	_respawn_pass()

func _combat_pass(dt: float) -> void:
	for unit: UnitBody in units:
		if unit == null or not unit.is_alive():
			continue
		var target: int = unit.target_id()
		if target < 0 or target >= units.size():
			continue
		var victim: UnitBody = units[target]
		if victim == null or not victim.is_alive():
			continue
		if not Combat.in_attack_range(unit.attack_position(), victim.attack_position(), unit.arch.attack_range):
			continue
		victim.take_damage(Combat.damage(unit.arch, dt), _clock)

func _respawn_pass() -> void:
	# Rate-limited PER SEAT so a wipe trickles back rather than resurrecting an army in one tick -- a step
	# change in the wire load exactly when someone is watching the bandwidth readout.
	for seat: int in RtsConfig.SEATS:
		var revived: int = 0
		var first: int = RtsConfig.first_id_of_seat(seat)
		for offset: int in RtsConfig.UNITS_PER_SEAT:
			if revived >= RtsConfig.RESPAWN_PER_TICK:
				break
			var unit: UnitBody = units[first + offset]
			if unit != null and unit.ready_to_respawn(_clock):
				unit.place(spawn_position(unit.id), spawn_facing(unit.id))
				revived += 1

func _refresh_mirrors() -> void:
	if _positions.size() != units.size():
		_positions.resize(units.size())
		_alive.resize(units.size())
	for id: int in units.size():
		var unit: UnitBody = units[id]
		if unit == null:
			_positions[id] = Vector3.ZERO
			_alive[id] = 0
			continue
		_positions[id] = unit.position
		_alive[id] = 1 if unit.is_alive() else 0


# --- fog of war ----------------------------------------------------------------------------------
## How many net ticks between visibility passes. A pass is every unit against every eye, and vision does not
## change meaningfully inside 200 ms at these speeds -- the hysteresis band is wider than a unit walks in
## that time, which is what makes the interval safe rather than merely cheap.
const FOG_REFRESH_TICKS: int = 4

## Turn fog of war on or off. SERVER-SIDE: on a client this changes nothing, because a client cannot decide
## what it is allowed to receive, which is the entire security property the veto has.
func set_fog(on: bool) -> void:
	if _fog_on == on:
		return
	_fog_on = on
	if not on:
		_retract_every_veto()
	_fog_ticks = 0

func fog_enabled() -> bool:
	return _fog_on

## How many units are currently withheld from `seat`. 0 when the fog is off or the seat is unknown.
func fog_hidden_count(seat: int) -> int:
	if not _fog_on or seat < 0 or seat >= _fog.size():
		return 0
	return _fog[seat].hidden_count()

## Recompute what each seat can see and move the vetoes to match.
##
## A LISTEN HOST'S OWN SEAT IS SKIPPED, and that is not an omission. The veto refuses a row in a DATAGRAM, and
## the server sends itself none -- it holds the authoritative world by construction. Fog is a thing you do to
## remote peers; a host sees everything and always did.
func _fog_pass() -> void:
	if not _fog_on or not Net.is_server():
		return
	_fog_ticks += 1
	if _fog_ticks < FOG_REFRESH_TICKS:
		return
	_fog_ticks = 0
	for seat: int in mini(RtsConfig.SEATS, _fog.size()):
		var peer: int = roster.peer_of_seat(seat)
		if peer <= SeatRoster.SERVER_PEER:
			continue
		var policy: ScoutPolicy = _fog[seat]
		var changed: PackedInt32Array = policy.refresh(seat, _unit_seats, _positions, _alive)
		for index: int in changed:
			var unit: UnitBody = units[index]
			if unit != null:
				Net.set_entity_hidden(peer, unit.entity_id(), not policy.is_visible(index))

## Hand every withheld unit back, then forget. Both halves are required: retracting without clearing leaves
## each policy believing units are hidden that are not, and clearing without retracting strands live vetoes
## the demo can no longer name.
func _retract_every_veto() -> void:
	for seat: int in mini(RtsConfig.SEATS, _fog.size()):
		var peer: int = roster.peer_of_seat(seat)
		var policy: ScoutPolicy = _fog[seat]
		if peer > SeatRoster.SERVER_PEER:
			for index: int in units.size():
				var unit: UnitBody = units[index]
				if unit != null and not policy.is_visible(index):
					Net.set_entity_hidden(peer, unit.entity_id(), false)
		policy.clear()

## The current liveness mirror, for the order validator and the HUD. A copy, so a caller cannot reach in and
## edit the server's view of who is alive.
func alive_mask() -> PackedByteArray:
	return _alive.duplicate()

## The current position mirror (a copy, same reasoning). The renderer and the selection code read it.
func position_mirror() -> PackedVector3Array:
	return _positions.duplicate()

## Simulated seconds since the world was built. The order-RTT measurement and the respawn timer share it.
func clock() -> float:
	return _clock

# --- orders --------------------------------------------------------------------------------------
func _build_order_channel(seat: int) -> NetCommand:
	var channel: NetCommand = NetCommand.new()
	channel.name = RtsNames.orders_node_name(seat)
	add_child(channel)
	var verbs: Array[StringName] = [OrderValidator.VERB_MOVE, OrderValidator.VERB_ATTACK_MOVE,
		OrderValidator.VERB_STOP, OrderValidator.VERB_HOLD]
	for verb: StringName in verbs:
		# bind() appends its arguments AFTER the call's own, so the handler signature is
		# (sender_id, payload, seat, verb) -- matching NetCommand's Callable(sender, payload) contract.
		channel.register(verb, _apply_order.bind(seat, verb))
	return channel

## Submit an order on `seat`'s channel, and answer the TAG that names it in [signal NetCommand.rejected].
## Called by the local player's controller; on a client this becomes a reliable RPC to the server, on a
## host/offline it applies immediately through the same path. `0` means nothing was sent.
func submit_order(seat: int, verb: StringName, ids: PackedInt32Array, point: Vector3) -> int:
	if seat < 0 or seat >= _order_channels.size():
		return 0
	var channel: NetCommand = _order_channels[seat]
	if channel == null:
		return 0
	return channel.request(verb, {"ids": ids, "point": point})

## One seat's order channel, so the local player's controller can hear its own refusals. Null for a seat with
## no channel, and null before the world is built.
##
## [signal NetCommand.rejected] fires on the peer that refused the order AND on the client that asked for it,
## which is what lets the RTT measurement cancel a refused order the tick it is refused instead of timing out.
func order_channel(seat: int) -> NetCommand:
	if seat < 0 or seat >= _order_channels.size():
		return null
	return _order_channels[seat]

# The server-side validator+applier for one order. Runs ONLY on the applying peer (server, or the local peer
# offline). Everything a client sent is suspect until this returns.
#
# Returns an [OrderValidator.Code] rather than a bool, and that is what carries the refusal back to the client
# that asked: NetCommand replies with an int verdict and announces nothing for a `false`. `OK` is 0, so the
# acceptance path reads exactly as it did.
#
# THE RATE-LIMIT BRANCH REPLIES TOO, and the alternative is worth stating. A refusal is one reliable packet
# per request, so replying to a throttled client is strictly 1:1 with what that client already sent -- it
# amplifies nothing. Returning `false` there instead would make the refusal silent, which is the right choice
# for a channel where a client can ask far faster than it can be answered; this one is capped at ten requests
# a second per sender, and a player being throttled is a player who should be told.
func _apply_order(sender_id: int, payload: Dictionary, channel_seat: int, verb: StringName) -> int:
	# 1. Rate limit FIRST -- it is the cheapest rejection, and it must run before any work proportional to
	#    the payload size.
	if not _throttle.allow(sender_id, Time.get_ticks_msec() / 1000.0):
		return OrderValidator.Code.RATE_LIMITED

	# 2. Resolve WHO is asking from the sender id, never from the payload.
	var sender_seat: int = roster.seat_for_sender(sender_id)

	# 3. The channel a request arrived on must be the sender's own. This is what makes a per-seat channel
	#    worth having: submitting on someone else's channel is unambiguous forgery, catchable before the
	#    payload is even parsed.
	if sender_seat != channel_seat:
		push_warning("RTS: peer %d holds seat %d but submitted on seat %d's channel"
			% [sender_id, sender_seat, channel_seat])
		return OrderValidator.Code.FOREIGN_CHANNEL

	# 4. Validate the payload itself (ownership of every named id, liveness, cardinality, finiteness).
	var result: OrderValidator.Result = OrderValidator.validate(sender_seat, verb, payload, _alive)
	if not result.accepted:
		# The detailed reason names ids and seats, so it stays on the server; the client is told the code.
		push_warning("RTS: order refused on seat %d: %s" % [channel_seat, result.reason])
		return result.code
	if result.ids.is_empty():
		# Every named unit died in flight. An ordinary race, not an error and not a refusal -- see rule 2.
		return OrderValidator.Code.OK

	# 5. Apply. One sequence number per ORDER, stamped onto every unit it names -- so a client can measure
	#    click-to-adjudicate latency by watching any one of them change.
	# Wrap inside 1..65535: the sequence rides 16 bits of net_meta and is only ever COMPARED for change, so
	# it never needs to be globally ordered -- but it must never land on 0, which means "no order yet".
	_next_sequence = (_next_sequence % 65535) + 1
	var count: int = result.ids.size()
	for index: int in count:
		var unit: UnitBody = units[result.ids[index]]
		if unit == null:
			continue
		var goal: Vector3 = Formation.goal_for(index, count, result.point)
		unit.apply_order(result.verb, goal, _next_sequence)
	order_applied.emit(channel_seat, verb, count, _next_sequence)
	return OrderValidator.Code.OK
