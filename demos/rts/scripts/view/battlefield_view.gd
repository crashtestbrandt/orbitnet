extends Node3D
class_name BattlefieldView
## The ground, the obstacles and the light. All primitives, no art assets -- this repo ships no binary
## content beyond the backend library, and a netcode demo that needed an artist would be a worse netcode
## demo.
##
## THE OBSTACLE BOXES ARE DRAWN FROM THE SAME AABB LIST THE STEERING READS (WorldDirector.build_obstacles).
## One list, two consumers, so what you see is provably what units collide with. A demo where the visual
## geometry and the collision geometry are separate definitions will eventually disagree, and the disagreement
## reads as a physics or a netcode bug rather than as the bookkeeping error it is.

func build(obstacles: Array[AABB]) -> void:
	name = "Battlefield"
	_add_ground()
	_add_light()
	for index: int in obstacles.size():
		_add_obstacle(obstacles[index], index)

func _add_ground() -> void:
	var mesh: PlaneMesh = PlaneMesh.new()
	mesh.size = Vector2(RtsConfig.FIELD_HALF_X * 2.0, RtsConfig.FIELD_HALF_Z * 2.0)
	var material: StandardMaterial3D = StandardMaterial3D.new()
	material.albedo_color = Color(0.16, 0.18, 0.20)
	material.roughness = 1.0
	mesh.material = material
	var instance: MeshInstance3D = MeshInstance3D.new()
	instance.name = "Ground"
	instance.mesh = mesh
	add_child(instance)

func _add_obstacle(box: AABB, index: int) -> void:
	var mesh: BoxMesh = BoxMesh.new()
	mesh.size = box.size
	var material: StandardMaterial3D = StandardMaterial3D.new()
	material.albedo_color = Color(0.30, 0.31, 0.34)
	material.roughness = 0.9
	mesh.material = material
	var instance: MeshInstance3D = MeshInstance3D.new()
	instance.name = "Obstacle%02d" % index
	instance.mesh = mesh
	# An AABB is min-corner + size; a BoxMesh is centred on its origin. Placing it at the centre is the whole
	# conversion, and getting it wrong puts the visual half a box away from the thing units actually hit.
	instance.position = box.position + box.size * 0.5
	add_child(instance)

func _add_light() -> void:
	var sun: DirectionalLight3D = DirectionalLight3D.new()
	sun.name = "Sun"
	sun.rotation = Vector3(deg_to_rad(-58.0), deg_to_rad(38.0), 0.0)
	sun.light_energy = 1.1
	add_child(sun)
	# A little ambient so the unlit faces of a unit are not solid black -- with flat-shaded primitives and one
	# directional light, half of every capsule is otherwise unreadable against the ground.
	var environment: Environment = Environment.new()
	environment.background_mode = Environment.BG_COLOR
	environment.background_color = Color(0.07, 0.08, 0.10)
	environment.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	environment.ambient_light_color = Color(0.42, 0.46, 0.55)
	environment.ambient_light_energy = 0.55
	var world_environment: WorldEnvironment = WorldEnvironment.new()
	world_environment.name = "Environment"
	world_environment.environment = environment
	add_child(world_environment)
