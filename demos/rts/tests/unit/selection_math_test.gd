extends UnitTest
## SelectionMath: box select, click select, and ground picking.

func _points(values: Array[Vector2]) -> PackedVector2Array:
	var out: PackedVector2Array = PackedVector2Array()
	for value: Vector2 in values:
		out.push_back(value)
	return out

func _mask(values: Array[int]) -> PackedByteArray:
	var out: PackedByteArray = PackedByteArray()
	for value: int in values:
		out.push_back(value)
	return out

# --- the drag rectangle ---------------------------------------------------------------------------
func test_a_rect_dragged_up_and_left_is_normalized() -> void:
	# Godot's Rect2 assumes a positive size. Dragging up-left produces a negative-size rect that contains
	# nothing, so box select appears to work in two directions and silently fail in the other two.
	var rect: Rect2 = SelectionMath.drag_rect(Vector2(100.0, 100.0), Vector2(20.0, 40.0))
	assert_almost_eq(rect.position.x, 20.0, 0.001, "the rect starts at the smaller x")
	assert_almost_eq(rect.position.y, 40.0, 0.001, "and the smaller y")
	assert_almost_eq(rect.size.x, 80.0, 0.001, "with a positive width")
	assert_almost_eq(rect.size.y, 60.0, 0.001, "and a positive height")
	assert_true(rect.has_point(Vector2(50.0, 50.0)), "so it actually contains the points it spans")

func test_all_four_drag_directions_agree() -> void:
	var a: Vector2 = Vector2(10.0, 10.0)
	var b: Vector2 = Vector2(90.0, 70.0)
	var forwards: Rect2 = SelectionMath.drag_rect(a, b)
	var backwards: Rect2 = SelectionMath.drag_rect(b, a)
	assert_true(forwards == backwards, "the rect does not depend on which corner you started from")

# --- click vs drag --------------------------------------------------------------------------------
func test_a_shaky_click_is_still_a_click() -> void:
	assert_true(SelectionMath.is_click(Vector2(100.0, 100.0), Vector2(103.0, 102.0)),
		"a few pixels of travel is a click, not an empty box -- nobody clicks perfectly still")
	assert_false(SelectionMath.is_click(Vector2(100.0, 100.0), Vector2(200.0, 180.0)),
		"a real drag is a box")

# --- box select -----------------------------------------------------------------------------------
func test_only_selectable_units_inside_the_box_are_picked() -> void:
	var points: PackedVector2Array = _points([
		Vector2(50.0, 50.0),     # inside, selectable
		Vector2(60.0, 60.0),     # inside, NOT selectable (an enemy, or a corpse)
		Vector2(500.0, 500.0),   # outside, selectable
		Vector2(70.0, 55.0),     # inside, selectable
	])
	var mask: PackedByteArray = _mask([1, 0, 1, 1])
	var picked: PackedInt32Array = SelectionMath.units_in_rect(
		Rect2(Vector2(0.0, 0.0), Vector2(100.0, 100.0)), points, mask)
	assert_eq(picked.size(), 2, "two units qualify")
	assert_eq(picked[0], 0, "and they come back in ascending id order")
	assert_eq(picked[1], 3, "so two identical drags produce identical order payloads")

func test_an_empty_box_selects_nothing() -> void:
	var picked: PackedInt32Array = SelectionMath.units_in_rect(
		Rect2(Vector2(0.0, 0.0), Vector2(1.0, 1.0)),
		_points([Vector2(500.0, 500.0)]), _mask([1]))
	assert_eq(picked.size(), 0, "nothing inside means nothing selected")

func test_mismatched_array_lengths_do_not_read_off_the_end() -> void:
	# The arrays are built by the caller each frame; a length mismatch is a caller bug, and it must degrade
	# rather than index out of bounds.
	var picked: PackedInt32Array = SelectionMath.units_in_rect(
		Rect2(Vector2(0.0, 0.0), Vector2(100.0, 100.0)),
		_points([Vector2(10.0, 10.0), Vector2(20.0, 20.0)]), _mask([1]))
	assert_eq(picked.size(), 1, "only the overlapping prefix is considered")

# --- click select ---------------------------------------------------------------------------------
func test_click_picks_the_nearest_selectable_unit() -> void:
	var points: PackedVector2Array = _points([Vector2(100.0, 100.0), Vector2(104.0, 100.0)])
	var picked: int = SelectionMath.nearest_to_point(Vector2(103.0, 100.0), points, _mask([1, 1]), 20.0)
	assert_eq(picked, 1, "the closer of two candidates wins")

func test_click_ignores_unselectable_units_even_when_closer() -> void:
	var points: PackedVector2Array = _points([Vector2(100.0, 100.0), Vector2(102.0, 100.0)])
	var picked: int = SelectionMath.nearest_to_point(Vector2(102.0, 100.0), points, _mask([1, 0]), 20.0)
	assert_eq(picked, 0, "an enemy under the cursor does not steal the click from your own unit behind it")

func test_click_beyond_the_radius_selects_nothing() -> void:
	var picked: int = SelectionMath.nearest_to_point(
		Vector2(0.0, 0.0), _points([Vector2(500.0, 500.0)]), _mask([1]), 20.0)
	assert_eq(picked, -1, "clicking empty ground clears rather than reaching across the screen")

# --- ground picking -------------------------------------------------------------------------------
func test_a_downward_ray_hits_the_ground_plane() -> void:
	var hit: Vector3 = SelectionMath.ground_point(Vector3(3.0, 20.0, -4.0), Vector3.DOWN)
	assert_vec_almost_eq(hit, Vector3(3.0, 0.0, -4.0), 0.001, "straight down lands directly below")

func test_an_angled_ray_hits_where_the_geometry_says() -> void:
	var hit: Vector3 = SelectionMath.ground_point(
		Vector3(0.0, 10.0, 0.0), Vector3(1.0, -1.0, 0.0).normalized())
	assert_vec_almost_eq(hit, Vector3(10.0, 0.0, 0.0), 0.001, "a 45-degree ray from 10 m up lands 10 m out")

func test_a_ray_at_the_sky_falls_back() -> void:
	var fallback: Vector3 = Vector3(7.0, 0.0, 7.0)
	var hit: Vector3 = SelectionMath.ground_point(Vector3(0.0, 10.0, 0.0), Vector3.UP, fallback)
	assert_vec_almost_eq(hit, fallback, 0.001,
		"a ray that never meets the ground returns the fallback rather than an undefined point")

func test_ground_picks_are_clamped_to_the_field() -> void:
	var hit: Vector3 = SelectionMath.ground_point(
		Vector3(0.0, 1.0, 0.0), Vector3(1.0, -0.001, 0.0).normalized())
	assert_true(absf(hit.x) <= RtsConfig.FIELD_HALF_X,
		"a near-horizontal ray lands far away but is clamped, so an order can never target infinity")
