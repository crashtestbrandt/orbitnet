extends Node
class_name BenchProbe
## The netbench harness entry. A game attaches this into a running networked session when `--bench` is on the
## command line, having first set [member subject] to its own [BenchSubject]. It reads the `--bench-*`
## sub-flags and wires the pieces a bench client needs -- a [BenchBot] to drive the body, a [BenchMetrics]
## recorder + gate, or an [InputTape] record/replay -- then, if a duration is given, self-finishes (flush
## artifacts, print the gate verdict, quit) so the harness gets a clean per-peer result.
##
## Wiring it up, from the game's session bring-up:
##
##     if BenchProbe.enabled():
##         var probe := BenchProbe.new()
##         probe.name = "BenchProbe"
##         probe.subject = MyGameBenchSubject.new()
##         add_child(probe)
##
## Flags (all after `--`, alongside the game's own session flags):
##   --bench                     enable the bench probe (required; the others are inert without it)
##   --bench-bot=<policy>        drive the owned body with a BenchPolicy (idle|strafe|orbit|wander|strafe_fire)
##   --bench-seed=<int>          seed for the bot's motion phase (default 1) -- vary per client to de-correlate a fleet
##   --bench-metrics=<path>      stream per-tick netcode metrics to a CSV + evaluate the gate on finish
##   --bench-record=<path>       record the body's per-tick input to a tape (capture a human/bot session as a fixture)
##   --bench-replay=<path>       replay a recorded tape through the body (takes precedence over --bench-bot)
##   --bench-duration=<seconds>  after N seconds: finish metrics, save the tape, quit (0 = run until killed)
##   --bench-profile=<name>      the profile this client runs under (for the metrics RTT gate; default clean)
##
## Never present in shipped play (the flags are only set by tools/netbench). Server/dedicated peers wire it
## harmlessly -- the bot/record/replay all act on the LOCAL owned body, which a dedicated server has none of,
## so they idle while the metrics recorder still samples the facade.

var subject: BenchSubject = null

var _bot: BenchBot = null
var _metrics: BenchMetrics = null
var _record_tape: InputTape = null
var _replay_tape: InputTape = null
var _replay_index: int = 0
var _recording: bool = false
var _replaying: bool = false
var _record_path: String = ""
var _quit_on_finish: bool = false
var _finished: bool = false
var _last_tick: int = -1   # gate record/replay to ONE step per net tick (cadence-consistent under the net/physics decouple)
var _duration: float = 0.0
var _timer_started: bool = false

## Whether `--bench` was passed. The game calls this before constructing the probe, so a shipped build never
## builds bench machinery at all.
static func enabled() -> bool:
	return OS.get_cmdline_user_args().has("--bench")

func _ready() -> void:
	process_mode = Node.PROCESS_MODE_ALWAYS
	if subject == null:
		push_error("BenchProbe: no BenchSubject was set -- the bench cannot reach the game. See BenchSubject.")
		return

	var seed: int = _int_arg("--bench-seed=", 1)
	var profile_name: String = arg_value("--bench-profile=", "clean")
	var replay_path: String = arg_value("--bench-replay=", "")
	var bot_name: String = arg_value("--bench-bot=", "")
	var metrics_path: String = arg_value("--bench-metrics=", "")
	_record_path = arg_value("--bench-record=", "")
	var duration: float = _float_arg("--bench-duration=", 0.0)

	var summary: PackedStringArray = PackedStringArray()

	# Replay takes precedence over a bot (both drive apply_input); attach at most one driver.
	if replay_path != "":
		_replay_tape = InputTape.new()
		var err: Error = _replay_tape.load_from(replay_path)
		if err == OK and _replay_tape.length() > 0:
			_replaying = true
			summary.push_back("replay=%s (%d frames)" % [replay_path, _replay_tape.length()])
		else:
			push_warning("BenchProbe: could not load replay tape '%s': %s" % [replay_path, error_string(err)])
	elif bot_name != "":
		_bot = BenchBot.new()
		_bot.name = "BenchBot"
		_bot.policy = BenchPolicy.policy_from_name(bot_name)
		_bot.seed = seed
		_bot.subject = subject
		add_child(_bot)
		summary.push_back("bot=%s seed=%d" % [bot_name, seed])

	if _record_path != "":
		_record_tape = InputTape.new()
		_recording = true
		summary.push_back("record=%s" % _record_path)

	if metrics_path != "":
		_metrics = BenchMetrics.new()
		_metrics.name = "BenchMetrics"
		_metrics.out_path = metrics_path
		_metrics.profile_name = profile_name
		_metrics.subject = subject
		add_child(_metrics)
		summary.push_back("metrics=%s profile=%s" % [metrics_path, profile_name])

	if duration > 0.0:
		_quit_on_finish = true
		_duration = duration
		summary.push_back("duration=%.0fs (from first spawn)" % duration)

	if not subject.subject_ready.is_connected(_on_subject_ready):
		subject.subject_ready.connect(_on_subject_ready)
	if subject.local_body() != null:
		_maybe_start_timer()   # a listen host already has its body at attach time; start the window now

	print("BENCHPROBE: %s" % (" ".join(summary) if not summary.is_empty() else "attached (no sub-flags)"))

func _on_subject_ready(_body: Node) -> void:
	_maybe_start_timer()

# Start the measurement window on the FIRST owned-body spawn, not at _ready -- so --bench-duration is N
# seconds of steady-state post-connect samples, not partly spent on bringup (a slow connect would otherwise
# starve the gate's sample count). One-shot: a respawn does not restart the window.
func _maybe_start_timer() -> void:
	if _timer_started or _duration <= 0.0:
		return
	_timer_started = true
	get_tree().create_timer(_duration).timeout.connect(_finish)

func _physics_process(_delta: float) -> void:
	if subject == null or not subject.is_ready():
		return
	var body: Node = subject.local_body()
	if body == null or not is_instance_valid(body):
		return
	# Act once per NET tick, not per physics frame: under the net/physics decouple a per-frame advance would
	# run the tape fast and record duplicates. Keying on the tick keeps record and replay at the same cadence
	# the input was captured at, whatever either side's physics rate is.
	var tick: int = Net.current_tick()
	if tick == _last_tick:
		return
	_last_tick = tick
	if _replaying:
		var frame: Dictionary = _replay_tape.frame_at(_replay_index)
		if not frame.is_empty():
			subject.apply_input(frame)
			_replay_index += 1
		else:
			subject.apply_input({})   # tape exhausted -> hand the body back to live input
			_replaying = false
	if _recording:
		_record_tape.record(subject.capture_input())

# Finish the run: save any recording, evaluate + print the metrics gate, then quit if this was a timed run.
func _finish() -> void:
	if _finished:
		return
	_finished = true
	if _bot != null:
		_bot.release()
	if subject != null:
		subject.release()
	if _recording and _record_tape != null:
		if _record_tape.length() == 0:
			push_warning("BenchProbe: nothing was recorded -- does the subject implement capture_input()?")
		var err: Error = _record_tape.save(_record_path)
		if err == OK:
			print("BENCHPROBE: recorded %d frames -> %s" % [_record_tape.length(), _record_path])
		else:
			push_warning("BenchProbe: could not save tape '%s': %s" % [_record_path, error_string(err)])
	if _metrics != null:
		_metrics.finish()
	print("BENCHPROBE-RESULT DONE")
	if _quit_on_finish:
		get_tree().quit(0)

# --- CLI args ------------------------------------------------------------------------------------
# Read straight off OS.get_cmdline_user_args() rather than through a game's own arg helper: the bench must
# not depend on the host project having one. `--` separates engine args from user args, so everything here
# is a flag the harness passed deliberately.

## The value of a `--flag=value` user arg, or `fallback` when absent. Public so a game's own BenchSubject can
## read bench-scoped flags with the same parsing.
static func arg_value(prefix: String, fallback: String = "") -> String:
	for arg: String in OS.get_cmdline_user_args():
		if arg.begins_with(prefix):
			return arg.substr(prefix.length())
	return fallback

func _int_arg(prefix: String, fallback: int) -> int:
	var raw: String = arg_value(prefix, "")
	return raw.to_int() if raw.is_valid_int() else fallback

func _float_arg(prefix: String, fallback: float) -> float:
	var raw: String = arg_value(prefix, "")
	return raw.to_float() if raw.is_valid_float() else fallback
