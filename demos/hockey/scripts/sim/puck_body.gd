extends Node3D
class_name PuckBody
## The puck: a rollback body with NO INPUT, predicted on every peer. This is the demo.
##
## THE UNFAMILIAR HALF OF THE ROLLBACK LANE. Every rollback example is a player's own avatar -- you author the
## input, you predict the result, you reconcile. The puck is authored by nobody. It is registered with an EMPTY
## input list and `predict = true` on every peer, and the backend's own roles fall out:
##
##   Server   -- owns the state, and an entity with no input props is stamped authoritative every tick, so its
##               simulation is the truth by construction.
##   Client   -- owns neither the state nor the input, but prediction is enabled and it is not exempt, so it
##               dead-reckons the puck THROUGH THE REAL SIMULATION and reconciles when the server's row lands.
##
## That is what makes a strike feel instant: your mallet is predicted locally, the puck it hits is predicted
## locally, and the pair of them answer before any packet leaves. What the server sends back is a correction,
## and how big that correction is, in millimetres, is the number this demo exists to show you.
##
## WHY THE CLIENT MISPREDICTS AT ALL. It has its own mallet's input and none of anybody else's -- rollback
## input goes client -> server and is never rebroadcast -- so an opponent's strike is unpredictable in the
## strict sense: nothing in the client's possession implies it. `Net.set_remote_resim(true)` at least lets the
## other mallets coast forward through the resim instead of standing still, which is why HockeyNet turns it on
## and why the F2 lever is worth pressing.
##
## WITHIN-TICK ORDERING. The backend replays rollback entities in ASCENDING ENTITY ID -- its planner keeps them
## in a BTreeMap precisely so replay order cannot vary run to run -- and an entity id is FNV-1a of the node
## path, so the order is identical on every peer and is already gated by the world signature. The puck
## therefore reads each mallet either at the start or the end of the tick depending on that fixed order,
## consistently everywhere; at 60 Hz the difference is under a centimetre. Mallets never write puck state, so
## there is exactly one direction of cross-entity read and it is this one.

## STATE, replicated: the puck's pose. Registered first, and a Vector3, so the interest anchor would be a real
## world position if this demo ever turned interest management on.
var net_pos: Vector3 = Vector3.ZERO
## STATE, replicated: the puck's velocity. NOT an optimisation -- a restore that returned position without
## velocity would resume the resim from the wrong basis and diverge on the very next tick.
var net_vel: Vector3 = Vector3.ZERO
## STATE, replicated: liveness, the face-off countdown, the serve sequence and the end being served toward, in
## one i64. All of it is read by the simulation, so all of it has to ride the lane that gets restored.
var net_flags: int = 0

# --- flag packing ----------------------------------------------------------------------------------
# Static so the tests can exercise the packing directly with no node and no session.
const _FACEOFF_BITS: int = 10
const _SEQ_BITS: int = 16
const _FACEOFF_MASK: int = (1 << _FACEOFF_BITS) - 1
const _SEQ_MASK: int = (1 << _SEQ_BITS) - 1
const _TO_TEAM_BIT: int = 1 << (_FACEOFF_BITS + _SEQ_BITS)
const _LIVE_BIT: int = 1 << (_FACEOFF_BITS + _SEQ_BITS + 1)

# The memo key for "a serve was requested and consumed at this tick". See _consume_serve_request().
const _MEMO_SERVE: int = 1

static func pack_flags(live: bool, faceoff: int, sequence: int, to_team: int) -> int:
	var out: int = clampi(faceoff, 0, _FACEOFF_MASK)
	out |= (sequence & _SEQ_MASK) << _FACEOFF_BITS
	if to_team == 1:
		out |= _TO_TEAM_BIT
	if live:
		out |= _LIVE_BIT
	return out

static func flags_live(flags: int) -> bool:
	return (flags & _LIVE_BIT) != 0

static func flags_faceoff(flags: int) -> int:
	return flags & _FACEOFF_MASK

static func flags_sequence(flags: int) -> int:
	return (flags >> _FACEOFF_BITS) & _SEQ_MASK

static func flags_to_team(flags: int) -> int:
	return 1 if (flags & _TO_TEAM_BIT) != 0 else 0

# --- wiring ----------------------------------------------------------------------------------------
var mallets: Array[MalletBody] = []
var scoreboard: Scoreboard = null

var _handle: NetRollbackHandle = null
var _meter: ReconcileMeter = ReconcileMeter.new()
var _mallet_pos: PackedVector3Array = PackedVector3Array()
var _mallet_vel: PackedVector3Array = PackedVector3Array()
# Server-only, never replicated: a validated serve request waiting for the next tick to consume it.
var _serve_pending: bool = false
# Rail and mallet contacts resolved by the most recent step. Presentation-only and deliberately NOT replicated:
# the view reads it to tell an intended discontinuity from a correction, and a wrong answer costs one frame of
# smoothing rather than a wrong puck.
var _contacts: int = 0
# The tick this puck last simulated, in WHICHEVER clock is driving it. Net.current_tick() is pinned at 0
# OFFLINE, so a consumer that needs to know when a tick actually advanced -- the view, to tell a correction
# from ordinary motion -- cannot ask the facade and asks here instead.
var _sim_tick: int = -1
# The tick after which a correction counts as DRIFT. See _measures().
var _armed_after: int = -1

func _init() -> void:
	name = HockeyNames.PUCK_NODE

## Configure the puck's view of the world. Called BEFORE bind_net; the mallet list is read every tick.
func configure(pool: Array[MalletBody], record: Scoreboard) -> void:
	mallets = pool
	scoreboard = record
	net_pos = TableGeometry.centre_spot()
	net_vel = Vector3.ZERO
	position = net_pos
	# Dead with a countdown running, so a fresh world serves itself rather than waiting for someone to ask.
	net_flags = pack_flags(false, HockeyConfig.FACEOFF_TICKS, 0, 0)

## Register the rollback lane. Called AFTER the puck is in the tree at its final path.
##
## The puck is its OWN input node. There is no input to carry, and a child node that never holds a property
## would still be a path every peer had to build identically -- so the root stands in, the input list is empty,
## and `predict = true` says the thing the demo is about in the call itself.
func bind_net() -> void:
	if Net.is_offline():
		_handle = Net.register_rollback_body(self, self, [], [], false, [])
		return
	_handle = Net.register_rollback_body(
		self,
		self,
		["net_pos@half", "net_vel@half", "net_flags"],
		[],      # NO INPUT -- nobody authors the puck
		true)    # predicted on EVERY peer, which is the whole demo

## Accept a validated serve request. Called on the server (or offline) from the NetCommand handler, i.e.
## OUTSIDE the tick, which is why it writes a plain server-only field rather than any replicated property.
func request_serve() -> void:
	_serve_pending = true

## One simulation step. Called by the backend as `_rollback_tick` when networked, and by RinkDirector's offline
## accumulator when there is no session.
func advance(delta: float, tick: int, is_fresh: bool) -> void:
	var live: bool = flags_live(net_flags)
	var was_live: bool = live
	var faceoff: int = flags_faceoff(net_flags)
	var sequence: int = flags_sequence(net_flags)
	var to_team: int = flags_to_team(net_flags)

	if live:
		_refresh_mallet_mirrors()
		var next: PuckPhysics.State = PuckPhysics.step(
			PuckPhysics.State.new(net_pos, net_vel), _mallet_pos, _mallet_vel, delta)
		net_pos = next.position
		net_vel = next.velocity
		_contacts = next.contacts
		var scorer: int = TableGeometry.scoring_team_at(net_pos.z)
		if scorer >= 0:
			# ONE-SHOT, GATED ON is_fresh. The ledger consumes freshness exactly once per tick, so a resim over
			# this same tick does not award the goal again. The known cost, stated rather than hidden: a goal
			# committed on the fresh pass is NOT un-awarded if a later correction invalidates the tick.
			# NetRollbackHandle.memo_set/memo_get is the primitive for that case and this demo does not use it
			# for the score -- at a 60 Hz coupled tick the window is a handful of milliseconds wide.
			if is_fresh and scoreboard != null and (Net.is_server() or Net.is_offline()):
				scoreboard.award(scorer)
			live = false
			faceoff = HockeyConfig.FACEOFF_TICKS
			sequence = (sequence % _SEQ_MASK) + 1
			to_team = 1 - scorer     # serve toward the team that conceded
			net_pos = TableGeometry.centre_spot()
			net_vel = Vector3.ZERO
	else:
		net_pos = TableGeometry.centre_spot()
		net_vel = Vector3.ZERO
		_contacts = 0
		if faceoff > 0:
			faceoff -= 1
		# Consumed BEFORE the countdown is tested, not short-circuited after it. Behind an `or` a request that
		# arrived on the tick the countdown expired anyway would be left pending, and would then serve the puck
		# the instant it next died -- a serve nobody asked for, one goal later.
		var requested: bool = _consume_serve_request(tick, is_fresh)
		if faceoff <= 0 or requested:
			live = true
			faceoff = 0
			net_vel = PuckPhysics.serve_velocity(to_team, sequence)

	net_flags = pack_flags(live, faceoff, sequence, to_team)
	position = net_pos
	_sim_tick = tick
	# Measured on EVERY peer, including the server. A client's correction comes from an opponent's strike it
	# could not know about; the server's comes from a client's input landing late enough that it had already
	# simulated the tick without it. Same quantity, same cause -- somebody simulated a tick before the truth
	# about it arrived.
	if _measures(tick, was_live, live):
		_meter.note(tick, net_pos)

func _rollback_tick(delta: float, tick: int, is_fresh: bool) -> void:
	advance(delta, tick, is_fresh)

## The correction meter, for the HUD and the bench subject.
func meter() -> ReconcileMeter:
	return _meter

## Whether the puck is live (in play), on any peer.
func is_live() -> bool:
	return flags_live(net_flags)

## Net ticks left on the face-off countdown, on any peer.
func faceoff_ticks() -> int:
	return flags_faceoff(net_flags)

## The tick this puck last simulated, under either clock. See `_sim_tick`.
func sim_tick() -> int:
	return _sim_tick

## Rail or mallet contacts in the most recent step. The view blends a correction and snaps a bounce, and this
## is what tells the two apart.
func contacts() -> int:
	return _contacts

## Whether the puck is slow enough for a serve to be legal -- the validator's other precondition.
func is_at_rest() -> bool:
	return PuckPhysics.is_at_rest(net_vel)

# --- internals -------------------------------------------------------------------------------------
# Whether this tick's answer belongs in the correction distribution.
#
# The meter measures DRIFT: how far a PREDICTED puck moved away from the authoritative one while both were
# simulating the same live puck. Two things are not drift, and both were large enough to be the entire number
# before they were excluded:
#
#   THE JOIN SYNC. A client builds its own puck and predicts it forward before any authoritative row has
#   arrived. The first row rewinds it onto a puck that was somewhere else entirely -- a third of a metre is
#   typical, and it is a state TRANSFER rather than a mispredicted simulation. It happens exactly once per
#   session, but a percentile window holds it for the rest of the run: measured on an otherwise untouched
#   puck, it was the only sample ever recorded, so p50, p99 and peak all reported that one join.
#
#   A FACE-OFF. The puck is teleported to the centre spot, so a peer that placed the goal one tick differently
#   differs by half a table. That is real, and it is a different quantity from drift -- reported through the
#   score, which is what a player actually reads it from.
#
# A server (and offline) is armed immediately: it has no join to sync and its corrections come from a client's
# input landing after it had already simulated the tick, which is drift.
func _measures(tick: int, was_live: bool, still_live: bool) -> bool:
	if not was_live or not still_live:
		return false
	if Net.is_offline() or Net.is_server():
		return true
	if _armed_after < 0:
		var known: int = -1 if _handle == null else _handle.get_last_known_state()
		if known <= 0:
			return false
		_armed_after = known
	return tick > _armed_after

# Consume a pending serve request, ONCE, in a way a resim reproduces.
#
# The request is written outside the tick by a command handler, so the flag is gone by the time anything
# replays the tick that consumed it -- and a replay that decided NOT to serve would overwrite the recorded row
# with "still dead" and un-serve the puck. The per-tick memo ring exists for exactly this: record the decision
# on the fresh pass, read the same answer back on every replayed pass.
#
# A CLIENT NEVER TAKES THIS BRANCH. `_serve_pending` is server-only and its memo is empty, so a client does not
# predict a manual serve and learns about it from the next authoritative row. That is correct rather than a
# gap: a command is a request the server may refuse, and there is nothing in the client's possession to predict
# it from.
func _consume_serve_request(tick: int, is_fresh: bool) -> bool:
	if _handle != null and _handle.is_active():
		if not is_fresh:
			return _handle.memo_get(tick, _MEMO_SERVE, 0) != 0
		if not _serve_pending:
			return false
		_serve_pending = false
		_handle.memo_set(tick, _MEMO_SERVE, 1)
		return true
	# Offline: no rollback loop, so no tick is ever replayed and the plain flag is the whole answer.
	if not _serve_pending:
		return false
	_serve_pending = false
	return true

# Rebuild the flat mirrors of the OCCUPIED mallets. Flat arrays rather than repeated node property reads
# because the collision pass runs PUCK_SUBSTEPS times over them per tick, and a PackedVector3Array read is a
# memory fetch where a node property read is a Variant round-trip.
func _refresh_mallet_mirrors() -> void:
	var count: int = 0
	for mallet: MalletBody in mallets:
		if mallet != null and mallet.is_occupied():
			count += 1
	if _mallet_pos.size() != count:
		_mallet_pos.resize(count)
		_mallet_vel.resize(count)
	var index: int = 0
	for mallet: MalletBody in mallets:
		if mallet == null or not mallet.is_occupied():
			continue
		_mallet_pos[index] = mallet.net_pos
		_mallet_vel[index] = mallet.net_vel
		index += 1
