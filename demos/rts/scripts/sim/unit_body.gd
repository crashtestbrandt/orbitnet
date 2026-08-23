extends Node3D
class_name UnitBody
## One unit. Carries netcode state and nothing else -- no mesh, no collision shape, no children beyond its
## synchronizer. What you see on screen is drawn by UnitRenderer through a MultiMeshInstance3D that reads
## these nodes; the netcode entity and the render representation are deliberately separable, and separating
## them is what makes 96 replicated units cost 96 draw-instances rather than 96 scene subtrees.
##
## THE STATE LANE, NOT THE ROLLBACK LANE. Three reasons, and they compound:
##
##   1. There is nothing to predict. A unit's "input" is a sparse ORDER, not a per-tick stream. Rollback
##      prediction extrapolates the next input from the last one; with orders, the last input is almost
##      always "nothing happened", so prediction would be exactly as good as doing nothing and would cost a
##      replay to achieve it.
##   2. Every rollback entity costs a history_limit-row ring plus a per-tick memcmp and a full replay. At 96
##      units that is 96 rings and 96 replays per tick to predict a value nobody can author.
##   3. Decisively: THE ROLLBACK LANE RESTORES RECORDED HISTORY ONTO ITS PROPERTIES EVERY TICK. An order
##      arrives through a NetCommand handler, which runs OUTSIDE the tick -- so a goal written there would be
##      overwritten by the next restore, silently, on the server, and the unit would simply never move.
##      That is precisely the guarantee make_state() offers and make_rollback() does not.
##
## THE WIRE SCHEMA -- 20 bytes of properties per unit per refresh:
##
##   position@half   6 B   Vector3 as three binary16s. Free: `position` is already a Node3D property, so the
##                         server writes it once and the state lane picks it up with no shadow copy.
##   net_aux@half    6 B   (sin facing, cos facing, hp01) packed into one Vector3.
##   net_meta        8 B   an i64 bitfield: alive | current target id | order sequence.
##
## FACING GOES AS A DIRECTION PAIR, NOT AN ANGLE, and this is the single most transferable trick in the demo.
## A yaw scalar seems obvious and is wrong twice over: a GDScript `float` is an f64, so `"facing@half"` would
## silently fall back to lossless and save nothing (@half is valid only for Vector3/Vector2/f32 -- an invalid
## pairing degrades quietly rather than erroring); and interpolating an angle across the +/-pi wrap makes a
## unit spin the long way round through a full rotation whenever it faces roughly south. Sending (sin, cos)
## costs 4 bytes as halves, interpolates correctly through the wrap because it is a point on a circle, and
## rides along in a Vector3 that had a spare component anyway.
##
## hp01 shares that Vector3 for the same reason: a bare `float` hp cannot be narrowed at all, but as a
## normalized third component of a quantized Vector3 it costs 2 bytes.

# --- identity (fixed at build, never replicated -- both peers derive it) --------------------------
var id: int = -1
var seat: int = -1
var arch: RtsConfig.Archetype = null

# --- REPLICATED state properties -----------------------------------------------------------------
## (sin facing, cos facing, hp01). Written by the server each tick, read by the renderer on every peer.
var net_aux: Vector3 = Vector3(0.0, 1.0, 1.0)
## Bitfield: see pack_meta(). Carries liveness, the current target, and the order sequence.
var net_meta: int = 0

# --- server-only simulation state (NEVER replicated) ---------------------------------------------
var _sim: UnitSteering.State = null
var _goal: Vector3 = Vector3.ZERO
var _order: StringName = OrderValidator.VERB_STOP
var _hp: float = 1.0
var _target: int = -1
var _ord_seq: int = 0
var _dead_since: float = -1.0

var _state_handle: NetStateHandle = null
var _interp: NetInterpolatorHandle = null
var _was_alive: bool = true

# --- meta packing --------------------------------------------------------------------------------
# Static so the tests can exercise the packing directly with no node and no session. The field widths are
# generous on purpose: target needs 7 bits for 96 units and gets 8, ord_seq gets 16 and wraps (a sequence only
# ever needs to be COMPARED for change, never ordered globally), and alive is one bit above them.
const _TARGET_BITS: int = 8
const _SEQ_BITS: int = 16
const _TARGET_MASK: int = (1 << _TARGET_BITS) - 1
const _SEQ_MASK: int = (1 << _SEQ_BITS) - 1
const _ALIVE_BIT: int = 1 << (_TARGET_BITS + _SEQ_BITS)

## Pack liveness, target and order sequence into one i64. `target` is -1 for "none" and is stored offset by
## one so that zero means none, which keeps a freshly zeroed entity meaningful instead of pointing at unit 0.
static func pack_meta(alive: bool, target: int, ord_seq: int) -> int:
	var target_field: int = 0
	if target >= 0:
		target_field = (target + 1) & _TARGET_MASK
	var seq_field: int = (ord_seq & _SEQ_MASK) << _TARGET_BITS
	var alive_field: int = _ALIVE_BIT if alive else 0
	return alive_field | seq_field | target_field

static func meta_alive(meta: int) -> bool:
	return (meta & _ALIVE_BIT) != 0

static func meta_target(meta: int) -> int:
	return (meta & _TARGET_MASK) - 1

static func meta_seq(meta: int) -> int:
	return (meta >> _TARGET_BITS) & _SEQ_MASK

## The facing angle encoded in a net_aux vector. The inverse of the (sin, cos) packing.
static func aux_facing(aux: Vector3) -> float:
	return atan2(aux.x, aux.y)

## The normalized health encoded in a net_aux vector.
static func aux_hp01(aux: Vector3) -> float:
	return clampf(aux.z, 0.0, 1.0)

# --- construction --------------------------------------------------------------------------------
## Configure identity. Called by WorldDirector BEFORE the node enters the tree, because the node's NAME (set
## from the id) is part of the path the backend derives its entity id from.
func configure(unit_id: int) -> void:
	id = unit_id
	seat = RtsConfig.seat_of(unit_id)
	arch = RtsConfig.archetype(RtsConfig.kind_for_index(unit_id % RtsConfig.UNITS_PER_SEAT))
	name = RtsNames.unit_node_name(unit_id)
	_sim = UnitSteering.State.new(Vector3.ZERO, Vector3.ZERO, 0.0)
	_hp = arch.hp_max
	_publish()

## Register the replication lane. Called by WorldDirector AFTER add_child, so the node is already at its final
## path -- the entity id is a hash of that path, so registering earlier would derive an id for a path the node
## no longer has.
func bind_net() -> void:
	_state_handle = Net.make_state(self)
	_state_handle.add_state(self, "position@half")
	_state_handle.add_state(self, "net_aux@half")
	_state_handle.add_state(self, "net_meta")
	# THE ANCHOR IS DECLARED, NOT INFERRED, AND WITHOUT IT THESE 96 UNITS ARE NEVER CULLED. A state channel
	# that names no anchor is ALWAYS relevant, at every distance, in every world -- so before this line the
	# demo's AOI radius reached the two cursors on the rollback lane and nothing else, leaving the units that
	# are actually the bandwidth replicating to every peer regardless of where that peer was looking.
	#
	# `"position"`, NOT `"position@half"`. The entry names a live Vector3 read on the AUTHORITY to compute
	# relevancy; it is not a wire entry, so it takes no quantization suffix and costs no bytes. That it happens
	# to also be replicated here is a coincidence of this unit having a position worth sending.
	_state_handle.set_anchor("position")
	_state_handle.process_settings()

	# Interpolation is for peers that RECEIVE this unit. The server writes position every tick from its own
	# sim, so smoothing there would fight the authoritative value; a listen host is a server and takes the
	# same branch. At a 20 Hz net tick a receiving client without this visibly steps -- which is exactly what
	# the F6 lever in the HUD turns off, so you can see it.
	if Net.is_client() and not Net.is_server():
		_interp = Net.make_interpolator(self)
		_interp.add_property(self, "position")
		_interp.add_property(self, "net_aux")
		_interp.process_settings()

## This unit's session-global entity id, or 0 while the facade is OFFLINE and the handle is inert. An
## observer tracking a unit names it by this; so does a veto withholding it from one peer.
func entity_id() -> int:
	return 0 if _state_handle == null else _state_handle.entity_id()

# --- server-side simulation ----------------------------------------------------------------------
## Place the unit (world build, and every respawn). Server-side; the position replicates from there.
func place(at: Vector3, facing: float) -> void:
	_sim = UnitSteering.State.new(at, Vector3.ZERO, facing)
	_goal = at
	_order = OrderValidator.VERB_STOP
	_hp = arch.hp_max
	_target = -1
	_dead_since = -1.0
	position = at
	_publish()

## Apply a validated order. Only ever called on the server (or offline), from the NetCommand handler -- i.e.
## OUTSIDE the tick. Safe precisely because these values live in server-only fields and on the STATE lane.
func apply_order(verb: StringName, goal: Vector3, sequence: int) -> void:
	_order = verb
	_ord_seq = sequence
	match verb:
		OrderValidator.VERB_STOP, OrderValidator.VERB_HOLD:
			_goal = _sim.position
			_target = -1
		_:
			_goal = goal
	_publish()

## One authoritative simulation step. Server-only.
func sim_step(positions: PackedVector3Array, alive: PackedByteArray, obstacles: Array[AABB],
		dt: float) -> void:
	if not is_alive():
		return
	# Auto-acquire. HOLD deliberately still shoots -- it means "do not move", not "do not fight" -- so only
	# the goal differs between HOLD and ATTACK_MOVE, which is the distinction a player expects.
	if _order != OrderValidator.VERB_STOP:
		if _target < 0 or _target >= alive.size() or alive[_target] == 0:
			_target = Combat.nearest_enemy(_sim.position, seat, positions, alive, arch.acquire_range)
	else:
		_target = Combat.nearest_enemy(_sim.position, seat, positions, alive, arch.attack_range)

	var goal: Vector3 = _goal
	if _target >= 0 and _target < positions.size():
		var target_position: Vector3 = positions[_target]
		if _order == OrderValidator.VERB_ATTACK_MOVE:
			goal = Combat.approach_goal(_sim.position, target_position, arch.attack_range)
		elif _order == OrderValidator.VERB_STOP or _order == OrderValidator.VERB_HOLD:
			goal = _sim.position
	_sim = UnitSteering.step(_sim, goal, obstacles, arch, dt)
	position = _sim.position
	_publish()

## Apply damage. Server-only. Returns true if this killed the unit.
func take_damage(amount: float, now: float) -> bool:
	if not is_alive() or amount <= 0.0:
		return false
	_hp = maxf(0.0, _hp - amount)
	if _hp > 0.0:
		_publish()
		return false
	_dead_since = now
	_target = -1
	_publish()
	return true

## Whether the respawn drip may return this unit yet.
func ready_to_respawn(now: float) -> bool:
	return not is_alive() and _dead_since >= 0.0 and (now - _dead_since) >= RtsConfig.RESPAWN_DELAY_S

## The server's current target for this unit (diagnostics + the combat pass).
func target_id() -> int:
	return _target

func attack_position() -> Vector3:
	return _sim.position

# --- shared reads (every peer) --------------------------------------------------------------------
## Liveness. On the server this is the sim's own hp; on a client it is read back out of replicated state, so
## both answer the same question from the same number.
func is_alive() -> bool:
	if Net.is_server() or Net.is_offline():
		return _hp > 0.0
	return meta_alive(net_meta)

## Normalized health for the health bar, on any peer.
func hp01() -> float:
	if Net.is_server() or Net.is_offline():
		return clampf(_hp / maxf(0.0001, arch.hp_max), 0.0, 1.0)
	return aux_hp01(net_aux)

## Drawn facing on any peer.
func facing() -> float:
	if Net.is_server() or Net.is_offline():
		return _sim.facing
	return aux_facing(net_aux)

## The order sequence last applied, as every peer sees it. The client measures order RTT by watching this
## change on a unit it just issued an order to.
func order_seq() -> int:
	if Net.is_server() or Net.is_offline():
		return _ord_seq
	return meta_seq(net_meta)

## Called on a receiving client each frame so a respawn does not smooth across the map. Cheap: it only acts on
## the tick where liveness actually flips.
func note_liveness_for_interp() -> void:
	var now_alive: bool = meta_alive(net_meta)
	if now_alive != _was_alive:
		_was_alive = now_alive
		if now_alive and _interp != null and _interp.is_active():
			_interp.teleport()

## Turn interpolation on/off (the HUD's F6 lever). No-op on the server, which has no interpolator.
func set_interpolation(on: bool) -> void:
	if _interp != null:
		_interp.set_enabled(on)

# --- internals -----------------------------------------------------------------------------------
# Refresh the replicated properties from the server-side sim. One place, called after every mutation, so a
# new field cannot be added to the sim and forgotten on the wire.
func _publish() -> void:
	if _sim == null:
		return
	var hp_fraction: float = clampf(_hp / maxf(0.0001, arch.hp_max), 0.0, 1.0)
	net_aux = Vector3(sin(_sim.facing), cos(_sim.facing), hp_fraction)
	net_meta = pack_meta(_hp > 0.0, _target, _ord_seq)
