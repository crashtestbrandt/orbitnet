extends Control
class_name RtsHud
## The diagnostics readout, the six levers, and the drag box. This is the point of the demo.
##
## An RTS with 96 units is a pleasant thing to look at for about ten seconds. What makes it worth running is
## being able to watch the netcode's own numbers move while you change the things that drive them -- so every
## lever is bound to a key, and the number each one moves is on screen next to it.
##
## THE HONEST AOI LINE. `Net.set_aoi_radius()` culls the ROLLBACK lane only: the backend iterates its rollback
## entities, anchors on the peer's own rollback body, and filters. State-lane entities always replicate. In
## this demo that means AOI can cull exactly one thing -- the other player's cursor -- and cannot touch the 96
## units that are actually the bandwidth. Rather than hide that behind a radius slider that appears to do
## something, the HUD says precisely what is culled and what is not. That teaches the lane distinction better
## than a demo where AOI looks like it works would.
##
## The counts here are computed LOCALLY from the same rule the backend uses (distance from this peer's cursor
## to each other rollback entity, with the same 1.25x exit hysteresis); the facade does not expose the
## server's interest sets, and a demo should not need it to.

const SPARK_SAMPLES: int = 120
const PANEL_WIDTH: float = 470.0

var net: RtsNet = null
var world: WorldDirector = null
var controller: CommanderController = null

var _label: Label = null
var _rtt_history: PackedFloat32Array = PackedFloat32Array()
var _order_history: PackedFloat32Array = PackedFloat32Array()

# --- lever state (all per-client and live) --------------------------------------------------------
var _tick_hz: int = RtsConfig.NET_TICK_HZ
var _remote_resim: bool = false
var _input_delay: int = 0
var _display_offset: int = 0
var _aoi_radius: float = 0.0
var _interpolate: bool = true

func build(session: RtsNet, director: WorldDirector, commander_controller: CommanderController) -> void:
	name = "Hud"
	net = session
	world = director
	controller = commander_controller
	set_anchors_preset(Control.PRESET_FULL_RECT)
	mouse_filter = Control.MOUSE_FILTER_IGNORE   # the HUD must never eat a selection drag

	_label = Label.new()
	_label.name = "Readout"
	_label.position = Vector2(14.0, 12.0)
	_label.add_theme_font_size_override("font_size", 13)
	_label.add_theme_color_override("font_color", Color(0.88, 0.92, 1.0))
	_label.add_theme_color_override("font_outline_color", Color(0.0, 0.0, 0.0, 0.85))
	_label.add_theme_constant_override("outline_size", 4)
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(_label)

func _process(_delta: float) -> void:
	if net == null:
		return
	if controller != null:
		controller.prune_selection()
	_sample()
	_label.text = _compose()
	queue_redraw()

func _sample() -> void:
	# Written against the member arrays directly, NOT through a helper taking a PackedFloat32Array. Packed
	# arrays are VALUE types in GDScript: a helper would push onto its own copy and the caller's history
	# would never grow -- a silent no-op that looks like "the sparkline is broken".
	var clock: Dictionary[String, float] = Net.clock_metrics()
	_rtt_history.push_back(clock["rtt_ms"])
	while _rtt_history.size() > SPARK_SAMPLES:
		_rtt_history.remove_at(0)
	if controller != null:
		_order_history.push_back(controller.order_rtt_percentile(0.5))
		while _order_history.size() > SPARK_SAMPLES:
			_order_history.remove_at(0)

# --- the readout ---------------------------------------------------------------------------------
func _compose() -> String:
	var clock: Dictionary[String, float] = Net.clock_metrics()
	var perf: Dictionary[String, float] = Net.perf_metrics()
	var lines: PackedStringArray = PackedStringArray()

	lines.push_back("ORBITNET RTS DEMO   mode=%s  seat=%s  transport=%s" % [
		Net.mode_name(Net.current_mode()),
		"none" if net.local_seat() < 0 else str(net.local_seat()),
		NetTransport.preferred_kind_name()])
	lines.push_back("tick=%d  %s  factor=%.2f  dt=%.1f ms" % [
		Net.current_tick(),
		"decoupled %d Hz" % Net.tickrate() if Net.is_decoupled() else "coupled",
		Net.net_tick_factor(), Net.net_tick_dt() * 1000.0])
	lines.push_back("")

	lines.push_back("CLOCK   rtt=%.1f ms  jitter=%.1f  offset=%.1f ms  stretch=%.3f  lead=%.1f ticks" % [
		clock["rtt_ms"], clock["jitter_ms"], clock["offset_ms"], clock["stretch"], clock["lead_ticks"]])
	# Read through typed locals: Dictionary.get() returns a Variant (the default argument widens it), and
	# passing a Variant to int() is a parse error here. Assigning it to a typed local is the allowed
	# conversion -- the same shape net.gd uses when it reads the backend's own metrics.
	var resim: float = perf.get("resim_ticks", 0.0)
	var rollback_ms: float = perf.get("rollback_ms", 0.0)
	var net_ms: float = perf.get("net_ms", 0.0)
	var rb_nodes: float = perf.get("rb_nodes", 0.0)
	lines.push_back("ROLLBACK  resim=%d ticks  loop=%.2f ms  net=%.2f ms  rb_nodes=%d" % [
		int(resim), rollback_ms, net_ms, int(rb_nodes)])
	lines.push_back("")

	# The lane split, stated as counts, because it is the architectural claim the whole demo exists to make.
	var alive: int = _alive_count()
	lines.push_back("LANES   rollback: %d commander cursors   state: %d/%d units live   command: %d order channels" % [
		RtsConfig.SEATS, alive, RtsConfig.UNIT_COUNT, RtsConfig.SEATS])
	lines.push_back("        wire: 20 B/unit  (position@half 6 + net_aux@half 6 + net_meta 8)  ~%d units/tick @1200 B" % 46)
	lines.push_back(_aoi_line())
	lines.push_back("")

	if controller != null:
		lines.push_back("ORDER RTT  p50=%.0f ms  p95=%.0f ms  n=%d      selected=%d" % [
			controller.order_rtt_percentile(0.50), controller.order_rtt_percentile(0.95),
			controller.order_rtt_samples(), controller.selection_size()])
		lines.push_back("           click -> validate -> state broadcast -> observed. Not ping.")
	lines.push_back("")

	lines.push_back("F1 net tick %d Hz    F2 remote_resim %s    F3 input_delay %d" % [
		_tick_hz, "on" if _remote_resim else "off", _input_delay])
	lines.push_back("F4 display_offset %d  F5 aoi %s              F6 interpolation %s" % [
		_display_offset,
		"off" if _aoi_radius <= 0.0 else "%.0f m" % _aoi_radius,
		"on" if _interpolate else "off"])
	lines.push_back("LMB select / drag   Shift add   Ctrl+A all   RMB move   Ctrl+RMB attack-move   S stop   H hold")
	return "\n".join(lines)

func _aoi_line() -> String:
	if _aoi_radius <= 0.0:
		return "AOI     off -- every peer receives every entity"
	var culled: int = 0
	var total: int = 0
	var mine: CommanderAvatar = _local_commander()
	if mine != null:
		for commander: CommanderAvatar in world.commanders:
			if commander == null or commander == mine:
				continue
			total += 1
			# The same rule the backend applies: enter at the radius, leave at 1.25x. Computed here only to
			# report it; the server is what actually culls.
			if mine.cmd_cursor.distance_to(commander.cmd_cursor) > _aoi_radius * 1.25:
				culled += 1
	return "AOI     %.0f m -- ROLLBACK LANE ONLY: %d/%d cursors culled, 0/%d units (state lane is never culled)" % [
		_aoi_radius, culled, total, RtsConfig.UNIT_COUNT]

func _alive_count() -> int:
	if world == null:
		return 0
	var alive: int = 0
	for unit: UnitBody in world.units:
		if unit != null and unit.is_alive():
			alive += 1
	return alive

func _local_commander() -> CommanderAvatar:
	if world == null or net == null:
		return null
	var seat: int = net.local_seat()
	if seat < 0 or seat >= world.commanders.size():
		return null
	return world.commanders[seat]

# --- the levers ----------------------------------------------------------------------------------
func _unhandled_input(event: InputEvent) -> void:
	if not (event is InputEventKey):
		return
	var key: InputEventKey = event
	if not key.pressed or key.echo:
		return
	match key.physical_keycode:
		KEY_F1:
			# 20 <-> 60 Hz, live. The single most instructive lever: at 20 Hz with interpolation off the
			# stepping is obvious, and the order RTT drops visibly at 60 while the bandwidth triples.
			_tick_hz = 60 if _tick_hz == RtsConfig.NET_TICK_HZ else RtsConfig.NET_TICK_HZ
			Net.set_tickrate(_tick_hz)
		KEY_F2:
			_remote_resim = not _remote_resim
			Net.set_remote_resim(_remote_resim)
		KEY_F3:
			_input_delay = (_input_delay + 2) % 8
			Net.set_input_delay(_input_delay)
		KEY_F4:
			_display_offset = (_display_offset + 2) % 8
			Net.set_display_offset(_display_offset)
		KEY_F5:
			# Server-side only; on a client this call is ignored by the facade, which is itself worth seeing.
			_aoi_radius = 0.0 if _aoi_radius >= 128.0 else (64.0 if _aoi_radius <= 0.0 else 128.0)
			Net.set_aoi_radius(_aoi_radius)
		KEY_F6:
			_interpolate = not _interpolate
			if world != null:
				for unit: UnitBody in world.units:
					if unit != null:
						unit.set_interpolation(_interpolate)

# --- drawing -------------------------------------------------------------------------------------
func _draw() -> void:
	_draw_drag_box()
	var top: float = 300.0
	_draw_spark(_rtt_history, Rect2(14.0, top, PANEL_WIDTH, 46.0), Color(0.45, 0.78, 1.0), "clock rtt ms")
	_draw_spark(_order_history, Rect2(14.0, top + 58.0, PANEL_WIDTH, 46.0), Color(1.0, 0.72, 0.35), "order rtt p50 ms")

func _draw_drag_box() -> void:
	if controller == null:
		return
	var rect: Rect2 = controller.drag_rect()
	if rect.size.x <= 0.0 and rect.size.y <= 0.0:
		return
	draw_rect(rect, Color(0.45, 0.85, 0.45, 0.12), true)
	draw_rect(rect, Color(0.55, 0.95, 0.55, 0.85), false, 1.0)

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
		draw_string(font, rect.position + Vector2(rect.size.x - 62.0, 12.0), "peak %.0f" % peak,
			HORIZONTAL_ALIGNMENT_LEFT, -1, 11, colour.darkened(0.15))
