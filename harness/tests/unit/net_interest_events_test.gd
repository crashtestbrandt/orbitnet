extends UnitTest
## Scene-free coverage for the RELEVANCY EDGE on the [code]Net[/code] facade: two signals and the two queries
## that give a handler bound mid-session somewhere to start from.
##
## A culled or withheld entity used to be a node frozen at its last pose with no way for the game to learn why.
## [signal Net.entity_left_interest] is that fact, and [signal Net.entity_entered_interest] is its twin. What
## this suite reaches without a session is the FACADE CONTRACT, which is the half that has to hold on every
## checkout rather than only on a live link:
##
## - OFFLINE IS INERT. Both queries answer empty and nothing announces itself, so a game wires one code path
##   and runs it unchanged in a single-player session.
## - A BACKEND THAT PREDATES THE CALLS ANSWERS THE SAME. That is not hypothetical: this suite runs against the
##   COMMITTED cdylib, which is refreshed only at a release tag, so a binary older than these sources is the
##   ordinary checkout and the `_backend_has` guard is what keeps it running.
## - THE SIGNALS CARRY `(peer, entity_id)`, in that order -- the [signal Net.seat_opened] /
##   [signal Net.seat_closed] convention, which is what lets one handler serve a server (where `peer` is the
##   connection that lost the entity) and a client (where it is this peer's own id). A facade signal whose
##   arity drifted from the backend's would connect and then never fire, which is a silent failure.
##
## What is NOT here, and where it lives instead: the wire section, the re-send-until-acked rule, the byte
## reserve the send path takes for it, and the "culled and unregistered on one tick fires exactly once" rule
## are all pure and are covered in the Rust suites (`codec.rs` and `orbit_net.rs`). A probe would only
## re-assert them through a slower door.

## An entity id the facade will never resolve. Ids are node-path hashes and routinely negative, so the queries
## have to be total over one they do not know rather than erroring.
const UNKNOWN_ENTITY: int = -8_070_450_532_247_928_832

## Peers and entities one watcher saw, flattened so a 64-bit id survives the recording.
var _seen_peers: Array[int] = []
var _seen_entities: Array[int] = []
var _seen_kinds: PackedStringArray = PackedStringArray()

func test_both_relevancy_signals_are_declared() -> void:
	assert_true(Net.has_signal(&"entity_entered_interest"), "entity_entered_interest is on the facade")
	assert_true(Net.has_signal(&"entity_left_interest"), "entity_left_interest is on the facade")

func test_a_handler_receives_the_connection_first_and_the_entity_second() -> void:
	# THE ARGUMENT ORDER IS THE CONTRACT, and it is the seat pair's. A handler is written once and bound on
	# both ends, so `peer` leading is what makes it portable between them.
	#
	# Driven through the facade's own BRIDGE handlers rather than by emitting the signal from outside: those
	# are what the backend's signals are connected to, so this exercises the relay a live session runs -- a
	# bridge that swapped its two arguments would connect cleanly and misreport every event.
	_clear()
	Net.entity_left_interest.connect(_on_left)
	Net.entity_entered_interest.connect(_on_entered)
	Net._on_backend_entity_left_interest(4, UNKNOWN_ENTITY)
	Net._on_backend_entity_entered_interest(7, 12)
	Net.entity_left_interest.disconnect(_on_left)
	Net.entity_entered_interest.disconnect(_on_entered)

	assert_eq(_seen_kinds.size(), 2, "both signals reached the handler")
	if _seen_kinds.size() == 2:
		assert_eq(_seen_kinds[0], "left", "the leave arrived first")
		assert_eq(_seen_peers[0], 4, "and named the connection that lost it")
		assert_eq(_seen_entities[0], UNKNOWN_ENTITY, "with the entity id unmodified")
		assert_eq(_seen_kinds[1], "entered", "then the enter")
		assert_eq(_seen_peers[1], 7, "naming its own connection")
		assert_eq(_seen_entities[1], 12, "and its own entity")

func test_offline_is_inert_for_both_queries() -> void:
	# OFFLINE the tick loop is not running and no interest pass has ever run, so there is nothing to answer
	# with. Empty and false, never an error.
	assert_eq(Net.current_mode(), Net.Mode.OFFLINE, "the suite runs with no session")
	assert_false(Net.is_entity_in_interest(4, UNKNOWN_ENTITY), "no session holds no interest")
	assert_eq(Net.entities_in_interest(4).size(), 0, "and lists none")

func test_the_queries_are_total_over_a_connection_that_does_not_exist() -> void:
	# Peer ids come from the transport and a caller may hold a stale one -- a connection that dropped between
	# the signal and the handler is the ordinary case. Answering empty is the contract; erroring is not. The
	# same answer covers a backend older than the calls, which is what the committed cdylib is.
	for peer: int in [0, -1, 1, 99]:
		assert_false(Net.is_entity_in_interest(peer, UNKNOWN_ENTITY), "peer %d holds nothing" % peer)
		assert_eq(Net.entities_in_interest(peer).size(), 0, "peer %d lists nothing" % peer)

func test_the_queries_answer_the_declared_types() -> void:
	# The facade assigns each backend answer to a typed local before returning it, because the call answers a
	# Variant and the GDScript rule for a wire-ish value is that the conversion is an assignment, never a cast.
	# Drift there surfaces here as the wrong type rather than as a runtime error in a consumer.
	var listed: PackedInt64Array = Net.entities_in_interest(4)
	assert_eq(typeof(listed), TYPE_PACKED_INT64_ARRAY, "entities_in_interest answers PackedInt64Array")
	var held: bool = Net.is_entity_in_interest(4, UNKNOWN_ENTITY)
	assert_eq(typeof(held), TYPE_BOOL, "is_entity_in_interest answers bool")

func test_an_entity_id_of_zero_is_answered_rather_than_refused() -> void:
	# `0` is what an unresolved handle reports. `set_entity_hidden` refuses to record it, and the queries are
	# consistent with that rather than making it a case the caller has to filter out first.
	assert_false(Net.is_entity_in_interest(4, 0), "an unresolved handle is in nobody's interest")

func test_binding_a_handler_offline_announces_nothing() -> void:
	# The shape a game actually writes: bind once at startup, before any session exists. Nothing the facade
	# offers OFFLINE is a relevancy transition, so nothing may fire.
	_clear()
	Net.entity_left_interest.connect(_on_left)
	Net.entity_entered_interest.connect(_on_entered)
	Net.set_entity_hidden(4, UNKNOWN_ENTITY, true)
	Net.set_entity_hidden(4, UNKNOWN_ENTITY, false)
	var ignored: PackedInt64Array = Net.entities_in_interest(4)
	Net.entity_left_interest.disconnect(_on_left)
	Net.entity_entered_interest.disconnect(_on_entered)
	assert_eq(ignored.size(), 0, "and the query answered empty on the way past")
	assert_eq(_seen_kinds.size(), 0, "an OFFLINE session announces no relevancy transition")

func _clear() -> void:
	_seen_peers.clear()
	_seen_entities.clear()
	_seen_kinds.clear()

func _on_left(peer: int, entity_id: int) -> void:
	_record("left", peer, entity_id)

func _on_entered(peer: int, entity_id: int) -> void:
	_record("entered", peer, entity_id)

func _record(kind: String, peer: int, entity_id: int) -> void:
	_seen_kinds.push_back(kind)
	_seen_peers.push_back(peer)
	_seen_entities.push_back(entity_id)
