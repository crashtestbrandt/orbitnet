extends UnitTest
## NetTransport: the one place that names a concrete transport.
##
## Every assertion here is about the FACTORY's contract rather than about sockets: which transport a build
## prefers, what the names are, and when a host session is advertised. That contract is what lets the rest of a
## game -- and both demos -- depend only on the resulting MultiplayerPeer. The advertising cases open a real ENet
## host on port 0 (the OS picks a free port) and close it again.

func test_a_build_always_prefers_a_real_transport() -> void:
	# preferred_kind() describes the BUILD, not the current session, so it is never OFFLINE. A caller that
	# wants to know whether a session is live asks Net, not this.
	assert_true(NetTransport.preferred_kind() != NetTransport.Kind.OFFLINE,
		"the factory always names a transport it could use")

func test_a_non_steam_build_prefers_enet() -> void:
	# The harness is not exported with the Steam preset, so the `steam` feature tag is absent and the ENET
	# arm is taken -- which means no Steamworks symbol is ever looked up. That is the property that lets this
	# project lint and run on a machine with no GodotSteam installed at all.
	assert_false(OS.has_feature("steam"), "the harness is not a Steam build")
	assert_eq(NetTransport.preferred_kind(), NetTransport.Kind.ENET, "so it prefers native ENet")
	assert_eq(NetTransport.preferred_kind_name(), "enet", "and says so by name")

func test_kind_names_are_stable() -> void:
	# These strings end up in logs, in HUDs, and in probe assertions. Renaming one is a breaking change to
	# every harness that greps for it, so they are pinned here.
	assert_eq(NetTransport.kind_name(NetTransport.Kind.OFFLINE), "offline", "offline")
	assert_eq(NetTransport.kind_name(NetTransport.Kind.ENET), "enet", "enet")
	assert_eq(NetTransport.kind_name(NetTransport.Kind.STEAM), "steam", "steam")

func test_the_default_port_and_client_cap_are_sane() -> void:
	assert_true(NetTransport.DEFAULT_PORT > 1024, "the default port is outside the privileged range")
	assert_true(NetTransport.DEFAULT_PORT < 65536, "and is a valid UDP port")
	assert_true(NetTransport.DEFAULT_MAX_CLIENTS >= 2, "a default session can hold at least two players")

func test_a_session_peer_outlasts_a_stalled_frame_but_a_dead_one_still_goes() -> void:
	# The floor is what a host's own worst synchronous frame is measured against, so it has to be longer than
	# ENet's 5 s default by a margin. The ceiling is ENet's own: this keeps a stalled peer, not a dead one.
	assert_true(NetTransport.SESSION_TIMEOUT_MIN_MS > 5000, "the floor is longer than ENet's own default")
	assert_true(NetTransport.SESSION_TIMEOUT_MIN_MS >= 15000, "and long enough to cover a world-build hitch")
	assert_true(NetTransport.SESSION_TIMEOUT_MAX_MS > NetTransport.SESSION_TIMEOUT_MIN_MS,
		"a peer that is really gone still goes, at the ceiling")
	assert_true(NetTransport.SESSION_TIMEOUT_LIMIT > 0, "the retry count before the floor applies is ENet's own")

func test_holding_a_peer_through_a_hitch_is_a_no_op_without_an_open_enet_peer() -> void:
	# The call site is a game's `peer_connected` handler, which runs on every transport. Steam, offline and a peer
	# that failed to open all reach it, and none of them may raise. The count is how many connections took the floor.
	assert_eq(NetTransport.hold_through_hitches(null), 0, "no peer at all")
	assert_eq(NetTransport.hold_through_hitches(null, 2), 0, "...nor for one id")
	assert_eq(NetTransport.hold_through_hitches(OfflineMultiplayerPeer.new()), 0, "an offline peer holds nothing")
	assert_eq(NetTransport.hold_through_hitches(ENetMultiplayerPeer.new()), 0, "...nor one that never opened")

func test_an_unknown_peer_id_holds_nothing_rather_than_everything() -> void:
	# THE FAILURE THAT MATTERS: asking for one connection and getting all of them. The id comes from a
	# `peer_connected` handler and the peer can be gone by the time it runs, so an id that resolves to nothing
	# must set nothing -- the count is what says which of the two happened.
	var peer: ENetMultiplayerPeer = ENetMultiplayerPeer.new()
	assert_eq(peer.create_server(0, 4), OK, "a host opens")   # port 0: the OS picks a free one
	assert_eq(NetTransport.hold_through_hitches(peer, 7), 0, "an id that joined nothing holds nothing")
	assert_eq(NetTransport.hold_through_hitches(peer), 0, "and the host's own connection list is empty")
	assert_eq(peer.get_connection_status(), MultiplayerPeer.CONNECTION_CONNECTED, "the host is still open")
	peer.close()

func test_a_live_connection_takes_the_floor_and_a_stranger_id_takes_nothing() -> void:
	# The distinction only shows once a connection EXISTS: with none open, holding one peer and holding them all
	# both count zero. So this opens a host and a client on the loopback and polls them into a handshake, with a
	# bounded budget -- no scene tree, no timers. A machine with no loopback UDP stands the case down rather than
	# failing; the guarantee it covers is structural either way.
	var server: ENetMultiplayerPeer = ENetMultiplayerPeer.new()
	assert_eq(server.create_server(0, 4), OK, "a host opens")   # port 0: the OS picks a free one
	var client: ENetMultiplayerPeer = ENetMultiplayerPeer.new()
	assert_eq(client.create_client("127.0.0.1", server.host.get_local_port()), OK, "a client opens")
	var joined: Array[int] = []
	server.peer_connected.connect(func(id: int) -> void: joined.push_back(id))
	for _i: int in 400:
		server.poll()
		client.poll()
		if not joined.is_empty():
			break
		OS.delay_msec(2)
	if not joined.is_empty():
		var id: int = joined[0]
		assert_eq(NetTransport.hold_through_hitches(server, id), 1, "the connection that joined takes the floor")
		assert_eq(NetTransport.hold_through_hitches(server, id + 1), 0,
			"an id that named no connection takes nothing, rather than every connection")
		assert_eq(NetTransport.hold_through_hitches(server), 1, "and every open connection is the one")
	client.close()
	server.close()

func test_a_host_opened_unadvertised_is_published_only_when_asked() -> void:
	# A game that builds its world after setting the peer opens the host unadvertised and publishes it once the
	# world exists. ENet publishes nothing, but records the same flags, which is what lets this run without Steam.
	# Port 0: the OS picks a free one, so the suite never collides with a running session.
	NetTransport.release_session()
	assert_false(NetTransport.is_session_advertised(), "no host session, nothing advertised")
	NetTransport.advertise_session()
	assert_false(NetTransport.is_session_advertised(), "advertising with no host session open does nothing")
	var peer: MultiplayerPeer = NetTransport.create_server(0, 2, false, false)
	assert_true(peer != null, "the host peer opens")
	assert_false(NetTransport.is_session_advertised(), "opened unadvertised, it is not advertised")
	NetTransport.advertise_session()
	assert_true(NetTransport.is_session_advertised(), "advertise_session publishes it")
	NetTransport.advertise_session()
	assert_true(NetTransport.is_session_advertised(), "a second call changes nothing")
	NetTransport.release_session()
	assert_false(NetTransport.is_session_advertised(), "release_session withdraws it")
	NetTransport.advertise_session()
	assert_false(NetTransport.is_session_advertised(), "and a released session cannot be advertised again")
	if peer != null:
		peer.close()

func test_a_host_opened_by_default_is_advertised_at_once() -> void:
	NetTransport.release_session()
	var peer: MultiplayerPeer = NetTransport.create_server(0, 2)
	assert_true(peer != null, "the host peer opens")
	assert_true(NetTransport.is_session_advertised(), "the default publishes as it opens, as it always has")
	NetTransport.release_session()
	if peer != null:
		peer.close()

func test_a_local_name_override_round_trips() -> void:
	# The name pipeline has to be exercisable with no Steam persona -- offline, in CI, and in a probe. That is
	# what the override exists for, and this is the case that proves the pipeline works without a platform.
	NetTransport.set_local_display_name("probe-player")
	assert_eq(NetTransport.local_display_name(), "probe-player", "the override wins")
	assert_eq(NetTransport.local_display_name_override(), "probe-player", "and echoes back for the cvar")
	NetTransport.set_local_display_name("   spaced   ")
	assert_eq(NetTransport.local_display_name(), "spaced", "surrounding whitespace is stripped at the seam")
	NetTransport.set_local_display_name("")
	assert_eq(NetTransport.local_display_name_override(), "", "clearing it empties the override")
	assert_eq(NetTransport.local_display_name(), "",
		"and on a non-Steam build the transport has no name of its own, so the roster falls back to a generic")
