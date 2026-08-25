extends UnitTest
## Scene-free coverage for the render interpolator's handle ([NetInterpolatorHandle]) -- the lane that exists
## purely so a strictly-typed consumer can reach the interpolator at all.
##
## It is LOCAL: nothing here touches the wire. The interpolator rotates the declared properties' recorded
## values at each tick boundary and blends between them every frame, so what a unit suite can pin is the
## declaration surface and the two switches -- not the blend, which needs frames.
##
## Three things are worth pinning, and each is a decision rather than a forward:
##
## - **`is_enabled()` READS THE BACKEND, and reports false when inert.** The interpolator owns the answer, so
##   the handle asks it every time rather than caching the last `set_enabled()`.
## - **A DECLARATION DOES NOT RE-READ THE SET**, the same split the other two lanes take: `add_property()`
##   lands immediately, and the game owes one `process_settings()` once the set is complete.
## - **THE INERT HANDLE IS A COMPLETE NO-OP.** Offline the replicated properties are written directly and
##   stick, so a caller wires the same code path either way and every method here runs on the first frame of
##   a single-player run.
##
## The backend interpolator is stubbed by a plain Node carrying the same property and method names. The handle
## holds it as an opaque Node and reaches it by name, so a stub is a faithful stand-in and this suite needs no
## cdylib, no scene tree and no session.

## Stands in for the backend interpolator: the one export the handle writes and reads, the declaration sink,
## and counters for the two calls that must stay explicit.
class InterpStub extends Node:
	var enabled: bool = true
	var declared: PackedStringArray = PackedStringArray()
	var settings_processed: int = 0
	var teleports: int = 0

	func add_property(_node: Object, property: String) -> void:
		declared.push_back(property)

	func process_settings() -> void:
		settings_processed += 1

	func teleport() -> void:
		teleports += 1

func test_a_backed_handle_reports_itself_active() -> void:
	var stub: InterpStub = InterpStub.new()
	assert_true(NetInterpolatorHandle.new(stub).is_active(), "a real interpolator backs this handle")
	assert_false(NetInterpolatorHandle.new(null).is_active(), "and OFFLINE nothing does")
	stub.free()

func test_declarations_land_immediately_but_do_not_re_read_the_set() -> void:
	# Feed it the SAME properties the state lane replicates -- so the declaration list is as long as that
	# channel's, and re-resolving on each entry would pay for the whole list once per property at spawn.
	var stub: InterpStub = InterpStub.new()
	var handle: NetInterpolatorHandle = NetInterpolatorHandle.new(stub)
	handle.add_property(stub, "net_pos")
	handle.add_property(stub, "net_orient")
	assert_eq(stub.declared.size(), 2, "both properties reached the interpolator")
	assert_eq(stub.declared[1], "net_orient", "in the order they were declared")
	assert_eq(stub.settings_processed, 0, "and declaring re-read nothing")
	handle.process_settings()
	assert_eq(stub.settings_processed, 1, "the game owes exactly one process_settings() afterward")
	stub.free()

func test_teleport_is_forwarded_and_is_not_implied_by_anything_else() -> void:
	# A discontinuity the sim INTENDS -- a spawn, a respawn, a world rebuild -- has to say so, because the
	# interpolator cannot tell one from a large legitimate step. Without it a unit that respawns across the
	# map visibly flies there over one net tick.
	var stub: InterpStub = InterpStub.new()
	var handle: NetInterpolatorHandle = NetInterpolatorHandle.new(stub)
	handle.add_property(stub, "net_pos")
	handle.process_settings()
	assert_eq(stub.teleports, 0, "neither declaring nor processing snaps the endpoints")
	handle.teleport()
	assert_eq(stub.teleports, 1, "only the call that means it does")
	stub.free()

func test_enabled_reads_the_interpolator_rather_than_a_shadow_copy() -> void:
	# The switch is live: a demo binds it to a key to make the difference visible, and the interpolator is
	# what owns the answer. A handle caching the last write would report a state nothing is in.
	var stub: InterpStub = InterpStub.new()
	var handle: NetInterpolatorHandle = NetInterpolatorHandle.new(stub)
	assert_true(handle.is_enabled(), "an interpolator starts running")
	handle.set_enabled(false)
	assert_false(stub.enabled, "the switch reaches the interpolator")
	assert_false(handle.is_enabled(), "and is read back from it")
	stub.enabled = true
	assert_true(handle.is_enabled(), "a change made anywhere else is read back too")
	stub.free()

func test_an_inert_handle_is_a_no_op_in_every_direction() -> void:
	# OFFLINE the replicated values are simply written directly and stick, so every one of these is reached
	# on the first frame of a single-player run and none of them may error.
	var handle: NetInterpolatorHandle = NetInterpolatorHandle.new(null)
	handle.add_property(handle, "net_pos")
	handle.process_settings()
	handle.teleport()
	handle.set_enabled(true)
	assert_false(handle.is_enabled(), "an inert handle interpolates nothing, whatever it was told")
	assert_false(handle.is_active(), "and reports itself inert")
