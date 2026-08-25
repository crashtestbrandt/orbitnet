extends Control
class_name ArenaHud
## The diagnostics readout and the levers. This is the point of the demo.
##
## THREE ARENAS AND TWENTY-FOUR FIGHTERS ARE PLEASANT TO LOOK AT FOR ABOUT TEN SECONDS. What makes the demo
## worth running is watching the interest filter's own numbers move while you change the things that drive
## them -- so every lever is bound to a key and the number it moves is on screen next to it.
##
## THE INTEREST LINE IS A CLIENT'S. A server holds every entity by construction, so it reports what it holds
## and says so; a client reports what it is actually being sent, which is the only place the three filter axes
## have an observable consequence.

const PANEL_WIDTH: float = 620.0

var net: ArenaNet = null
var controller: FighterController = null

var _label: Label = null
var _bulk: bool = true
var _veto: bool = true
var _aoi_radius: float = ArenaConfig.AOI_RADIUS_M
var _watch_arena: int = ArenaConfig.FIRST_ARENA_ID
var _watch_seat: int = -1
## The most recent shot refusal in words. Empty until one arrives.
var _last_refusal: String = ""

func build(session: ArenaNet, input_controller: FighterController, watch: int) -> void:
	name = "ArenaHud"
	net = session
	controller = input_controller
	_watch_arena = watch if ArenaConfig.is_arena(watch) else ArenaConfig.FIRST_ARENA_ID
	if net != null and net.world != null:
		var shots: NetCommand = net.world.shot_channel()
		if shots != null and not shots.rejected.is_connected(_on_shot_rejected):
			shots.rejected.connect(_on_shot_rejected)
	set_anchors_preset(Control.PRESET_FULL_RECT)
	mouse_filter = Control.MOUSE_FILTER_IGNORE

	var panel: PanelContainer = PanelContainer.new()
	panel.name = "Panel"
	panel.position = Vector2(12.0, 12.0)
	panel.custom_minimum_size = Vector2(PANEL_WIDTH, 0.0)
	panel.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(panel)

	_label = Label.new()
	_label.name = "Readout"
	_label.add_theme_font_size_override("font_size", 12)
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	panel.add_child(_label)

func _process(_delta: float) -> void:
	if _label != null:
		_label.text = _compose()

# The refusal arrives as a ShotValidator.Verdict, not a sentence. `tag` names the request that failed, which
# this readout does not need -- but a game with several requests outstanding cancels exactly the one that
# failed rather than guessing by verb.
func _on_shot_rejected(_verb: StringName, code: int, _tag: int) -> void:
	_last_refusal = ShotValidator.describe(code as ShotValidator.Verdict)

# --- the readout -----------------------------------------------------------------------------------
func _compose() -> String:
	if net == null:
		return "ARENA: no session layer"
	var clock: Dictionary[String, float] = Net.clock_metrics()
	var perf: Dictionary[String, float] = Net.perf_metrics()
	var lines: PackedStringArray = PackedStringArray()

	lines.push_back("ORBITNET ARENA DEMO   mode=%s  seats=%s  transport=%s" % [
		Net.mode_name(Net.current_mode()), _seats_text(), NetTransport.preferred_kind_name()])
	lines.push_back("tick=%d  %s  dt=%.2f ms  factor=%.2f" % [
		Net.current_tick(),
		"decoupled %d Hz" % Net.tickrate() if Net.is_decoupled() else "coupled",
		Net.net_tick_dt() * 1000.0, Net.net_tick_factor()])
	lines.push_back("")

	lines.push_back("CLOCK   rtt=%.1f ms  jitter=%.1f  offset=%.1f ms  stretch=%.3f  lead=%.1f ticks" % [
		clock["rtt_ms"], clock["jitter_ms"], clock["offset_ms"], clock["stretch"], clock["lead_ticks"]])
	# Read through typed locals: Dictionary.get() returns a Variant (the default argument widens it), and
	# passing a Variant to int() is a parse error here. Assigning it to a typed local is the allowed
	# conversion -- the same shape net.gd uses when it reads the backend's own metrics.
	var resim: float = perf.get("resim_ticks", 0.0)
	var rollback_ms: float = perf.get("rollback_ms", 0.0)
	var restore_ms: float = perf.get("restore_ms", 0.0)
	var record_ms: float = perf.get("record_ms", 0.0)
	var sim_ms: float = perf.get("sim_ms", 0.0)
	lines.push_back("ROLLBACK  resim=%d ticks  loop=%.2f ms   restore=%.2f  sim=%.2f  record=%.2f" % [
		int(resim), rollback_ms, restore_ms, sim_ms, record_ms])
	lines.push_back(_marshal_line())
	lines.push_back("")

	lines.push_back(_wire_line())
	lines.push_back(_blocks_line())
	lines.push_back("")

	lines.push_back(_interest_line())
	lines.push_back(_events_line())
	lines.push_back(_arena_line())
	lines.push_back(_veto_line())
	lines.push_back(_anchor_line())
	lines.push_back("")

	lines.push_back(_rewind_line())
	lines.push_back(_rtt_line())
	lines.push_back(_interp_line())
	lines.push_back(_shot_line())
	lines.push_back("")

	lines.push_back(_score_line())
	lines.push_back("")

	lines.push_back("F1 aoi %s        F2 bulk marshalling %s      F3 cloak veto %s" % [
		"off" if _aoi_radius <= 0.0 else "%.0f m" % _aoi_radius,
		"on" if _bulk else "off",
		"on" if _veto_state() else "off"])
	lines.push_back("F4 observe %s     F5 watch %s" % [
		"on" if net.is_observing() else "off",
		"arena %d" % _watch_arena if _watch_seat < 0 else "seat %d" % _watch_seat])
	lines.push_back("WASD move  QE turn  SPACE fire     seat 2: IJKL / UO / RIGHT SHIFT")
	return "\n".join(lines)

func _seats_text() -> String:
	if net.is_observing():
		return "observing"
	var seats: PackedInt32Array = net.local_seats()
	return "none" if seats.is_empty() else str(seats)

## The wire, per second.
##
## `unproven acks` COUNTS REFUSED ACKNOWLEDGMENTS. The server mints a token per snapshot frame from a secret
## it never transmits and refuses any acknowledgment that does not quote it back, so a peer cannot claim to
## have received a frame that never reached it -- and therefore cannot deepen its own rewind by lying about
## its link. A clean session sits at 0.
func _wire_line() -> String:
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	return "WIRE    tx=%.0f B/s in %.0f dg/s   rx=%.0f B/s   peers=%d   in interest=%d   interest=%.2f ms" % [
		wire["tx_bytes_s"], wire["tx_datagrams_s"], wire["rx_bytes_s"],
		int(wire["peers"]), int(wire["interest_entities"]), wire["interest_ms"]]

func _blocks_line() -> String:
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	return "        blocks admitted=%.0f/s deferred=%.0f/s culled=%.0f/s   unproven acks=%.0f/s  stale=%.0f/s" % [
		wire["blocks_admitted_s"], wire["blocks_deferred_s"], wire["blocks_culled_s"],
		wire["unproven_acks_s"], wire["stale_blocks_s"]]

## What the token proof does NOT settle, and the bound that stands in for it.
##
## An acknowledgment that quotes the right token proves the peer received the frame it names. It does not
## prove the peer received nothing NEWER -- a client advancing its ack at full rate while holding a constant
## lag quotes a real token every time and is measured at that lag, indistinguishable from a peer behind a
## traffic shaper. No wire field closes that, so the containment is a CEILING on what the server will believe,
## and `at ceiling` is the only server-side signal an operator gets: which connections are asking for the
## deepest window in the session.
func _rtt_line() -> String:
	if not Net.is_server():
		return "RTT     measured on the server; a client never learns what window it was granted"
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	var believed: float = Net.rtt_believed_max_ms()
	var line: String = "RTT     believe at most %.0f ms   at ceiling=%d peer(s)" % [
		believed, int(wire["rtt_at_ceiling_peers"])]
	# One worked pair, so the two readings are visible as two rather than described as two.
	for peer: int in multiplayer.get_peers():
		var raw: float = Net.peer_rtt_raw_ms(peer)
		if raw >= 0.0:
			line += "   peer %d raw=%.0f believed=%.0f" % [peer, raw, Net.peer_rtt_ms(peer)]
			break
	return line

## Which lanes are actually marshalling in bulk, asked of the backend rather than of the lever.
##
## THE TWO ANSWERS CAN DISAGREE, which is why this is a readout rather than an echo of `_bulk`. A hook is
## resolved by NAME on the body's root; a name that does not resolve leaves the lane on the per-property walk
## and reports nothing at the call site.
func _marshal_line() -> String:
	if net.world == null:
		return "MARSHAL no world"
	var counts: Vector2i = net.world.bulk_counts()
	return "MARSHAL state=%d/%d  input=%d/%d fighters   (one crossing per lane per tick, %d+%d props)" % [
		counts.x, ArenaConfig.SEAT_COUNT, counts.y, ArenaConfig.SEAT_COUNT,
		FighterBody.STATE_PROPS.size(), FighterBody.INPUT_PROPS.size()]

## The same fact as the line above, arriving as EVENTS instead of as a poll.
##
## `INTEREST` counts bodies whose rows are recent; this counts the transitions the server told this peer about.
## The two answer the same question and only one of them is an EDGE -- a moment a game can act on once, rather
## than a threshold it has to re-derive every frame and de-duplicate by hand. A server holds every entity by
## construction and is told nothing, and says so.
func _events_line() -> String:
	if Net.is_server():
		return "EVENTS  interest runs here, so this peer is told nothing -- a client is the one being filtered"
	var log: InterestLog = net.interest_log
	return "EVENTS  entered=%d  left=%d  holding=%d entities   (Net.entity_left_interest / _entered_interest)" % [
		log.entered(), log.left(), log.held()]

## What this peer is actually being sent.
##
## A SERVER REPORTS WHAT IT HOLDS AND SAYS SO. Interest runs where state authority is, so the server has every
## entity by construction and a "fresh" count there would be the entity count with extra steps.
func _interest_line() -> String:
	if net.world == null:
		return "INTEREST no world"
	if Net.is_server():
		return "INTEREST server -- holds all %d entities; the filter is applied to the peers, not to itself" % [
			ArenaConfig.SEAT_COUNT + net.world.prop_count() + ArenaConfig.ARENAS]
	var reading: InterestMeter.Reading = InterestMeter.read(net.world, Net.current_tick())
	return "INTEREST receiving %d/%d entities   fighters %d/%d  props %d/%d  scorecards %d/%d" % [
		reading.total_fresh(), reading.total(),
		reading.fighters_fresh, reading.fighters_total,
		reading.props_fresh, reading.props_total,
		reading.cards_fresh, reading.cards_total]

## The membership axis, per arena. The line that makes #12's claim visible: a client in one arena receives
## fighters from that arena and none from the others, at coordinates that are identical.
func _arena_line() -> String:
	if net.world == null or Net.is_server():
		return "ARENAS  %d, each rebased to its own origin -- what replicates is ARENA-LOCAL, so no radius " % [
			ArenaConfig.ARENAS] + "can separate them"
	var reading: InterestMeter.Reading = InterestMeter.read(net.world, Net.current_tick())
	var parts: PackedStringArray = PackedStringArray()
	for offset: int in reading.fighters_by_arena.size():
		parts.push_back("arena %d: %d" % [
			ArenaConfig.FIRST_ARENA_ID + offset, reading.fighters_by_arena[offset]])
	return "ARENAS  fighters received per arena -- %s   (membership, not distance)" % "   ".join(parts)

## The veto axis. Server-side counts, because a veto is the only interest fact a client cannot recompute for
## itself -- distance and membership are properties of the entity, readable by anyone, while a veto is a fact
## about one PAIR that only the authority holds.
func _veto_line() -> String:
	if net.world == null:
		return "CLOAK   no world"
	if not Net.is_server():
		return "CLOAK   decided by the server -- a client is not told what it is not being sent"
	if not net.world.veto_enabled():
		return "CLOAK   veto off -- a cloaked fighter is sent to everybody, and is only a color"
	return "CLOAK   veto on -- %d fighter-peer pairs withheld right now (one entity, one peer, one answer)" % [
		net.world.hidden_total()]

## Where this peer's interest center comes from. A player's is INFERRED off the body its input drives; an
## observer's is DECLARED, and a declaration replaces inference on the center AND the world at once.
func _anchor_line() -> String:
	var watching: String = "%d observing" % net.observer_count() if Net.is_server() else ""
	if not net.is_observing():
		return "CENTER  inferred, one per seat, off the fighters this connection drives   %s" % watching
	return "CENTER  DECLARED %s -- this peer drives nothing   %s" % [net.observer.describe(), watching]

## The signature number of this demo: three rewind depths from one shot.
func _rewind_line() -> String:
	if net.world == null:
		return "REWIND  no world"
	if not Net.is_server():
		return "REWIND  resolved on the server; a client never learns what window it was granted"
	var meter: RewindMeter = net.world.rewind
	if meter.shots() == 0:
		return "REWIND  no shot resolved yet"
	var spread: String = "differ" if meter.bands_differ() else "equal -- no per-band measurement yet"
	return "REWIND  base=%.1f ticks   near=%.1f  mid=%.1f  far=%.1f (%s)   hits %d/%d = %.0f%%" % [
		meter.mean_base_ticks(),
		meter.mean_ticks(NetLagComp.Band.NEAR),
		meter.mean_ticks(NetLagComp.Band.MID),
		meter.mean_ticks(NetLagComp.Band.FAR),
		spread, meter.hits(), meter.shots(), meter.hit_rate() * 100.0]

## What the shot channel said back, and what batching saved.
##
## THE REFUSAL REACHES THE PEER THAT ASKED. A shot is a NetCommand, and its validator states a reason code
## rather than a bare `false`, so a client that pulled the trigger while cooling reads "still cooling" instead
## of watching a dead trigger. The reason is derived on the client from the code -- the code crosses the wire,
## the sentence does not.
func _shot_line() -> String:
	if net.world == null:
		return "SHOTS   no world"
	var batched: String = ""
	if net.world.batched_packets() > 0:
		batched = "   batched %d requests -> %d packets" % [
			net.world.batched_requests(), net.world.batched_packets()]
	if _last_refusal == "":
		return "SHOTS   no refusal yet%s" % batched
	return "SHOTS   refused: %s%s" % [_last_refusal, batched]

## How far behind the server's present each peer draws its remote bodies, in net ticks -- that peer's OWN
## measured send cadence, not the session's mean. Half of the rewind window above is this number.
func _interp_line() -> String:
	if not Net.is_server():
		return "INTERP  measured on the server; a client does not see the other peers' cadences"
	var peers: PackedInt32Array = multiplayer.get_peers()
	if peers.is_empty():
		return "INTERP  pooled %.2f ticks   (no remote peer connected)" % NetLagComp.observed_interp_ticks
	var parts: PackedStringArray = PackedStringArray()
	for peer: int in peers:
		parts.push_back("p%d=%.2f" % [peer, NetLagComp.observed_interp_for(peer)])
	return "INTERP  pooled %.2f   per peer: %s   %s" % [
		NetLagComp.observed_interp_ticks, "  ".join(parts),
		"(per peer)" if Net.has_peer_interarrival() else "(POOLED -- this binary has no per-peer accessor)"]

func _score_line() -> String:
	if net.world == null:
		return ""
	var parts: PackedStringArray = PackedStringArray()
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		var card: Scorecard = net.world.scorecard_of(arena)
		if card == null:
			continue
		var scores: PackedInt32Array = card.teams()
		parts.push_back("arena %d: %d-%d" % [arena, scores[0], scores[1]])
	return "SCORE   %s   (state lane, membership-bounded, NO anchor -- nothing to cull it by)" % [
		"   ".join(parts)]

func _veto_state() -> bool:
	return _veto if net.world == null else net.world.veto_enabled()

# --- the levers ----------------------------------------------------------------------------------
func _unhandled_input(event: InputEvent) -> void:
	if not (event is InputEventKey):
		return
	var key: InputEventKey = event
	if not key.pressed or key.echo:
		return
	match key.physical_keycode:
		KEY_F1:
			# Server-side only; on a client this call is ignored by the facade, which is itself worth seeing.
			# At 0 the distance filter is off and MEMBERSHIP still runs, which is the point of the pair.
			_aoi_radius = 0.0 if _aoi_radius > 0.0 else ArenaConfig.AOI_RADIUS_M
			Net.set_aoi_radius(_aoi_radius)
		KEY_F2:
			# Live, and it needs no agreement between peers: nothing about a hook reaches the wire.
			_bulk = not _bulk
			if net.world != null:
				net.world.set_bulk_marshalling(_bulk)
		KEY_F3:
			_veto = not _veto_state()
			if net.world != null:
				net.world.set_veto(_veto)
		KEY_F4:
			var observing: bool = net.is_observing()
			if not observing:
				_watch_seat = -1
				net.observe_from(Vector3.ZERO, _watch_arena)
			net.request_observe(not observing)
		KEY_F5:
			_cycle_watch_target()

## Cycle what an observer watches: each arena's center in turn, then one fighter in the current arena.
##
## FOLLOWING A FIGHTER IS `set_peer_anchor_entity()`, and the difference is not cosmetic -- a tracked entity
## carries its own position, so the declaration costs one message and then nothing however far it runs, while
## a fixed point costs one every time the observer moves it.
func _cycle_watch_target() -> void:
	if net.world == null or not net.is_observing():
		return
	if _watch_seat < 0:
		var first: int = ArenaConfig.first_seat_of_arena(_watch_arena)
		for step: int in ArenaConfig.SEATS_PER_ARENA:
			var fighter: FighterBody = net.world.fighter_at(first + step)
			if fighter != null and fighter.is_alive() and fighter.entity_id() != 0:
				_watch_seat = fighter.seat
				net.observe_entity(fighter.entity_id(), _watch_arena)
				return
		# Nothing alive to follow. Falling through to the next arena is better than declaring entity 0, which
		# the facade reads as a RETRACTION.
	_watch_seat = -1
	_watch_arena += 1
	if not ArenaConfig.is_arena(_watch_arena):
		_watch_arena = ArenaConfig.FIRST_ARENA_ID
	net.observe_from(Vector3.ZERO, _watch_arena)

## The arena an observer is currently watching, for the camera.
func watch_arena() -> int:
	return _watch_arena
