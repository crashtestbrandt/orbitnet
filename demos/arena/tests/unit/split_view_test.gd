extends UnitTest
## SplitView.rects(): the split-screen layout, which is the visible half of what a seat is.

const SCREEN: Vector2 = Vector2(1280.0, 800.0)

func test_one_seat_takes_the_whole_screen() -> void:
	var layout: Array[Rect2] = SplitView.rects(1, SCREEN)
	assert_eq(layout.size(), 1, "one seat, one view")
	assert_eq(layout[0], Rect2(Vector2.ZERO, SCREEN), "and it is the whole screen")

func test_zero_seats_still_gets_a_view() -> void:
	# An observer drives nothing and still has to see something.
	var layout: Array[Rect2] = SplitView.rects(0, SCREEN)
	assert_eq(layout.size(), 1, "a peer with no seat gets one full-screen view rather than none")

func test_two_seats_split_vertically_and_cover_the_screen() -> void:
	var layout: Array[Rect2] = SplitView.rects(2, SCREEN)
	assert_eq(layout.size(), 2, "two seats, two views")
	assert_almost_eq(layout[0].size.x + layout[1].size.x, SCREEN.x, 0.001, "together they span the width")
	assert_almost_eq(layout[0].size.y, SCREEN.y, 0.001, "each is full height -- these arenas are wide")
	assert_almost_eq(layout[1].position.x, layout[0].size.x, 0.001, "and the second starts where the first ends")

func test_the_left_seat_comes_first() -> void:
	var layout: Array[Rect2] = SplitView.rects(2, SCREEN)
	assert_almost_eq(layout[0].position.x, 0.0, 0.001,
		"seat order is left to right, so the first local seat is the left half")

func test_the_halves_do_not_overlap() -> void:
	var layout: Array[Rect2] = SplitView.rects(2, SCREEN)
	assert_false(layout[0].intersects(layout[1]),
		"an overlap would draw one seat's world over the other's")
