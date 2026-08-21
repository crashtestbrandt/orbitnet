extends RefCounted
class_name TableProjection
## Where the pointer is on the table, as a pure function over a ray and the board's transform.
##
## Plane.intersects_ray, NOT a physics raycast: there is no collision shape for the table, no PhysicsServer
## involvement, and therefore nothing to configure, warm up, or get wrong about collision layers. It also means
## pointer picking works in a headless run, which a physics ray against an unrendered world does not reliably.
##
## The board carries the incline AND the half-turn that puts a player's own end nearest, so inverting its
## transform is what turns a screen position into a TABLE-SPACE point -- the same coordinates the simulation
## and the wire use, whichever way round the player is looking at it. Nothing downstream of here knows the
## table is tilted.
##
## Taking a transform rather than a Node3D keeps this testable: the caller does the projection (which needs a
## live Camera3D) and hands over plain values.

## The table-space point under a ray, or `fallback` when the ray misses the plane (aimed along it, or away).
static func table_point(ray_origin: Vector3, ray_direction: Vector3, board: Transform3D,
		fallback: Vector3 = Vector3.ZERO) -> Vector3:
	var normal: Vector3 = board.basis.y.normalized()
	if normal.length_squared() <= 0.0:
		return fallback
	var plane: Plane = Plane(normal, board.origin)
	var hit: Variant = plane.intersects_ray(ray_origin, ray_direction)
	if not (hit is Vector3):
		return fallback
	# A typed local, not an as-cast: narrowing a Variant by assignment is the conversion this project allows.
	var world: Vector3 = hit
	return TableGeometry.flatten(board.affine_inverse() * world)
