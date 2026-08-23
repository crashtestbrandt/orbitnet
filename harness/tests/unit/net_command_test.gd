extends UnitTest
## Scene-free coverage for the third lane's VERB TABLE ([NetCommand]) -- registration, dispatch, and the
## ownership refusal that is the only thing standing between a client's claim about itself and authoritative
## state.
##
## What this suite does NOT cover, deliberately: the routing in `request()`. Which of the three branches runs
## (offline / server-local / rpc_id to the server) is a property of the live session, and the two networked
## branches need a real `MultiplayerAPI`. The two-peer probe already drives them end to end -- it issues a
## real order and a forged foreign-seat order through the lane and asserts on both. What no probe pins is the
## table itself, which is what is here.
##
## The suite drives `_apply()` directly wherever a REMOTE sender matters. That is the entry point the server
## reaches from `_submit()`, with `multiplayer.get_remote_sender_id()` already resolved, and it is the only
## way to state "peer 9 sent this" without a socket. `request()` is exercised through its offline branch,
## which is live in a unit run because no session has been opened.

const OFFLINE_SENDER: int = 0   # the sentinel NetCommand hands a handler offline

const VERB_MOVE: StringName = &"move"
const VERB_UNKNOWN: StringName = &"scuttle"

# What the handlers under test record. Reset by `_fresh()` at the top of every test.
var _seen_senders: Array[int] = []
var _seen_payloads: Array[Dictionary] = []
var _announced: Array[StringName] = []
var _announced_payloads: Array[Dictionary] = []
var _applied_state: PackedStringArray = PackedStringArray()

# The server's own record of which seats it handed to which connection. A validator checks the payload's
# CLAIMED seat against this, because the seat in a payload is the client's claim about itself and the sender
# id is the only identity the transport supplies.
var _roster: Dictionary[int, int] = {}

func _fresh() -> NetCommand:
	_seen_senders.clear()
	_seen_payloads.clear()
	_announced.clear()
	_announced_payloads.clear()
	_applied_state = PackedStringArray()
	_roster.clear()
	var lane: NetCommand = NetCommand.new()
	lane.applied.connect(_on_applied)
	return lane

func _on_applied(verb: StringName, payload: Dictionary) -> void:
	_announced.push_back(verb)
	_announced_payloads.push_back(payload)

# Accepts anything, and records what it was handed.
func _accept(sender_id: int, payload: Dictionary) -> bool:
	_seen_senders.push_back(sender_id)
	_seen_payloads.push_back(payload)
	_applied_state.push_back("applied")
	return true

# Refuses everything, and mutates nothing -- the shape of a validator that found the request illegal.
func _refuse(sender_id: int, payload: Dictionary) -> bool:
	_seen_senders.push_back(sender_id)
	_seen_payloads.push_back(payload)
	return false

# The OWNERSHIP CHECK: the claimed seat must be one the SERVER assigned to this sender.
func _move_if_owned(sender_id: int, payload: Dictionary) -> bool:
	var claimed_v: Variant = payload.get("seat", null)
	# ASSIGNED to a typed local rather than cast: a payload is wire-decoded, so its fields are whatever the
	# sender put there, and a String where an int was expected must fail the check rather than convert.
	if typeof(claimed_v) != TYPE_INT:
		return false
	var claimed: int = claimed_v
	if not _roster.has(sender_id) or _roster[sender_id] != claimed:
		return false
	_applied_state.push_back("seat %d moved" % claimed)
	return true

# --- the table ------------------------------------------------------------------------------------

func test_a_registered_verb_is_validated_and_applied() -> void:
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	lane.request(VERB_MOVE, {"to": Vector3(1.0, 0.0, 2.0)})
	assert_eq(_seen_senders.size(), 1, "the registered handler ran exactly once")
	assert_eq(_applied_state.size(), 1, "and applied its change")
	assert_eq(_announced.size(), 1, "a validated command is announced")
	assert_eq(_announced[0], VERB_MOVE, "under the verb it was submitted as")
	lane.free()

func test_an_unregistered_verb_is_dropped_rather_than_erroring() -> void:
	# A client can name any verb it likes. An unknown one must be a silent no-op on the server, not a crash
	# and not an announcement other subsystems would act on.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	lane.request(VERB_UNKNOWN, {"to": Vector3.ZERO})
	assert_eq(_seen_senders.size(), 0, "no handler ran")
	assert_eq(_announced.size(), 0, "and nothing was announced")
	lane.free()

func test_an_invalid_callable_is_dropped_rather_than_erroring() -> void:
	# A verb registered with an empty Callable -- a subsystem that freed its handler's object, or wired one
	# from a method name that does not exist. `is_valid()` is what keeps that from aborting the caller.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, Callable())
	lane.request(VERB_MOVE, {})
	assert_eq(_announced.size(), 0, "an unusable handler announces nothing")
	lane.free()

func test_registering_a_verb_twice_replaces_the_handler() -> void:
	# One verb, one handler: the table is a Dictionary, so the later registration wins outright rather than
	# both running. A subsystem re-registering on respawn depends on that.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _refuse)
	lane.register(VERB_MOVE, _accept)
	lane.request(VERB_MOVE, {})
	assert_eq(_applied_state.size(), 1, "the second registration is the one that runs")
	assert_eq(_announced.size(), 1, "and its verdict is the one announced")
	lane.free()

func test_a_refused_command_mutates_nothing_and_announces_nothing() -> void:
	# `applied` is what other subsystems key off. A handler returning false must not fire it, or a refused
	# request would drive every downstream reaction anyway.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _refuse)
	lane.request(VERB_MOVE, {"to": Vector3.ONE})
	assert_eq(_seen_senders.size(), 1, "the validator still ran")
	assert_eq(_applied_state.size(), 0, "but nothing was applied")
	assert_eq(_announced.size(), 0, "and nothing was announced")
	lane.free()

func test_the_handler_runs_before_the_announcement() -> void:
	# The signal means "this is already in authoritative state", not "this is about to be". A listener that
	# reads the state it was told about must find the change there.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	var state_at_announcement: Array[int] = []
	lane.applied.connect(func(_verb: StringName, _payload: Dictionary) -> void:
		state_at_announcement.push_back(_applied_state.size()))
	lane.request(VERB_MOVE, {})
	assert_eq(state_at_announcement.size(), 1, "the listener ran once")
	assert_eq(state_at_announcement[0], 1, "the change is in place when the signal fires")
	lane.free()

func test_the_payload_reaches_the_handler_and_the_signal_verbatim() -> void:
	# Nothing in the lane inspects or rewrites a payload -- it is the subsystem's own vocabulary, and a lane
	# that normalized it would be guessing at game semantics.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	var payload: Dictionary = {"seat": 3, "to": Vector3(4.0, 0.0, 5.0), "queue": true}
	lane.request(VERB_MOVE, payload)
	assert_eq(_seen_payloads[0], payload, "the handler sees what was submitted")
	assert_eq(_announced_payloads[0], payload, "and so does the announcement")
	lane.free()

func test_offline_hands_the_handler_the_sentinel_sender() -> void:
	# Single-player is its own authority: there is no connection to name, so the ownership check has nothing
	# to check against and the handler is handed the sentinel rather than a peer id that would be a lie.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	lane.request(VERB_MOVE, {})
	assert_eq(_seen_senders[0], OFFLINE_SENDER, "offline submits arrive as the sentinel sender")
	lane.free()

# --- the ownership refusal ------------------------------------------------------------------------

func test_a_seat_the_server_assigned_to_this_sender_is_accepted() -> void:
	var lane: NetCommand = _fresh()
	_roster[7] = 2
	lane.register(VERB_MOVE, _move_if_owned)
	lane._apply(7, VERB_MOVE, {"seat": 2})
	assert_eq(_applied_state.size(), 1, "the owner's own seat moves")
	assert_eq(_announced.size(), 1, "and the move is announced")
	lane.free()

func test_a_seat_assigned_to_another_connection_is_refused() -> void:
	# THE FORGED ORDER. The seat is carried in the payload, where it is the client's claim about itself; the
	# sender id is the only identity the transport supplies. Checking the claim against the seats the SERVER
	# assigned is what makes the claim worthless to an attacker -- and skipping the check is the one new
	# mistake a per-connection command lane makes available to a game with several seats per connection.
	var lane: NetCommand = _fresh()
	_roster[7] = 2
	_roster[9] = 3
	lane.register(VERB_MOVE, _move_if_owned)
	lane._apply(9, VERB_MOVE, {"seat": 2})
	assert_eq(_applied_state.size(), 0, "peer 9 cannot move peer 7's seat")
	assert_eq(_announced.size(), 0, "and a refused order announces nothing")
	lane.free()

func test_a_sender_with_no_seat_at_all_is_refused() -> void:
	# A spectator, or a connection whose seat was released while its order was in flight. There is nothing to
	# compare the claim against, so the claim loses.
	var lane: NetCommand = _fresh()
	_roster[7] = 2
	lane.register(VERB_MOVE, _move_if_owned)
	lane._apply(11, VERB_MOVE, {"seat": 2})
	assert_eq(_applied_state.size(), 0, "an unseated sender commands nothing")
	lane.free()

func test_a_payload_with_no_seat_field_is_refused_rather_than_defaulted() -> void:
	# A missing field must not resolve to seat 0, which is the seat every body carries until something says
	# otherwise -- that would turn an omission into a command on the first seat of the roster.
	var lane: NetCommand = _fresh()
	_roster[7] = 0
	lane.register(VERB_MOVE, _move_if_owned)
	lane._apply(7, VERB_MOVE, {})
	assert_eq(_applied_state.size(), 0, "an absent seat claims nothing")
	lane.free()

func test_a_seat_field_of_the_wrong_type_is_refused() -> void:
	# A payload crosses the wire, so its fields are whatever the sender put there. A String where an int was
	# expected must fail the check rather than convert into one.
	var lane: NetCommand = _fresh()
	_roster[7] = 2
	lane.register(VERB_MOVE, _move_if_owned)
	lane._apply(7, VERB_MOVE, {"seat": "2"})
	assert_eq(_applied_state.size(), 0, "a seat that is not an int owns nothing")
	lane.free()

func test_each_sender_is_checked_against_its_own_row() -> void:
	# The refusal is per sender, not a latch: one peer's forged order must not stop the next peer's real one.
	var lane: NetCommand = _fresh()
	_roster[7] = 2
	_roster[9] = 3
	lane.register(VERB_MOVE, _move_if_owned)
	lane._apply(9, VERB_MOVE, {"seat": 2})   # forged
	lane._apply(9, VERB_MOVE, {"seat": 3})   # its own
	lane._apply(7, VERB_MOVE, {"seat": 2})   # its own
	assert_eq(_applied_state.size(), 2, "both legitimate orders ran")
	assert_eq(_announced.size(), 2, "and only those two were announced")
	lane.free()
