extends Node
class_name ArenaMain
## Boot: parse the command line, bring a session up, and wire the views to it.
##
## There is no menu. A demo whose job is to be launched three times from three terminals and stared at is
## better served by flags than by a UI, and the flags are what the probe and the bench drive anyway -- so the
## path CI exercises is the same path a human uses.
##
##   (no flags)          offline single player -- no peer, no socket, the facade stays OFFLINE
##   --host[=PORT]       listen server: authoritative AND a local player
##   --join=ADDR[:PORT]  client
##   --dedicated[=PORT]  authoritative server with no local player (headless)
##   --seats=N           ask for N locally-driven fighters (split-screen). 1 or 2.
##   --same-arena        put both seats in ONE arena. The default spreads them, which is the case worth
##                       seeing: a connection with a body in two worlds has no world of its own.
##   --observe           watch without playing: no seat, no fighter, an interest center and an ARENA declared
##                       by the server
##   --watch=N           which arena an observer declares itself into. Deterministic, so a probe does not have
##                       to guess which one it will be given
##   --session=N         pin this peer's session identity, so a RESTARTED process reclaims its seats
##   --resume-token=N    the token the SERVER issued this identity on a previous join. An identity alone no
##                       longer reclaims seats: the server mints a token per identity and a rejoiner must
##                       quote it back, which is what stops a peer that merely READ somebody's session id off
##                       a roster broadcast from taking their body. A real game persists it beside the
##                       identity; this demo has no store, so the value comes from the command line and the
##                       process prints its own on `ARENA-TOKEN=`
##   --props=N           state-lane props per arena, overriding the configured count (slot-table pressure)
##   --wire-log          print one greppable ARENA-WIRE line per second: total egress, the entity count and
##                       what the send rota did with it. A headless server has no HUD, and total egress is
##                       exactly what a slot-table or an interest change moves
##   --arena-probe       attach the automated gate (tools/instr/arena_probe.gd)
##   --bench             attach netbench's harness with this demo's BenchSubject
##   --quit-after=SEC    exit after N seconds (probes and smoke runs)
##
## Flags go after `--`, e.g.  godot --path demos/arena -- --join=127.0.0.1 --seats=2

const DEFAULT_PORT: int = NetTransport.DEFAULT_PORT

var net: ArenaNet = null
var controller: FighterController = null
var views: SplitView = null
var hud: ArenaHud = null
var arena_view: ArenaView = null
var fighters_view: FighterRenderer = null

## The arena an observer watches. Read from `--watch=N`, so a headless observer declares itself somewhere
## definite rather than wherever a HUD happened to start.
var _watch_arena: int = ArenaConfig.FIRST_ARENA_ID

## Whether to print the per-second ARENA-WIRE line, and its accumulator. See _log_wire().
var _wire_log: bool = false
var _wire_timer: float = 0.0
## Whether the resume token this server issued has been printed. Once per process. See _log_resume_token().
var _token_logged: bool = false

func _ready() -> void:
	# One greppable line per boot, in the shape every probe and smoke script keys on. Cheap, and it turns
	# "did it even start" into a question with an answer.
	print("ARENA-BOOT godot=%s mode=%s seats=%d" % [
		Engine.get_version_info().get("string", "?"), _mode_summary(), _int_flag("--seats=", 1)])

	_wire_log = _has_flag("--wire-log")
	_watch_arena = _int_flag("--watch=", ArenaConfig.FIRST_ARENA_ID)
	if not ArenaConfig.is_arena(_watch_arena):
		_watch_arena = ArenaConfig.FIRST_ARENA_ID

	net = ArenaNet.new()
	net.wanted_seats = clampi(_int_flag("--seats=", 1), 1, ArenaConfig.MAX_SEATS_PER_PEER)
	net.spread_seats = not _has_flag("--same-arena")
	net.props_per_arena = _int_flag("--props=", -1)
	add_child(net)
	net.session_state_changed.connect(_on_session_state)
	net.local_seats_changed.connect(_on_local_seats)

	controller = FighterController.new()
	controller.name = "FighterController"
	add_child(controller)
	controller.configure(net)

	_start_session()

	# Views are built from the world, so they wait for it. Connecting AFTER _start_session would miss the
	# signal on the offline/host paths, which build synchronously -- hence the explicit call below.
	if net.world != null:
		_build_views()

	if _has_flag("--arena-probe"):
		_attach_probe()
	if BenchProbe.enabled():
		_attach_bench()
	var quit_after: float = _float_flag("--quit-after=", 0.0)
	if quit_after > 0.0:
		get_tree().create_timer(quit_after).timeout.connect(_quit)

func _start_session() -> void:
	# BEFORE any join: the identity is read out of the handshake, and the handshake is the first thing a
	# client sends. `Net` mints a random one per process, which already resumes a player who returns through
	# the same process -- this flag is for the other case, a restarted binary, where the demo has no store to
	# remember itself from. A real game persists the value instead of taking it from the command line.
	var session: int = _int_flag("--session=", 0)
	if session != 0:
		Net.set_session_id(session)
	# BEFORE THE HANDSHAKE, like the identity, and for the same reason: the token rides the hello, and the
	# hello is the first thing a client sends.
	var token: int = _int_flag("--resume-token=", 0)
	if token != 0:
		Net.set_resume_token(token)
	# A build exported with the dedicated-server preset boots authoritative with NO argument. That is the
	# property that makes a server image deployable: an operator runs the binary, not a command line.
	if OS.has_feature("dedicated_server"):
		net.host_dedicated(_int_flag("--dedicated=", DEFAULT_PORT))
		return
	if _has_flag("--dedicated") or _flag_value("--dedicated=", "") != "":
		net.host_dedicated(_int_flag("--dedicated=", DEFAULT_PORT))
		return
	if _has_flag("--host") or _flag_value("--host=", "") != "":
		net.host_listen(_int_flag("--host=", DEFAULT_PORT))
		return
	var join: String = _flag_value("--join=", "")
	if join != "":
		net.join(join)
		return
	net.start_offline()

func _build_views() -> void:
	# A dedicated server draws nothing. Skipping the views is not just an optimization -- it proves the
	# netcode has no dependency on them, which is the property that lets the same build run headless.
	if Net.current_mode() == Net.Mode.SERVER:
		print("ARENA: dedicated -- no views built")
		return

	arena_view = ArenaView.new()
	add_child(arena_view)
	arena_view.build(net.world.prop_count() / maxi(1, ArenaConfig.ARENAS))

	fighters_view = FighterRenderer.new()
	add_child(fighters_view)
	fighters_view.build(net.world)

	views = SplitView.new()
	add_child(views)
	views.build(net.wanted_seats)

	hud = ArenaHud.new()
	add_child(hud)
	hud.build(net, controller, _watch_arena)

## THE TOKEN THIS SERVER ISSUED THIS IDENTITY, printed once, as soon as it exists.
##
## POLLED RATHER THAN PRINTED AT `PLAYING`, and the difference is a real one. `PLAYING` is the TRANSPORT being
## up; the token rides OrbitNet's own welcome, which is a reliable control frame the server sends after its
## handshake -- so reading it on the state change reads a zero and prints it as if it were the answer.
##
## A real game persists this beside the session id. A demo has no store, so it prints it and a relaunched
## process is handed the value its predecessor was given. Without it an identity alone reclaims nothing, which
## is the whole point of the token: reading somebody's session id off a roster broadcast must not be enough to
## take their body.
func _log_resume_token() -> void:
	if _token_logged or Net.is_server() or Net.is_offline():
		return
	var token: int = Net.resume_token()
	if token == 0:
		return
	_token_logged = true
	print("ARENA-TOKEN=%d session=%d" % [token, Net.session_id()])

## One WIRE line per second, for a run nobody is watching.
##
## `Net.bandwidth_metrics()["tx_bytes_s"]` counts EVERY datagram this peer sent -- the unreliable snapshots and
## the reliable control frames alike -- which is what makes it the number a change to the entity manifest or to
## the interest filter moves. A HUD reads it every frame; a dedicated server has no HUD, and the run that
## matters most for those two (`just arena-slots`, tens of thousands of entities) is headless by construction.
func _log_wire(delta: float) -> void:
	_wire_timer += delta
	if _wire_timer < 1.0:
		return
	_wire_timer = 0.0
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	print("ARENA-WIRE tick=%d peers=%d entities=%d tx=%.0f B/s in %.0f dg/s rx=%.0f B/s "
		% [Net.current_tick(), int(wire["peers"]), int(wire["interest_entities"]),
			wire["tx_bytes_s"], wire["tx_datagrams_s"], wire["rx_bytes_s"]]
		+ "admitted=%.0f/s deferred=%.0f/s culled=%.0f/s interest=%.2f ms"
		% [wire["blocks_admitted_s"], wire["blocks_deferred_s"], wire["blocks_culled_s"],
			wire["interest_ms"]])

## The cameras follow the seats this peer drives, or the arena it is observing.
func _process(delta: float) -> void:
	if net == null:
		return
	if _wire_log:
		_log_wire(delta)
	_log_resume_token()
	if views != null:
		_aim_cameras()
	if net.is_observing():
		# An observing peer's viewpoint is offered every frame; ObserverDesk decides which offers are worth a
		# reliable message, which is what makes wiring this to _process affordable at all.
		if net.observer.mode() == ObserverDesk.Mode.TRACKED:
			net.observe_entity(net.observer.tracked_entity(), net.observer.arena())
		else:
			net.observe_from(Vector3.ZERO, _observed_arena())

func _aim_cameras() -> void:
	if net.world == null:
		return
	if net.is_observing():
		var center: Vector3 = ArenaGeometry.origin_of(_observed_arena())
		for index: int in views.camera_count():
			views.look_down_at(index, center, 46.0)
		return
	var seats: PackedInt32Array = net.local_seats()
	for index: int in views.camera_count():
		if index >= seats.size():
			continue
		var fighter: FighterBody = net.world.fighter_at(seats[index])
		if fighter != null:
			views.look_at_fighter(index, fighter.position, fighter.net_aim)

## Which arena this peer observes: the HUD's, once a human has cycled it, and the launch flag's until then.
func _observed_arena() -> int:
	return hud.watch_arena() if hud != null else _watch_arena

func _on_session_state(state: ArenaNet.State) -> void:
	print("ARENA-STATE %s" % ArenaNet.State.keys()[state])
	if state == ArenaNet.State.ERROR:
		printerr("ARENA-ERROR %s" % net.error_message())
	# Asked for HERE rather than at boot, because the request is a reliable message to the server and there is
	# no server to send it to until the session is up. A listen host takes the same path and answers itself.
	if state == ArenaNet.State.PLAYING and _has_flag("--observe") and not net.is_observing():
		net.request_observe(true)


func _on_local_seats(seats: PackedInt32Array) -> void:
	print("ARENA-SEATS %s" % seats)

func _quit() -> void:
	print("ARENA-QUIT")
	get_tree().quit(0)

# --- optional attachments -------------------------------------------------------------------------
func _attach_probe() -> void:
	# A typed assignment, not an as-cast: load() returns Resource, and narrowing by assignment is the
	# conversion this project allows.
	var script: GDScript = load("res://tools/instr/arena_probe.gd")
	if script == null:
		printerr("ARENA: --arena-probe given but tools/instr/arena_probe.gd could not be loaded")
		return
	var probe: Node = script.new()
	probe.name = "ArenaProbe"
	add_child(probe)

func _attach_bench() -> void:
	var probe: BenchProbe = BenchProbe.new()
	probe.name = "BenchProbe"
	probe.subject = ArenaBenchSubject.new(net)
	add_child(probe)

func _mode_summary() -> String:
	if OS.has_feature("dedicated_server") or _has_flag("--dedicated") \
			or _flag_value("--dedicated=", "") != "":
		return "dedicated"
	if _has_flag("--host") or _flag_value("--host=", "") != "":
		return "host"
	if _flag_value("--join=", "") != "":
		return "observe" if _has_flag("--observe") else "join"
	return "offline"

# --- command line ---------------------------------------------------------------------------------
# Read straight off OS.get_cmdline_user_args(): everything after `--` is ours, everything before it is the
# engine's, so there is no flag namespace to collide with.
static func _has_flag(flag: String) -> bool:
	return OS.get_cmdline_user_args().has(flag)

static func _flag_value(prefix: String, fallback: String) -> String:
	for arg: String in OS.get_cmdline_user_args():
		if arg.begins_with(prefix):
			return arg.substr(prefix.length())
	return fallback

static func _int_flag(prefix: String, fallback: int) -> int:
	var raw: String = _flag_value(prefix, "")
	return raw.to_int() if raw.is_valid_int() else fallback

static func _float_flag(prefix: String, fallback: float) -> float:
	var raw: String = _flag_value(prefix, "")
	return raw.to_float() if raw.is_valid_float() else fallback
