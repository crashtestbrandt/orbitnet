extends UnitTest
## The two bitfields on the wire: the puck's flags and the scoreboard's counters.
##
## Both are packed because a lane pays a per-property entry and the fields inside each always change together.
## Both are static, so they are exercised here with no node and no session.

# --- the puck's flags ------------------------------------------------------------------------------
func test_flags_round_trip() -> void:
	var packed: int = PuckBody.pack_flags(true, 37, 1234, 1)
	assert_true(PuckBody.flags_live(packed), "liveness")
	assert_eq(PuckBody.flags_faceoff(packed), 37, "the face-off countdown")
	assert_eq(PuckBody.flags_sequence(packed), 1234, "the serve sequence")
	assert_eq(PuckBody.flags_to_team(packed), 1, "and which end is being served toward")

func test_the_fields_do_not_bleed_into_each_other() -> void:
	assert_eq(PuckBody.flags_faceoff(PuckBody.pack_flags(false, 0, 65535, 1)), 0,
		"a full sequence does not read back as a countdown")
	assert_eq(PuckBody.flags_sequence(PuckBody.pack_flags(true, 1023, 0, 1)), 0,
		"nor a full countdown as a sequence")
	assert_false(PuckBody.flags_live(PuckBody.pack_flags(false, 1023, 65535, 1)),
		"and a full field does not set the liveness bit")

func test_a_dead_puck_at_zero_is_the_default_state() -> void:
	# A freshly zeroed entity must be meaningful: dead, no countdown, no goals yet.
	assert_false(PuckBody.flags_live(0), "zero is not live")
	assert_eq(PuckBody.flags_faceoff(0), 0, "with no countdown")
	assert_eq(PuckBody.flags_sequence(0), 0, "and no serve yet")

func test_the_countdown_field_holds_the_shipped_value() -> void:
	assert_true(HockeyConfig.FACEOFF_TICKS <= 1023,
		"the face-off countdown has to fit its 10 bits, or a longer one would wrap to a shorter one")
	var packed: int = PuckBody.pack_flags(false, HockeyConfig.FACEOFF_TICKS, 1, 0)
	assert_eq(PuckBody.flags_faceoff(packed), HockeyConfig.FACEOFF_TICKS, "and it does")

# --- the scoreboard --------------------------------------------------------------------------------
func test_scores_round_trip_independently() -> void:
	var packed: int = Scoreboard.pack_score(7, 3)
	assert_eq(Scoreboard.score_of(packed, 0), 7, "team 0")
	assert_eq(Scoreboard.score_of(packed, 1), 3, "team 1")
	var lopsided: int = Scoreboard.pack_score(0, 65535)
	assert_eq(Scoreboard.score_of(lopsided, 0), 0, "a full team 1 counter does not leak into team 0")
	assert_eq(Scoreboard.score_of(lopsided, 1), 65535, "and reads back whole")

func test_the_goal_event_distinguishes_no_goal_from_a_goal_for_team_zero() -> void:
	# `team` is stored offset by one so a freshly zeroed entity reads as "no goal yet" rather than as a goal
	# for team 0 -- which a client would otherwise flash on join, once, on every session.
	assert_eq(Scoreboard.goal_team(0), -1, "zero means nothing has been scored")
	assert_eq(Scoreboard.goal_sequence(0), 0, "and no sequence has been issued")
	assert_eq(Scoreboard.goal_team(Scoreboard.pack_goal(0, 1)), 0, "team 0's first goal is team 0's")
	assert_eq(Scoreboard.goal_sequence(Scoreboard.pack_goal(0, 1)), 1, "with sequence 1")
	assert_eq(Scoreboard.goal_team(Scoreboard.pack_goal(1, 900)), 1, "and team 1's is team 1's")
	assert_eq(Scoreboard.goal_sequence(Scoreboard.pack_goal(1, 900)), 900, "with its own sequence")

func test_awarding_a_goal_moves_one_counter_and_the_event() -> void:
	var board: Scoreboard = Scoreboard.new()
	board.award(0)
	assert_eq(board.goals(0), 1, "the scoring team gains one")
	assert_eq(board.goals(1), 0, "the other does not")
	assert_eq(board.last_scorer(), 0, "and the event names the scorer")
	var first: int = board.last_sequence()
	board.award(1)
	assert_eq(board.goals(0), 1, "the first goal is not lost")
	assert_eq(board.goals(1), 1, "and the second lands")
	assert_true(board.last_sequence() != first,
		"the sequence moves on every goal, which is how a client flashes each one exactly once")
	board.free()

func test_an_out_of_range_team_awards_nothing() -> void:
	var board: Scoreboard = Scoreboard.new()
	board.award(-1)
	board.award(7)
	assert_eq(board.goals(0), 0, "nothing was awarded")
	assert_eq(board.goals(1), 0, "to either team")
	assert_eq(board.last_sequence(), 0, "and no event was issued")
	board.free()

func test_reset_clears_the_record() -> void:
	var board: Scoreboard = Scoreboard.new()
	board.award(0)
	board.reset()
	assert_eq(board.goals(0), 0, "a fresh world starts at nil-nil")
	assert_eq(board.last_sequence(), 0, "with no goal to flash")
	board.free()
