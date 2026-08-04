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

const _CSV_HEADER: String = "tick,time,rtt_ms,jitter_ms,stretch,offset_ms,resim_ticks,rollback_ms,net_ms,reconcile_error,reconcile_smooth,reconcile_snap"
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

	_rtt.push_back(clock["rtt_ms"])
	_stretch.push_back(clock["stretch"])
	_resim.push_back(perf.get("resim_ticks", 0.0))
	_last_snap = snap
	_sample_count += 1

	if _file != null:
		_file.store_line("%d,%.4f,%.3f,%.3f,%.5f,%.3f,%.1f,%.3f,%.3f,%.5f,%d,%d" % [
			Net.current_tick(), Net.current_time(),
			clock["rtt_ms"], clock["jitter_ms"], clock["stretch"], clock["offset_ms"],
			perf.get("resim_ticks", 0.0), perf.get("rollback_ms", 0.0), perf.get("net_ms", 0.0),
			reconcile_err, smooth, snap])
		_rows_since_flush += 1
		if _rows_since_flush >= _FLUSH_EVERY:
			_rows_since_flush = 0
			_file.flush()

	if _sample_count % _PRINT_EVERY == 0:
		print("BENCH: n=%d rtt_p50=%.1f rtt_p95=%.1f stretch_mean=%.4f snaps=%d" % [
			_sample_count, BenchGate.percentile(_rtt, 0.50), BenchGate.percentile(_rtt, 0.95),
			BenchGate.mean(_stretch), _last_snap])

## Evaluate the gate over everything sampled so far, print the BENCH-RESULT marker + per-gate reasons, flush
## and close the CSV. Idempotent (the probe may call it on duration-end and again on teardown). Returns the
## verdict.
func finish() -> BenchGate.Result:
	var result: BenchGate.Result = BenchGate.evaluate(_profile, _rtt, _stretch, _resim, _last_snap)
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
