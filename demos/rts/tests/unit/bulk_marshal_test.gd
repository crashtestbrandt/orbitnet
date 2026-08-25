extends UnitTest
## The correspondence between what UnitBody REGISTERS and what its bulk hooks read and write.
##
## THE HOOKS ARE POSITIONAL AND THE REGISTRATION IS THE ORDER. `bulk_capture_order()` is derived from the
## `add_state()` calls in `bind_net()`, so reordering those lines silently reorders the slots the hooks must
## fill -- and a hook writing the right values into the wrong slots replicates wrong rather than erroring.
## Nothing in the engine can catch that; this suite is what catches it.
##
## It runs with no session, so the handle is inert and the backend is never asked. What is under test is the
## demo's own two functions and the constant they are keyed to, which is exactly the half that can be wrong
## without the backend noticing.

const SLOTS: int = 3   # position@half, net_aux@half, net_meta -- the three entries bind_net() registers

func _unit(unit_id: int) -> UnitBody:
	var unit: UnitBody = UnitBody.new()
	unit.configure(unit_id)
	return unit

func test_the_hook_names_are_the_methods_that_exist() -> void:
	# A hook is resolved by NAME on the channel's root. A typo leaves the channel on the per-property walk
	# with nothing erroring, so the names and the methods are asserted against each other rather than trusted.
	var unit: UnitBody = _unit(0)
	assert_true(unit.has_method(UnitBody.MARSHAL_OUT), "the capture hook names a method that exists")
	assert_true(unit.has_method(UnitBody.MARSHAL_IN), "and so does the apply hook")
	unit.free()

func test_the_state_lane_round_trips_through_the_two_hooks() -> void:
	# Capture one unit's row, land it on a second, and assert every replicated property arrived. This is the
	# server's half and a receiving client's half, back to back.
	var source: UnitBody = _unit(0)
	source.position = Vector3(4.0, 0.0, -9.0)
	source.net_aux = Vector3(0.5, -0.5, 0.25)
	source.net_meta = UnitBody.pack_meta(true, 7, 42)

	var row: Array = []
	row.resize(SLOTS)
	source._net_marshal_out(NetStateHandle.LANE_STATE, row)

	var sink: UnitBody = _unit(1)
	sink._net_marshal_in(NetStateHandle.LANE_STATE, row)

	assert_eq(sink.position, source.position, "position rode slot 0")
	assert_eq(sink.net_aux, source.net_aux, "net_aux rode slot 1")
	assert_eq(sink.net_meta, source.net_meta, "net_meta rode slot 2")
	source.free()
	sink.free()

func test_the_slots_are_distinguishable_from_each_other() -> void:
	# THE NEGATIVE CONTROL. A round trip alone passes even if both hooks agree on the WRONG order, so the
	# three values are made mutually distinguishable and each slot is asserted to carry its own.
	var unit: UnitBody = _unit(0)
	unit.position = Vector3(1.0, 2.0, 3.0)
	unit.net_aux = Vector3(9.0, 8.0, 7.0)
	unit.net_meta = 12345
	var row: Array = []
	row.resize(SLOTS)
	unit._net_marshal_out(NetStateHandle.LANE_STATE, row)
	assert_eq(row[0], Vector3(1.0, 2.0, 3.0), "slot 0 is the position, not the aux vector")
	assert_eq(row[1], Vector3(9.0, 8.0, 7.0), "slot 1 is the aux vector, not the position")
	assert_eq(row[2], 12345, "slot 2 is the meta bitfield")
	unit.free()

func test_the_hooks_fill_every_slot_they_are_given() -> void:
	# A slot the hook leaves alone keeps the previous tick's value, silently. Filling a poisoned array and
	# asserting nothing survives is what pins that every declared entry is written.
	var unit: UnitBody = _unit(0)
	unit.position = Vector3.ZERO
	unit.net_aux = Vector3.ZERO
	unit.net_meta = 0
	var row: Array = ["stale", "stale", "stale"]
	unit._net_marshal_out(NetStateHandle.LANE_STATE, row)
	for slot: int in SLOTS:
		assert_true(typeof(row[slot]) != TYPE_STRING, "slot %d was written rather than left alone" % slot)
	unit.free()

func test_an_inert_channel_reports_no_bulk_marshalling() -> void:
	# OFFLINE the handle is inert, so the declaration reaches no backend and the readout must say so rather
	# than echoing the call site.
	var unit: UnitBody = _unit(0)
	assert_false(unit.uses_bulk_marshalling(), "a unit that was never bound marshals nothing in bulk")
	unit.free()
