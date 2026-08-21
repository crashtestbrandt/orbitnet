extends Control
class_name HockeyHud
## The diagnostics readout and the six levers. This is the point of the demo.
##
## A puck bouncing round a table is a pleasant thing to look at for about ten seconds. What makes it worth
## running is watching the correction -- the distance between what this peer predicted and what the server
## says -- move while you change the things that drive it. So every lever is bound to a key and the number it
## moves is on screen next to it.
##
## THE CORRECTION IS REPORTED WITH ITS FLOOR. `net_pos` rides the wire as three IEEE-754 binary16s, whose
## spacing near a table coordinate of 1 m is about a millimetre, and the backend writes the quantized value
## back after every record so that every peer replays from the same canonical basis. A correction cannot be
## measured below that spacing, so the floor is printed beside the number rather than left for a reader to
## mistake for noise.
##
## THE REJECTION LINE IS HONEST ABOUT WHERE IT WORKS. `RinkDirector.serve_rejected` fires on the peer that
## APPLIED the command -- the server -- because the library carries no rejected-command feedback back to the
## requester. On a client the line says so instead of staying suspiciously empty.

const SPARK_SAMPLES: int = 160
const PANEL_WIDTH: float = 470.0

var net: HockeyNet = null
var rink: RinkDirector = null
var controller: MalletController = null
var puck_view: PuckView = null
var mallets: MalletRenderer = null

var _label: Label = null
var _rtt_history: PackedFloat32Array = PackedFloat32Array()
var _error_history: PackedFloat32Array = PackedFloat32Array()
var _last_rejection: String = ""
var _last_goal: String = ""

# --- lever state (all per-client and live) ---------------------------------------------------------
var _tick_hz: int = HockeyConfig.NET_TICK_HZ
var _remote_resim: bool = true
var _input_delay: int = 0
var _display_offset: int = 0

func build(session: HockeyNet, director: RinkDirector, input: MalletController, view: PuckView,
		renderer: MalletRenderer) -> void:
	name = "Hud"
	net = session
	rink = director
	controller = input
	puck_view = view
	mallets = renderer
	set_anchors_preset(Control.PRESET_FULL_RECT)
	mouse_filter = Control.MOUSE_FILTER_IGNORE   # the HUD must never eat the pointer the mallet follows

	_label = Label.new()
	_label.name = "Readout"
	_label.position = Vector2(14.0, 12.0)
	_label.add_theme_font_size_override("font_size", 13)
	_label.add_theme_color_override("font_color", Color(0.88, 0.92, 1.0))
	_label.add_theme_color_override("font_outline_color", Color(0.0, 0.0, 0.0, 0.85))
	_label.add_theme_constant_override("outline_size", 4)
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(_label)

	if rink != null:
		rink.serve_rejected.connect(_on_serve_rejected)
		rink.goal_scored.connect(_on_goal)

func _process(_delta: float) -> void:
	if net == null:
		return
	_sample()
	_label.text = _compose()
	queue_redraw()

func _sample() -> void:
	# Written against the member arrays directly, NOT through a helper taking a PackedFloat32Array. Packed
	# arrays are VALUE types in GDScript: a helper would push onto its own copy and the history would never
	# grow -- a silent no-op that looks like "the sparkline is broken".
	var clock: Dictionary[String, float] = Net.clock_metrics()
	_rtt_history.push_back(clock["rtt_ms"])
	while _rtt_history.size() > SPARK_SAMPLES:
		_rtt_history.remove_at(0)
	var meter: ReconcileMeter = _meter()
	if meter != null:
		_error_history.push_back(meter.percentile_mm(0.5))
		while _error_history.size() > SPARK_SAMPLES:
			_error_history.remove_at(0)

# --- the readout -----------------------------------------------------------------------------------
func _compose() -> String:
	var clock: Dictionary[String, float] = Net.clock_metrics()
	var perf: Dictionary[String, float] = Net.perf_metrics()
	var lines: PackedStringArray = PackedStringArray()
	var seat: int = net.local_seat()

	lines.push_back("ORBITNET AIR HOCKEY   mode=%s  seat=%s  team=%s  transport=%s" % [
		Net.mode_name(Net.current_mode()),
		"spectating" if seat < 0 else str(seat),
		"-" if seat < 0 else str(HockeyConfig.team_of_seat(seat)),
		NetTransport.preferred_kind_name()])
	lines.push_back("tick=%d  %s  dt=%.2f ms  factor=%.2f" % [
		Net.current_tick(),
		"decoupled %d Hz" % Net.tickrate() if Net.is_decoupled() else "coupled %d Hz" % _tick_hz,
		Net.net_tick_dt() * 1000.0, Net.net_tick_factor()])
	lines.push_back("")

	lines.push_back("CLOCK   rtt=%.1f ms  jitter=%.1f  offset=%.1f ms  stretch=%.3f  lead=%.1f ticks" % [
		clock["rtt_ms"], clock["jitter_ms"], clock["offset_ms"], clock["stretch"], clock["lead_ticks"]])
	# Read through typed locals: Dictionary.get() returns a Variant (the default argument widens it), and
	# passing a Variant to int() is a parse error here. Assigning it to a typed local is the allowed conversion.
	var resim: float = perf.get("resim_ticks", 0.0)
	var rollback_ms: float = perf.get("rollback_ms", 0.0)
	var rb_nodes: float = perf.get("rb_nodes", 0.0)
	lines.push_back("ROLLBACK  resim=%d ticks  loop=%.2f ms  rb_nodes=%d" % [
		int(resim), rollback_ms, int(rb_nodes)])
	lines.push_back("")

	lines.push_back(_lane_line())
	lines.push_back(_correction_line())
	lines.push_back(_correction_detail())
	lines.push_back("")

	lines.push_back(_score_line())
	lines.push_back(_serve_line())
	lines.push_back("")

	lines.push_back("F1 net tick %d Hz     F2 predict-unowned %s     F3 input_delay %d" % [
		_tick_hz, "on" if _remote_resim else "off", _input_delay])
	lines.push_back("F4 display_offset %d   F5 smoothing %s            F6 team-mate fade %s" % [
		_display_offset,
		"on" if puck_view == null or puck_view.smoothing else "off",
		"on" if mallets == null or mallets.fade else "off"])
	lines.push_back("Move the pointer to drive your mallet.   SPACE serve.")
	return "\n".join(lines)

func _lane_line() -> String:
	var counts: Vector2i = Vector2i.ZERO if rink == null else rink.team_counts()
	return "LANES   rollback: %d mallets + THE PUCK   state: 1 scoreboard   command: 1 serve channel\n" \
		% HockeyConfig.SEATS \
		+ "        seated %d v %d   the puck has NO input and is predicted on every peer" % [counts.x, counts.y]

func _correction_line() -> String:
	var meter: ReconcileMeter = _meter()
	if meter == null:
		return "PUCK CORRECTION  --"
	return "PUCK CORRECTION  p50=%.1f mm  p99=%.1f mm  peak=%.1f mm  n=%d" % [
		meter.percentile_mm(0.50), meter.percentile_mm(0.99), meter.peak_mm(), meter.sample_count()]

func _correction_detail() -> String:
	var meter: ReconcileMeter = _meter()
	if meter == null:
		return ""
	var smoothed: int = 0 if puck_view == null else puck_view.smoothed()
	var snapped: int = 0 if puck_view == null else puck_view.snaps()
	var floor_mm: float = half_float_ulp_mm(HockeyConfig.HALF_LENGTH)
	return "        replayed %d of %d ticks   view: %d blended, %d snapped   wire floor ~%.2f mm (@half)" % [
		meter.corrections(), meter.visits(), smoothed, snapped, floor_mm]

func _score_line() -> String:
	if rink == null or rink.scoreboard == null or rink.puck == null:
		return "SCORE   --"
	var state: String = "live"
	if not rink.puck.is_live():
		state = "face-off in %.1f s" % (float(rink.puck.faceoff_ticks()) * _tick_seconds())
	return "SCORE   team0 %d  -  %d team1      puck: %s%s" % [
		rink.scoreboard.goals(0), rink.scoreboard.goals(1), state, _last_goal]

func _serve_line() -> String:
	if _last_rejection != "":
		return "SERVE   refused: %s" % _last_rejection
	if Net.is_client() and not Net.is_server():
		return "SERVE   refusals are only visible on the host -- the library carries no rejected-command reply"
	return "SERVE   press SPACE while the puck is dead; a live puck refuses the request"

func _meter() -> ReconcileMeter:
	if rink == null or rink.puck == null:
		return null
	return rink.puck.meter()

func _tick_seconds() -> float:
	var dt: float = Net.net_tick_dt()
	return dt if dt > 0.0 else 1.0 / float(HockeyConfig.NET_TICK_HZ)

func _on_serve_rejected(seat: int, reason: String) -> void:
	_last_rejection = "%s (seat %d)" % [reason, seat]

func _on_goal(team: int, sequence: int) -> void:
	_last_goal = "   goal #%d to team %d" % [sequence, team]

## The spacing between adjacent IEEE-754 binary16 values near `value`, in millimetres -- the floor under any
## correction this demo can measure. Static and pure so the arithmetic is unit-testable.
##
## binary16 carries a 10-bit significand, so the spacing at a magnitude in [2^e, 2^(e+1)) is 2^(e-10).
static func half_float_ulp_mm(value: float) -> float:
	var magnitude: float = absf(value)
	if magnitude <= 0.0:
		return 0.0
	var exponent: float = floor(log(magnitude) / log(2.0))
	return pow(2.0, exponent - 10.0) * 1000.0

# --- the levers ------------------------------------------------------------------------------------
func _unhandled_input(event: InputEvent) -> void:
	if not (event is InputEventKey):
		return
	var key: InputEventKey = event
	if not key.pressed or key.echo:
		return
	match key.physical_keycode:
		KEY_F1:
			# 60 <-> 30 Hz, live. Under the coupled path the physics rate does not follow, so 30 Hz is the
			# honest "half the ticks" experiment: the correction grows because each one covers twice the travel.
			_tick_hz = 30 if _tick_hz == HockeyConfig.NET_TICK_HZ else HockeyConfig.NET_TICK_HZ
			Net.set_tickrate(_tick_hz)
		KEY_F2:
			# THE HEADLINE LEVER, and its name here is what it actually does. Net.set_remote_resim(false)
			# exempts every body this peer owns neither the state nor the input of -- the other mallets AND the
			# puck -- so turning it off is exactly "stop predicting, draw what arrives".
			_remote_resim = not _remote_resim
			Net.set_remote_resim(_remote_resim)
		KEY_F3:
			_input_delay = (_input_delay + 2) % 8
			Net.set_input_delay(_input_delay)
		KEY_F4:
			_display_offset = (_display_offset + 2) % 8
			Net.set_display_offset(_display_offset)
		KEY_F5:
			if puck_view != null:
				puck_view.smoothing = not puck_view.smoothing
		KEY_F6:
			if mallets != null:
				mallets.fade = not mallets.fade

# --- drawing ---------------------------------------------------------------------------------------
func _draw() -> void:
	var top: float = 330.0
	_draw_spark(_rtt_history, Rect2(14.0, top, PANEL_WIDTH, 46.0), Color(0.45, 0.78, 1.0), "clock rtt ms")
	_draw_spark(_error_history, Rect2(14.0, top + 58.0, PANEL_WIDTH, 46.0), Color(1.0, 0.72, 0.35),
		"puck correction p50 mm")

# A polyline in _draw rather than a Line2D node: identical output, and it keeps the whole HUD one Control
# instead of a CanvasLayer with Node2D children whose coordinates would need keeping in step with it.
func _draw_spark(history: PackedFloat32Array, rect: Rect2, colour: Color, caption: String) -> void:
	draw_rect(rect, Color(0.0, 0.0, 0.0, 0.35), true)
	var font: Font = get_theme_default_font()
	if font != null:
		draw_string(font, rect.position + Vector2(4.0, -3.0), caption,
			HORIZONTAL_ALIGNMENT_LEFT, -1, 11, colour)
	if history.size() < 2:
		return
	var peak: float = 1.0
	for value: float in history:
		peak = maxf(peak, value)
	var points: PackedVector2Array = PackedVector2Array()
	var step: float = rect.size.x / float(SPARK_SAMPLES - 1)
	for index: int in history.size():
		var x: float = rect.position.x + step * float(index)
		var y: float = rect.position.y + rect.size.y * (1.0 - clampf(history[index] / peak, 0.0, 1.0))
		points.push_back(Vector2(x, y))
	draw_polyline(points, colour, 1.5)
	if font != null:
		draw_string(font, rect.position + Vector2(rect.size.x - 62.0, 12.0), "peak %.1f" % peak,
			HORIZONTAL_ALIGNMENT_LEFT, -1, 11, colour.darkened(0.15))
