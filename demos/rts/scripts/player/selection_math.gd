extends RefCounted
class_name SelectionMath
## Box-select and click-select, as pure functions over SCREEN-SPACE points.
##
## SELECTION IS ENTIRELY CLIENT-LOCAL and never crosses the wire. That is worth stating plainly because it
## looks like it should be replicated and is not:
##
##   * It changes at mouse-move rates -- replicating it would cost more than every unit's position.
##   * The order payload already names its unit ids explicitly, so the server never needs to know what the
##     client currently has selected in order to adjudicate an order.
##   * A client-authored id list is safe BY CONSTRUCTION, because the server re-derives ownership from the
##     ids themselves (RtsConfig.seat_of) rather than trusting anything the client says about them.
##
## The one visible consequence -- how many units your opponent has selected -- rides the commander avatar's
## COSMETIC channel instead, where it costs a few bytes and cannot cause a misprediction.
##
## Taking screen points rather than a camera keeps this testable: the caller does the projection (which needs
## a live Camera3D) and hands over plain Vector2s.

## A drag rectangle from two corners in any order. Godot's Rect2 assumes a positive size, so dragging up-left
## produces a rect that contains nothing unless it is normalized first -- a classic and very confusing bug.
static func drag_rect(from: Vector2, to: Vector2) -> Rect2:
	var min_x: float = minf(from.x, to.x)
	var min_y: float = minf(from.y, to.y)
	var max_x: float = maxf(from.x, to.x)
	var max_y: float = maxf(from.y, to.y)
	return Rect2(Vector2(min_x, min_y), Vector2(max_x - min_x, max_y - min_y))

## Whether a drag is big enough to mean "box select" rather than "click". Below this it is a click, so a
## slightly shaky single click still selects the unit under the cursor instead of an empty box.
const CLICK_SLOP_PX: float = 6.0

static func is_click(from: Vector2, to: Vector2) -> bool:
	return from.distance_to(to) <= CLICK_SLOP_PX

## Every selectable unit whose screen point falls inside `rect`.
##
## `screen_points` is indexed by unit id; `selectable` is the parallel mask (owned AND alive AND on screen).
## Ids come back in ascending order, which makes the resulting order payload stable -- two identical drags
## produce byte-identical packets, which matters when you are staring at a bandwidth readout.
static func units_in_rect(rect: Rect2, screen_points: PackedVector2Array,
		selectable: PackedByteArray) -> PackedInt32Array:
	var out: PackedInt32Array = PackedInt32Array()
	var count: int = mini(screen_points.size(), selectable.size())
	for id: int in count:
		if selectable[id] == 0:
			continue
		if rect.has_point(screen_points[id]):
			out.push_back(id)
	return out

## The selectable unit nearest `point` within `max_px`, or -1. Click-select.
static func nearest_to_point(point: Vector2, screen_points: PackedVector2Array,
		selectable: PackedByteArray, max_px: float) -> int:
	var best_id: int = -1
	var best_distance_sq: float = max_px * max_px
	var count: int = mini(screen_points.size(), selectable.size())
	for id: int in count:
		if selectable[id] == 0:
			continue
		var distance_sq: float = point.distance_squared_to(screen_points[id])
		if distance_sq < best_distance_sq:
			best_distance_sq = distance_sq
			best_id = id
	return best_id

## Where a camera ray meets the ground plane (y = 0), or `fallback` when it does not (a ray aimed at the sky).
##
## Plane.intersects_ray, not a physics raycast: there is no collision shape for the ground, no PhysicsServer
## involvement, and therefore nothing to configure, warm up or get wrong about collision layers. It also means
## ground picking works in a headless probe, which a physics ray against an unrendered world does not
## reliably.
static func ground_point(ray_origin: Vector3, ray_direction: Vector3, fallback: Vector3 = Vector3.ZERO) -> Vector3:
	var plane: Plane = Plane(Vector3.UP, 0.0)
	var hit: Variant = plane.intersects_ray(ray_origin, ray_direction)
	if hit is Vector3:
		var point: Vector3 = hit
		return UnitSteering.clamp_to_field(point, 0.0)
	return fallback
