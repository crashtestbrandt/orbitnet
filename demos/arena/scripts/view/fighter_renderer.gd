extends Node3D
class_name FighterRenderer
## Draws the fighters, and draws what the netcode is doing to them.
##
## THE FADE IS THE FEATURE. A fighter this peer has stopped receiving is drawn faded rather than removed, and
## that is not a stylistic choice -- it is what a cull looks like. The rows stop, the node stays, and it
## freezes at its last pose; deleting it would be the demo hiding the one behaviour the interest filter has.
## The same fade covers all three axes, because from a client they are indistinguishable.
##
## A CLOAKED FIGHTER LOOKS THE SAME AS A CULLED ONE FROM THE OTHER TEAM, and that is the point of using a veto
## for it. Its own team, which is still being sent it, draws it solid with a tint.
##
## ONE MESH PER FIGHTER rather than a MultiMesh: twenty-four of them, each needing its own colour and its own
## transparency, and per-instance colour on a MultiMesh needs a custom material to read it.

const TEAM_COLOURS: Array[Color] = [
	Color(0.35, 0.62, 0.95),
	Color(0.95, 0.52, 0.32),
]
const CLOAK_TINT: Color = Color(0.62, 0.95, 0.72)
const CULLED_ALPHA: float = 0.16
const DEAD_ALPHA: float = 0.30

var world: MatchDirector = null

var _meshes: Array[MeshInstance3D] = []
var _materials: Array[StandardMaterial3D] = []

func build(director: MatchDirector) -> void:
	name = "FighterRenderer"
	world = director
	for seat: int in ArenaConfig.SEAT_COUNT:
		var mesh: MeshInstance3D = MeshInstance3D.new()
		mesh.name = "FighterView%03d" % seat
		var capsule: CapsuleMesh = CapsuleMesh.new()
		capsule.radius = ArenaConfig.FIGHTER_RADIUS
		capsule.height = ArenaConfig.FIGHTER_HEIGHT
		mesh.mesh = capsule

		var material: StandardMaterial3D = StandardMaterial3D.new()
		material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
		material.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
		material.albedo_color = TEAM_COLOURS[ArenaConfig.team_of_seat(seat) % TEAM_COLOURS.size()]
		mesh.material_override = material

		add_child(mesh)
		_meshes.push_back(mesh)
		_materials.push_back(material)

func _process(_delta: float) -> void:
	if world == null:
		return
	var now: int = world.current_tick()
	for seat: int in mini(_meshes.size(), world.fighters.size()):
		var fighter: FighterBody = world.fighters[seat]
		var mesh: MeshInstance3D = _meshes[seat]
		if fighter == null or mesh == null:
			continue
		mesh.position = fighter.position + Vector3(0.0, ArenaConfig.FIGHTER_HEIGHT * 0.5, 0.0)
		# Face where it is aiming. The capsule is rotationally symmetric, so this is only visible through the
		# aim marker below -- but the marker is what makes a rewind legible: a target's aim at the tick a shot
		# was resolved against is the thing a shooter was leading.
		var aim: Vector3 = FighterMotion.clamp_aim(fighter.net_aim)
		mesh.look_at(mesh.position + aim, Vector3.UP)
		mesh.rotate_object_local(Vector3.RIGHT, PI * 0.5)
		_materials[seat].albedo_color = _colour_for(fighter, now)

func _colour_for(fighter: FighterBody, now: int) -> Color:
	var base: Color = TEAM_COLOURS[fighter.team % TEAM_COLOURS.size()]
	if fighter.is_cloaked():
		base = base.lerp(CLOAK_TINT, 0.55)
	var alpha: float = 1.0
	if not fighter.is_alive():
		alpha = DEAD_ALPHA
	# A body this peer has stopped being sent. It reads the RECEIPT rather than the frontier, so the branch is
	# a client's by construction rather than by accident: a server authors every row and receives none, so its
	# receipt is -1 for every body and the guard above is what keeps it from fading the whole world.
	# `is_receiving()` rather than a threshold on the raw tick: it folds in both short-circuits -- the
	# authority, which receives nothing and would otherwise fade the whole world on a host, and a backend too
	# old to report a receipt, which answers -1 for every body and would fade it for a different reason. A
	# threshold written here would fail CLOSED in both cases; this fails open.
	if not fighter.is_receiving(InterestMeter.STALE_TICKS):
		alpha = CULLED_ALPHA
	base.a = alpha
	return base
