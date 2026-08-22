extends Node
class_name RtsMain
## Boot: parse the command line, bring a session up, and wire the views to it.
##
## There is no menu. A demo whose job is to be launched twice from two terminals and stared at is better
## served by flags than by a UI, and the flags are what the probe and the bench drive anyway -- so the path CI
## exercises is the same path a human uses.
##
##   (no flags)          offline single player -- no peer, no socket, the facade stays OFFLINE
##   --host[=PORT]       listen server: authoritative AND a local player
##   --join=ADDR[:PORT]  client
##   --dedicated[=PORT]  authoritative server with no local player (headless)
##   --session=N         pin this peer's session identity, so a RESTARTED process reclaims its seat
##   --rts-probe         attach the automated gate (tools/instr/rts_probe.gd)
##   --bench             attach netbench's harness with this demo's BenchSubject
##   --quit-after=SEC    exit after N seconds (probes and smoke runs)
##
## Flags go after `--`, e.g.  godot --path demos/rts -- --join=127.0.0.1

const DEFAULT_PORT: int = NetTransport.DEFAULT_PORT

var net: RtsNet = null
var camera: CameraRig = null
var controller: CommanderController = null
var hud: RtsHud = null
var renderer: UnitRenderer = null
var markers: OrderMarkers = null
var battlefield: BattlefieldView = null

func _ready() -> void:
	# One greppable line per boot, in the shape every probe and smoke script keys on. Cheap, and it turns
	# "did it even start" into a question with an answer.
	print("RTS-BOOT godot=%s mode=%s" % [Engine.get_version_info().get("string", "?"), _mode_summary()])

	net = RtsNet.new()
	add_child(net)
	net.session_state_changed.connect(_on_session_state)
	net.local_seat_changed.connect(_on_local_seat)

	camera = CameraRig.new()
	camera.name = "CameraRig"
	add_child(camera)

	controller = CommanderController.new()
	add_child(controller)

	_start_session()

	# Views are built from the world, so they wait for it. Connecting AFTER _start_session would miss the
	# signal on the offline/host paths, which build synchronously -- hence the explicit call below.
	if net.world != null:
		_build_views()

	if _has_flag("--rts-probe"):
		_attach_probe()
	if BenchProbe.enabled():
		_attach_bench()
	var quit_after: float = _float_flag("--quit-after=", 0.0)
	if quit_after > 0.0:
		get_tree().create_timer(quit_after).timeout.connect(_quit)

func _start_session() -> void:
	# BEFORE any join: the identity is read out of the handshake, and the handshake is the first thing a
	# client sends. Net mints a random one per process, which already resumes a player who returns through
	# the same process -- this flag is for the other case, a restarted binary, where the demo has no store to
	# remember itself from. A real game persists the value instead of taking it from the command line.
	var session: int = _int_flag("--session=", 0)
	if session != 0:
		Net.set_session_id(session)
	# A build exported with the dedicated-server preset boots authoritative with NO argument. That is the
	# property that makes the server image deployable: an operator runs the binary, not a command line.
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
	var world: WorldDirector = net.world
	if world == null:
		return
	# A dedicated server draws nothing. Skipping the views is not just an optimization -- it proves the
	# netcode has no dependency on them, which is the property that lets the same build run headless.
	if Net.current_mode() == Net.Mode.SERVER:
		print("RTS: dedicated -- no views built")
		return

	battlefield = BattlefieldView.new()
	add_child(battlefield)
	battlefield.build(world.obstacles)

	renderer = UnitRenderer.new()
	add_child(renderer)
	renderer.build(world, controller)

	markers = OrderMarkers.new()
	add_child(markers)
	markers.build()
	controller.order_issued.connect(_on_order_issued)

	hud = RtsHud.new()
	add_child(hud)
	hud.build(net, world, controller)

func _on_order_issued(verb: StringName, point: Vector3) -> void:
	if markers != null:
		markers.spawn(verb, point)

func _on_session_state(state: RtsNet.State) -> void:
	print("RTS-STATE %s" % RtsNet.State.keys()[state])
	if state == RtsNet.State.ERROR:
		printerr("RTS-ERROR %s" % net.error_message())

func _on_local_seat(seat: int) -> void:
	print("RTS-SEAT %d" % seat)
	if seat < 0:
		controller.configure(net.world, camera, -1)
		return
	controller.configure(net.world, camera, seat)
	if camera != null:
		# Start looking at your own army rather than at the origin, which on this map is the contested middle.
		camera.look_at_point(RtsConfig.spawn_center(seat))

func _quit() -> void:
	print("RTS-QUIT")
	get_tree().quit(0)

# --- optional attachments -------------------------------------------------------------------------
func _attach_probe() -> void:
	# A typed assignment, not an as-cast: load() returns Resource, and narrowing by assignment is the
	# conversion this project allows.
	var script: GDScript = load("res://tools/instr/rts_probe.gd")
	if script == null:
		printerr("RTS: --rts-probe given but tools/instr/rts_probe.gd could not be loaded")
		return
	var probe: Node = script.new()
	probe.name = "RtsProbe"
	add_child(probe)

func _attach_bench() -> void:
	var probe: BenchProbe = BenchProbe.new()
	probe.name = "BenchProbe"
	probe.subject = RtsBenchSubject.new(net, controller)
	add_child(probe)

func _mode_summary() -> String:
	if OS.has_feature("dedicated_server") or _has_flag("--dedicated") \
			or _flag_value("--dedicated=", "") != "":
		return "dedicated"
	if _has_flag("--host") or _flag_value("--host=", "") != "":
		return "host"
	if _flag_value("--join=", "") != "":
		return "join"
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
