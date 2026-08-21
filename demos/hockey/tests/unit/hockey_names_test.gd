extends UnitTest
## HockeyNames: deterministic node naming and the world signature.
##
## The highest-value suite in the demo per line of code, because the failure it guards is silent. OrbitNet
## derives an entity id from a node PATH; if two peers build different paths, replication goes nowhere and
## nothing errors. No exception, no warning, no log line -- the puck simply sits still on one of them.

func test_mallet_names_are_fixed_width_and_ordered() -> void:
	assert_eq(HockeyNames.mallet_node_name(0), "M00", "seat 0")
	assert_eq(HockeyNames.mallet_node_name(7), "M07", "seat 7")
	assert_eq(HockeyNames.mallet_node_name(31), "M31", "seat 31")
	# Fixed width is not cosmetic: it makes the names sort in seat order in a remote scene-tree inspector,
	# which is how you debug a pool of thirty-two.
	assert_true(HockeyNames.mallet_node_name(9) < HockeyNames.mallet_node_name(10),
		"zero padding makes string order match numeric order")

func test_names_round_trip_to_seats() -> void:
	for seat: int in [0, 1, 15, HockeyConfig.SEATS - 1]:
		assert_eq(HockeyNames.seat_from_name(HockeyNames.mallet_node_name(seat)), seat,
			"seat %d round-trips" % seat)

func test_non_mallet_names_do_not_parse_as_seats() -> void:
	for junk: String in ["", "M", "Mallet0", "@Node3D@27", "M0", "M0A", "XM01", "Puck"]:
		assert_eq(HockeyNames.seat_from_name(junk), -1, "'%s' is not a mallet name" % junk)

func test_the_fixed_names_are_distinct() -> void:
	var names: PackedStringArray = PackedStringArray([HockeyNames.RINK_ROOT, HockeyNames.MALLETS_ROOT,
		HockeyNames.PUCK_NODE, HockeyNames.SCORE_NODE, HockeyNames.SERVE_NODE, HockeyNames.INPUT_NODE])
	var seen: Dictionary[String, bool] = {}
	for entry: String in names:
		assert_false(seen.has(entry), "'%s' is used for exactly one node" % entry)
		seen[entry] = true
	# A mallet must never collide with a fixed name either, or two entities would hash to one id.
	for seat: int in HockeyConfig.SEATS:
		assert_false(seen.has(HockeyNames.mallet_node_name(seat)),
			"mallet %d does not collide with a fixed node name" % seat)

# --- FNV-1a ----------------------------------------------------------------------------------------
func test_fnv1a_matches_the_canonical_vectors() -> void:
	# The published FNV-1a 64 test vectors, as SIGNED 64-bit values (GDScript has no unsigned int, and the
	# arithmetic wraps modulo 2^64, which is exactly what FNV specifies). Pinned here as well as in the RTS
	# demo because the two are separate Godot projects and neither can import the other's copy.
	assert_eq(HockeyNames.fnv1a_64(""), -3750763034362895579, "the empty string is the offset basis")
	assert_eq(HockeyNames.fnv1a_64("a"), -5808556873153909620, "\"a\"")
	assert_eq(HockeyNames.fnv1a_64("foobar"), -8821353812377114648, "\"foobar\"")

func test_fnv1a_separates_similar_strings() -> void:
	assert_true(HockeyNames.fnv1a_64("M01") != HockeyNames.fnv1a_64("M02"),
		"adjacent mallet names hash apart")

# --- the world signature ---------------------------------------------------------------------------
func test_signature_ignores_collection_order() -> void:
	var forwards: PackedStringArray = PackedStringArray(["/root/Main/Rink/Mallets/M00",
		"/root/Main/Rink/Mallets/M01", "/root/Main/Rink/Puck"])
	var backwards: PackedStringArray = PackedStringArray(["/root/Main/Rink/Puck",
		"/root/Main/Rink/Mallets/M01", "/root/Main/Rink/Mallets/M00"])
	assert_eq(HockeyNames.world_signature(forwards), HockeyNames.world_signature(backwards),
		"a peer that built the same rink in a different order is not a bug and must not report one")

func test_signature_detects_a_single_auto_named_node() -> void:
	var good: PackedStringArray = PackedStringArray(["/root/Main/Rink/Mallets/M00"])
	var auto_named: PackedStringArray = PackedStringArray(["/root/Main/Rink/Mallets/@Node3D@27"])
	assert_true(HockeyNames.world_signature(good) != HockeyNames.world_signature(auto_named),
		"one auto-named node changes the signature -- which is the whole point, since Godot's auto-names are "
		+ "allocation-order dependent and WILL differ between peers")

func test_signature_detects_a_missing_node() -> void:
	var full: PackedStringArray = PackedStringArray(["a", "b", "c"])
	var short: PackedStringArray = PackedStringArray(["a", "b"])
	assert_true(HockeyNames.world_signature(full) != HockeyNames.world_signature(short),
		"a peer that built fewer entities is caught")
