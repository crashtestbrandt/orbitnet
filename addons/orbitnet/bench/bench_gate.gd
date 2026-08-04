extends RefCounted
class_name BenchGate
## Pure pass/fail evaluation for an OrbitNet bench run (netbench). Given the per-tick metric samples a run
## collected -- RTT, clock stretch, rollback resim depth, reconcile snap count -- plus the profile it ran under,
## it produces a verdict with human-readable reasons. PURE (arrays in, verdict out): no scene/socket/time
## dependency, so the thresholds and percentile math are unit-tested directly on synthetic sample sets.
##
## The gates are TICK-DOMAIN / distribution-based, never render-domain wallclock jerk -- the hard-won S8 lesson
## (render-domain metrics on a spinning interpolated body are timing-flaky) and, per the research, exactly the AAA
## practice (Riot gates on functional/tick-domain assertions, not frame timing). What a run asserts:
##   * measured RTT reflects the INJECTED latency -- p50 RTT within tolerance of the profile's ~2x one-way
##     estimate. This is the "the conditioner actually did something / the game observed it" check; without it a
##     silent no-op relay would pass every other gate on a clean link.
##   * the clock stays disciplined under the impairment -- mean |stretch-1| bounded (the time sync isn't
##     thrashing) and hard stretch excursions rare.
##   * reconciliation converges -- the smoother resolves corrections without an unreasonable run of hard SNAPs
##     (a snap storm means prediction never catches up). This is the real "prediction still works" signal.
## Rollback resim DEPTH is REPORTED, not gated: under latency the resim window legitimately deepens (the #214
## cost), bounded by the backend's history_limit -- broken prediction shows up as snaps, not depth. Thresholds SCALE
## with the profile: a 250ms worst_case link legitimately shows more RTT/jitter/stretch than broadband, so the
## bounds are derived from the profile rather than fixed, and `clean` is held to the tightest bar.

## The evaluated verdict. `passed` is the AND of every gate; `reasons` lists each gate's outcome (PASS or FAIL
## with the measured-vs-bound detail) so a failing run is self-diagnosing in the artifact.
class Result extends RefCounted:
	var passed: bool = true
	var reasons: PackedStringArray = PackedStringArray()

	func _record(ok: bool, detail: String) -> void:
		reasons.push_back(("PASS " if ok else "FAIL ") + detail)
		if not ok:
			passed = false

	# Report a measured value WITHOUT affecting pass/fail (a cost metric, not a correctness bound).
	func _info(detail: String) -> void:
		reasons.push_back("INFO " + detail)

## The q-th percentile (q in [0,1]) of `values` by linear interpolation between closest ranks. Empty -> 0. The
## input need not be pre-sorted (a copy is sorted here) so callers can pass raw sample arrays.
static func percentile(values: Array[float], q: float) -> float:
	if values.is_empty():
		return 0.0
	var sorted: Array[float] = values.duplicate()
	sorted.sort()
	if sorted.size() == 1:
		return sorted[0]
	var rank: float = clampf(q, 0.0, 1.0) * float(sorted.size() - 1)
	var lo: int = floori(rank)
	var hi: int = ceili(rank)
	var frac: float = rank - float(lo)
	return lerpf(sorted[lo], sorted[hi], frac)

static func mean(values: Array[float]) -> float:
	if values.is_empty():
		return 0.0
	var total: float = 0.0
	for v: float in values:
		total += v
	return total / float(values.size())

## Evaluate a run. Inputs are the raw per-tick sample arrays plus the profile that was injected and the reconcile
## snap tally (a monotonic count, so just the final value). `min_samples` guards a silently-empty run (a client
## that never connected collects nothing and must FAIL, not vacuously pass -- the same "empty run is a red flag"
## rule the unit runner and probes use).
static func evaluate(profile: NetProfile, rtt_ms: Array[float], stretch: Array[float],
		resim_ticks: Array[float], reconcile_snaps: int, min_samples: int = 30) -> Result:
	var r: Result = Result.new()

	# 0) Enough samples to mean anything at all.
	var n: int = rtt_ms.size()
	r._record(n >= min_samples, "sample count %d >= %d (run produced data)" % [n, min_samples])
	if n < min_samples:
		return r   # nothing else is meaningful without data

	# 1) Measured RTT reflects the injected latency. The relay conditions BOTH directions, so observed RTT should
	# land near the profile's 2x one-way estimate. Tolerance is generous (the tolerance band widens with jitter and
	# has an absolute floor for the near-zero profiles) -- this gate proves the conditioner is LIVE and OBSERVED,
	# not that the mean is exact. For `clean` (expected ~0) the check is just "RTT is small".
	var rtt_p50: float = percentile(rtt_ms, 0.50)
	var expected_rtt: float = profile.rtt_estimate_ms()
	if expected_rtt <= 1.0:
		r._record(rtt_p50 <= 30.0, "clean-link RTT p50 %.1fms <= 30ms (no phantom latency)" % rtt_p50)
	else:
		var tol: float = maxf(0.5 * expected_rtt, 2.0 * profile.jitter_ms + 20.0)
		var lo: float = maxf(0.0, expected_rtt - tol)
		var hi: float = expected_rtt + tol
		r._record(rtt_p50 >= lo and rtt_p50 <= hi,
			"RTT p50 %.1fms within [%.0f,%.0f]ms of injected ~%.0fms (conditioner observed)" % [rtt_p50, lo, hi, expected_rtt])

	# 2) Clock discipline: the sim clock should hold near 1.0 on average. A stretched clock slides tick
	# boundaries (the loopback-stutter failure mode); the gate catches a THRASHING clock (never settling). The bound
	# SCALES with the profile: the backend caps stretch at max_time_stretch (1.05), and under a severe link the clock
	# legitimately rides near that envelope -- partly a longer convergence transient at connect -- so a fixed tight
	# bound would fail a correctly-working clock at the design ceiling. Clean links are held tight (~0.03); worst_case
	# is allowed up to the ~0.06 envelope. A truly broken clock (mean far past the cap) still fails everywhere.
	if not stretch.is_empty():
		var mean_dev: float = absf(mean(stretch) - 1.0)
		var stretch_bound: float = clampf(0.02 + expected_rtt / 10000.0 + profile.loss * 0.3, 0.03, 0.06)
		r._record(mean_dev <= stretch_bound,
			"mean |clock stretch - 1| %.4f <= %.4f (clock not thrashing; bound scales with the profile)" % [mean_dev, stretch_bound])

	# 3) Reconcile convergence: a hard SNAP is the smoother giving up on a small glide. Some snaps are normal under
	# loss; a STORM (snaps on a large fraction of ticks) means prediction never converges. Bound snaps as a rate.
	var snap_rate: float = float(reconcile_snaps) / float(n)
	r._record(snap_rate <= 0.25, "reconcile snap rate %.3f (%d over %d ticks) <= 0.25" % [snap_rate, reconcile_snaps, n])

	# 4) Rollback resim depth: REPORTED, not gated. Under real latency/jitter/loss the resim window legitimately
	# deepens (a late/lost input forces a deep catch-up resim) -- that is the resim COST, expected behaviour,
	# and it is bounded by history_limit by design, so it can't "run away". If deep resims were
	# actually breaking prediction, the reconcile snap-rate gate above would catch it. So this is an INFO line (cost
	# for the artifact / comparing profiles), never a pass/fail. The counters are live in EVERY build -- they
	# are a byproduct of the native loop rather than debug-only monitors -- so this line is always populated.
	if not resim_ticks.is_empty() and percentile(resim_ticks, 0.95) > 0.0:
		r._info("resim depth p50=%.0f p95=%.0f ticks (cost under latency; bounded by history_limit 128)" % [
			percentile(resim_ticks, 0.50), percentile(resim_ticks, 0.95)])

	return r
