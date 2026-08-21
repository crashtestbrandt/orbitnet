extends Node
class_name BenchMetrics
## Per-net-tick metrics recorder + gate evaluator for a netbench client. It samples the netcode health
## surfaces the facade already exposes -- clock RTT/jitter/stretch/offset ([method Net.clock_metrics]) and
## rollback resim depth + loop ms ([method Net.perf_metrics]) -- plus whatever the game contributes through
## [method BenchSubject.sample], once per net tick ([signal Net.post_tick]), streams them to a CSV artifact
## (kill-safe: flushed as it goes), and on finish runs the pure [BenchGate] to a PASS/FAIL verdict printed as
## a greppable `BENCH-RESULT` marker.
##
## TICK-DOMAIN by construction: every sample is a facade metric read at a net tick, never render-frame jerk.
## A frame-domain measurement conflates the renderer with the network and cannot be compared across machines;
## that is the single most common way a netcode bench lies.
##
## The three columns that need a game to fill them (reconcile error / smooth / snap) come from the subject
## and default to 0. A server-authoritative game with no client-side prediction has nothing to reconcile, so
## those columns are legitimately flat -- every other column still measures.
##
## Attached by [BenchProbe] from `--bench-metrics=<path>`; never present in shipped play.

const _CSV_HEADER: String = "tick,time,rtt_ms,jitter_ms,stretch,offset_ms,resim_ticks,rollback_ms,net_ms,reconcile_error,reconcile_smooth,reconcile_snap,rx_bytes_s,rx_datagrams_s,tx_bytes_s,tx_wire_bytes_s,tx_peak_peer_bytes_s,blocks_admitted_s,blocks_full_s,blocks_deferred_s,blocks_culled_s,want_full_nacks_s,starve_ticks_max,unsent_backlog_max,interest_ms,interest_entities,interarrival_near,interarrival_mid,interarrival_far,shots_fired,hits_confirmed"
const _PRINT_EVERY: int = 240      # emit a periodic BENCH line every N samples
const _FLUSH_EVERY: int = 60       # flush the CSV every N rows (bound data loss if the process is killed)

var out_path: String = ""
var profile_name: String = "clean"
var subject: BenchSubject = null

var _profile: NetProfile = null
var _file: FileAccess = null
var _rows_since_flush: int = 0
var _sample_count: int = 0
var _finished: bool = false

# Samples kept for the gate's distribution checks (percentiles / means). reconcile_snap is monotonic -> keep last.
var _rtt: Array[float] = []
var _stretch: Array[float] = []
var _resim: Array[float] = []
var _last_snap: int = 0

# The send path's own accounting, kept for the gate's distribution checks. These come from
# [method Net.bandwidth_metrics], NOT from perf_metrics() -- widening the dictionary the perf probe indexes into
# is how an unrelated harness breaks. They read zero on a peer that sends nothing, which is why the gate reports
# them rather than asserting on them.
var _rx_bytes: Array[float] = []
var _want_full: Array[float] = []
var _starve: Array[float] = []

# How often each watched remote body's pose actually changed. Fed only when the subject publishes remote bodies;
# a game that does not implement [method BenchSubject.remote_bodies] simply reports no cadence.
var _cadence: RemoteCadence = RemoteCadence.new()

# --- combat, measured as a DIFFERENCE across the window -------------------------------------------
# The game publishes monotonic run totals, so a bench window that starts mid-session subtracts its own
# first reading rather than assuming the counters began at zero.
var _shots_first: int = -1
var _shots_last: int = 0
var _confirms_first: int = -1
var _confirms_last: int = 0

# --- orientation reconciliation -------------------------------------------------------------------
var _orient_smooth_first: int = -1
var _orient_smooth_last: int = 0
var _orient_miss_first: int = -1
var _orient_miss_last: int = 0
## Running maximum over the whole run. Monotonic in the LENGTH of the run, so it cannot distinguish one
## absorbed correction from a tilt that never bled away. Reported beside the standing figure below, which
## can.
var _orient_err_max: float = 0.0
## The worst trailing-window MINIMUM seen. A residual that bleeds to zero puts a zero in every window and
## leaves this at 0; one that persists holds the window's minimum above zero. This is the figure that has
## to reach zero before an orientation arm may ship.
var _orient_err_floor: float = 0.0
var _orient_err_window: Array[float] = []
var _orient_err_window_at: int = 0
const _STANDING_WINDOW: int = 60
var _resim_max: float = 0.0
## Latched from the game's samples, so `finish()` still knows after the body is gone.
var _orient_armed: bool = false
var _target_kind: String = BenchSubject.TARGET_NONE

func _ready() -> void:
	process_mode = Node.PROCESS_MODE_ALWAYS
	_profile = NetProfiles.get_profile(profile_name)
	if _profile == null:
		_profile = NetProfiles.get_profile("clean")
	if out_path != "":
		_open_csv()
	if not Net.post_tick.is_connected(_on_post_tick):
		Net.post_tick.connect(_on_post_tick)

func _on_post_tick() -> void:
	if _finished:
		return
	var clock: Dictionary[String, float] = Net.clock_metrics()
	var perf: Dictionary[String, float] = Net.perf_metrics()
	var bw: Dictionary[String, float] = Net.bandwidth_metrics()

	# The game's own numbers, if it publishes any. Guarded on a live body: a client between death and
	# respawn has none, and a dedicated server never does.
	var game: Dictionary = {}
	if subject != null:
		var body: Node = subject.local_body()
		if body != null and is_instance_valid(body):
			game = subject.sample(body)
	var reconcile_err: float = BenchSubject.float_field(game, BenchSubject.KEY_RECONCILE_ERROR)
	var smooth: int = int(BenchSubject.float_field(game, BenchSubject.KEY_RECONCILE_SMOOTH))
	var snap: int = int(BenchSubject.float_field(game, BenchSubject.KEY_RECONCILE_SNAP))
	# Read through a typed local: `Dictionary.get` with a default widens the return to Variant.
	var resim_ticks: float = perf.get("resim_ticks", 0.0)
	if not game.is_empty():
		_sample_combat(game)
		_sample_orientation(game, resim_ticks)
		# Latched rather than read at `finish()`: by then a dead or despawned body reports nothing.
		if subject != null:
			_target_kind = subject.target_kind()

	_rtt.push_back(clock["rtt_ms"])
	_stretch.push_back(clock["stretch"])
	_resim.push_back(resim_ticks)
	_rx_bytes.push_back(bw["rx_bytes_s"])
	_want_full.push_back(bw["want_full_nacks_s"])
	_starve.push_back(bw["starve_ticks_max"])
	_last_snap = snap
	_sample_count += 1
	_sample_cadence()

	if _file != null:
		_file.store_line("%d,%.4f,%.3f,%.3f,%.5f,%.3f,%.1f,%.3f,%.3f,%.5f,%d,%d,%.0f,%.1f,%.0f,%.0f,%.0f,%.1f,%.1f,%.1f,%.1f,%.2f,%.0f,%.0f,%.3f,%.1f,%.2f,%.2f,%.2f,%d,%d" % [
			Net.current_tick(), Net.current_time(),
			clock["rtt_ms"], clock["jitter_ms"], clock["stretch"], clock["offset_ms"],
			resim_ticks, perf.get("rollback_ms", 0.0), perf.get("net_ms", 0.0),
			reconcile_err, smooth, snap,
			bw["rx_bytes_s"], bw["rx_datagrams_s"], bw["tx_bytes_s"], bw["tx_wire_bytes_s"],
			bw["tx_peak_peer_bytes_s"], bw["blocks_admitted_s"], bw["blocks_full_s"],
			bw["blocks_deferred_s"],
			bw["blocks_culled_s"], bw["want_full_nacks_s"], bw["starve_ticks_max"],
			bw["unsent_backlog_max"], bw["interest_ms"], bw["interest_entities"],
			bw["interarrival_near"], bw["interarrival_mid"], bw["interarrival_far"],
			shots_fired(), hits_confirmed()])
		_rows_since_flush += 1
		if _rows_since_flush >= _FLUSH_EVERY:
			_rows_since_flush = 0
			_file.flush()

	if _sample_count % _PRINT_EVERY == 0:
		print("BENCH: n=%d rtt_p50=%.1f rtt_p95=%.1f stretch_mean=%.4f snaps=%d rx=%.0fB/s nacks=%.2f/s shots=%d hits=%d" % [
			_sample_count, BenchGate.percentile(_rtt, 0.50), BenchGate.percentile(_rtt, 0.95),
			BenchGate.mean(_stretch), _last_snap,
			BenchGate.mean(_rx_bytes), BenchGate.mean(_want_full),
			shots_fired(), hits_confirmed()])

## Evaluate the gate over everything sampled so far, print the BENCH-RESULT marker + per-gate reasons, flush
## and close the CSV. Idempotent (the probe may call it on duration-end and again on teardown). Returns the
## verdict.
func finish() -> BenchGate.Result:
	var result: BenchGate.Result = BenchGate.evaluate(_profile, _rtt, _stretch, _resim, _last_snap)
	result = BenchGate.evaluate_bandwidth(result, _rx_bytes, _want_full, _starve)
	result = BenchGate.evaluate_remote_cadence(result, _cadence)
	result = BenchGate.evaluate_hit_registration(result, shots_fired(), hits_confirmed(), _target_kind)
	result = BenchGate.evaluate_orientation_arm(result, _orient_armed, orient_smoothed(), orient_misses(),
		peak_orient_residual(), standing_orient_residual(), _sample_count, _resim_max)
	if not _finished:
		_finished = true
		var verdict: String = "PASS" if result.passed else "FAIL"
		print("BENCH-RESULT %s profile=%s samples=%d | %s" % [
			verdict, _profile.name, _sample_count, _profile.describe()])
		for reason: String in result.reasons:
			print("  BENCH-GATE %s" % reason)
		if _file != null:
			_file.flush()
			_file.close()
			_file = null
			print("BENCH: metrics csv -> %s" % out_path)
	return result

# Two monotonic run totals from the game, held as first-and-last so the window reports a DIFFERENCE.
func _sample_combat(game: Dictionary) -> void:
	_shots_last = int(BenchSubject.float_field(game, BenchSubject.KEY_SHOTS_FIRED))
	if _shots_first < 0:
		_shots_first = _shots_last
	_confirms_last = int(BenchSubject.float_field(game, BenchSubject.KEY_HITS_CONFIRMED))
	if _confirms_first < 0:
		_confirms_first = _confirms_last

func _sample_orientation(game: Dictionary, resim_ticks: float) -> void:
	_orient_smooth_last = int(BenchSubject.float_field(game, BenchSubject.KEY_ORIENT_SMOOTH))
	if _orient_smooth_first < 0:
		_orient_smooth_first = _orient_smooth_last
	_orient_miss_last = int(BenchSubject.float_field(game, BenchSubject.KEY_ORIENT_MISS))
	if _orient_miss_first < 0:
		_orient_miss_first = _orient_miss_last
	if BenchSubject.float_field(game, BenchSubject.KEY_ORIENT_ARMED) > 0.0:
		_orient_armed = true
	var orient_err: float = BenchSubject.float_field(game, BenchSubject.KEY_ORIENT_ERROR)
	_orient_err_max = maxf(_orient_err_max, orient_err)
	_push_orient_residual(orient_err)
	_resim_max = maxf(_resim_max, resim_ticks)

# Feed one residual into the trailing window and update the standing floor. Ring-buffered, so this is two
# float writes plus, once a window is full, one pass over `_STANDING_WINDOW` floats -- and the pass exits
# early on a zero sample, because a window containing a zero cannot raise the floor.
func _push_orient_residual(err: float) -> void:
	if _orient_err_window.size() < _STANDING_WINDOW:
		_orient_err_window.push_back(err)
		if _orient_err_window.size() < _STANDING_WINDOW:
			return   # a partial window would report a floor the run has not earned
	else:
		_orient_err_window[_orient_err_window_at] = err
		_orient_err_window_at = (_orient_err_window_at + 1) % _STANDING_WINDOW
	if err <= 0.0:
		return
	var lo: float = INF
	for v: float in _orient_err_window:
		lo = minf(lo, v)
		if lo <= 0.0:
			return
	_orient_err_floor = maxf(_orient_err_floor, lo)

## Rounds this peer fired inside the measurement window.
func shots_fired() -> int:
	return maxi(0, _shots_last - maxi(_shots_first, 0))

## How many of those the server confirmed landing on something damageable.
func hits_confirmed() -> int:
	return maxi(0, _confirms_last - maxi(_confirms_first, 0))

## Orientation corrections absorbed by smoothing inside the window.
func orient_smoothed() -> int:
	return maxi(0, _orient_smooth_last - maxi(_orient_smooth_first, 0))

## Orientation corrections that found no history row to correct against, inside the window.
func orient_misses() -> int:
	return maxi(0, _orient_miss_last - maxi(_orient_miss_first, 0))

## The worst orientation residual seen at any instant. Monotonic in run length.
func peak_orient_residual() -> float:
	return _orient_err_max

## The worst trailing-window minimum: a residual that never bled away. Reaches 0 when every correction
## is eventually absorbed, whatever the peak was.
func standing_orient_residual() -> float:
	return _orient_err_floor

## The per-tick pose cadence of the remote bodies this peer is watching.
func cadence() -> RemoteCadence:
	return _cadence

## Observe every remote body the subject publishes, once per net tick.
##
## The measurement is a POSE CHANGE rather than a row arrival -- see [RemoteCadence] for why, and for the
## direction that biases the reading in. A subject that publishes nothing leaves the cadence empty, and the gate
## then reports no cadence line at all rather than a line of zeroes.
func _sample_cadence() -> void:
	if subject == null:
		return
	var bodies: Array[Node] = subject.remote_bodies()
	if bodies.is_empty():
		return
	var origin: Vector3 = Vector3.ZERO
	var own: Node = subject.local_body()
	if own != null and is_instance_valid(own):
		var own3d: Node3D = own as Node3D
		if own3d != null:
			origin = own3d.global_position
	var tick: int = Net.current_tick()
	for body: Node in bodies:
		if body == null or not is_instance_valid(body):
			continue
		var node3d: Node3D = body as Node3D
		if node3d == null:
			continue
		var pose: Vector3 = node3d.global_position
		_cadence.observe(node3d.get_instance_id(), pose, tick, origin.distance_to(pose))

func _exit_tree() -> void:
	# Backstop flush if the probe never called finish() (e.g. a hard teardown) -- don't lose the CSV.
	if _file != null:
		_file.flush()
		_file.close()
		_file = null

func _open_csv() -> void:
	var dir: String = out_path.get_base_dir()
	if dir != "" and not DirAccess.dir_exists_absolute(dir):
		DirAccess.make_dir_recursive_absolute(dir)
	_file = FileAccess.open(out_path, FileAccess.WRITE)
	if _file == null:
		push_warning("BenchMetrics: could not open '%s': %s" % [out_path, error_string(FileAccess.get_open_error())])
		return
	_file.store_line(_CSV_HEADER)
