extends UnitTest
## ArenaConfig: the derivations every peer makes instead of being told.

func test_the_first_arena_is_not_zero() -> void:
	assert_true(ArenaConfig.FIRST_ARENA_ID >= 1,
		"0 is the facade's EVERY-WORLD membership, so an arena numbered 0 would be a world that matches every "
		+ "other world -- and a membership property that was never written would silently join it")

func test_seats_map_to_arenas_in_contiguous_blocks() -> void:
	assert_eq(ArenaConfig.arena_of_seat(0), ArenaConfig.FIRST_ARENA_ID, "seat 0 is in the first arena")
	assert_eq(ArenaConfig.arena_of_seat(ArenaConfig.SEATS_PER_ARENA - 1), ArenaConfig.FIRST_ARENA_ID,
		"and so is the last seat of that block")
	assert_eq(ArenaConfig.arena_of_seat(ArenaConfig.SEATS_PER_ARENA), ArenaConfig.FIRST_ARENA_ID + 1,
		"the next seat starts the next arena")

func test_every_seat_is_in_a_real_arena() -> void:
	for seat: int in ArenaConfig.SEAT_COUNT:
		assert_true(ArenaConfig.is_arena(ArenaConfig.arena_of_seat(seat)),
			"seat %d lands in an arena this session builds" % seat)

func test_an_out_of_range_seat_is_in_no_arena() -> void:
	assert_false(ArenaConfig.is_arena(ArenaConfig.arena_of_seat(-1)), "a negative seat is nowhere")
	assert_false(ArenaConfig.is_arena(ArenaConfig.arena_of_seat(ArenaConfig.SEAT_COUNT)),
		"and so is one past the pool")

func test_teams_are_seat_parity() -> void:
	assert_eq(ArenaConfig.team_of_seat(0), 0, "even seats are team 0")
	assert_eq(ArenaConfig.team_of_seat(1), 1, "odd seats are team 1")
	assert_eq(ArenaConfig.team_of_seat(ArenaConfig.SEATS_PER_ARENA), 0,
		"parity carries across the arena boundary, so every arena opens with a balanced pair of ends")

func test_first_seat_of_arena_inverts_arena_of_seat() -> void:
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		var first: int = ArenaConfig.first_seat_of_arena(arena)
		assert_eq(ArenaConfig.arena_of_seat(first), arena, "arena %d's first seat is in arena %d" % [arena, arena])

func test_the_shot_mask_contains_both_halves() -> void:
	assert_true((ArenaConfig.SHOT_MASK & ArenaConfig.LAYER_FIGHTER) != 0, "a shot can hit a fighter")
	assert_true((ArenaConfig.SHOT_MASK & ArenaConfig.LAYER_COVER) != 0, "and can stop on cover")
	assert_eq(ArenaConfig.SHOT_DYNAMIC_MASK, ArenaConfig.LAYER_FIGHTER,
		"only the fighters are reconstructed from the rewind ring; cover is the same cover at every tick")
	assert_eq(ArenaConfig.SHOT_MASK & ~ArenaConfig.SHOT_DYNAMIC_MASK, ArenaConfig.LAYER_COVER,
		"so the static remainder the live cast queries is exactly the cover layer")
