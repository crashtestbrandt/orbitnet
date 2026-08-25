extends UnitTest
## FighterBody's bulk-marshalling hooks: that they fill and read the right slots, in the order the
## registration declares.
##
## WHAT THIS CAN AND CANNOT COVER. Offline every handle is inert, so `uses_bulk_capture()` answers false and
## there is no backend to hand a hook a real array -- that half needs a live session and belongs in a probe.
## What is pure, and is the half that actually breaks, is the CORRESPONDENCE between the declared property
## list and the slots the hooks touch. Reordering a registration silently reorders the array, and a hook
## writing the right values into the wrong slots replays wrong rather than erroring.

func _slots(count: int) -> Array:
	var values: Array = []
	values.resize(count)
	return values

func _fighter(seat: int) -> FighterBody:
	var fighter: FighterBody = FighterBody.new()
	fighter.configure(seat, 0)
	return fighter

func test_the_fighter_is_a_fat_lane() -> void:
	# The reason this body exists. The rollback loop pays a capture and a restore walk PER REPLAYED TICK, PER
	# BODY, so the property count is the multiplier the hook divides.
	assert_true(FighterBody.STATE_PROPS.size() >= 5,
		"pose, velocity, aim, vitals and flags -- enough that one crossing per lane is worth having")
	assert_eq(FighterBody.INPUT_PROPS.size(), 3, "move, aim and buttons")

func test_the_anchor_is_the_first_state_property() -> void:
	# A rollback body's interest anchor is its FIRST Vector3 state property, so the registration ORDER is
	# load-bearing: putting velocity first would center this peer's interest on a velocity.
	assert_true(FighterBody.STATE_PROPS[0].begins_with("net_pos"),
		"net_pos is registered first, because that is what makes it the anchor")

# --- the state lane -------------------------------------------------------------------------------------
func test_the_state_capture_fills_every_slot_in_order() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.net_pos = Vector3(1.0, 0.0, 2.0)
	fighter.net_vel = Vector3(3.0, 0.0, 4.0)
	fighter.net_aim = Vector3(0.0, 0.0, 1.0)
	fighter.net_vitals = Vector3(0.5, 0.25, 0.0)
	fighter.net_flags = 0x105
	var values: Array = _slots(FighterBody.STATE_PROPS.size())
	fighter._net_marshal_out(NetRollbackHandle.LANE_STATE, values)
	assert_eq(values[0], Vector3(1.0, 0.0, 2.0), "slot 0 is net_pos, matching STATE_PROPS[0]")
	assert_eq(values[1], Vector3(3.0, 0.0, 4.0), "slot 1 is net_vel")
	assert_eq(values[2], Vector3(0.0, 0.0, 1.0), "slot 2 is net_aim")
	assert_eq(values[3], Vector3(0.5, 0.25, 0.0), "slot 3 is net_vitals")
	assert_eq(values[4], 0x105, "slot 4 is net_flags")
	fighter.free()

func test_the_state_lane_round_trips() -> void:
	var out: FighterBody = _fighter(0)
	out.net_pos = Vector3(-4.0, 0.0, 6.5)
	out.net_vel = Vector3(0.1, 0.0, -2.2)
	out.net_aim = Vector3(1.0, 0.0, 0.0)
	out.net_vitals = Vector3(0.33, 0.66, 0.99)
	out.net_flags = 12345
	var values: Array = _slots(FighterBody.STATE_PROPS.size())
	out._net_marshal_out(NetRollbackHandle.LANE_STATE, values)

	var back: FighterBody = _fighter(1)
	back._net_marshal_in(NetRollbackHandle.LANE_STATE, values)
	assert_eq(back.net_pos, out.net_pos, "position survives the round trip")
	assert_eq(back.net_vel, out.net_vel, "and velocity, without which the next resim tick diverges")
	assert_eq(back.net_aim, out.net_aim, "and aim, which is what a rewound shot was leading")
	assert_eq(back.net_vitals, out.net_vitals, "and the vitals")
	assert_eq(back.net_flags, out.net_flags, "and the flags, which carry liveness and the cloak")
	out.free()
	back.free()

# --- the input lane, which is the half that can quietly fail -----------------------------------------------
func test_the_input_lane_reaches_through_the_input_node() -> void:
	# The input entries live on the CHILD input node while the hook is resolved on the body's ROOT, so this is
	# the half that reaches through `input` rather than reading a field of its own.
	var out: FighterBody = _fighter(0)
	out.input.nin_move = Vector3(0.5, 0.0, -0.5)
	out.input.nin_aim = Vector3(0.0, 0.0, -1.0)
	out.input.nin_buttons = FighterInput.BUTTON_FIRE
	var values: Array = _slots(FighterBody.INPUT_PROPS.size())
	out._net_marshal_out(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(values[0], Vector3(0.5, 0.0, -0.5), "the hook read the child's move, not the root's")
	assert_eq(values[1], Vector3(0.0, 0.0, -1.0), "and its aim")
	assert_eq(values[2], FighterInput.BUTTON_FIRE, "and its buttons")

	var back: FighterBody = _fighter(1)
	back._net_marshal_in(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(back.input.nin_move, out.input.nin_move, "and wrote them back to the child")
	assert_true(back.input.is_firing(), "including the fire bit")
	out.free()
	back.free()

func test_the_two_lanes_do_not_write_each_others_slots() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.net_pos = Vector3.ONE
	fighter.input.nin_move = Vector3(9.0, 0.0, 9.0)
	var values: Array = _slots(FighterBody.INPUT_PROPS.size())
	fighter._net_marshal_out(NetRollbackHandle.LANE_INPUT, values)
	assert_eq(values.size(), FighterBody.INPUT_PROPS.size(),
		"a state-lane write into the input array would be a RESIZE, and a resize is the documented way a lane "
		+ "drops silently back to the per-property walk")
	assert_eq(values[0], Vector3(9.0, 0.0, 9.0), "and it holds the input value, not the pose")
	fighter.free()

# --- the offline contract ----------------------------------------------------------------------------------
func test_declaring_hooks_offline_is_inert_and_does_not_crash() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.set_bulk_marshalling(true)     # no handle yet: must be a no-op, not a null dereference
	assert_false(fighter.uses_bulk_state(),
		"an unregistered body marshals nothing, and says so rather than claiming a hook it never declared")
	assert_false(fighter.uses_bulk_input(), "on either lane")
	assert_eq(fighter.entity_id(), 0, "and it has no entity id to be vetoed or tracked by")
	fighter.free()

# --- the flags ------------------------------------------------------------------------------------------
## A queued event is drained INSIDE the tick, so every test below advances one to see the result. Offline
## `is_fresh` is always true, which is what makes this a unit test rather than a session.
func _tick(fighter: FighterBody, at: int) -> void:
	fighter.advance(ArenaConfig.NET_TICK_DT, at, true)

func test_a_queued_cloak_lands_on_the_next_tick() -> void:
	var fighter: FighterBody = _fighter(0)
	assert_true(fighter.is_alive(), "a fresh fighter is alive")
	assert_true(fighter.queue_cloak(), "it takes the cloak")
	assert_false(fighter.is_cloaked(),
		"and is NOT cloaked yet -- the write is queued, because the flag is on the rollback lane and a write "
		+ "from outside the tick is erased by the next restore")
	_tick(fighter, 100)
	assert_true(fighter.is_cloaked(), "after one tick the flag is set, and recorded at that tick")
	fighter.free()

func test_a_second_cloak_is_refused_while_one_is_queued() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.queue_cloak()
	assert_false(fighter.queue_cloak(),
		"the pickup must not be spent twice on a fighter that has not used the first one yet")
	assert_true(fighter.cloak_pending(), "and the readout can say why nothing appears to have happened")
	fighter.free()

func test_dying_drops_the_cloak() -> void:
	# A corpse that stayed cloaked would stay withheld, so the peer it was hidden from would keep drawing it
	# alive and standing exactly where it fell.
	var fighter: FighterBody = _fighter(0)
	fighter.queue_cloak()
	_tick(fighter, 100)
	fighter.queue_damage(1.0, 4)
	_tick(fighter, 101)
	assert_false(fighter.is_alive(), "one full-health hit is a kill")
	assert_false(fighter.is_cloaked(), "and a corpse is not cloaked")
	fighter.free()

func test_a_kill_is_credited_once_to_the_seat_that_scored_it() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.queue_damage(1.0, 7)
	_tick(fighter, 100)
	assert_eq(fighter.take_kill_credit(), 7, "the killer's seat comes back with the kill")
	assert_eq(fighter.take_kill_credit(), -1, "and only once, so one death scores one point")
	fighter.free()

func test_damage_short_of_a_kill_is_not_one() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.queue_damage(ArenaConfig.SHOT_DAMAGE, 4)
	_tick(fighter, 100)
	assert_true(fighter.is_alive(), "one hit is not a kill")
	assert_true(fighter.health() < 1.0, "but it hurt")
	assert_eq(fighter.take_kill_credit(), -1, "and nobody scored")
	fighter.free()

func test_a_dead_fighter_takes_no_further_damage() -> void:
	var fighter: FighterBody = _fighter(0)
	fighter.queue_damage(1.0, 4)
	_tick(fighter, 100)
	assert_eq(fighter.take_kill_credit(), 4, "the first hit killed it, and that kill is credited")
	fighter.queue_damage(1.0, 5)
	_tick(fighter, 101)
	assert_eq(fighter.take_kill_credit(), -1, "a second kill on one corpse would score twice")
	fighter.free()

func test_the_shot_sequence_moves_so_a_client_can_see_a_shot() -> void:
	var fighter: FighterBody = _fighter(0)
	var before: int = fighter.shot_sequence()
	fighter.queue_shot(100)
	_tick(fighter, 100)
	assert_true(fighter.shot_sequence() != before,
		"the sequence rides in the flags, so a tracer needs no channel of its own")
	fighter.free()

func test_a_queued_shot_already_counts_against_the_cooldown() -> void:
	# Without the pending half, two shots inside one tick would both pass the cooldown, because neither had
	# been recorded yet.
	var fighter: FighterBody = _fighter(0)
	assert_eq(fighter.last_shot_tick(), -1, "it has never fired")
	fighter.queue_shot(100)
	assert_eq(fighter.last_shot_tick(), 100, "a queued shot is already the last shot for the cooldown")
	fighter.free()
