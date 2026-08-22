extends UnitTest
## Scene-free coverage for [method NetRollbackHandle.set_input_authority] -- the one call a roster makes when a
## seat changes hands.
##
## What is worth testing here is that it is ONE call rather than two, and that it still is against a backend
## that predates it. Re-pointing a body means writing the input node's multiplayer authority AND re-resolving
## the synchronizer's cached copy of the answer; doing only the first leaves this peer predicting the wrong
## body and the send path anchoring the wrong peer's interest radius, and nothing errors when it happens.
##
## The backend synchronizer is stubbed by a plain Node carrying the same method and property names. The handle
## holds its synchronizer as an opaque Node and reaches it by name, so a stub is a faithful stand-in and this
## suite needs no cdylib, no scene tree and no session.

## Stands in for a current backend: it owns `set_input_authority` and does both halves itself.
class CurrentSyncStub extends Node:
	var authority_set_to: int = -1
	var authority_reprocessed: int = 0

	func set_input_authority(peer: int) -> void:
		authority_set_to = peer
		authority_reprocessed += 1

	func process_authority() -> void:
		authority_reprocessed += 1

## Stands in for a backend built before the call: it has the `input_authority_node` export and
## `process_authority`, and nothing else. This is the pairing a bisect or an un-rebuilt working copy produces.
class LegacySyncStub extends Node:
	var input_authority_node: Node = null
	var authority_reprocessed: int = 0

	func process_authority() -> void:
		authority_reprocessed += 1

func test_one_call_writes_the_authority_and_re_resolves_it() -> void:
	var stub: CurrentSyncStub = CurrentSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_input_authority(7)
	assert_eq(stub.authority_set_to, 7, "the peer reaches the synchronizer verbatim")
	assert_eq(stub.authority_reprocessed, 1, "and the cached owner is re-resolved in the same call")
	stub.free()

func test_peer_one_hands_the_body_back_to_the_server() -> void:
	# What an emptied seat means. It is a peer id like any other, so nothing special-cases it -- the test
	# exists because the RELEASE path is the one a game reaches for least often and gets wrong most.
	var stub: CurrentSyncStub = CurrentSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_input_authority(1)
	assert_eq(stub.authority_set_to, 1, "the server takes the input")
	stub.free()

func test_a_backend_without_the_call_still_re_points_the_body() -> void:
	var stub: LegacySyncStub = LegacySyncStub.new()
	var input_node: Node = Node.new()
	stub.input_authority_node = input_node
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_input_authority(5)
	assert_eq(input_node.get_multiplayer_authority(), 5, "the fallback writes the input node's authority")
	assert_eq(stub.authority_reprocessed, 1, "and re-resolves the cached copy, exactly as the backend would")
	input_node.free()
	stub.free()

func test_the_fallback_still_re_resolves_when_no_input_node_is_declared() -> void:
	# A synchronizer with no `input_authority_node` resolves its input against the body root, which the
	# fallback cannot reach from here. Re-resolving anyway is what keeps the handle's contract -- the cached
	# owner is never left describing a state that has moved on.
	var stub: LegacySyncStub = LegacySyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_input_authority(5)
	assert_eq(stub.authority_reprocessed, 1, "process_authority ran")
	stub.free()

func test_an_inert_handle_no_ops() -> void:
	# OFFLINE contract: the same code path runs with no session at all.
	var handle: NetRollbackHandle = NetRollbackHandle.new(null)
	handle.set_input_authority(3)
	assert_false(handle.is_active(), "an inert handle stays inert")
