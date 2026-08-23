extends Node3D
class_name RinkDirector
## Builds the rink, owns the mallet pool and the puck, and adjudicates serve requests.
##
## A STATIC MALLET POOL, NOT SPAWN/DESPAWN. Every peer creates all HockeyConfig.SEATS mallet nodes at world
## build, with identical names, and the node set NEVER changes afterwards. Seating sets a mallet's owning peer;
## leaving clears it. That is a deliberate departure from "the server queue_free()s a mallet when its player
## quits", and the reason is entity identity:
##
##   OrbitNet derives an entity id from its synchronizer root's NODE PATH. Freeing and re-creating a node means
##   re-creating it at exactly the same path on every peer, in the same order, or the ids diverge and
##   replication silently goes nowhere. Doing that correctly needs a spawn-replication mechanism -- a real and
##   interesting problem, and completely the wrong one for a demo about predicting a shared object to also be
##   about. A fixed pool makes path agreement true by construction.
##
##   It also costs almost nothing: a vacant seat's mallet is parked, never drawn, skipped by the puck's
##   collision pass, and its properties stop changing -- so the delta tracking stops sending it. An empty table
##   is free.
##
## THIS NODE RUNS NO SIMULATION WHEN NETWORKED. Everything here is on the rollback lane, so the backend drives
## every body through `_rollback_tick` and there is no per-tick server step to write -- the exact opposite of
## the RTS demo, whose units are on the state lane and are stepped by hand from `Net.pre_tick`. The only step
## loop below is the OFFLINE accumulator.

## Emitted once the node graph exists. HockeyNet builds the world before it binds the transport.
signal world_built()
## Emitted on the peer that applied a serve.
signal serve_applied(seat: int)
## Emitted on the peer that refused one, with the validator's reason. Rejections are the interesting half: a
## demo that never shows you a refused request has not shown you the security model.
signal serve_rejected(seat: int, reason: String)
## Emitted on EVERY peer when the replicated goal sequence moves. The state lane carries the event; this turns
## it into a signal the view can flash on, exactly once.
signal goal_scored(team: int, sequence: int)

var mallets: Array[MalletBody] = []
var puck: PuckBody = null
var scoreboard: Scoreboard = null
var roster: TeamRoster = null

var _mallets_root: Node3D = null
var _serve_channel: NetCommand = null
var _built: bool = false
var _bound: bool = false
var _offline_accumulator: float = 0.0
var _offline_tick: int = 0
var _seen_goal_sequence: int = 0

func _init() -> void:
	# The name is set at CONSTRUCTION, before this node is ever added to a tree. Every entity id under it is a
	# hash of a path that starts with this name, so renaming it after the children exist would silently re-key
	# the whole world.
	name = HockeyNames.RINK_ROOT

# --- world build -----------------------------------------------------------------------------------
## Build the rink. `seat_owners` maps seat index -> peer id (0 where a seat is empty); it must be the SAME on
## every peer, which is why a client builds with an all-zero table and is corrected by the roster broadcast.
func build(seat_roster: TeamRoster, seat_owners: PackedInt32Array) -> void:
	if _built:
		return
	_built = true
	roster = seat_roster

	_mallets_root = Node3D.new()
	_mallets_root.name = HockeyNames.MALLETS_ROOT
	add_child(_mallets_root)

	# Mallets. configure() before add_child, because the NAME is part of the path the entity id is hashed from.
	# Registration is a SEPARATE pass -- see bind_net_all().
	mallets.resize(HockeyConfig.SEATS)
	for seat: int in HockeyConfig.SEATS:
		var owner_peer: int = seat_owners[seat] if seat < seat_owners.size() else 0
		var mallet: MalletBody = MalletBody.new()
		mallet.configure(seat, owner_peer)
		_mallets_root.add_child(mallet)
		mallets[seat] = mallet

	scoreboard = Scoreboard.new()
	add_child(scoreboard)

	puck = PuckBody.new()
	puck.configure(mallets, scoreboard)
	add_child(puck)

	# ONE serve channel for the whole rink, not one per seat. A NetCommand routes by node path, and the RTS
	# demo's per-seat channels earn their keep because an order names unit ids -- a request on someone else's
	# channel is forgery, catchable before the payload is parsed. A serve names nothing, so the sender id is
	# the entire authorization and thirty-two channels would be thirty-two nodes checking nothing.
	_serve_channel = NetCommand.new()
	_serve_channel.name = HockeyNames.SERVE_NODE
	add_child(_serve_channel)
	_serve_channel.register(ServeValidator.VERB_SERVE, _apply_serve)

	world_built.emit()
	print("HOCKEY world built: %d mallets, 1 puck, sig=%d" % [mallets.size(), world_signature()])

## Register every entity with the facade. A SEPARATE pass from build(), and the split is not cosmetic.
##
## `Net.make_state()` / `Net.register_rollback_body()` return INERT handles while the facade is OFFLINE -- that
## is the contract that lets a single-player launch run the same code with no networking. Which means
## registration cannot happen until after `Net.set_mode()`, and `set_mode()` cannot happen until a peer is
## assigned. If build and bind were one call, a session would have to build its world after the socket was
## already live, re-opening the window HockeyNet's rule 1 exists to close.
func bind_net_all() -> void:
	if not _built or _bound:
		return
	_bound = true
	for mallet: MalletBody in mallets:
		if mallet != null:
			mallet.bind_net()
	scoreboard.bind_net()
	puck.bind_net()
	# The marshalling state is printed at bind rather than left to the HUD, and the reason is that a hook is
	# resolved by NAME: a rename that misses a call site leaves the lane on the per-property walk with nothing
	# erroring. One greppable line at boot turns "is it actually on" into a question with an answer, on a
	# dedicated server that draws no HUD as much as on a client that does.
	var bulk: Vector2i = bulk_mallet_counts()
	print("HOCKEY rink bound to the %s lane set (%d rollback bodies, 1 state channel)" % [
		Net.mode_name(Net.current_mode()), mallets.size() + 1])
	print("HOCKEY-MARSHAL puck=%s mallets_state=%d/%d mallets_input=%d/%d" % [
		"bulk" if uses_bulk_marshalling() else "walk", bulk.x, mallets.size(), bulk.y, mallets.size()])

## Re-point a seat's mallet at a new owning peer, and re-evaluate prediction. Called on every peer when the
## roster changes, so authority agrees everywhere.
func set_seat_owner(seat: int, peer: int) -> void:
	if seat < 0 or seat >= mallets.size():
		return
	var mallet: MalletBody = mallets[seat]
	if mallet != null:
		mallet.set_owner_peer(peer)

## The signature of the world this peer built -- see HockeyNames.world_signature(). Printed at build and
## compared between peers by hand: it is the direct gate on deterministic naming, and therefore on entity-id
## agreement.
## Turn bulk marshalling on or off across every rollback body at once. The lever behind F7.
##
## NOTHING ABOUT A HOOK REACHES THE WIRE. The row, the mask, the delta base and the mispredict compare all
## read the backend's own layout, so this can be flipped mid-session on one peer while another keeps walking
## its properties, and neither notices anything about the other.
func set_bulk_marshalling(on: bool) -> void:
	for mallet: MalletBody in mallets:
		if mallet != null:
			mallet.set_bulk_marshalling(on)
	if puck != null:
		puck.set_bulk_marshalling(on)

## Whether the puck's state lane is marshalling in bulk right now. Asked of the backend rather than tracked
## here, so the readout cannot claim a hook that failed to resolve.
func uses_bulk_marshalling() -> bool:
	return puck != null and puck.uses_bulk_marshalling()

## How many of the pool's mallets have each lane marshalling in bulk: state in x, input in y. The input half
## is the one worth counting separately -- its entry lives on a child node while the hook resolves on the
## root, so it is the half that can quietly stay on the walk.
func bulk_mallet_counts() -> Vector2i:
	var counts: Vector2i = Vector2i.ZERO
	for mallet: MalletBody in mallets:
		if mallet == null:
			continue
		if mallet.uses_bulk_marshalling():
			counts.x += 1
		if mallet.uses_bulk_input_marshalling():
			counts.y += 1
	return counts

func world_signature() -> int:
	var paths: PackedStringArray = PackedStringArray()
	for mallet: MalletBody in mallets:
		if mallet != null:
			paths.push_back(String(mallet.get_path()))
	if puck != null:
		paths.push_back(String(puck.get_path()))
	if scoreboard != null:
		paths.push_back(String(scoreboard.get_path()))
	return HockeyNames.world_signature(paths)

## How many seats each team currently has filled, as (team 0, team 1).
func team_counts() -> Vector2i:
	var counts: Vector2i = Vector2i.ZERO
	for mallet: MalletBody in mallets:
		if mallet == null or not mallet.is_occupied():
			continue
		if mallet.team() == 0:
			counts.x += 1
		else:
			counts.y += 1
	return counts

# --- the offline clock -----------------------------------------------------------------------------
func _physics_process(delta: float) -> void:
	if not _built:
		return
	_watch_goal()
	if not Net.is_offline():
		return
	# OFFLINE the net tick loop does not run, so the sim is paced by a fixed accumulator at exactly the same dt
	# the networked path uses. One advance() per body, two clocks -- so "it behaves differently offline" cannot
	# happen quietly.
	_offline_accumulator += delta
	var step: float = 1.0 / float(HockeyConfig.NET_TICK_HZ)
	var guard: int = 0
	while _offline_accumulator >= step and guard < 4:
		_offline_accumulator -= step
		guard += 1
		_offline_tick += 1
		_advance_all(step, _offline_tick)
	if guard >= 4:
		# A long stall (a breakpoint, a window drag) must not turn into a burst of catch-up ticks; drop the
		# backlog instead. The networked path gets the same protection from the tick clock itself.
		_offline_accumulator = 0.0

# Mallets first, then the puck. Offline there is no second peer to agree with, so the order is simply the
# natural one; networked, the backend replays in ascending entity id and PuckBody's header records why that is
# stable across peers.
func _advance_all(delta: float, tick: int) -> void:
	for mallet: MalletBody in mallets:
		if mallet != null:
			mallet.advance(delta)
	if puck != null:
		# Offline is its own authority, so every tick is fresh: nothing is ever replayed.
		puck.advance(delta, tick, true)

# The state lane carries the goal as a sequence number that only ever needs to be COMPARED for change. Watching
# it here turns it into one signal per goal on every peer, with no reliable event channel to build.
func _watch_goal() -> void:
	if scoreboard == null:
		return
	var sequence: int = scoreboard.last_sequence()
	if sequence == _seen_goal_sequence:
		return
	_seen_goal_sequence = sequence
	if sequence > 0:
		goal_scored.emit(scoreboard.last_scorer(), sequence)

# --- serves ----------------------------------------------------------------------------------------
## Ask for a serve on the local player's behalf. On a client this becomes a reliable RPC to the server; on a
## host or offline it applies immediately through the same path.
func submit_serve() -> void:
	if _serve_channel != null:
		_serve_channel.request(ServeValidator.VERB_SERVE, {})

# The server-side validator+applier. Runs ONLY on the applying peer (server, or the local peer offline).
# Everything a client sent is suspect until this returns.
#
# `payload` is untyped on purpose: it crosses the @rpc boundary, where Godot decodes it as a PLAIN Dictionary,
# and a Dictionary[String, Variant] annotation would reject the wire-decoded value. This verb carries no
# fields, so there is nothing to read out of it -- which is itself the reason one channel is enough.
func _apply_serve(sender_id: int, _payload: Dictionary) -> bool:
	if puck == null or roster == null:
		return false
	# WHO is asking comes from the sender id, never from the payload.
	var seat: int = roster.seat_for_sender(sender_id)
	var result: ServeValidator.Result = ServeValidator.validate(
		seat, puck.is_at_rest(), puck.faceoff_ticks())
	if not result.accepted:
		serve_rejected.emit(seat, result.reason)
		return false
	puck.request_serve()
	serve_applied.emit(seat)
	return true
