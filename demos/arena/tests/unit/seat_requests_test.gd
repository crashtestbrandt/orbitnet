extends UnitTest
## SeatRequests: a connection is seated once, on the second of its join and its seat request, with what it
## asked for.

const PEER: int = 1234
const SESSION: int = 0x5EA7

# --- the two orders --------------------------------------------------------------------------------
func test_a_request_before_the_join_seats_on_the_join() -> void:
	var requests: SeatRequests = SeatRequests.new()
	assert_false(requests.note_request(PEER, 2, true), "a request alone is not enough: no identity yet")
	assert_true(requests.note_joined(PEER, SESSION, 0), "the join completes it")
	assert_eq(requests.count(PEER), 2, "with the count it asked for")
	assert_true(requests.spread(PEER), "and its spread")
	assert_eq(requests.session_id(PEER), SESSION, "and the identity it joined with")

func test_a_join_before_the_request_seats_on_the_request() -> void:
	var requests: SeatRequests = SeatRequests.new()
	assert_false(requests.note_joined(PEER, SESSION, 0), "a join alone waits for the request")
	assert_true(requests.note_request(PEER, 1, false), "the request completes it")
	assert_eq(requests.count(PEER), 1, "one seat, as asked")

# --- what a request may ask for --------------------------------------------------------------------
func test_a_request_is_bounded_by_the_configured_maximum() -> void:
	var requests: SeatRequests = SeatRequests.new()
	requests.note_joined(PEER, SESSION, 0)
	requests.note_request(PEER, 99, false)
	assert_eq(requests.count(PEER), ArenaConfig.MAX_SEATS_PER_PEER, "ninety-nine is clamped to the maximum")

func test_a_request_for_none_still_gets_one() -> void:
	var requests: SeatRequests = SeatRequests.new()
	requests.note_joined(PEER, SESSION, 0)
	requests.note_request(PEER, 0, false)
	assert_eq(requests.count(PEER), 1, "a connection that joined to play gets at least one seat")

# --- once ------------------------------------------------------------------------------------------
func test_nothing_seats_a_connection_twice() -> void:
	var requests: SeatRequests = SeatRequests.new()
	requests.note_joined(PEER, SESSION, 0)
	assert_true(requests.note_request(PEER, 1, false), "seated on the request")
	requests.mark_seated(PEER)
	assert_false(requests.note_request(PEER, 2, true), "a second request after seating changes nothing")
	assert_eq(requests.count(PEER), 1, "and the count stays what it was seated with")
	assert_false(requests.take_default(PEER), "nor does the fallback")
	assert_false(requests.note_joined(PEER, SESSION, 0), "nor a repeated join")

# --- the fallback ----------------------------------------------------------------------------------
func test_a_joined_client_that_never_asks_gets_one_seat() -> void:
	var requests: SeatRequests = SeatRequests.new()
	requests.note_joined(PEER, SESSION, 0)
	assert_true(requests.take_default(PEER), "the fallback seats it")
	assert_eq(requests.count(PEER), SeatRequests.DEFAULT_SEATS, "with the default")
	assert_false(requests.spread(PEER), "in one arena")

func test_the_fallback_does_not_override_a_request() -> void:
	var requests: SeatRequests = SeatRequests.new()
	requests.note_request(PEER, 2, true)
	requests.note_joined(PEER, SESSION, 0)
	assert_false(requests.take_default(PEER), "a peer that asked is seated with what it asked for")
	assert_eq(requests.count(PEER), 2, "two")

func test_the_fallback_ignores_a_peer_that_never_joined() -> void:
	var requests: SeatRequests = SeatRequests.new()
	assert_false(requests.take_default(PEER), "no identity, nothing to seat")

# --- leaving -------------------------------------------------------------------------------------
func test_a_departed_peer_is_forgotten() -> void:
	var requests: SeatRequests = SeatRequests.new()
	requests.note_request(PEER, 2, true)
	requests.note_joined(PEER, SESSION, 0)
	requests.mark_seated(PEER)
	requests.forget(PEER)
	assert_false(requests.is_seated(PEER), "a reused peer id starts unseated")
	assert_eq(requests.count(PEER), SeatRequests.DEFAULT_SEATS, "with no request on file")
	assert_false(requests.take_default(PEER), "and the old join is gone with it")
