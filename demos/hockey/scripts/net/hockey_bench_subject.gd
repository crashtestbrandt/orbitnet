extends BenchSubject
class_name HockeyBenchSubject
## netbench's view of this demo.
##
## THE FIRST SUBJECT WITH A REAL RECONCILE ERROR TO REPORT. `BenchSubject` defines KEY_RECONCILE_ERROR,
## KEY_RECONCILE_SMOOTH and KEY_RECONCILE_SNAP, and until now nothing fed them: the RTS demo's own subject
## says so in its header, because a commander cursor whose entire simulation is a clamp essentially never
## mispredicts. Air hockey mispredicts for a reason that cannot be designed away -- an opponent's strike is not
## implied by anything this peer holds -- so those three columns finally carry a distribution.
##
## The neutral vocabulary maps onto this game without stretching:
##
##   translate -> drives the mallet's requested point around the player's own half, which rides the ROLLBACK
##                lane exactly as a character's input frame did.
##   fire      -> asks for a serve, which exercises the COMMAND lane (reliable, server-validated) and is
##                refused while the puck is live -- so a bot holding `fire` measures the validator too.

## Meters per second the policy's own motion sweeps the requested point at. It is a JITTER on top of the puck
## tracking below, not the whole of the bot's motion.
const SWEEP_SPEED: float = 1.1
## Fraction of the way to the tracked point the bot moves its request each tick. Below 1.0 so the mallet
## approaches rather than teleports, which is what gives it a velocity to strike with.
const TRACK_GAIN: float = 0.16
## How far up its own half a bot will come to meet the puck, as a fraction of the half's length. Short of the
## center line, so two bots trade the puck back and forth instead of both camping on it.
const COMMIT_FRACTION: float = 0.72

var _net: HockeyNet = null
var _view: PuckView = null
var _controller: MalletController = null
var _target: Vector3 = Vector3.ZERO
var _last_frame: Dictionary = {}

func _init(session: HockeyNet, view: PuckView = null, controller: MalletController = null) -> void:
	_net = session
	_view = view
	_controller = controller
	if _net != null:
		_net.local_seat_changed.connect(_on_seat_changed)

func _on_seat_changed(seat: int) -> void:
	if seat < 0:
		return
	_target = TableGeometry.home_point(seat)
	var body: Node = local_body()
	if body != null:
		subject_ready.emit(body)

# --- the BenchSubject contract ---------------------------------------------------------------------
func is_ready() -> bool:
	return _net != null and _net.state() == HockeyNet.State.PLAYING and _net.local_seat() >= 0 \
		and _net.rink != null

## The locally-owned body is this peer's MALLET. It is the only entity this peer authors input for -- the puck
## is on the rollback lane too, and is authored by nobody.
func local_body() -> Node:
	return _mallet()

## The same lookup, typed. Everything inside this class goes through it: narrowing local_body()'s `Node` return
## would be a downcast, and this project bans as-casts outright.
func _mallet() -> MalletBody:
	if _net == null or _net.rink == null:
		return null
	var seat: int = _net.local_seat()
	if seat < 0 or seat >= _net.rink.mallets.size():
		return null
	return _net.rink.mallets[seat]

func apply_input(frame: Dictionary) -> void:
	var mallet: MalletBody = _mallet()
	if mallet == null or not is_instance_valid(mallet):
		return
	if frame.is_empty():
		# The release signal. Stop driving; the mallet goes back to whatever a human (or nothing) is doing.
		_last_frame = {}
		if _controller != null:
			_controller.set_scripted(false)
		return
	_last_frame = frame
	if _controller != null:
		# Live input writes the SAME input node from the pointer every net tick, so it has to stand aside while
		# the bot drives -- otherwise the two overwrite each other and neither is what goes on the wire.
		_controller.set_scripted(true)

	# THE BOT PLAYS. A policy that only swept its own end would never touch the puck, and a bench run that never
	# touches the puck reports a reconcile error of exactly zero -- which is the signature of a perfectly
	# behaved client and would be entirely wrong. So the request tracks the puck, and the policy's own motion
	# rides on top as jitter, which is what keeps two bots from settling into one repeating rally.
	var translate: Vector3 = vec3_field(frame, KEY_TRANSLATE)
	var step: float = SWEEP_SPEED * _tick_dt()
	var intent: Vector3 = _intercept(mallet.seat)
	_target = _target.lerp(intent, TRACK_GAIN) + Vector3(translate.x, 0.0, translate.z) * step
	_target = TableGeometry.clamp_to_half(_target, mallet.seat, HockeyConfig.MALLET_RADIUS)
	mallet.set_local_target(_target)

	if bool_field(frame, KEY_FIRE) and _net.rink != null:
		# Refused while the puck is live, which is the point of pressing it constantly: the validator is on the
		# measured path rather than exercised once at the start of a run.
		_net.rink.submit_serve()

func capture_input() -> Dictionary:
	# What the body actually consumed this tick. For a bot this is the frame just applied; returning it keeps a
	# recorded tape replayable through apply_input() unchanged, which is the property that matters.
	return _last_frame.duplicate(true)

func sample(_body: Node) -> Dictionary:
	var out: Dictionary = {}
	if _net == null or _net.rink == null or _net.rink.puck == null:
		return out
	# In METERS, the game's own units, as the vocabulary asks. The meter reports millimeters because that is
	# what a human reads; the CSV keeps the unit the rest of the bench uses.
	var meter: ReconcileMeter = _net.rink.puck.meter()
	out[KEY_RECONCILE_ERROR] = meter.percentile_mm(0.5) / 1000.0
	if _view != null:
		out[KEY_RECONCILE_SMOOTH] = float(_view.smoothed())
		out[KEY_RECONCILE_SNAP] = float(_view.snaps())
	return out

## Every replicated body that is not the local mallet: the other mallets, and the puck. The cadence reading
## this feeds answers "how often does a remote body's authoritative pose actually reach this client", which is
## the one thing a player complains about that no local-player metric can see.
func remote_bodies() -> Array[Node]:
	var out: Array[Node] = []
	if _net == null or _net.rink == null:
		return out
	var seat: int = _net.local_seat()
	for index: int in _net.rink.mallets.size():
		var mallet: MalletBody = _net.rink.mallets[index]
		if mallet != null and index != seat and mallet.is_occupied():
			out.push_back(mallet)
	if _net.rink.puck != null:
		out.push_back(_net.rink.puck)
	return out

# --- internals -------------------------------------------------------------------------------------
# Where this seat wants its mallet: on the puck while it is on this half, and between the puck and its own goal
# otherwise. Enough to sustain a rally; not an opponent worth beating.
func _intercept(seat: int) -> Vector3:
	var puck: PuckBody = _net.rink.puck
	var team: int = HockeyConfig.team_of_seat(seat)
	var sign_z: float = HockeyConfig.end_sign(team)
	var puck_at: Vector3 = puck.net_pos
	if puck_at.z * sign_z > 0.0:
		# On this half: meet it, but not past the point the mallet could be beaten behind.
		var reach: float = HockeyConfig.HALF_LENGTH * COMMIT_FRACTION
		return Vector3(puck_at.x, 0.0, clampf(puck_at.z * sign_z, 0.0, reach) * sign_z)
	# On the other half: cover the angle, standing off the goal line.
	return Vector3(puck_at.x * 0.5, 0.0, sign_z * HockeyConfig.HALF_LENGTH * 0.86)

# The net tick length, which under this demo's coupled configuration is the physics rate -- but the F1 lever
# changes it live, and Net.net_tick_dt() is the only thing that knows.
func _tick_dt() -> float:
	var dt: float = Net.net_tick_dt()
	return dt if dt > 0.0 else 1.0 / float(HockeyConfig.NET_TICK_HZ)
