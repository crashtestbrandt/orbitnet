extends RefCounted
class_name NetRay
## Hitscan ray facade (#65, OrbitNet). A thin wrapper over Godot's physics ray query so the weapon AUTHORITY --
## and, later, the survival INTERACTION system (use / pickup / mine / scan rays for crafting) -- cast through ONE
## seam instead of poking PhysicsDirectSpaceState3D inline at every call site. Pure Godot physics: it names no
## rollback-backend symbols (the `just net-check` gate), so it lives in the facade as netcode-adjacent infra.
##
## The cast is a PRESENT-tick query against the live space. True lag compensation -- rewinding hittable colliders
## to a past tick before casting -- is layered on top by [NetLagComp], which owns the per-tick history ring; this
## stays the primitive both the present-tick and the (reserved) rewound cast funnel through, so the ballistics
## model only ever sees a Hit.
##
## EXTENSIBILITY: hitscan is the first fire MODE. Advanced weapon physics (projectile travel + drop, spread,
## penetration, charge) grows by adding modes that still resolve down to one-or-more NetRay casts against the
## same space, reading their parameters from the data-driven WeaponProfile -- the rig and the wire never change.

## A resolved ray hit. `valid` is false when the ray reached `dist` without striking anything.
class Hit extends RefCounted:
	var valid: bool = false
	var collider: Object = null      # the struck body (a PlayerBody for a player hit; static geometry otherwise)
	var position: Vector3 = Vector3.ZERO
	var normal: Vector3 = Vector3.ZERO
	var distance: float = 0.0        # metres from the muzzle to the hit point

## Cast a ray from `origin` along unit `dir` for `dist` metres in `space`, excluding `exclude` (the shooter's own
## body RID, so a point-blank shot never self-hits). `mask` is the collision-layer mask tested against. Returns a
## Hit (valid=false on a miss / bad args). `space` must be queried where the physics space is UNLOCKED -- the net
## tick loop (the server's _rollback_tick) qualifies; never call inside _integrate_forces (the space is locked).
static func cast(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID] = [], mask: int = 0xFFFFFFFF) -> Hit:
	var hit: Hit = Hit.new()
	if space == null or dist <= 0.0 or dir.length_squared() < 0.000001:
		return hit
	var to: Vector3 = origin + dir.normalized() * dist
	var query: PhysicsRayQueryParameters3D = PhysicsRayQueryParameters3D.create(origin, to, mask, exclude)
	query.collide_with_areas = false
	query.collide_with_bodies = true
	# Untyped Dictionary: intersect_ray returns a PLAIN (untyped) Dictionary; a typed annotation here errors at
	# runtime ("Trying to assign a dictionary of type Dictionary to ... Dictionary[String, Variant]"), the same way
	# a wire-decoded Dictionary can't take a typed annotation (cf. SpawnDirector). Fields go to typed locals below.
	var result: Dictionary = space.intersect_ray(query)
	if result.is_empty():
		return hit
	# Variant -> typed via assignment (the project's allowed conversion; never as-cast a Variant).
	var collider: Object = result["collider"]
	var position: Vector3 = result["position"]
	var normal: Vector3 = result["normal"]
	hit.valid = true
	hit.collider = collider
	hit.position = position
	hit.normal = normal
	hit.distance = origin.distance_to(position)
	return hit

## Sweep a SPHERE of `radius` from `origin` along unit `dir` for `dist` metres in `space` -- the "forgiving shape
## cast" the survival INTERACTION pickup (#94 `take`) wants instead of a thin ray: a fat tube down the aim so a
## near-miss still grabs the item. `mask` restricts the cast to a layer set (the take cast masks to the dedicated
## WorldItem layer so station geometry is invisible to it); NEAREST contact along the sweep wins. Returns a Hit
## (valid=false on a miss / bad args); the struck body is Hit.collider (a WorldItem for a pickup). Same space-lock
## rule as cast(): query only where the physics space is UNLOCKED (a NetCommand handler / the net tick loop --
## never inside _integrate_forces).
##
## HOW: cast_motion finds the first-contact fraction along the sweep (nearest, and [0,0] when already overlapping at
## the origin -- the point-blank grab); intersect_shape at that contact centre enumerates the contacting bodies and
## we pick the one nearest `origin`, so a true nearest-hit is returned with the collider Object in hand (get_rest_info
## would give only a collider_id). A clean miss (cast_motion reports the full sweep clear) short-circuits.
static func cast_sphere(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, radius: float, exclude: Array[RID] = [], mask: int = 0xFFFFFFFF) -> Hit:
	var hit: Hit = Hit.new()
	if space == null or dist <= 0.0 or radius <= 0.0 or dir.length_squared() < 0.000001:
		return hit
	var d: Vector3 = dir.normalized()
	var shape: SphereShape3D = SphereShape3D.new()
	shape.radius = radius
	var params: PhysicsShapeQueryParameters3D = PhysicsShapeQueryParameters3D.new()
	params.shape = shape
	params.transform = Transform3D(Basis(), origin)
	params.motion = d * dist
	params.collision_mask = mask
	params.exclude = exclude
	params.collide_with_areas = false
	params.collide_with_bodies = true
	# cast_motion returns [safe, unsafe] sweep fractions: safe = last clear, unsafe = first overlap. Both 1.0 -> the
	# whole sweep is clear (miss). Both 0.0 -> already overlapping at the origin (point-blank). Otherwise contact at
	# `unsafe`. PackedFloat32Array via the allowed Variant -> typed assignment (never an as-cast).
	var motion: PackedFloat32Array = space.cast_motion(params)
	if motion.size() < 2:
		return hit
	var unsafe: float = motion[1]
	if motion[0] >= 1.0 and unsafe >= 1.0:
		return hit   # nothing in the swept tube
	# Re-seat the sphere at the first-contact centre and enumerate the bodies it overlaps there; the cast already
	# stopped at the nearest contact, so the nearest-to-origin overlap is the picked item (deterministic tie-break).
	params.transform = Transform3D(Basis(), origin + d * dist * unsafe)
	var results: Array[Dictionary] = space.intersect_shape(params, 16)
	var best: Object = null
	var best_d: float = INF
	var best_pos: Vector3 = Vector3.ZERO
	for r: Dictionary in results:
		var collider: Object = r["collider"]
		if collider == null:
			continue
		var node: Node3D = collider as Node3D
		var p: Vector3 = node.global_position if node != null else params.transform.origin
		var od: float = origin.distance_to(p)
		if od < best_d:
			best_d = od
			best = collider
			best_pos = p
	if best == null:
		return hit
	hit.valid = true
	hit.collider = best
	hit.position = best_pos
	hit.distance = best_d
	return hit
