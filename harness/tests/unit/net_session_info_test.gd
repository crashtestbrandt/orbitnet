extends UnitTest
## Scene-free coverage for the join browser's session row model ([NetSessionInfo]): the pure summary /
## fallback / joinable / room logic the [SessionMenu] browser renders. No scene tree, no Steam -- these rows only
## ever carry already-resolved plain data.

func test_make_normalizes_fields() -> void:
	var info: NetSessionInfo = NetSessionInfo.make(76561190000000001, "  Ada  ", 3, 8, true)
	assert_eq(info.host_id, 76561190000000001, "host id preserved")
	assert_eq(info.owner_name, "Ada", "owner name trimmed")
	assert_eq(info.players, 3, "player count preserved")
	assert_eq(info.max_players, 8, "max players preserved")
	assert_true(info.friends_only, "friends-only flag preserved")

func test_make_clamps_negative_counts() -> void:
	var info: NetSessionInfo = NetSessionInfo.make(10, "Ada", -2, -5, false)
	assert_eq(info.players, 0, "negative player count clamps to 0")
	assert_eq(info.max_players, 0, "negative max clamps to 0")

func test_connect_target_and_joinable() -> void:
	var real: NetSessionInfo = NetSessionInfo.make(76561190000000001, "Ada", 1, 8, false)
	assert_eq(real.connect_target(), "76561190000000001", "connect target is the host id string")
	assert_true(real.is_joinable(), "a row with a host id is joinable")
	var degenerate: NetSessionInfo = NetSessionInfo.make(0, "", 0, 0, false)
	assert_eq(degenerate.connect_target(), "", "no host id -> empty connect target")
	assert_false(degenerate.is_joinable(), "no host id -> not joinable")

func test_display_owner_falls_back() -> void:
	assert_eq(NetSessionInfo.make(10, "Ada", 1, 8, false).display_owner(), "Ada", "named owner shows verbatim")
	assert_eq(NetSessionInfo.make(10, "   ", 1, 8, false).display_owner(), "Unknown host", "blank owner falls back")

func test_has_room() -> void:
	assert_true(NetSessionInfo.make(10, "Ada", 3, 8, false).has_room(), "3/8 has room")
	assert_false(NetSessionInfo.make(10, "Ada", 8, 8, false).has_room(), "8/8 is full")
	assert_true(NetSessionInfo.make(10, "Ada", 99, 0, false).has_room(), "no cap advertised counts as room")

func test_summary_layout() -> void:
	assert_eq(NetSessionInfo.make(10, "Ada", 3, 8, false).summary(), "Ada  (3/8)", "plain public session")
	assert_eq(NetSessionInfo.make(10, "Ada", 2, 4, true).summary(), "Ada  (2/4)  · friends", "friends-only tagged")
	assert_eq(NetSessionInfo.make(10, "Ada", 8, 8, false).summary(), "Ada  (8/8)  · FULL", "full session marked")
	assert_eq(NetSessionInfo.make(10, "", 1, 6, false).summary(), "Unknown host  (1/6)", "blank owner falls back in summary")
	assert_eq(NetSessionInfo.make(10, "Ada", 2, 0, false).summary(), "Ada  (2)", "no cap -> bare count")
