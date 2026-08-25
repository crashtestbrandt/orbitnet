extends UnitTest
## CommandThrottle: the per-sender rate limit on the reliable command channel.
##
## Testable at all because the clock is a PARAMETER. A throttle that reads Time.get_ticks_msec() internally
## can only be tested by sleeping, which makes the suite slow and flaky; passing `now` in makes every case
## below exact and instant.

func test_the_first_command_is_never_dropped() -> void:
	var throttle: CommandThrottle = CommandThrottle.new(1.0, 3)
	assert_true(throttle.allow(7, 0.0),
		"a sender starts with a FULL bucket -- an empty start would drop the first click of every session")

func test_the_burst_is_spent_then_refused() -> void:
	var throttle: CommandThrottle = CommandThrottle.new(1.0, 3)
	assert_true(throttle.allow(7, 0.0), "burst 1 of 3")
	assert_true(throttle.allow(7, 0.0), "burst 2 of 3")
	assert_true(throttle.allow(7, 0.0), "burst 3 of 3")
	assert_false(throttle.allow(7, 0.0), "the fourth in the same instant is refused")

func test_tokens_refill_at_the_configured_rate() -> void:
	var throttle: CommandThrottle = CommandThrottle.new(2.0, 1)   # 2 per second, burst 1
	assert_true(throttle.allow(7, 0.0), "the first is allowed")
	assert_false(throttle.allow(7, 0.1), "0.1 s later there is only 0.2 of a token")
	assert_true(throttle.allow(7, 0.5), "0.5 s in, a whole token has accrued")

func test_the_bucket_does_not_overfill() -> void:
	var throttle: CommandThrottle = CommandThrottle.new(10.0, 2)
	assert_true(throttle.allow(7, 0.0), "prime the sender")
	# A long idle would accrue 100 tokens without the cap, letting a returning player fire a huge burst.
	assert_true(throttle.allow(7, 10.0), "burst 1 after a long idle")
	assert_true(throttle.allow(7, 10.0), "burst 2")
	assert_false(throttle.allow(7, 10.0), "but only up to the burst size -- idle time does not accumulate")

func test_senders_are_independent() -> void:
	var throttle: CommandThrottle = CommandThrottle.new(1.0, 1)
	assert_true(throttle.allow(1, 0.0), "peer 1 spends its token")
	assert_false(throttle.allow(1, 0.0), "and is throttled")
	assert_true(throttle.allow(2, 0.0),
		"peer 2 is unaffected -- one player flooding must never rate-limit another")

func test_forget_releases_a_sender() -> void:
	var throttle: CommandThrottle = CommandThrottle.new(1.0, 1)
	throttle.allow(1, 0.0)
	throttle.allow(2, 0.0)
	assert_eq(throttle.tracked(), 2, "both senders are tracked")
	throttle.forget(1)
	assert_eq(throttle.tracked(), 1,
		"a disconnect drops the entry, so a long-lived server does not accumulate one per peer that ever joined")

func test_time_going_backward_does_not_grant_tokens() -> void:
	# Clocks are handed in by callers, and a caller can hand in a smaller value than last time (a resync, a
	# wrapped counter). Negative elapsed time must not be treated as a refill.
	var throttle: CommandThrottle = CommandThrottle.new(1.0, 1)
	assert_true(throttle.allow(7, 100.0), "spend the token at t=100")
	assert_false(throttle.allow(7, 50.0), "and time moving backward refills nothing")
