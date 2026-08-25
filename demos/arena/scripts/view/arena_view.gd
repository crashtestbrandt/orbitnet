extends Node3D
class_name ArenaView
## The static scenery: three floors, their cover, and their props. Built once and never touched again.
##
## NOT NETCODE, AND DELIBERATELY SEPARABLE FROM IT. The props are replicated entities and the cover is a
## physics body a shot casts against; neither needs a mesh to do its job, and a dedicated server builds none
## of this. Keeping the meshes out of those nodes is what lets the headless server prove it.
##
## ONE MULTIMESH PER ARENA FOR THE PROPS. Several hundred props per arena is several hundred draw calls as
## separate nodes and one as a MultiMesh, and they never move -- a static MultiMesh is what that is for.

const FLOOR_COLORS: Array[Color] = [
	Color(0.10, 0.11, 0.14),
	Color(0.12, 0.10, 0.13),
	Color(0.10, 0.13, 0.13),
]

func build(prop_count: int) -> void:
	name = "ArenaView"
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		var root: Node3D = Node3D.new()
		root.name = "View%02d" % arena
		root.position = ArenaGeometry.origin_of(arena)
		add_child(root)
		_build_floor(root, offset)
		_build_cover(root)
		_build_props(root, prop_count)

func _build_floor(root: Node3D, offset: int) -> void:
	var floor_mesh: MeshInstance3D = MeshInstance3D.new()
	floor_mesh.name = "Floor"
	var plane: BoxMesh = BoxMesh.new()
	plane.size = Vector3(ArenaConfig.ARENA_HALF_X * 2.0, 0.2, ArenaConfig.ARENA_HALF_Z * 2.0)
	floor_mesh.mesh = plane
	floor_mesh.position = Vector3(0.0, -0.1, 0.0)
	floor_mesh.material_override = _flat(FLOOR_COLORS[offset % FLOOR_COLORS.size()])
	root.add_child(floor_mesh)

func _build_cover(root: Node3D) -> void:
	for index: int in ArenaConfig.COVER_PER_ARENA:
		var box: AABB = ArenaGeometry.cover_local(index)
		var mesh: MeshInstance3D = MeshInstance3D.new()
		mesh.name = "CoverView%02d" % index
		var cube: BoxMesh = BoxMesh.new()
		cube.size = box.size
		mesh.mesh = cube
		mesh.position = box.get_center()
		mesh.material_override = _flat(Color(0.28, 0.30, 0.34))
		root.add_child(mesh)

func _build_props(root: Node3D, prop_count: int) -> void:
	if prop_count <= 0:
		return
	var mesh: MultiMeshInstance3D = MultiMeshInstance3D.new()
	mesh.name = "PropView"
	var multi: MultiMesh = MultiMesh.new()
	multi.transform_format = MultiMesh.TRANSFORM_3D
	var cube: BoxMesh = BoxMesh.new()
	cube.size = Vector3(0.35, 0.35, 0.35)
	multi.mesh = cube
	multi.instance_count = prop_count
	for index: int in prop_count:
		multi.set_instance_transform(index,
			Transform3D(Basis.IDENTITY, ArenaGeometry.prop_local(index) + Vector3(0.0, 0.18, 0.0)))
	mesh.multimesh = multi
	mesh.material_override = _flat(Color(0.42, 0.38, 0.30))
	root.add_child(mesh)

static func _flat(color: Color) -> StandardMaterial3D:
	var material: StandardMaterial3D = StandardMaterial3D.new()
	material.albedo_color = color
	material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	return material
