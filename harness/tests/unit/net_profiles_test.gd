extends UnitTest
## Pure coverage of the [NetProfiles] catalog + [NetProfile] record for the OrbitNet bench (netbench). The catalog
## is the single source of truth read by the relay, the Steam conditioner, and the gates, so this pins that the
## shipped profiles exist with their calibrated values, that get_profile hands out independent COPIES (a caller
## tweaking one knob must not mutate the shared catalog), and that the profile record round-trips losslessly.

func test_shipped_profiles_exist() -> void:
	var names: PackedStringArray = NetProfiles.names()
	for expected: String in ["clean", "lan", "broadband", "congested_wifi", "mobile_4g", "worst_case", "torture"]:
		assert_true(names.has(expected), "catalog ships the '%s' profile" % expected)
	assert_true(NetProfiles.has("congested_wifi"), "has() finds a known profile")
	assert_false(NetProfiles.has("dialup_1998"), "has() rejects an unknown profile")
	assert_true(NetProfiles.get_profile("dialup_1998") == null, "get_profile returns null for an unknown name")

func test_congested_wifi_values() -> void:
	var p: NetProfile = NetProfiles.get_profile("congested_wifi")
	assert_almost_eq(p.latency_ms, 50.0, 0.0, "congested_wifi is 50ms one-way")
	assert_almost_eq(p.jitter_ms, 50.0, 0.0, "congested_wifi has 50ms jitter (the story is jitter, not mean)")
	assert_almost_eq(p.loss, 0.02, 1e-6, "congested_wifi drops 2%")
	assert_almost_eq(p.rtt_estimate_ms(), 100.0, 0.0, "RTT estimate is 2x the one-way latency")

func test_worst_case_burst_uses_gilbert_elliott() -> void:
	var p: NetProfile = NetProfiles.get_profile("worst_case_burst")
	assert_true(p.burst, "worst_case_burst enables the bursty-loss model")
	assert_true(p.burst_loss_bad > p.burst_loss_good, "the bad state loses more than the good state")

func test_get_profile_returns_independent_copies() -> void:
	# Mutating a fetched profile must NOT change the shared catalog instance a later fetch returns.
	var a: NetProfile = NetProfiles.get_profile("broadband")
	a.latency_ms = 9999.0
	var b: NetProfile = NetProfiles.get_profile("broadband")
	assert_almost_eq(b.latency_ms, 30.0, 0.0, "a second fetch is unaffected by mutating the first (a copy, not the catalog)")

func test_profile_dict_round_trip_is_lossless() -> void:
	var original: NetProfile = NetProfiles.get_profile("worst_case_burst")
	var restored: NetProfile = NetProfile.new()
	restored.from_dict(original.to_dict())
	assert_eq(restored.name, original.name, "name round-trips")
	assert_almost_eq(restored.latency_ms, original.latency_ms, 0.0, "latency round-trips")
	assert_almost_eq(restored.jitter_ms, original.jitter_ms, 0.0, "jitter round-trips")
	assert_eq(restored.burst, original.burst, "burst flag round-trips")
	assert_almost_eq(restored.burst_loss_bad, original.burst_loss_bad, 0.0, "burst loss round-trips")
