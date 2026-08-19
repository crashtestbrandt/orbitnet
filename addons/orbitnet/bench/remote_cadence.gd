extends RefCounted
class_name RemoteCadence
## How often a remote body's authoritative pose reaches this client, in NET TICKS.
##
## THIS IS "THE REMOTE BODIES MOVE CHOPPILY" AS A NUMBER. Every other netcode gate here asks about the LOCAL
## player -- prediction offset, reconcile convergence, clock stretch, resim depth -- and a remote body's motion is
## the one thing a player complains about that none of them can see.
##
## The mechanism is not subtle. A client renders a remote body by interpolating between the last two poses it
## captured at a net tick. Those captures happen every net tick whether or not a row arrived, so a tick that
## brought no fresh state
## captures the same pose twice and renders a HELD FRAME. If rows arrive every k ticks the body holds still for
## k-1 and covers k ticks of travel in one -- which is what a player reports as stutter, and what the send path's
## per-peer byte budget produces when it cannot carry every entity every tick.
##
## It bears on hit registration as much as on feel. A rewind built from a constant that assumes every remote body
## renders exactly ONE net tick behind ([NetLagComp.INTERP_TICKS]) is wrong for a body arriving every k ticks: it
## renders further behind than that, and the server tests the shot against a pose the shooter never saw. That is
## why [member NetLagComp.observed_interp_ticks] is measured rather than assumed.
##
## ## Two things it must not conflate, both learned by measuring the wrong thing first
##
## **A DISTANT BODY IS NOT A STARVED ONE.** Interest management is supposed to stop sending what a peer is not
## looking at, so pooling every watched body into one distribution reports a working cull as a regression: the
## first AOI A/B run read *worse* with culling on (mean 3.86 -> 5.60 ticks) purely because the far bodies it
## had correctly stopped sending were still in the average. Gaps are therefore split at [member near_radius_m],
## and the NEAR figure is the one an A/B compares -- it is also the one a player is fighting inside.
##
## **AN ABSENT BODY IS NOT A SLOW ONE.** A body that dies, or leaves interest entirely, stops producing rows for
## seconds at a time; folding that into the distribution produced a 6801-tick "gap" (113 s) beside a p50 of 2.
## Anything past [constant _MAX_CREDIBLE_GAP_TICKS] is counted as an ABSENCE and reported separately, because
## "this body stopped being replicated" and "this body's rota slot is starved" are different facts with different
## fixes.
##
## **AND ONE IT CANNOT SEPARATE, stated because it biases the figure in a known direction.** What is measured is
## a POSE CHANGE, not a row arrival, because a pose is what every watched body publishes and a per-body "when did
## your newest authoritative row land" would have to be plumbed out of both synchronizer types. A body that pauses
## mid-window -- one that reaches its post and holds, a player who stops -- therefore records the pause as one
## long gap when it next moves. So the distribution reads LONG rather than short: it can report a healthy rota as
## starved, never a starved one as healthy, which is the safe direction for a figure whose job is to catch
## starvation. [method bodies_moving] beside [method bodies_seen] is how far the reading can be trusted: run it
## over bodies that are in motion for the whole window, and a run where those two numbers diverge is a run whose
## cadence figure is measuring stillness.
##
## PURE, so the whole rule is unit-tested without a session: feed it observations, ask it for the distribution.

## A body whose pose never changed across the whole window is STANDING STILL, not starved, and averaging its
## non-existent gaps in would report a still arena as a perfectly smooth one. Only bodies that moved at least
## this many times contribute.
const _MIN_MOVES_TO_COUNT: int = 2

## Past this many net ticks (5 s at 60 Hz) a gap is not a rota gap. Nothing the send path does to a body it is
## still replicating produces a five-second hold -- that is a despawn, a death, or a cull. Counted, not averaged.
const _MAX_CREDIBLE_GAP_TICKS: int = 300

## The distance (m) inside which a body counts as NEAR. Set it to something a fight happens inside -- the diagonal
## of the space the action takes place in is a good default -- so "near" means "in the fight" rather than naming a
## network quantity. The default suits a roughly 60 m arena; a game measuring another scale must set its own.
var near_radius_m: float = 105.0

# Per entity: the last pose seen, the tick it changed on, and how many changes it has produced.
var _last_pose: Dictionary[int, Vector3] = {}
var _last_change_tick: Dictionary[int, int] = {}
var _moves: Dictionary[int, int] = {}
var _seen: Dictionary[int, bool] = {}
var _near_gaps: Array[int] = []
var _far_gaps: Array[int] = []
var _absences: int = 0

## Observe one body's replicated pose at `tick`, `dist` metres from the local player. Call once per net tick per
## watched body.
##
## The FIRST observation of a body seeds a baseline only: we do not know how long it had been sitting there
## before we started looking, so the interval up to its first observed change measures nothing.
func observe(id: int, pose: Vector3, tick: int, dist: float = 0.0) -> void:
	_seen[id] = true
	if not _last_pose.has(id):
		_last_pose[id] = pose
		_last_change_tick[id] = tick
		_moves[id] = 0
		return
	if _last_pose[id] == pose:
		return   # a held frame: no row for this body landed on this tick
	var gap: int = tick - _last_change_tick[id]
	_last_pose[id] = pose
	_last_change_tick[id] = tick
	_moves[id] = _moves[id] + 1
	if _moves[id] < _MIN_MOVES_TO_COUNT or gap <= 0:
		return
	if gap > _MAX_CREDIBLE_GAP_TICKS:
		_absences += 1
		return
	if dist <= near_radius_m:
		_near_gaps.push_back(gap)
	else:
		_far_gaps.push_back(gap)

## How many distinct bodies were observed at all.
func bodies_seen() -> int:
	return _seen.size()

## ...and how many moved enough to contribute gaps.
func bodies_moving() -> int:
	var n: int = 0
	for id: int in _moves:
		if _moves[id] >= _MIN_MOVES_TO_COUNT:
			n += 1
	return n

## Gaps longer than any rota can explain -- a despawn, a death, or a body leaving interest entirely.
func absences() -> int:
	return _absences

## The near-band gaps, ascending. `near` is the band an A/B compares: it is what the player is fighting inside,
## and it is the one interest culling is supposed to make BETTER rather than merely smaller.
func near_gaps() -> Array[int]:
	var out: Array[int] = _near_gaps.duplicate()
	out.sort()
	return out

func far_gaps() -> Array[int]:
	var out: Array[int] = _far_gaps.duplicate()
	out.sort()
	return out

## The mean gap in net ticks over `gaps`, or 0.0 when empty. 1.0 is a row every tick -- the floor
## [NetLagComp.INTERP_TICKS] assumes; 4.0 means a body holds for three ticks and jumps on the fourth.
static func mean_of(gaps: Array[int]) -> float:
	if gaps.is_empty():
		return 0.0
	var total: int = 0
	for g: int in gaps:
		total += g
	return float(total) / float(gaps.size())

## The q-th percentile (q in [0,1]) of an ASCENDING `gaps` by nearest rank, 0 when empty. Read the p95, not the
## mean: a mean of 1.2 with a p95 of 9 is a body that is mostly fine and visibly jumps twice a second, which is
## what a player notices and what a mean hides.
static func percentile_of(gaps: Array[int], q: float) -> float:
	if gaps.is_empty():
		return 0.0
	var i: int = clampi(int(floor(clampf(q, 0.0, 1.0) * float(gaps.size() - 1))), 0, gaps.size() - 1)
	return float(gaps[i])

## Drop everything, so a probe can measure a named window rather than a whole run.
func reset() -> void:
	_last_pose.clear()
	_last_change_tick.clear()
	_moves.clear()
	_seen.clear()
	_near_gaps.clear()
	_far_gaps.clear()
	_absences = 0
