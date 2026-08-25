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
##   --resume-token=N    the token the SERVER issued this identity on a previous join. An identity alone no
##                       longer reclaims a seat: the server mints a token per identity and a rejoiner must
##                       quote it back, which is what stops a peer that merely READ somebody's session id off
##                       a roster broadcast from taking their body. A real game persists it beside the
##                       identity; this demo has no store, so the value comes from the command line and the
##                       process prints its own on `RTS-TOKEN=`
##   --observe           watch without playing: no seat, no commander, an interest center DECLARED by the
##                       server and driven by this peer's camera
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

## Whether the resume token the server issued this process has been printed. Once per process. See _log_resume_token().
var _token_logged: bool = false

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
	# BEFORE THE HANDSHAKE, like the identity, and for the same reason: the token rides the hello, and the
	# hello is the first thing a client sends. Without it a pinned identity reclaims nothing, because the
	# server refuses any claim that does not quote the token it minted for that identity.
	var token: int = _int_flag("--resume-token=", 0)
	if token != 0:
		Net.set_resume_token(token)
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
	# Asked for HERE rather than at boot, because the request is a reliable message to the server and there is
	# no server to send it to until the session is up. A listen host takes the same path and answers itself.
	if state == RtsNet.State.PLAYING and _has_flag("--observe") and not net.is_observing():
		net.request_observe(true)

## An observing peer's viewpoint follows its camera, and the camera moves every frame. Offering it every
## frame is correct and cheap: ObserverDesk decides which offers are worth a message, and the throttle is
## the reason this can be wired to _process at all.
func _process(_delta: float) -> void:
	_log_resume_token()
	if net == null or camera == null or not net.is_observing():
		return
	if net.observer.mode() == ObserverDesk.Mode.TRACKED:
		net.observe_entity(net.observer.tracked_entity())
		return
	net.observe_from(camera.position)

## Print the resume token the server issued this process, once, so the next launch can quote it back.
##
## POLLED RATHER THAN ANSWERED ON A SIGNAL. The token arrives in the WELCOME, which lands after the transport
## is up: reading it when the session first reports PLAYING returns 0. A real game reads it the same way and
## writes it beside the identity in its own store.
##
## A server mints tokens rather than holding one, and an offline session has no server to mint one, so both
## return early. So does a peer seated with identity 0 -- a token names an identity, and an anonymous seat has
## none.
func _log_resume_token() -> void:
	if _token_logged or Net.is_server() or Net.is_offline():
		return
	var token: int = Net.resume_token()
	if token == 0:
		return
	_token_logged = true
	print("RTS-TOKEN=%d session=%d" % [token, Net.session_id()])

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
