extends UnitTest
## NetTransport: the one place that names a concrete transport.
##
## Every assertion here is about the FACTORY's contract rather than about sockets: which transport a build
## prefers, and what the names are. That contract is what lets the rest of a game -- and both demos -- depend
## only on the resulting MultiplayerPeer.

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
