extends UnitTest
## The facade's OFFLINE contract, for every call this demo adds.
##
## THE CONTRACT IS LOAD-BEARING, NOT A CONVENIENCE. At boot the facade is OFFLINE and every method no-ops,
## returning inert handles -- which is what lets a single-player launch run the exact same code path with no
## networking spun up at all. "Does it work offline" is then not a separate mode to maintain, and this suite
## is what keeps that true for the calls this demo introduced.

func test_the_facade_starts_offline() -> void:
	assert_true(Net.is_offline(), "the unit runner spins up no session, so every call below is the offline one")

func test_the_interest_declarations_no_op_offline() -> void:
	# All of these are SERVER-SIDE calls. Offline there is no server and no peer, and each must be a no-op
	# rather than an error -- a demo that guarded every one of them at its call site would be a demo whose
	# offline path is a different path.
	Net.set_peer_anchor(2, Vector3.ZERO, ArenaConfig.FIRST_ARENA_ID)
	Net.set_peer_anchor_entity(2, 12345, ArenaConfig.FIRST_ARENA_ID)
	Net.clear_peer_anchor(2)
	assert_eq(Net.peer_membership(2), 0, "no declaration survives, because none was made")

func test_the_veto_no_ops_offline() -> void:
	Net.set_entity_hidden(2, 12345, true)
	assert_false(Net.is_entity_hidden(2, 12345),
		"a veto offline is a veto on a peer that does not exist, and it must not be remembered")
	Net.set_entity_hidden(2, 12345, false)

func test_a_rollback_handle_is_inert_offline() -> void:
	var body: FighterBody = FighterBody.new()
	body.configure(0, 0)
	body.bind_net()
	assert_eq(body.entity_id(), 0, "an inert handle has no entity id")
	assert_false(body.uses_bulk_state(), "and no bulk hook, however it was declared")
	assert_eq(body.last_known_state(), -1, "and no authoritative row has ever arrived")
	body.free()

func test_a_state_handle_is_inert_offline() -> void:
	var card: Scorecard = Scorecard.new()
	card.configure(ArenaConfig.FIRST_ARENA_ID)
	card.bind_net()
	assert_eq(card.entity_id(), 0, "an inert channel has no entity id")
	card.credit(0, 3)
	assert_eq(card.teams()[0], 1,
		"and the property still takes the value -- offline the properties simply stick where they are written")
	card.free()

func test_the_scorecard_packs_two_teams_into_one_value() -> void:
	var card: Scorecard = Scorecard.new()
	card.configure(ArenaConfig.FIRST_ARENA_ID)
	card.credit(0, 1)
	card.credit(0, 2)
	card.credit(1, 3)
	assert_eq(card.teams()[0], 2, "team 0 scored twice")
	assert_eq(card.teams()[1], 1, "team 1 once")
	assert_eq(card.last_kill_seat(), 3, "and the last kill names its seat")
	assert_true(card.kill_sequence() > 0, "with a sequence a client can watch for a change")
	card.free()

func test_crediting_a_team_that_does_not_exist_changes_nothing() -> void:
	var card: Scorecard = Scorecard.new()
	card.configure(ArenaConfig.FIRST_ARENA_ID)
	card.credit(7, 1)
	assert_eq(card.teams()[0], 0, "an out-of-range team scores nothing")
	assert_eq(card.teams()[1], 0, "for either side")
	card.free()

func test_the_session_identity_survives_being_set() -> void:
	var original: int = Net.session_id()
	Net.set_session_id(0x1234ABCD)
	assert_eq(Net.session_id(), 0x1234ABCD, "the identity a rejoiner presents is settable before a handshake")
	Net.set_session_id(original)
