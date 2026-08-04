extends UnitTest
## RtsNames: deterministic node naming and the world signature.
##
## This is the highest-value suite in the demo per line of code, because the failure it guards is silent.
## OrbitNet derives an entity id from a node PATH; if two peers build different paths, replication goes
## nowhere and nothing errors. There is no exception, no warning, and no log line -- units simply sit still.

func test_unit_names_are_fixed_width_and_ordered() -> void:
	assert_eq(RtsNames.unit_node_name(0), "U00000000", "id 0")
	assert_eq(RtsNames.unit_node_name(7), "U00000007", "id 7")
	assert_eq(RtsNames.unit_node_name(95), "U00000095", "id 95")
	# Fixed width is not cosmetic: it makes the names sort in id order in a remote scene-tree inspector, which
	# is how you debug 96 entities.
	assert_true(RtsNames.unit_node_name(9) < RtsNames.unit_node_name(10),
		"zero padding makes string order match numeric order")

func test_names_round_trip_to_ids() -> void:
	for id: int in [0, 1, 42, 95, RtsConfig.UNIT_COUNT - 1]:
		assert_eq(RtsNames.unit_id_from_name(RtsNames.unit_node_name(id)), id, "id %d round-trips" % id)

func test_non_unit_names_do_not_parse_as_ids() -> void:
	for junk: String in ["", "U", "Unit0", "@Node3D@27", "C00", "U0000000A", "XU0000001"]:
		assert_eq(RtsNames.unit_id_from_name(junk), -1, "'%s' is not a unit name" % junk)

func test_commander_and_channel_names_are_distinct_per_seat() -> void:
	assert_eq(RtsNames.commander_node_name(0), "C00", "seat 0's commander")
	assert_eq(RtsNames.commander_node_name(1), "C01", "seat 1's commander")
	assert_true(RtsNames.orders_node_name(0) != RtsNames.orders_node_name(1),
		"each seat's order channel is its own node -- a shared name would collapse the per-seat forgery check")

# --- FNV-1a --------------------------------------------------------------------------------------
func test_fnv1a_matches_the_canonical_vectors() -> void:
	# The published FNV-1a 64 test vectors, as SIGNED 64-bit values (GDScript has no unsigned int, and the
	# arithmetic wraps modulo 2^64, which is exactly what FNV specifies).
	assert_eq(RtsNames.fnv1a_64(""), -3750763034362895579, "the empty string is the offset basis")
	assert_eq(RtsNames.fnv1a_64("a"), -5808556873153909620, "\"a\"")
	assert_eq(RtsNames.fnv1a_64("foobar"), -8821353812377114648, "\"foobar\"")

func test_fnv1a_separates_similar_strings() -> void:
	assert_true(RtsNames.fnv1a_64("U00000001") != RtsNames.fnv1a_64("U00000002"),
		"adjacent unit names hash apart")

# --- the world signature ---------------------------------------------------------------------------
func test_signature_ignores_collection_order() -> void:
	var forwards: PackedStringArray = PackedStringArray(["/root/Main/World/Units/U00000000",
		"/root/Main/World/Units/U00000001", "/root/Main/World/Commanders/C00"])
	var backwards: PackedStringArray = PackedStringArray(["/root/Main/World/Commanders/C00",
		"/root/Main/World/Units/U00000001", "/root/Main/World/Units/U00000000"])
	assert_eq(RtsNames.world_signature(forwards), RtsNames.world_signature(backwards),
		"a peer that built the same world in a different order is not a bug and must not report one")

func test_signature_detects_a_single_renamed_node() -> void:
	var good: PackedStringArray = PackedStringArray(["/root/Main/World/Units/U00000000"])
	var auto_named: PackedStringArray = PackedStringArray(["/root/Main/World/Units/@Node3D@27"])
	assert_true(RtsNames.world_signature(good) != RtsNames.world_signature(auto_named),
		"one auto-named node changes the signature -- which is the whole point, since Godot's auto-names are "
		+ "allocation-order dependent and WILL differ between peers")

func test_signature_detects_a_missing_node() -> void:
	var full: PackedStringArray = PackedStringArray(["a", "b", "c"])
	var short: PackedStringArray = PackedStringArray(["a", "b"])
	assert_true(RtsNames.world_signature(full) != RtsNames.world_signature(short),
		"a peer that built fewer entities is caught")
