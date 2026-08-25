extends UnitTest
## Scene-free coverage for [BenchMetrics]' WINDOW ACCOUNTING -- the arithmetic that turns the monotonic run
## totals a game publishes into a figure about the measured window, and the two orientation residuals that
## answer different questions about the same samples.
##
## The recorder's other half -- the per-tick sampling, the CSV, the gate call -- needs a live session and a
## subject and belongs to a bench run. What is pure here, and what this suite drives directly, is the
## bookkeeping each sample feeds:
##
## - **A WINDOW SUBTRACTS ITS OWN FIRST READING.** The game publishes run totals, so a bench window opened
##   mid-session must not report the whole session's shots as its own. Reading the raw total is how a
##   `--bench-duration=20` run over a five-minute session reports five minutes of combat.
## - **A COUNTER THAT GOES BACKWARD REPORTS ZERO.** A respawn or a reconnect can re-seed a total, and a
##   negative count is worse than no count: the gate would read it as a peer that fired nothing.
## - **PEAK AND STANDING RESIDUAL ARE DIFFERENT FACTS.** The peak is monotonic in the LENGTH of the run, so
##   it cannot tell one absorbed correction from a tilt that never bled away. The standing figure -- the
##   worst trailing-window MINIMUM -- can, and it is the one that has to reach zero before an orientation
##   arm may ship.
## - **A PARTIAL WINDOW EARNS NO FLOOR.** Fewer samples than the trailing window is not evidence of a
##   standing residual, and reporting one from a short run would fail an arm on the run's length.
##
## The recorder is constructed with `.new()` and never enters the tree, so `_ready()` -- which is the half
## that opens the CSV and subscribes to the tick -- never runs. Every sample is handed in directly.

## The trailing window the standing floor is taken over. Read off the recorder rather than copied, so the two
## cannot drift apart.
const STANDING_WINDOW: int = BenchMetrics._STANDING_WINDOW

func _combat(shots: int, hits: int) -> Dictionary:
	var out: Dictionary = {}
	out[BenchSubject.KEY_SHOTS_FIRED] = shots
	out[BenchSubject.KEY_HITS_CONFIRMED] = hits
	return out

func _orientation(smooth: int, miss: int, err: float, armed: bool) -> Dictionary:
	var out: Dictionary = {}
	out[BenchSubject.KEY_ORIENT_SMOOTH] = smooth
	out[BenchSubject.KEY_ORIENT_MISS] = miss
	out[BenchSubject.KEY_ORIENT_ERROR] = err
	out[BenchSubject.KEY_ORIENT_ARMED] = 1.0 if armed else 0.0
	return out

# --- the combat window ----------------------------------------------------------------------------

func test_a_recorder_that_sampled_nothing_reports_nothing() -> void:
	# A dedicated server, or a client that was dead for the whole window, publishes no game numbers at all.
	# That has to read as zero rather than as a negative difference against an unset first reading.
	var m: BenchMetrics = BenchMetrics.new()
	assert_eq(m.shots_fired(), 0, "no samples, no shots")
	assert_eq(m.hits_confirmed(), 0, "no samples, no confirms")
	m.free()

func test_a_window_opened_mid_session_subtracts_its_own_first_reading() -> void:
	# The game publishes monotonic RUN totals. A 20-second window over a five-minute session that reported
	# the raw total would report five minutes of combat as its own.
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_combat(_combat(1000, 400))
	m._sample_combat(_combat(1012, 405))
	m._sample_combat(_combat(1015, 407))
	assert_eq(m.shots_fired(), 15, "fifteen rounds fired inside the window")
	assert_eq(m.hits_confirmed(), 7, "and seven of them confirmed inside it")
	m.free()

func test_a_single_sample_is_a_window_of_zero_length() -> void:
	# One sample is a first reading and a last reading at once, so the difference is zero -- not the total,
	# which would be the whole run credited to a window that measured one tick.
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_combat(_combat(1000, 400))
	assert_eq(m.shots_fired(), 0, "one sample measures no interval")
	assert_eq(m.hits_confirmed(), 0, "on either counter")
	m.free()

func test_a_window_over_a_session_that_started_at_zero_reports_the_whole_total() -> void:
	# The subtraction must not cost a run that DID start at zero anything: the first reading is 0, so the
	# difference is the total.
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_combat(_combat(0, 0))
	m._sample_combat(_combat(31, 12))
	assert_eq(m.shots_fired(), 31, "every round the run fired")
	assert_eq(m.hits_confirmed(), 12, "and every confirm it earned")
	m.free()

func test_a_counter_that_goes_backward_reports_zero_rather_than_a_negative() -> void:
	# A respawn or a reconnect can re-seed a total the recorder had already read. A negative count is worse
	# than no count: the hit-registration gate reads "fired a lot, confirmed nothing" as a broken mechanism.
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_combat(_combat(1000, 400))
	m._sample_combat(_combat(3, 1))
	assert_eq(m.shots_fired(), 0, "a re-seeded total reports no shots, never a negative one")
	assert_eq(m.hits_confirmed(), 0, "and no confirms")
	m.free()

func test_a_missing_field_reads_as_zero_rather_than_stopping_the_sample() -> void:
	# A game contributes what it has. A subject that publishes shots but not confirms must still get its
	# shots counted -- the columns a game cannot fill are legitimately flat, and every other one measures.
	var m: BenchMetrics = BenchMetrics.new()
	var partial: Dictionary = {}
	partial[BenchSubject.KEY_SHOTS_FIRED] = 40
	m._sample_combat(partial)
	partial[BenchSubject.KEY_SHOTS_FIRED] = 52
	m._sample_combat(partial)
	assert_eq(m.shots_fired(), 12, "the field the game does publish is counted")
	assert_eq(m.hits_confirmed(), 0, "and the one it does not reads flat")
	m.free()

# --- the orientation window -----------------------------------------------------------------------

func test_the_orientation_counters_are_windowed_the_same_way() -> void:
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_orientation(_orientation(80, 3, 0.0, true), 0.0)
	m._sample_orientation(_orientation(97, 5, 0.0, true), 0.0)
	assert_eq(m.orient_smoothed(), 17, "corrections absorbed inside the window")
	assert_eq(m.orient_misses(), 2, "and corrections that found no history row to correct against")
	m.free()

func test_the_armed_flag_latches_so_finish_still_knows_after_the_body_is_gone() -> void:
	# A client between death and respawn publishes nothing, and `finish()` runs after the last sample. Read
	# live at the end, the arm would report itself off on every run that ended with a dead body.
	var m: BenchMetrics = BenchMetrics.new()
	assert_false(m._orient_armed, "an unsampled run has seen no arm")
	m._sample_orientation(_orientation(0, 0, 0.0, true), 0.0)
	m._sample_orientation(_orientation(0, 0, 0.0, false), 0.0)
	assert_true(m._orient_armed, "once armed, a later quiet sample does not disarm the run")
	m.free()

func test_the_resim_peak_is_the_worst_tick_of_the_run() -> void:
	# The orientation gate reads it beside the residual: a residual measured over a run that never resimulated
	# says nothing about how corrections are absorbed.
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_orientation(_orientation(0, 0, 0.0, true), 2.0)
	m._sample_orientation(_orientation(0, 0, 0.0, true), 11.0)
	m._sample_orientation(_orientation(0, 0, 0.0, true), 4.0)
	assert_almost_eq(m._resim_max, 11.0, 1e-6, "the deepest resim the run paid for")
	m.free()

# --- peak versus standing residual ----------------------------------------------------------------

func test_the_peak_is_the_worst_instant_and_a_spike_leaves_no_floor() -> void:
	# THE DISTINCTION THE TWO FIGURES EXIST FOR. One correction that is absorbed shows up as a large peak and
	# no floor at all; reading the peak alone would fail an arm that works.
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_orientation(_orientation(0, 0, 5.0, true), 0.0)
	for _i: int in range(0, STANDING_WINDOW * 2):
		m._sample_orientation(_orientation(0, 0, 0.0, true), 0.0)
	assert_almost_eq(m.peak_orient_residual(), 5.0, 1e-6, "the spike is the peak")
	assert_almost_eq(m.standing_orient_residual(), 0.0, 1e-6, "and a residual that bled away leaves no floor")
	m.free()

func test_a_tilt_that_never_bleeds_away_holds_the_floor_up() -> void:
	# A residual that persists puts no zero in any window, so every window's minimum is above zero and the
	# floor rises to it. This is the figure that has to reach zero before an orientation arm may ship.
	var m: BenchMetrics = BenchMetrics.new()
	for _i: int in range(0, STANDING_WINDOW * 3):
		m._sample_orientation(_orientation(0, 0, 0.25, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.25, 1e-6, "the tilt the run never absorbed")
	assert_almost_eq(m.peak_orient_residual(), 0.25, 1e-6, "and the peak agrees, because nothing spiked")
	m.free()

func test_a_partial_window_earns_no_floor() -> void:
	# Fewer samples than the trailing window is not evidence of a standing residual. Reporting one would fail
	# an arm on the run's LENGTH rather than on its behavior.
	var m: BenchMetrics = BenchMetrics.new()
	for _i: int in range(0, STANDING_WINDOW - 1):
		m._sample_orientation(_orientation(0, 0, 0.4, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.0, 1e-6, "a window the run has not filled earns nothing")
	assert_almost_eq(m.peak_orient_residual(), 0.4, 1e-6, "though the peak is measurable from the first sample")
	m._sample_orientation(_orientation(0, 0, 0.4, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.4, 1e-6, "the sample that fills the window earns it")
	m.free()

func test_one_zero_anywhere_in_a_window_keeps_that_window_from_raising_the_floor() -> void:
	# A window containing a zero cannot raise the floor, because the residual demonstrably reached zero
	# inside it. That is what makes the figure "never bled away" rather than "was large at some point".
	var m: BenchMetrics = BenchMetrics.new()
	m._sample_orientation(_orientation(0, 0, 0.0, true), 0.0)
	for _i: int in range(0, STANDING_WINDOW - 1):
		m._sample_orientation(_orientation(0, 0, 0.9, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.0, 1e-6, "the zero is still inside the trailing window")
	# ...and once the zero has rolled out of the trailing window, the standing residual is earned.
	for _i: int in range(0, STANDING_WINDOW):
		m._sample_orientation(_orientation(0, 0, 0.9, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.9, 1e-6, "a window with no zero left in it raises the floor")
	m.free()

func test_the_floor_is_the_worst_window_and_a_later_recovery_does_not_erase_it() -> void:
	# The floor is a maximum over windows, so a run that stood at 0.3 for a while and then bled clean still
	# reports the period it stood -- which is the fact an arm has to answer for.
	var m: BenchMetrics = BenchMetrics.new()
	for _i: int in range(0, STANDING_WINDOW * 2):
		m._sample_orientation(_orientation(0, 0, 0.3, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.3, 1e-6, "the standing period is measured")
	for _i: int in range(0, STANDING_WINDOW * 2):
		m._sample_orientation(_orientation(0, 0, 0.0, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.3, 1e-6, "and recovering afterward does not erase it")
	m.free()

func test_the_floor_rises_to_the_worst_standing_level_rather_than_the_first() -> void:
	var m: BenchMetrics = BenchMetrics.new()
	for _i: int in range(0, STANDING_WINDOW * 2):
		m._sample_orientation(_orientation(0, 0, 0.1, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.1, 1e-6, "the first standing level")
	for _i: int in range(0, STANDING_WINDOW * 2):
		m._sample_orientation(_orientation(0, 0, 0.6, true), 0.0)
	assert_almost_eq(m.standing_orient_residual(), 0.6, 1e-6, "and a worse one later replaces it")
	m.free()

# --- the cadence the recorder owns ----------------------------------------------------------------

func test_a_recorder_starts_with_an_empty_cadence_rather_than_none() -> void:
	# A subject that does not publish remote bodies leaves the cadence empty, and the gate then reports no
	# cadence line at all rather than a line of zeroes. That needs a real recorder to ask, not a null.
	var m: BenchMetrics = BenchMetrics.new()
	var cadence: RemoteCadence = m.cadence()
	assert_true(cadence != null, "the recorder owns a cadence from construction")
	assert_eq(cadence.bodies_seen(), 0, "which has watched nothing yet")
	assert_eq(cadence.near_gaps().size(), 0, "and reports no gaps for the gate to print")
	m.free()
