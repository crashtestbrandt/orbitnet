extends Control
class_name RtsHud
## The diagnostics readout, the six levers, and the drag box. This is the point of the demo.
##
## An RTS with 96 units is a pleasant thing to look at for about ten seconds. What makes it worth running is
## being able to watch the netcode's own numbers move while you change the things that drive them -- so every
## lever is bound to a key, and the number each one moves is on screen next to it.
##
## THE AOI LINE REPORTS BOTH LANES. `Net.set_aoi_radius()` filters every entity that declares an ANCHOR. A
## rollback body anchors on its first Vector3 state property automatically; a state channel anchors where
## `NetStateHandle.set_anchor()` says, and declares nothing by default. `UnitBody.bind_net()` declares one, so
## the radius reaches all 96 units as well as the two cursors -- which is where the bandwidth actually is.
##
## That default is worth naming rather than hiding: a state channel with no anchor is ALWAYS relevant, at every
## distance, so a game that forgets the call gets a radius that appears to work and culls nothing. The line
## below prints the count per lane so the difference is visible on screen.
##
## The counts here are computed LOCALLY from the same rule the backend uses (distance from this peer's cursor
## to each anchored entity, with the same 1.25x exit hysteresis); the facade does not expose the server's
## interest sets, and a demo should not need it to.

const SPARK_SAMPLES: int = 120
const PANEL_WIDTH: float = 470.0

var net: RtsNet = null
var world: WorldDirector = null
var controller: CommanderController = null

var _label: Label = null
## Which seat's lead unit an observer is following, or -1 for its own camera. Cycled by F8.
var _watch_seat: int = -1
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
	lines.push_back(_wire_line())
	lines.push_back(_blocks_line())
	lines.push_back(_interp_line())
	lines.push_back("")

	# The lane split, stated as counts, because it is the architectural claim the whole demo exists to make.
	var alive: int = _alive_count()
	lines.push_back("LANES   rollback: %d commander cursors   state: %d/%d units live   command: %d order channels" % [
		RtsConfig.SEATS, alive, RtsConfig.UNIT_COUNT, RtsConfig.SEATS])
	lines.push_back("        wire: 20 B/unit  (position@half 6 + net_aux@half 6 + net_meta 8)  ~%d units/tick @1200 B" % 46)
	lines.push_back(_aoi_line())
	lines.push_back(_fog_line())
	lines.push_back(_observer_line())
	lines.push_back("")

	if controller != null:
		lines.push_back("ORDER RTT  p50=%.0f ms  p95=%.0f ms  n=%d      selected=%d" % [
			controller.order_rtt_percentile(0.50), controller.order_rtt_percentile(0.95),
			controller.order_rtt_samples(), controller.selection_size()])
		lines.push_back("           click -> validate -> state broadcast -> observed. Not ping.")
		# The refusal reaches the peer that ASKED, so this line works on a client. It is also what stops a
		# refused order sitting in the RTT window for four seconds before the old timeout canceled it.
		var refusal: String = controller.last_refusal()
		if refusal != "":
			lines.push_back("           last order refused: %s" % refusal)
	lines.push_back("")

	lines.push_back("F1 net tick %d Hz    F2 remote_resim %s    F3 input_delay %d" % [
		_tick_hz, "on" if _remote_resim else "off", _input_delay])
	lines.push_back("F4 display_offset %d  F5 aoi %s              F6 interpolation %s" % [
		_display_offset,
		"off" if _aoi_radius <= 0.0 else "%.0f m" % _aoi_radius,
		"on" if _interpolate else "off"])
	lines.push_back("F7 observe %s        F8 watch %s               F9 fog %s" % [
		"on" if net.is_observing() else "off",
		"camera" if net.observer.mode() == ObserverDesk.Mode.FIXED else net.observer.describe(),
		"on" if world.fog_enabled() else "off"])
	lines.push_back("LMB select / drag   Shift add   Ctrl+A all   RMB move   Ctrl+RMB attack-move   S stop   H hold")
	return "\n".join(lines)

func _aoi_line() -> String:
	if _aoi_radius <= 0.0:
		return "AOI     off -- every peer receives every entity"
	var mine: CommanderAvatar = _local_commander()
	if mine == null:
		# An observer declares its center instead of driving one. Reporting a cull it does not compute here
		# would be a guess; the declared-anchor line says where it is watching from.
		return "AOI     %.0f m -- no local cursor; this peer's center is declared, not driven" % _aoi_radius
	var center: Vector3 = mine.cmd_cursor
	var cursors: int = 0
	var cursors_culled: int = 0
	for commander: CommanderAvatar in world.commanders:
		if commander == null or commander == mine:
			continue
		cursors += 1
		if _outside_aoi(center, commander.cmd_cursor):
			cursors_culled += 1
	var units_culled: int = 0
	for unit: UnitBody in world.units:
		if unit != null and _outside_aoi(center, unit.position):
			units_culled += 1
	return "AOI     %.0f m -- rollback %d/%d cursors culled, state %d/%d units culled" % [
		_aoi_radius, cursors_culled, cursors, units_culled, RtsConfig.UNIT_COUNT]

## The rule the backend applies: enter at the radius, leave at 1.25x. Computed here only to REPORT it -- the
## server is the only peer that actually culls, because it is the only one that knows every entity.
func _outside_aoi(center: Vector3, at: Vector3) -> bool:
	return center.distance_to(at) > _aoi_radius * 1.25

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

## The wire, per second. Two columns here are facts the demo could not report before.
##
## `unproven` COUNTS REFUSED ACKNOWLEDGMENTS. The server mints a token per snapshot frame from a secret it
## never transmits and refuses any acknowledgment that does not quote it back, so a peer cannot acknowledge a
## frame that never reached it -- and therefore cannot deepen its own rewind by claiming a link it does not
## have. A clean session sits at 0. Anything else is a peer whose acks do not match what it was sent.
##
## `culled` IS WHAT THE INTEREST FILTER REFUSED, and it is the number the AOI line above is about: with the
## radius off it is 0 by construction, and turning F5 on is what makes it move.
func _wire_line() -> String:
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	return "WIRE    tx=%.0f B/s in %.0f dg/s   rx=%.0f B/s   peers=%d   in interest=%d" % [
		wire["tx_bytes_s"], wire["tx_datagrams_s"], wire["rx_bytes_s"],
		int(wire["peers"]), int(wire["interest_entities"])]

func _blocks_line() -> String:
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	return "        blocks admitted=%.0f/s deferred=%.0f/s culled=%.0f/s   unproven acks=%.0f/s   stale=%.0f/s" % [
		wire["blocks_admitted_s"], wire["blocks_deferred_s"], wire["blocks_culled_s"],
		wire["unproven_acks_s"], wire["stale_blocks_s"]]

## How far behind the server's present each peer draws its remote bodies, in net ticks -- that peer's OWN
## measured send cadence, not the session's mean.
##
## WHY PER PEER IS NOT A REFINEMENT. The byte budget is charged per peer, so a peer watching a quiet corner
## gets its rows every tick while one in the middle of the fight waits several. One pooled number is measured
## partly from the other peer's link: over-rewound above the mean, under-rewound below it. This demo does not
## rewind -- there is no hitscan to compensate -- but it is decoupled at 20 Hz, so the staleness the number
## describes is exactly what F6's interpolation is smoothing over.
func _interp_line() -> String:
	if not Net.is_server():
		return "INTERP  measured on the server; a client does not see the other peers' cadences"
	var peers: PackedInt32Array = multiplayer.get_peers()
	if peers.is_empty():
		return "INTERP  pooled %.2f ticks   (no remote peer connected)" % NetLagComp.observed_interp_ticks
	var parts: PackedStringArray = PackedStringArray()
	for peer: int in peers:
		parts.push_back("p%d=%.2f" % [peer, NetLagComp.observed_interp_for(peer)])
	return "INTERP  pooled %.2f ticks   per peer: %s   %s" % [
		NetLagComp.observed_interp_ticks, "  ".join(parts),
		"(per peer)" if Net.has_peer_interarrival() else "(POOLED -- this binary has no per-peer accessor)"]

## What fog of war is currently withholding, and from whom.
##
## THE COUNTS ARE THE SERVER'S. A veto is the only interest fact this demo cannot recompute locally to report
## it -- distance and membership are properties of the entity, readable by anyone, while a veto is a fact
## about one PAIR that only the authority holds. So a client prints what it can see rather than what is being
## kept from it, which is the honest thing for a client to say about fog anyway.
func _fog_line() -> String:
	if not Net.is_server():
		return "FOG     decided by the server -- a client is told a unit stopped being sent, never that fog is why"
	if not world.fog_enabled():
		return "FOG     off -- every seated peer receives every unit its radius admits"
	var parts: PackedStringArray = PackedStringArray()
	for seat: int in RtsConfig.SEATS:
		parts.push_back("seat %d: %d withheld" % [seat, world.fog_hidden_count(seat)])
	return "FOG     on (vision %.0f m, lost past %.0f m)   %s" % [
		ScoutPolicy.VISION_RADIUS_M, ScoutPolicy.VISION_EXIT_M, "   ".join(parts)]

## Where this peer's interest center comes from, and how many peers are watching without playing.
##
## THE TWO SOURCES ARE NOT THE SAME KIND OF FACT. A player's center is INFERRED -- read off the rollback body
## its input drives, and therefore wherever that body happens to be. An observer's is DECLARED, and a
## declaration replaces inference on both the center and the world at once, which is why the line says which
## one is in force rather than just printing a position.
func _observer_line() -> String:
	var watching: String = "%d observing" % net.observer_count() if Net.is_server() else ""
	if not net.is_observing():
		return "CENTER  inferred from this peer's own cursor   %s" % watching
	return "CENTER  DECLARED %s -- this peer drives nothing   %s" % [net.observer.describe(), watching]

## Cycle what an observer watches: its own camera, then each seat's lead unit, then back.
##
## Following a UNIT rather than a point is `set_peer_anchor_entity()`, and the difference is not cosmetic --
## a tracked entity carries its own position, so the declaration costs one message and then nothing, however
## far the unit runs. A camera point costs a message every time the observer pans far enough.
func _cycle_watch_target() -> void:
	if world == null or not net.is_observing():
		return
	_watch_seat += 1
	if _watch_seat >= RtsConfig.SEATS:
		_watch_seat = -1
		net.observe_from(_camera_point())
		return
	for unit: UnitBody in world.units:
		if unit != null and unit.seat == _watch_seat and unit.is_alive():
			net.observe_entity(unit.entity_id())
			return
	# Nothing alive on that seat to follow. Fall back to the camera rather than declaring entity 0, which the
	# facade reads as a RETRACTION.
	_watch_seat = -1
	net.observe_from(_camera_point())

func _camera_point() -> Vector3:
	var rig: CameraRig = controller.camera if controller != null else null
	return Vector3.ZERO if rig == null else rig.position

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
		KEY_F7:
			# Give up the seat and watch, or ask for it back. The server answers; this only asks.
			var observing: bool = net.is_observing()
			if not observing:
				_watch_seat = -1
				net.observe_from(_camera_point())
			net.request_observe(not observing)
		KEY_F8:
			_cycle_watch_target()
		KEY_F9:
			# Server-side only, like F5. On a client the call changes nothing, which is the security property
			# rather than a limitation: a peer cannot decide what it is allowed to receive.
			if world != null:
				world.set_fog(not world.fog_enabled())

# --- drawing -------------------------------------------------------------------------------------
func _draw() -> void:
	_draw_drag_box()
	var top: float = _spark_top()
	_draw_spark(_rtt_history, Rect2(14.0, top, PANEL_WIDTH, 46.0), Color(0.45, 0.78, 1.0), "clock rtt ms")
	_draw_spark(_order_history, Rect2(14.0, top + 58.0, PANEL_WIDTH, 46.0), Color(1.0, 0.72, 0.35), "order rtt p50 ms")

# Where the sparklines start: BELOW the readout, measured, never a constant.
#
# A fixed y is a guess about how tall the text is, and the text is not fixed -- the readout gains a line when a
# seat is taken or an order is refused, and its height also moves with the font size and the display scale. The
# guess was wrong often enough to render the readout straight over the graphs. `get_minimum_size()` asks the
# label how tall its current text actually is, which is the only number that cannot drift out of step with it.
func _spark_top() -> float:
	if _label == null:
		return 300.0
	return _label.position.y + _label.get_minimum_size().y + 22.0

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
func _draw_spark(history: PackedFloat32Array, rect: Rect2, color: Color, caption: String) -> void:
	draw_rect(rect, Color(0.0, 0.0, 0.0, 0.35), true)
	var font: Font = get_theme_default_font()
	if font != null:
		draw_string(font, rect.position + Vector2(4.0, -3.0), caption,
			HORIZONTAL_ALIGNMENT_LEFT, -1, 11, color)
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
	draw_polyline(points, color, 1.5)
	if font != null:
		draw_string(font, rect.position + Vector2(rect.size.x - 62.0, 12.0), "peak %.0f" % peak,
			HORIZONTAL_ALIGNMENT_LEFT, -1, 11, color.darkened(0.15))
