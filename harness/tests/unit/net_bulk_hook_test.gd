extends UnitTest
## Scene-free coverage for the bulk marshalling hook declarations on the two handles
## ([NetRollbackHandle], [NetStateHandle]).
##
## A bulk hook replaces the per-property walk with one script-boundary crossing per lane per tick. The handles
## are thin forwarders, so what is worth testing here is the three places they decide something:
##
## - The declaration reaches the synchronizer under the right export name, per lane and per direction.
## - The order lists and the "is it on" flags DEGRADE on a backend too old to answer, rather than aborting the
##   caller. The cdylib is committed separately from this GDScript, so new addon code legitimately runs against
##   an older binary -- and a `PackedStringArray` local assigned `null` is a hard runtime error, not a warning.
## - The lane ordinals the two handles publish AGREE, because a game is meant to point one method at both.
##
## The backend synchronizers are stubbed by plain Nodes carrying the same property and method names. The
## handles hold their synchronizer as an opaque Node and reach it by name, so a stub is a faithful stand-in and
## this suite needs no cdylib, no scene tree and no session.

## Stands in for the backend rollback synchronizer: the two exports the handle writes, and the four queries it
## reads. The order lists mirror a body with two state props, one cosmetic prop and one input prop -- so the
## state lane's restore order is SHORTER than its capture order by exactly the cosmetic entry.
class RollbackSyncStub extends Node:
	var bulk_capture_method: String = ""
	var bulk_restore_method: String = ""

	func bulk_capture_order(lane: int) -> PackedStringArray:
		var out: PackedStringArray = PackedStringArray()
		if bulk_capture_method.is_empty():
			return out
		if lane == 1:
			out.push_back("nin_move")
			return out
		out.push_back("net_pos")
		out.push_back("net_orient")
		out.push_back("net_rcs_lin")
		return out

	func bulk_restore_order(lane: int) -> PackedStringArray:
		var out: PackedStringArray = PackedStringArray()
		if bulk_restore_method.is_empty():
			return out
		if lane == 1:
			out.push_back("nin_move")
			return out
		out.push_back("net_pos")
		out.push_back("net_orient")
		return out

	func uses_bulk_capture(lane: int) -> bool:
		return not bulk_capture_method.is_empty() and lane >= 0

	func uses_bulk_restore(lane: int) -> bool:
		return not bulk_restore_method.is_empty() and lane >= 0

## Stands in for the backend state synchronizer: one lane, capture only.
class StateSyncStub extends Node:
	var bulk_capture_method: String = ""

	func bulk_capture_order(lane: int) -> PackedStringArray:
		var out: PackedStringArray = PackedStringArray()
		if bulk_capture_method.is_empty() or lane != 0:
			return out
		out.push_back("hp")
		out.push_back("team")
		return out

	func uses_bulk_capture(lane: int) -> bool:
		return not bulk_capture_method.is_empty() and lane == 0

## Stands in for a backend built before the hooks existed: neither export nor any of the queries.
class OldSyncStub extends Node:
	var membership_property: String = ""

func test_a_rollback_declaration_reaches_both_exports() -> void:
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	assert_false(handle.uses_bulk_capture(NetRollbackHandle.LANE_STATE), "a body starts on the walk")
	handle.set_bulk_capture("_net_capture")
	handle.set_bulk_restore("_net_restore")
	assert_eq(stub.bulk_capture_method, "_net_capture", "the capture name reaches the synchronizer verbatim")
	assert_eq(stub.bulk_restore_method, "_net_restore", "and so does the restore name")
	assert_true(handle.uses_bulk_capture(NetRollbackHandle.LANE_STATE), "the state lane now captures in bulk")
	assert_true(handle.uses_bulk_capture(NetRollbackHandle.LANE_INPUT), "and so does the input lane")
	assert_true(handle.uses_bulk_restore(NetRollbackHandle.LANE_STATE), "and both restore in bulk")
	stub.free()

## The case the two order lists exist for: a cosmetic prop is captured and replicated but never restored, so a
## hook written against the capture order would read one slot too many on the way back.
func test_the_restore_order_drops_the_cosmetic_entries() -> void:
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_bulk_capture("_net_capture")
	handle.set_bulk_restore("_net_restore")
	var captured: PackedStringArray = handle.bulk_capture_order(NetRollbackHandle.LANE_STATE)
	var restored: PackedStringArray = handle.bulk_restore_order(NetRollbackHandle.LANE_STATE)
	assert_eq(captured.size(), 3, "the state lane captures every entry, cosmetics included")
	assert_eq(restored.size(), 2, "and restores every entry but the cosmetic one")
	assert_eq(restored[0], "net_pos", "in the same relative order")
	assert_eq(handle.bulk_capture_order(NetRollbackHandle.LANE_INPUT).size(), 1, "the input lane is its own list")
	stub.free()

func test_the_input_lane_captures_and_restores_the_same_entries() -> void:
	var stub: RollbackSyncStub = RollbackSyncStub.new()
	var handle: NetRollbackHandle = NetRollbackHandle.new(stub)
	handle.set_bulk_capture("_net_capture")
	handle.set_bulk_restore("_net_restore")
	# Every input entry is restored -- there is no cosmetic role on this lane -- so the two lists are equal.
	assert_eq(
		handle.bulk_capture_order(NetRollbackHandle.LANE_INPUT),
		handle.bulk_restore_order(NetRollbackHandle.LANE_INPUT),
		"nothing on the input lane is captured without being restored"
	)
	stub.free()

func test_a_state_channel_declares_capture_only() -> void:
	var stub: StateSyncStub = StateSyncStub.new()
	var handle: NetStateHandle = NetStateHandle.new(stub)
	assert_false(handle.uses_bulk_capture(), "a channel starts on the walk")
	handle.set_bulk_capture("_net_capture")
	assert_eq(stub.bulk_capture_method, "_net_capture", "the name reaches the synchronizer verbatim")
	assert_true(handle.uses_bulk_capture(), "and the channel captures in bulk")
	assert_eq(handle.bulk_capture_order().size(), 2, "the order covers every replicated entry")
	stub.free()

## A binary mismatch must degrade a declaration, never abort the game -- the rule every other
## backwards-compatibility path in the facade follows. An unanswered order query returning `null` would be a
## hard runtime error at the typed local it is assigned to.
func test_an_old_backend_reports_the_walk_rather_than_failing() -> void:
	var stub: OldSyncStub = OldSyncStub.new()
	var rollback: NetRollbackHandle = NetRollbackHandle.new(stub)
	rollback.set_bulk_capture("_net_capture")
	assert_false(rollback.uses_bulk_capture(NetRollbackHandle.LANE_STATE), "no query means no hook")
	assert_false(rollback.uses_bulk_restore(NetRollbackHandle.LANE_STATE), "on either direction")
	assert_eq(rollback.bulk_capture_order(NetRollbackHandle.LANE_STATE).size(), 0, "and an empty order")
	assert_eq(rollback.bulk_restore_order(NetRollbackHandle.LANE_INPUT).size(), 0, "on every lane")

	var state: NetStateHandle = NetStateHandle.new(stub)
	state.set_bulk_capture("_net_capture")
	assert_false(state.uses_bulk_capture(), "the state channel degrades the same way")
	assert_eq(state.bulk_capture_order().size(), 0, "with an empty order")
	stub.free()

## OFFLINE: the handle wraps no synchronizer, and every method no-ops so the game wires one code path.
func test_an_inert_handle_declares_nothing() -> void:
	var rollback: NetRollbackHandle = NetRollbackHandle.new(null)
	rollback.set_bulk_capture("_net_capture")
	rollback.set_bulk_restore("_net_restore")
	assert_false(rollback.uses_bulk_capture(NetRollbackHandle.LANE_STATE), "an inert handle has no hook")
	assert_eq(rollback.bulk_restore_order(NetRollbackHandle.LANE_STATE).size(), 0, "and no order to publish")

	var state: NetStateHandle = NetStateHandle.new(null)
	state.set_bulk_capture("_net_capture")
	assert_false(state.uses_bulk_capture(), "nor does an inert channel")

## One game method is meant to serve a body's two lanes AND a channel's one, so the ordinals have to agree.
func test_the_two_handles_agree_on_the_lane_ordinals() -> void:
	assert_eq(NetStateHandle.LANE_STATE, NetRollbackHandle.LANE_STATE, "the state lane is lane 0 on both")
	assert_true(NetRollbackHandle.LANE_INPUT != NetRollbackHandle.LANE_STATE, "and the input lane is its own")
