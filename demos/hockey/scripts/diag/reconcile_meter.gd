extends RefCounted
class_name ReconcileMeter
## The demo's signature number: how far the predicted puck was from the authoritative one, in millimetres.
##
## HOW IT IS MEASURED. The puck records its simulated position the FIRST time a tick is simulated, and compares
## on every later pass over that same tick. A later pass only happens because an authoritative row arrived and
## the backend rewound to replay from it -- so the difference between the two answers for one tick IS the
## correction, at the tick where it happened, with no facade surface needed to observe it.
##
## KEYED ON VISITATION, DELIBERATELY NOT ON `is_fresh`. Those answer different questions and this one needs the
## other answer. `is_fresh` is keyed on INPUT novelty: true on the first simulation of a tick whose input is
## authoritative for the simulating peer. The puck has NO INPUT at all, so on a client it is never fresh --
## `is_fresh` would report nothing here, forever. What this needs is "has this tick been simulated before",
## which is a plain high-water mark, and which the protocol docs correctly call the WRONG definition of
## `is_fresh`. Both are right about their own question.
##
## THE FLOOR IS REAL. `position@half` is three IEEE-754 binary16s, whose spacing near a table coordinate of
## 1.0 m is about 1 mm, and the backend writes the quantized value back after every record so that every peer
## replays from the same canonical basis. A correction cannot be measured below that, so the HUD prints the
## floor beside the number rather than letting a reader mistake it for noise.

## Ticks of recorded prediction. Sized to HockeyConfig-independent 128 to match the project's `history_limit`:
## the backend never replays past evicted history, so a correction can never outrun the record it is measured
## against.
const RING: int = 128
## Corrections kept for the percentile readout. 240 at 60 Hz is the last four seconds -- long enough to have a
## p99 worth printing, short enough that flipping a lever moves it while you are still looking at it.
const WINDOW: int = 240

## Recorded correction magnitudes, in metres, oldest first.
var _errors: PackedFloat32Array = PackedFloat32Array()
var _ticks: PackedInt64Array = PackedInt64Array()
var _positions: PackedVector3Array = PackedVector3Array()
var _corrections: int = 0
var _visits: int = 0

func _init() -> void:
	_ticks.resize(RING)
	_positions.resize(RING)
	for slot: int in RING:
		_ticks[slot] = -1

## Record `position` as the simulation's answer for `tick`.
##
## Returns the correction in METRES when this tick had already been simulated, or -1.0 on a first visit. The
## caller distinguishes the two: -1.0 is "nothing to compare against", which is not the same as "no error".
func note(tick: int, position: Vector3) -> float:
	if tick < 0:
		return -1.0
	var slot: int = tick % RING
	_visits += 1
	if _ticks[slot] != tick:
		_ticks[slot] = tick
		_positions[slot] = position
		return -1.0
	var error: float = _positions[slot].distance_to(position)
	_positions[slot] = position
	_push(error)
	if error > 0.0:
		_corrections += 1
	return error

## The `fraction` percentile of the recorded corrections, in MILLIMETRES. 0.0 when nothing has been recorded.
func percentile_mm(fraction: float) -> float:
	if _errors.is_empty():
		return 0.0
	var sorted: PackedFloat32Array = _errors.duplicate()
	sorted.sort()
	var index: int = roundi(clampf(fraction, 0.0, 1.0) * float(sorted.size() - 1))
	return sorted[index] * 1000.0

## The most recent recorded correction, in millimetres. 0.0 before the first one.
##
## Separate from the percentiles because it is the only one of these figures with a TIME to it. A percentile
## over a rolling window barely moves frame to frame, so plotting one produces a staircase pinned at whatever
## the window maximum happens to be -- which says nothing about when corrections arrive or how they cluster.
## Corrections are sparse and spiky, and this is what lets them be drawn that way.
func last_error_mm() -> float:
	if _errors.is_empty():
		return 0.0
	return _errors[_errors.size() - 1] * 1000.0

## The largest correction in the window, in millimetres.
func peak_mm() -> float:
	var peak: float = 0.0
	for error: float in _errors:
		peak = maxf(peak, error)
	return peak * 1000.0

## Corrections recorded in the window (a replayed tick whose answer actually changed).
func sample_count() -> int:
	return _errors.size()

## Every correction since the session started, monotonic. Divided by elapsed time it is the correction RATE,
## which moves under the levers even when the magnitudes do not.
func corrections() -> int:
	return _corrections

## Every simulation pass recorded, monotonic. The denominator that turns `corrections()` into a fraction.
func visits() -> int:
	return _visits

## Forget everything (session teardown).
func reset() -> void:
	_errors = PackedFloat32Array()
	_corrections = 0
	_visits = 0
	for slot: int in RING:
		_ticks[slot] = -1

# --- internals -------------------------------------------------------------------------------------
# Written against the member array directly, NOT through a helper taking a PackedFloat32Array. Packed arrays
# are VALUE types in GDScript: a helper would push onto its own copy and the window would never grow -- a
# silent no-op that looks like "the readout is broken".
func _push(error: float) -> void:
	_errors.push_back(error)
	while _errors.size() > WINDOW:
		_errors.remove_at(0)
