extends UnitTest
## SeatRoster: several seats behind one connection, and seats that outlive the connection that held them.

const SESSION_A: int = 0x5EA701
const SESSION_B: int = 0x5EA702

func _one(roster: SeatRoster, peer: int) -> PackedInt32Array:
	return roster.assign(peer, 1)

# --- one seat ----------------------------------------------------------------------------------------
func test_the_first_peer_takes_the_lowest_seat() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = _one(roster, 10)
	assert_eq(seats.size(), 1, "it asked for one")
	assert_eq(seats[0], 0, "and got the lowest")

func test_assignment_is_idempotent() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var first: PackedInt32Array = _one(roster, 10)
	assert_eq(roster.assign(10, 2), first,
		"asking again returns what it already holds rather than consuming a second seat")
	assert_eq(roster.occupied(), 1, "so only one seat is taken")

# --- two seats, which is the whole difference ---------------------------------------------------------
func test_a_connection_can_drive_two_seats() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 2)
	assert_eq(seats.size(), 2, "split-screen is two owned bodies behind one connection")
	assert_true(seats[0] != seats[1], "and they are different bodies")

func test_the_request_is_bounded_by_the_configured_maximum() -> void:
	var roster: SeatRoster = SeatRoster.new()
	assert_eq(roster.assign(10, 99).size(), ArenaConfig.MAX_SEATS_PER_PEER,
		"a client asking for ninety seats gets the configured maximum, not ninety")

func test_spread_puts_the_second_seat_in_another_arena() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 2, SeatRoster.NO_SESSION, true)
	assert_eq(seats.size(), 2, "two seats")
	assert_true(ArenaConfig.arena_of_seat(seats[0]) != ArenaConfig.arena_of_seat(seats[1]),
		"a connection with a body in two WORLDS is the case worth having: it has no inferred world of its "
		+ "own, and the union of its seats' interest sets spans both")

func test_no_spread_keeps_both_seats_together() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 2, SeatRoster.NO_SESSION, false)
	assert_eq(ArenaConfig.arena_of_seat(seats[0]), ArenaConfig.arena_of_seat(seats[1]),
		"the ordinary split-screen case is two players in one arena")

func test_the_seat_index_is_per_connection() -> void:
	# The backend's seat index says which of THIS CONNECTION's bodies a fighter is, not which of the
	# session's. Two connections both have a seat 0.
	var roster: SeatRoster = SeatRoster.new()
	var mine: PackedInt32Array = roster.assign(10, 2)
	var theirs: PackedInt32Array = roster.assign(11, 2)
	assert_eq(roster.seat_index_for_peer(10, mine[0]), 0, "my first seat is index 0")
	assert_eq(roster.seat_index_for_peer(10, mine[1]), 1, "and my second is index 1")
	assert_eq(roster.seat_index_for_peer(11, theirs[0]), 0, "their first is also index 0")
	assert_eq(roster.seat_index_for_peer(10, theirs[0]), -1, "and I hold none of theirs")

# --- the security check ---------------------------------------------------------------------------------
func test_a_sender_owns_only_the_seats_it_was_assigned() -> void:
	# A connection with several seats cannot resolve a seat from the sender id alone, so the seat travels in
	# the payload -- which makes it a claim, and this is the check that turns a claim into a fact.
	var roster: SeatRoster = SeatRoster.new()
	var mine: PackedInt32Array = roster.assign(10, 2)
	var theirs: PackedInt32Array = roster.assign(11, 1)
	assert_true(roster.owns_seat(10, mine[0]), "the sender holds its own first seat")
	assert_true(roster.owns_seat(10, mine[1]), "and its second")
	assert_false(roster.owns_seat(10, theirs[0]), "and not somebody else's, however it labels the payload")
	assert_false(roster.owns_seat(4242, mine[0]), "a peer that was never seated owns nothing")

func test_the_offline_sender_owns_seat_zero_without_a_lookup() -> void:
	# Offline there is no peer list to be in, so a lookup would answer false and every single-player shot
	# would be refused as unowned -- and the offline path is the one people develop against.
	var roster: SeatRoster = SeatRoster.new()
	assert_true(roster.owns_seat(SeatRoster.OFFLINE_SENDER, 0), "the offline sentinel owns seat 0")
	assert_false(roster.owns_seat(SeatRoster.OFFLINE_SENDER, 1), "and only seat 0")

# --- held seats ------------------------------------------------------------------------------------------
func test_held_seats_go_back_to_the_identity_that_left_them() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 2)
	roster.hold(10, SESSION_A)
	assert_true(roster.seats_of_peer(10).is_empty(), "the departed peer drives nothing")
	assert_eq(roster.seats_of_session(SESSION_A), seats, "but its identity is holding both")
	assert_eq(roster.assign(11, 2, SESSION_A), seats,
		"and the same identity on a NEW peer id takes the same seats -- the same arena, the same team")

func test_a_held_seat_is_not_handed_to_a_newcomer() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 1)
	roster.hold(10, SESSION_A)
	assert_false(_one(roster, 11).has(seats[0]),
		"a newcomer is seated elsewhere; a held seat is taken, not free")

func test_an_unknown_identity_is_a_newcomer() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 1)
	roster.hold(10, SESSION_A)
	assert_false(roster.assign(11, 1, SESSION_B).has(seats[0]),
		"an identity nobody is holding seats for gets fresh ones, however confidently it is presented")

func test_no_session_can_never_hold_a_seat() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(10, 1)
	assert_true(roster.hold(10, SeatRoster.NO_SESSION).is_empty(),
		"several peers present NO_SESSION at once, so seats held under it would go to whichever reconnected "
		+ "first -- which is not the player who left")
	assert_eq(roster.seats_of_peer(10).size(), 1, "and the peer keeps its seat rather than losing it")

func test_an_expired_session_frees_its_seats() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var seats: PackedInt32Array = roster.assign(10, 2)
	roster.hold(10, SESSION_A)
	roster.release_session(SESSION_A)
	assert_true(roster.seats_of_session(SESSION_A).is_empty(), "the identity holds nothing")
	assert_eq(roster.reserved(), 0, "and nothing is reserved")
	assert_eq(roster.assign(11, 2, SeatRoster.NO_SESSION, false)[0], seats[0],
		"so the next arrival takes the lowest of them")

func test_releasing_a_reclaimed_peer_drops_the_hold_too() -> void:
	# The ghost case: a killed client's identity was already taken back by the returning player, and then the
	# dead socket's drop arrives.
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(10, 1)
	roster.hold(10, SESSION_A)
	roster.assign(11, 1, SESSION_A)
	roster.release(11)
	assert_true(roster.seats_of_session(SESSION_A).is_empty(),
		"no identity is left holding a seat nobody is in")
	assert_eq(roster.reserved(), 0, "and the seat is genuinely free again")

# --- balance ----------------------------------------------------------------------------------------------
func test_arenas_fill_evenly() -> void:
	var roster: SeatRoster = SeatRoster.new()
	for index: int in ArenaConfig.ARENAS:
		_one(roster, 10 + index)
	for offset: int in ArenaConfig.ARENAS:
		assert_eq(roster.taken_in_arena(ArenaConfig.FIRST_ARENA_ID + offset), 1,
			"one peer per arena before any arena takes a second")

func test_teams_fill_evenly_within_an_arena() -> void:
	var roster: SeatRoster = SeatRoster.new()
	var first: PackedInt32Array = _one(roster, 10)
	var arena: int = ArenaConfig.arena_of_seat(first[0])
	# Seat the rest of that arena and check the ends stay level as it fills.
	var placed: int = 1
	var peer: int = 11
	while placed < ArenaConfig.SEATS_PER_ARENA and peer < 200:
		var seats: PackedInt32Array = _one(roster, peer)
		peer += 1
		if not seats.is_empty() and ArenaConfig.arena_of_seat(seats[0]) == arena:
			placed += 1
	assert_eq(roster.taken_in_arena(arena), ArenaConfig.SEATS_PER_ARENA, "the arena filled")

func test_a_full_table_seats_nobody() -> void:
	var roster: SeatRoster = SeatRoster.new()
	for seat: int in ArenaConfig.SEAT_COUNT:
		_one(roster, 100 + seat)
	assert_true(roster.is_full(), "every seat is taken")
	assert_true(_one(roster, 9999).is_empty(),
		"and the next arrival is seated nowhere -- the session layer admits it as an observer instead")

func test_held_seats_count_toward_fullness() -> void:
	var roster: SeatRoster = SeatRoster.new()
	for seat: int in ArenaConfig.SEAT_COUNT:
		_one(roster, 100 + seat)
	roster.hold(100, SESSION_A)
	assert_true(roster.is_full(), "a seat waiting for its player to return is still taken")

func test_seat_owners_is_a_full_table() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(10, 2)
	var owners: PackedInt32Array = roster.seat_owners()
	assert_eq(owners.size(), ArenaConfig.SEAT_COUNT, "one entry per seat, so the broadcast is self-healing")
	assert_eq(owners[0], 10, "the seat this peer drives names it")

func test_clear_forgets_everyone() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(10, 2)
	roster.hold(10, SESSION_A)
	roster.clear()
	assert_eq(roster.occupied(), 0, "session teardown empties the table")
	assert_eq(roster.reserved(), 0, "and leaves no reclaims for the next session to honor")
