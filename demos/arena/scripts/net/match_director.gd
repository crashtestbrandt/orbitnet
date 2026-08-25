extends Node3D
class_name MatchDirector
## Builds the arenas, owns every entity in them, and is the only place in this demo that declares interest.
##
## THREE ARENAS, ONE SESSION, AND EVERY ENTITY DECLARES WHICH ONE IT IS IN. That declaration is the demo:
## without it the three arenas are one, because the coordinates they replicate are arena-LOCAL and therefore
## identical. A radius cannot separate two fighters standing on the same spot in different worlds; a
## membership can, and nothing else in the facade can.
##
## A STATIC POOL, BUILT IDENTICALLY ON EVERY PEER. Entity ids are hashes of node paths, so creating fighters
## as players join would have the two peers build different node sets and therefore different ids -- nothing
## errors, the rows simply go nowhere. Every peer builds all 24 fighters, all the props and all the
## scorecards, and only OWNERSHIP moves at runtime.
##
## THE VETO PASS IS SERVER-SIDE AND DIFFERENTIAL. A cloaked fighter is withheld from the connections that must
## not see it with `Net.set_entity_hidden()`, and only the seats whose answer CHANGED are touched: starting a
## veto clears that peer's delta bookkeeping for the entity, so re-asserting it every tick would hold every
## withheld fighter permanently at "send a full block next".

signal world_built()

## How many net ticks between cloak-veto passes WHEN NOTHING CHANGED. A pass is every seat against every
## connection, and the answer changes a few times a minute, so a pass per tick would be spending real time to
## re-derive a set that is already correct.
##
## A CHANGE RUNS THE PASS IMMEDIATELY, and that is not an optimization in the other direction -- it is
## correctness. The tick a fighter cloaks is a tick whose row is about to be encoded, so a veto placed three
## ticks later has already let the cloak reach the peer it exists to hide it from. On the rollback lane that
## row carries the cloak FLAG, so the peer does not merely see a stale pose: it learns the fighter cloaked.
const VETO_REFRESH_TICKS: int = 12
## How close a fighter must be to the cloak spot to take it, meters.
const CLOAK_PICKUP_M: float = 1.6

var fighters: Array[FighterBody] = []
var props: Array[PropBody] = []
var scorecards: Array[Scorecard] = []
var roster: SeatRoster = null
var resolver: HitResolver = HitResolver.new()
## How deep the last few shots rewound, per band. Server-side, because the server is the only peer that
## resolves an authoritative shot.
var rewind: RewindMeter = RewindMeter.new()

var _fighters_root: Node3D = null
var _props_root: Node3D = null
var _scorecards_root: Node = null
var _arenas_root: Node3D = null
var _shots: NetCommand = null

## Per arena, the tick its cloak pickup becomes available again. Server-only.
var _cloak_ready_tick: PackedInt32Array = PackedInt32Array()
## Per connection, the seats currently withheld from it. Server-only, and the diff base for the veto pass.
var _hidden_of_peer: Dictionary[int, PackedInt32Array] = {}
var _veto_ticks: int = 0
var _veto_on: bool = true
## Who was cloaked at the last pass. A difference here is what forces a pass on the tick it happens.
var _cloak_mask: PackedByteArray = PackedByteArray()

var _prop_count: int = ArenaConfig.PROPS_PER_ARENA
var _built: bool = false
var _bound: bool = false
var _offline_accumulator: float = 0.0
var _offline_tick: int = 0

## The last shot resolved on this peer, for the readout. Server-side; a client learns about shots from the
## replicated shot sequence instead.
var _last_shot: HitResolver.Shot = null
# What batching has actually saved on this peer, for the HUD. Requests coalesced, and the packets they went in.
var _batched_requests: int = 0
var _batched_packets: int = 0

func _init() -> void:
	# The name is set at CONSTRUCTION, before this node is ever added to a tree. Every entity id under it is a
	# hash of a path that starts with this name, so renaming it after the children exist would silently re-key
	# the whole world.
	name = ArenaNames.WORLD_ROOT

# --- world build ---------------------------------------------------------------------------------------
## Build the world. `props_per_arena` overrides the configured count so a stress run can put real pressure on
## the entity slot table without a rebuild.
func build(seat_roster: SeatRoster, seat_owners: PackedInt32Array, props_per_arena: int = -1) -> void:
	if _built:
		return
	_built = true
	roster = seat_roster
	_prop_count = ArenaConfig.PROPS_PER_ARENA if props_per_arena < 0 else maxi(0, props_per_arena)

	_arenas_root = Node3D.new()
	_arenas_root.name = ArenaNames.ARENAS_ROOT
	add_child(_arenas_root)
	_build_cover()

	_fighters_root = Node3D.new()
	_fighters_root.name = ArenaNames.FIGHTERS_ROOT
	add_child(_fighters_root)

	_props_root = Node3D.new()
	_props_root.name = ArenaNames.PROPS_ROOT
	add_child(_props_root)

	_scorecards_root = Node.new()
	_scorecards_root.name = ArenaNames.SCORECARDS_ROOT
	add_child(_scorecards_root)

	# Fighters. configure() before add_child, because the NAME is part of the path the entity id is hashed
	# from. Registration is a SEPARATE pass -- see bind_net_all().
	fighters.resize(ArenaConfig.SEAT_COUNT)
	for seat: int in ArenaConfig.SEAT_COUNT:
		var owner_peer: int = seat_owners[seat] if seat < seat_owners.size() else 0
		var fighter: FighterBody = FighterBody.new()
		fighter.configure(seat, owner_peer)
		_fighters_root.add_child(fighter)
		fighters[seat] = fighter

	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		for index: int in _prop_count:
			var prop: PropBody = PropBody.new()
			prop.configure(arena, index)
			_props_root.add_child(prop)
			props.push_back(prop)
		var card: Scorecard = Scorecard.new()
		card.configure(arena)
		_scorecards_root.add_child(card)
		scorecards.push_back(card)

	_cloak_ready_tick.resize(ArenaConfig.ARENAS)
	_cloak_ready_tick.fill(0)

	_shots = _build_shot_channel()
	resolver.configure(fighters)

	world_built.emit()
	print("ARENA world built: %d arenas, %d fighters, %d props, sig=%d" % [
		ArenaConfig.ARENAS, fighters.size(), props.size(), world_signature()])

## Register every entity with the facade. A SEPARATE pass from build(), and the split is not cosmetic.
##
## `Net.make_state()` / `Net.register_rollback_body()` return INERT handles while the facade is OFFLINE --
## that is the contract that lets a single-player launch run the same code with no networking. So registration
## cannot happen until after `Net.set_mode()`, and `set_mode()` cannot happen until a peer is assigned. If
## build and bind were one call, a session would have to build its world after the socket was already live.
func bind_net_all() -> void:
	if not _built or _bound:
		return
	_bound = true
	for fighter: FighterBody in fighters:
		if fighter != null:
			fighter.bind_net()
	for prop: PropBody in props:
		if prop != null:
			prop.bind_net()
	for card: Scorecard in scorecards:
		if card != null:
			card.bind_net()
	print("ARENA world bound to the %s lane set (%d entities)" % [
		Net.mode_name(Net.current_mode()), fighters.size() + props.size() + scorecards.size()])
	print("ARENA-MARSHAL fighters_state=%d/%d fighters_input=%d/%d" % [
		bulk_counts().x, fighters.size(), bulk_counts().y, fighters.size()])

## Cover. STATIC bodies, and static is what makes them the live half of a lag-compensated cast: a wall is the
## same wall at every tick, so it never needs reconstructing from the rewind ring.
func _build_cover() -> void:
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		var root: Node3D = Node3D.new()
		root.name = ArenaNames.arena_node_name(arena)
		root.position = ArenaGeometry.origin_of(arena)
		_arenas_root.add_child(root)
		for index: int in ArenaConfig.COVER_PER_ARENA:
			var box: AABB = ArenaGeometry.cover_local(index)
			var body: StaticBody3D = StaticBody3D.new()
			body.name = "Cover%02d" % index
			body.collision_layer = ArenaConfig.LAYER_COVER
			body.collision_mask = 0
			body.position = box.get_center()
			var shape: CollisionShape3D = CollisionShape3D.new()
			shape.name = "Shape"
			var cube: BoxShape3D = BoxShape3D.new()
			cube.size = box.size
			shape.shape = cube
			body.add_child(shape)
			root.add_child(body)

func _build_shot_channel() -> NetCommand:
	var channel: NetCommand = NetCommand.new()
	channel.name = "Shots"
	channel.register(&"fire", _apply_shot)
	add_child(channel)
	return channel

## The signature of the world this peer built. Printed at build and asserted equal across peers by the probe:
## it is the direct gate on deterministic naming, and therefore on entity-id agreement.
func world_signature() -> int:
	var paths: PackedStringArray = PackedStringArray()
	for fighter: FighterBody in fighters:
		if fighter != null:
			paths.push_back(String(fighter.get_path()))
	for prop: PropBody in props:
		if prop != null:
			paths.push_back(String(prop.get_path()))
	for card: Scorecard in scorecards:
		if card != null:
			paths.push_back(String(card.get_path()))
	return ArenaNames.world_signature(paths)

# --- ownership -------------------------------------------------------------------------------------------
## Re-point a seat's fighter at a new owning connection, and say which of that connection's seats it is.
## Called on every peer when the roster changes, so authority agrees everywhere.
func set_seat_owner(seat: int, peer: int, seat_index: int) -> void:
	if seat < 0 or seat >= fighters.size():
		return
	var fighter: FighterBody = fighters[seat]
	if fighter != null:
		fighter.set_owner_peer(peer, seat_index)

# --- the authoritative step --------------------------------------------------------------------------
## Called once per net tick BEFORE the rollback loop, on the server.
##
## THE VETO PASS RUNS HERE, AND AFTER THE TICK IT DOES NOT WORK. A cloak is QUEUED between ticks and applied
## inside `advance()`, which is also where this tick's row is built from -- so a pass that ran after the loop
## would first see the flag on the tick whose row already carried it, and the peer the cloak is hidden from
## would learn about it exactly once. One tick of leak is the whole cloak.
##
## Running before the loop closes it, because the pass reads a PENDING cloak as well as an applied one: the
## veto is in force before the flag it hides has ever been written.
func pre_tick() -> void:
	if not Net.is_server():
		return
	_veto_pass()

## Called once per net tick AFTER the rollback loop, on the server. Every fighter has already advanced -- the
## backend called `_rollback_tick` on each -- so this is where the things that are NOT per-body happen.
##
## THE ORDER IS DELIBERATE. The rewind ring is recorded FIRST, from the poses this tick just produced, because
## a shot resolved later in the same tick rewinds into it. Kills are credited next, from what the tick decided,
## and cloaks are picked up last -- queued, for the pass at the top of the next tick to act on.
func post_tick(tick: int) -> void:
	if not Net.is_server():
		return
	resolver.record(tick)
	_credit_kills()
	_cloak_pass(tick)

## Credit every kill the tick just produced. Read from the fighters AFTER the tick rather than decided during
## the shot resolution, because whether a hit was fatal is decided INSIDE the tick -- see
## FighterBody._drain_pending(). The scorecard is on the STATE lane, so writing it here is safe; on the
## rollback lane the next restore would put the score back.
func _credit_kills() -> void:
	for fighter: FighterBody in fighters:
		if fighter == null:
			continue
		var by_seat: int = fighter.take_kill_credit()
		if by_seat < 0:
			continue
		var killer: FighterBody = fighter_at(by_seat)
		var card: Scorecard = _scorecard_of(fighter.arena_id)
		if killer != null and card != null:
			card.credit(killer.team, by_seat)

func _physics_process(delta: float) -> void:
	if not _built or not Net.is_offline():
		return
	# OFFLINE the net tick loop does not run, so the simulation is paced by a fixed accumulator at exactly the
	# same dt the networked path uses. One advance() body, two clocks -- so "it behaves differently offline"
	# cannot happen quietly.
	_offline_accumulator += delta
	var guard: int = 0
	while _offline_accumulator >= ArenaConfig.NET_TICK_DT and guard < 4:
		_offline_accumulator -= ArenaConfig.NET_TICK_DT
		guard += 1
		_offline_tick += 1
		# No veto pass offline: there is no peer to withhold anything from, and `Net.set_entity_hidden()` is a
		# no-op there anyway. The cloak still applies, so the local player still turns green.
		for fighter: FighterBody in fighters:
			if fighter != null:
				# `is_fresh` is unconditionally true offline: there is no rollback loop, so no tick is ever
				# replayed and every one of them is the first and only time it runs.
				fighter.advance(ArenaConfig.NET_TICK_DT, _offline_tick, true)
		_credit_kills()
		_cloak_pass(_offline_tick)
	if guard >= 4:
		# A long stall (a breakpoint, a window drag) must not turn into a burst of catch-up ticks; drop the
		# backlog instead. The networked path gets the same protection from the tick clock itself.
		_offline_accumulator = 0.0

## The tick this director is running at: the session's when there is one, its own accumulator's when offline.
func current_tick() -> int:
	return _offline_tick if Net.is_offline() else Net.current_tick()

# --- cloaks ------------------------------------------------------------------------------------------
func _cloak_pass(tick: int) -> void:
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		if tick < _cloak_ready_tick[offset]:
			continue
		var spot: Vector3 = ArenaGeometry.cloak_local()
		var first: int = ArenaConfig.first_seat_of_arena(arena)
		for step: int in ArenaConfig.SEATS_PER_ARENA:
			var fighter: FighterBody = fighters[first + step]
			if fighter == null or not fighter.is_alive() or fighter.is_cloaked():
				continue
			if fighter.net_pos.distance_to(spot) > CLOAK_PICKUP_M:
				continue
			if fighter.queue_cloak():
				_cloak_ready_tick[offset] = tick + ArenaConfig.CLOAK_RESPAWN_TICKS
				print("ARENA: seat %d cloaked in arena %d" % [fighter.seat, arena])
				break

## Whether an arena's cloak pickup is available right now.
func cloak_available(arena_id: int) -> bool:
	var offset: int = arena_id - ArenaConfig.FIRST_ARENA_ID
	if offset < 0 or offset >= _cloak_ready_tick.size():
		return false
	return current_tick() >= _cloak_ready_tick[offset]

# --- the veto ----------------------------------------------------------------------------------------
## Turn the cloak veto on or off. SERVER-SIDE: on a client this changes nothing, because a client cannot
## decide what it is allowed to receive, which is the whole security property the veto has.
func set_veto(on: bool) -> void:
	if _veto_on == on:
		return
	_veto_on = on
	if not on:
		_retract_every_veto()
	_veto_ticks = 0
	# Forget the mask with the vetoes, so switching the fog back on re-derives the whole set rather than
	# diffing against what was true before it was switched off.
	_cloak_mask = PackedByteArray()

func veto_enabled() -> bool:
	return _veto_on

## How many fighters are currently withheld from `peer`.
func hidden_count(peer: int) -> int:
	var seats: PackedInt32Array = _hidden_of_peer.get(peer, PackedInt32Array())
	return seats.size()

## How many fighters are withheld from anybody at all.
func hidden_total() -> int:
	var total: int = 0
	for peer: int in _hidden_of_peer.keys():
		# Through a typed local: `Dictionary` indexing answers a Variant, and calling a method on one is a
		# parse error under this project's promoted warnings. Assigning is the conversion that is allowed.
		var seats: PackedInt32Array = _hidden_of_peer[peer]
		total += seats.size()
	return total

## Recompute what each connection must not receive, and move the vetoes to match.
##
## PER CONNECTION, NOT PER SEAT, because a veto refuses a row in a DATAGRAM and a datagram is per connection.
## A split-screen player's two seats share one, so the teams of both are folded together before the policy is
## asked -- a connection with a seat on each team may see both teams' cloaks, and blinding one half of its
## screen for the other half's benefit would be wrong.
func _veto_pass() -> void:
	if not _veto_on:
		return
	var teams: PackedInt32Array = PackedInt32Array()
	var cloaked: PackedByteArray = PackedByteArray()
	teams.resize(fighters.size())
	cloaked.resize(fighters.size())
	for seat: int in fighters.size():
		var fighter: FighterBody = fighters[seat]
		teams[seat] = 0 if fighter == null else fighter.team
		# A PENDING CLOAK COUNTS. It is applied inside the tick this pass runs at the top of, so treating it as
		# already in force is what puts the veto in place before the flag exists to be leaked. A queued cloak
		# that never applies -- the fighter died in between -- costs one withheld tick and then retracts.
		var hidden: bool = fighter != null and fighter.is_alive() \
			and (fighter.is_cloaked() or fighter.cloak_pending())
		cloaked[seat] = 1 if hidden else 0

	# Cheap: 24 bytes compared per tick, against a pass that is every seat times every connection. The
	# comparison is what lets the pass itself be slow.
	_veto_ticks += 1
	var changed: bool = cloaked != _cloak_mask
	if not changed and _veto_ticks < VETO_REFRESH_TICKS:
		return
	_veto_ticks = 0
	_cloak_mask = cloaked

	for peer: int in multiplayer.get_peers():
		var viewer_teams: PackedInt32Array = PackedInt32Array()
		for seat: int in roster.seats_of_peer(peer):
			var team: int = ArenaConfig.team_of_seat(seat)
			if not viewer_teams.has(team):
				viewer_teams.push_back(team)
		var current: PackedInt32Array = CloakPolicy.hidden_seats(viewer_teams, teams, cloaked)
		var previous: PackedInt32Array = _hidden_of_peer.get(peer, PackedInt32Array())
		for seat: int in CloakPolicy.changes(previous, current):
			var fighter: FighterBody = fighters[seat]
			if fighter != null:
				Net.set_entity_hidden(peer, fighter.entity_id(), current.has(seat))
		_hidden_of_peer[peer] = current

## Hand every withheld fighter back, then forget. Both halves are required: retracting without clearing leaves
## the diff base claiming fighters are hidden that are not, and clearing without retracting strands live
## vetoes the demo can no longer name.
func _retract_every_veto() -> void:
	for peer: int in _hidden_of_peer.keys():
		var seats: PackedInt32Array = _hidden_of_peer[peer]
		for seat: int in seats:
			var fighter: FighterBody = fighters[seat]
			if fighter != null:
				Net.set_entity_hidden(peer, fighter.entity_id(), false)
	_hidden_of_peer.clear()

## Forget one connection's vetoes without retracting them. Called when that peer drops: the backend drops a
## departed peer's vetoes with its connection, so retracting here would be naming a peer that is gone.
func forget_peer(peer: int) -> void:
	_hidden_of_peer.erase(peer)

# --- shots -------------------------------------------------------------------------------------------
## Ask to fire one seat. Called on the peer holding `seat`; the server adjudicates.
func request_shot(seat: int) -> void:
	if _shots == null:
		return
	_shots.request(&"fire", _shot_payload(seat))

## Ask to fire several seats in ONE reliable packet.
##
## THIS IS WHY A SPLIT-SCREEN CONNECTION COSTS ONE PACKET AND NOT TWO. Both fighters behind one connection
## fire in the same frame on the same channel, so sending them separately spends two lots of RPC framing,
## reliable-command headers, acks and retransmit state to carry two 24-byte payloads. A batch is a coalescing
## optimization and NOT a transaction: each seat is validated on its own, so seat 0 can be admitted in the
## same packet that refuses seat 1 for cooling, and the refusals come back together.
func request_shots(seats: PackedInt32Array) -> void:
	if _shots == null or seats.is_empty():
		return
	if seats.size() == 1:
		_shots.request(&"fire", _shot_payload(seats[0]))
		return
	var payloads: Array = []
	for seat: int in seats:
		payloads.push_back(_shot_payload(seat))
	_shots.request_batch(&"fire", payloads)
	_batched_requests += payloads.size()
	_batched_packets += 1

func _shot_payload(seat: int) -> Dictionary:
	return {
		ShotValidator.KEY_SEAT: seat,
		ShotValidator.KEY_TICK: current_tick(),
	}

## How many shot requests this peer has coalesced, and into how many packets. Both 0 on a connection driving
## one seat, which is the honest reading: there is nothing to coalesce.
func batched_requests() -> int:
	return _batched_requests

func batched_packets() -> int:
	return _batched_packets

## The shot channel, so a HUD can hear its refusals. [signal NetCommand.rejected] fires on the peer that
## refused the shot AND on the client that asked, which is what turns a dead trigger into a stated reason.
func shot_channel() -> NetCommand:
	return _shots

## The server-side validator AND applier for a shot, in one place, so an unvalidated request can never reach
## the state.
##
## THE SEAT COMES OUT OF THE PAYLOAD AND IS THEREFORE A CLAIM. `sender_id` is the only identity a client
## cannot author, and a connection here may drive two fighters -- so the seat has to travel in the payload and
## has to be checked against the seats the SERVER assigned to that sender. That check is the whole security
## model of this lane.
func _apply_shot(sender_id: int, payload: Dictionary) -> int:
	# Read through typed locals: a payload crosses the RPC boundary as a plain Dictionary, so its values are
	# Variants, and assigning is the conversion this project allows where a cast is not.
	var seat_value: Variant = payload.get(ShotValidator.KEY_SEAT, -1)
	var tick_value: Variant = payload.get(ShotValidator.KEY_TICK, -1)
	var seat: int = seat_value
	var asked_tick: int = tick_value
	var present: int = current_tick()
	var fighter: FighterBody = fighters[seat] if seat >= 0 and seat < fighters.size() else null
	var verdict: ShotValidator.Verdict = ShotValidator.check(
		roster, sender_id, seat,
		fighter != null and fighter.is_alive(),
		fighter.last_shot_tick() if fighter != null else -1,
		present)
	if verdict != ShotValidator.Verdict.OK:
		# Returning the verdict rather than `false` is what carries the reason back to the peer that asked:
		# NetCommand replies with an int verdict and announces nothing for a bool. `Verdict.OK` is 0, so the
		# acceptance path below reads exactly as it did.
		return verdict as int

	fighter.queue_shot(present)
	var at_tick: int = ShotValidator.clamp_command_tick(
		asked_tick, present, NetLagComp.retain_ticks(ArenaConfig.NET_TICK_HZ))
	if at_tick < 0:
		at_tick = present
	var space: PhysicsDirectSpaceState3D = _space_state()
	if space == null:
		return ShotValidator.Verdict.OK as int
	var is_authority_shooter: bool = sender_id <= SeatRoster.SERVER_PEER
	var rtt_ms: float = -1.0 if is_authority_shooter else Net.peer_rtt_ms(sender_id)
	var shot: HitResolver.Shot = resolver.resolve(space, fighter, sender_id, rtt_ms, present,
		is_authority_shooter)
	_last_shot = shot
	rewind.note(shot, present)
	if shot.hit_seat < 0:
		return ShotValidator.Verdict.OK as int
	var struck: FighterBody = fighters[shot.hit_seat]
	if struck == null or struck.team == fighter.team:
		return ShotValidator.Verdict.OK as int
	# QUEUED, NOT WRITTEN. This handler runs OUTSIDE the tick, and the health it would write lives on the
	# ROLLBACK lane -- the next restore would put it back and the fighter would never die. Whether the hit is
	# fatal is therefore decided inside the tick, and `_credit_kills()` reads the answer afterward.
	struck.queue_damage(ArenaConfig.SHOT_DAMAGE, fighter.seat)
	return ShotValidator.Verdict.OK as int

func _space_state() -> PhysicsDirectSpaceState3D:
	var world: World3D = get_world_3d()
	return null if world == null else world.direct_space_state

func _scorecard_of(arena_id: int) -> Scorecard:
	var offset: int = arena_id - ArenaConfig.FIRST_ARENA_ID
	if offset < 0 or offset >= scorecards.size():
		return null
	return scorecards[offset]

## The last shot this peer resolved, for the readout. Null until one has been.
func last_shot() -> HitResolver.Shot:
	return _last_shot

# --- bulk marshalling --------------------------------------------------------------------------------
## Turn bulk marshalling on or off across every fighter at once.
##
## NOTHING ABOUT A HOOK REACHES THE WIRE. The row, the mask, the delta base and the mispredict compare all
## read the backend's own layout, so this can be flipped mid-session on one peer while another keeps walking
## its properties, and neither notices anything about the other.
func set_bulk_marshalling(on: bool) -> void:
	for fighter: FighterBody in fighters:
		if fighter != null:
			fighter.set_bulk_marshalling(on)

## How many fighters have each lane marshalling in bulk: state in x, input in y. Asked of the backend rather
## than tracked here, so a readout cannot claim a hook that failed to resolve.
func bulk_counts() -> Vector2i:
	var counts: Vector2i = Vector2i.ZERO
	for fighter: FighterBody in fighters:
		if fighter == null:
			continue
		if fighter.uses_bulk_state():
			counts.x += 1
		if fighter.uses_bulk_input():
			counts.y += 1
	return counts

# --- reads -------------------------------------------------------------------------------------------
func fighter_at(seat: int) -> FighterBody:
	return fighters[seat] if seat >= 0 and seat < fighters.size() else null

func scorecard_of(arena_id: int) -> Scorecard:
	return _scorecard_of(arena_id)

func prop_count() -> int:
	return props.size()
