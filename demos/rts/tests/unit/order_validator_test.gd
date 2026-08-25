extends UnitTest
## OrderValidator: the server's adjudication of a hostile payload.
##
## Every case here is written from the attacker's side, because that is the only way the malicious cases ever
## get written. The validator being a pure static function is what makes that cheap -- there is no session to
## stand up, no server to run, and no client to fake.

func _all_alive() -> PackedByteArray:
	var alive: PackedByteArray = PackedByteArray()
	alive.resize(RtsConfig.UNIT_COUNT)
	alive.fill(1)
	return alive

func _ids(values: Array[int]) -> PackedInt32Array:
	var out: PackedInt32Array = PackedInt32Array()
	for value: int in values:
		out.push_back(value)
	return out

func _own_id(seat: int, offset: int = 0) -> int:
	return RtsConfig.first_id_of_seat(seat) + offset

# --- the happy path ------------------------------------------------------------------------------
func test_accepts_a_well_formed_move() -> void:
	var payload: Dictionary = {"ids": _ids([_own_id(0), _own_id(0, 1)]), "point": Vector3(5.0, 0.0, 5.0)}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, payload, _all_alive())
	assert_true(result.accepted, "an honest order is accepted")
	assert_eq(result.ids.size(), 2, "and keeps both named units")
	assert_eq(result.verb, OrderValidator.VERB_MOVE, "and carries the verb through")

func test_duplicate_ids_are_collapsed_not_rejected() -> void:
	var payload: Dictionary = {"ids": _ids([_own_id(0), _own_id(0), _own_id(0)]), "point": Vector3.ZERO}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, payload, _all_alive())
	assert_true(result.accepted, "duplicates are harmless")
	assert_eq(result.ids.size(), 1, "and are collapsed rather than applied repeatedly")

func test_stop_and_hold_discard_the_point() -> void:
	var payload: Dictionary = {"ids": _ids([_own_id(0)]), "point": Vector3(30.0, 0.0, -20.0)}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_STOP, payload, _all_alive())
	assert_true(result.accepted, "a stop order is accepted")
	assert_vec_almost_eq(result.point, Vector3.ZERO, 0.0001,
		"a destination is meaningless for STOP and is normalized away rather than left to be acted on")

func test_the_point_is_clamped_to_the_field() -> void:
	var payload: Dictionary = {"ids": _ids([_own_id(0)]), "point": Vector3(99999.0, 0.0, 99999.0)}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, payload, _all_alive())
	assert_true(result.accepted, "an off-map destination is clamped, not refused -- it is not forgery")
	assert_true(absf(result.point.x) <= RtsConfig.FIELD_HALF_X, "and lands inside the field")

# --- RULE 1: foreign-seat ids reject the WHOLE batch ------------------------------------------------
func test_a_foreign_id_rejects_the_entire_batch() -> void:
	# The forgery: seat 0 names one of its own units plus one of seat 1's.
	var payload: Dictionary = {"ids": _ids([_own_id(0), _own_id(1)]), "point": Vector3.ZERO}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, payload, _all_alive())
	assert_false(result.accepted, "a batch containing a foreign unit is refused outright")
	assert_eq(result.ids.size(), 0, "and NOTHING is applied -- not even the legal half")

func test_out_of_range_ids_are_rejected() -> void:
	for bad: int in [-1, RtsConfig.UNIT_COUNT, RtsConfig.UNIT_COUNT + 5000]:
		var payload: Dictionary = {"ids": _ids([bad]), "point": Vector3.ZERO}
		var result: OrderValidator.Result = OrderValidator.validate(
			0, OrderValidator.VERB_MOVE, payload, _all_alive())
		assert_false(result.accepted, "id %d is out of range and is refused" % bad)

func test_an_unseated_sender_is_refused() -> void:
	var payload: Dictionary = {"ids": _ids([_own_id(0)]), "point": Vector3.ZERO}
	for seat: int in [-1, RtsConfig.SEATS, 99]:
		var result: OrderValidator.Result = OrderValidator.validate(
			seat, OrderValidator.VERB_MOVE, payload, _all_alive())
		assert_false(result.accepted, "seat %d holds no army and may not order one" % seat)

# --- RULE 2: dead ids are dropped, NOT rejected ------------------------------------------------------
func test_dead_units_are_silently_dropped() -> void:
	var alive: PackedByteArray = _all_alive()
	alive[_own_id(0, 1)] = 0
	var payload: Dictionary = {"ids": _ids([_own_id(0), _own_id(0, 1)]), "point": Vector3.ZERO}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, payload, alive)
	assert_true(result.accepted, "a unit dying in flight is a RACE, not forgery -- the order still applies")
	assert_eq(result.ids.size(), 1, "the survivor is ordered")
	assert_eq(result.dropped_dead, 1, "and the drop is reported rather than hidden")

func test_an_all_dead_batch_accepts_with_nothing_to_do() -> void:
	var alive: PackedByteArray = _all_alive()
	alive.fill(0)
	var payload: Dictionary = {"ids": _ids([_own_id(0)]), "point": Vector3.ZERO}
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, payload, alive)
	assert_true(result.accepted, "not an error -- the player did nothing wrong")
	assert_eq(result.ids.size(), 0, "but there is nothing left to order")

# --- RULE 3: cardinality --------------------------------------------------------------------------
func test_an_empty_batch_is_refused() -> void:
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"ids": PackedInt32Array(), "point": Vector3.ZERO}, _all_alive())
	assert_false(result.accepted, "an empty order is malformed, not a no-op")

func test_an_oversized_batch_is_refused() -> void:
	var ids: PackedInt32Array = PackedInt32Array()
	for _i: int in RtsConfig.MAX_ORDER_IDS + 1:
		ids.push_back(_own_id(0))
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"ids": ids, "point": Vector3.ZERO}, _all_alive())
	assert_false(result.accepted,
		"the cap is checked BEFORE the per-id loop, so a huge payload cannot buy unbounded server work")

# --- RULE 4: non-finite vectors ----------------------------------------------------------------------
func test_non_finite_points_are_refused() -> void:
	var bad_points: Array[Vector3] = [
		Vector3(NAN, 0.0, 0.0), Vector3(0.0, NAN, 0.0), Vector3(0.0, 0.0, NAN),
		Vector3(INF, 0.0, 0.0), Vector3(0.0, 0.0, -INF)]
	for point: Vector3 in bad_points:
		var result: OrderValidator.Result = OrderValidator.validate(
			0, OrderValidator.VERB_MOVE, {"ids": _ids([_own_id(0)]), "point": point}, _all_alive())
		assert_false(result.accepted, "a non-finite destination component is refused: %s" % point)

# --- malformed payloads ----------------------------------------------------------------------------
func test_unknown_verbs_are_refused() -> void:
	var result: OrderValidator.Result = OrderValidator.validate(
		0, &"self_destruct", {"ids": _ids([_own_id(0)]), "point": Vector3.ZERO}, _all_alive())
	assert_false(result.accepted, "a verb with no handler is refused rather than silently ignored")

func test_a_missing_id_list_is_refused() -> void:
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"point": Vector3.ZERO}, _all_alive())
	assert_false(result.accepted, "no ids at all is malformed")

func test_a_wrong_typed_id_list_is_refused_not_crashed() -> void:
	# The payload crosses an @rpc boundary, where Godot decodes containers generically. Indexing a String as
	# if it were an array is exactly how a malformed packet becomes a server crash.
	for junk: Variant in ["not an array", 42, Vector3.ZERO, {"a": 1}]:
		var result: OrderValidator.Result = OrderValidator.validate(
			0, OrderValidator.VERB_MOVE, {"ids": junk, "point": Vector3.ZERO}, _all_alive())
		assert_false(result.accepted, "a non-array id list is refused")

func test_a_mixed_type_id_list_is_refused_whole() -> void:
	var mixed: Array = [_own_id(0), "seven", _own_id(0, 1)]
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"ids": mixed, "point": Vector3.ZERO}, _all_alive())
	assert_false(result.accepted, "a list with a non-int entry is malformed, not partially valid")

func test_a_plain_int_array_is_accepted() -> void:
	# An honest Godot client sends a PackedInt32Array, but a hand-written payload or another language's client
	# may send a plain Array of ints. That is well-formed and must work.
	var plain: Array = [_own_id(0)]
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"ids": plain, "point": Vector3.ZERO}, _all_alive())
	assert_true(result.accepted, "a plain Array of ints is a valid id list")
	assert_eq(result.ids.size(), 1, "and is converted rather than rejected")

# --- the code the refusal travels as ---------------------------------------------------------------
#
# NetCommand reads an int verdict of 0 as acceptance and any other value as a refusal carrying that value,
# and only the int form reaches the client that asked. So Code.OK being 0 is a wire contract, not a style
# choice -- an enum that started at 1 would announce every accepted order as a refusal.

func test_the_accepting_code_is_zero() -> void:
	assert_eq(OrderValidator.Code.OK as int, 0,
		"NetCommand reads 0 as acceptance, so an enum starting at 1 would refuse every order it accepted")

func test_each_rule_refuses_under_its_own_code() -> void:
	var alive: PackedByteArray = _all_alive()
	assert_eq(OrderValidator.validate(-1, OrderValidator.VERB_MOVE,
		{"ids": [_own_id(0)], "point": Vector3.ZERO}, alive).code,
		OrderValidator.Code.NO_SEAT as int, "an unseated sender")
	assert_eq(OrderValidator.validate(0, &"detonate",
		{"ids": [_own_id(0)], "point": Vector3.ZERO}, alive).code,
		OrderValidator.Code.UNKNOWN_VERB as int, "an unknown verb")
	assert_eq(OrderValidator.validate(0, OrderValidator.VERB_MOVE,
		{"point": Vector3.ZERO}, alive).code,
		OrderValidator.Code.MALFORMED_IDS as int, "a missing id list")
	assert_eq(OrderValidator.validate(0, OrderValidator.VERB_MOVE,
		{"ids": [_own_id(0)], "point": Vector3(NAN, 0.0, 0.0)}, alive).code,
		OrderValidator.Code.POINT_NOT_FINITE as int, "a non-finite point")
	assert_eq(OrderValidator.validate(0, OrderValidator.VERB_MOVE,
		{"ids": [_own_id(1)], "point": Vector3.ZERO}, alive).code,
		OrderValidator.Code.FOREIGN_ID as int, "a unit belonging to another seat")

func test_an_accepted_order_carries_the_accepting_code() -> void:
	var result: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"ids": [_own_id(0)], "point": Vector3.ZERO}, _all_alive())
	assert_eq(result.code, OrderValidator.Code.OK as int,
		"acceptance is the same value NetCommand reads as applied")

func test_a_refusal_reaching_the_client_names_no_unit_and_no_seat() -> void:
	# THE REASON STAYS ON THE SERVER. `result.reason` names ids and seats, which is server-side knowledge about
	# units the asker may not own; the client is told the code and says its own sentence. A describe() that
	# leaked an id would hand a probing client exactly the ownership answer rule 1 refuses to give it.
	var refused: OrderValidator.Result = OrderValidator.validate(
		0, OrderValidator.VERB_MOVE, {"ids": [_own_id(1)], "point": Vector3.ZERO}, _all_alive())
	assert_true(refused.reason.contains("%d" % _own_id(1)),
		"the server's own reason names the offending id")
	var told: String = OrderValidator.describe(refused.code)
	assert_true(told.length() > 0, "the client is still told something")
	assert_false(told.contains("%d" % _own_id(1)), "but never the id it probed for")

func test_every_code_describes_itself_and_acceptance_describes_nothing() -> void:
	var codes: Array[int] = [
		OrderValidator.Code.NO_SEAT, OrderValidator.Code.UNKNOWN_VERB,
		OrderValidator.Code.MALFORMED_IDS, OrderValidator.Code.TOO_MANY_IDS,
		OrderValidator.Code.POINT_NOT_FINITE, OrderValidator.Code.ID_OUT_OF_RANGE,
		OrderValidator.Code.FOREIGN_ID, OrderValidator.Code.RATE_LIMITED,
		OrderValidator.Code.FOREIGN_CHANNEL]
	for code: int in codes:
		assert_true(OrderValidator.describe(code).length() > 0,
			"code %d says something a player can read" % code)
	assert_eq(OrderValidator.describe(OrderValidator.Code.OK), "", "and acceptance explains nothing")
