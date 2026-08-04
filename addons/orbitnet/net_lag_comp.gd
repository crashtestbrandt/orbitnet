extends RefCounted
class_name NetLagComp
## Server-side hit resolution with a tick-indexed history ring (#65, OrbitNet). The authoritative weapon model
## resolves shots through here so lag compensation has ONE home. Each server tick the ring records the poses of
## the hittable bodies; a shot then RESOLVES against either:
##   * the PRESENT tick -- a live-space ray cast (NetRay), which is what S7 ships and is correct on a localhost
##     listen-server where RTT is ~0, OR
##   * a PAST tick reconstructed from the ring, so a high-ping shooter's shot is judged against where the targets
##     were on THEIR screen (the classic "favour the shooter" rewind). The ring and the rewind method SIGNATURE
##     are in place now but the reconstruct-and-cast body is RESERVED (it falls back to the present cast) -- wiring
##     it is a follow-up once interpolation delay + per-target colliders are replicated.
##
## Pure Godot physics through [NetRay] -- no rollback-backend symbols (the `just net-check` gate). Owned by the
## SERVER's weapon authority; a client never instances one (it never resolves an authoritative hit). The hittable
## snapshot is supplied by a provider Callable so this stays decoupled from how the session enumerates players.

const _RING_SIZE: int = 128   # matches the backend history_limit default; the tick window a rewind could span

## Lag-comp rewind delay in ticks (#100 / 89c) -- how far into the past a shot is resolved: the server tests a
## traveling round against targets rewound by THIS many ticks (the shooter's view / interpolation delay), not all
## the way to fire-tick. Small ⇒ targets stay near-live, so leading + dodging + tracer-coherence survive while the
## shooter's view lag is modestly compensated. 0 ⇒ present-tick (no comp). Tunable live via the `net.lagcomp_ticks`
## cvar; default ~3 ≈ interp/render lag at 120 Hz. GLOBAL for M1; the per-shooter (interp + measured RTT/2) form is
## the documented follow-up and needs no change here -- the ring + _resolve_rewound are identical.
static var delay_ticks: int = 3

## One hittable collider's pose at a recorded tick (the unit the rewind reconstructs before resolving). For the 89c
## per-region model these are the individual region capsules: `collider` is the struck node (read back by the
## caller to resolve region + health), `transform` its recorded world pose, and `radius`/`height` the capsule
## descriptor the analytic rewind ray-tests against (a generic descriptor so this addon stays game-class-agnostic).
class Sample extends RefCounted:
	var collider: Object = null
	var transform: Transform3D = Transform3D.IDENTITY
	var radius: float = 0.0   # capsule radius (m); <= 0 = skip (no analytic shape recorded)
	var height: float = 0.0   # capsule total height (m), axis along the transform's local Y

# Fixed ring indexed by tick % _RING_SIZE. _ring_ticks[slot] is the tick that slot currently holds (-1 = empty),
# guarding against a stale wrap-around read; _ring_snaps[slot] is that tick's Sample list.
var _ring_ticks: PackedInt64Array = PackedInt64Array()
var _ring_snaps: Array[Array] = []

# #214 perf instrumentation: the O(N^2) lag-comp cost made visible. record() runs once per hittable body per
# server tick and snapshots every OTHER body's region capsules into fresh Samples, so today's N per-body rings
# allocate ~N*(N-1)*regions Samples/tick. These counters are STATIC so they SUM across every ring into one
# server-wide figure the net.perf report reads, and they survive the shared-ring refactor (one ring -> the same
# counters, lower numbers). _perf_tick_lo/hi bracket the server-tick span the accumulation covers so the reader
# can derive per-tick figures. Read-and-reset via perf_take_static().
static var _perf_samples: int = 0
static var _perf_record_usec: int = 0
static var _perf_tick_lo: int = -1
static var _perf_tick_hi: int = -1

## Read-and-reset the server-wide lag-comp perf counters (net.perf, #214): total Samples recorded, total usec
## spent in record(), and the span of distinct server ticks those covered (0 if nothing recorded since the last
## call -- e.g. a pure client, which never records, or offline). The caller derives samples/tick + usec/tick.
static func perf_take_static() -> Dictionary[String, int]:
	var span: int = (_perf_tick_hi - _perf_tick_lo + 1) if _perf_tick_lo >= 0 else 0
	var out: Dictionary[String, int] = {"samples": _perf_samples, "usec": _perf_record_usec, "ticks": maxi(span, 0)}
	_perf_samples = 0
	_perf_record_usec = 0
	_perf_tick_lo = -1
	_perf_tick_hi = -1
	return out

# Returns Array[Sample]: the bodies a shot could hit AT THE TICK BEING RECORDED (the server passes the live player
# set minus the shooter). Optional -- without it the ring stays empty and only present-tick live casts resolve.
var hittable_provider: Callable = Callable()

func _init() -> void:
	_ring_ticks.resize(_RING_SIZE)
	_ring_snaps.resize(_RING_SIZE)
	for i: int in _RING_SIZE:
		_ring_ticks[i] = -1
		_ring_snaps[i] = []

## Record the hittable snapshot for `tick` into the ring (server, once per fresh tick). No-op without a provider.
func record(tick: int) -> void:
	if not hittable_provider.is_valid():
		return
	var t0: int = Time.get_ticks_usec()
	var samples: Array[Sample] = hittable_provider.call()
	var slot: int = tick % _RING_SIZE
	_ring_ticks[slot] = tick
	_ring_snaps[slot] = samples
	# #214 perf: accumulate the server-side lag-comp cost. Static -- sums across every body's ring so net.perf
	# reads the whole-server figure; the wall-clock captures the Sample allocation cost the pooling step will cut.
	_perf_samples += samples.size()
	_perf_record_usec += Time.get_ticks_usec() - t0
	if _perf_tick_lo < 0 or tick < _perf_tick_lo:
		_perf_tick_lo = tick
	if tick > _perf_tick_hi:
		_perf_tick_hi = tick

## Drop every recorded snapshot. Called on session teardown: the ring outlives a session (the session layer owns ONE
## shared ring for the process), tick numbering restarts with the next session, and a slot still holding the
## previous session's samples would answer has_tick() for a colliding tick with references to long-freed bodies.
func clear() -> void:
	for i: int in _RING_SIZE:
		_ring_ticks[i] = -1
		_ring_snaps[i] = []


## Whether a recorded snapshot exists for `tick` (the reserved rewind path checks this before reconstructing).
func has_tick(tick: int) -> bool:
	if tick < 0:
		return false
	return _ring_ticks[tick % _RING_SIZE] == tick

## Resolve a shot. `at_tick` is the shooter's command tick and `present_tick` the server's current tick; when they
## match (or the snapshot is missing) this is a live present-tick cast. A PAST `at_tick` is the rewind case: the
## bits in `dynamic_mask` (the per-region hit layer, #89c) are reconstructed from the ring at `at_tick` and tested
## analytically, while the remaining (static) mask bits -- walls, tick-invariant -- are cast live, nearest wins.
## Returns a NetRay.Hit (valid=false on a miss). `space` must be queried where the physics space is unlocked (server net tick).
func resolve_hit(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID], mask: int, at_tick: int, present_tick: int, dynamic_mask: int = 0) -> NetRay.Hit:
	if at_tick >= 0 and at_tick < present_tick and has_tick(at_tick):
		return _resolve_rewound(space, origin, dir, dist, exclude, mask, at_tick, dynamic_mask)
	return NetRay.cast(space, origin, dir, dist, exclude, mask)

# Lag-comp rewind (89c): the dynamic (per-region) colliders are reconstructed from their `at_tick` ring poses and
# tested ANALYTICALLY (ray-vs-capsule) -- never by mutating the live physics world, so a concurrent projectile's
# present-tick cast in the same frame is never disturbed. The static remainder of the mask (world geometry, which
# does not move) is cast live at the present tick. The nearer of the two is the resolved hit; a struck region's
# Sample.collider is returned so the caller resolves region + health uniformly with the present-tick path.
func _resolve_rewound(space: PhysicsDirectSpaceState3D, origin: Vector3, dir: Vector3, dist: float, exclude: Array[RID], mask: int, at_tick: int, dynamic_mask: int) -> NetRay.Hit:
	var static_mask: int = mask & ~dynamic_mask
	var best: NetRay.Hit = NetRay.cast(space, origin, dir, dist, exclude, static_mask)
	var best_dist: float = best.distance if best.valid else dist + 1.0
	# record() stores a typed Array[Sample] per slot; iterate it with a typed loop var (no untyped Array, no Variant
	# cast). A nested-typed field Array[Array[Sample]] would be cleaner but GDScript 4.x does not parse nested typed
	# collections, so the loop-var typing is the equivalent.
	for sample: Sample in _ring_snaps[at_tick % _RING_SIZE]:
		if sample == null or sample.radius <= 0.0:
			continue
		# A sample holds a RAW reference to the recorded collider, and a Node reference keeps nothing alive: a
		# death/respawn frees the body and its region hitboxes while the ring still carries the `delay_ticks`
		# snapshots that named them. record() checks liveness at CAPTURE time; this is the only read of the ring,
		# so it must check again -- an EXPORTED build performs no liveness validation on a freed Object reference
		# (Variant's raw-pointer accessor; the "object was freed" guard is DEBUG_ENABLED-only), so `as`-casting a
		# dead sample, reading its RID, or handing it back as hit.collider all dereference freed memory. That is
		# the host crash that lands "right after somebody died": the shot resolves against a corpse's stale sample.
		if not is_instance_valid(sample.collider):
			continue
		# #214 self-exclusion: the shared server ring records EVERY body (including the shooter), so drop any sample
		# whose region collider is in the caller's exclude set (projectile.gd passes shooter.region_rids()). Harmless
		# for a legacy per-body ring -- it never records self, so nothing here ever matches.
		var co: CollisionObject3D = sample.collider as CollisionObject3D
		if co != null and exclude.has(co.get_rid()):
			continue
		var t: float = _ray_capsule(origin, dir, dist, sample.transform, sample.radius, sample.height)
		if t >= 0.0 and t < best_dist:
			best_dist = t
			var hit: NetRay.Hit = NetRay.Hit.new()
			hit.valid = true
			hit.collider = sample.collider
			hit.position = origin + dir * t
			hit.normal = _capsule_normal(hit.position, sample.transform, sample.radius, sample.height)
			hit.distance = t
			best = hit
	return best

# The outward surface normal of a capsule (axis = local Y) at world point `pos`: radial from the NEAREST point on
# the capsule's axis segment (the cap centres clamp the height), which is correct for both the cylinder wall (purely
# radial) and the hemisphere caps (from the cap centre). The capsule-centre vector the naive form used is wrong for
# both; this matters for any future ricochet / penetration-angle / decal consumer of the rewound hit normal.
static func _capsule_normal(pos: Vector3, xform: Transform3D, radius: float, height: float) -> Vector3:
	var local_hit: Vector3 = xform.affine_inverse() * pos
	var half: float = maxf(0.0, height * 0.5 - radius)
	var axis_world: Vector3 = xform * Vector3(0.0, clampf(local_hit.y, -half, half), 0.0)
	return (pos - axis_world).normalized()

# Analytic ray-vs-capsule: the nearest entry distance along `dir` (unit) within [0, dist] of a capsule at `xform`
# (no scale) with the given radius + total height (axis = local Y), or -1.0 for a miss. Tests the infinite cylinder
# clipped to the cap centres plus the two hemisphere spheres, and returns the smallest valid entry.
static func _ray_capsule(origin: Vector3, dir: Vector3, dist: float, xform: Transform3D, radius: float, height: float) -> float:
	var inv: Transform3D = xform.affine_inverse()
	var lo: Vector3 = inv * origin          # ray origin in capsule-local space
	var ld: Vector3 = inv.basis * dir       # ray dir in capsule-local space (rotation only -> stays unit length)
	var half: float = maxf(0.0, height * 0.5 - radius)   # cap-centre offset along local Y
	var r2: float = radius * radius
	var best_t: float = -1.0
	# Side wall: infinite cylinder of radius r around local Y, clipped to the cap-centre span.
	var a: float = ld.x * ld.x + ld.z * ld.z
	if a > 0.000000001:
		var b: float = 2.0 * (lo.x * ld.x + lo.z * ld.z)
		var c: float = lo.x * lo.x + lo.z * lo.z - r2
		var disc: float = b * b - 4.0 * a * c
		if disc >= 0.0:
			var t: float = (-b - sqrt(disc)) / (2.0 * a)
			var y: float = lo.y + t * ld.y
			if t >= 0.0 and t <= dist and y >= -half and y <= half:
				best_t = t
	# The two hemisphere caps.
	best_t = _nearest_sphere(lo, ld, Vector3(0.0, half, 0.0), r2, dist, best_t)
	best_t = _nearest_sphere(lo, ld, Vector3(0.0, -half, 0.0), r2, dist, best_t)
	return best_t

# Nearest entry distance of a unit-dir ray vs a sphere (centre `c`, radius^2 `r2`) within [0, dist], or `current`
# if none is nearer. `ld` is assumed unit length (the capsule transform carries no scale).
static func _nearest_sphere(lo: Vector3, ld: Vector3, c: Vector3, r2: float, dist: float, current: float) -> float:
	var oc: Vector3 = lo - c
	var b: float = 2.0 * oc.dot(ld)
	var disc: float = b * b - 4.0 * (oc.dot(oc) - r2)
	if disc < 0.0:
		return current
	var t: float = (-b - sqrt(disc)) * 0.5
	if t < 0.0 or t > dist:
		return current
	if current < 0.0 or t < current:
		return t
	return current
