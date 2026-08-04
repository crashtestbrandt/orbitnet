extends BenchSubject
class_name RtsBenchSubject
## netbench's view of this demo. Five methods, and it is the whole cost of pointing the bench at a game.
##
## THIS IS THE PROOF THAT `BenchSubject` IS A REAL SEAM AND NOT A RENAME. netbench was written for a
## first-person EVA shooter: continuous 6DOF thruster input, a predicted character body, reconcile error and
## snap counts. This game shares none of that. It has no character, no prediction to reconcile, and its
## "input" is a cursor plus occasional orders. If the seam were merely a renamed coupling, that mismatch would
## show up here as a pile of stubs that do nothing -- and instead the same five bot policies drive a real
## workload:
##
##   translate -> pans the commander's cursor, which rides the ROLLBACK lane (per-tick input, prediction,
##                reconciliation) exactly as a character's input frame did.
##   fire      -> issues an order to this seat's living units, which exercises the COMMAND lane (reliable,
##                server-validated) and then the STATE lane as 48 units change goal and start moving.
##
## So a `strafe_fire` bot under a congested profile produces continuous rollback input, periodic reliable
## commands, and sustained state churn across 96 entities -- which is a harder workload than the shooter it
## was built for, from an unmodified policy table.
##
## The one honest gap: `sample()` reports no reconcile error, because there is nothing to reconcile. The
## commander's "simulation" is a clamp, so it essentially never mispredicts. That column is legitimately flat,
## and BenchMetrics reads every field with a 0.0 default precisely so a game can leave one out.

## Metres of cursor travel per second at full stick. Fast enough that the cursor crosses the field in a few
## seconds, so a bot fleet's cursors genuinely spread out and AOI has something to cull.
const CURSOR_SPEED: float = 30.0
## Seconds between orders while the policy holds `fire`. Without a floor, a `fire` that is true for half of
## every second would submit one order per net tick and spend the whole run rate-limited by the server, which
## measures the throttle rather than the netcode.
const ORDER_INTERVAL_S: float = 1.5

var _net: RtsNet = null
var _controller: CommanderController = null
var _cursor: Vector3 = Vector3.ZERO
var _last_order_at: float = -999.0
var _last_frame: Dictionary = {}

func _init(session: RtsNet, controller: CommanderController = null) -> void:
	_net = session
	_controller = controller
	if _net != null:
		_net.local_seat_changed.connect(_on_seat_changed)

func _on_seat_changed(seat: int) -> void:
	if seat < 0:
		return
	_cursor = RtsConfig.spawn_center(seat)
	var body: Node = local_body()
	if body != null:
		subject_ready.emit(body)

# --- the BenchSubject contract --------------------------------------------------------------------
func is_ready() -> bool:
	return _net != null and _net.state() == RtsNet.State.PLAYING and _net.local_seat() >= 0 \
		and _net.world != null

## The locally-owned body is the COMMANDER, not a unit. That is the honest answer for this game: it is the
## only entity this peer authors input for, and the only one on the rollback lane.
func local_body() -> Node:
	return _commander()

## The same lookup, typed. Everything inside this class goes through it.
func _commander() -> CommanderAvatar:
	if _net == null or _net.world == null:
		return null
	var seat: int = _net.local_seat()
	if seat < 0 or seat >= _net.world.commanders.size():
		return null
	return _net.world.commanders[seat]

func apply_input(frame: Dictionary) -> void:
	# Resolved through the typed helper rather than narrowing local_body()'s `Node` return: an assignment from
	# a base type to a derived one is a downcast, and this project bans as-casts outright.
	var commander: CommanderAvatar = _commander()
	if commander == null or not is_instance_valid(commander):
		return
	if frame.is_empty():
		# The release signal. Park the cursor and stop issuing orders; the body goes back to whatever a human
		# (or nothing) is doing with it.
		_last_frame = {}
		return
	_last_frame = frame

	var translate: Vector3 = vec3_field(frame, KEY_TRANSLATE)
	_cursor += Vector3(translate.x, 0.0, translate.z) * CURSOR_SPEED * RtsConfig.NET_TICK_DT
	_cursor = UnitSteering.clamp_to_field(_cursor, 0.0)
	commander.set_local_cursor(_cursor)
	commander.set_selection_hint(RtsConfig.UNITS_PER_SEAT, _cursor, 0.0)

	if bool_field(frame, KEY_FIRE):
		_maybe_order()

func capture_input() -> Dictionary:
	# What the body actually consumed this tick. For a bot this is the frame just applied; for a human at the
	# keyboard it would be reconstructed from the live cursor. Returning the applied frame keeps a recorded
	# tape replayable through apply_input() unchanged, which is the property that matters.
	return _last_frame.duplicate(true)

func sample(_body: Node) -> Dictionary:
	# No prediction to reconcile (see the header). What this game CAN contribute is its own signature number,
	# which the CSV carries in the reconcile_error column so a bench run records it alongside the clock.
	if _controller == null:
		return {}
	return {KEY_RECONCILE_ERROR: _controller.order_rtt_percentile(0.5)}

# --- driving -------------------------------------------------------------------------------------
func _maybe_order() -> void:
	if _net == null or _net.world == null:
		return
	var now: float = float(Time.get_ticks_msec()) / 1000.0
	if now - _last_order_at < ORDER_INTERVAL_S:
		return
	_last_order_at = now
	var seat: int = _net.local_seat()
	var ids: PackedInt32Array = PackedInt32Array()
	var first: int = RtsConfig.first_id_of_seat(seat)
	for offset: int in RtsConfig.UNITS_PER_SEAT:
		var unit: UnitBody = _net.world.units[first + offset]
		if unit != null and unit.is_alive():
			ids.push_back(first + offset)
	if ids.is_empty():
		return
	# Attack-move rather than move: it keeps the fight going, so a long bench run stays in the expensive
	# steady state instead of settling into two idle armies.
	_net.world.submit_order(seat, OrderValidator.VERB_ATTACK_MOVE, ids, _cursor)
