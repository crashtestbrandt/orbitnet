extends UnitTest
## The impairment scheduler, the profile catalog, and the tick-domain gate. All pure.
##
## The scheduler is what makes netbench a bench rather than a demo: it is a deterministic, seeded model of a bad
## link that can be replayed exactly. These cases pin the properties that makes a run comparable to another run.

func _profile(latency: float, jitter: float, loss: float) -> NetProfile:
	var profile: NetProfile = NetProfile.new()
	profile.name = "test"
	profile.latency_ms = latency
	profile.jitter_ms = jitter
	profile.loss = loss
	return profile

func _payload(byte: int) -> PackedByteArray:
	var out: PackedByteArray = PackedByteArray()
	out.push_back(byte)
	return out

# --- the catalog ----------------------------------------------------------------------------------
func test_the_catalog_has_a_clean_control() -> void:
	# A run that fails on `clean` is a harness bug, not a netcode finding. The control existing is what makes
	# that distinction possible at all.
	var clean: NetProfile = NetProfiles.get_profile("clean")
	assert_true(clean != null, "the clean profile exists")
	assert_almost_eq(clean.latency_ms, 0.0, 0.0001, "with no injected latency")
	assert_almost_eq(clean.loss, 0.0, 0.0001, "and no injected loss")

func test_unknown_profiles_return_null_rather_than_a_default() -> void:
	# Silently conditioning NOTHING when a name is misspelled would produce a run that looks clean and proves
	# nothing. The caller has to surface it.
	assert_true(NetProfiles.get_profile("congsted_wifi") == null, "a typo yields null")
	assert_false(NetProfiles.has("congsted_wifi"), "and has() says so, for arg validation")

func test_profiles_are_handed_out_as_copies() -> void:
	var a: NetProfile = NetProfiles.get_profile("clean")
	a.latency_ms = 500.0
	var b: NetProfile = NetProfiles.get_profile("clean")
	assert_almost_eq(b.latency_ms, 0.0, 0.0001,
		"mutating a fetched profile must not poison the shared catalog for the rest of the run")

func test_every_catalog_entry_resolves() -> void:
	var names: PackedStringArray = NetProfiles.names()
	assert_true(names.size() >= 2, "the catalog is not empty")
	for name: String in names:
		assert_true(NetProfiles.get_profile(name) != null, "'%s' resolves" % name)

func test_the_rtt_estimate_is_two_way() -> void:
	# The relay conditions BOTH directions, so a 60 ms one-way profile is a ~120 ms round trip. The gate
	# compares measured RTT against this, so getting it wrong would fail every correct run.
	var profile: NetProfile = _profile(60.0, 0.0, 0.0)
	assert_almost_eq(profile.rtt_estimate_ms(), 120.0, 0.001, "one-way latency doubles into an RTT estimate")

# --- the scheduler --------------------------------------------------------------------------------
func test_a_clean_link_delivers_everything_in_order() -> void:
	var impairment: PacketImpairment = PacketImpairment.new()
	impairment.configure(_profile(0.0, 0.0, 0.0), 1)
	for index: int in 10:
		impairment.push(_payload(index), 0)
	var delivered: Array[PackedByteArray] = impairment.poll(0)
	assert_eq(delivered.size(), 10, "nothing is dropped on a clean link")
	for index: int in delivered.size():
		assert_eq(delivered[index][0], index, "and packet %d arrives in order" % index)

func test_latency_holds_packets_until_their_release_time() -> void:
	var impairment: PacketImpairment = PacketImpairment.new()
	impairment.configure(_profile(50.0, 0.0, 0.0), 1)
	impairment.push(_payload(7), 0)
	assert_eq(impairment.poll(10).size(), 0, "10 ms in, a 50 ms packet is still in flight")
	assert_eq(impairment.pending(), 1, "and is accounted for")
	assert_eq(impairment.poll(50).size(), 1, "at 50 ms it lands")
	assert_eq(impairment.pending(), 0, "and the queue drains")

func test_the_scheduler_is_deterministic_for_a_seed() -> void:
	# THE property that makes two bench runs comparable. Without it, "the numbers moved" could always be the
	# link rather than the change under test.
	var first: PackedInt32Array = _run(_profile(20.0, 10.0, 0.3), 1234)
	var second: PackedInt32Array = _run(_profile(20.0, 10.0, 0.3), 1234)
	assert_eq(first, second, "the same seed replays the same link exactly")

func test_different_seeds_produce_different_links() -> void:
	var a: PackedInt32Array = _run(_profile(20.0, 10.0, 0.3), 1)
	var b: PackedInt32Array = _run(_profile(20.0, 10.0, 0.3), 2)
	assert_true(a != b, "a different seed is a different link, so a fleet is not one correlated waveform")

func test_total_loss_delivers_nothing() -> void:
	var impairment: PacketImpairment = PacketImpairment.new()
	impairment.configure(_profile(0.0, 0.0, 1.0), 5)
	for index: int in 20:
		impairment.push(_payload(index), 0)
	assert_eq(impairment.poll(1000).size(), 0, "loss = 1.0 drops everything")
	var stats: Dictionary[String, int] = impairment.stats()
	assert_eq(stats["dropped"], 20, "and the drops are counted, so a run reports what it did to the link")

func test_stats_account_for_every_packet() -> void:
	var impairment: PacketImpairment = PacketImpairment.new()
	impairment.configure(_profile(0.0, 0.0, 0.0), 3)
	for index: int in 15:
		impairment.push(_payload(index), 0)
	var stats: Dictionary[String, int] = impairment.stats()
	assert_eq(stats["in"], 15, "every pushed packet is counted on the way in")

# Drive a scheduler and return the delivered byte sequence, as a comparable fingerprint of the link.
func _run(profile: NetProfile, seed: int) -> PackedInt32Array:
	var impairment: PacketImpairment = PacketImpairment.new()
	impairment.configure(profile, seed)
	var out: PackedInt32Array = PackedInt32Array()
	for step: int in 60:
		impairment.push(_payload(step % 256), step * 5)
		for packet: PackedByteArray in impairment.poll(step * 5):
			out.push_back(packet[0])
	for packet: PackedByteArray in impairment.poll(100000):
		out.push_back(packet[0])
	return out

# --- the gate -------------------------------------------------------------------------------------
func test_percentiles_and_means() -> void:
	var values: Array[float] = [1.0, 2.0, 3.0, 4.0, 5.0]
	assert_almost_eq(BenchGate.percentile(values, 0.0), 1.0, 0.0001, "p0 is the minimum")
	assert_almost_eq(BenchGate.percentile(values, 0.5), 3.0, 0.0001, "p50 is the median")
	assert_almost_eq(BenchGate.percentile(values, 1.0), 5.0, 0.0001, "p100 is the maximum")
	assert_almost_eq(BenchGate.mean(values), 3.0, 0.0001, "the mean is the mean")

func test_percentiles_do_not_require_sorted_input() -> void:
	var shuffled: Array[float] = [5.0, 1.0, 4.0, 2.0, 3.0]
	assert_almost_eq(BenchGate.percentile(shuffled, 0.5), 3.0, 0.0001,
		"callers pass raw sample arrays, so the function sorts a copy itself")

func test_empty_inputs_do_not_divide_by_zero() -> void:
	var empty: Array[float] = []
	assert_almost_eq(BenchGate.percentile(empty, 0.5), 0.0, 0.0001, "an empty percentile is 0, not NaN")
	assert_almost_eq(BenchGate.mean(empty), 0.0, 0.0001, "and so is an empty mean")

func test_a_run_with_no_samples_FAILS_rather_than_vacuously_passing() -> void:
	# The most important gate in the file. A client that never connected collects nothing; if "no samples"
	# passed, a completely broken run would report green.
	var empty: Array[float] = []
	var result: BenchGate.Result = BenchGate.evaluate(
		NetProfiles.get_profile("clean"), empty, empty, empty, 0)
	assert_false(result.passed, "an empty run is a red flag, not a pass")
	assert_true(result.reasons.size() > 0, "and says why")

func test_a_clean_run_with_plausible_samples_passes() -> void:
	var rtt: Array[float] = []
	var stretch: Array[float] = []
	var resim: Array[float] = []
	for _i: int in 120:
		rtt.push_back(4.0)
		stretch.push_back(1.0)
		resim.push_back(0.0)
	var result: BenchGate.Result = BenchGate.evaluate(
		NetProfiles.get_profile("clean"), rtt, stretch, resim, 0)
	assert_true(result.passed, "a healthy clean-link run passes every gate")

func test_a_snap_storm_fails() -> void:
	# Occasional reconcile snaps are normal under loss. Snapping on most ticks means prediction never
	# converges, which is a real failure and must not be tolerated just because the link is bad.
	var rtt: Array[float] = []
	var stretch: Array[float] = []
	var resim: Array[float] = []
	for _i: int in 120:
		rtt.push_back(4.0)
		stretch.push_back(1.0)
		resim.push_back(0.0)
	var result: BenchGate.Result = BenchGate.evaluate(
		NetProfiles.get_profile("clean"), rtt, stretch, resim, 100)
	assert_false(result.passed, "snapping on 100 of 120 ticks is a storm, not noise")

func test_a_thrashing_clock_fails() -> void:
	var rtt: Array[float] = []
	var stretch: Array[float] = []
	var resim: Array[float] = []
	for _i: int in 120:
		rtt.push_back(4.0)
		stretch.push_back(1.4)   # far past any legitimate stretch envelope
		resim.push_back(0.0)
	var result: BenchGate.Result = BenchGate.evaluate(
		NetProfiles.get_profile("clean"), rtt, stretch, resim, 0)
	assert_false(result.passed, "a clock parked well off 1.0 is not disciplined")
