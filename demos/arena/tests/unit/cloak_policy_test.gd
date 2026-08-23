extends UnitTest
## CloakPolicy: who must not be sent a cloaked fighter, and what changed since last time.

const TEAM_A: int = 0
const TEAM_B: int = 1

func test_your_own_team_still_sees_you_cloaked() -> void:
	assert_true(CloakPolicy.may_see(TEAM_A, TEAM_A, true),
		"a cloak is a fact about a PAIR -- the cloaked fighter's own team is not being deceived by it, which "
		+ "is exactly why a membership cannot express this and a per-peer veto can")

func test_the_other_team_does_not() -> void:
	assert_false(CloakPolicy.may_see(TEAM_B, TEAM_A, true), "the team being hidden from is hidden from")

func test_an_uncloaked_fighter_is_visible_to_everyone() -> void:
	assert_true(CloakPolicy.may_see(TEAM_B, TEAM_A, false), "no cloak, no veto")

# --- the set --------------------------------------------------------------------------------------------
func test_only_cloaked_enemies_are_withheld() -> void:
	var teams: PackedInt32Array = PackedInt32Array([TEAM_A, TEAM_A, TEAM_B, TEAM_B])
	var cloaked: PackedByteArray = PackedByteArray([1, 0, 1, 0])
	var hidden: PackedInt32Array = CloakPolicy.hidden_seats(PackedInt32Array([TEAM_A]), teams, cloaked)
	assert_eq(hidden.size(), 1, "one seat is withheld")
	assert_eq(hidden[0], 2, "the cloaked one on the other team")

func test_a_connection_on_both_teams_is_withheld_nothing() -> void:
	# A split-screen player whose two seats are on opposite teams may see both teams' cloaks. Blinding one
	# half of its screen for the other half's benefit would be wrong, and a veto refuses a row in a DATAGRAM,
	# which every seat behind a connection shares.
	var teams: PackedInt32Array = PackedInt32Array([TEAM_A, TEAM_B])
	var cloaked: PackedByteArray = PackedByteArray([1, 1])
	var hidden: PackedInt32Array = CloakPolicy.hidden_seats(
		PackedInt32Array([TEAM_A, TEAM_B]), teams, cloaked)
	assert_eq(hidden.size(), 0, "a connection with a seat on each team may see both")

func test_a_connection_with_no_seats_is_withheld_every_cloak() -> void:
	# An observer holds no seat, so it is on no team and no cloak is its to see. That is this demo's rule
	# rather than the facade's: a spectator watching cloaked fighters would be a spectator with better
	# information than either player.
	var teams: PackedInt32Array = PackedInt32Array([TEAM_A, TEAM_B])
	var cloaked: PackedByteArray = PackedByteArray([1, 1])
	assert_eq(CloakPolicy.hidden_seats(PackedInt32Array(), teams, cloaked).size(), 2,
		"both cloaked fighters are withheld from a peer on no team")

func test_mismatched_array_lengths_read_the_shorter_one() -> void:
	var teams: PackedInt32Array = PackedInt32Array([TEAM_A, TEAM_B, TEAM_A])
	var cloaked: PackedByteArray = PackedByteArray([1])
	assert_eq(CloakPolicy.hidden_seats(PackedInt32Array([TEAM_B]), teams, cloaked).size(), 1,
		"a short array bounds the walk rather than indexing past it")

# --- the diff, which is the part that costs bandwidth if it is wrong ---------------------------------------
func test_a_newly_cloaked_seat_is_a_change() -> void:
	var changed: PackedInt32Array = CloakPolicy.changes(PackedInt32Array(), PackedInt32Array([3]))
	assert_eq(changed.size(), 1, "one veto to place")
	assert_eq(changed[0], 3, "on the seat that just cloaked")

func test_a_dropped_cloak_is_a_change() -> void:
	var changed: PackedInt32Array = CloakPolicy.changes(PackedInt32Array([3]), PackedInt32Array())
	assert_eq(changed.size(), 1, "one veto to retract")
	assert_eq(changed[0], 3, "on the seat that just uncloaked")

func test_an_unchanged_set_reports_nothing() -> void:
	# RE-VETOING AN ENTITY ALREADY IN THAT STATE IS NOT FREE. Starting a veto drops the entity from that
	# peer's interest and CLEARS ITS DELTA BOOKKEEPING, so a later retraction sends a full block rather than a
	# delta against a base the peer dropped. Asserting the whole set every tick would hold every withheld
	# fighter permanently at "send a full block next".
	assert_eq(CloakPolicy.changes(PackedInt32Array([1, 2]), PackedInt32Array([1, 2])).size(), 0,
		"nothing moved, so nothing is touched")

func test_a_swap_reports_both_halves() -> void:
	var changed: PackedInt32Array = CloakPolicy.changes(PackedInt32Array([1]), PackedInt32Array([2]))
	assert_eq(changed.size(), 2, "one veto to retract and one to place")
	assert_true(changed.has(1) and changed.has(2), "and they name the two seats that moved")
