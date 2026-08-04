extends MainLoop
## The OrbitNet bench UDP impairment relay (netbench) -- a standalone headless process that sits BETWEEN a client
## and the dedicated/listen server and delays/drops/duplicates/reorders raw datagrams under a [NetProfile], so the
## ENet transport is conditioned BELOW its reliability layer. This is the honest ENet conditioner (Godot/ENet ship
## no built-in one; a client just joins the relay's port instead of the server's -- zero game-code change, since
## NetManager.join / `net.join` already accept an arbitrary address:port). The pure decision logic lives in
## [PacketImpairment]; this shell owns only the UDP plumbing, so it stays a live-integration tool (exercised by the
## bench run, not a unit test).
##
## Run it (the tools/netbench harness does this for you):
##   godot --headless -s res://addons/orbitnet/bench/relay_main.gd -- \
##       --relay-listen=47810 --relay-target=127.0.0.1:47800 --relay-profile=congested_wifi --relay-seed=1
## then point clients at 127.0.0.1:47810 and the server listens on 47800 as usual.
##
## ONE relay handles MANY clients: each client that appears gets its own upstream socket toward the server (so the
## server sees N distinct peers) and its own pair of seeded [PacketImpairment] instances (client->server "up",
## server->client "down"), so per-client links are independent yet each reproducible. All clients share the one
## profile here; for per-client DIFFERENT conditions, run several relays on different ports (the architecture
## already isolates each client-session -- this is the seam that supports it).
##
## Args (all after `--`):
##   --relay-listen=<port>            UDP port clients connect to (REQUIRED)
##   --relay-target=<host>:<port>     the real server (default 127.0.0.1:47800)
##   --relay-profile=<name>           a NetProfiles catalog name (default clean)
##   --relay-seed=<int>               base RNG seed (default 1); each client/direction derives a distinct seed
##   --relay-duration=<seconds>       auto-quit after N seconds (default 0 = run until killed)
##   --relay-latency/jitter/loss/dup/reorder/reorder_ms=<value>   override individual profile knobs (one-way ms / [0,1])

const _DEFAULT_TARGET_HOST: String = "127.0.0.1"
const _DEFAULT_TARGET_PORT: int = 47800
const _MAX_SESSIONS: int = 64          # backstop against unbounded socket growth
const _POLL_DELAY_USEC: int = 200      # ~5kHz poll: fine for ms-resolution impairment without pinning a core
const _STAT_INTERVAL_MS: int = 2000    # periodic RELAY: stat line cadence

var _server: UDPServer = UDPServer.new()
var _profile: NetProfile = NetProfile.new()
var _base_seed: int = 1
var _target_host: String = _DEFAULT_TARGET_HOST
var _target_port: int = _DEFAULT_TARGET_PORT
var _listen_port: int = 0
var _duration_ms: int = 0
var _start_ms: int = 0
var _last_stat_ms: int = 0
var _session_count: int = 0
var _sessions: Array[_Session] = []
var _should_quit: bool = false
var _bound: bool = false

class _Session extends RefCounted:
	var client: PacketPeerUDP = null    # UDPServer.take_connection() -- already addressed at the real client
	var server: PacketPeerUDP = null    # our own upstream socket toward the server (distinct source per client)
	var up: PacketImpairment = null     # client -> server
	var down: PacketImpairment = null   # server -> client

func _initialize() -> void:
	_parse_args()
	_start_ms = Time.get_ticks_msec()
	_last_stat_ms = _start_ms
	if _listen_port <= 0:
		print("RELAY-RESULT FAIL: --relay-listen=<port> is required")
		_should_quit = true
		return
	var err: Error = _server.listen(_listen_port)
	if err != OK:
		print("RELAY-RESULT FAIL: could not bind udp/%d: %s" % [_listen_port, error_string(err)])
		_should_quit = true
		return
	_bound = true
	# The `bound` marker the harness waits on before launching clients.
	print("RELAY: bound listen=%d target=%s:%d seed=%d dur=%ds | %s" % [
		_listen_port, _target_host, _target_port, _base_seed,
		int(_duration_ms / 1000), _profile.describe()])

func _process(_delta: float) -> bool:
	if _should_quit:
		return true
	var now: int = Time.get_ticks_msec()
	_server.poll()   # routes client datagrams into their session peer + surfaces new connections
	_accept_new_sessions()
	for s: _Session in _sessions:
		_pump(s, now)
	if now - _last_stat_ms >= _STAT_INTERVAL_MS:
		_last_stat_ms = now
		_print_stats(now)
	if _duration_ms > 0 and now - _start_ms >= _duration_ms:
		_drain_and_finish(now)
		return true
	OS.delay_usec(_POLL_DELAY_USEC)
	return false

func _finalize() -> void:
	if _bound:
		_server.stop()

# Accept every pending client connection, pairing each with a fresh upstream socket + seeded impairment pair.
func _accept_new_sessions() -> void:
	while _server.is_connection_available() and _sessions.size() < _MAX_SESSIONS:
		var client: PacketPeerUDP = _server.take_connection()
		if client == null:
			break
		var s: _Session = _Session.new()
		s.client = client
		s.server = PacketPeerUDP.new()
		s.server.connect_to_host(_target_host, _target_port)
		s.up = PacketImpairment.new()
		s.down = PacketImpairment.new()
		# Distinct seed per client + direction, all derived from the base seed so the whole run is reproducible.
		s.up.configure(_profile, _base_seed + _session_count * 2)
		s.down.configure(_profile, _base_seed + _session_count * 2 + 1)
		_sessions.push_back(s)
		_session_count += 1
		print("RELAY: client %d connected (now %d session(s))" % [_session_count, _sessions.size()])

# Move datagrams both ways through the impairment, releasing whatever is due at `now`.
func _pump(s: _Session, now: int) -> void:
	while s.client.get_available_packet_count() > 0:
		s.up.push(s.client.get_packet(), now)
	for pkt: PackedByteArray in s.up.poll(now):
		s.server.put_packet(pkt)
	while s.server.get_available_packet_count() > 0:
		s.down.push(s.server.get_packet(), now)
	for pkt: PackedByteArray in s.down.poll(now):
		s.client.put_packet(pkt)

# Best-effort flush of anything still queued (so late packets aren't stranded), then the final RESULT marker.
func _drain_and_finish(now: int) -> void:
	# Advance the clock past the deepest pending release so poll() empties both queues.
	var flush_at: int = now + int(_profile.latency_ms) + int(_profile.jitter_ms) + int(_profile.reorder_ms) + 5
	for s: _Session in _sessions:
		for pkt: PackedByteArray in s.up.poll(flush_at):
			s.server.put_packet(pkt)
		for pkt: PackedByteArray in s.down.poll(flush_at):
			s.client.put_packet(pkt)
	_print_stats(now)
	print("RELAY-RESULT PASS: %d client(s) served over %ds under '%s'" % [
		_session_count, int((now - _start_ms) / 1000), _profile.name])

func _print_stats(_now: int) -> void:
	var up_in: int = 0
	var up_drop: int = 0
	var down_in: int = 0
	var down_drop: int = 0
	for s: _Session in _sessions:
		var u: Dictionary[String, int] = s.up.stats()
		var d: Dictionary[String, int] = s.down.stats()
		up_in += u["in"]
		up_drop += u["dropped"]
		down_in += d["in"]
		down_drop += d["dropped"]
	print("RELAY: sessions=%d up(in=%d drop=%d) down(in=%d drop=%d)" % [
		_sessions.size(), up_in, up_drop, down_in, down_drop])

# --- arg parsing -------------------------------------------------------------------------------
func _parse_args() -> void:
	var args: PackedStringArray = OS.get_cmdline_user_args()
	_listen_port = _arg_int(args, "--relay-listen=", 0)
	_base_seed = _arg_int(args, "--relay-seed=", 1)
	_duration_ms = int(1000.0 * _arg_float(args, "--relay-duration=", 0.0))
	_parse_target(_arg_str(args, "--relay-target=", "%s:%d" % [_DEFAULT_TARGET_HOST, _DEFAULT_TARGET_PORT]))
	var profile_name: String = _arg_str(args, "--relay-profile=", "clean")
	var catalog: NetProfile = NetProfiles.get_profile(profile_name)
	if catalog == null:
		print("RELAY: unknown profile '%s', falling back to clean (known: %s)" % [
			profile_name, ", ".join(NetProfiles.names())])
		catalog = NetProfiles.get_profile("clean")
	_profile = catalog
	# Per-knob overrides let the harness sweep one dimension without adding a catalog entry.
	_profile.latency_ms = _arg_float(args, "--relay-latency=", _profile.latency_ms)
	_profile.jitter_ms = _arg_float(args, "--relay-jitter=", _profile.jitter_ms)
	_profile.loss = _arg_float(args, "--relay-loss=", _profile.loss)
	_profile.dup = _arg_float(args, "--relay-dup=", _profile.dup)
	_profile.reorder = _arg_float(args, "--relay-reorder=", _profile.reorder)
	_profile.reorder_ms = _arg_float(args, "--relay-reorder_ms=", _profile.reorder_ms)

func _parse_target(spec: String) -> void:
	var parts: PackedStringArray = spec.rsplit(":", false, 1)
	if parts.size() == 2 and parts[1].is_valid_int():
		_target_host = parts[0]
		_target_port = parts[1].to_int()
	elif parts.size() == 1 and parts[0] != "":
		_target_host = parts[0]

func _arg_str(args: PackedStringArray, prefix: String, fallback: String) -> String:
	for a: String in args:
		if a.begins_with(prefix):
			return a.substr(prefix.length())
	return fallback

func _arg_int(args: PackedStringArray, prefix: String, fallback: int) -> int:
	var raw: String = _arg_str(args, prefix, "")
	return raw.to_int() if raw.is_valid_int() else fallback

func _arg_float(args: PackedStringArray, prefix: String, fallback: float) -> float:
	var raw: String = _arg_str(args, prefix, "")
	return raw.to_float() if raw.is_valid_float() else fallback
