extends UnitTest
## Scene-free coverage for the seat declaration and the seat VERBS on [NetRollbackHandle], and for the
## SEAT-RELEASE POLICY on the [code]Net[/code] facade.
##
## A SEAT is one owned, predicted body behind a connection. Local split-screen puts several on one socket, and
## the backend anchors interest on `(peer, seat)` rather than on the peer alone. The handle is a thin forwarder,
## so what is worth testing here is the three places it decides something:
##
## - `seat()` reads the value back as a PROPERTY and type-checks it, because a backend that predates the export
##   answers `null` -- which is "too old to carry one", not a value to convert. Getting that wrong is a crash on
##   a mismatched binary rather than the 0 every other backwards-compatibility path in the facade returns.
## - `assign_seat()` and `release_seat()` write the owning connection and the label as ONE statement. The roster
##   is derived from the pair, so two separate writes are announced as a seat opening and closing again.
## - The fallback for a backend without the verbs writes the LABEL FIRST, which is the order that cannot leave
##   the body reading as `(new peer, old label)` -- a seat nobody assigned.
##
## The backend synchronizer is stubbed by a plain Node carrying the same property name. The handle holds its
## synchronizer as an opaque Node and reaches it by property NAME, so a stub is a faithful stand-in and this
## suite needs no cdylib, no scene tree and no session.
##
## The release policy is exercised through the facade itself rather than through a stub, because what is worth
## pinning there is not a forward but a DEFAULT and a CLAMP:
##
## - `Net.SeatRelease.HOLD` is the default, and it must stay the default: it is what a pinned released binary
##   does, what the reconnect grace window depends on, and what three signal contracts already promise.
## - The enum NUMBERS are part of the contract. The facade writes `int(policy)` into a backend property, so the
##   two enums must agree member for member or a stored policy means something else.
## - A number outside the enum clamps to HOLD, which is the direction that takes nobody's body away.
## - The release helpers ANSWER 0 rather than erroring when the loaded binary predates them. That is not
##   hypothetical here: this suite runs against the committed cdylib, which is refreshed only at a release tag.

## Stands in for the backend rollback synchronizer, which carries `seat` as an exported int.
class SeatedSyncStub extends Node:
	var seat: int = 0

## Stands in for a backend built before the export existed: no `seat` property at all.
class OldSyncStub extends Node:
	var membership_property: String = ""

## Stands in for a backend carrying the seat VERBS: the one-call add and remove. Records the order the two
## halves land in, which is the whole point of the verbs existing.
class SeatVerbSyncStub extends Node:
	var seat: int = 0
	var authority: int = 1
	var calls: PackedStringArray = PackedStringArray()

	func assign_seat(peer: int, index: int) -> void:
		seat = index
		authority = peer
		calls.push_back("assign %d/%d" % [peer, index])

	func release_seat() -> void:
		seat = 0
		authority = 1
		calls.push_back("release")

## Stands in for a backend that carries `seat` and `set_input_authority` but not the seat verbs -- the
## checkout that pairs new GDScript with an older binary. The handle must reach the same end state through
## the two writes it already had, in the order that cannot invent a seat.
class NoVerbSyncStub extends Node:
	var seat: int = 0
	var authority: int = 1
	## The label as it stood when the authority was written. The fallback writes the label FIRST, so a
	## correct fallback records the NEW label here and a reversed one records the old.
	var seat_when_authority_changed: int = -1

	func set_input_authority(peer: int) -> void:
		authority = peer
		seat_when_authority_changed = seat

func test_seat_reaches_the_synchronizer_verbatim() -> void:
	var stub: SeatedSyncStub = SeatedSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_eq(handle.seat(), 0, "every body starts on seat 0 -- one seat per connection")
	handle.set_seat(2)
	assert_eq(stub.seat, 2, "the label reaches the synchronizer unmodified")
	assert_eq(handle.seat(), 2, "and reads back")
	stub.free()

func test_an_old_backend_reports_seat_zero_rather_than_failing() -> void:
	var stub: OldSyncStub = OldSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_eq(handle.seat(), 0, "no seat property is the same answer as the default seat")
	handle.set_seat(3)
	assert_eq(handle.seat(), 0, "and writing one does not invent it")
	stub.free()

func test_an_inert_handle_is_seated_at_zero() -> void:
	# OFFLINE: the handle wraps no synchronizer, and every method no-ops so the game wires the same code path.
	var handle: NetRollbackHandle = NetRollbackHandle.new(null)
	handle.set_seat(5)
	assert_eq(handle.seat(), 0, "an inert handle answers the default")
	handle.assign_seat(4, 2)
	handle.release_seat()
	assert_eq(handle.seat(), 0, "and the seat verbs no-op on it too")

func test_assign_seat_writes_the_connection_and_the_label_in_one_call() -> void:
	# The whole reason the verb exists: the roster is derived from `(input owner, seat)`, so two separate
	# writes are observable as an intermediate seat nobody assigned.
	var stub: SeatVerbSyncStub = SeatVerbSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.assign_seat(4, 2)
	assert_eq(stub.authority, 4, "the connection reached the synchronizer")
	assert_eq(stub.seat, 2, "and so did the label")
	assert_eq(stub.calls.size(), 1, "as ONE call, not two")
	stub.free()

func test_release_seat_empties_the_seat_without_unregistering() -> void:
	var stub: SeatVerbSyncStub = SeatVerbSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.assign_seat(4, 2)
	handle.release_seat()
	assert_eq(stub.authority, 1, "input goes back to the server")
	assert_eq(stub.seat, 0, "and the label goes back to the default")
	assert_eq(stub.calls[1], "release", "one verb, not a re-assignment to peer 1")
	stub.free()

func test_an_old_backend_reaches_the_same_seat_through_the_two_writes() -> void:
	# A checkout pairing new GDScript with an older binary re-seats the body rather than silently not.
	var stub: NoVerbSyncStub = NoVerbSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.assign_seat(4, 2)
	assert_eq(stub.authority, 4, "the connection is written")
	assert_eq(stub.seat, 2, "and the label")
	assert_eq(stub.seat_when_authority_changed, 2,
		"label first: the body is never briefly (new peer, old label), which is a seat nobody assigned")

	handle.release_seat()
	assert_eq(stub.authority, 1, "input goes back to the server")
	assert_eq(stub.seat, 0, "and the label back to the default")
	assert_eq(stub.seat_when_authority_changed, 0, "and the release clears the label first for the same reason")
	stub.free()

# --- the seat-release policy on the facade ---------------------------------------------------------

func test_the_seat_release_enum_matches_the_backend_numbering() -> void:
	# The facade writes `int(policy)` into a backend property and reads the same number back, so the two enums
	# are one contract. Drift here fails nothing loudly -- it silently renames the chosen policy.
	assert_eq(int(Net.SeatRelease.HOLD), 0, "HOLD is 0")
	assert_eq(int(Net.SeatRelease.RELEASE_ON_EXPIRY), 1, "RELEASE_ON_EXPIRY is 1")
	assert_eq(int(Net.SeatRelease.RELEASE_ON_DROP), 2, "RELEASE_ON_DROP is 2")

func test_a_session_that_chose_nothing_holds_its_seats() -> void:
	# THE DEFAULT-DRIFT GUARD. A releasing default would despawn a player's viewpoint on a transient drop, which
	# is what the reconnect grace window exists to prevent, and it would falsify the contract stated on
	# `peer_session_expired`, on `seat_closed` and on `set_reconnect_grace`.
	assert_eq(Net.seat_release_policy(), Net.SeatRelease.HOLD, "no policy chosen means nothing is released")

func test_a_policy_outside_the_enum_clamps_to_hold() -> void:
	# A stored number this build does not know must release NOTHING rather than select whichever member happens
	# to sit at that index. Clamped on write and again on read, so the property reads back what is in force.
	for junk: int in [3, 99, -1, -7]:
		Net.set_seat_release_policy(junk)
		assert_eq(Net.seat_release_policy(), Net.SeatRelease.HOLD, "policy %d is not a policy" % junk)

func test_releasing_a_peers_seats_answers_zero_rather_than_erroring() -> void:
	# The backwards-compatibility path, and it is the one this suite actually runs: the committed cdylib is
	# refreshed only at a release tag, so a binary that predates these calls is the ordinary checkout. A
	# teardown call has no business erroring there -- it answers "nothing changed" and the game runs. The same
	# answer covers a session that is OFFLINE, and a connection that drives nothing.
	assert_eq(Net.release_peer_seats(4), 0, "no seated body means no entity changed")
	assert_eq(Net.release_seat(4, 0), 0, "and the same for one named seat")
	assert_eq(Net.release_seat(4, 1 << 20), 0, "a label outside the range releases nothing at all")

func test_a_second_release_is_idempotent_and_announces_nothing() -> void:
	# Releasing a connection that holds no seat is neither an error nor an event. A `seat_closed` with no
	# matching open is worse than nothing for a presentation layer -- it tears down a viewport that was never
	# built -- so a release announces only what it actually changed.
	var closed: Array[Vector2i] = []
	var watcher: Callable = func(peer: int, seat: int) -> void:
		closed.push_back(Vector2i(peer, seat))
	Net.seat_closed.connect(watcher)
	var first: int = Net.release_peer_seats(4)
	var second: int = Net.release_peer_seats(4)
	Net.seat_closed.disconnect(watcher)
	assert_eq(first, 0, "nothing was seated on that connection")
	assert_eq(second, 0, "and the second release changes nothing either")
	assert_eq(closed.size(), 0, "a release that changed nothing closes no seat")
