extends UnitTest
## ObserverDesk: what an observing peer declares, and when that declaration is worth resending.

func test_the_first_declaration_is_always_due() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO)
	assert_true(desk.due(0.0),
		"nothing has been sent, so the server has been told nothing and the first message is unconditional")

func test_a_point_that_barely_moved_is_not_resent() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3(ObserverDesk.RESEND_DISTANCE_M * 0.5, 0.0, 0.0))
	assert_false(desk.due(0.1),
		"half the resend distance is a pan in progress, not a new place to watch from")

func test_a_point_that_moved_far_enough_is_resent() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3(ObserverDesk.RESEND_DISTANCE_M, 0.0, 0.0))
	assert_true(desk.due(0.1), "the resend distance is the threshold, inclusive")

func test_a_stationary_observer_is_refreshed_on_the_interval() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3.ZERO)
	desk.mark_sent(0.0)
	assert_false(desk.due(ObserverDesk.RESEND_INTERVAL_S * 0.5), "not yet")
	assert_true(desk.due(ObserverDesk.RESEND_INTERVAL_S),
		"a declaration that was lost in flight is corrected by the next interval rather than never")

# --- the one that is not about distance ------------------------------------------------------------
func test_a_mode_change_is_due_however_close_the_two_are() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(4242)
	desk.mark_sent(0.0)
	desk.watch_point(Vector3.ZERO)
	assert_true(desk.due(0.0),
		"FIXED and TRACKED are different facade calls, so switching between them always crosses the wire")

func test_tracking_a_different_entity_is_due() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(1)
	desk.mark_sent(0.0)
	desk.watch_entity(2)
	assert_true(desk.due(0.0), "a new entity to follow is a new declaration")

func test_tracking_the_same_entity_is_not() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(1)
	desk.mark_sent(0.0)
	desk.watch_entity(1)
	assert_false(desk.due(0.0),
		"a tracked entity carries its own position, so following it costs no further messages")

func test_a_tracked_entity_moving_costs_nothing() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(9)
	desk.mark_sent(0.0)
	assert_false(desk.due(ObserverDesk.RESEND_INTERVAL_S * 0.5),
		"the point the desk still holds is stale and irrelevant while the mode is TRACKED")

# --- the retraction value ---------------------------------------------------------------------------
func test_entity_zero_is_refused() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3(5.0, 0.0, 5.0))
	assert_false(desk.watch_entity(0),
		"0 is the facade's RETRACTION value, so tracking it would declare a centre and withdraw it forever")
	assert_eq(desk.mode(), ObserverDesk.Mode.FIXED, "and the desk is left as it was")
	assert_eq(desk.point(), Vector3(5.0, 0.0, 5.0), "including the point it was watching")

func test_forgetting_what_was_sent_leaves_what_is_watched() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_entity(77)
	desk.mark_sent(10.0)
	desk.forget_sent()
	assert_true(desk.due(10.0), "a new session's server has been told nothing")
	assert_eq(desk.tracked_entity(), 77, "but the observer is still watching what it was watching")

func test_describe_names_both_modes() -> void:
	var desk: ObserverDesk = ObserverDesk.new()
	desk.watch_point(Vector3(12.0, 0.0, -8.0))
	assert_eq(desk.describe(), "(12, -8)", "a fixed point reads as ground coordinates")
	desk.watch_entity(31)
	assert_eq(desk.describe(), "entity 31", "and a tracked one names the entity")
