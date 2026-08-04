extends Node
class_name CommanderController
## The local player's hands: selection, orders, the replicated cursor, and the order-RTT measurement.
##
## Everything here runs on ONE peer -- the one holding the seat. Selection never leaves this machine (see
## SelectionMath); the cursor rides the commander's rollback lane; orders go out as NetCommand requests and
## come back as replicated state.
##
## ORDER RTT IS THE DEMO'S SIGNATURE NUMBER, and it is the one thing here that is not obvious.
##
## Every netcode demo shows you ping. Ping is the transport's round trip and it is not what a player feels.
## What a player feels in an RTS is CLICK-TO-ADJUDICATE: the time from releasing the mouse to the moment the
## world visibly agrees that the order happened. That path is client -> reliable RPC -> server validation ->
## authoritative state change -> state-lane broadcast (which waits for the next net tick, and for this
## entity's turn in the stalest-first rotation) -> client apply. It contains the tick rate, the send budget
## and the round-robin -- everything the levers in the HUD trade against -- and none of that is in ping.
##
## Measuring it needs no new networking: the server stamps each accepted order with a sequence number onto
## every unit it named, and that number already replicates inside net_meta. So the client records the
## sequence it saw at send time and stops the clock when any targeted unit's sequence changes.
##
## Controls (raw keys, no InputMap -- see CameraRig for why):
##   LMB drag / click      select (hold Shift to add to the selection)
##   Ctrl+A                select every living unit you own
##   RMB                   move order            Ctrl+RMB   attack-move order
##   S                     stop                  H          hold position

## Emitted the instant an order is SENT -- before the server has seen it, let alone adjudicated it. That is
## deliberate: the marker it drives is local, predicted feedback, and the gap between the marker appearing and
## the units actually turning is the order RTT made visible without reading a number.
signal order_issued(verb: StringName, point: Vector3)

## How many completed order round-trips to keep for the percentile readout.
const RTT_WINDOW: int = 64
## Pixels of pointer travel below which a drag counts as a click.
const CLICK_RADIUS_PX: float = 24.0

var world: WorldDirector = null
var camera: CameraRig = null
var seat: int = -1

var _selected: Dictionary[int, bool] = {}
var _dragging: bool = false
var _drag_start: Vector2 = Vector2.ZERO
var _drag_now: Vector2 = Vector2.ZERO

# One pending order at a time is enough to measure: orders are user-paced, and a second one before the first
# resolves means the first measurement is stale anyway (the player has moved on).
var _pending_ids: PackedInt32Array = PackedInt32Array()
var _pending_seq: PackedInt32Array = PackedInt32Array()
var _pending_sent_us: int = 0
var _rtt_ms: Array[float] = []

func _ready() -> void:
	name = "CommanderController"
	set_process_unhandled_input(true)

func configure(director: WorldDirector, rig: CameraRig, seat_index: int) -> void:
	world = director
	camera = rig
	seat = seat_index
	_selected.clear()
	_pending_ids = PackedInt32Array()

func has_seat() -> bool:
	return seat >= 0 and seat < RtsConfig.SEATS and world != null and camera != null

# --- per-frame -----------------------------------------------------------------------------------
func _process(_delta: float) -> void:
	if not has_seat():
		return
	var commander: CommanderAvatar = _commander()
	if commander == null:
		return
	# The cursor is written EVERY FRAME but captured once per net tick by the rollback lane. Writing it more
	# often than it is sampled is free and keeps the local view smooth; the lane does the rate reduction.
	var ground: Vector3 = camera.ground_under_pointer()
	commander.set_local_cursor(ground)
	_publish_selection_hint(commander, ground)
	_poll_pending_order()

func _publish_selection_hint(commander: CommanderAvatar, ground: Vector3) -> void:
	if not _dragging:
		commander.set_selection_hint(_selected.size(), ground, 0.0)
		return
	var a: Vector3 = camera.ground_at_screen(_drag_start)
	var b: Vector3 = camera.ground_at_screen(_drag_now)
	var centre: Vector3 = (a + b) * 0.5
	var half: float = maxf(absf(a.x - b.x), absf(a.z - b.z)) * 0.5
	commander.set_selection_hint(_selected.size(), centre, half)

# --- input ---------------------------------------------------------------------------------------
func _unhandled_input(event: InputEvent) -> void:
	if not has_seat():
		return
	# Each branch assigns to a TYPED local before calling. Passing the `InputEvent` straight through would be
	# an unsafe call argument, which this project promotes to an error -- and the assignment is the allowed
	# narrowing conversion, unlike an as-cast.
	if event is InputEventMouseButton:
		var button: InputEventMouseButton = event
		_on_mouse_button(button)
	elif event is InputEventMouseMotion and _dragging:
		var motion: InputEventMouseMotion = event
		_drag_now = motion.position
	elif event is InputEventKey:
		var key: InputEventKey = event
		_on_key(key)

func _on_mouse_button(button: InputEventMouseButton) -> void:
	if button.button_index == MOUSE_BUTTON_LEFT:
		if button.pressed:
			_dragging = true
			_drag_start = button.position
			_drag_now = button.position
		elif _dragging:
			_dragging = false
			_drag_now = button.position
			_commit_selection(button.shift_pressed)
	elif button.button_index == MOUSE_BUTTON_RIGHT and button.pressed:
		var verb: StringName = OrderValidator.VERB_ATTACK_MOVE if button.ctrl_pressed else OrderValidator.VERB_MOVE
		_issue(verb, camera.ground_at_screen(button.position))

func _on_key(key: InputEventKey) -> void:
	if not key.pressed or key.echo:
		return
	match key.physical_keycode:
		KEY_S:
			# S is also camera pan-down. The order only fires with a live selection, so the two never fight in
			# practice -- with nothing selected, S is purely a camera key.
			if not _selected.is_empty():
				_issue(OrderValidator.VERB_STOP, Vector3.ZERO)
		KEY_H:
			_issue(OrderValidator.VERB_HOLD, Vector3.ZERO)
		KEY_A:
			if key.ctrl_pressed:
				_select_all()

# --- selection -----------------------------------------------------------------------------------
func _commit_selection(additive: bool) -> void:
	if not additive:
		_selected.clear()
	var points: PackedVector2Array = PackedVector2Array()
	var mask: PackedByteArray = PackedByteArray()
	points.resize(world.units.size())
	mask.resize(world.units.size())
	for id: int in world.units.size():
		var unit: UnitBody = world.units[id]
		var selectable: bool = unit != null and unit.seat == seat and unit.is_alive()
		mask[id] = 1 if selectable else 0
		points[id] = camera.screen_of(unit.position) if unit != null else Vector2(-10000.0, -10000.0)

	if SelectionMath.is_click(_drag_start, _drag_now):
		var hit: int = SelectionMath.nearest_to_point(_drag_now, points, mask, CLICK_RADIUS_PX)
		if hit >= 0:
			_selected[hit] = true
		return
	var rect: Rect2 = SelectionMath.drag_rect(_drag_start, _drag_now)
	for id: int in SelectionMath.units_in_rect(rect, points, mask):
		_selected[id] = true

func _select_all() -> void:
	_selected.clear()
	var first: int = RtsConfig.first_id_of_seat(seat)
	for offset: int in RtsConfig.UNITS_PER_SEAT:
		var unit: UnitBody = world.units[first + offset]
		if unit != null and unit.is_alive():
			_selected[first + offset] = true

## The current selection as a sorted id list -- what goes into an order payload, and what the renderer draws
## rings under. Sorted so two identical selections produce identical packets.
func selection_ids() -> PackedInt32Array:
	var out: PackedInt32Array = PackedInt32Array()
	for id: int in _selected.keys():
		out.push_back(id)
	out.sort()
	return out

func selection_size() -> int:
	return _selected.size()

## Drop dead units from the selection. Called each frame by the HUD refresh; a selection full of corpses
## makes every subsequent order smaller than it looks.
func prune_selection() -> void:
	var doomed: Array[int] = []
	for id: int in _selected.keys():
		var unit: UnitBody = world.units[id] if id < world.units.size() else null
		if unit == null or not unit.is_alive():
			doomed.push_back(id)
	for id: int in doomed:
		_selected.erase(id)

# --- orders --------------------------------------------------------------------------------------
func _issue(verb: StringName, point: Vector3) -> void:
	var ids: PackedInt32Array = selection_ids()
	if ids.is_empty():
		return
	_arm_rtt(ids)
	world.submit_order(seat, verb, ids, point)
	order_issued.emit(verb, point)

# Record what the targeted units' sequence numbers were BEFORE the order went out. The measurement stops on
# the first one that changes -- not on a fixed count, because the stalest-first rotation means the units in
# one order do not all refresh on the same tick, and waiting for the last one would measure the round-robin's
# worst case rather than the latency a player perceives.
func _arm_rtt(ids: PackedInt32Array) -> void:
	_pending_ids = ids
	_pending_seq = PackedInt32Array()
	_pending_seq.resize(ids.size())
	for index: int in ids.size():
		var unit: UnitBody = world.units[ids[index]]
		_pending_seq[index] = unit.order_seq() if unit != null else 0
	_pending_sent_us = Time.get_ticks_usec()

func _poll_pending_order() -> void:
	if _pending_ids.is_empty():
		return
	for index: int in _pending_ids.size():
		var unit: UnitBody = world.units[_pending_ids[index]]
		if unit == null:
			continue
		if unit.order_seq() != _pending_seq[index]:
			var elapsed_ms: float = float(Time.get_ticks_usec() - _pending_sent_us) / 1000.0
			_rtt_ms.push_back(elapsed_ms)
			if _rtt_ms.size() > RTT_WINDOW:
				_rtt_ms.remove_at(0)
			_pending_ids = PackedInt32Array()
			return
	# A rejected order never changes any sequence, so the pending entry would linger forever and block the
	# next measurement. Time it out at a generous multiple of any plausible round trip.
	if Time.get_ticks_usec() - _pending_sent_us > 4_000_000:
		_pending_ids = PackedInt32Array()

## Order round-trip percentile over the recent window, in milliseconds. 0 with no samples yet.
func order_rtt_percentile(fraction: float) -> float:
	if _rtt_ms.is_empty():
		return 0.0
	var sorted: Array[float] = _rtt_ms.duplicate()
	sorted.sort()
	var index: int = clampi(int(round(fraction * float(sorted.size() - 1))), 0, sorted.size() - 1)
	return sorted[index]

func order_rtt_samples() -> int:
	return _rtt_ms.size()

## The live drag rectangle in screen space, or an empty rect. The HUD draws it.
func drag_rect() -> Rect2:
	if not _dragging:
		return Rect2()
	return SelectionMath.drag_rect(_drag_start, _drag_now)

func _commander() -> CommanderAvatar:
	if world == null or seat < 0 or seat >= world.commanders.size():
		return null
	return world.commanders[seat]
