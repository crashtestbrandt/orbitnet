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
## A REFUSED ORDER IS CANCELED, NOT TIMED OUT. A refusal changes no sequence number, so the measurement has
## nothing to stop on; it used to sit for four seconds and then give up, which blocked the next measurement
## for that whole window and folded every refusal into the percentile as a four-second sample.
## [signal NetCommand.rejected] now reaches the peer that asked, carrying the tag [method NetCommand.request]
## returned, so the pending entry is cleared on the reply and only the request that actually failed is
## canceled.
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
# The tag NetCommand minted for the pending order, so a refusal can name exactly that request. 0 means none.
var _pending_tag: int = 0
# The tag of the most recent refusal. Recorded rather than acted on, because on a HOST the refusal arrives
# INSIDE `submit_order()` -- before the call has returned the tag this side would have compared it against.
var _refused_tag: int = 0
var _rtt_ms: Array[float] = []
## The most recent refusal, in words, for the HUD. Empty until one arrives.
var _last_refusal: String = ""

func _ready() -> void:
	name = "CommanderController"
	set_process_unhandled_input(true)

func configure(director: WorldDirector, rig: CameraRig, seat_index: int) -> void:
	world = director
	camera = rig
	seat = seat_index
	_selected.clear()
	_clear_pending()
	_last_refusal = ""
	# Hear this seat's own refusals. The signal fires on the peer that refused the order and on the client that
	# asked for it, so a client learns the verdict on the reliable reply rather than by watching nothing happen.
	if world != null:
		var channel: NetCommand = world.order_channel(seat)
		if channel != null and not channel.rejected.is_connected(_on_order_rejected):
			channel.rejected.connect(_on_order_rejected)

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
	var center: Vector3 = (a + b) * 0.5
	var half: float = maxf(absf(a.x - b.x), absf(a.z - b.z)) * 0.5
	commander.set_selection_hint(_selected.size(), center, half)

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
	_arm_rtt(ids, world.submit_order(seat, verb, ids, point))
	order_issued.emit(verb, point)

# Record what the targeted units' sequence numbers were BEFORE the order went out. The measurement stops on
# the first one that changes -- not on a fixed count, because the stalest-first rotation means the units in
# one order do not all refresh on the same tick, and waiting for the last one would measure the round-robin's
# worst case rather than the latency a player perceives.
## Arm the measurement for the order just sent.
##
## THE REFUSAL MAY ALREADY HAVE ARRIVED. On a host the validator runs inside `submit_order()`, so
## [signal NetCommand.rejected] fires before that call returns and before this function has the tag to compare
## against -- which is why the handler records the refused tag and this checks it, rather than the other way
## round. On a client the refusal is a reply and always arrives later, where `_on_order_rejected` cancels.
func _arm_rtt(ids: PackedInt32Array, tag: int) -> void:
	if tag != 0 and tag == _refused_tag:
		_clear_pending()
		return
	_pending_ids = ids
	_pending_tag = tag
	_pending_seq = PackedInt32Array()
	_pending_seq.resize(ids.size())
	for index: int in ids.size():
		var unit: UnitBody = world.units[ids[index]]
		_pending_seq[index] = unit.order_seq() if unit != null else 0
	_pending_sent_us = Time.get_ticks_usec()

func _clear_pending() -> void:
	_pending_ids = PackedInt32Array()
	_pending_tag = 0

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
			_clear_pending()
			return
	# The BACKSTOP, and it is no longer the mechanism. A refused order never changes any sequence, and it used
	# to be canceled by this timeout alone -- four seconds of blocked measurement, and every refusal folded
	# into the percentile as a four-second sample. `_on_order_rejected` now cancels it on the reply, so what
	# is left here is the case no reply covers: an order the server accepted whose units all died before any
	# of their sequence numbers reached this client.
	if Time.get_ticks_usec() - _pending_sent_us > 4_000_000:
		_clear_pending()

# A refusal for THIS seat's channel. Canceling on the tag rather than on the verb is what keeps the
# measurement honest when a player issues a second order before the first resolves: the reply names the
# request that failed, so an older refusal cannot cancel a newer pending order.
func _on_order_rejected(_verb: StringName, code: int, tag: int) -> void:
	# TAG 0 IS NOT THIS PLAYER'S REFUSAL, with one exception. A listen host applies every peer's
	# request locally, and a refusal of one it did not mint arrives on this same lane under tag 0 --
	# so recording those put another player's forged order in the local HUD, attributed to the local
	# player. The batch-shape reply is the exception: a batch whose halves disagree has unreadable
	# tags, so 0 is the only tag the server can answer under, and dropping it left a buggy batch
	# builder invisible -- every minted tag outstanding and no refusal line ever printed. It reaches
	# the HUD and cancels nothing, because it names no request.
	if tag == 0:
		if code == NetCommand.CODE_BATCH_MALFORMED:
			_last_refusal = _describe_refusal(code)
		return
	_last_refusal = _describe_refusal(code)
	_refused_tag = tag
	if tag == _pending_tag:
		_clear_pending()

# The lane's own refusals are negative and OrderValidator.describe() answers "" for them, which made
# the HUD line blank exactly when the refusal was not the game's.
func _describe_refusal(code: int) -> String:
	match code:
		NetCommand.CODE_BATCH_TOO_LARGE:
			return "too many orders in one batch"
		NetCommand.CODE_BATCH_MALFORMED:
			return "a submitted batch was malformed"
		_:
			return OrderValidator.describe(code)

## The most recent order refusal in words, for the HUD. Empty until one arrives.
func last_refusal() -> String:
	return _last_refusal

## Order round-trip percentile over the recent window, in milliseconds. 0 with no samples yet.
func order_rtt_percentile(fraction: float) -> float:
	if _rtt_ms.is_empty():
		return 0.0
	var sorted: Array[float] = _rtt_ms.duplicate()
	sorted.sort()
	# roundi, not int(round(...)) -- round() returns Variant. See formation.gd.
	var index: int = clampi(roundi(fraction * float(sorted.size() - 1)), 0, sorted.size() - 1)
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
