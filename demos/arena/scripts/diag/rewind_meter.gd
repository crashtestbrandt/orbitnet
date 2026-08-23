extends RefCounted
class_name RewindMeter
## How deep the last few shots rewound, per band. The number this demo exists to put on screen.
##
## THREE DEPTHS FROM ONE SHOT is the whole claim of the per-target rewind, and it is invisible without a
## readout: a shot resolves, a target falls over or does not, and nothing about the frame says whether the
## near target and the far one were reconstructed at the same tick. `NetLagComp.rewind_band_ticks()` returns
## the three absolute ticks; this keeps the last few and reports them as depths, because a depth is the thing
## that is comparable between shots and an absolute tick is not.
##
## SERVER-SIDE, because the server is the only peer that resolves an authoritative shot. A client sees its own
## tracer and the result; it never learns what window it was granted.

## How many shots to keep. Enough to smooth a reading, few enough that a burst does not hide a change.
const WINDOW: int = 16

var _near: PackedInt32Array = PackedInt32Array()
var _mid: PackedInt32Array = PackedInt32Array()
var _far: PackedInt32Array = PackedInt32Array()
var _base: PackedInt32Array = PackedInt32Array()
var _hits: int = 0
var _shots: int = 0
var _last_band: int = -1
var _last_target_ticks: int = -1

## Record one resolved shot.
func note(shot: HitResolver.Shot, present_tick: int) -> void:
	if shot == null:
		return
	_shots += 1
	if shot.hit_seat >= 0:
		_hits += 1
		_last_band = shot.target_band
		_last_target_ticks = shot.target_rewind_ticks
	_push(_base, shot.base_rewind_ticks)
	if shot.band_ticks.size() == 3:
		_push(_near, present_tick - shot.band_ticks[NetLagComp.Band.NEAR])
		_push(_mid, present_tick - shot.band_ticks[NetLagComp.Band.MID])
		_push(_far, present_tick - shot.band_ticks[NetLagComp.Band.FAR])

func clear() -> void:
	_near.clear()
	_mid.clear()
	_far.clear()
	_base.clear()
	_hits = 0
	_shots = 0
	_last_band = -1
	_last_target_ticks = -1

func shots() -> int:
	return _shots

func hits() -> int:
	return _hits

## Hit registration, the one property lag compensation exists to serve. 0.0 before any shot.
func hit_rate() -> float:
	return 0.0 if _shots == 0 else float(_hits) / float(_shots)

## Mean rewind depth for a band, in ticks. -1.0 when nothing has been recorded for it.
func mean_ticks(band: NetLagComp.Band) -> float:
	match band:
		NetLagComp.Band.NEAR:
			return _mean(_near)
		NetLagComp.Band.MID:
			return _mean(_mid)
		NetLagComp.Band.FAR:
			return _mean(_far)
	return -1.0

## The mean BASE depth -- the flat per-shooter window the bands refine around. Reported beside them because
## the three bands are only interesting relative to it.
func mean_base_ticks() -> float:
	return _mean(_base)

## Whether the three bands actually differ. They do not when the send path has published no per-band
## measurement, when no band scale is configured, or when every body sits in one band -- and in all three
## cases the per-target rewind correctly degenerates to the flat window rather than inventing a spread.
func bands_differ() -> bool:
	var near: float = _mean(_near)
	var far: float = _mean(_far)
	if near < 0.0 or far < 0.0:
		return false
	return absf(near - far) >= 0.5

## The band the last hit target sat in, and how deep it was rewound. -1 before any hit.
func last_hit_band() -> int:
	return _last_band

func last_hit_ticks() -> int:
	return _last_target_ticks

func _push(into: PackedInt32Array, value: int) -> void:
	into.push_back(value)
	if into.size() > WINDOW:
		into.remove_at(0)

func _mean(values: PackedInt32Array) -> float:
	if values.is_empty():
		return -1.0
	var sum: int = 0
	for value: int in values:
		sum += value
	return float(sum) / float(values.size())
