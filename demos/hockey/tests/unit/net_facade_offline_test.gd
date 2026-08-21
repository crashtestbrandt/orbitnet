extends UnitTest
## The OFFLINE contract of the `Net` facade, as this demo depends on it.
##
## The facade's promise is that a game wires its netcode UNCONDITIONALLY and, with no session, gets inert
## handles and no sockets. That is what lets `just hockey` run the whole rink with the netcode code paths
## present and the network absent, and it is what makes "does it still work offline?" a question with a
## structural answer rather than a testing burden.
##
## The demo leans on it harder than the RTS one does, because EVERYTHING here is on the rollback lane: offline
## there is no tick loop at all, so RinkDirector's accumulator calls the same advance() the backend would.
## If any assertion here fails, `just hockey` has quietly started depending on a session.

func test_the_facade_boots_offline() -> void:
	assert_eq(Net.current_mode(), Net.Mode.OFFLINE, "a process that never started a session is OFFLINE")
	assert_true(Net.is_offline(), "and says so")
	assert_false(Net.is_server(), "it is not a server")
	assert_false(Net.is_client(), "and not a client")

func test_the_clock_is_inert_offline() -> void:
	assert_eq(Net.current_tick(), 0, "no tick loop runs")
	assert_eq(Net.rollback_tick(), 0, "and there is no rollback in progress")
	assert_almost_eq(Net.net_tick_dt(), 0.0, 0.0001, "there is no tick duration")
	assert_false(Net.is_decoupled(), "decoupling is meaningless without a loop")

func test_net_tick_dt_is_zero_offline_so_callers_must_have_a_fallback() -> void:
	# Stated as its own case because three places in this demo depend on it -- PuckView, the bench subject and
	# the HUD all ask for the tick length every frame, and multiplying a velocity by zero would make every
	# offline frame look like a correction of exactly one tick of travel.
	assert_almost_eq(Net.net_tick_dt(), 0.0, 0.0001,
		"a caller that needs seconds per tick offline has to supply its own")
	assert_eq(Net.effective_tickrate(), 0, "and the rate reads zero for the same reason")

func test_every_lane_hands_back_an_inert_handle() -> void:
	var probe: Node3D = Node3D.new()
	probe.name = "OfflineProbe"

	var state: NetStateHandle = Net.make_state(probe)
	assert_false(state.is_active(), "the state lane is inert offline")
	state.add_state(probe, "position")     # must not crash, must do nothing
	state.process_settings()

	var rollback: NetRollbackHandle = Net.make_rollback(probe)
	assert_false(rollback.is_active(), "and the rollback lane")
	assert_false(rollback.is_predicting(), "an inert rollback handle never claims to be mispredicting")

	probe.free()

func test_registering_an_inputless_rollback_body_offline_is_a_no_op() -> void:
	# The puck's own registration shape: an EMPTY input list, the body as its own input node, predicted on
	# every peer. Offline it must attach nothing at all.
	var puck: Node3D = Node3D.new()
	puck.name = "OfflinePuck"
	var state_props: Array[String] = ["position"]
	var input_props: Array[String] = []
	var handle: NetRollbackHandle = Net.register_rollback_body(puck, puck, state_props, input_props, true)
	assert_false(handle.is_active(), "no synchronizer is created offline")
	assert_eq(puck.get_child_count(), 0, "and no backend node is attached to the body")
	puck.free()

func test_the_memo_ring_falls_through_offline() -> void:
	# The puck consumes a serve request through the memo ring, so that a resim reproduces the decision the
	# fresh pass made. Offline no tick is ever replayed, and the memo must hand back the caller's own
	# fallback rather than a recorded zero.
	var handle: NetRollbackHandle = Net.make_rollback(null)
	handle.memo_set(10, 1, 99)
	assert_eq(handle.memo_get(10, 1, 7), 7,
		"an inert memo returns the caller's fallback, so game code reads its own live value offline")

func test_the_remote_resim_lever_round_trips() -> void:
	# HockeyNet turns it ON at session start, which is a deliberate departure from the facade default, and the
	# teardown puts it back. Both halves have to be reachable with no session for that to be safe.
	var original: bool = Net.remote_resim()
	Net.set_remote_resim(true)
	assert_true(Net.remote_resim(), "the lever reads back what it was set to")
	Net.set_remote_resim(false)
	assert_false(Net.remote_resim(), "in both directions")
	Net.set_remote_resim(original)

func test_mode_names_are_stable() -> void:
	# Printed into logs and grepped by smoke scripts.
	assert_eq(Net.mode_name(Net.Mode.OFFLINE), "offline", "offline")
	assert_eq(Net.mode_name(Net.Mode.CLIENT), "client", "client")
	assert_eq(Net.mode_name(Net.Mode.SERVER), "server", "server")
	assert_eq(Net.mode_name(Net.Mode.HOST), "host", "host")

func test_the_transport_factory_reports_a_concrete_kind() -> void:
	# preferred_kind() describes the BUILD, not the session, so it is never OFFLINE even here.
	assert_true(NetTransport.preferred_kind() != NetTransport.Kind.OFFLINE,
		"a build always prefers some real transport")
	assert_eq(NetTransport.kind_name(NetTransport.Kind.ENET), "enet", "enet")
