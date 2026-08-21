extends UnitTest
## Scene-free coverage for the interest-membership declarations on the two handles ([NetStateHandle],
## [NetRollbackHandle]).
##
## What is worth testing here is the RELEVANCY PROMOTION RULE and nothing else. Both handles are thin
## forwarders, but `NetStateHandle.set_membership()` decides a value: a channel that has already declared an
## anchor must stay ANCHORED (culled by distance WITHIN its world), while one still on the default ALWAYS is
## promoted to MEMBERSHIP (one world, no distance test). Getting that backwards silently un-culls every
## anchored channel, which is a bandwidth regression no assertion elsewhere would catch.
##
## The backend synchronizer is stubbed by a plain Node carrying the same property names. The handles hold their
## synchronizer as an opaque Node and reach it by property/method NAME, so a stub is a faithful stand-in and
## this suite needs no cdylib, no scene tree and no session.

const RELEVANCY_ALWAYS: int = 0
const RELEVANCY_ANCHORED: int = 1
const RELEVANCY_MEMBERSHIP: int = 2

## Stands in for the backend state synchronizer: the exports the handle writes, and the diagnostic it reads.
class StateSyncStub extends Node:
	var relevancy: int = RELEVANCY_ALWAYS
	var anchor_property: String = ""
	var membership_property: String = ""
	var membership_value: int = 0

	func get_membership() -> int:
		# Mirrors the backend: ALWAYS means every world, whatever membership_property names.
		return 0 if relevancy == RELEVANCY_ALWAYS else membership_value

## Stands in for the backend rollback synchronizer, which has no relevancy export and needs none.
class RollbackSyncStub extends Node:
	var membership_property: String = ""
	var membership_value: int = 0
	var id: int = 0

	func get_membership() -> int:
		return membership_value

	func get_entity_id() -> int:
		return id

func test_membership_promotes_an_always_channel_to_membership() -> void:
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_membership("world_id")
	assert_eq(stub.membership_property, "world_id", "the entry reaches the synchronizer verbatim")
	assert_eq(stub.relevancy, RELEVANCY_MEMBERSHIP, "a default ALWAYS channel becomes MEMBERSHIP")
	stub.free()

func test_membership_does_not_clobber_an_anchored_channel() -> void:
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_anchor("global_position")
	handle.set_membership("world_id")
	assert_eq(stub.relevancy, RELEVANCY_ANCHORED, "an anchored channel stays culled by distance too")
	assert_eq(stub.anchor_property, "global_position", "the anchor survives the membership call")
	assert_eq(stub.membership_property, "world_id", "and the world is declared alongside it")
	stub.free()

func test_anchor_after_membership_adds_the_distance_axis() -> void:
	# Declaration order must not matter: both axes end up declared either way.
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_membership("world_id")
	handle.set_anchor("global_position")
	assert_eq(stub.relevancy, RELEVANCY_ANCHORED, "adding an anchor turns the distance test on")
	assert_eq(stub.membership_property, "world_id", "and the world declared first is still there")
	stub.free()

func test_membership_reports_zero_until_the_channel_declares_a_world() -> void:
	var stub: StateSyncStub = StateSyncStub.new()
	stub.membership_value = 7
	var handle: NetStateHandle = NetStateHandle.new(stub)
	assert_eq(handle.membership(), 0, "an ALWAYS channel is in every world, whatever the property says")
	handle.set_membership("world_id")
	assert_eq(handle.membership(), 7, "once promoted, the live value is what the filter reads")
	stub.free()

func test_an_empty_entry_declares_nothing_and_does_not_switch_the_policy() -> void:
	# Promoting on an empty entry would leave the channel permanently non-ALWAYS, warning at every
	# process_settings() about a membership_property it does not have, with no way back through the handle.
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_membership("")
	assert_eq(stub.relevancy, RELEVANCY_ALWAYS, "an empty entry leaves the channel in every world")
	assert_eq(stub.membership_property, "", "and clears the declaration rather than half-setting it")
	stub.free()

func test_membership_degrades_on_a_backend_with_no_relevancy_export() -> void:
	# `Object.get()` on an absent property returns null, and assigning Nil to a typed int aborts the caller.
	# The cdylib is committed separately from this GDScript, so that mismatch is a real tree, not a hypothetical.
	var stub: RollbackSyncStub = RollbackSyncStub.new()   # no `relevancy` property at all
	var handle: NetStateHandle = NetStateHandle.new(stub)
	handle.set_membership("world_id")
	assert_eq(stub.membership_property, "world_id", "the declaration still lands")
	stub.free()

func test_rollback_membership_is_forwarded_with_no_relevancy_switch() -> void:
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_membership("world_id")
	assert_eq(stub.membership_property, "world_id", "the entry reaches the synchronizer verbatim")
	stub.free()

func test_rollback_membership_reads_back() -> void:
	# The read-back that matters most: the OWNING PEER's world is read off this body, so a body reporting 0 is a
	# peer seeing every world and no other entity's declaration can filter anything for it.
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	stub.membership_value = 4
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_eq(handle.membership(), 4, "the live value the filter would read")
	stub.free()

func test_offline_handles_are_inert() -> void:
	# The OFFLINE contract: every method no-ops so callers wire the same code path unconditionally.
	var state: NetStateHandle = NetStateHandle.new(null)
	state.set_membership("world_id")
	assert_eq(state.membership(), 0, "an inert state handle reports every world")
	assert_false(state.is_active(), "and reports itself inert")
	var rollback: NetRollbackHandle = NetRollbackHandle.new(null)
	rollback.set_membership("world_id")
	assert_eq(rollback.membership(), 0, "an inert rollback handle reports every world")
	assert_false(rollback.is_active(), "an inert rollback handle reports itself inert")

func test_membership_is_zero_on_a_backend_that_cannot_answer() -> void:
	# The cdylib is committed separately from the GDScript, so new addon code legitimately runs against an
	# older binary with no `get_membership`. A binary mismatch must degrade the diagnostic, not error.
	var bare: Node = Node.new()
	assert_eq(NetStateHandle.new(bare).membership(), 0, "no get_membership on the state lane reports every world")
	assert_eq(NetRollbackHandle.new(bare).membership(), 0, "and the same on the rollback lane")
	bare.free()

func test_entity_id_forwards_the_token_unmodified() -> void:
	# The id is an FNV hash read as a signed int, so it is routinely NEGATIVE and must survive the handle
	# untouched -- `Net.set_peer_anchor_entity()` casts it straight back to the unsigned id the registry keys on.
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	stub.id = -6917529027641081856
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_eq(handle.entity_id(), -6917529027641081856, "a negative token is not clamped or re-signed")
	stub.free()

func test_entity_id_is_zero_on_a_backend_that_cannot_answer() -> void:
	# Same binary-mismatch rule as membership(), and the failure it prevents is worse: 0 is what
	# `Net.set_peer_anchor_entity()` reads as a RETRACTION, so an unanswerable id declares nothing rather than
	# anchoring a peer on entity zero.
	var bare: Node = Node.new()
	assert_eq(NetRollbackHandle.new(bare).entity_id(), 0, "no get_entity_id on the rollback lane reports 0")
	assert_eq(NetStateHandle.new(bare).entity_id(), 0, "and the same on the state lane")
	bare.free()

func test_inert_handles_report_no_entity_id() -> void:
	assert_eq(NetRollbackHandle.new(null).entity_id(), 0, "an inert rollback handle names no entity")
	assert_eq(NetStateHandle.new(null).entity_id(), 0, "an inert state handle names no entity")
