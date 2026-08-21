extends UnitTest
## ServeValidator: who may serve, and when.

func test_a_seated_player_may_serve_during_a_face_off() -> void:
	var result: ServeValidator.Result = ServeValidator.validate(0, false, 40)
	assert_true(result.accepted, "a countdown is running, so any seated player may start it early")
	assert_eq(result.reason, "", "and there is nothing to explain")

func test_a_seated_player_may_serve_a_stalled_puck() -> void:
	var result: ServeValidator.Result = ServeValidator.validate(3, true, 0)
	assert_true(result.accepted, "a puck stalled against a rail with nobody able to reach it can be re-served")

func test_a_live_puck_refuses_the_request() -> void:
	var result: ServeValidator.Result = ServeValidator.validate(0, false, 0)
	assert_false(result.accepted, "a live puck is not re-served on demand")
	assert_true(result.reason.length() > 0, "and the refusal says why, because a demo that hides refusals has "
		+ "not shown you the security model")

func test_an_unseated_sender_is_refused() -> void:
	# The seat is resolved from the sender id, never from the payload, so this is "the peer that asked holds no
	# seat" -- a spectator, or a peer whose disconnect is still in flight.
	for seat: int in [-1, HockeyConfig.SEATS, HockeyConfig.SEATS + 5]:
		var result: ServeValidator.Result = ServeValidator.validate(seat, true, 60)
		assert_false(result.accepted, "seat %d cannot serve" % seat)

func test_the_state_precondition_is_the_rate_limit() -> void:
	# Why this channel needs no token bucket: serving makes the puck live, and a live puck refuses. A peer
	# spamming the verb is refused by the state it just created.
	assert_true(ServeValidator.validate(0, true, 0).accepted, "the first request lands")
	assert_false(ServeValidator.validate(0, false, 0).accepted,
		"and every request after it is refused until the puck dies again")
