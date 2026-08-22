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

# --- reconnection: a seat is owned by a SESSION, occupied by a PEER --------------------------------
func test_a_rejoiner_reclaims_its_own_seat_under_a_new_peer_id() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(2, 0xaaa)
	roster.assign(3, 0xbbb)
	assert_eq(roster.seat_of_peer(2), 0, "the first player took seat 0")
	# Peer 2 drops and dials back in under a new transport id. Nothing released the seat: the session layer
	# holds it for the grace window.
	assert_eq(roster.assign(9, 0xaaa), 0, "the same session gets its own seat back, not the free one")
	assert_eq(roster.seat_of_peer(9), 0, "under the new peer id")
	assert_eq(roster.seat_of_peer(2), -1, "and the id it dropped under resolves to nothing")
	assert_eq(roster.peer_of_seat(1), 3, "the other player is untouched")

func test_a_held_seat_is_not_handed_to_a_newcomer() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(2, 0xaaa)
	# The drop is not a release -- the seat reads as taken for the whole grace window.
	assert_eq(roster.assign(4, 0xccc), 1,
		"a newcomer takes the next seat while seat 0 is being held")
	assert_eq(roster.seat_of_session(0xaaa), 0, "and the held seat still belongs to its session")

func test_an_expired_session_frees_its_seat_and_its_stale_peer_binding() -> void:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(2, 0xaaa)
	roster.release_session(0xaaa)
	assert_eq(roster.seat_of_session(0xaaa), -1, "the session owns nothing")
	assert_eq(roster.seat_of_peer(2), -1, "the peer it dropped under is unbound too")
	assert_eq(roster.assign(5, 0xddd), 0, "and the seat is handed to the next arrival")

func test_a_peer_claiming_no_identity_is_never_reclaimable() -> void:
	# The local host presents no identity, and neither does a peer on a backend too old to send one. Such a
	# seat must not be reclaimable, or the FIRST such peer's seat would be handed to the next one.
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(SeatRoster.SERVER_PEER, SeatRoster.NO_SESSION)
	assert_eq(roster.session_of_seat(0), SeatRoster.NO_SESSION, "no session owns the seat")
	assert_eq(roster.seat_of_session(SeatRoster.NO_SESSION), -1,
		"and identity 0 is not a key anything can be looked up by")
	assert_eq(roster.assign(6, SeatRoster.NO_SESSION), 1, "the next anonymous peer takes its own seat")

func test_releasing_a_peer_gives_up_its_session_ownership_too() -> void:
	# release() is the outright leave -- a drop the session layer is NOT holding open. Leaving the session
	# ownership behind would keep the seat reserved forever with nobody able to claim it.
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(2, 0xaaa)
	roster.release(2)
	assert_eq(roster.seat_of_session(0xaaa), -1, "the session no longer owns a seat")
	assert_eq(roster.assign(7, 0xeee), 0, "which the next arrival takes")

func test_reassignment_is_still_idempotent_for_a_seated_session() -> void:
	var roster: SeatRoster = SeatRoster.new()
	assert_eq(roster.assign(4, 0xaaa), 0, "seated")
	assert_eq(roster.assign(4, 0xaaa), 0, "and asking again returns the same seat")
	assert_eq(roster.occupied(), 1, "without consuming a second one")
