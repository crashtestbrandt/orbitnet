extends UnitTest
## ObserverDesk: what an observing peer declares, which arena it declares it into, and when that is worth
## resending.

const ARENA_A: int = ArenaConfig.FIRST_ARENA_ID
const ARENA_B: int = ArenaConfig.FIRST_ARENA_ID + 1

func test_the_first_declaration_is_always_due() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO, ARENA_A)
	assert_true(desk.due(0.0),
		"nothing has been sent, so the server has been told nothing and the first message is unconditional")

func test_a_point_that_barely_moved_is_not_resent() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO, ARENA_A)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3(ObserverDesk.RESEND_DISTANCE_M * 0.5, 0.0, 0.0), ARENA_A)
	assert_false(desk.due(0.1), "half the resend distance is a pan in progress, not a new place to watch from")

func test_a_point_that_moved_far_enough_is_resent() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO, ARENA_A)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3(ObserverDesk.RESEND_DISTANCE_M, 0.0, 0.0), ARENA_A)
	assert_true(desk.due(0.1), "the resend distance is the threshold, inclusive")

func test_a_stationary_observer_is_refreshed_on_the_interval() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO, ARENA_A)
	desk.mark_sent(0.0)
	assert_false(desk.due(ObserverDesk.RESEND_INTERVAL_S * 0.5), "not yet")
	assert_true(desk.due(ObserverDesk.RESEND_INTERVAL_S),
		"a declaration lost in flight is corrected by the next interval rather than never")

# --- the arena axis, which is the half that cannot be inferred -----------------------------------------
func test_changing_arena_is_due_even_without_moving() -> void:
	# An observer that walked its centre from one arena to the SAME LOCAL POINT in the next moved zero metres
	# and changed everything it can see. A distance threshold alone would never send it.
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO, ARENA_A)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3.ZERO, ARENA_B)
	assert_true(desk.due(0.0), "a different world is a different declaration, at zero metres")

func test_the_arena_is_carried_on_both_modes() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3(1.0, 0.0, 1.0), ARENA_B)
	assert_eq(desk.arena(), ARENA_B, "a fixed point names its arena")
	desk.watch_entity(77, ARENA_A)
	assert_eq(desk.arena(), ARENA_A, "and so does a tracked entity")

# --- the two facade calls --------------------------------------------------------------------------------
func test_a_mode_change_is_due_however_close_the_two_are() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(4242, ARENA_A)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3.ZERO, ARENA_A)
	assert_true(desk.due(0.0),
		"set_peer_anchor and set_peer_anchor_entity are different calls, so switching always crosses the wire")

func test_tracking_a_different_entity_is_due() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(1, ARENA_A)
	desk.mark_sent(0.0)
	desk.watch_entity(2, ARENA_A)
	assert_true(desk.due(0.0), "a new entity to follow is a new declaration")

func test_a_tracked_entity_moving_costs_nothing() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(9, ARENA_A)
	desk.mark_sent(0.0)
	assert_false(desk.due(ObserverDesk.RESEND_INTERVAL_S * 0.5),
		"a tracked entity carries its own position, so following it costs one message however far it runs")

func test_entity_zero_is_refused() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3(5.0, 0.0, 5.0), ARENA_B)
	assert_false(desk.watch_entity(0, ARENA_A),
		"0 is the facade's RETRACTION value, so tracking it would declare a centre and withdraw it forever")
	assert_eq(desk.mode(), ObserverDesk.Mode.FIXED, "and the desk is left as it was")
	assert_eq(desk.arena(), ARENA_B, "including the arena it was watching")

func test_forgetting_what_was_sent_leaves_what_is_watched() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(77, ARENA_B)
	desk.mark_sent(10.0)
	desk.forget_sent()
	assert_true(desk.due(10.0), "a new session's server has been told nothing")
	assert_eq(desk.tracked_entity(), 77, "but the observer is still watching what it was watching")
	assert_eq(desk.arena(), ARENA_B, "in the arena it was watching it in")

func test_describe_names_both_modes() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3(12.0, 0.0, -8.0), ARENA_A)
	assert_eq(desk.describe(), "(12, -8) in arena %d" % ARENA_A, "a fixed point reads as local coordinates")
	desk.watch_entity(31, ARENA_B)
	assert_eq(desk.describe(), "entity 31 in arena %d" % ARENA_B, "and a tracked one names the entity")
