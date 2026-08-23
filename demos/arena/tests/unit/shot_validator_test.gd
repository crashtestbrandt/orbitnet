extends UnitTest
## ShotValidator: what the server checks about a shot before it resolves one.

const NOW: int = 1000

func _seated(peer: int, count: int) -> SeatRoster:
	var roster: SeatRoster = SeatRoster.new()
	roster.assign(peer, count)
	return roster

# --- the security check ------------------------------------------------------------------------------
func test_a_shot_on_a_seat_the_sender_does_not_hold_is_refused() -> void:
	# The one check in this file that is not rate limiting. A connection here may drive two fighters, so the
	# seat travels in the PAYLOAD -- which makes it the client's own claim about itself, and it has to be
	# checked against the seats the SERVER assigned to that sender.
	var roster: SeatRoster = _seated(10, 1)
	roster.assign(11, 1)
	var theirs: int = roster.seats_of_peer(11)[0]
	assert_eq(ShotValidator.check(roster, 10, theirs, true, -1, NOW), ShotValidator.Verdict.NOT_YOURS,
		"a forged shot on somebody else's fighter is refused")

func test_a_shot_on_your_own_seat_is_admitted() -> void:
	var roster: SeatRoster = _seated(10, 2)
	for seat: int in roster.seats_of_peer(10):
		assert_eq(ShotValidator.check(roster, 10, seat, true, -1, NOW), ShotValidator.Verdict.OK,
			"a connection may fire either of the fighters it drives")

func test_an_unseated_sender_owns_nothing() -> void:
	var roster: SeatRoster = _seated(10, 1)
	assert_eq(ShotValidator.check(roster, 4242, 0, true, -1, NOW), ShotValidator.Verdict.NOT_YOURS,
		"a peer that was never seated cannot fire anything")

# --- the ordinary checks ------------------------------------------------------------------------------
func test_a_seat_outside_the_pool_is_refused() -> void:
	var roster: SeatRoster = _seated(10, 1)
	assert_eq(ShotValidator.check(roster, 10, -1, true, -1, NOW), ShotValidator.Verdict.NO_SUCH_SEAT,
		"a negative seat is not a seat")
	assert_eq(ShotValidator.check(roster, 10, ArenaConfig.SEAT_COUNT, true, -1, NOW),
		ShotValidator.Verdict.NO_SUCH_SEAT, "and neither is one past the pool -- checked BEFORE the array")

func test_a_dead_fighter_cannot_fire() -> void:
	var roster: SeatRoster = _seated(10, 1)
	assert_eq(ShotValidator.check(roster, 10, 0, false, -1, NOW), ShotValidator.Verdict.NOT_ALIVE,
		"a fighter on its respawn countdown is not shooting")

func test_the_cooldown_is_enforced_on_the_server() -> void:
	var roster: SeatRoster = _seated(10, 1)
	var just_fired: int = NOW - 1
	assert_eq(ShotValidator.check(roster, 10, 0, true, just_fired, NOW), ShotValidator.Verdict.COOLING,
		"a client predicting its own rate of fire does not get to set it")
	var cooled: int = NOW - ArenaConfig.SHOT_COOLDOWN_TICKS
	assert_eq(ShotValidator.check(roster, 10, 0, true, cooled, NOW), ShotValidator.Verdict.OK,
		"exactly the cooldown later it may fire again")

func test_a_fighter_that_has_never_fired_may_fire() -> void:
	var roster: SeatRoster = _seated(10, 1)
	assert_eq(ShotValidator.check(roster, 10, 0, true, -1, NOW), ShotValidator.Verdict.OK,
		"-1 means never, not 'a very long time ago in tick 0'")

# --- the command tick ------------------------------------------------------------------------------------
func test_a_tick_from_the_future_is_clamped_to_the_present() -> void:
	# A client's clock leading the server's is ordinary, and refusing the shot would refuse it for an error
	# the server is at least half of. Clamping makes it a live present-tick cast, which is conservative.
	assert_eq(ShotValidator.clamp_command_tick(NOW + 50, NOW, 20), NOW,
		"a future tick resolves live rather than being rejected")

func test_a_tick_from_too_far_back_is_clamped_to_what_the_ring_holds() -> void:
	assert_eq(ShotValidator.clamp_command_tick(NOW - 500, NOW, 20), NOW - 20,
		"a shot is resolved SHALLOWER than asked, never against a slot holding some other tick's world")

func test_a_tick_inside_the_window_is_left_alone() -> void:
	assert_eq(ShotValidator.clamp_command_tick(NOW - 7, NOW, 20), NOW - 7,
		"inside the retained window the shot is resolved where it asked")

func test_the_window_never_reaches_before_the_session_started() -> void:
	assert_eq(ShotValidator.clamp_command_tick(0, 5, 100), 0,
		"five ticks into a session there are five ticks of history, whatever the retention says")

func test_every_verdict_has_a_description() -> void:
	for verdict: int in ShotValidator.Verdict.values():
		assert_true(ShotValidator.describe(verdict) != "unknown",
			"a refusal a server logs without a reason is a refusal nobody can debug")
