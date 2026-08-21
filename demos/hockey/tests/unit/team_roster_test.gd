extends UnitTest
## TeamRoster: alternating ends, balance under drop-out, and the sender-id security rule.

func test_an_empty_table_seats_players_alternately() -> void:
	var roster: TeamRoster = TeamRoster.new()
	assert_eq(roster.assign(10), 0, "the first player takes seat 0")
	assert_eq(roster.assign(11), 1, "the second takes the other end")
	assert_eq(roster.assign(12), 2, "the third comes back to the first end")
	assert_eq(roster.assign(13), 3, "and so on -- strict alternation while nobody has left")
	assert_eq(HockeyConfig.team_of_seat(0), HockeyConfig.team_of_seat(2), "seats 0 and 2 share an end")
	assert_true(HockeyConfig.team_of_seat(0) != HockeyConfig.team_of_seat(1), "0 and 1 do not")

func test_assigning_twice_returns_the_same_seat() -> void:
	var roster: TeamRoster = TeamRoster.new()
	var first: int = roster.assign(10)
	assert_eq(roster.assign(10), first, "a peer already seated keeps its seat rather than taking a second")
	assert_eq(roster.occupied(), 1, "and the table still holds one player")

func test_a_mid_round_join_refills_the_thinner_end() -> void:
	# The rule that makes "alternating ends" survive a drop-out. Strict alternation would send the next
	# joiner to the end that already has more players and deepen the gap.
	var roster: TeamRoster = TeamRoster.new()
	for peer: int in [10, 11, 12, 13]:
		roster.assign(peer)
	assert_eq(roster.occupied_on_team(0), 2, "two a side to start")
	assert_eq(roster.occupied_on_team(1), 2, "two a side to start")
	roster.release(11)                      # a team 1 player leaves
	assert_eq(roster.occupied_on_team(1), 1, "team 1 is a player down")
	assert_eq(roster.assign(14), 1, "the next joiner takes the freed seat on the THIN end")
	assert_eq(roster.occupied_on_team(0), 2, "and the sides are level again")
	assert_eq(roster.occupied_on_team(1), 2, "and the sides are level again")

func test_a_released_seat_is_reused_lowest_first() -> void:
	var roster: TeamRoster = TeamRoster.new()
	for peer: int in [10, 11, 12, 13, 14, 15]:
		roster.assign(peer)
	roster.release(14)                      # seat 4, on team 0
	roster.release(10)                      # seat 0, on team 0
	assert_eq(roster.assign(20), 0, "the lowest free seat on the thin end wins, so a pool never fragments")
	assert_eq(roster.seat_of_peer(20), 0, "and the table agrees")
	assert_eq(roster.peer_of_seat(0), 20, "in both directions")

func test_releasing_an_unseated_peer_is_harmless() -> void:
	var roster: TeamRoster = TeamRoster.new()
	roster.assign(10)
	roster.release(999)
	assert_eq(roster.occupied(), 1, "a disconnect for a peer that never held a seat changes nothing")

func test_the_table_fills_and_then_refuses() -> void:
	var roster: TeamRoster = TeamRoster.new()
	for index: int in HockeyConfig.SEATS:
		assert_true(roster.assign(100 + index) >= 0, "seat %d is handed out" % index)
	assert_true(roster.is_full(), "the pool is full")
	assert_eq(roster.assign(9999), -1,
		"and one more is refused rather than seated on top of somebody -- the transport is capped at the same "
		+ "number, so a caller reaching this has let the two disagree")

func test_the_seat_comes_from_the_sender_id() -> void:
	# The entire security model for the command channel. A client says WHAT it wants; the server decides WHO
	# is asking, from a value the transport supplies and the sender cannot author.
	var roster: TeamRoster = TeamRoster.new()
	roster.assign(77)
	assert_eq(roster.seat_for_sender(77), 0, "a seated peer resolves to its own seat")
	assert_eq(roster.seat_for_sender(78), -1, "an unseated peer resolves to no seat, never to a default one")

func test_offline_resolves_to_seat_zero_without_a_table_lookup() -> void:
	# Offline there is no peer list to be in, so a lookup would return -1 and every single-player serve would
	# be rejected as unseated -- and the offline path is the one people develop against.
	var roster: TeamRoster = TeamRoster.new()
	assert_eq(roster.seat_for_sender(TeamRoster.OFFLINE_SENDER), 0, "the offline sentinel is seat 0")
	assert_eq(roster.occupied(), 0, "and nothing was seated to make that true")

func test_clear_forgets_everyone() -> void:
	var roster: TeamRoster = TeamRoster.new()
	roster.assign(10)
	roster.assign(11)
	roster.clear()
	assert_eq(roster.occupied(), 0, "session teardown empties the table")
	assert_eq(roster.assign(12), 0, "and the next session starts from seat 0")
