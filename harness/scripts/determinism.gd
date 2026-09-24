extends Node
## The DETERMINISM scenario: two peers run the same tick range from the same input tape, and their RESTORED
## state is compared column by column, per tick.
##
## WHAT IT GATES, and what the three other probes do not. `tools/rts-probe.sh`, `tools/arena-probe.sh` and
## `tools/server-shape-probe.sh` compare a world SIGNATURE across peers, which gates deterministic node naming
## and therefore entity-id agreement. None of them gates **simulation determinism** -- whether two peers handed
## the same inputs compute the same state. The unit suites do not either: each is one function called twice with
## the same seed in one process, which is repeatability on one machine. The two failure classes this scenario
## reaches are a **float storage change** (a history column narrowed to f32) and a **quantizer landing on the
## wrong path** (a canonical value that is not what the simulation resimulates from).
## `native/crates/orbitnet-core/src/quant.rs` states the promise both would break: "resimulation stays bit-exact
## across peers (both sides simulate from canonical state)".
##
## THREE ROLES, one script, launched by `tools/determinism-probe.sh`:
##
##   --role=record --tape=PATH --frames=N   write an input tape from a seeded [BenchPolicy], then quit
##   --role=peer --label=NAME --tape=PATH   replay it, sample the restored state per tick, print the rows
##   --diverge-at=TICK                      the NEGATIVE CONTROL -- see "the injected divergence" below
##
## NO SOCKET, AND THAT IS THE POINT. Two unconnected processes have nothing to make them agree except the
## simulation being a pure function of (tape, tick). A connected pair would agree for a different reason: the
## authority's rows overwrite the client's state every tick, so a live session measures reconciliation, which
## the three probes above already gate. The wire encoding is still in the loop for the quantized column, because
## `quant.rs` canonicalizes a quantized property AT CAPTURE on whichever peer records the row -- so the value
## this scenario resimulates from is the wire-representable one whether or not a peer is listening.
##
## Two further consequences: the probe binds no UDP port, so it cannot fail for a bind reason and runs first
## among the probes; and it exits on a TICK COUNT rather than a wall-clock duration, so the sampled range is
## identical on a fast desktop and a loaded CI runner.
##
## THE SUBJECT IS DELIBERATELY DETERMINISTIC. `DetBody` advances by a fixed step, reads no frame delta, no wall
## clock and no RNG, and takes its whole input from the rollback lane's input row for the tick being run -- so a
## replayed tick is handed exactly what the fresh tick was handed. A demo whose steering documents that it does
## not require determinism would make a failure here unreadable.
##
## THE COLUMNS ARE CHOSEN FOR SENSITIVITY, one failure class each:
##
## | Column | Kind | What it catches |
## | --- | --- | --- |
## | `sim_pos` | `Vector3` | the ordinary case -- three f32 components, the shape a game actually replicates |
## | `sim_drift` | `float` | a float storage narrowing. Accumulated through `0.1`, so every low mantissa bit of the f64 is populated and an f32 row would drop them |
## | `sim_odometer` | `float` | the same narrowing by MAGNITUDE. Seeded at 2^25, where f32's ULP is 4.0, so an f32 row would swallow the per-tick increment whole and freeze the column |
## | `sim_heading` | `Vector3 @half` | the quantizer path. Its canonical value is computed at capture, and `canon=` below counts the ticks where canonicalization actually moved the value |
## | `sim_steps` | `int` | an i64 counter, the cheapest possible tick-alignment check |
##
## THREE ASSERTIONS, of which only the first is cross-peer:
##
## 1. **Per-tick agreement between the two peers.** `DET-ROW` carries one digest per column per tick -- the
##    column's exact stored bytes in hex -- and the driver reports the FIRST divergent tick and the column that
##    diverged. A boolean verdict would cost more to act on than it saves.
## 2. **The restore round trip is bit-exact** (`DET-RESTORE`). `Net.set_resim_force()` makes the rollback loop
##    rewind and replay every tick, so each sampled tick is entered many times; the state entering tick `T` must
##    be identical on every pass. A history row that does not round trip, or a restore that stops running before
##    the tick it belongs to, breaks this on one peer with no second peer needed.
## 3. **The live path and the restore path agree** (`DET-WRITEBACK`). What the simulation wrote at the end of
##    fresh tick `T-1` must come back byte-identical entering fresh tick `T`, for every lossless column. A
##    float storage narrowing breaks this even when both peers narrow identically -- which is the uniform,
##    agreed-upon error a cross-peer comparison alone cannot see.
##
## THE INJECTED DIVERGENCE. `--diverge-at=TICK` nudges `sim_drift` by ONE ULP at that tick, before the tick is
## sampled. The driver runs a third peer with it and asserts the comparison catches it AND names that exact
## tick. One ULP is the smallest difference that exists, and every failure class above is larger, so a gate that
## sees this sees those. A run with the injection still passes all three of its OWN assertions, which is the
## whole argument for the probe: the desync is invisible to either peer alone.
##
## WHAT IT DOES NOT COVER. Two limits, so neither class below is read as gated:
##
## - **One rollback entity.** The world registers a single `DetBody`, so **within-tick entity ordering** is not
##   observable here. `docs/protocol.md` records what that order is -- the loop replays one tick across every
##   planned entity in three phases, restore all, simulate all, record all, visiting entities in ascending
##   entity id -- and with one entity in the lane, a planner that stopped iterating in sorted order, or phases
##   interleaved per entity, changes nothing this scenario measures. The same page records that the order is a
##   function of the node-path hashes and is therefore identical on every peer, so a cross-peer comparison
##   cannot reach it either; a scenario that could would need two bodies whose advance reads the other's state.
## - **One machine, one build.** Both peers are the same binary on the same host, so a divergence that needs a
##   different CPU, operating system or compiler cannot appear -- `_advance` calls `sin` and `cos`, and a libm
##   whose last bit differs is exactly the class this run cannot see.
##
## COUPLED, at the harness project's 60 Hz physics rate. `--fixed-fps` is not needed and not wanted here for
## the reason the other probes state, and the tick count rather than the clock decides when the run ends.

## The input carrier. A separate node because the rollback lane splits state authority from input authority;
## with no remote peer its authority never moves, and the lane still records and restores a row per tick.
class DetInput extends Node:
	var nin_move: Vector3 = Vector3.ZERO
	var nin_turn: float = 0.0
	var nin_fire: bool = false


## The body under comparison. Every value it holds is a pure function of the input rows for ticks up to the one
## being run, which is what makes any disagreement between two peers a defect rather than a measurement.
class DetBody extends Node3D:
	## The state columns, in the order they are registered, printed and compared. Read from outside as
	## `DetBody.COLUMNS`, so the report, the driver's parser and the registration cannot drift apart.
	const COLUMNS: Array[String] = ["sim_pos", "sim_drift", "sim_odometer", "sim_heading", "sim_steps"]
	## The registration list -- COLUMNS with the wire-quantizer suffix that `sim_heading` opts into.
	const STATE_PROPS: Array[String] = [
		"sim_pos", "sim_drift", "sim_odometer", "sim_heading@half", "sim_steps"]
	const INPUT_PROPS: Array[String] = ["nin_move", "nin_turn", "nin_fire"]
	## Index of `sim_heading` in COLUMNS. The one column whose value the backend is EXPECTED to rewrite, so the
	## write-back check counts it instead of failing on it.
	const QUANTIZED_COLUMN: int = 3

	# --- the simulation's constants -----------------------------------------------------------------
	## Metres per tick of full input deflection, and the box the position is clamped into. Small and bounded,
	## so a 300-tick run stays in a range where every column is exactly representable.
	const _SPEED_M: float = 0.25
	const _BOUND_M: float = 12.0
	## Radians per tick of full turn deflection.
	const _TURN_RAD: float = 0.125
	## Exactly representable (127/128), so the decay itself contributes no rounding and `sim_drift`'s low bits
	## come from the `0.1` terms below rather than from the multiply.
	const _DRIFT_DECAY: float = 0.9921875
	const _DRIFT_GAIN: float = 0.1
	## 2^25. f32's ULP here is 4.0, so an f32 history row cannot hold the per-tick increment at all.
	const _ODOMETER_SEED: float = 33554432.0
	const _ODOMETER_STEP: float = 0.1
	## Not representable in binary16, so canonicalizing `sim_heading` always moves this component.
	const _HEADING_TILT: float = 0.1234567
	## Radians of heading per metre of position, an arbitrary non-dyadic scale.
	const _HEADING_SCALE: float = 0.37
	## What a firing tick adds to the drift. An event term, so the tape's fire flag reaches the state.
	const _FIRE_KICK: float = 0.375

	var sim_pos: Vector3 = Vector3.ZERO
	var sim_drift: float = 0.0
	var sim_odometer: float = _ODOMETER_SEED
	var sim_heading: Vector3 = Vector3(0.0, 0.0, 1.0)
	var sim_steps: int = 0

	var input: DetInput = null
	## Ticks below this are simulated but not sampled -- the rollback loop cannot rewind past the start of
	## history, so the first ticks of a session are entered a varying number of times.
	var sample_from: int = 0
	## The injected divergence's tick, -1 when off.
	var diverge_at: int = -1

	## Tick -> one mark per column, as the body ENTERED that tick. A mark is `"<digest>|<text>"`; the digest is
	## what the driver compares and the text is there for a human reading the failure.
	var entry_marks: Dictionary[int, PackedStringArray] = {}
	## How many times a sampled tick was entered again after the first time.
	var repeat_passes: int = 0
	var restore_bad_tick: int = -1
	var restore_bad_col: String = "-"
	var writeback_bad_tick: int = -1
	var writeback_bad_col: String = "-"
	## Ticks where the quantized column came back with a different value than the simulation wrote. Zero means
	## canonicalization did not run, so the quantized column is not testing the quantizer.
	var canon_writebacks: int = 0
	var fresh_ticks: int = 0
	var replay_ticks: int = 0

	## What the simulation wrote at the end of the last FRESH tick, and which tick that was. Compared against
	## the next fresh tick's entry state, which arrives through restore-and-replay rather than through the live
	## path.
	var _written_marks: PackedStringArray = PackedStringArray()
	var _written_tick: int = -1

	## The rollback tick, run by the backend on every simulated body. The sampling happens here rather than in
	## `_process` because the state that matters is the one the lane RESTORED for this tick, and that value is
	## live only between the restore and the next advance.
	func _rollback_tick(_delta: float, tick: int, is_fresh: bool) -> void:
		if is_fresh:
			fresh_ticks += 1
			_check_writeback(tick)
		else:
			replay_ticks += 1
		# Injected AFTER the write-back check and BEFORE the sample, so it lands on the value this tick is
		# compared on, and so it is applied identically on every pass over this tick.
		if tick == diverge_at:
			sim_drift = _one_ulp_up(sim_drift)
		if tick >= sample_from:
			var marks: PackedStringArray = column_marks()
			if entry_marks.has(tick):
				repeat_passes += 1
				_check_restore(tick, marks)
			else:
				entry_marks[tick] = marks
		_advance()
		if is_fresh:
			_written_marks = column_marks()
			_written_tick = tick

	## One step of the simulation. Pure in (state, input row): no delta, no wall clock, no RNG, no engine state.
	func _advance() -> void:
		var move: Vector3 = input.nin_move
		var turn: float = input.nin_turn
		sim_pos = (sim_pos + move * _SPEED_M).clamp(
			Vector3(-_BOUND_M, -_BOUND_M, -_BOUND_M), Vector3(_BOUND_M, _BOUND_M, _BOUND_M))
		var kick: float = _FIRE_KICK if input.nin_fire else 0.0
		sim_drift = sim_drift * _DRIFT_DECAY + (float(sim_pos.x) + turn * _TURN_RAD + kick) * _DRIFT_GAIN
		sim_odometer = sim_odometer + _ODOMETER_STEP
		var angle: float = float(sim_pos.z) * _HEADING_SCALE
		sim_heading = Vector3(sin(angle), _HEADING_TILT, cos(angle))
		sim_steps += 1

	## One mark per column, in COLUMNS order.
	func column_marks() -> PackedStringArray:
		var out: PackedStringArray = PackedStringArray()
		out.push_back(_mark(sim_pos, _vec_text(sim_pos)))
		out.push_back(_mark(sim_drift, _float_text(sim_drift)))
		out.push_back(_mark(sim_odometer, _float_text(sim_odometer)))
		out.push_back(_mark(sim_heading, _vec_text(sim_heading)))
		out.push_back(_mark(sim_steps, str(sim_steps)))
		return out

	## The state entering tick `tick` must be what every earlier pass over it saw. The loop rewinds and replays
	## each tick `resim_force` times, so this is the restore round trip asserted against itself. Only sampled
	## ticks are checked, for the reason `sample_from` states.
	func _check_restore(tick: int, marks: PackedStringArray) -> void:
		if restore_bad_tick >= 0:
			return
		var was: PackedStringArray = entry_marks[tick]
		for index: int in marks.size():
			if was[index] != marks[index]:
				restore_bad_tick = tick
				restore_bad_col = COLUMNS[index]
				return

	## The live path and the restore path, compared. Entering fresh tick `tick` the body holds what the replay
	## of `tick - 1` produced; the simulation itself wrote its own answer for that tick a moment earlier. Every
	## lossless column must match byte for byte. The quantized one is expected to differ and is counted.
	##
	## Sampled ticks only. Below `sample_from` the loop has no history to rewind into, so the opening ticks are
	## re-advanced rather than restored and their entry state is legitimately not what the live path wrote.
	func _check_writeback(tick: int) -> void:
		if tick < sample_from or _written_tick != tick - 1 or _written_marks.size() != COLUMNS.size():
			return
		var marks: PackedStringArray = column_marks()
		for index: int in marks.size():
			if _written_marks[index] == marks[index]:
				continue
			if index == QUANTIZED_COLUMN:
				canon_writebacks += 1
			elif writeback_bad_tick < 0:
				writeback_bad_tick = tick
				writeback_bad_col = COLUMNS[index]

	# --- exact values, as text -----------------------------------------------------------------------
	## One column's mark: the value's EXACT bytes in hex, paired with a human-readable rendering.
	##
	## THE BYTES THEMSELVES RATHER THAN A FOLDED HASH. A 64-bit FNV would need a wrapping multiply, and
	## signed-integer overflow is not a property to rest a gate on; the widest column here is 16 bytes, so the
	## exact encoding costs 32 characters a row and removes the collision question entirely. `var_to_bytes` is
	## the lossless encoding the tape codec already relies on, so the digest is a function of the stored bits
	## and of nothing else -- which is what makes a one-ULP difference visible.
	static func _mark(value: Variant, text: String) -> String:
		return "%s|%s" % [var_to_bytes(value).hex_encode(), text]

	## Space-free, so a mark stays one whitespace-delimited field on a report line.
	static func _vec_text(value: Vector3) -> String:
		return "%.9f/%.9f/%.9f" % [value.x, value.y, value.z]

	## 17 places, so a ONE-ULP difference is visible in the text and not only in the digest. A human reading a
	## divergence needs to see how large it is, and a shorter rendering prints the two peers' values identical.
	static func _float_text(value: float) -> String:
		return "%.17f" % value

	## One ULP away from `value`, by incrementing its bit pattern. Godot exposes no `nextafter`, so the bits
	## are stepped directly: the next double toward +inf for a positive value and the next one away from zero
	## for a negative one. Either direction is exactly one ULP, which is all the injection needs.
	static func _one_ulp_up(value: float) -> float:
		var bytes: PackedByteArray = PackedByteArray()
		bytes.resize(8)
		bytes.encode_double(0, value)
		bytes.encode_u64(0, bytes.decode_u64(0) + 1)
		return bytes.decode_double(0)


# --- the run ---------------------------------------------------------------------------------------
## Ticks simulated before sampling starts. The rollback loop cannot rewind past the start of history, so the
## opening ticks are entered a varying number of times and their entry state is not yet a restored one. Well
## clear of `_RESIM_FORCE` and of the 128-tick history the harness project configures.
const _WARMUP_TICKS: int = 60
## Sampled ticks. Every one contributes `DetBody.COLUMNS.size()` rows to the comparison.
const _SAMPLE_TICKS: int = 240
## Forced rollback depth. It makes the loop rewind and replay on every tick with no correction to trigger one,
## which is what turns each sampled tick into many passes over the same restored state.
const _RESIM_FORCE: int = 8
## The seeded policy the record pass drives, and its seed. `BenchPolicy` is a pure function of (policy, t,
## seed), so the tape is reproducible from these two values alone. Named rather than taken as an enum value so
## the report prints the name the bench itself uses.
const _TAPE_POLICY: String = "wander"
const _TAPE_SEED: int = 4171
## Seconds of drive time per recorded frame. One frame per tick at the harness project's 60 Hz.
const _TAPE_STEP_S: float = 1.0 / 60.0

var _role: String = "peer"
var _label: String = "peer"
var _tape_path: String = ""
var _frames: int = 0
var _diverge_at: int = -1

var _tape: InputTape = InputTape.new()
var _body: DetBody = null
var _handle: NetRollbackHandle = null
var _reported: bool = false
var _passed: bool = false

func _ready() -> void:
	name = "Determinism"
	process_mode = Node.PROCESS_MODE_ALWAYS
	_parse_args()
	if _role == "record":
		_record_tape()
		return
	_run_peer()

## Total ticks the peer runs, sampled range included.
func _total_ticks() -> int:
	return _WARMUP_TICKS + _SAMPLE_TICKS

# --- the record pass -------------------------------------------------------------------------------
## Write the tape both peers replay. It goes through `InputTape.save()` and comes back through
## `load_from()`, so the file itself is part of what the run exercises -- a tape that did not round-trip would
## hand the two peers different input and look exactly like a desync.
func _record_tape() -> void:
	var frames: int = _frames if _frames > 0 else _total_ticks()
	var policy: BenchPolicy.Policy = BenchPolicy.policy_from_name(_TAPE_POLICY)
	for index: int in frames:
		_tape.record(BenchPolicy.frame(policy, float(index) * _TAPE_STEP_S, _TAPE_SEED))
	var err: Error = _tape.save(_tape_path)
	if err != OK:
		printerr("DET-FAIL role=record could not write '%s' (error %d)" % [_tape_path, err])
		print("DET-RESULT label=%s FAIL" % _label)
		get_tree().quit(1)
		return
	print("DET-TAPE path=%s frames=%d policy=%s seed=%d" % [
		_tape_path, _tape.length(), _TAPE_POLICY, _TAPE_SEED])
	print("DET-RESULT label=%s PASS" % _label)
	get_tree().quit(0)

# --- the peer pass ---------------------------------------------------------------------------------
func _run_peer() -> void:
	var err: Error = _tape.load_from(_tape_path)
	if err != OK:
		_finish(false, "could not load the tape '%s' (error %d)" % [_tape_path, err])
		get_tree().quit(1)
		return
	if _tape.length() < _total_ticks():
		_finish(false, "the tape holds %d frames and the run needs %d" % [_tape.length(), _total_ticks()])
		get_tree().quit(1)
		return

	_build_world()
	# The facade hands out INERT handles while it is OFFLINE, so the lane cannot be registered until a peer is
	# assigned and the mode is set. `OfflineMultiplayerPeer` is what makes this process authoritative without
	# binding a socket -- see the header.
	multiplayer.multiplayer_peer = OfflineMultiplayerPeer.new()
	Net.set_mode(Net.Mode.HOST)
	_handle = Net.register_rollback_body(
		_body, _body.input, DetBody.STATE_PROPS, DetBody.INPUT_PROPS, true)
	Net.set_resim_force(_RESIM_FORCE)
	Net.pre_tick.connect(_on_pre_tick)

	print("DET-BOOT label=%s godot=%s timing=%s tape_frames=%d warmup=%d sample=%d resim_force=%d diverge_at=%d" % [
		_label, Engine.get_version_info().get("string", "?"), Net.debug_timing(),
		_tape.length(), _WARMUP_TICKS, _SAMPLE_TICKS, _RESIM_FORCE, _diverge_at])
	print("DET-COLS label=%s cols=%s" % [_label, ",".join(DetBody.COLUMNS)])

## The same graph on both peers, every name written out. An entity id is a hash of a node path, and the state
## schema is positional, so two peers that name a node differently agree about nothing.
func _build_world() -> void:
	var world: Node = Node.new()
	world.name = "World"
	add_child(world)
	_body = DetBody.new()
	_body.name = "Subject"
	_body.sample_from = _WARMUP_TICKS
	_body.diverge_at = _diverge_at
	world.add_child(_body)
	var input: DetInput = DetInput.new()
	input.name = "Input"
	_body.add_child(input)
	_body.input = input

## The tape is applied per NET TICK rather than per frame, because the lane records one input row per tick and
## a frame that arrived twice would put a different row into history than the tape holds.
func _on_pre_tick(tick: int) -> void:
	if _reported:
		return
	if tick >= _total_ticks():
		_report()
		return
	var frame: Dictionary = _tape.frame_at(tick)
	_body.input.nin_move = BenchSubject.vec3_field(frame, BenchSubject.KEY_TRANSLATE)
	_body.input.nin_turn = BenchSubject.vec3_field(frame, BenchSubject.KEY_ROTATE).y
	_body.input.nin_fire = BenchSubject.bool_field(frame, BenchSubject.KEY_FIRE)

# --- the report ------------------------------------------------------------------------------------
func _report() -> void:
	var fallbacks: PackedStringArray = _handle.quantizer_fallbacks()
	print("DET-LANE label=%s active=%s fresh=%d replay=%d passes=%d fallbacks=%s" % [
		_label, "1" if _handle.is_active() else "0", _body.fresh_ticks, _body.replay_ticks,
		_body.repeat_passes, "-" if fallbacks.is_empty() else ",".join(fallbacks)])
	print("DET-RESTORE label=%s bad_tick=%d bad_col=%s" % [
		_label, _body.restore_bad_tick, _body.restore_bad_col])
	print("DET-WRITEBACK label=%s bad_tick=%d bad_col=%s canon=%d" % [
		_label, _body.writeback_bad_tick, _body.writeback_bad_col, _body.canon_writebacks])

	var ticks: Array[int] = []
	for tick: int in _body.entry_marks.keys():
		ticks.push_back(tick)
	ticks.sort()
	for tick: int in ticks:
		var marks: PackedStringArray = _body.entry_marks[tick]
		for index: int in marks.size():
			var parts: PackedStringArray = marks[index].split("|")
			print("DET-ROW t=%d c=%s h=%s v=%s" % [tick, DetBody.COLUMNS[index], parts[0], parts[1]])
	print("DET-RANGE label=%s first=%d last=%d ticks=%d rows=%d" % [
		_label, -1 if ticks.is_empty() else ticks[0], -1 if ticks.is_empty() else ticks[ticks.size() - 1],
		ticks.size(), ticks.size() * DetBody.COLUMNS.size()])

	# An empty run is a failure rather than a pass, so every count the comparison rests on is asserted here as
	# well as in the driver.
	if not _handle.is_active():
		_finish(false, "the rollback lane is inert -- this run registered nothing and simulated nothing")
	elif not fallbacks.is_empty():
		_finish(false, "the backend dropped a wire quantizer (%s), so the quantized column is lossless and
       this run does not exercise the quantizer path at all" % ",".join(fallbacks))
	elif ticks.size() < _SAMPLE_TICKS:
		_finish(false, "sampled %d of %d ticks" % [ticks.size(), _SAMPLE_TICKS])
	elif _body.repeat_passes <= 0:
		_finish(false, "no sampled tick was ever entered twice -- the loop never rewound, so the restore path
       was not exercised. Check that set_resim_force() reached the backend.")
	elif _body.restore_bad_tick >= 0:
		_finish(false, "the state entering tick %d differed between two passes over it, at column %s -- the
       restore round trip is not bit-exact" % [_body.restore_bad_tick, _body.restore_bad_col])
	elif _body.writeback_bad_tick >= 0:
		_finish(false, "column %s came back changed entering tick %d -- the history row does not hold what the
       simulation wrote, so a replayed tick starts from a different value than the live one did" % [
			_body.writeback_bad_col, _body.writeback_bad_tick])
	elif _body.canon_writebacks <= 0:
		_finish(false, "the quantized column was never canonicalized, so this run says nothing about the
       quantizer path")
	else:
		_finish(true, "")
	# The exit code agrees with the verdict, so a driver reading `rc` and a driver reading `DET-RESULT` cannot
	# disagree about the same run.
	get_tree().quit(0 if _passed else 1)

func _finish(passed: bool, reason: String) -> void:
	_reported = true
	_passed = passed
	if not passed and not reason.is_empty():
		printerr("DET-FAIL label=%s %s" % [_label, reason])
	print("DET-RESULT label=%s %s" % [_label, "PASS" if passed else "FAIL"])

# --- the command line ------------------------------------------------------------------------------
func _parse_args() -> void:
	_role = _flag("--role=", _role)
	_label = _flag("--label=", _label)
	_tape_path = _flag("--tape=", _tape_path)
	var frames: String = _flag("--frames=", "")
	if frames.is_valid_int():
		_frames = frames.to_int()
	var diverge: String = _flag("--diverge-at=", "")
	if diverge.is_valid_int():
		_diverge_at = diverge.to_int()

func _flag(prefix: String, fallback: String) -> String:
	for arg: String in OS.get_cmdline_user_args():
		if arg.begins_with(prefix):
			return arg.substr(prefix.length())
	return fallback
