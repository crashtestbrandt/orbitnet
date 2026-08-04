extends Node3D
class_name UnitRenderer
## Draws every unit, every selection ring and every health bar -- from the REPLICATED state on the UnitBody
## nodes, through MultiMeshInstance3D.
##
## THE NETCODE ENTITY AND THE RENDER REPRESENTATION ARE SEPARABLE, and this file is the demonstration. A
## UnitBody carries netcode state and nothing else: no mesh, no material, no children but its synchronizer.
## Everything visible is assembled here each frame into a handful of instance buffers. The consequences are
## worth being explicit about, because "one scene per entity" is the default habit and it is the expensive
## one:
##
##   * 96 replicated units cost 6 draw calls (one per team x archetype), not 96 scene subtrees.
##   * Adding a visual -- a ring, a bar, a muzzle flash -- costs one more buffer, not one more child node on
##     every entity.
##   * The renderer can be switched off entirely (a dedicated server never instantiates it) without the
##     netcode noticing, because nothing it does is load-bearing.
##
## THE BARREL MATTERS. A capsule has no visible orientation, so replicated FACING would be invisible and the
## (sin, cos) packing in UnitBody would look like a pointless flourish. Every unit therefore carries a
## forward-pointing barrel drawn from the same facing the wire carries. Watching 48 barrels swing round as an
## order lands is the most legible evidence that facing is replicating correctly.

## Per (seat, archetype) body buffer, plus the barrels, rings and bars.
var _bodies: Array[MultiMesh] = []
var _barrels: Array[MultiMesh] = []
var _rings: MultiMesh = null
var _bars: MultiMesh = null

var _world: WorldDirector = null
var _controller: CommanderController = null

const _KIND_COUNT: int = 3

func build(world: WorldDirector, controller: CommanderController) -> void:
	name = "UnitRenderer"
	_world = world
	_controller = controller
	_bodies.resize(RtsConfig.SEATS * _KIND_COUNT)
	_barrels.resize(RtsConfig.SEATS * _KIND_COUNT)
	for seat: int in RtsConfig.SEATS:
		for kind: int in _KIND_COUNT:
			var slot: int = seat * _KIND_COUNT + kind
			var colour: Color = RtsConfig.seat_color(seat)
			_bodies[slot] = _add_layer("Body_s%d_k%d" % [seat, kind], _body_mesh(kind), colour)
			_barrels[slot] = _add_layer("Barrel_s%d_k%d" % [seat, kind], _barrel_mesh(kind),
				colour.lightened(0.35))
	_rings = _add_layer("SelectionRings", _ring_mesh(), Color(0.95, 0.95, 0.55), true)
	_bars = _add_layer("HealthBars", _bar_mesh(), Color.WHITE, true)

func _process(_delta: float) -> void:
	if _world == null or _world.units.is_empty():
		return
	_draw_units()
	_draw_selection()

# --- per-frame fill ------------------------------------------------------------------------------
func _draw_units() -> void:
	# Counters per buffer. Instances are packed densely and visible_instance_count is set at the end, so a
	# dead unit costs nothing to draw rather than being drawn somewhere harmless.
	var counts: PackedInt32Array = PackedInt32Array()
	counts.resize(_bodies.size())
	var bar_count: int = 0

	for unit: UnitBody in _world.units:
		if unit == null or not unit.is_alive():
			continue
		var slot: int = unit.seat * _KIND_COUNT + unit.arch.kind
		if slot < 0 or slot >= _bodies.size():
			continue
		var index: int = counts[slot]
		if index >= RtsConfig.UNIT_COUNT:
			continue
		var facing: float = unit.facing()
		var basis: Basis = Basis(Vector3.UP, facing)
		var ground: Vector3 = unit.position
		var height: float = _body_height(unit.arch.kind)

		_bodies[slot].set_instance_transform(index,
			Transform3D(basis, ground + Vector3(0.0, height * 0.5, 0.0)))
		# The barrel is offset ALONG the facing, which is the whole point of drawing it.
		var forward: Vector3 = UnitSteering.forward_of(facing)
		_barrels[slot].set_instance_transform(index, Transform3D(
			basis, ground + Vector3(0.0, height * 0.62, 0.0) + forward * (unit.arch.radius + 0.5)))
		counts[slot] = index + 1

		# Health bar: a flat quad above the unit, scaled on X by remaining health and tinted from green to
		# red. Only drawn below full health -- a field of full bars is visual noise that hides the ones that
		# matter.
		var hp: float = unit.hp01()
		if hp < 0.995 and bar_count < RtsConfig.UNIT_COUNT:
			# Tilted to face the camera exactly, rather than using a billboard material: the camera's pitch
			# is a constant and its yaw is always zero, so the correct orientation is known in closed form.
			# Billboarding a MultiMesh is renderer-dependent and this demo has to run on GL Compatibility.
			var bar_basis: Basis = Basis(Vector3.RIGHT, deg_to_rad(CameraRig.PITCH_DEGREES)) \
				.scaled(Vector3(maxf(0.02, hp), 1.0, 1.0))
			_bars.set_instance_transform(bar_count,
				Transform3D(bar_basis, ground + Vector3(0.0, height + 1.1, 0.0)))
			_bars.set_instance_color(bar_count, Color(1.0 - hp, hp, 0.15, 1.0))
			bar_count += 1

	for slot: int in _bodies.size():
		_bodies[slot].visible_instance_count = counts[slot]
		_barrels[slot].visible_instance_count = counts[slot]
	_bars.visible_instance_count = bar_count

func _draw_selection() -> void:
	if _controller == null:
		_rings.visible_instance_count = 0
		return
	var count: int = 0
	for id: int in _controller.selection_ids():
		if id < 0 or id >= _world.units.size():
			continue
		var unit: UnitBody = _world.units[id]
		if unit == null or not unit.is_alive():
			continue
		var scale: float = unit.arch.radius * 2.2
		var basis: Basis = Basis().scaled(Vector3(scale, 1.0, scale))
		_rings.set_instance_transform(count,
			Transform3D(basis, unit.position + Vector3(0.0, 0.06, 0.0)))
		_rings.set_instance_color(count, RtsConfig.seat_color(unit.seat).lightened(0.5))
		count += 1
		if count >= RtsConfig.UNIT_COUNT:
			break
	_rings.visible_instance_count = count

# --- buffer + mesh construction --------------------------------------------------------------------
func _add_layer(layer_name: String, mesh: Mesh, tint: Color, per_instance_colour: bool = false) -> MultiMesh:
	var multi: MultiMesh = MultiMesh.new()
	multi.transform_format = MultiMesh.TRANSFORM_3D
	# use_colors MUST be set before instance_count: the buffer stride is decided when the count is assigned,
	# and enabling colours afterwards silently reallocates without them.
	multi.use_colors = per_instance_colour
	multi.mesh = mesh
	multi.instance_count = RtsConfig.UNIT_COUNT
	multi.visible_instance_count = 0

	var material: StandardMaterial3D = StandardMaterial3D.new()
	material.albedo_color = tint
	material.roughness = 0.75
	if per_instance_colour:
		material.vertex_color_use_as_albedo = true
		# Rings and bars are UI drawn in 3D: they must read the same in shadow as in sunlight, so they are
		# unshaded. A health bar that dims when a unit walks behind a box is a bar you cannot trust.
		material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED

	var instance: MultiMeshInstance3D = MultiMeshInstance3D.new()
	instance.name = layer_name
	instance.multimesh = multi
	# material_override on the INSTANCE, not surface_set_material on the mesh: surface_set_material lives on
	# ArrayMesh, and every mesh here is a PrimitiveMesh.
	instance.material_override = material
	add_child(instance)
	return multi

static func _body_height(kind: int) -> float:
	if kind == RtsConfig.Kind.TANK:
		return 1.4
	if kind == RtsConfig.Kind.TROOPER:
		return 1.7
	return 1.5

# A distinct silhouette per archetype, so a glance tells you what is fighting what: Scouts are slim capsules,
# Troopers taller capsules, Tanks low boxes.
static func _body_mesh(kind: int) -> Mesh:
	if kind == RtsConfig.Kind.TANK:
		var box: BoxMesh = BoxMesh.new()
		box.size = Vector3(2.0, 1.4, 2.6)
		return box
	var capsule: CapsuleMesh = CapsuleMesh.new()
	capsule.radius = 0.42 if kind == RtsConfig.Kind.SCOUT else 0.5
	capsule.height = 1.5 if kind == RtsConfig.Kind.SCOUT else 1.7
	return capsule

static func _barrel_mesh(kind: int) -> Mesh:
	var barrel: BoxMesh = BoxMesh.new()
	if kind == RtsConfig.Kind.TANK:
		barrel.size = Vector3(0.28, 0.28, 1.8)
	else:
		barrel.size = Vector3(0.16, 0.16, 0.9)
	return barrel

static func _ring_mesh() -> Mesh:
	var torus: TorusMesh = TorusMesh.new()
	torus.inner_radius = 0.44
	torus.outer_radius = 0.5
	return torus

static func _bar_mesh() -> Mesh:
	var quad: QuadMesh = QuadMesh.new()
	quad.size = Vector2(1.6, 0.16)
	return quad
