extends UnitTest
## ReconcileMeter: the demo's signature number.

func test_a_first_visit_reports_nothing_to_compare() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	assert_almost_eq(meter.note(10, Vector3(0.1, 0.0, 0.2)), -1.0, 0.0001,
		"-1.0 is 'no record yet', which is not the same as 'no error'")
	assert_eq(meter.sample_count(), 0, "and nothing enters the distribution")
	assert_eq(meter.corrections(), 0, "nor the correction count")
	assert_eq(meter.visits(), 1, "but the visit is counted")

func test_a_replay_that_agrees_is_a_zero_correction() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	var at: Vector3 = Vector3(0.1, 0.0, 0.2)
	meter.note(10, at)
	assert_almost_eq(meter.note(10, at), 0.0, 0.000001, "a replay that lands in the same place corrected nothing")
	assert_eq(meter.corrections(), 0, "so it is not counted as a correction")
	assert_eq(meter.sample_count(), 1, "though it is a sample, which is what makes the p50 honest")

func test_a_replay_that_moves_the_puck_is_measured() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	meter.note(10, Vector3.ZERO)
	var error: float = meter.note(10, Vector3(0.05, 0.0, 0.0))
	assert_almost_eq(error, 0.05, 0.000001, "the correction is the distance between the two answers, in metres")
	assert_eq(meter.corrections(), 1, "and it counts")
	assert_almost_eq(meter.percentile_mm(0.5), 50.0, 0.01, "reported to a human in millimetres")

func test_the_latest_answer_becomes_the_new_record() -> void:
	# A tick can be replayed more than once. Each pass is measured against the previous one, so a series of
	# corrections is not counted repeatedly from the original prediction.
	var meter: ReconcileMeter = ReconcileMeter.new()
	meter.note(10, Vector3.ZERO)
	meter.note(10, Vector3(0.05, 0.0, 0.0))
	assert_almost_eq(meter.note(10, Vector3(0.06, 0.0, 0.0)), 0.01, 0.000001,
		"the second correction is measured from the first, not from the original prediction")

func test_percentiles_order_the_window() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	for index: int in 10:
		meter.note(index, Vector3.ZERO)
		meter.note(index, Vector3(float(index) * 0.001, 0.0, 0.0))
	assert_almost_eq(meter.percentile_mm(0.0), 0.0, 0.01, "the smallest correction")
	assert_almost_eq(meter.percentile_mm(1.0), 9.0, 0.01, "the largest")
	assert_almost_eq(meter.peak_mm(), 9.0, 0.01, "and the peak agrees with the top of the distribution")
	assert_true(meter.percentile_mm(0.5) > 0.0 and meter.percentile_mm(0.5) < 9.0, "with a p50 between them")

func test_an_empty_meter_reports_zero_rather_than_erroring() -> void:
	# A HUD reads these every frame from the first one, before anything has been simulated.
	var meter: ReconcileMeter = ReconcileMeter.new()
	assert_almost_eq(meter.percentile_mm(0.5), 0.0, 0.0001, "no samples, no percentile")
	assert_almost_eq(meter.peak_mm(), 0.0, 0.0001, "and no peak")

func test_the_window_bounds_the_distribution() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	for index: int in ReconcileMeter.WINDOW * 2:
		var tick: int = index % ReconcileMeter.RING
		meter.note(tick, Vector3.ZERO)
		meter.note(tick, Vector3(0.01, 0.0, 0.0))
	assert_eq(meter.sample_count(), ReconcileMeter.WINDOW,
		"the percentile window is bounded, so flipping a lever moves the number while you are looking at it")
	assert_true(meter.corrections() > ReconcileMeter.WINDOW,
		"while the lifetime count keeps climbing")

func test_a_tick_evicted_from_the_ring_reads_as_a_first_visit() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	meter.note(0, Vector3.ZERO)
	meter.note(ReconcileMeter.RING, Vector3(1.0, 0.0, 0.0))   # same slot, newer tick
	assert_almost_eq(meter.note(0, Vector3(9.0, 0.0, 0.0)), -1.0, 0.0001,
		"a tick the backend can no longer replay is not compared against a stale record")

func test_a_negative_tick_is_ignored() -> void:
	# Net.current_tick() is 0 offline and the rollback tick is -1 before a loop runs.
	var meter: ReconcileMeter = ReconcileMeter.new()
	assert_almost_eq(meter.note(-1, Vector3.ZERO), -1.0, 0.0001, "there is no such tick to record")
	assert_eq(meter.visits(), 0, "and it is not counted as a pass")

func test_reset_clears_the_record() -> void:
	var meter: ReconcileMeter = ReconcileMeter.new()
	meter.note(5, Vector3.ZERO)
	meter.note(5, Vector3(0.02, 0.0, 0.0))
	meter.reset()
	assert_eq(meter.sample_count(), 0, "session teardown empties the window")
	assert_eq(meter.corrections(), 0, "and the counters")
	assert_almost_eq(meter.note(5, Vector3.ZERO), -1.0, 0.0001, "and the ring, so tick 5 is new again")
