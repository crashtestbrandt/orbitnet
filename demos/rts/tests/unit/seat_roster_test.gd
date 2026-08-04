extends UnitTest
## SeatRoster: who sits where, and -- the part that matters -- how a command's sender is resolved to a seat.

func test_seats_are_handed_out_lowest_first() -> void:
	var roster: SeatRoster = SeatRoster.new()
	assert_eq(roster.assign(1), 0, "the first peer takes seat 0")
	assert_eq(roster.assign(2), 1, "the second takes seat 1")

func test_reassignment_is_idempotent() -> void:
	var roster: SeatRoster = SeatRoster.new()
	assert_eq(roster.assign(7), 0, "a peer is seated")
	assert_eq(roster.assign(7), 0, "and asking again returns the same seat, it does not consume a second one")
	assert_eq(roster.occupied(), 1, "so only one seat is taken")

func test_a_full_table_refuses() -> void:
	var roster: SeatRoster = SeatRoster.new()
	for index: int in RtsConfig.SEATS:
		roster.assign(index + 1)
	assert_true(roster.is_full(), "every seat is taken")
	assert_eq(roster.assign(999), -1,
		"an extra peer is refused rather than sharing an army with someone")

func test_release_frees_the_seat_for_a_reconnect() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(1)
	roster.assign(2)
	roster.release(1)
	assert_eq(roster.seat_of_peer(1), -1, "a released peer holds no seat")
	assert_eq(roster.assign(3), 0, "and its seat is handed to the next arrival")

func test_peer_and_seat_lookups_agree() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(11)
	assert_eq(roster.seat_of_peer(11), 0, "peer -> seat")
	assert_eq(roster.peer_of_seat(0), 11, "seat -> peer")
	assert_eq(roster.peer_of_seat(RtsConfig.SEATS), -1, "an out-of-range seat holds nobody")

# --- the security-relevant one ---------------------------------------------------------------------
func test_an_unknown_sender_holds_no_seat() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(1)
	assert_eq(roster.seat_for_sender(4242), -1,
		"a peer that was never seated resolves to no seat, so every order it sends is refused")

func test_the_offline_sender_resolves_to_seat_zero() -> void:
	# The offline path applies commands locally with NetCommand's sentinel sender id 0. Resolving that through
	# the table would return -1 and every single-player order would be rejected as unseated -- and the offline
	# path is the one people develop against, so the bug would be found late and blamed on the netcode.
	var roster: SeatRoster = SeatRoster.new()
	assert_eq(roster.seat_for_sender(SeatRoster.OFFLINE_SENDER), 0,
		"offline is its own authority and always holds seat 0")
	assert_eq(roster.occupied(), 0, "without occupying a seat in the table")

func test_clear_empties_the_table() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(1)
	roster.assign(2)
	roster.clear()
	assert_eq(roster.occupied(), 0, "teardown releases everyone")
	assert_eq(roster.seat_of_peer(1), -1, "and forgets their seats")
