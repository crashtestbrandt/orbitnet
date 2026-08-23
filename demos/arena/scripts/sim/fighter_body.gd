extends Node3D
class_name FighterBody
## One fighter: a rollback body with a fat state lane, a child input node, and a capsule a shot can hit.
##
## THE FAT LANE IS THE POINT OF THIS BODY. Five state properties and three input ones, and the rollback loop
## pays a capture and a restore walk PER REPLAYED TICK, PER BODY -- so a twelve-tick resim over eight
## properties is 96 property reads and 96 writes in one frame, for one fighter. `set_bulk_capture()` and
## `set_bulk_restore()` make it 24 calls, and the lever that turns them off is what makes the difference
## visible in `Net.perf_metrics()`.
##
## WHAT IT REPLICATES IS ARENA-LOCAL. `net_pos` is a position in this fighter's OWN arena's frame, and the
## node's world transform is derived from it at placement time. That is the arrangement membership exists for:
## three arenas' worth of fighters occupy the same coordinates, so no radius can separate them and
## `set_membership("arena_id")` is the only thing that can.
##
## THE ANCHOR IS `net_pos` BY POSITION, NOT BY NAME. A rollback body's interest anchor is its FIRST Vector3
## state property, so the registration order below is load-bearing: putting velocity first would centre this
## peer's interest on a velocity.
##
## THE HITBOX IS NOT REPLICATED AND IS STILL PART OF THE CONTRACT. A lag-compensated shot reconstructs it from
## the rewind ring and hands the struck collider back, so its name is fixed and its collision layer is what
## decides whether a rewound cast can see it at all.

## The two lanes' entries, in the order the bulk hooks fill their arrays. Named once rather than spelled at the
## registration: `bulk_capture_order()` is derived from the registration, so reordering these lines silently
## reorders what the hooks must write, and a hook writing the right values into the wrong slots replays wrong
## rather than erroring. `bulk_marshal_test.gd` asserts the correspondence.
const STATE_PROPS: Array[String] = [
	"net_pos@half", "net_vel@half", "net_aim@half", "net_vitals@half", "net_flags"]
const INPUT_PROPS: Array[String] = ["nin_move@half", "nin_aim@half", "nin_buttons"]

const MARSHAL_OUT: String = "_net_marshal_out"
const MARSHAL_IN: String = "_net_marshal_in"

# --- flag bits ---------------------------------------------------------------------------------------
const FLAG_ALIVE: int = 1 << 0
const FLAG_CLOAKED: int = 1 << 1
## The shot sequence, so a client can tell one shot from the next without a second channel. Wraps; only
## equality against the previous value is ever asked.
const FLAG_SEQ_SHIFT: int = 8
const FLAG_SEQ_MASK: int = 0xFF

# --- identity (fixed at build, never replicated -- every peer derives it from the seat) ---------------
var seat: int = -1
var team: int = 0
## The MEMBERSHIP property. An int, read live on the authority, and not on the wire: every peer derives it
## from the seat, so replicating it would be replicating a division.
var arena_id: int = 0
var owner_peer: int = 0

# --- REPLICATED state --------------------------------------------------------------------------------
## Arena-local position. THE INTEREST ANCHOR, by being the first Vector3 registered.
var net_pos: Vector3 = Vector3.ZERO
var net_vel: Vector3 = Vector3.ZERO
var net_aim: Vector3 = Vector3(0.0, 0.0, 1.0)
## (health01, heat01, respawn01) packed into one Vector3. A bare float cannot be narrowed at all; as a
## component of a quantized Vector3 each costs two bytes.
var net_vitals: Vector3 = Vector3(1.0, 0.0, 0.0)
var net_flags: int = FLAG_ALIVE

var input: FighterInput = null

# --- server-only ---------------------------------------------------------------------------------------
var _handle: NetRollbackHandle = null
var _hitbox: Area3D = null
var _last_shot_tick: int = -1
var _cloak_until_tick: int = -1
var _respawn_at_tick: int = -1
var _shot_sequence: int = 0

## DISCRETE EVENTS, QUEUED, AND THIS IS THE RULE THE WHOLE REPOSITORY KEEPS RUNNING INTO. The rollback lane
## RESTORES RECORDED HISTORY ONTO ITS PROPERTIES EVERY TICK. A hit written by the command handler that
## resolved it -- which runs OUTSIDE the tick -- is overwritten by the next restore, silently, on the server,
## and the fighter simply never dies. Queued here and drained INSIDE the tick, the result is recorded at that
## tick and every replay restores it.
##
## The other two demos meet the same rule from the other side: the hockey scoreboard is on the STATE lane
## precisely so a goal found inside the tick survives, and the RTS demo puts its units there so an order can
## be written from a handler at all. Here the values have to be on the rollback lane -- they are what a
## rewound shot is resolved against -- so the write is what moves instead.
var _pending_damage: float = 0.0
var _pending_damage_from: int = -1
var _pending_cloak: bool = false
var _pending_shots: int = 0
var _pending_shot_tick: int = -1
## Set INSIDE the tick when a queued hit killed this fighter, and drained by the director afterwards. The
## scorecard it credits is on the STATE lane, so writing that outside the tick is safe -- which is the whole
## reason the score lives there.
var _kill_by_seat: int = -1

# --- construction ---------------------------------------------------------------------------------------
## Configure identity. Called BEFORE the node enters the tree, because the node's NAME is part of the path the
## entity id is hashed from.
func configure(seat_index: int, peer: int) -> void:
	seat = seat_index
	team = ArenaConfig.team_of_seat(seat_index)
	arena_id = ArenaConfig.arena_of_seat(seat_index)
	owner_peer = peer
	name = ArenaNames.fighter_node_name(seat_index)
	net_pos = ArenaGeometry.home_local(seat_index)
	net_aim = Vector3(0.0, 0.0, 1.0 if team == 0 else -1.0)

	input = FighterInput.new()
	input.name = ArenaNames.INPUT_NODE
	input.nin_aim = net_aim
	add_child(input)

	_hitbox = Area3D.new()
	_hitbox.name = ArenaNames.HITBOX_NODE
	_hitbox.collision_layer = ArenaConfig.LAYER_FIGHTER
	# A hitbox that MONITORS costs a broadphase pair per fighter per tick and answers a question nothing here
	# asks. It exists to be found by a ray and to be reconstructed by the rewind, both of which read the layer.
	_hitbox.monitoring = false
	_hitbox.monitorable = true
	var shape: CollisionShape3D = CollisionShape3D.new()
	shape.name = "Shape"
	var capsule: CapsuleShape3D = CapsuleShape3D.new()
	capsule.radius = ArenaConfig.FIGHTER_RADIUS
	capsule.height = ArenaConfig.FIGHTER_HEIGHT
	shape.shape = capsule
	_hitbox.add_child(shape)
	add_child(_hitbox)
	_apply_pose()

## Register the replication lanes. Called AFTER the fighter is in the tree at its final path.
func bind_net() -> void:
	if Net.is_offline():
		# Offline the handle is inert and every property simply sticks where it is written -- the same code
		# path, which is the point of the facade's OFFLINE contract. MatchDirector drives advance() directly.
		_handle = Net.register_rollback_body(self, input, [], [], false, [])
		return
	# The fighter's STATE authority stays with the server (peer 1, the default); the INPUT node's moves to the
	# owner. Set before registration: the backend reads the authority when it processes settings.
	input.set_multiplayer_authority(owner_peer if owner_peer > 0 else 1)
	var predict: bool = Net.is_server() or owner_peer == multiplayer.get_unique_id()
	_handle = Net.register_rollback_body(self, input, STATE_PROPS, INPUT_PROPS, predict)
	# THE MEMBERSHIP, and without it this demo has one arena rather than three. A body that declares no
	# membership is in every world, so all 24 fighters would replicate to every peer at every distance -- and
	# because the coordinates are arena-local, the radius could not separate them either.
	_handle.set_membership("arena_id")
	_handle.process_settings()
	set_bulk_marshalling(true)

# --- seats ------------------------------------------------------------------------------------------
## Re-point this fighter's INPUT at `peer`, and say which of that connection's seats it is.
##
## THE TWO ARE DIFFERENT AXES AND BOTH ARE NEEDED. `set_input_authority()` says WHICH CONNECTION authors this
## body's input; `set_seat()` says which of that connection's owned bodies this one is, for interest. A
## connection driving two fighters that both sat at seat 0 would have one interest centre for both, and the
## second player's surroundings culled around where the first was standing.
func set_owner_peer(peer: int, seat_index: int) -> void:
	owner_peer = peer
	if _handle == null:
		return
	_handle.set_input_authority(peer if peer > 0 else 1)
	_handle.set_seat(maxi(0, seat_index))

## This fighter's session-global entity id, or 0 while the facade is OFFLINE. A veto names it; so does an
## observer that follows this fighter.
func entity_id() -> int:
	return 0 if _handle == null else _handle.entity_id()

## The tick of the newest authoritative row this peer holds for this fighter, or -1 when inert.
##
## THE CLIENT HALF OF INTEREST FILTERING. A cull -- by distance, by membership, or by a veto -- stops the rows
## and never removes the node, so a client that wants to know whether it is still being sent a body has to
## notice for itself that this number stopped moving. All three axes look identical from here, which is the
## honest situation rather than a gap: what an entity that stopped updating MEANS is the game's decision.
func last_known_state() -> int:
	return -1 if _handle == null else _handle.get_last_known_state()

# --- bulk marshalling --------------------------------------------------------------------------------
## Marshal a whole lane in ONE script-boundary crossing instead of one per property.
##
## FILL EVERY SLOT. The array is preallocated and reused, so a slot left alone silently keeps last tick's
## value; there is no unset sentinel. Do not resize it -- a wrong-length array drops the lane back to the walk
## and says so once.
##
## BOTH LANES GO THROUGH ONE METHOD, dispatched on `lane`. The input entries live on the CHILD input node
## while the hook resolves on the body's ROOT, so the input half reaches through `input`. Where a value is
## stored is the game's business; the hook only has to supply it.
func _net_marshal_out(lane: int, values: Array) -> void:
	if lane == NetRollbackHandle.LANE_INPUT:
		values[0] = input.nin_move
		values[1] = input.nin_aim
		values[2] = input.nin_buttons
		return
	values[0] = net_pos
	values[1] = net_vel
	values[2] = net_aim
	values[3] = net_vitals
	values[4] = net_flags

## The restore half. Assigned to the typed fields rather than cast: `values[i]` is a Variant, and an
## assignment is the conversion this project allows.
func _net_marshal_in(lane: int, values: Array) -> void:
	if lane == NetRollbackHandle.LANE_INPUT:
		input.nin_move = values[0]
		input.nin_aim = values[1]
		input.nin_buttons = values[2]
		return
	net_pos = values[0]
	net_vel = values[1]
	net_aim = values[2]
	net_vitals = values[3]
	net_flags = values[4]

## Declare the hooks, or take them away. An unresolvable method name is the documented way back to the
## per-property walk, so `false` sets an empty name and re-processes the settings.
func set_bulk_marshalling(on: bool) -> void:
	if _handle == null:
		return
	_handle.set_bulk_capture(MARSHAL_OUT if on else "")
	_handle.set_bulk_restore(MARSHAL_IN if on else "")
	_handle.process_settings()

## Whether each lane is actually marshalling in bulk. Asked of the backend rather than assumed: a name that
## does not resolve leaves the lane on the walk and reports nothing at the call site.
func uses_bulk_state() -> bool:
	return _handle != null and _handle.uses_bulk_capture(NetRollbackHandle.LANE_STATE)

func uses_bulk_input() -> bool:
	return _handle != null and _handle.uses_bulk_capture(NetRollbackHandle.LANE_INPUT)

# --- the tick ----------------------------------------------------------------------------------------
## One simulation step. Called by the backend as `_rollback_tick` when networked, and by MatchDirector's
## offline accumulator when there is no session -- one body, two clocks, so "it behaves differently offline"
## cannot happen quietly.
func advance(delta: float, tick: int, is_fresh: bool) -> void:
	# ON A FRESH TICK ONLY. A replay re-simulates a tick that already happened, and its recorded result
	# already carries whatever these events did; draining again would apply one hit several times.
	if is_fresh and (Net.is_server() or Net.is_offline()):
		_drain_pending(tick)
	if is_alive():
		var state: FighterMotion.State = FighterMotion.State.new(net_pos, net_vel)
		var next: FighterMotion.State = state
		if owner_peer <= 0:
			# A vacant seat is parked, re-asserted EVERY tick rather than once: the rollback lane restores
			# recorded history onto these properties, so a single write from outside the tick would be undone
			# by the next restore.
			next = FighterMotion.park(ArenaGeometry.home_local(seat))
		else:
			next = FighterMotion.step(state, input.nin_move, delta)
		net_pos = next.position
		net_vel = next.velocity
		net_aim = FighterMotion.clamp_aim(input.nin_aim)
	else:
		net_pos = ArenaGeometry.home_local(seat)
		net_vel = Vector3.ZERO

	# The countdowns are SERVER-SIDE and are folded into replicated values here rather than being replicated
	# themselves. A client reads them out of net_vitals and net_flags, which it was going to receive anyway.
	if Net.is_server() or Net.is_offline():
		_advance_timers(tick)
	_apply_pose()

func _rollback_tick(delta: float, tick: int, is_fresh: bool) -> void:
	advance(delta, tick, is_fresh)

## Apply the discrete events queued since the last fresh tick. SERVER-SIDE, and inside the tick.
func _drain_pending(tick: int) -> void:
	if _pending_shots > 0:
		_last_shot_tick = tick
		_shot_sequence = (_shot_sequence + _pending_shots) & FLAG_SEQ_MASK
		net_flags = (net_flags & ~(FLAG_SEQ_MASK << FLAG_SEQ_SHIFT)) | (_shot_sequence << FLAG_SEQ_SHIFT)
		_pending_shots = 0
		_pending_shot_tick = -1
	if _pending_cloak:
		_pending_cloak = false
		if is_alive() and not is_cloaked():
			net_flags |= FLAG_CLOAKED
			_cloak_until_tick = tick + ArenaConfig.CLOAK_TICKS
	if _pending_damage > 0.0 and is_alive():
		var health: float = clampf(net_vitals.x - _pending_damage, 0.0, 1.0)
		net_vitals = Vector3(health, net_vitals.y, net_vitals.z)
		if health <= 0.0:
			net_flags &= ~FLAG_ALIVE
			# A CORPSE MUST NOT STAY CLOAKED. It would stay withheld, so the peer it was hidden from would
			# keep drawing it alive and standing exactly where it fell.
			net_flags &= ~FLAG_CLOAKED
			_cloak_until_tick = -1
			_respawn_at_tick = tick + ArenaConfig.RESPAWN_TICKS
			_kill_by_seat = _pending_damage_from
	_pending_damage = 0.0
	_pending_damage_from = -1

func _advance_timers(tick: int) -> void:
	var alive: bool = is_alive()
	if not alive and _respawn_at_tick >= 0 and tick >= _respawn_at_tick:
		_respawn_at_tick = -1
		net_flags |= FLAG_ALIVE
		net_vitals = Vector3(1.0, 0.0, 0.0)
		alive = true
	if is_cloaked() and _cloak_until_tick >= 0 and tick >= _cloak_until_tick:
		_cloak_until_tick = -1
		net_flags &= ~FLAG_CLOAKED
	var heat: float = 0.0
	if _last_shot_tick >= 0:
		heat = clampf(1.0 - float(tick - _last_shot_tick) / float(ArenaConfig.SHOT_COOLDOWN_TICKS), 0.0, 1.0)
	var respawn: float = 0.0
	if not alive and _respawn_at_tick >= 0:
		respawn = clampf(float(_respawn_at_tick - tick) / float(ArenaConfig.RESPAWN_TICKS), 0.0, 1.0)
	net_vitals = Vector3(net_vitals.x, heat, respawn)

## Place the node where its replicated arena-local pose says, in world space. Called every tick on every peer:
## this is the one place the rebasing is applied, and it is presentation only.
func _apply_pose() -> void:
	position = ArenaGeometry.local_to_world(arena_id, net_pos)
	if _hitbox != null:
		_hitbox.position = Vector3(0.0, ArenaConfig.FIGHTER_HEIGHT * 0.5, 0.0)

# --- server-side outcomes ------------------------------------------------------------------------------
## Queue damage from `by_seat`. SERVER-SIDE, from the shot resolution, which runs OUTSIDE the tick -- see
## `_drain_pending()` for why that means queueing rather than writing.
func queue_damage(amount: float, by_seat: int) -> void:
	if not is_alive():
		return
	_pending_damage += amount
	_pending_damage_from = by_seat

## Queue a shot fired at `tick`: the sequence a client watches to draw a tracer, and the cooldown's mark.
func queue_shot(tick: int) -> void:
	_pending_shots += 1
	_pending_shot_tick = maxi(_pending_shot_tick, tick)

## When this fighter last fired, INCLUDING a shot queued but not yet drained. Without the pending half, two
## shots inside one tick would both pass the cooldown, because neither had been recorded yet.
func last_shot_tick() -> int:
	return maxi(_last_shot_tick, _pending_shot_tick)

## Queue the cloak. Returns false when this fighter is down, already cloaked, or already holds a queued one --
## so the pickup is not spent on a fighter that will not use it.
func queue_cloak() -> bool:
	if not is_alive() or is_cloaked() or _pending_cloak:
		return false
	_pending_cloak = true
	return true

## Whether a cloak is queued but not yet applied. One tick, and the readout says so rather than showing a
## pickup that appears to have done nothing.
func cloak_pending() -> bool:
	return _pending_cloak

## Take the credit for a kill this fighter suffered, if any: the seat that scored it, or -1. Clears it, so
## one death is credited once.
func take_kill_credit() -> int:
	var by: int = _kill_by_seat
	_kill_by_seat = -1
	return by

# --- reads ----------------------------------------------------------------------------------------------
func is_alive() -> bool:
	return (net_flags & FLAG_ALIVE) != 0

func is_cloaked() -> bool:
	return (net_flags & FLAG_CLOAKED) != 0

func health() -> float:
	return net_vitals.x

func shot_sequence() -> int:
	return (net_flags >> FLAG_SEQ_SHIFT) & FLAG_SEQ_MASK

## The hit capsule's world transform, for the rewind snapshot.
func hitbox_transform() -> Transform3D:
	return _hitbox.global_transform if _hitbox != null else global_transform

func hitbox() -> Area3D:
	return _hitbox

func hitbox_rid() -> RID:
	return _hitbox.get_rid() if _hitbox != null else RID()

## Where a shot from this fighter leaves it, in world space.
func muzzle_world() -> Vector3:
	return ArenaGeometry.local_to_world(arena_id, FighterMotion.muzzle(net_pos))
