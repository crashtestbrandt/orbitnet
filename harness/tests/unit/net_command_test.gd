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

# A game's own reason codes, in the shape the lane expects: an enum whose OK member is 0, counting up. The
# lane's own refusals are negative, so the two ranges cannot collide.
const REASON_COOLING: int = 1
const REASON_ODD: int = 2

# What the handlers under test record. Reset by `_fresh()` at the top of every test.
var _seen_senders: Array[int] = []
var _seen_payloads: Array[Dictionary] = []
var _announced: Array[StringName] = []
var _announced_payloads: Array[Dictionary] = []
var _applied_state: PackedStringArray = PackedStringArray()
var _rejected_verbs: Array[StringName] = []
var _rejected_codes: Array[int] = []
var _rejected_tags: Array[int] = []

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
	_rejected_verbs.clear()
	_rejected_codes.clear()
	_rejected_tags.clear()
	_roster.clear()
	var lane: NetCommand = NetCommand.new()
	lane.applied.connect(_on_applied)
	lane.rejected.connect(_on_rejected)
	return lane

func _on_applied(verb: StringName, payload: Dictionary) -> void:
	_announced.push_back(verb)
	_announced_payloads.push_back(payload)

func _on_rejected(verb: StringName, code: int, tag: int) -> void:
	_rejected_verbs.push_back(verb)
	_rejected_codes.push_back(code)
	_rejected_tags.push_back(tag)

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

# Accepts, and says so as an INT rather than a bool -- the shape a game with reason codes uses.
func _accept_with_code(sender_id: int, payload: Dictionary) -> int:
	_seen_senders.push_back(sender_id)
	_seen_payloads.push_back(payload)
	_applied_state.push_back("applied")
	return NetCommand.CODE_OK

# Refuses with a reason the requester is meant to read.
func _refuse_with_code(sender_id: int, payload: Dictionary) -> int:
	_seen_senders.push_back(sender_id)
	_seen_payloads.push_back(payload)
	return REASON_COOLING

# Accepts an even "n" and refuses an odd one, so one batch carries both outcomes.
func _accept_even_n(sender_id: int, payload: Dictionary) -> int:
	_seen_senders.push_back(sender_id)
	_seen_payloads.push_back(payload)
	var n_v: Variant = payload.get("n", 1)
	if typeof(n_v) != TYPE_INT:
		return REASON_ODD
	var n: int = n_v
	if n % 2 != 0:
		return REASON_ODD
	_applied_state.push_back("applied %d" % n)
	return NetCommand.CODE_OK

# A validator that states no verdict at all -- declared `-> void`, or one that fell off a branch.
func _verdictless(sender_id: int, payload: Dictionary) -> void:
	_seen_senders.push_back(sender_id)
	_seen_payloads.push_back(payload)

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

# --- the verdict table ----------------------------------------------------------------------------
#
# The handler's return value decides the outcome AND whether the requester is told. `bool` keeps exactly the
# meaning it had before `rejected` existed, which is what makes the signal additive rather than a migration;
# `int` opts into feedback and carries the game's own reason. The two are separated by `typeof`, never by
# truthiness, because a non-zero int is truthy and means the opposite of `true` here.

func test_a_bool_false_refuses_silently_and_reports_no_code() -> void:
	# TODAY'S BEHAVIOUR, PINNED. Every validator in this repository and in a consuming game is declared
	# `-> bool`, so this is the row that must not move.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _refuse)
	var code: int = lane._apply(5, VERB_MOVE, {}, 42)
	assert_eq(code, NetCommand.CODE_OK, "a bool refusal carries nothing to reply with")
	assert_eq(_rejected_codes.size(), 0, "and announces no refusal")
	assert_eq(_announced.size(), 0, "and applies nothing")
	lane.free()

func test_an_int_verdict_of_zero_applies() -> void:
	# CODE_OK is 0 because a game's reason enum has `OK = 0`. Reading the int as truthiness would invert this
	# row and the next one at once.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept_with_code)
	var code: int = lane._apply(5, VERB_MOVE, {}, 42)
	assert_eq(code, NetCommand.CODE_OK, "zero is acceptance")
	assert_eq(_announced.size(), 1, "so the command is announced as applied")
	assert_eq(_rejected_codes.size(), 0, "and no refusal is announced")
	lane.free()

func test_a_nonzero_int_verdict_refuses_with_that_code() -> void:
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _refuse_with_code)
	var code: int = lane._apply(5, VERB_MOVE, {}, 42)
	assert_eq(code, REASON_COOLING, "the validator's own reason comes back verbatim")
	assert_eq(_announced.size(), 0, "nothing was applied")
	assert_eq(_rejected_codes.size(), 1, "and the refusal is announced on the applying peer")
	assert_eq(_rejected_codes[0], REASON_COOLING, "carrying the reason")
	assert_eq(_rejected_tags[0], 42, "and the tag that names the request")
	lane.free()

func test_a_handler_returning_nothing_refuses_rather_than_applying() -> void:
	# A validator declared `-> void`, or one that fell off the end of a branch. `null` is not acceptance: the
	# fail direction for an unrecognised verdict is refusal, because applying on a value nobody chose is how
	# an unvalidated request reaches authoritative state.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _verdictless)
	var code: int = lane._apply(5, VERB_MOVE, {}, 42)
	assert_eq(code, NetCommand.CODE_OK, "there is no code to reply with")
	assert_eq(_announced.size(), 0, "and nothing is applied")
	lane.free()

func test_a_tag_is_never_zero_and_never_negative() -> void:
	# 0 is the sentinel `rejected` carries for a refusal no request() on this peer produced, so a real request
	# must never mint it -- including at the wrap, where a counter that resets to 0 would hand one out.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	var first: int = lane.request(VERB_MOVE, {})
	var second: int = lane.request(VERB_MOVE, {})
	assert_true(first > 0, "the first tag is positive")
	assert_eq(second, first + 1, "and they advance")
	lane._next_tag = 0x7FFF_FFFE
	assert_eq(lane.request(VERB_MOVE, {}), 0x7FFF_FFFF, "the last tag before the wrap")
	assert_eq(lane.request(VERB_MOVE, {}), 1, "and the wrap lands on 1, never on 0")
	lane.free()

# --- batching -------------------------------------------------------------------------------------

func test_a_batch_applies_each_payload_in_order() -> void:
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	var tags: PackedInt32Array = lane.request_batch(VERB_MOVE, [{"n": 1}, {"n": 2}, {"n": 3}])
	assert_eq(tags.size(), 3, "one tag per payload")
	assert_eq(_seen_payloads.size(), 3, "the handler ran once per entry")
	assert_eq(_seen_payloads[0]["n"], 1, "in submission order")
	assert_eq(_seen_payloads[2]["n"], 3, "to the last entry")
	assert_eq(_announced.size(), 3, "and each acceptance is announced on its own")
	lane.free()

func test_a_mixed_batch_applies_the_accepted_and_refuses_only_the_rest() -> void:
	# A batch is a coalescing optimization, NOT a transaction: an entry refused after an accepted one does not
	# undo it. That is the property a caller must know before batching an inventory move.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept_even_n)
	var tags: PackedInt32Array = lane.request_batch(VERB_MOVE, [{"n": 2}, {"n": 3}, {"n": 4}])
	assert_eq(_announced.size(), 2, "the two legal entries applied")
	assert_eq(_rejected_codes.size(), 1, "and exactly one refusal was announced")
	assert_eq(_rejected_codes[0], REASON_ODD, "carrying the validator's reason")
	assert_eq(_rejected_tags[0], tags[1], "against the tag of the entry that was refused")
	lane.free()

func test_a_batch_over_the_cap_is_refused_whole() -> void:
	# Trimming to a legal prefix would apply requests the caller never separated out, and it is exactly the
	# shape that makes a validator's bound unassertable.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	var payloads: Array = []
	for i: int in NetCommand.MAX_BATCH + 1:
		payloads.push_back({"n": i})
	var tags: PackedInt32Array = lane.request_batch(VERB_MOVE, payloads)
	assert_eq(_seen_payloads.size(), 0, "no entry was applied")
	assert_eq(_rejected_codes.size(), tags.size(), "every entry was refused")
	assert_eq(_rejected_codes[0], NetCommand.CODE_BATCH_TOO_LARGE, "under the lane's own reason code")
	assert_true(NetCommand.CODE_BATCH_TOO_LARGE < 0, "which is negative so it cannot collide with a game's")
	lane.free()

func test_a_non_dictionary_entry_is_dropped_rather_than_erroring() -> void:
	# A batch crosses the wire as a plain Array, so its entries are whatever the sender put there. A String
	# where a payload was expected must be skipped, exactly as an unregistered verb is.
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	lane.request_batch(VERB_MOVE, [{"n": 1}, "not a payload", {"n": 2}])
	assert_eq(_seen_payloads.size(), 2, "the two real payloads still applied")
	assert_eq(_announced.size(), 2, "and the junk entry announced nothing")
	lane.free()

func test_an_empty_batch_mints_nothing_and_does_nothing() -> void:
	var lane: NetCommand = _fresh()
	lane.register(VERB_MOVE, _accept)
	assert_eq(lane.request_batch(VERB_MOVE, []).size(), 0, "no tags for no payloads")
	assert_eq(_seen_payloads.size(), 0, "and no handler ran")
	lane.free()

func test_a_refusal_outside_a_session_drives_nothing() -> void:
	# `_refused` is what drives a client's UI, and it is guarded twice: the transfer mode restricts the caller
	# to the server, and the sender is checked again so one client cannot tell another that its request failed.
	# A lane outside the SceneTree has no MultiplayerAPI at all, which is the arm under test here -- the sender
	# arm needs a live session and is exercised by the two-peer probe, which asserts the code that comes back.
	var lane: NetCommand = _fresh()
	assert_true(lane.multiplayer == null, "the case this asserts is a lane with no MultiplayerAPI")
	lane._refused(VERB_MOVE, PackedInt32Array([REASON_COOLING]), PackedInt32Array([7]))
	assert_eq(_rejected_codes.size(), 0, "a refusal reaching a lane outside a session drives nothing")
	lane.free()

func test_a_refusal_with_mismatched_lengths_is_ignored() -> void:
	# The two arrays are read in lockstep. A frame whose halves disagree is corrupt or hostile, and reading
	# past the shorter one is the bug it would cause. Driven through the ANNOUNCE half rather than through
	# `_refused`, whose routing guards need a live MultiplayerAPI -- otherwise this test would return at the
	# null guard and assert nothing about lengths at all.
	var lane: NetCommand = _fresh()
	lane._announce_refusals(VERB_MOVE, PackedInt32Array([1, 2]), PackedInt32Array([7]))
	assert_eq(_rejected_codes.size(), 0, "mismatched halves announce nothing")
	lane._announce_refusals(VERB_MOVE, PackedInt32Array([REASON_COOLING]), PackedInt32Array([7]))
	assert_eq(_rejected_codes.size(), 1, "and matched halves do announce, so the guard is not a blanket refusal")
	assert_eq(_rejected_tags[0], 7, "carrying the tag the server quoted back")
	lane.free()
