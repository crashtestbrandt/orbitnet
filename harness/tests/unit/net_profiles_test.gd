extends UnitTest
## Pure coverage of the [NetProfiles] catalog + [NetProfile] record for the OrbitNet bench (netbench). The catalog
## is the single source of truth read by the relay, the Steam conditioner, and the gates, so this pins that the
## shipped profiles exist with their calibrated values, that get_profile hands out independent COPIES (a caller
## tweaking one knob must not mutate the shared catalog), and that the profile record round-trips losslessly.

func test_shipped_profiles_exist() -> void:
	var names: PackedStringArray = NetProfiles.names()
	for expected: String in ["clean", "lan", "broadband", "congested_wifi", "mobile_4g", "relayed", "worst_case", "torture"]:
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

func test_relayed_models_a_relay_rather_than_a_radio() -> void:
	# `relayed` is the one profile whose story is duplication and reordering rather than loss and jitter, because
	# a relay service forwards over its own backbone and may carry a copy on a second route. Pin the numbers and
	# pin the CONTRAST with the wireless profiles -- a relay profile whose dup/reorder drifted to zero would be
	# congested_wifi with different latency, and the bench would be back to having no coverage of that shape.
	var p: NetProfile = NetProfiles.get_profile("relayed")
	assert_almost_eq(p.latency_ms, 60.0, 0.0, "relayed is 60ms one-way (two access legs plus a backbone middle)")
	assert_almost_eq(p.jitter_ms, 10.0, 0.0, "relayed jitter is low relative to its latency (a provisioned backbone)")
	assert_almost_eq(p.loss, 0.005, 1e-6, "relayed drops 0.5% -- loss is not the story on a relay")
	assert_almost_eq(p.dup, 0.02, 1e-6, "relayed duplicates 2% (a copy carried on a second route)")
	assert_almost_eq(p.reorder, 0.05, 1e-6, "relayed reorders 5%")
	assert_almost_eq(p.reorder_ms, 30.0, 0.0, "a reordered packet lands 30ms late (the spread between two routes)")
	assert_false(p.burst, "relayed uses uniform loss -- the bursty model describes a contended radio")
	var wifi: NetProfile = NetProfiles.get_profile("congested_wifi")
	assert_almost_eq(wifi.dup, 0.0, 0.0, "the wireless profiles do not duplicate")
	assert_almost_eq(wifi.reorder, 0.0, 0.0, "the wireless profiles do not reorder")
	assert_true(p.dup > wifi.dup and p.reorder > wifi.reorder, "relayed is the profile that exercises dup + reorder")
	assert_true(p.jitter_ms < wifi.jitter_ms and p.loss < wifi.loss, "and it is steadier and cleaner than the radio")

func test_relayed_dup_and_reorder_round_trip() -> void:
	# `relayed` is the first shipped profile with non-zero dup/reorder/reorder_ms, so those three knobs ride a
	# to_dict()/from_dict() snapshot for the first time -- the path a profile takes into a CLI arg blob and a
	# metrics artifact header. A knob that silently reset to 0 there would condition nothing on the relay.
	# ASSERT AGAINST THE LITERALS, not against `original`. get_profile() hands back duplicate_profile(), which is
	# itself from_dict(to_dict()) -- comparing the two would put the same serialization path on both sides of the
	# assertion, and a key dropped from to_dict() would read 0.0 == 0.0 and pass.
	var original: NetProfile = NetProfiles.get_profile("relayed")
	var restored: NetProfile = NetProfile.new()
	restored.from_dict(original.to_dict())
	assert_almost_eq(restored.dup, 0.02, 1e-6, "dup survives to_dict()/from_dict()")
	assert_almost_eq(restored.reorder, 0.05, 1e-6, "reorder survives to_dict()/from_dict()")
	assert_almost_eq(restored.reorder_ms, 30.0, 0.0, "reorder_ms survives to_dict()/from_dict()")
	assert_almost_eq(restored.latency_ms, 60.0, 0.0, "the latency the reorder delay rides on survives with them")
	assert_true(original.describe().contains("dup=2.0%"), "describe() surfaces the dup rate in the run's marker line")

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
