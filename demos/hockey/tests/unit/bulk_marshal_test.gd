extends UnitTest
## The bulk-marshalling hooks on the two rollback bodies: that they fill and read the right slots, in the
## order the registration declares, and that each lane is handled.
##
## WHAT THIS CAN AND CANNOT COVER. Offline every handle is inert, so `uses_bulk_capture()` answers false and
## there is no backend to hand the hook a real array -- that half needs a live session and belongs in a probe.
## What is pure, and is the half that actually breaks, is the CORRESPONDENCE between the declared property
## list and the slots the hook writes. Reordering a registration silently reorders the array, and a hook
## writing the right values into the wrong slots replays wrong rather than erroring.

func _slots(count: int) -> Array:
	var values: Array = []
	values.resize(count)
	return values

# --- the puck ---------------------------------------------------------------------------------------
func test_the_puck_declares_three_state_entries() -> void:
	assert_eq(PuckBody.STATE_PROPS.size(), 3,
		"position, velocity and flags -- the whole simulation state, because a restore that returned position "
		+ "without velocity would resume the resim from the wrong basis")

func test_the_puck_capture_fills_every_slot_in_order() -> void:
	var puck: PuckBody = PuckBody.new()
	puck.net_pos = Vector3(1.0, 2.0, 3.0)
	puck.net_vel = Vector3(4.0, 5.0, 6.0)
	puck.net_flags = 0x2A
	var values: Array = _slots(PuckBody.STATE_PROPS.size())
	puck._net_marshal_out(NetRollbackHandle.LANE_STATE, values)
	assert_eq(values[0], Vector3(1.0, 2.0, 3.0), "slot 0 is net_pos, matching STATE_PROPS[0]")
	assert_eq(values[1], Vector3(4.0, 5.0, 6.0), "slot 1 is net_vel")
	assert_eq(values[2], 0x2A, "slot 2 is net_flags")
	puck.free()

func test_the_puck_round_trips_through_both_hooks() -> void:
	var out: PuckBody = PuckBody.new()
	out.net_pos = Vector3(-0.4, 0.0, 0.9)
	out.net_vel = Vector3(0.1, 0.0, -2.2)
	out.net_flags = 12345
	var values: Array = _slots(PuckBody.STATE_PROPS.size())
	out._net_marshal_out(NetRollbackHandle.LANE_STATE, values)

	var back: PuckBody = PuckBody.new()
	back._net_marshal_in(NetRollbackHandle.LANE_STATE, values)
	assert_eq(back.net_pos, out.net_pos, "position survives the round trip")
	assert_eq(back.net_vel, out.net_vel, "and velocity, without which the next resim tick diverges")
	assert_eq(back.net_flags, out.net_flags, "and the flags, which carry liveness and the serve sequence")
	out.free()
	back.free()

func test_the_pucks_input_lane_writes_nothing() -> void:
	var puck: PuckBody = PuckBody.new()
	var values: Array = _slots(0)
	puck._net_marshal_out(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(values.size(), 0,
		"the puck registered an EMPTY input list, so its array is zero-length and writing to it would be the "
		+ "resize that drops the lane back to the walk")
	puck.free()

# --- the mallet -------------------------------------------------------------------------------------
func _mallet() -> MalletBody:
	var mallet: MalletBody = MalletBody.new()
	mallet.configure(0, 0)
	return mallet

func test_the_mallet_declares_two_lanes() -> void:
	assert_eq(MalletBody.STATE_PROPS.size(), 2, "pose and velocity, server-authored")
	assert_eq(MalletBody.INPUT_PROPS.size(), 1, "one target, client-authored")

func test_the_mallet_state_lane_round_trips() -> void:
	var out: MalletBody = _mallet()
	out.net_pos = Vector3(0.2, 0.0, -0.7)
	out.net_vel = Vector3(0.0, 0.0, 1.5)
	var values: Array = _slots(MalletBody.STATE_PROPS.size())
	out._net_marshal_out(NetRollbackHandle.LANE_STATE, values)

	var back: MalletBody = _mallet()
	back._net_marshal_in(NetRollbackHandle.LANE_STATE, values)
	assert_eq(back.net_pos, out.net_pos, "pose")
	assert_eq(back.net_vel, out.net_vel, "velocity")
	out.free()
	back.free()

## The half worth its own test: the input entry lives on the CHILD input node while the hook is resolved on
## the body's ROOT, so this is the one that reaches through `input` rather than reading a field of its own.
func test_the_mallet_input_lane_reaches_through_the_input_node() -> void:
	var out: MalletBody = _mallet()
	out.input.nin_target = Vector3(0.33, 0.0, -0.11)
	var values: Array = _slots(MalletBody.INPUT_PROPS.size())
	out._net_marshal_out(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(values[0], Vector3(0.33, 0.0, -0.11), "the hook read the child's property, not the root's")

	var back: MalletBody = _mallet()
	back._net_marshal_in(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(back.input.nin_target, out.input.nin_target, "and wrote it back to the child")
	out.free()
	back.free()

func test_the_two_lanes_do_not_write_each_others_slots() -> void:
	var mallet: MalletBody = _mallet()
	mallet.net_pos = Vector3.ONE
	mallet.input.nin_target = Vector3(9.0, 9.0, 9.0)
	var values: Array = _slots(MalletBody.INPUT_PROPS.size())
	mallet._net_marshal_out(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(values.size(), 1,
		"the input array is one slot; a state-lane write into it would be a resize, and a resize is the "
		+ "documented way a lane drops silently back to the per-property walk")
	assert_eq(values[0], Vector3(9.0, 9.0, 9.0), "and it holds the input value, not the pose")
	mallet.free()

# --- the offline contract ---------------------------------------------------------------------------
func test_declaring_hooks_offline_is_inert_and_does_not_crash() -> void:
	var puck: PuckBody = PuckBody.new()
	puck.set_bulk_marshalling(true)     # no handle yet: must be a no-op, not a null dereference
	assert_false(puck.uses_bulk_marshalling(),
		"an unregistered body marshals nothing, and says so rather than claiming a hook it never declared")
	puck.free()
