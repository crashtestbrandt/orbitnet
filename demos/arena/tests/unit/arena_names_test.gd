extends UnitTest
## ArenaNames: the node names the wire depends on, and the signature that catches a disagreement.

func test_fighter_names_are_stable_and_distinct() -> void:
	var seen: PackedStringArray = PackedStringArray()
	for seat: int in ArenaConfig.SEAT_COUNT:
		var name_of: String = ArenaNames.fighter_node_name(seat)
		assert_false(seen.has(name_of), "seat %d's name is its own" % seat)
		seen.push_back(name_of)

func test_a_name_is_a_function_of_the_seat_alone() -> void:
	# Entity ids are hashes of node paths. A name that depended on join order, allocation order or anything
	# else a peer does not share would give the two peers different ids -- and nothing errors, the rows simply
	# go nowhere.
	assert_eq(ArenaNames.fighter_node_name(7), ArenaNames.fighter_node_name(7),
		"the same seat always names the same node")

func test_names_are_zero_padded_so_they_sort_as_they_number() -> void:
	assert_eq(ArenaNames.fighter_node_name(1), "Fighter001", "single digits are padded")
	assert_eq(ArenaNames.fighter_node_name(23), "Fighter023", "and so are double")

func test_prop_names_carry_their_arena() -> void:
	assert_true(ArenaNames.prop_node_name(1, 4) != ArenaNames.prop_node_name(2, 4),
		"the same prop index in two arenas is two entities, and two node paths")

func test_scorecards_are_one_per_arena() -> void:
	var seen: PackedStringArray = PackedStringArray()
	for offset: int in ArenaConfig.ARENAS:
		var name_of: String = ArenaNames.scorecard_node_name(ArenaConfig.FIRST_ARENA_ID + offset)
		assert_false(seen.has(name_of), "each arena's scorecard is its own node")
		seen.push_back(name_of)

# --- the signature ------------------------------------------------------------------------------------
func test_the_signature_is_order_independent() -> void:
	# The two peers build the same SET; requiring them to build it in the same sequence would make the
	# signature a test of the build loop rather than of the names.
	var forwards: PackedStringArray = PackedStringArray(["a/b", "a/c", "a/d"])
	var backwards: PackedStringArray = PackedStringArray(["a/d", "a/c", "a/b"])
	assert_eq(ArenaNames.world_signature(forwards), ArenaNames.world_signature(backwards),
		"the same set of paths in any order signs the same")

func test_a_different_set_signs_differently() -> void:
	var one: PackedStringArray = PackedStringArray(["a/b", "a/c"])
	var other: PackedStringArray = PackedStringArray(["a/b", "a/C"])
	assert_true(ArenaNames.world_signature(one) != ArenaNames.world_signature(other),
		"a single renamed node changes the signature, which is what makes the probe's comparison a gate")

func test_an_empty_world_signs_zero() -> void:
	assert_eq(ArenaNames.world_signature(PackedStringArray()), 0,
		"so a peer that built nothing is distinguishable from one that built something")
