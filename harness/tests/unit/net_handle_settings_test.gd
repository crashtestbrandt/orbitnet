extends UnitTest
## Scene-free coverage for the SETTINGS RESOLUTION on the two synchronizer handles ([NetStateHandle],
## [NetRollbackHandle]) -- the declarations a game makes before `process_settings()` re-reads them, and the
## reads it makes afterward.
##
## The handles are thin forwarders, so what is worth pinning is the places they decide something or degrade
## something. Three of those, and none is covered by the membership, seat, input-authority or bulk-hook
## suites beside this one:
##
## - **`set_priority()` CLAMPS.** The backend takes 1..16 and a game may compute a weight from anything. An
##   unclamped 0 is a channel with no weight in the rota; an unclamped 99 is one that starves every other.
## - **`last_known_state()` FAILS OPEN.** A cdylib too old to answer reports the PRESENT, not -1. Returning
##   -1 there would read as "no row has ever arrived", and every staleness rule keyed on it would blank the
##   world one threshold after spawn. `reports_last_known_state()` is what makes the fallback visible, since
##   the number itself cannot show which branch produced it.
## - **`is_receiving()` FAILS OPEN AND `last_received_state()` DOES NOT.** The receipt reading answers one
##   question -- is this peer still being SENT this entity -- and it answers `-1` when it cannot, including
##   for the whole session on the peer that authors the state and receives nothing. The rule built on it
##   guesses "yes" in every case that is not a known no, so the sentinel and the policy are different numbers
##   on purpose. Both handles carry the same four methods, so one game helper spans both lanes.
## - **A DECLARATION DOES NOT RE-READ THE SCHEMA.** Every declaration lands on the synchronizer immediately,
##   and `process_settings()` is a separate call the game owes once the set is complete. A handle that
##   processed on every declaration would re-resolve the property list once per entry at spawn.
## - **`quantizer_fallbacks()` IS THE ASSERTABLE HALF OF THE `@` SUFFIXES.** A dropped `@half` is a bandwidth
##   bug: the game runs, the frames decode, and the only symptom is a wire wider than the property list
##   claims. The list reaches the game verbatim so a boot check can fail on it, and it fails OPEN -- empty on
##   a backend too old to answer -- so such a check passes rather than blocking a bisect.
##
## Plus the inert contract for the reads that have one: OFFLINE there is no synchronizer, and each read has a
## documented value that is not merely zero -- -1 for a tick that never arrived, the caller's own fallback
## for an unrecorded memo.
##
## The backend synchronizers are stubbed by plain Nodes carrying the same property and method names. The
## handles hold their synchronizer as an opaque Node and reach it by name, so a stub is a faithful stand-in
## and this suite needs no cdylib, no scene tree and no session.

const RELEVANCY_ALWAYS: int = 0
const RELEVANCY_ANCHORED: int = 1

## Stands in for a CURRENT backend state synchronizer: the exports the handle writes, the staleness getter,
## and counters for the two calls that are meant to be explicit.
class StateSyncStub extends Node:
	var relevancy: int = RELEVANCY_ALWAYS
	var anchor_property: String = ""
	var priority: int = 1
	var last_state_tick: int = -1
	var received_tick: int = -1
	var authors: bool = false
	var declared: PackedStringArray = PackedStringArray()
	var settings_processed: int = 0
	var quant_fallbacks: PackedStringArray = PackedStringArray()

	func add_state(_node: Object, property: String) -> void:
		declared.push_back(property)

	func process_settings() -> void:
		settings_processed += 1

	func get_last_known_state() -> int:
		return last_state_tick

	func get_last_received_state() -> int:
		return received_tick

	func authors_state() -> bool:
		return authors

	func quantizer_fallbacks() -> PackedStringArray:
		return quant_fallbacks

## Stands in for a backend built before `get_last_known_state` existed. Everything else is there, which is
## exactly the pairing a bisect or an un-rebuilt working copy produces. It has neither receipt method either,
## so it stands in for the old binary on both readings.
class OldStateSyncStub extends Node:
	var relevancy: int = RELEVANCY_ALWAYS
	var anchor_property: String = ""
	var priority: int = 1

## Stands in for a current backend rollback synchronizer: the declarations, the two re-read calls, the
## prediction flag, the two lane frontiers, and the resim memo log.
class RollbackSyncStub extends Node:
	var state_props: PackedStringArray = PackedStringArray()
	var input_props: PackedStringArray = PackedStringArray()
	var settings_processed: int = 0
	var authority_processed: int = 0
	var predicting: bool = false
	var state_tick: int = -1
	var input_tick: int = -1
	var received_tick: int = -1
	var authors: bool = false
	var memos: Dictionary[int, int] = {}
	var quant_fallbacks: PackedStringArray = PackedStringArray()

	func add_state(_node: Object, property: String) -> void:
		state_props.push_back(property)

	func add_input(_node: Object, property: String) -> void:
		input_props.push_back(property)

	func process_settings() -> void:
		settings_processed += 1

	func process_authority() -> void:
		authority_processed += 1

	func is_predicting() -> bool:
		return predicting

	func get_last_known_state() -> int:
		return state_tick

	func get_last_known_input() -> int:
		return input_tick

	func get_last_received_state() -> int:
		return received_tick

	func authors_state() -> bool:
		return authors

	# Keyed on (tick, key) the way the backend's log is, flattened so a stub needs one dictionary.
	func memo_set(tick: int, key: int, value: int) -> void:
		memos[tick * 1000 + key] = value

	func memo_get(tick: int, key: int, fallback: int) -> int:
		var slot: int = tick * 1000 + key
		if not memos.has(slot):
			return fallback
		return memos[slot]

	func quantizer_fallbacks() -> PackedStringArray:
		return quant_fallbacks

## Stands in for a rollback backend built before the receipt reading existed. It KEEPS `get_last_known_state`
## deliberately: that method is what the old binary had, and it is the reading the new one has to degrade
## independently of -- a peer can know a frontier and still be unable to say where it came from.
class OldRollbackSyncStub extends Node:
	var state_tick: int = -1
	var input_tick: int = -1

	func get_last_known_state() -> int:
		return state_tick

	func get_last_known_input() -> int:
		return input_tick

# --- the priority clamp ---------------------------------------------------------------------------

func test_priority_is_clamped_into_the_band_the_backend_takes() -> void:
	# A game computes a weight from its own semantics -- a threat score, a squad size -- and nothing stops
	# that arriving as 0 or 99. The backend takes 1..16, and a weight outside that band is undefined rather
	# than merely extreme.
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_priority(0)
	assert_eq(stub.priority, 1, "0 clamps up to the floor rather than de-weighting the channel entirely")
	handle.set_priority(-5)
	assert_eq(stub.priority, 1, "a negative weight clamps up too")
	handle.set_priority(99)
	assert_eq(stub.priority, 16, "99 clamps down to the ceiling rather than starving every other channel")
	stub.free()

func test_a_weight_inside_the_band_is_forwarded_untouched() -> void:
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_priority(1)
	assert_eq(stub.priority, 1, "the floor is a legal weight, not a clamped one")
	handle.set_priority(8)
	assert_eq(stub.priority, 8, "an ordinary weight reaches the rota verbatim")
	handle.set_priority(16)
	assert_eq(stub.priority, 16, "and so does the ceiling")
	stub.free()

func test_an_inert_handle_takes_a_priority_without_erroring() -> void:
	var handle: NetStateHandle = NetStateHandle.new(null)
	handle.set_priority(99)
	assert_false(handle.is_active(), "an inert handle stays inert")

# --- the anchor declaration -----------------------------------------------------------------------

func test_an_anchor_turns_the_distance_test_on() -> void:
	# A channel declares no anchor by default and is ALWAYS relevant. Naming one is the whole opt-in: the
	# obvious heuristic -- the first Vector3 the channel replicates -- is as likely to be a local-space
	# offset, which would park the channel at the world origin and cull it for everybody.
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	assert_eq(stub.relevancy, RELEVANCY_ALWAYS, "a channel starts always-relevant")
	handle.set_anchor("../Body:global_position")
	assert_eq(stub.anchor_property, "../Body:global_position", "the entry reaches the synchronizer verbatim")
	assert_eq(stub.relevancy, RELEVANCY_ANCHORED, "and declaring it is what makes the channel cullable")
	stub.free()

func test_an_inert_handle_takes_an_anchor_without_erroring() -> void:
	var handle: NetStateHandle = NetStateHandle.new(null)
	handle.set_anchor("global_position")
	assert_false(handle.is_active(), "an inert handle stays inert")

# --- declaring is not processing ------------------------------------------------------------------

func test_state_declarations_land_immediately_but_do_not_re_read_the_schema() -> void:
	# The two halves are separate on purpose: a handle that processed on every declaration would re-resolve
	# the whole property list once per entry, and a fat channel is dozens of them.
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.add_state(stub, "net_pos@half")
	handle.add_state(stub, "hp")
	assert_eq(stub.declared.size(), 2, "both entries reached the synchronizer")
	assert_eq(stub.declared[0], "net_pos@half", "including the quantizer suffix, which the backend reads")
	assert_eq(stub.settings_processed, 0, "and neither declaration re-read the schema")
	handle.process_settings()
	assert_eq(stub.settings_processed, 1, "the game owes exactly one process_settings() afterward")
	stub.free()

func test_rollback_declarations_keep_the_two_lanes_apart() -> void:
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.add_state(stub, "net_pos")
	handle.add_input(stub, "nin_move")
	handle.add_state(stub, "net_orient")
	assert_eq(stub.state_props.size(), 2, "the state entries went to the state lane")
	assert_eq(stub.input_props.size(), 1, "and the input entry to the input lane")
	assert_eq(stub.settings_processed, 0, "declaring re-reads nothing")
	stub.free()

func test_the_two_re_read_calls_are_separate_on_the_rollback_lane() -> void:
	# `process_settings()` re-resolves the schema; `process_authority()` re-resolves who owns prediction. A
	# roster change needs the second and not the first, and a spawn needs the first and not the second.
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.process_settings()
	assert_eq(stub.settings_processed, 1, "process_settings() re-reads the schema")
	assert_eq(stub.authority_processed, 0, "and does not touch the authority")
	handle.process_authority()
	assert_eq(stub.authority_processed, 1, "process_authority() re-reads the owner")
	assert_eq(stub.settings_processed, 1, "and does not re-read the schema")
	stub.free()

func test_an_inert_handle_processes_nothing_without_erroring() -> void:
	# Callers wire the same code path whether or not networking is live, so every one of these is reached
	# offline on the very first frame of a single-player run.
	var state: NetStateHandle = NetStateHandle.new(null)
	state.add_state(state, "hp")
	state.process_settings()
	var rollback: NetRollbackHandle = NetRollbackHandle.new(null)
	rollback.add_state(rollback, "net_pos")
	rollback.add_input(rollback, "nin_move")
	rollback.process_settings()
	rollback.process_authority()
	assert_false(state.is_active(), "the state handle is inert throughout")
	assert_false(rollback.is_active(), "and so is the rollback handle")

# --- the staleness read, and its fail-open --------------------------------------------------------

func test_a_backend_that_answers_reports_the_measured_tick() -> void:
	var stub: StateSyncStub = StateSyncStub.new()
	stub.last_state_tick = 412
	var handle: NetStateHandle = NetStateHandle.new(stub)
	assert_eq(handle.last_known_state(), 412, "the tick of the newest row this channel received")
	assert_true(handle.reports_last_known_state(), "and the reading is measured, not a fallback")
	stub.free()

func test_the_capability_is_cached_but_the_value_is_not() -> void:
	# The has_method() lookup is resolved once at construction, because the answer cannot change within a
	# process and the question was being asked once per replicated body per render frame. What must NOT be
	# cached is the tick itself -- it rises with every row that lands.
	var stub: StateSyncStub = StateSyncStub.new()
	stub.last_state_tick = 100
	var handle: NetStateHandle = NetStateHandle.new(stub)
	assert_eq(handle.last_known_state(), 100, "the first read")
	stub.last_state_tick = 137
	assert_eq(handle.last_known_state(), 137, "and every later read is live, not the cached first answer")
	stub.free()

func test_a_backend_too_old_to_answer_reports_the_present() -> void:
	# THE FAIL-OPEN. -1 here would read as "no row has ever arrived", a staleness rule would measure from the
	# body's spawn tick instead, and every remote body on that peer would vanish one threshold after spawn
	# and never return. A binary mismatch degrades a diagnostic; it never blanks the world.
	var stub: OldStateSyncStub = OldStateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	assert_eq(handle.last_known_state(), Net.current_tick(), "an unanswerable backend reports the present")
	assert_false(handle.reports_last_known_state(), "and says the reading is the fallback, not a measurement")
	stub.free()

func test_an_inert_handle_reports_no_row_and_no_measurement() -> void:
	# Inert is the one case that DOES report -1: there is no channel, so there is no fail-open to make and
	# nothing that could mistake the answer for a live one.
	var handle: NetStateHandle = NetStateHandle.new(null)
	assert_eq(handle.last_known_state(), -1, "an inert channel has received no row")
	assert_false(handle.reports_last_known_state(), "and measures nothing")

# --- the rollback lane's reads --------------------------------------------------------------------

func test_the_prediction_flag_is_forwarded() -> void:
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_false(handle.is_predicting(), "a settled body is not predicting")
	stub.predicting = true
	assert_true(handle.is_predicting(), "and the reconciliation gate reads live")
	stub.free()

func test_both_lane_frontiers_are_forwarded() -> void:
	# The input frontier is what the stale-input coast rule keys on: `tick - last_known_input()` is how long
	# that lane has been silent. Reading the state frontier for it would coast on the wrong silence.
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	stub.state_tick = 480
	stub.input_tick = 476
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_eq(handle.get_last_known_state(), 480, "the state lane's frontier")
	assert_eq(handle.get_last_known_input(), 476, "and the input lane's, which is a different number")
	stub.free()

func test_an_inert_rollback_handle_reports_no_row_on_either_lane() -> void:
	var handle: NetRollbackHandle = NetRollbackHandle.new(null)
	assert_false(handle.is_predicting(), "an inert body never mis-predicts")
	assert_eq(handle.get_last_known_state(), -1, "no state row has arrived")
	assert_eq(handle.get_last_known_input(), -1, "and no input row has")

func test_a_memo_round_trips_per_tick_and_key() -> void:
	# The resim log: recorded on the fresh pass, read back identically by every replayed pass, so a resim
	# resolves against what the fresh pass saw even if live state changed since.
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.memo_set(480, 1, 7)
	handle.memo_set(481, 1, 9)
	assert_eq(handle.memo_get(480, 1, -1), 7, "the value recorded at that tick")
	assert_eq(handle.memo_get(481, 1, -1), 9, "and the next tick's is its own")
	assert_eq(handle.memo_get(480, 2, -1), -1, "another key at the same tick was never recorded")
	assert_eq(handle.memo_get(479, 1, -1), -1, "and a tick before the first record has nothing either")
	stub.free()

func test_an_inert_memo_read_falls_through_to_the_callers_own_value() -> void:
	# OFFLINE there is no resim to resolve against, so the read must hand back the live value the caller
	# passed rather than a zero the caller would then act on.
	var handle: NetRollbackHandle = NetRollbackHandle.new(null)
	handle.memo_set(480, 1, 7)
	assert_eq(handle.memo_get(480, 1, 3), 3, "the caller's fallback is the answer offline")
	assert_eq(handle.memo_get(480, 1, -1), -1, "whatever that fallback is")

# --- the receipt reading, and the two things it separates -----------------------------------------

func test_the_receipt_tick_is_forwarded_on_both_lanes() -> void:
	# Two different numbers on purpose. The frontier is the newest state this peer KNOWS; the receipt is the
	# newest row it was SENT. They diverge on every peer that authors state, and the rollback lane's frontier
	# is raised by that peer's own simulation.
	var channel: StateSyncStub = StateSyncStub.new()
	channel.last_state_tick = 480
	channel.received_tick = 477
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_eq(state.last_received_state(), 477, "the state channel's newest received row")
	assert_eq(state.last_known_state(), 480, "which is a separate reading from the frontier it knows")
	assert_true(state.reports_last_received_state(), "and the receipt is measured, not a sentinel")
	var body: RollbackSyncStub = RollbackSyncStub.new()
	body.state_tick = 480
	body.received_tick = 477
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	assert_eq(rollback.last_received_state(), 477, "the same reading on the rollback lane")
	assert_eq(rollback.get_last_known_state(), 480, "beside the frontier this peer's own simulation raises")
	assert_true(rollback.reports_last_received_state(), "and it too is measured")
	channel.free()
	body.free()

func test_the_authority_reads_as_receiving_though_no_row_ever_arrives() -> void:
	# THE CASE THE READING EXISTS FOR. On the peer that authors the state nothing is ever received, so the
	# receipt stays -1 for the whole session -- which on its own is indistinguishable from "every row was
	# withheld since spawn". Without the authorship short-circuit a stale-body rule would blank the server's
	# own world, and every body on a listen-server host with it.
	var channel: StateSyncStub = StateSyncStub.new()
	channel.authors = true
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_true(state.authors_state(), "this peer authors the channel")
	assert_eq(state.last_received_state(), -1, "so not one row has arrived, and the reading says so plainly")
	assert_true(state.is_receiving(), "while the rule short-circuits on authorship rather than on the tick")
	var body: RollbackSyncStub = RollbackSyncStub.new()
	body.authors = true
	body.state_tick = 480   # raised by this peer's own simulation, with no row behind it
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	assert_true(rollback.authors_state(), "the same on the rollback lane")
	assert_eq(rollback.last_received_state(), -1, "where the frontier beside it is the locally simulated tick")
	assert_true(rollback.is_receiving(), "and the body is not culled from the peer that authors it")
	channel.free()
	body.free()

func test_a_receiver_that_has_never_been_sent_a_row_is_the_one_known_no() -> void:
	# The only answer that is a no: a backend that CAN measure, a peer that does not author the entity, and
	# no row at all. Everything else in this rule guesses yes.
	var channel: StateSyncStub = StateSyncStub.new()
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_false(state.authors_state(), "this peer receives the channel rather than authoring it")
	assert_false(state.is_receiving(), "and no row has ever been sent to it")
	var body: RollbackSyncStub = RollbackSyncStub.new()
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	assert_false(rollback.authors_state(), "the same on the rollback lane")
	assert_false(rollback.is_receiving(), "and the same answer")
	channel.free()
	body.free()

func test_the_window_is_reached_only_after_the_two_short_circuits() -> void:
	# The suite runs with no session, so `Net.current_tick()` is 0 and every non-negative reading sits inside
	# every non-negative window. A window that can admit nothing is what pins two things from here: that the
	# comparison happens at all, and that the fail-open branches above it are not reached through it.
	var channel: StateSyncStub = StateSyncStub.new()
	channel.received_tick = Net.current_tick()
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_true(state.is_receiving(), "a row at the current tick is well inside the default window")
	assert_false(state.is_receiving(-1), "a window that admits nothing rejects that same row")
	channel.authors = true
	assert_true(state.is_receiving(-1), "and authorship is answered before the window is ever consulted")
	channel.free()

func test_a_backend_without_the_receipt_reads_as_receiving() -> void:
	# THE OLD-BINARY CONTRACT. The cdylib is refreshed only at a release tag, so new addon code legitimately
	# runs against a binary that has neither method -- a PR branch, a bisect, any tree that has not rebuilt.
	# The rule degrades to "still arriving" rather than blanking every remote entity on that peer, and it
	# says that it is guessing.
	var channel: OldStateSyncStub = OldStateSyncStub.new()
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_true(state.is_receiving(), "an unanswerable backend fails open")
	assert_false(state.reports_last_received_state(), "and says the reading is not a measurement")
	assert_eq(state.last_received_state(), -1, "the reading stays the honest sentinel rather than the present")
	assert_false(state.authors_state(), "and authorship it cannot answer reads as false")
	var body: OldRollbackSyncStub = OldRollbackSyncStub.new()
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	assert_true(rollback.is_receiving(), "the same fail-open on the rollback lane")
	assert_false(rollback.reports_last_received_state(), "the same admission that nothing was measured")
	assert_eq(rollback.last_received_state(), -1, "and the same sentinel")
	channel.free()
	body.free()

func test_an_inert_handle_reports_no_receipt_but_still_reads_as_receiving() -> void:
	# OFFLINE there is no wire to stop, so the rule must not start hiding entities on the first frame of a
	# single-player run -- while the reading itself has nothing to report and says -1.
	var state: NetStateHandle = NetStateHandle.new(null)
	assert_eq(state.last_received_state(), -1, "an inert channel has been sent no row")
	assert_false(state.reports_last_received_state(), "and measures nothing")
	assert_false(state.authors_state(), "there is no synchronizer whose state anything authors")
	assert_true(state.is_receiving(), "and the rule fails open")
	var rollback: NetRollbackHandle = NetRollbackHandle.new(null)
	assert_eq(rollback.last_received_state(), -1, "the same four answers on the rollback lane")
	assert_false(rollback.reports_last_received_state(), "no measurement")
	assert_false(rollback.authors_state(), "no authorship")
	assert_true(rollback.is_receiving(), "and the same fail-open")

# --- the dropped-quantizer list -------------------------------------------------------------------

func test_the_dropped_quantizer_list_reaches_the_game_verbatim_on_both_lanes() -> void:
	# A boot check asserts on the ENTRIES, not on a count: the whole point is that the failure message names
	# the declaration the game wrote, since `"hp@half"` looks like it is saving bytes and saves none.
	var channel: StateSyncStub = StateSyncStub.new()
	channel.quant_fallbacks = PackedStringArray([".:hp@half", "Turret:aim@bogus"])
	var state: NetStateHandle = NetStateHandle.new(channel)
	var dropped: PackedStringArray = state.quantizer_fallbacks()
	assert_eq(dropped.size(), 2, "both entries reached the game")
	assert_eq(dropped[0], ".:hp@half", "an invalid pairing, with its suffix intact")
	assert_eq(dropped[1], "Turret:aim@bogus", "and an unrecognized suffix on the same list")
	var body: RollbackSyncStub = RollbackSyncStub.new()
	body.quant_fallbacks = PackedStringArray(["nin_throttle@half"])
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	var body_dropped: PackedStringArray = rollback.quantizer_fallbacks()
	assert_eq(body_dropped.size(), 1, "one list spans the body's state, cosmetic and input lanes")
	assert_eq(body_dropped[0], "nin_throttle@half", "and an input entry lands on it like any other")
	channel.free()
	body.free()

func test_an_entity_whose_quantizers_all_apply_reports_nothing_dropped() -> void:
	# Empty is the healthy answer, and it has to be reachable through a backend that CAN answer -- otherwise
	# the boot check below would pass for the wrong reason on every tree that has not rebuilt the cdylib.
	var channel: StateSyncStub = StateSyncStub.new()
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_eq(state.quantizer_fallbacks().size(), 0, "nothing was dropped, and the backend says so")
	var body: RollbackSyncStub = RollbackSyncStub.new()
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	assert_eq(rollback.quantizer_fallbacks().size(), 0, "the same on the rollback lane")
	channel.free()
	body.free()

func test_a_backend_too_old_to_answer_reports_nothing_dropped() -> void:
	# THE FAIL-OPEN, and it points the opposite way from the diagnostic it reports on: a boot check written
	# against a newer addon than the loaded cdylib PASSES rather than blocking a bisect. The cdylib is
	# refreshed only at a release tag, so this pairing is ordinary rather than exotic.
	var channel: OldStateSyncStub = OldStateSyncStub.new()
	var state: NetStateHandle = NetStateHandle.new(channel)
	assert_eq(state.quantizer_fallbacks().size(), 0, "an unanswerable backend reports nothing dropped")
	var body: OldRollbackSyncStub = OldRollbackSyncStub.new()
	var rollback: NetRollbackHandle = NetRollbackHandle.new(body)
	assert_eq(rollback.quantizer_fallbacks().size(), 0, "and so does the older rollback backend")
	channel.free()
	body.free()

func test_an_inert_handle_reports_nothing_dropped() -> void:
	# OFFLINE there is no schema and no wire, so there is nothing a quantizer could have been dropped from.
	var state: NetStateHandle = NetStateHandle.new(null)
	assert_eq(state.quantizer_fallbacks().size(), 0, "an inert channel dropped nothing")
	var rollback: NetRollbackHandle = NetRollbackHandle.new(null)
	assert_eq(rollback.quantizer_fallbacks().size(), 0, "and neither did an inert body")
