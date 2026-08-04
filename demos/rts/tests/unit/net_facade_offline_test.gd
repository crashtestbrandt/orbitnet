extends UnitTest
## The OFFLINE contract of the `Net` facade -- the gate on the zero-networking single-player launch.
##
## The facade's promise is that a game wires its netcode UNCONDITIONALLY and, with no session, gets inert
## handles and no sockets. That is what lets `just rts` run a full 96-unit RTS with the netcode code paths
## present and the network absent, and it is what makes "does it still work offline?" a question with a
## structural answer rather than a testing burden.
##
## If any assertion here fails, offline single player has quietly started depending on a session.

func test_the_facade_boots_offline() -> void:
	assert_eq(Net.current_mode(), Net.Mode.OFFLINE, "a process that never started a session is OFFLINE")
	assert_true(Net.is_offline(), "and says so")
	assert_false(Net.is_server(), "it is not a server")
	assert_false(Net.is_client(), "and not a client")

func test_the_clock_is_inert_offline() -> void:
	assert_eq(Net.current_tick(), 0, "no tick loop runs")
	assert_almost_eq(Net.current_time(), 0.0, 0.0001, "so no network time accrues")
	assert_eq(Net.rollback_tick(), 0, "and there is no rollback in progress")
	assert_almost_eq(Net.net_tick_factor(), 1.0, 0.0001,
		"the sub-tick factor is pinned at 1.0, so a body that interpolates by it renders its live pose")
	assert_almost_eq(Net.net_tick_dt(), 0.0, 0.0001, "and there is no tick duration")
	assert_false(Net.is_decoupled(), "decoupling is meaningless without a loop")

func test_metrics_are_zeroed_rather_than_absent() -> void:
	# A HUD reads these every frame. Returning zeros rather than an empty dictionary means the offline HUD
	# needs no special case -- which is the same argument as the inert handles, applied to diagnostics.
	var clock: Dictionary[String, float] = Net.clock_metrics()
	assert_almost_eq(clock["rtt_ms"], 0.0, 0.0001, "no round trip offline")
	assert_almost_eq(clock["stretch"], 1.0, 0.0001, "and the clock is not stretched")
	var perf: Dictionary[String, float] = Net.perf_metrics()
	assert_almost_eq(perf["resim_ticks"], 0.0, 0.0001, "nothing is resimulated")
	assert_eq(Net.perf_summary(), "offline (no rollback loop)", "and the summary says so plainly")

func test_every_lane_hands_back_an_inert_handle() -> void:
	var probe: Node3D = Node3D.new()
	probe.name = "OfflineProbe"

	var state: NetStateHandle = Net.make_state(probe)
	assert_false(state.is_active(), "the state lane is inert offline")
	state.add_state(probe, "position")     # must not crash, must do nothing
	state.process_settings()

	var interp: NetInterpolatorHandle = Net.make_interpolator(probe)
	assert_false(interp.is_active(), "so is the interpolator")
	assert_false(interp.is_enabled(), "which reports itself disabled rather than erroring")
	interp.add_property(probe, "position")
	interp.process_settings()
	interp.teleport()
	interp.set_enabled(true)

	var rollback: NetRollbackHandle = Net.make_rollback(probe)
	assert_false(rollback.is_active(), "and the rollback lane")
	assert_false(rollback.is_predicting(), "an inert rollback handle never claims to be mispredicting")
	assert_eq(rollback.get_last_known_state(), -1, "and reports no known authoritative tick")

	probe.free()

func test_an_inert_interpolator_is_returned_not_null() -> void:
	# The reason NetInterpolatorHandle exists. make_interpolator() used to return a bare `Node`, which was
	# null offline -- so every caller needed a null check, and reaching add_property() on the non-null case
	# meant an untyped call on a Node, which is a compile ERROR under unsafe_method_access. A typed handle
	# that is merely INERT removes both problems.
	var handle: NetInterpolatorHandle = Net.make_interpolator(null)
	assert_true(handle != null, "a null root yields an inert handle, never null")
	assert_false(handle.is_active(), "and it is inert")

func test_registering_a_rollback_body_offline_is_a_no_op() -> void:
	var body: Node3D = Node3D.new()
	var input: Node = Node.new()
	input.name = "Input"
	body.add_child(input)
	var state_props: Array[String] = ["position"]
	var input_props: Array[String] = ["name"]
	var handle: NetRollbackHandle = Net.register_rollback_body(body, input, state_props, input_props, false)
	assert_false(handle.is_active(), "no synchronizer is created offline")
	assert_eq(body.get_child_count(), 1, "and no backend node is attached to the body")
	body.free()

func test_the_memo_ring_falls_through_offline() -> void:
	var handle: NetRollbackHandle = Net.make_rollback(null)
	handle.memo_set(10, 1, 99)
	assert_eq(handle.memo_get(10, 1, 7), 7,
		"an inert memo returns the caller's fallback, so game code reads its own live value offline")

func test_mode_names_are_stable() -> void:
	# Printed into logs and asserted on by probes and smoke scripts.
	assert_eq(Net.mode_name(Net.Mode.OFFLINE), "offline", "offline")
	assert_eq(Net.mode_name(Net.Mode.CLIENT), "client", "client")
	assert_eq(Net.mode_name(Net.Mode.SERVER), "server", "server")
	assert_eq(Net.mode_name(Net.Mode.HOST), "host", "host")

func test_the_transport_factory_reports_a_concrete_kind() -> void:
	# preferred_kind() describes the BUILD, not the session, so it is never OFFLINE even here.
	assert_true(NetTransport.preferred_kind() != NetTransport.Kind.OFFLINE,
		"a build always prefers some real transport")
	assert_eq(NetTransport.kind_name(NetTransport.Kind.ENET), "enet", "enet")
	assert_eq(NetTransport.kind_name(NetTransport.Kind.STEAM), "steam", "steam")
	assert_eq(NetTransport.kind_name(NetTransport.Kind.OFFLINE), "offline", "offline")
