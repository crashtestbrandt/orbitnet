extends Node
class_name FighterController
## Turns local input into the replicated input frame, once per net tick, for each locally-driven seat.
##
## TWO SEATS, TWO KEY SETS, ONE CONNECTION. Split-screen is not a networking feature here -- both fighters'
## input rides the same connection, in the same frame -- but it is what makes the backend's seat index mean
## something, and it needs a second set of keys to be playable at all.
##
## WRITTEN ON `pre_tick`, NOT PER FRAME. The backend records the input frame right after that signal, so a
## write anywhere else is either captured a tick late or captured twice. Reading the keyboard here rather than
## in `_input` also means the value captured is the state of the keys AT THE TICK, which is what a replay
## reproduces.
##
## INPUT IS READ FROM RAW KEYS rather than through the project's input map. A deliberate simplification for a
## netcode demo: an InputMap is a rebinding feature, it is verbose to define in project.godot, and it would be
## the single most likely thing to break when this project is opened in a different Godot version. A real game
## should use one.

## Seat 0: WASD to move, QE to turn, SPACE to fire.
const KEYS_A: Array[Key] = [KEY_W, KEY_S, KEY_A, KEY_D, KEY_Q, KEY_E, KEY_SPACE]
## Seat 1: IKJL to move, UO to turn, RIGHT SHIFT to fire.
const KEYS_B: Array[Key] = [KEY_I, KEY_K, KEY_J, KEY_L, KEY_U, KEY_O, KEY_SHIFT]

const TURN_RATE: float = 2.8

var net: ArenaNet = null

var _aim: PackedFloat32Array = PackedFloat32Array()
var _fired_seq: PackedInt32Array = PackedInt32Array()

func configure(session: ArenaNet) -> void:
	net = session
	_aim.resize(ArenaConfig.MAX_SEATS_PER_PEER)
	_fired_seq.resize(ArenaConfig.MAX_SEATS_PER_PEER)
	for index: int in _aim.size():
		_aim[index] = 0.0
		_fired_seq[index] = 0

func _ready() -> void:
	if not Net.pre_tick.is_connected(_on_pre_tick):
		Net.pre_tick.connect(_on_pre_tick)

func _process(delta: float) -> void:
	# OFFLINE the tick loop does not run, so `pre_tick` never fires and the input frame would never be
	# written. Driving it from the frame instead keeps the single-player path playable through exactly the
	# same code.
	if Net.is_offline():
		_write_frames(delta)

func _on_pre_tick(_tick: int) -> void:
	_write_frames(ArenaConfig.NET_TICK_DT)

func _write_frames(delta: float) -> void:
	if net == null or net.world == null or net.is_observing():
		return
	var seats: PackedInt32Array = net.local_seats()
	# EVERY LOCAL SEAT'S SHOT IN ONE PACKET. Both fighters behind one connection fire in the same frame on the
	# same channel, so the requests are collected here and flushed once rather than sent as they are found --
	# which is the whole reason `request_batch` exists. A connection driving one seat sends what it always did.
	var firing: PackedInt32Array = PackedInt32Array()
	for index: int in seats.size():
		if index >= ArenaConfig.MAX_SEATS_PER_PEER:
			break
		if _write_frame(index, seats[index], delta):
			firing.push_back(seats[index])
	net.world.request_shots(firing)

## Writes one seat's replicated input frame. Answers whether that seat wants to fire this frame, so the caller
## can coalesce every local seat's request into one packet.
func _write_frame(index: int, seat: int, delta: float) -> bool:
	var fighter: FighterBody = net.world.fighter_at(seat)
	if fighter == null or fighter.input == null:
		return false
	var keys: Array[Key] = KEYS_A if index == 0 else KEYS_B
	var forward: float = _axis(keys[0], keys[1])
	var strafe: float = _axis(keys[3], keys[2])
	_aim[index] += _axis(keys[5], keys[4]) * TURN_RATE * delta

	var facing: Vector3 = Vector3(sin(_aim[index]), 0.0, cos(_aim[index]))
	var right: Vector3 = Vector3(facing.z, 0.0, -facing.x)
	fighter.input.nin_move = FighterMotion.clamp_intent(facing * forward + right * strafe)
	fighter.input.nin_aim = facing
	var firing: bool = Input.is_physical_key_pressed(keys[6])
	fighter.input.set_firing(firing)
	# THE SHOT IS A COMMAND, NOT AN INPUT BIT. A shot discovered inside `_rollback_tick` would be replayed on
	# every resim and fire again each time; the bit above exists so the readout can show a held trigger.
	return firing and fighter.is_alive()

static func _axis(positive: Key, negative: Key) -> float:
	var value: float = 0.0
	if Input.is_physical_key_pressed(positive):
		value += 1.0
	if Input.is_physical_key_pressed(negative):
		value -= 1.0
	return value
