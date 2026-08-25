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

# --- the code the refusal travels as ---------------------------------------------------------------
#
# NetCommand reads an int verdict of 0 as acceptance and any other value as a refusal carrying that value,
# and only the int form reaches the client that asked. So `Code.OK` being 0 is a wire contract, not a
# convention -- an enum that started at 1 would announce every accepted serve as a refusal.

func test_the_accepting_code_is_zero() -> void:
	assert_eq(ServeValidator.Code.OK as int, 0,
		"NetCommand reads 0 as acceptance, so an enum starting at 1 would refuse every serve it accepted")

func test_every_refusal_carries_a_distinct_non_zero_code() -> void:
	assert_eq(ServeValidator.validate(-1, true, 60).code, ServeValidator.Code.NO_SEAT as int,
		"an unseated sender is refused under its own code")
	assert_eq(ServeValidator.validate(0, false, 0).code, ServeValidator.Code.PUCK_LIVE as int,
		"and a live puck under a different one, so a HUD can tell them apart")

func test_an_accepted_serve_carries_the_accepting_code() -> void:
	assert_eq(ServeValidator.validate(0, true, 0).code, ServeValidator.Code.OK as int,
		"acceptance is the same value NetCommand reads as applied")

func test_the_client_derives_the_same_sentence_the_server_would() -> void:
	# The code crosses the wire, the sentence does not: a client that receives PUCK_LIVE must be able to say
	# what the server would have said, or the refusal reaches the player as a bare number.
	var refused: ServeValidator.Result = ServeValidator.validate(0, false, 0)
	assert_eq(ServeValidator.describe(refused.code), refused.reason,
		"describe() of the code is the reason the validator wrote")
	assert_eq(ServeValidator.describe(ServeValidator.Code.OK), "",
		"and acceptance explains nothing")
