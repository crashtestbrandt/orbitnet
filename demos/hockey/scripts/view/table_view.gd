extends Node3D
class_name TableView
## The rink you look at: the inclined board, the rails, the goal mouths, and the one fixed camera.
##
## ENTIRELY CLIENT-LOCAL, and separate from RinkDirector on purpose. RinkDirector's node graph is the thing
## entity ids are hashed from, so it holds netcode nodes and nothing else and sits at the identity transform.
## The incline lives HERE, on a node the wire has never heard of, which is what lets TableGeometry and
## PuckPhysics be plain 2D math over an axis-aligned rectangle.
##
## The camera is a sibling of the board rather than a child of it -- it has to stay in world space, or tilting
## the table would tilt the view with it and nothing would have moved on screen.

## The tilted, possibly half-turned node every mesh and every body's render position hangs off.
var board: Node3D = null
var camera: Camera3D = null

var _team: int = 0
var _aspect: float = 0.0

func _init() -> void:
	name = "TableView"

func build(team: int) -> void:
	board = Node3D.new()
	board.name = "Board"
	add_child(board)

	camera = Camera3D.new()
	camera.name = "Camera"
	camera.fov = HockeyConfig.CAMERA_FOV_DEGREES
	camera.near = 0.05
	camera.far = 40.0
	add_child(camera)
	camera.current = true

	_build_surface()
	set_viewpoint(team)

## Point the board at `team`'s end of the table. Called once when a seat is assigned, and again if the seat
## changes; never during play. A spectator gets team 0's view.
func set_viewpoint(team: int) -> void:
	_team = 1 if team == 1 else 0
	if board != null:
		board.basis = TableFraming.viewpoint_basis(
			_team, deg_to_rad(HockeyConfig.TABLE_TILT_DEGREES))
	_apply_framing(true)

func _process(_delta: float) -> void:
	_apply_framing(false)

# Re-solve the camera whenever the viewport aspect changes. Under KEEP_HEIGHT a narrower window has less
# horizontal room, so a framing solved at 16:9 and left alone would crop the rails off a tall window.
func _apply_framing(force: bool) -> void:
	if camera == null:
		return
	var viewport: Viewport = get_viewport()
	if viewport == null:
		return
	var size: Vector2 = viewport.get_visible_rect().size
	if size.y <= 0.0:
		return
	var aspect: float = size.x / size.y
	if not force and absf(aspect - _aspect) < 0.001:
		return
	_aspect = aspect
	camera.transform = TableFraming.solve(_team, aspect)

# --- the board ---------------------------------------------------------------------------------------
func _build_surface() -> void:
	const RAIL_HEIGHT: float = 0.05
	const RAIL_THICKNESS: float = 0.03
	var length: float = HockeyConfig.HALF_LENGTH * 2.0
	var width: float = HockeyConfig.HALF_WIDTH * 2.0

	_slab("Surface", Vector3(width, 0.012, length), Vector3.ZERO, Color(0.09, 0.13, 0.20))
	_slab("CenterLine", Vector3(width, 0.014, 0.006), Vector3(0.0, 0.001, 0.0), Color(0.30, 0.36, 0.46))
	_ring("CenterSpot", Color(0.30, 0.36, 0.46))

	# Side rails run the full length; end rails are two segments each, leaving the goal mouth open. The gap is
	# the same GOAL_HALF_WIDTH the simulation reflects against, read from the same constant, so what you see is
	# provably what the puck bounces off.
	var side_x: float = HockeyConfig.HALF_WIDTH + RAIL_THICKNESS * 0.5
	_slab("RailLeft", Vector3(RAIL_THICKNESS, RAIL_HEIGHT, length + RAIL_THICKNESS * 2.0),
		Vector3(-side_x, RAIL_HEIGHT * 0.5, 0.0), Color(0.55, 0.60, 0.68))
	_slab("RailRight", Vector3(RAIL_THICKNESS, RAIL_HEIGHT, length + RAIL_THICKNESS * 2.0),
		Vector3(side_x, RAIL_HEIGHT * 0.5, 0.0), Color(0.55, 0.60, 0.68))

	var segment: float = HockeyConfig.HALF_WIDTH - HockeyConfig.GOAL_HALF_WIDTH
	var segment_center: float = HockeyConfig.GOAL_HALF_WIDTH + segment * 0.5
	for team: int in 2:
		var z: float = TableGeometry.goal_line_z(team) + HockeyConfig.end_sign(team) * RAIL_THICKNESS * 0.5
		for side: int in 2:
			var x: float = segment_center if side == 0 else -segment_center
			_slab("Rail%d_%d" % [team, side], Vector3(segment, RAIL_HEIGHT, RAIL_THICKNESS),
				Vector3(x, RAIL_HEIGHT * 0.5, z), Color(0.55, 0.60, 0.68))
		# The mouth itself, painted in the defending team's color so which end is yours is readable at a
		# glance -- the only cue a fixed camera can give you.
		_slab("Goal%d" % team, Vector3(HockeyConfig.GOAL_HALF_WIDTH * 2.0, 0.016, 0.05),
			Vector3(0.0, 0.002, TableGeometry.goal_line_z(team) - HockeyConfig.end_sign(team) * 0.025),
			HockeyConfig.team_color(team))

func _slab(slab_name: String, size: Vector3, at: Vector3, color: Color) -> void:
	var mesh: BoxMesh = BoxMesh.new()
	mesh.size = size
	var instance: MeshInstance3D = MeshInstance3D.new()
	instance.name = slab_name
	instance.mesh = mesh
	instance.material_override = unshaded(color)
	instance.position = at
	board.add_child(instance)

func _ring(ring_name: String, color: Color) -> void:
	var mesh: TorusMesh = TorusMesh.new()
	mesh.inner_radius = 0.055
	mesh.outer_radius = 0.062
	var instance: MeshInstance3D = MeshInstance3D.new()
	instance.name = ring_name
	instance.mesh = mesh
	instance.material_override = unshaded(color)
	instance.position = Vector3(0.0, 0.002, 0.0)
	board.add_child(instance)

## An unshaded, optionally translucent material. Unshaded because this project renders under GL Compatibility
## on a software CI runner and nothing here is trying to look lit -- a flat color reads the table's geometry
## more clearly than a single default light would.
static func unshaded(color: Color) -> StandardMaterial3D:
	var material: StandardMaterial3D = StandardMaterial3D.new()
	material.albedo_color = color
	material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
	if color.a < 1.0:
		material.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	return material
