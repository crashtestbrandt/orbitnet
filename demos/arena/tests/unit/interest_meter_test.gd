extends UnitTest
## InterestMeter: the client-side reading of what is actually being sent.

const NOW: int = 900

func test_a_recent_row_counts_as_received() -> void:
	assert_true(InterestMeter.is_fresh(NOW, NOW), "a row that arrived this tick is fresh")
	assert_true(InterestMeter.is_fresh(NOW - InterestMeter.STALE_TICKS, NOW),
		"and one exactly at the threshold still is -- the bound is inclusive")

func test_an_old_row_does_not() -> void:
	assert_false(InterestMeter.is_fresh(NOW - InterestMeter.STALE_TICKS - 1, NOW),
		"one tick past the threshold reads as no longer being sent")

func test_never_having_arrived_is_not_the_same_as_stale() -> void:
	# Both answer false, but only one of them is a cull. A caller counting "never arrived" as staleness would
	# report a fresh join as a session-wide outage.
	assert_false(InterestMeter.is_fresh(-1, NOW), "-1 means no row has ever arrived for this entity")

func test_the_threshold_is_generous_enough_for_the_send_rota() -> void:
	# A body at the far edge of the rota can legitimately wait several ticks for its turn under a byte
	# budget. Calling that a cull would report the rota as a filter.
	assert_true(InterestMeter.STALE_TICKS >= 8,
		"the threshold leaves room for a deferred block to arrive before it is called a cull")

func test_a_null_world_reads_empty_rather_than_crashing() -> void:
	var reading: InterestMeter.Reading = InterestMeter.read(null, NOW)
	assert_eq(reading.total(), 0, "nothing exists")
	assert_eq(reading.total_fresh(), 0, "so nothing is being received")
	assert_eq(reading.fighters_by_arena.size(), ArenaConfig.ARENAS,
		"and the per-arena breakdown is still the right shape, so a readout can index it")

func test_the_reading_sums_its_parts() -> void:
	var reading: InterestMeter.Reading = InterestMeter.Reading.new()
	reading.fighters_fresh = 3
	reading.props_fresh = 40
	reading.cards_fresh = 1
	reading.fighters_total = 24
	reading.props_total = 288
	reading.cards_total = 3
	assert_eq(reading.total_fresh(), 44, "the three lanes' fresh counts add up")
	assert_eq(reading.total(), 315, "and so do their totals")
