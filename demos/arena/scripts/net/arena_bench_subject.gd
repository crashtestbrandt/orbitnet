extends BenchSubject
class_name ArenaBenchSubject
## netbench's view of this demo. Four things and nothing else: whether a session is live, the locally-owned
## body, how to push one tick-pure input frame into it, and that body's per-tick health numbers.
##
## THE READING THIS DEMO ADDS IS HIT REGISTRATION. It is the one property lag compensation exists to serve,
## and it is only measurable against a target the shooter actually resolved -- so `shots_fired` and
## `hits_confirmed` come from the server's own shot resolution rather than from anything the bench could see
## from outside. `target_kind()` answers PLAYER, which is what tells the gate those numbers mean something.
##
## SEAT 0 ONLY. A connection here may drive two fighters, but the bench drives one body and measures it;
## handing it two would make every reading an average of two players and the reconciliation error meaningless.
##
## THE FRAME IS A PLAIN DICTIONARY in a neutral vocabulary, because the bench cannot name a game's input type
## -- that is exactly the coupling this seam removes. An EMPTY dictionary means "release": stop overriding and
## hand the body back to live input, which the bot and the tape replay both send on teardown.

var _net: ArenaNet = null
var _seat: int = -1
var _driving: bool = false

func _init(session: ArenaNet) -> void:
	_net = session

func is_ready() -> bool:
	return _net != null and _net.state() == ArenaNet.State.PLAYING and local_body() != null

func local_body() -> Node:
	if _net == null or _net.world == null:
		return null
	var seats: PackedInt32Array = _net.local_seats()
	if seats.is_empty():
		return null
	if seats[0] != _seat:
		_seat = seats[0]
		var arrived: Node = _net.world.fighter_at(_seat)
		if arrived != null:
			# A client's body arrives asynchronously, after the handshake and the roster; the bench binds to
			# this rather than polling for it.
			subject_ready.emit(arrived)
	return _net.world.fighter_at(_seat)

func apply_input(frame: Dictionary) -> void:
	var body: Node = local_body()
	var fighter: FighterBody = body as FighterBody
	if fighter == null or fighter.input == null:
		return
	if frame.is_empty():
		# Release: stop overriding, and stop firing in particular -- a bot that ended a run mid-burst would
		# otherwise leave the trigger held for the human who takes the body back.
		_driving = false
		fighter.input.nin_move = Vector3.ZERO
		fighter.input.set_firing(false)
		return
	_driving = true
	fighter.input.nin_move = FighterMotion.clamp_intent(
		vec3_field(frame, BenchSubject.KEY_TRANSLATE, Vector3.ZERO))
	fighter.input.nin_aim = FighterMotion.clamp_aim(
		vec3_field(frame, BenchSubject.KEY_AIM_DIR, fighter.net_aim))
	var firing: bool = bool_field(frame, BenchSubject.KEY_FIRE, false)
	fighter.input.set_firing(firing)
	# THE SHOT IS A COMMAND, NOT AN INPUT BIT, here as everywhere else in this demo: a shot discovered inside
	# `_rollback_tick` would be replayed on every resim and fire again each time.
	if firing and fighter.is_alive():
		_net.world.request_shot(_seat)

func capture_input() -> Dictionary:
	var body: Node = local_body()
	var fighter: FighterBody = body as FighterBody
	if fighter == null or fighter.input == null or _driving:
		return {}
	return {
		BenchSubject.KEY_TRANSLATE: fighter.input.nin_move,
		BenchSubject.KEY_AIM_DIR: fighter.input.nin_aim,
		BenchSubject.KEY_FIRE: fighter.input.is_firing(),
	}

func sample(_body: Node) -> Dictionary:
	# SERVER-SIDE NUMBERS, and a client reports none. The shot resolution is authoritative, so only the peer
	# that ran it knows how many rounds landed -- a client counting its own tracers would be counting requests
	# rather than hits, which is the number lag compensation is supposed to move.
	if _net == null or _net.world == null or not Net.is_server():
		return {}
	return {
		BenchSubject.KEY_SHOTS_FIRED: float(_net.world.rewind.shots()),
		BenchSubject.KEY_HITS_CONFIRMED: float(_net.world.rewind.hits()),
	}

func remote_bodies() -> Array[Node]:
	var out: Array[Node] = []
	if _net == null or _net.world == null:
		return out
	var mine: PackedInt32Array = _net.local_seats()
	for fighter: FighterBody in _net.world.fighters:
		if fighter != null and not mine.has(fighter.seat):
			out.push_back(fighter)
	return out

## Every shot in this demo is aimed at another player's body, so the rounds above prove something about hit
## registration rather than merely about firing.
func target_kind() -> String:
	return BenchSubject.TARGET_PLAYER
