extends UnitTest
## Scene-free coverage for the seat declaration on [NetRollbackHandle].
##
## A SEAT is one owned, predicted body behind a connection. Local split-screen puts several on one socket, and
## the backend anchors interest on `(peer, seat)` rather than on the peer alone. The handle is a thin forwarder,
## so what is worth testing here is the one place it decides something: `seat()` reads the value back as a
## PROPERTY and type-checks it, because a backend that predates the export answers `null` -- which is "too old
## to carry one", not a value to convert. Getting that wrong is a crash on a mismatched binary rather than the
## 0 every other backwards-compatibility path in the facade returns.
##
## The backend synchronizer is stubbed by a plain Node carrying the same property name. The handle holds its
## synchronizer as an opaque Node and reaches it by property NAME, so a stub is a faithful stand-in and this
## suite needs no cdylib, no scene tree and no session.

## Stands in for the backend rollback synchronizer, which carries `seat` as an exported int.
class SeatedSyncStub extends Node:
	var seat: int = 0

## Stands in for a backend built before the export existed: no `seat` property at all.
class OldSyncStub extends Node:
	var membership_property: String = ""

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
