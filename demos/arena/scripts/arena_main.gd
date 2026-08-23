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
##   --observe           watch without playing: no seat, no fighter, an interest centre and an ARENA declared
##                       by the server
##   --watch=N           which arena an observer declares itself into. Deterministic, so a probe does not have
##                       to guess which one it will be given
##   --session=N         pin this peer's session identity, so a RESTARTED process reclaims its seats
##   --props=N           state-lane props per arena, overriding the configured count (slot-table pressure)
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

func _ready() -> void:
	# One greppable line per boot, in the shape every probe and smoke script keys on. Cheap, and it turns
	# "did it even start" into a question with an answer.
	print("ARENA-BOOT godot=%s mode=%s seats=%d" % [
		Engine.get_version_info().get("string", "?"), _mode_summary(), _int_flag("--seats=", 1)])

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

## The cameras follow the seats this peer drives, or the arena it is observing.
func _process(_delta: float) -> void:
	if net == null:
		return
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
		var centre: Vector3 = ArenaGeometry.origin_of(_observed_arena())
		for index: int in views.camera_count():
			views.look_down_at(index, centre, 46.0)
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
