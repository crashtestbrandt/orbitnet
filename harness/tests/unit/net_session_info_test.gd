extends UnitTest
## Scene-free coverage for the join browser's session row model ([NetSessionInfo]): the pure summary /
## fallback / joinable / room logic the [SessionMenu] browser renders. No scene tree, no Steam -- these rows only
## ever carry already-resolved plain data.
##
## And for the SESSION-IDENTITY half of the [code]Net[/code] facade that a browser row leads into: the RESUME
## POLICY and the RESUME TOKEN. Both are exercised through the facade itself rather than through a stub,
## because what is worth pinning there is not a forward but a DEFAULT, a CLAMP and a DEGRADED ANSWER:
##
## - [code]Net.ResumePolicy.ALWAYS[/code] is the default and it must stay the default. The token is what
##   removed the reachable takeover -- a claim has to quote a value the server minted -- so a stricter policy
##   buys nothing against an on-path observer while refusing every honest fast reconnect.
## - The enum NUMBERS are part of the contract. The facade writes [code]int(policy)[/code] into a backend
##   property, so the two enums must agree member for member or a stored policy means something else.
## - A number outside the enum clamps to ALWAYS, which is the direction that refuses nobody. That is the
##   OPPOSITE direction from the seat-release clamp, and deliberately so.
## - The token accessors ANSWER 0 rather than erroring when the loaded binary predates them, and
##   `peer_resume_token` answers 0 OFFLINE. That is not hypothetical: this suite runs against the committed
##   cdylib, which is refreshed only at a release tag.

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

# --- the resume policy and the resume token on the facade --------------------------------------------

func test_the_resume_policy_enum_matches_the_backend_numbering() -> void:
	# The facade writes `int(policy)` into a backend property and reads the same number back, so the two enums
	# are one contract. Drift here fails nothing loudly -- it silently renames the chosen policy.
	assert_eq(int(Net.ResumePolicy.ALWAYS), 0, "ALWAYS is 0")
	assert_eq(int(Net.ResumePolicy.ONLY_IF_DROPPED), 1, "ONLY_IF_DROPPED is 1")
	assert_eq(int(Net.ResumePolicy.NEVER), 2, "NEVER is 2")

func test_a_session_that_chose_nothing_grants_every_token_backed_claim() -> void:
	# THE DEFAULT-DRIFT GUARD. The resume TOKEN is what removed the reachable takeover: a claim has to quote a
	# value the server minted and sent only to the client that owned the identity. A stricter default would buy
	# nothing against an on-path observer -- who reads the join reply and can quote the token anyway -- while
	# refusing every honest fast reconnect, which is the case resume exists for.
	assert_eq(Net.resume_policy(), Net.ResumePolicy.ALWAYS, "no policy chosen means no claim is refused")

func test_a_resume_policy_outside_the_enum_clamps_to_always() -> void:
	# A stored number this build does not know must refuse NOBODY rather than select whichever member happens
	# to sit at that index. That is the OPPOSITE direction from the seat-release clamp, which falls onto the
	# policy that takes nothing away: here the harm of guessing wrong is locking honest players out of their
	# own bodies, and ALWAYS is token-gated so falling onto it forfeits nothing.
	for junk: int in [3, 99, -1, -7]:
		Net.set_resume_policy(junk)
		assert_eq(Net.resume_policy(), Net.ResumePolicy.ALWAYS, "policy %d is not a policy" % junk)
	Net.set_resume_policy(Net.ResumePolicy.ALWAYS)

func test_a_peers_resume_token_answers_zero_rather_than_erroring() -> void:
	# SERVER-SIDE and a diagnostic, so it has three ways to answer nothing: OFFLINE, an unknown peer, and a
	# binary that predates the call. None of them is an error -- an admin readout has no business bringing the
	# session down.
	assert_eq(Net.peer_resume_token(4), 0, "no session means no token to report")
	assert_eq(Net.peer_resume_token(0), 0, "and a peer id that names no connection")

func test_this_peer_holds_no_resume_token_until_a_server_mints_one() -> void:
	# The token is SERVER-MINTED, unlike the session id the facade mints for itself at boot. A client that has
	# joined nothing holds none, and quoting 0 is what seats a first-time joiner as a newcomer.
	assert_eq(Net.resume_token(), 0, "nothing has issued one yet")

func test_restoring_a_resume_token_either_round_trips_or_degrades_to_zero() -> void:
	# The persistence path: a game restores the token beside the stored session id, in either order, before it
	# joins. Against a binary that predates the call the write is a no-op and the read answers 0 -- the
	# degraded answer every backwards-compatible accessor on this facade gives -- and the game still runs.
	var stored: int = 0x0123456789abcdef
	Net.set_resume_token(stored)
	var read_back: int = Net.resume_token()
	assert_true(read_back == stored or read_back == 0, "a stored token round-trips, or the backend predates it")
	Net.set_resume_token(0)
	assert_eq(Net.resume_token(), 0, "and 0 clears it, which quotes no token at all")
