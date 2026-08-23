extends UnitTest
## Scene-free coverage for [RemoteCadence] -- "the remote bodies move choppily", as a number.
##
## It is the one class in the addon with no coverage anywhere else in this repository: no probe drives it and
## the bench gate that prints its figure only formats what it is handed. It is also pure by construction --
## feed it observations, ask it for the distribution -- so the whole rule belongs here.
##
## What the rule has to get right, and what each of these tests pins:
##
## - **A gap is the interval between pose CHANGES**, charged to the tick the change landed on. A client
##   captures every watched body's pose every net tick whether or not a row arrived, so an unchanged pose is
##   a held frame and records nothing.
## - **A body that never moved contributes nothing.** Averaging its non-existent gaps in would report a still
##   arena as perfectly smooth.
## - **Gaps are split into a near band and a far one.** Interest culling is supposed to stop sending what a
##   peer is not looking at, so pooling the far bodies in reports a working cull as a regression. The near
##   band is the figure an A/B compares.
## - **A gap longer than any rota can explain is counted as an absence, never averaged.** A despawn, a death
##   or a cull stops rows for seconds, and that is a different fact from a starved rota slot, with a
##   different fix.
## - **The first observation seeds a baseline only.** We do not know how long a body had been sitting there
##   before the window opened, so the interval up to its first observed change is discarded.

const NEAR: float = 5.0    # a distance inside the default near band
const FAR: float = 500.0   # and one outside it

## Walk a body in a straight line, one step per change, so every observation carries a distinct pose.
func _pose(step: int) -> Vector3:
	return Vector3(float(step), 0.0, 0.0)

# --- held frames ----------------------------------------------------------------------------------

func test_a_row_every_tick_is_a_gap_of_one() -> void:
	# The floor, and the cadence NetLagComp.INTERP_TICKS assumes: a body whose pose changes on every tick.
	var c: RemoteCadence = RemoteCadence.new()
	for tick: int in range(0, 10):
		c.observe(1, _pose(tick), tick, NEAR)
	assert_almost_eq(RemoteCadence.mean_of(c.near_gaps()), 1.0, 1e-6, "a row every tick means a mean gap of 1")
	assert_eq(c.near_gaps().size(), 8, "nine changes, the first of which seeds the baseline instead")

func test_held_frames_are_charged_to_the_tick_the_change_lands_on() -> void:
	# Rows every four ticks: the body holds for three and covers four ticks of travel in one. That is what a
	# player reports as stutter, and what a per-peer byte budget produces when it cannot carry everything.
	var c: RemoteCadence = RemoteCadence.new()
	var step: int = 0
	for tick: int in range(0, 21):
		if tick % 4 == 0:
			step += 1
		c.observe(1, _pose(step), tick, NEAR)
	var gaps: Array[int] = c.near_gaps()
	assert_eq(gaps.size(), 4, "five changes, the first seeding the baseline")
	assert_almost_eq(RemoteCadence.mean_of(gaps), 4.0, 1e-6, "and each is a four-tick hold")

func test_a_body_that_never_moves_contributes_nothing() -> void:
	# A body standing still is not evidence about the rota. bodies_seen beside bodies_moving is how a reader
	# tells that a window measured stillness instead.
	var c: RemoteCadence = RemoteCadence.new()
	for tick: int in range(0, 60):
		c.observe(1, Vector3(3.0, 0.0, 4.0), tick, NEAR)
	assert_eq(c.near_gaps().size(), 0, "no change, no gap")
	assert_eq(c.bodies_seen(), 1, "the body was watched all window")
	assert_eq(c.bodies_moving(), 0, "and contributed nothing to the figure")

func test_the_first_change_seeds_the_baseline_rather_than_a_gap() -> void:
	# We do not know how long the body had been sitting there before the window opened, so the interval up to
	# its first observed change measures the window's start, not the rota.
	var c: RemoteCadence = RemoteCadence.new()
	c.observe(1, _pose(0), 0, NEAR)
	c.observe(1, _pose(1), 40, NEAR)    # the first change: a 40-tick "gap" that measures nothing
	assert_eq(c.near_gaps().size(), 0, "the first change records no gap")
	assert_eq(c.bodies_moving(), 0, "and one change is not yet a moving body")
	c.observe(1, _pose(2), 43, NEAR)
	assert_eq(c.near_gaps().size(), 1, "the second change is the first measurable interval")
	assert_eq(c.near_gaps()[0], 3, "and it measures the interval since the first one")
	assert_eq(c.bodies_moving(), 1, "and now the body counts as moving")

# --- the bands ------------------------------------------------------------------------------------

func test_the_bands_split_at_the_near_radius_inclusively() -> void:
	# The first AOI A/B run read WORSE with culling on, purely because the far bodies it had correctly stopped
	# sending were still in the average. Splitting the bands is what makes that comparison mean anything.
	var c: RemoteCadence = RemoteCadence.new()
	c.near_radius_m = 10.0
	for body: int in [1, 2, 3]:
		for step: int in range(0, 4):
			var dist: float = 9.0 if body == 1 else (10.0 if body == 2 else 10.5)
			c.observe(body, _pose(step), step, dist)
	assert_eq(c.near_gaps().size(), 4, "inside and exactly at the radius are both near")
	assert_eq(c.far_gaps().size(), 2, "and only past it is far")

func test_a_body_crossing_the_radius_is_banded_per_observation() -> void:
	# The band is decided where the change landed, not where the body ended the window. A body running at the
	# player is near for the half of its gaps that happened near.
	var c: RemoteCadence = RemoteCadence.new()
	c.near_radius_m = 10.0
	c.observe(1, _pose(0), 0, FAR)
	c.observe(1, _pose(1), 1, FAR)   # baseline change
	c.observe(1, _pose(2), 2, FAR)
	c.observe(1, _pose(3), 3, NEAR)
	assert_eq(c.far_gaps().size(), 1, "the change that landed far is a far gap")
	assert_eq(c.near_gaps().size(), 1, "and the one that landed near is a near one")

func test_the_default_radius_suits_an_arena_rather_than_naming_a_network_quantity() -> void:
	# The default is a distance a fight happens inside; a game at another scale must set its own. Pinned so
	# that changing it is a deliberate act rather than a silent re-banding of every existing reading.
	assert_almost_eq(RemoteCadence.new().near_radius_m, 105.0, 1e-6, "the default near band, in metres")

# --- absences -------------------------------------------------------------------------------------

func test_a_gap_no_rota_can_explain_is_counted_as_an_absence() -> void:
	# Folding a despawn into the distribution produced a 6801-tick "gap" (113 s) beside a p50 of 2.
	var c: RemoteCadence = RemoteCadence.new()
	c.observe(1, _pose(0), 0, NEAR)
	c.observe(1, _pose(1), 1, NEAR)      # baseline change
	c.observe(1, _pose(2), 2, NEAR)      # an ordinary one-tick gap
	c.observe(1, _pose(3), 6803, NEAR)   # gone for nearly two minutes, then back
	assert_eq(c.near_gaps().size(), 1, "only the credible gap is averaged")
	assert_eq(c.near_gaps()[0], 1, "and it is the ordinary one-tick interval")
	assert_eq(c.absences(), 1, "and the hole is reported as its own fact")

func test_the_absence_threshold_is_five_seconds_at_sixty_hertz() -> void:
	# Nothing the send path does to a body it is STILL replicating produces a five-second hold, which is what
	# makes the split safe. Both sides of the boundary, so the threshold cannot drift unnoticed.
	var c: RemoteCadence = RemoteCadence.new()
	c.observe(1, _pose(0), 0, NEAR)
	c.observe(1, _pose(1), 1, NEAR)
	c.observe(1, _pose(2), 301, NEAR)    # exactly 300 ticks: still a gap
	assert_eq(c.near_gaps().size(), 1, "300 ticks is still a gap")
	assert_eq(c.near_gaps()[0], 300, "and it is the longest credible one")
	assert_eq(c.absences(), 0, "and is not an absence")
	c.observe(1, _pose(3), 602, NEAR)    # 301 ticks: past it
	assert_eq(c.near_gaps().size(), 1, "301 ticks is not averaged in")
	assert_eq(c.absences(), 1, "it is an absence")

# --- several bodies -------------------------------------------------------------------------------

func test_bodies_are_tracked_independently() -> void:
	# One body starved while another is fine is exactly the reading the rota can produce, so the per-entity
	# state must not be shared.
	var c: RemoteCadence = RemoteCadence.new()
	for tick: int in range(0, 13):
		c.observe(1, _pose(tick), tick, NEAR)              # every tick
		c.observe(2, _pose(tick / 3), tick, NEAR)          # every third tick
	assert_eq(c.bodies_seen(), 2, "both bodies were watched")
	assert_eq(c.bodies_moving(), 2, "and both moved enough to count")
	var gaps: Array[int] = c.near_gaps()
	assert_eq(gaps[0], 1, "the smallest gap is the body arriving every tick")
	assert_eq(gaps[gaps.size() - 1], 3, "and the largest is the one arriving every third")

func test_a_body_seen_once_is_seen_but_not_moving() -> void:
	var c: RemoteCadence = RemoteCadence.new()
	c.observe(1, _pose(0), 0, NEAR)
	c.observe(2, _pose(0), 0, NEAR)
	assert_eq(c.bodies_seen(), 2, "both were observed")
	assert_eq(c.bodies_moving(), 0, "neither has produced a measurable interval")

# --- out-of-order ticks ---------------------------------------------------------------------------

func test_a_tick_that_went_backwards_records_nothing_and_re_bases() -> void:
	# A window straddling a hard clock resync can hand the same body an older tick than the one before. A
	# negative interval measures nothing, so it is dropped -- and the body re-bases on the tick it was last
	# seen at rather than staying anchored to a future one.
	var c: RemoteCadence = RemoteCadence.new()
	c.observe(1, _pose(0), 0, NEAR)
	c.observe(1, _pose(1), 5, NEAR)    # baseline change
	c.observe(1, _pose(2), 3, NEAR)    # backwards: no gap
	assert_eq(c.near_gaps().size(), 0, "a negative interval is not recorded")
	c.observe(1, _pose(3), 9, NEAR)
	assert_eq(c.near_gaps().size(), 1, "the next change is measurable again")
	assert_eq(c.near_gaps()[0], 6, "and is measured from the re-based tick, not the future one")

# --- the distribution -----------------------------------------------------------------------------

func test_the_gap_lists_come_back_ascending_and_as_copies() -> void:
	# percentile_of() takes an ASCENDING array by contract, and the recorder is the only thing that can
	# guarantee that. Handing out its own array would let a caller's sort or clear reach the recorder.
	var c: RemoteCadence = RemoteCadence.new()
	c.observe(1, _pose(0), 0, NEAR)
	c.observe(1, _pose(1), 1, NEAR)
	c.observe(1, _pose(2), 9, NEAR)    # gap 8
	c.observe(1, _pose(3), 11, NEAR)   # gap 2
	c.observe(1, _pose(4), 16, NEAR)   # gap 5
	var sorted: Array[int] = c.near_gaps()
	assert_eq(sorted.size(), 3, "three measurable intervals")
	assert_eq(sorted[0], 2, "ascending, whatever order they arrived in")
	assert_eq(sorted[1], 5, "the middle gap")
	assert_eq(sorted[2], 8, "and the largest last")
	var taken: Array[int] = c.near_gaps()
	taken.clear()
	assert_eq(c.near_gaps().size(), 3, "and clearing the copy leaves the recorder alone")

func test_mean_of_an_empty_distribution_is_zero_rather_than_a_division() -> void:
	var none: Array[int] = []
	assert_almost_eq(RemoteCadence.mean_of(none), 0.0, 1e-6, "no gaps, no mean")
	assert_almost_eq(RemoteCadence.percentile_of(none, 0.95), 0.0, 1e-6, "and no percentile")

func test_the_percentile_is_nearest_rank_over_an_ascending_array() -> void:
	# Read the p95, not the mean: a mean of 1.2 with a p95 of 9 is a body that is mostly fine and visibly
	# jumps twice a second, which is what a player notices and what a mean hides.
	# Eighteen ordinary ticks and two visible jumps, ascending as the recorder hands them over.
	var gaps: Array[int] = []
	for i: int in range(0, 18):
		gaps.push_back(1)
	gaps.push_back(20)
	gaps.push_back(20)
	assert_almost_eq(RemoteCadence.mean_of(gaps), 2.9, 1e-6, "the mean hides the jump")
	assert_almost_eq(RemoteCadence.percentile_of(gaps, 0.95), 20.0, 1e-6, "the p95 is the jump")
	assert_almost_eq(RemoteCadence.percentile_of(gaps, 0.50), 1.0, 1e-6, "and the p50 is the ordinary tick")

func test_the_percentile_clamps_its_quantile_rather_than_indexing_out_of_range() -> void:
	var gaps: Array[int] = [2, 4, 6, 8]
	assert_almost_eq(RemoteCadence.percentile_of(gaps, 0.0), 2.0, 1e-6, "p0 is the smallest gap")
	assert_almost_eq(RemoteCadence.percentile_of(gaps, 1.0), 8.0, 1e-6, "p100 is the largest")
	assert_almost_eq(RemoteCadence.percentile_of(gaps, 4.0), 8.0, 1e-6, "a quantile above 1 clamps to it")
	assert_almost_eq(RemoteCadence.percentile_of(gaps, -1.0), 2.0, 1e-6, "and one below 0 clamps the other way")

func test_a_single_gap_is_its_own_every_percentile() -> void:
	var one: Array[int] = [7]
	assert_almost_eq(RemoteCadence.percentile_of(one, 0.95), 7.0, 1e-6, "one sample, one answer")

# --- windowing ------------------------------------------------------------------------------------

func test_reset_drops_everything_so_a_probe_can_measure_a_named_window() -> void:
	# An A/B run measures a named window rather than a whole process, and a baseline left over from the
	# previous arm is the difference the run is trying to report.
	var c: RemoteCadence = RemoteCadence.new()
	for tick: int in range(0, 10):
		c.observe(1, _pose(tick), tick, NEAR)
	c.observe(2, _pose(0), 0, FAR)
	c.observe(2, _pose(1), 1, FAR)
	c.observe(2, _pose(2), 400, FAR)   # an absence
	c.reset()
	assert_eq(c.near_gaps().size(), 0, "no near gaps survive")
	assert_eq(c.far_gaps().size(), 0, "no far gaps either")
	assert_eq(c.absences(), 0, "no absences")
	assert_eq(c.bodies_seen(), 0, "no bodies")
	assert_eq(c.bodies_moving(), 0, "and nothing moving")
	# And the next window seeds its own baseline rather than measuring from the old one.
	c.observe(1, _pose(50), 500, NEAR)
	c.observe(1, _pose(51), 501, NEAR)
	assert_eq(c.near_gaps().size(), 0, "the first change after a reset seeds a baseline, as at the start")
