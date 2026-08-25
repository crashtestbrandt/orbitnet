extends UnitTest
## Scene-free coverage for the interest-membership declarations on the two handles ([NetStateHandle],
## [NetRollbackHandle]), and for the ANCHOR READ-BACKS on the `Net` facade.
##
## What is worth testing on the handles is the RELEVANCY PROMOTION RULE and nothing else. Both handles are thin
## forwarders, but `NetStateHandle.set_membership()` decides a value: a channel that has already declared an
## anchor must stay ANCHORED (culled by distance WITHIN its world), while one still on the default ALWAYS is
## promoted to MEMBERSHIP (one world, no distance test). Getting that backwards silently un-culls every
## anchored channel, which is a bandwidth regression no assertion elsewhere would catch.
##
## The backend synchronizer is stubbed by a plain Node carrying the same property names. The handles hold their
## synchronizer as an opaque Node and reach it by property/method NAME, so a stub is a faithful stand-in and
## this suite needs no cdylib, no scene tree and no session.
##
## The second half covers `Net.peer_anchor()`, `Net.seat_anchor()` and the unanchored policy, and its own header
## states what a scene-free suite can and cannot reach there.

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

# --- the anchor read-backs on the facade -----------------------------------------------------------

# WHAT THIS HALF CAN AND CANNOT REACH. `Net.peer_anchor()` and `Net.seat_anchor()` report what the SERVER's
# interest pass resolved, and a unit suite has no session, no peers and no interest pass. So what is pinned here
# is the FACADE CONTRACT, which is the half that breaks silently:
#
# - The OFFLINE / old-binary answer is a FULLY KEYED dictionary with `stale` true. A caller indexes these keys
#   directly, and a missing key is a `nil` reaching a typed local -- an abort, not a fallback. Every other
#   degrade path in this facade answers a zero of the right shape and these must too.
# - `stale` is true in that answer, not false. `false` would claim "centred at the origin, in world 0" for every
#   peer in a session that never ran the pass, which is a description of a filter that is not running.
# - The enum NUMBERS are a contract with the backend, which stores and returns the raw int.
# - A number outside `UnanchoredPolicy` clamps to OPEN, the direction that withholds nothing from anybody.
#
# The suite runs against the committed cdylib, which is refreshed only at a release tag -- so the missing-method
# path is the ordinary checkout rather than a hypothetical, and these calls have no business erroring there.

const ANCHOR_KEYS: Array[String] = [
	"source", "viewpoints", "membership", "located", "centre", "open", "ambiguous", "stale",
]

func test_the_anchor_source_enum_matches_the_backend_numbering() -> void:
	# The backend returns the raw int in the `source` key, so the two enums are one contract. Drift here does not
	# fail anything loudly -- it silently renames what a peer's anchor came from.
	assert_eq(int(Net.AnchorSource.NONE), 0, "NONE is 0")
	assert_eq(int(Net.AnchorSource.INFERRED), 1, "INFERRED is 1")
	assert_eq(int(Net.AnchorSource.FIXED), 2, "FIXED is 2")
	assert_eq(int(Net.AnchorSource.ENTITY), 3, "ENTITY is 3")

func test_the_unanchored_policy_enum_matches_the_backend_numbering() -> void:
	# The facade writes `int(policy)` into a backend call and reads the same number back.
	assert_eq(int(Net.UnanchoredPolicy.OPEN), 0, "OPEN is 0")
	assert_eq(int(Net.UnanchoredPolicy.CLOSED), 1, "CLOSED is 1")

func test_an_unanswerable_peer_anchor_is_fully_keyed_and_stale() -> void:
	# The degrade path, and the one this suite actually runs. Every key present, every value a zero of the right
	# TYPE -- `centre` a Vector3 rather than a 0 -- so a caller's typed local takes it without a check.
	var info: Dictionary[String, Variant] = Net.peer_anchor(4)
	for key: String in ANCHOR_KEYS:
		assert_true(info.has(key), "key %s is present even with nothing to report" % key)
	assert_eq(info.size(), ANCHOR_KEYS.size(), "and no key beyond the documented set")
	var stale: bool = info["stale"]
	assert_true(stale, "no interest pass has run, and that is the gate on every other key")
	assert_eq(info["source"], int(Net.AnchorSource.NONE), "no anchor came from anywhere")
	assert_eq(info["viewpoints"], 0, "no viewpoint was resolved")
	assert_eq(info["membership"], 0, "and no world is in effect")
	var centre: Vector3 = info["centre"]
	assert_eq(centre, Vector3.ZERO, "an unlocated centre reads as the zero vector, not as a stale position")
	var located: bool = info["located"]
	assert_false(located, "nothing was located")
	var open: bool = info["open"]
	assert_false(open, "and 'culling nothing' is not the claim either -- read `stale` first")
	var ambiguous: bool = info["ambiguous"]
	assert_false(ambiguous, "no pick was made, so no pick was ambiguous")

func test_an_unanswerable_seat_anchor_is_fully_keyed() -> void:
	var info: Dictionary[String, Variant] = Net.seat_anchor(4, 1)
	assert_eq(info.size(), 3, "the seat answer is exactly its three keys")
	var centre: Vector3 = info["centre"]
	assert_eq(centre, Vector3.ZERO, "no centre")
	var located: bool = info["located"]
	assert_false(located, "and it says so rather than reporting the origin as a position")
	assert_eq(info["membership"], 0, "and no world")

func test_the_anchor_read_backs_answer_any_peer_id_without_erroring() -> void:
	# Peer ids come from the transport and a diagnostic HUD polls them; 0 and negatives are what a caller holds
	# before a connection exists. The answer is the zeroed one, not an error.
	for peer: int in [0, -1, 1, 4, 1 << 30]:
		var info: Dictionary[String, Variant] = Net.peer_anchor(peer)
		var stale: bool = info["stale"]
		assert_true(stale, "peer %d has no resolved anchor to report" % peer)
		assert_eq(Net.seat_anchor(peer, 0).size(), 3, "and its seat answer is still fully keyed")

func test_each_anchor_call_answers_a_fresh_dictionary() -> void:
	# A caller that annotates the answer for its own HUD must not be editing the next caller's.
	var first: Dictionary[String, Variant] = Net.peer_anchor(4)
	first["stale"] = false
	var second: Dictionary[String, Variant] = Net.peer_anchor(4)
	var stale: bool = second["stale"]
	assert_true(stale, "the second answer is built fresh, not handed the first one back")

func test_a_session_that_chose_nothing_leaves_every_connection_open() -> void:
	# THE DEFAULT-DRIFT GUARD. A CLOSED default would take every world away from a spectator that declares no
	# anchor, on the tick a consumer's binary was refreshed without their source changing.
	assert_eq(Net.unanchored_policy(), Net.UnanchoredPolicy.OPEN, "nothing chosen means nothing is withheld")

func test_an_unanchored_policy_outside_the_enum_clamps_to_open() -> void:
	# A stored number this build does not know must withhold NOTHING rather than select whichever member happens
	# to sit at that index. Clamped on write and again on read, so the getter reports what is in force.
	for junk: int in [2, 99, -1, -7]:
		Net.set_unanchored_policy(junk)
		assert_eq(Net.unanchored_policy(), Net.UnanchoredPolicy.OPEN, "policy %d is not a policy" % junk)
	# And OPEN is reachable by name, which is also what leaves this suite's session as it found it.
	Net.set_unanchored_policy(int(Net.UnanchoredPolicy.OPEN))
	assert_eq(Net.unanchored_policy(), Net.UnanchoredPolicy.OPEN, "OPEN is chosen the same way it is defaulted")

func test_a_per_peer_unanchored_policy_is_inert_offline() -> void:
	# Per-connection, so it needs a connection. OFFLINE there is none, and declaring one must neither error nor
	# leak into the session-wide default that a later networked session would read.
	Net.set_peer_unanchored_policy(4, int(Net.UnanchoredPolicy.CLOSED))
	Net.set_peer_unanchored_policy(-1, int(Net.UnanchoredPolicy.CLOSED))
	assert_eq(Net.unanchored_policy(), Net.UnanchoredPolicy.OPEN, "a per-peer policy is not the session default")
	var info: Dictionary[String, Variant] = Net.peer_anchor(4)
	var stale: bool = info["stale"]
	assert_true(stale, "and it seated no connection to report on")
