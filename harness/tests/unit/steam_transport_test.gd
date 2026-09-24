extends UnitTest
## SteamTransport: the lobby lifecycle, the metadata rows, and the platform-code mapping, with no Steam.
##
## WHY THIS SUITE EXISTS. `addons/orbitnet/steam_transport.gd` was the addon's largest file with no coverage,
## and no probe reached it either: the harness is not a Steam build, so `OS.has_feature("steam")` is false and
## `transport_factory_test.gd` can only assert that the factory's Steam arm is NOT taken. Everything behind
## that arm -- the lobby lifecycle, the metadata rows a browser reads, the mapping from platform result codes
## onto the states the facade publishes, and every guarded degradation -- is ordinary logic that needs no
## Steam client to run.
##
## HOW IT REACHES THE FILE. `SteamTransport.use_platform_double()` points the file's two Steamworks lookups at
## a [SteamTransport.PlatformDouble] instead of at `Engine` / `ClassDB`. The double is declared INSIDE
## `steam_transport.gd` on purpose -- it has to answer the same platform method names and signals the dynamic
## calls ask for, and that file is the only one allowed to name them, so a double written here would put those
## names in a second file and break the boundary. This suite therefore names no Steamworks class, method or
## signal: it reads the double through `rows()`, `lobby_requests`, `ok_result()` and `emit_*`.
##
## WHY IT CALLS PRIVATE METHODS. The platform's callbacks (`_on_lobby_created`, `_on_lobby_match_list`,
## `_on_validate_auth_ticket_response`) ARE the entry points for most of this logic, and they are reached by
## firing the double's signals. Two pure helpers -- `_lobby_data` and `_init_ok` -- are called directly,
## because each encodes a rule with no public caller of its own.
##
## WHAT IS STILL MANUAL. `docs/steam.md` records it: persona names, lobby discovery across accounts, invite
## delivery through the real overlay and ticket validation against Valve need a real Steam build, two accounts
## and a human. This suite covers the logic AROUND those calls, never the calls themselves.
##
## Every transport here is an orphan `Node` (never `service()`, which parents itself to the scene root), so
## `_ready` never runs and nothing here is bound to a live `MultiplayerAPI`. Three paths need one and are out
## of reach: the headcount republish (it counts live peers), the ticket handoff on connect, and the promotion
## of a staged invite lobby on connect. The staging invariant itself is asserted from the other side -- see
## [method test_a_joined_invite_lobby_is_held_out_of_reach_of_the_teardown_between_join_and_connect].
##
## `SteamTransport._double` is process-global and the harness has no teardown hook, so each case installs its
## own double and clears it at the end. A case aborted by a runtime error before that clear leaves the double
## installed for whatever runs next.

const _LOBBY: int = 777          # the lobby id the double hands back from a create
const _ACCOUNT: int = 900001     # this host's own account id
const _INVITE_LOBBY: int = 8801  # a lobby somebody else hosts, reached through an invite
const _OTHER_LOBBY: int = 8802   # a second such lobby, for the supersede case
const _INVITE_HOST: int = 5551   # the account hosting _INVITE_LOBBY
const _OTHER_HOST: int = 5552    # the account hosting _OTHER_LOBBY

# --- fixtures -------------------------------------------------------------------------------------

func _platform() -> SteamTransport.PlatformDouble:
	var double: SteamTransport.PlatformDouble = SteamTransport.PlatformDouble.new()
	double.steam_id = _ACCOUNT
	double.persona = "Ada"
	return double

func _transport(double: SteamTransport.PlatformDouble) -> SteamTransport:
	SteamTransport.use_platform_double(double)
	return SteamTransport.new()

func _done(transport: SteamTransport) -> void:
	SteamTransport.clear_platform_double()
	transport.free()

# --- lobby metadata: what a host stamps ------------------------------------------------------------

func test_an_advertised_host_stamps_the_rows_a_browser_reads() -> void:
	# The browser reads a discovered lobby's METADATA rather than querying its members, so every field a row
	# renders has to be published by the host. `host_id` is the load-bearing one: a browsing peer is not a
	# member of the lobbies it lists and cannot ask who owns one, so the connect target travels as metadata.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var peer: MultiplayerPeer = transport.create_listen_host(0, 8, false, true)
	assert_true(peer != null, "the host peer opens")
	assert_eq(double.lobby_requests.size(), 1, "an advertised host asks for a lobby as it opens")
	assert_eq(double.last_lobby_cap(), 8, "carrying the player cap it was opened with")
	assert_false(double.last_lobby_is_friends_only(), "and the discoverable lobby type")
	double.emit_lobby_created(double.ok_result(), _LOBBY)
	assert_eq(double.row(_LOBBY, "game"), "orbitnet",
		"the game tag, which is what filters another app's lobbies out of the list")
	assert_eq(double.row(_LOBBY, "host_id"), str(_ACCOUNT), "the connect target, as a decimal string")
	assert_eq(double.row(_LOBBY, "owner_name"), "Ada", "the owner's display name")
	assert_eq(double.row(_LOBBY, "max"), "8", "the advertised cap")
	assert_eq(double.row(_LOBBY, "friends_only"), "0", "and the friends-only flag, as a flag rather than absent")
	assert_eq(double.published_cap(_LOBBY), 8, "the member limit is published too, not just advertised")
	assert_eq(double.rows(_LOBBY).size(), 5,
		"exactly five rows: the headcount is republished off live peer churn, which an orphan node has none of")
	assert_true(transport.can_invite(), "a host with a lobby has somewhere to invite friends into")
	assert_true(double.has_presence(), "and publishes a join string beside its own name")
	assert_true(double.presence_connect().ends_with(str(_LOBBY)), "naming the lobby to join")
	_done(transport)

func test_a_friends_only_host_asks_for_the_narrower_lobby_type() -> void:
	# A friends-only lobby is deliberately invisible to the browser -- the platform does not list one -- so
	# the toggle is only reachable through an invite. What has to hold here is that the flag survives from
	# the create call into the row a joiner reads.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport.create_listen_host(0, 2, true, true) != null, "the host peer opens")
	assert_true(double.last_lobby_is_friends_only(), "the narrower lobby type is requested")
	double.emit_lobby_created(double.ok_result(), _LOBBY)
	assert_eq(double.row(_LOBBY, "friends_only"), "1", "and the row says so")
	_done(transport)

func test_an_extra_row_reads_back_verbatim_and_an_unstamped_one_reads_empty() -> void:
	# The session-secret row. docs/steam.md's first source for `Net.set_session_secret()` is a per-lobby value
	# the host writes beside the cap and the game tag, read off the same row that carried the headcount. So
	# the property that source depends on is this one: the reader returns an arbitrary row byte-for-byte, and
	# answers "" -- never null, never a placeholder -- for a row nobody stamped.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")
	double.publish_row(21, "session_secret", "3f9c07a1b2")
	assert_eq(transport._lobby_data(21, "session_secret"), "3f9c07a1b2", "a stamped row reads back verbatim")
	assert_eq(transport._lobby_data(21, "not_a_row"), "", "a row that was never stamped reads empty")
	assert_eq(transport._lobby_data(99, "session_secret"), "", "and so does every row of an unknown lobby")
	_done(transport)

# --- lobby metadata: what a browser reads ----------------------------------------------------------

func test_discovery_turns_lobby_rows_into_platform_blind_session_records() -> void:
	# The reader's job is to hand the join browser plain data. Three lobbies cover the three shapes it meets:
	# one fully stamped, one whose host published no headcount or cap (so the live counts answer instead and
	# the owner resolves the way only a member can), and one with no resolvable host at all -- which is not
	# joinable and must not reach the browser as a clickable row.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var refreshes: Array[int] = []
	transport.sessions_updated.connect(func() -> void: refreshes.push_back(1))
	transport.request_session_list()
	assert_eq(double.list_requests, 1, "a sweep is requested")
	assert_true(transport.sessions().is_empty(), "and nothing is published until its result arrives")

	double.publish_row(11, "host_id", "5551")
	double.publish_row(11, "owner_name", "Bea")
	double.publish_row(11, "players", "3")
	double.publish_row(11, "max", "8")
	double.publish_row(11, "friends_only", "1")
	double.owners[12] = 5552                 # no host_id row: the members-only owner lookup is the fallback
	double.member_counts[12] = 2
	double.member_limits[12] = 4
	double.friend_names[5552] = "Cyd"
	double.publish_row(13, "owner_name", "ghost")   # a row with no host behind it

	double.emit_lobby_list([11, 12, 13, 0])
	assert_eq(refreshes.size(), 1, "the browser is told once per sweep")
	var rows: Array[NetSessionInfo] = transport.sessions()
	assert_eq(rows.size(), 2, "the two joinable lobbies are published and the other two are dropped")
	assert_eq(rows[0].host_id, 5551, "the connect target comes off the row")
	assert_eq(rows[0].owner_name, "Bea", "so does the owner name")
	assert_eq(rows[0].players, 3, "and the published headcount")
	assert_eq(rows[0].max_players, 8, "and the published cap")
	assert_true(rows[0].friends_only, "and the friends-only flag")
	assert_eq(rows[1].host_id, 5552, "an unstamped host id falls back to the owner lookup")
	assert_eq(rows[1].owner_name, "Cyd", "an unstamped owner name falls back to the cached display name")
	assert_eq(rows[1].players, 2, "an unpublished headcount falls back to the live member count")
	assert_eq(rows[1].max_players, 4, "and an unpublished cap to the live member limit")
	_done(transport)

func test_a_later_sweep_replaces_the_list_rather_than_growing_it() -> void:
	# The browser renders whatever the last sweep found. A lobby that went away between sweeps has to
	# disappear from the list, which it only does if each result REPLACES the set.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")
	double.publish_row(11, "host_id", "5551")
	double.publish_row(12, "host_id", "5552")
	double.emit_lobby_list([11, 12])
	assert_eq(transport.sessions().size(), 2, "two sessions are found")
	double.emit_lobby_list([12])
	var rows: Array[NetSessionInfo] = transport.sessions()
	assert_eq(rows.size(), 1, "a sweep that finds one publishes one")
	assert_eq(rows[0].host_id, 5552, "and it is the one that is still there")
	double.emit_lobby_list([])
	assert_true(transport.sessions().is_empty(), "a sweep that finds nothing empties the browser")
	_done(transport)

# --- the lifecycle of a host opened unadvertised ---------------------------------------------------

func test_a_listen_host_opened_unadvertised_publishes_nothing_until_it_is_asked() -> void:
	# A game that builds its world with the peer already set opens the host unadvertised, so no browser row
	# ever points at a host with nothing to join. The settings it opened with are stored and replayed, which
	# is the part that can silently go wrong: publishing later must not publish a different session.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport.create_listen_host(0, 4, true, false) != null, "the host peer opens")
	assert_true(double.lobby_requests.is_empty(), "nothing is published as it opens")
	assert_false(transport.can_invite(), "and there is nothing to invite anyone into yet")

	transport.advertise_session()
	assert_eq(double.lobby_requests.size(), 1, "advertise_session creates the lobby")
	assert_eq(double.last_lobby_cap(), 4, "with the cap the host opened with")
	assert_true(double.last_lobby_is_friends_only(), "and the friends-only choice it opened with")
	transport.advertise_session()
	assert_eq(double.lobby_requests.size(), 1, "a second call publishes nothing more")

	double.emit_lobby_created(double.ok_result(), _LOBBY)
	assert_true(transport.can_invite(), "the created lobby is invitable")
	transport.open_invite_overlay()
	assert_eq(double.invite_overlays.size(), 1, "the overlay is opened for it")
	assert_eq(double.invite_overlays[0], _LOBBY, "naming the lobby we host")

	transport.release_session()
	assert_true(double.lobbies_left.has(_LOBBY), "release_session leaves the lobby it advertised")
	assert_false(transport.can_invite(), "so there is nothing to invite into again")
	assert_false(double.has_presence(), "and the join string beside our name is withdrawn")
	transport.advertise_session()
	assert_eq(double.lobby_requests.size(), 1, "a released session cannot be advertised again")
	_done(transport)

func test_a_dedicated_host_opened_unadvertised_lists_itself_only_when_asked() -> void:
	# A dedicated server has no account, so it cannot own a matchmaking lobby -- its discovery is the game
	# server LISTING instead. Same two-step contract, different publish call.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport.create_dedicated_host(0, 8, false, false) != null, "the server peer opens")
	assert_true(double.server_listing.is_empty(), "nothing is listed as it opens")
	assert_true(double.lobby_requests.is_empty(), "and a server with no account creates no lobby")

	transport.advertise_session()
	assert_eq(double.server_listing.size(), 1, "advertise_session publishes the listing")
	assert_true(double.server_listing[0], "turning it on")
	transport.release_session()
	assert_eq(double.server_listing.size(), 2, "release_session touches it again")
	assert_false(double.server_listing[1], "turning it off, so a stopped server stops being findable")
	_done(transport)

func test_a_host_advertised_by_default_publishes_as_it_opens() -> void:
	var double: SteamTransport.PlatformDouble = _platform()
	var listen: SteamTransport = _transport(double)
	assert_true(listen.create_listen_host(0, 4) != null, "the listen host opens")
	assert_eq(double.lobby_requests.size(), 1, "and asks for its lobby at once, as it always has")
	_done(listen)

	var server_double: SteamTransport.PlatformDouble = _platform()
	var dedicated: SteamTransport = _transport(server_double)
	assert_true(dedicated.create_dedicated_host(0, 4) != null, "the dedicated host opens")
	assert_eq(server_double.server_listing.size(), 1, "and lists itself at once")
	assert_true(server_double.server_listing[0], "as listed")
	_done(dedicated)

# --- opening a client peer -------------------------------------------------------------------------

func test_a_client_peer_is_opened_against_the_host_it_was_asked_for() -> void:
	# The client arm instantiates the peer class and opens it against the host's account id on virtual port 0 --
	# the same virtual port the host's own socket is opened on, or the two never meet. Whether the target string
	# is a usable join target is `NetTransport`'s rule rather than this file's; what is asserted here is that the
	# id reaches the peer, in the argument the peer reads it from.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var peer: MultiplayerPeer = transport.create_client(str(_INVITE_HOST), 0)
	assert_true(peer != null, "the client peer opens")
	assert_eq(double.peers.size(), 1, "exactly one peer is instantiated")
	var opened: SteamTransport.DoublePeer = double.peers[0]
	assert_eq(opened.target_host_id, _INVITE_HOST, "pointed at the host id it was asked for")
	assert_false(opened.opened_as_host, "and opened as a client")
	_done(transport)

	var host_double: SteamTransport.PlatformDouble = _platform()
	var host: SteamTransport = _transport(host_double)
	assert_true(host.create_listen_host(0, 4, false, true) != null, "the listen host opens")
	var host_peer: SteamTransport.DoublePeer = host_double.peers[0]
	assert_true(host_peer.opened_as_host, "the host arm opens the same class as a host")
	assert_eq(host_peer.target_host_id, 0, "and points it at nobody")
	_done(host)

func test_a_client_peer_that_cannot_be_built_or_opened_yields_null() -> void:
	# Same degradation as the host arm: every failure returns null so the session layer surfaces one error, and
	# no half-open peer is handed back.
	var refusing: SteamTransport.PlatformDouble = _platform()
	refusing.peer_opens = false
	var refused: SteamTransport = _transport(refusing)
	assert_eq(refused.create_client(str(_INVITE_HOST), 0), null, "a socket that will not open yields no peer")
	_done(refused)

	var missing_double: SteamTransport.PlatformDouble = _platform()
	missing_double.peer_class_missing = true
	var missing: SteamTransport = _transport(missing_double)
	assert_eq(missing.create_client(str(_INVITE_HOST), 0), null, "nor does an unvendored peer class")
	assert_true(missing_double.peers.is_empty(), "and nothing was instantiated for it")
	_done(missing)

# --- accepting a play invite -----------------------------------------------------------------------

func test_an_accepted_invite_joins_the_lobby_and_publishes_the_host_it_resolves() -> void:
	# An invite names a LOBBY, but `create_client` takes a HOST id and only a member can read a lobby's metadata
	# reliably. So accepting is a two-step hop: join the lobby, then resolve the host on the join callback and
	# publish it as a plain connect target. Both live routes -- the overlay's join request and the connect-string
	# form -- end in that same hop.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var targets: Array[String] = []
	transport.invite_accepted.connect(func(target: String) -> void: targets.push_back(target))
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists, so the invite callbacks are wired")
	double.publish_row(_INVITE_LOBBY, "host_id", str(_INVITE_HOST))

	double.emit_join_request(_INVITE_LOBBY, 4242)
	assert_eq(double.lobbies_joined.size(), 1, "accepting joins the lobby the invite named")
	assert_eq(double.lobbies_joined[0], _INVITE_LOBBY, "that lobby and no other")
	assert_true(targets.is_empty(), "and publishes nothing until the join lands")

	double.emit_lobby_joined(_INVITE_LOBBY, double.joined_response())
	assert_eq(targets.size(), 1, "the resolved invite reaches the session layer once")
	assert_eq(targets[0], str(_INVITE_HOST), "as the host id, in the decimal-string form create_client takes")
	_done(transport)

func test_the_connect_string_route_reaches_the_same_hop_as_the_join_request() -> void:
	# Rich-presence joins and inviteUserToGame deliver the literal connect string the host published instead of
	# a lobby id. It parses with the pure parser the cold-start path uses, so one accepted invite delivered on
	# both routes joins once rather than twice.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")
	double.emit_join_game_request(4242, "+connect_lobby %d" % _INVITE_LOBBY)
	assert_eq(double.lobbies_joined.size(), 1, "the lobby named in the connect string is joined")
	assert_eq(double.lobbies_joined[0], _INVITE_LOBBY, "the one the string named")
	double.emit_join_request(_INVITE_LOBBY, 4242)
	assert_eq(double.lobbies_joined.size(), 1, "the same lobby already in flight is not chased twice")
	double.emit_join_game_request(4242, "nothing to connect to here")
	assert_eq(double.lobbies_joined.size(), 1, "and a connect string with no lobby in it starts nothing")
	_done(transport)

func test_a_host_never_accepts_an_invite_into_the_session_it_is_running() -> void:
	# The host receives the lobby callbacks for the lobby it created itself. Neither the accept guard nor the
	# join callback may read those as an invite, or hosting would immediately try to join its own session.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var targets: Array[String] = []
	transport.invite_accepted.connect(func(target: String) -> void: targets.push_back(target))
	assert_true(transport.create_listen_host(0, 4, false, true) != null, "the host peer opens")
	double.emit_lobby_created(double.ok_result(), _LOBBY)
	double.emit_join_request(_LOBBY, 4242)
	assert_true(double.lobbies_joined.is_empty(), "the lobby we host is never chased as an invite")
	double.emit_lobby_joined(_LOBBY, double.joined_response())
	assert_true(targets.is_empty(), "and our own lobby_joined callback publishes no connect target")
	_done(transport)

func test_a_join_that_did_not_land_clears_the_pending_invite_instead_of_swallowing_the_next_one() -> void:
	# The duplicate guard keys on the lobby currently in flight, so a join that failed has to clear it. A lobby
	# with no resolvable host is the same shape of failure one step later: nothing to connect to, and the
	# membership has to be given back rather than held.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var targets: Array[String] = []
	transport.invite_accepted.connect(func(target: String) -> void: targets.push_back(target))
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")

	double.emit_join_request(_INVITE_LOBBY, 4242)
	double.emit_lobby_joined(_INVITE_LOBBY, double.refused_response())
	assert_true(targets.is_empty(), "a refused join publishes no connect target")
	assert_true(double.lobbies_left.is_empty(), "and leaves no lobby it never entered")
	double.emit_join_request(_INVITE_LOBBY, 4242)
	assert_eq(double.lobbies_joined.size(), 2, "the same lobby can be accepted again afterwards")

	double.emit_lobby_joined(_INVITE_LOBBY, double.joined_response())
	assert_true(targets.is_empty(), "a lobby with no resolvable host publishes nothing either")
	assert_true(double.lobbies_left.has(_INVITE_LOBBY), "and we drop out of it rather than stay a member")
	_done(transport)

func test_a_joined_invite_lobby_is_held_out_of_reach_of_the_teardown_between_join_and_connect() -> void:
	# Accepting an invite while already in a session tears the OLD session down between the lobby join and the
	# new connect. The freshly joined lobby is therefore staged rather than recorded as the lobby we are a guest
	# in: that teardown's release_session() must not leave the lobby the next session needs. Ownership moves to
	# the guest slot only once the session it belongs to connects.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")
	double.publish_row(_INVITE_LOBBY, "host_id", str(_INVITE_HOST))
	double.emit_join_request(_INVITE_LOBBY, 4242)
	double.emit_lobby_joined(_INVITE_LOBBY, double.joined_response())

	transport.release_session()
	assert_false(double.lobbies_left.has(_INVITE_LOBBY),
		"the teardown that precedes the new connect leaves the staged lobby alone")
	transport._on_connection_failed()
	assert_true(double.lobbies_left.has(_INVITE_LOBBY),
		"but a session that never came up releases it, so the staging slot cannot pin us forever")
	transport._on_connection_failed()
	assert_eq(double.lobbies_left.size(), 1, "and releases it once, not once per failure signal")
	_done(transport)

func test_a_second_accepted_invite_drops_out_of_the_lobby_the_first_one_staged() -> void:
	# A player who accepts a second invite before the first one's session connected is never going to join the
	# first. Superseding it has to give the stale membership back, or memberships accumulate in sessions we
	# never enter and the platform keeps counting us against their caps.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	var targets: Array[String] = []
	transport.invite_accepted.connect(func(target: String) -> void: targets.push_back(target))
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")
	double.publish_row(_INVITE_LOBBY, "host_id", str(_INVITE_HOST))
	double.publish_row(_OTHER_LOBBY, "host_id", str(_OTHER_HOST))

	double.emit_join_request(_INVITE_LOBBY, 4242)
	double.emit_lobby_joined(_INVITE_LOBBY, double.joined_response())
	double.emit_join_request(_OTHER_LOBBY, 4343)
	assert_true(double.lobbies_left.has(_INVITE_LOBBY), "the superseded lobby is left as the second accept starts")
	double.emit_lobby_joined(_OTHER_LOBBY, double.joined_response())
	assert_eq(targets.size(), 2, "both accepts published a target")
	assert_eq(targets[1], str(_OTHER_HOST), "and the last one names the host of the lobby we are actually in")
	_done(transport)

func test_the_cold_start_launch_line_is_read_once_and_starts_the_same_lobby_hop() -> void:
	# Steam launches the process with `+connect_lobby <id>` when a player accepts an invite while the game is
	# NOT running. The launch args do not change, so they are read at most once per process -- re-reading them
	# would re-join on every visit to the menu.
	var double: SteamTransport.PlatformDouble = _platform()
	double.launch_command_line = "+connect_lobby %d" % _INVITE_LOBBY
	var transport: SteamTransport = _transport(double)
	transport.check_launch_invite()
	assert_eq(double.lobbies_joined.size(), 1, "the lobby named on the launch line is chased")
	assert_eq(double.lobbies_joined[0], _INVITE_LOBBY, "the one the line named")
	transport.check_launch_invite()
	assert_eq(double.lobbies_joined.size(), 1, "and the launch args are consumed once per process")
	_done(transport)

	var quiet_double: SteamTransport.PlatformDouble = _platform()
	var quiet: SteamTransport = _transport(quiet_double)
	quiet.check_launch_invite()
	assert_true(quiet_double.lobbies_joined.is_empty(), "an ordinary launch chases nothing")
	_done(quiet)

func test_the_connect_lobby_token_is_parsed_out_of_the_launch_line_shapes_steam_produces() -> void:
	# Pure and static, so the cold-start path is assertable with no platform at all. Every case here is a line
	# the parser has to answer correctly: a wrong answer either loses an accepted invite or points the player at a
	# lobby id that was never in the line.
	assert_eq(SteamTransport.parse_connect_lobby("+connect_lobby 777"), 777, "the token and the id after it")
	assert_eq(SteamTransport.parse_connect_lobby("game -x +connect_lobby 777 -y"), 777,
		"wherever the token sits in the line")
	assert_eq(SteamTransport.parse_connect_lobby("game   +connect_lobby   777"), 777, "however it is spaced")
	assert_eq(SteamTransport.parse_connect_lobby(""), 0, "an empty line names no lobby")
	assert_eq(SteamTransport.parse_connect_lobby("game -x -y"), 0, "nor a line without the token")
	assert_eq(SteamTransport.parse_connect_lobby("game +connect_lobby"), 0, "nor the token with nothing behind it")
	assert_eq(SteamTransport.parse_connect_lobby("+connect_lobby nonsense"), 0, "nor an id that does not parse")
	assert_eq(SteamTransport.parse_connect_lobby("+connect_lobby 0"), 0, "nor a zero id, which is 'no lobby'")
	assert_eq(SteamTransport.parse_connect_lobby("+connect_lobby -3"), 0, "nor a negative one")
	assert_eq(SteamTransport.parse_connect_lobby("+connect_lobby_more 777"), 0,
		"and the token has to match whole, not as a prefix of a longer argument")

# --- platform result codes -> the states the facade publishes --------------------------------------

func test_a_create_that_did_not_succeed_publishes_no_lobby_at_all() -> void:
	# The create callback carries a result code and a lobby id, and both can say "there is no lobby". Either
	# one has to stop the metadata stamping, or the host advertises rows on a lobby nobody can enter.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport.create_listen_host(0, 4, false, true) != null, "the host peer opens")
	double.emit_lobby_created(double.rejected_result(), _LOBBY)
	assert_true(double.rows(_LOBBY).is_empty(), "a rejected result stamps nothing")
	assert_false(transport.can_invite(), "and leaves nothing to invite into")
	double.emit_lobby_created(double.ok_result(), 0)
	assert_false(transport.can_invite(), "nor does a success that names no lobby")
	assert_false(double.has_presence(), "and no join string is published either way")
	double.emit_lobby_created(double.ok_result(), _LOBBY)
	assert_true(transport.can_invite(), "the create that did succeed is the one that counts")
	_done(transport)

func test_an_ownership_verdict_becomes_the_signal_the_session_layer_watches() -> void:
	# The dedicated-server trust boundary. The platform answers a ticket validation asynchronously with a
	# response code; the facade republishes it as a plain "does this account own the game" verdict, so no
	# game code reads a platform enum. Only the OK code means owned -- every other value is a refusal.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport.create_dedicated_host(0, 4) != null, "the server peer opens")
	var verdicts: Array[bool] = []
	var accounts: Array[int] = []
	transport.auth_validated.connect(func(account_id: int, owns: bool) -> void:
		accounts.push_back(account_id)
		verdicts.push_back(owns))
	double.emit_auth_verdict(4242, double.owns_response())
	double.emit_auth_verdict(4343, double.disowns_response())
	assert_eq(verdicts.size(), 2, "both verdicts reach the facade")
	assert_true(verdicts[0], "the OK response means the account owns the game")
	assert_false(verdicts[1], "and every other response is a refusal")
	assert_eq(accounts[0], 4242, "each verdict names the account it is about")
	assert_eq(accounts[1], 4343, "...and the second one too")
	_done(transport)

func test_the_ownership_gate_only_runs_where_it_belongs() -> void:
	# It is the DEDICATED server's boundary. A client has no server context, so beginning a validation there
	# fails synchronously and the platform is never asked -- which is what makes a listen host fail OPEN
	# rather than kick every joiner it cannot validate.
	var client_double: SteamTransport.PlatformDouble = _platform()
	var client: SteamTransport = _transport(client_double)
	client_double.ticket = PackedByteArray([7, 8, 9])
	assert_true(client.create_listen_host(0, 4) != null, "a listen host opens")
	assert_eq(client.issue_auth_ticket(), PackedByteArray([7, 8, 9]), "a logged-in account issues its ticket")
	assert_eq(client.begin_auth_session(PackedByteArray([1, 2]), 4242), FAILED,
		"but cannot validate anybody else's")
	assert_true(client_double.auth_sessions_begun.is_empty(), "and never asks the platform to")
	client.end_auth_session(4242)
	assert_true(client_double.auth_sessions_ended.is_empty(), "nor to release one it never opened")
	_done(client)

	var server_double: SteamTransport.PlatformDouble = _platform()
	var server: SteamTransport = _transport(server_double)
	assert_true(server.create_dedicated_host(0, 4) != null, "a dedicated server opens")
	assert_eq(server.begin_auth_session(PackedByteArray([1, 2]), 4242), OK, "and does validate a ticket")
	assert_eq(server_double.auth_sessions_begun.size(), 1, "asking the platform once")
	assert_eq(server_double.auth_sessions_begun[0], 4242, "for the account that claimed it")
	assert_eq(server_double.auth_ticket_sizes[0], 2, "handing over the ticket's length as its own argument")
	server_double.begin_auth_result = FAILED
	assert_eq(server.begin_auth_session(PackedByteArray([1, 2]), 4343), FAILED,
		"a ticket the platform refuses outright surfaces that refusal rather than a blanket OK")
	assert_true(server.issue_auth_ticket().is_empty(), "a server has no account, so it issues no ticket")
	_done(server)

func test_a_departing_peer_releases_the_validation_it_opened() -> void:
	# Every validation the server began has to be released, or the platform accumulates open auth sessions
	# for accounts that left. The mapping is peer id -> the account that peer claimed, recorded at submit.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport.create_dedicated_host(0, 4) != null, "the server peer opens")
	transport._peer_steam[9] = 4242
	transport._on_peer_disconnected(9)
	assert_eq(double.auth_sessions_ended.size(), 1, "the departing peer's validation is released")
	assert_eq(double.auth_sessions_ended[0], 4242, "naming the account it was opened for")
	transport._on_peer_disconnected(9)
	assert_eq(double.auth_sessions_ended.size(), 1, "and released once, not once per disconnect signal")
	transport._on_peer_disconnected(11)
	assert_eq(double.auth_sessions_ended.size(), 1, "a peer that never submitted a ticket releases nothing")
	_done(transport)

func test_the_platform_init_reply_is_read_in_every_shape_it_arrives_in() -> void:
	# The vendored extension answers an init call with a `{status, verbal}` record on current versions and
	# with a bare bool or int on older ones. All three shapes have to read as success, and a bad status has to
	# read as failure -- a misread here is the difference between a working transport and a silent one.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_true(transport._init_ok({"status": 0}), "a record whose status is 0")
	assert_false(transport._init_ok({"status": 5}), "a record whose status is anything else")
	assert_false(transport._init_ok({}), "a record with no status at all")
	assert_true(transport._init_ok(true), "a bare true")
	assert_false(transport._init_ok(false), "a bare false")
	assert_true(transport._init_ok(0), "a bare 0, which is the OK code")
	assert_false(transport._init_ok(1), "a bare non-zero")
	assert_false(transport._init_ok(null), "and nothing at all, which is what a missing entry point answers")
	_done(transport)

func test_an_init_that_failed_opens_no_peer_and_publishes_nothing() -> void:
	# The degradation the whole file is written around: a failure anywhere returns a null peer, so the
	# session layer surfaces the error exactly as it does for a native transport failure.
	var client_double: SteamTransport.PlatformDouble = _platform()
	client_double.client_init_reply = {"status": 5}
	var client: SteamTransport = _transport(client_double)
	assert_eq(client.create_listen_host(0, 4, false, true), null, "a failed client init opens no listen host")
	assert_true(client_double.lobby_requests.is_empty(), "and advertises nothing")
	assert_eq(client.local_persona_name(), "", "and has no display name to offer")
	assert_eq(client.local_steam_id(), 0, "nor an account id")
	_done(client)

	var server_double: SteamTransport.PlatformDouble = _platform()
	server_double.server_init_reply = false
	var server: SteamTransport = _transport(server_double)
	assert_eq(server.create_dedicated_host(0, 4), null, "a failed server init opens no dedicated host")
	assert_true(server_double.server_listing.is_empty(), "and lists nothing")
	_done(server)

func test_a_host_whose_socket_never_opened_advertises_nothing() -> void:
	# Order matters here: the lobby is created only once the peer exists, so no browser row ever points at a
	# host whose socket never opened.
	var double: SteamTransport.PlatformDouble = _platform()
	double.peer_opens = false
	var transport: SteamTransport = _transport(double)
	assert_eq(transport.create_listen_host(0, 4, false, true), null, "a socket that will not open yields no peer")
	assert_true(double.lobby_requests.is_empty(), "and no lobby is advertised for it")
	transport.advertise_session()
	assert_true(double.lobby_requests.is_empty(), "not even on a later publish, which was never armed")
	_done(transport)

	var missing_double: SteamTransport.PlatformDouble = _platform()
	missing_double.peer_class_missing = true
	var missing: SteamTransport = _transport(missing_double)
	assert_eq(missing.create_listen_host(0, 4, false, true), null, "an unvendored peer class yields no host peer")
	assert_eq(missing.create_dedicated_host(0, 4), null, "nor a server peer")
	assert_true(missing_double.server_listing.is_empty(), "and nothing is listed for it")
	_done(missing)

func test_the_callback_pump_runs_only_once_a_platform_context_exists() -> void:
	# The init call is made without embedded callbacks, so the lobby, auth and socket callbacks fire only
	# when this file pumps them. A build with no platform must pump nothing rather than look a singleton up
	# every frame.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	transport._process(0.016)
	assert_eq(double.callback_pumps, 0, "no context, no pump")
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a context is opened")
	transport._process(0.016)
	transport._process(0.016)
	assert_eq(double.callback_pumps, 2, "and then every frame pumps once")
	_done(transport)

# --- unavailable, and not yet cached ---------------------------------------------------------------

func test_a_name_the_platform_has_not_cached_yet_reads_empty() -> void:
	# The browser asks for the display name of an account that owns a lobby it is not a member of, which the
	# platform may not have cached. "" is the answer, and the session record falls back to a generic label --
	# never a null, never a placeholder that looks like a real name.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	double.friend_names[5551] = "Bea"
	assert_eq(transport.local_steam_id(), _ACCOUNT, "a platform context exists")
	assert_eq(transport.persona_name_for(5551), "Bea", "a cached name resolves")
	assert_eq(transport.persona_name_for(5552), "", "one the platform has not cached yet reads empty")
	assert_eq(transport.persona_name_for(0), "", "an unset account id is never looked up")
	assert_eq(transport.persona_name_for(-1), "", "nor a nonsense one")
	_done(transport)

func test_every_seam_reads_empty_when_there_is_no_platform_at_all() -> void:
	# The state every non-Steam build is in. With no singleton registered, every accessor answers empty and
	# every action is a no-op. None of them may raise: a game wires them unconditionally and only the build
	# decides whether they do anything.
	SteamTransport.clear_platform_double()
	var transport: SteamTransport = SteamTransport.new()
	assert_eq(transport.local_persona_name(), "", "no display name")
	assert_eq(transport.local_steam_id(), 0, "no account id")
	assert_eq(transport.persona_name_for(5551), "", "and no name for anybody else")
	assert_true(transport.sessions().is_empty(), "no discovered sessions")
	transport.request_session_list()
	assert_true(transport.sessions().is_empty(), "and a sweep finds none")
	assert_false(transport.can_invite(), "nothing to invite anyone into")
	transport.open_invite_overlay()
	transport.advertise_session()
	transport.release_session()
	transport._process(0.016)
	assert_eq(transport.create_listen_host(0, 4), null, "no listen host")
	assert_eq(transport.create_dedicated_host(0, 4), null, "no dedicated host")
	assert_eq(transport.create_client(str(_ACCOUNT), 0), null, "no client")
	assert_true(transport.issue_auth_ticket().is_empty(), "no ticket to prove anything with")
	assert_eq(transport.begin_auth_session(PackedByteArray([1]), 4242), FAILED, "no validation")
	transport.end_auth_session(4242)
	assert_false(transport.apply_fake_conditions(50.0, 5.0, 1.0, 0.0, 0.0, 0.0),
		"and the conditioner reports that it applied nothing, rather than claiming it did")
	transport.free()

func test_the_conditioner_reports_failure_when_the_knobs_are_not_there() -> void:
	# The impairment knobs are resolved by NAME from the platform's own registered constants, because the
	# ints are not verifiable without a real build. A platform that has no such knobs answers false, so a bench
	# run never reports numbers it measured over an unconditioned link.
	var double: SteamTransport.PlatformDouble = _platform()
	var transport: SteamTransport = _transport(double)
	assert_false(transport.apply_fake_conditions(50.0, 5.0, 1.0, 0.0, 0.0, 0.0),
		"a platform with no impairment knobs conditions nothing and says so")
	_done(transport)
