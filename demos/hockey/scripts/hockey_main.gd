extends Node
class_name HockeyMain
## Boot: parse the command line, bring a session up, and wire the views to it.
##
## There is no menu. A demo whose job is to be launched three times from three terminals and stared at is
## better served by flags than by a UI, and the flags are what the bench drives anyway -- so the path a harness
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
##                       process prints its own on `HOCKEY-TOKEN=`
##   --bench             attach netbench's harness with this demo's BenchSubject
##   --quit-after=SEC    exit after N seconds (smoke runs)
##
## Flags go after `--`, e.g.  godot --path demos/hockey -- --join=127.0.0.1

const DEFAULT_PORT: int = NetTransport.DEFAULT_PORT

var net: HockeyNet = null
var controller: MalletController = null
var view: TableView = null
var mallets: MalletRenderer = null
var puck_view: PuckView = null
var hud: HockeyHud = null

## Whether the resume token this session issued has been printed. Once per process. See _log_resume_token().
var _token_logged: bool = false

func _ready() -> void:
	# One greppable line per boot, in the shape every smoke script keys on. Cheap, and it turns "did it even
	# start" into a question with an answer.
	print("HOCKEY-BOOT godot=%s mode=%s" % [
		Engine.get_version_info().get("string", "?"), _mode_summary()])

	net = HockeyNet.new()
	add_child(net)
	net.session_state_changed.connect(_on_session_state)
	net.local_seat_changed.connect(_on_local_seat)

	controller = MalletController.new()
	add_child(controller)

	_start_session()

	# Views are built from the rink, so they wait for it. Connecting AFTER _start_session would miss the seat
	# signal on the offline/host paths, which seat synchronously -- hence the explicit apply below.
	if net.rink != null:
		_build_views()

	if BenchProbe.enabled():
		_attach_bench()
	var quit_after: float = _float_flag("--quit-after=", 0.0)
	if quit_after > 0.0:
		get_tree().create_timer(quit_after).timeout.connect(_quit)

func _start_session() -> void:
	# BEFORE any join: the identity is read out of the handshake, and the handshake is the first thing a client
	# sends. `Net` mints a random one per process, which already resumes a player who returns through the same
	# process -- this flag is for the other case, a RESTARTED binary, where the demo has no store to remember
	# itself from. A real game persists the value instead of taking it from the command line.
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

## Print the resume token the server issued this process, once, so the next launch can quote it back.
##
## POLLED RATHER THAN ANSWERED ON A SIGNAL, and this node has no other per-frame work -- the token arrives in
## the WELCOME, which lands after the transport is up, so reading it when the session first reports PLAYING
## returns 0. A real game reads it the same way and writes it beside the identity in its own store.
##
## A server mints tokens rather than holding one, and an offline session has no server to mint one, so both
## return early. So does a peer seated with identity 0 -- a token names an identity, and an anonymous seat has
## none.
func _process(_delta: float) -> void:
	if _token_logged or Net.is_server() or Net.is_offline():
		return
	var token: int = Net.resume_token()
	if token == 0:
		return
	_token_logged = true
	print("HOCKEY-TOKEN=%d session=%d" % [token, Net.session_id()])

func _build_views() -> void:
	var rink: RinkDirector = net.rink
	if rink == null:
		return
	# A dedicated server draws nothing. Skipping the views is not just an optimization -- it proves the netcode
	# has no dependency on them, which is the property that lets the same build run headless.
	if Net.current_mode() == Net.Mode.SERVER:
		print("HOCKEY: dedicated -- no views built")
		return

	view = TableView.new()
	add_child(view)
	view.build(_local_team())

	mallets = MalletRenderer.new()
	view.board.add_child(mallets)
	mallets.build(rink, net)

	puck_view = PuckView.new()
	view.board.add_child(puck_view)
	puck_view.build(rink.puck)

	controller.configure(rink, view, net.local_seat())

	hud = HockeyHud.new()
	add_child(hud)
	hud.build(net, rink, controller, puck_view, mallets)

func _local_team() -> int:
	var seat: int = net.local_seat()
	return 0 if seat < 0 else HockeyConfig.team_of_seat(seat)

func _on_session_state(state: HockeyNet.State) -> void:
	print("HOCKEY-STATE %s" % HockeyNet.State.keys()[state])
	if state == HockeyNet.State.ERROR:
		printerr("HOCKEY-ERROR %s" % net.error_message())

func _on_local_seat(seat: int) -> void:
	print("HOCKEY-SEAT %d" % seat)
	if controller != null:
		controller.configure(net.rink, view, seat)
	if view != null:
		# The one thing about the framing that is not fixed, decided once here: which end faces you. Nothing
		# moves the camera during play.
		view.set_viewpoint(_local_team())

func _quit() -> void:
	print("HOCKEY-QUIT")
	get_tree().quit(0)

# --- optional attachments --------------------------------------------------------------------------
func _attach_bench() -> void:
	var probe: BenchProbe = BenchProbe.new()
	probe.name = "BenchProbe"
	probe.subject = HockeyBenchSubject.new(net, puck_view, controller)
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

# --- command line ----------------------------------------------------------------------------------
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
