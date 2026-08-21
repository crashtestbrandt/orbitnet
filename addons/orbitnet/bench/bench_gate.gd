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

## Report the send path's bandwidth and fairness accounting. INFORMATIONAL rather than asserted, because the
## honest bar differs per game and per arena: what is universal is which three figures to look at.
##
## `rx_bytes` is PAYLOAD only -- the link additionally carries roughly 41 bytes of UDP+ENet header per datagram,
## so a run that looks comfortable here can still saturate a thin link.
static func evaluate_bandwidth(r: Result, rx_bytes: Array[float], want_full: Array[float],
		starve_ticks: Array[float]) -> Result:
	if rx_bytes.is_empty():
		return r
	r._info("rx p50=%.0fB/s p95=%.0fB/s (PAYLOAD -- the link additionally carries ~41 B per datagram)" % [
		percentile(rx_bytes, 0.50), percentile(rx_bytes, 0.95)])
	# THE ACCEPTANCE BAR FOR INTEREST MANAGEMENT BEING ON. An entity re-entering interest must get its full block
	# without a want_full storm: WANT_FULL is a per-peer, ALL-ENTITY flag, so one bad re-entry costs a round trip
	# plus a full-state burst for everything that peer holds, arriving exactly when a fight starts. Clearing
	# last_sent and acked_base at the LEAVE is what keeps this near zero; watch it whenever the radius changes.
	r._info("want_full nacks p50=%.2f/s p95=%.2f/s (near zero is the interest-management acceptance bar)" % [
		percentile(want_full, 0.50), percentile(want_full, 0.95)])
	# Starvation is what the priority rota exists to delete, and it is NOT bandwidth overage: the bytes are never
	# sent. It reads zero on a client (the server owns the rota), so an all-zero column here means "not measured",
	# not "healthy" -- which is why the figure is printed rather than asserted against.
	r._info("worst in-interest staleness p95=%.0f ticks (server-side; 0 in a client-only run)" % [
		percentile(starve_ticks, 0.95)])
	return r

## Report how often remote bodies' poses actually reached this client. INFORMATIONAL, and read the NEAR figure:
## the far band is what interest management is supposed to make sparser, so pooling the two reports a working
## cull as a regression. See [RemoteCadence] for why the reading is biased LONG and never short.
## Rounds a run must fire before "the server confirmed nothing" is evidence rather than luck.
const MIN_SHOTS_TO_CONCLUDE: int = 120

## Report on hit registration: the one property lag compensation exists to serve.
##
## `target_kind` is one of [BenchSubject]'s `TARGET_*` constants and decides what the run can conclude.
## The shot floor gates the NEGATIVE conclusion only: a run that landed even one confirmed hit has
## demonstrated the mechanism -- the shooter aimed, the server adjudicated, the confirmation came back --
## and no number of rounds makes that less true. Only "the server confirmed nothing" needs enough rounds
## behind it to mean something.
##
## The hit RATE is reported and not bounded. It moves with the scenario, the seed and the profile, so a
## threshold on it needs a week of runs first.
static func evaluate_hit_registration(r: Result, shots: int, confirms: int, target_kind: String,
		min_shots: int = MIN_SHOTS_TO_CONCLUDE) -> Result:
	if target_kind == BenchSubject.TARGET_NONE:
		r._info("hit registration NOT EXERCISED: the shooter resolved no target, so its %d round(s) say nothing about hit registration (a blind-firing policy, or nobody lined up)" % shots)
		return r
	if confirms <= 0 and shots < min_shots:
		r._info("hit registration NOT EXERCISED: %d shot(s) fired at a %s target and none confirmed, under the %d rounds needed before that means anything" % [
			shots, target_kind, min_shots])
		return r
	r._record(confirms > 0,
		"hit registration: %d of %d shot(s) confirmed by the server (target=%s). Zero confirms from a firing peer is a broken mechanism, not a bad rate" % [
			confirms, shots, target_kind])
	r._info("hit rate %.1f%% (%d/%d, target=%s) -- reported, not bounded" % [
		100.0 * float(confirms) / float(shots), confirms, shots, target_kind])
	if target_kind == BenchSubject.TARGET_STATIONARY:
		r._info("target was STATIONARY: this proves adjudication and confirmation under latency, but a still target resolves the same at the present tick, so it does not exercise the rewind")
	elif target_kind == BenchSubject.TARGET_MOVING:
		r._info("target was MOVING: a confirmed hit on one is a statement about the rewind and not only about adjudication")
	return r

## Report on the orientation reconciliation arm.
##
## `armed` is the game's answer to "is the arm actually on". A disabled arm must not report like a healthy
## one: with it off every counter below is zero by construction, which is the exact signature of a perfectly
## behaved arm, and a reader would conclude the defect was fixed.
##
## Two residuals, and the decision turns on the second. `peak_rad` is a running maximum over the run, so it
## grows with the run's LENGTH and one cleanly absorbed correction reads the same as a tilt that never left.
## `standing_rad` is the worst trailing-window minimum: a residual that bleeds out touches zero inside every
## window and scores 0 however many corrections were folded, so a non-zero figure is a residual being
## refreshed faster than it decays.
static func evaluate_orientation_arm(r: Result, armed: bool, smooths: int, misses: int, peak_rad: float,
		standing_rad: float, samples: int, resim_max: float) -> Result:
	if samples <= 0:
		return r
	if not armed:
		r._info("orientation reconcile arm NOT EXERCISED: the game reports the arm off, so its counters are zero by construction rather than by behaving -- arm it to measure it")
		return r
	r._info("orientation reconcile arm: %d correction(s) over %d ticks, STANDING residual %.4f deg (peak %.4f deg), %d ring miss(es), deepest resim %.0f ticks. The standing figure is the gauge; a peak only says one correction was big" % [
		smooths, samples, rad_to_deg(standing_rad), rad_to_deg(peak_rad), misses, resim_max])
	return r

static func evaluate_remote_cadence(r: Result, cadence: RemoteCadence) -> Result:
	var near: Array[int] = cadence.near_gaps()
	if near.is_empty() and cadence.far_gaps().is_empty():
		return r
	var far: Array[int] = cadence.far_gaps()
	r._info("remote cadence near mean=%.2f p95=%.0f ticks | far mean=%.2f p95=%.0f ticks" % [
		RemoteCadence.mean_of(near), RemoteCadence.percentile_of(near, 0.95),
		RemoteCadence.mean_of(far), RemoteCadence.percentile_of(far, 0.95)])
	# bodies_moving beside bodies_seen is how far the figure can be trusted: a window in which most watched bodies
	# never moved is measuring stillness, not the rota.
	r._info("remote cadence over %d bodies seen / %d moving, %d absences (despawn, death or cull)" % [
		cadence.bodies_seen(), cadence.bodies_moving(), cadence.absences()])
	return r
