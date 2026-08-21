extends MultiMeshInstance3D
class_name MalletRenderer
## Draws the mallet pool. One MultiMesh, so thirty-two replicated bodies cost one draw call and a MalletBody
## carries netcode state and nothing else -- no mesh, no material, no children but its input node and its
## synchronizer. The netcode entity and the render representation are deliberately separable.
##
## MALLETS DO NOT COLLIDE WITH EACH OTHER, so two team-mates can stand in the same place, and the one in front
## would hide the one behind exactly when a player most needs to see their own. Rather than push them apart --
## which would put a rule in the simulation to solve a drawing problem, and a rule every peer would then have
## to predict -- every mallet but your own FADES as it approaches yours. Presentation-only: it costs no wire
## bytes, cannot mispredict, and never fully hides anybody.

var rink: RinkDirector = null
var session: HockeyNet = null

## Whether a team-mate's mallet fades as it approaches yours. The F6 lever, so the crowding it solves can be
## seen as well as described.
var fade: bool = true

func _init() -> void:
	name = "MalletRenderer"

func build(director: RinkDirector, net: HockeyNet) -> void:
	rink = director
	session = net

	var mesh: CylinderMesh = CylinderMesh.new()
	mesh.top_radius = HockeyConfig.MALLET_RADIUS
	mesh.bottom_radius = HockeyConfig.MALLET_RADIUS * 0.86
	mesh.height = 0.034
	mesh.radial_segments = 20
	mesh.rings = 1

	var pool: MultiMesh = MultiMesh.new()
	pool.transform_format = MultiMesh.TRANSFORM_3D
	pool.use_colors = true
	pool.mesh = mesh
	pool.instance_count = HockeyConfig.SEATS
	multimesh = pool

	var material: StandardMaterial3D = TableView.unshaded(Color(1.0, 1.0, 1.0, 0.99))
	material.vertex_color_use_as_albedo = true
	material_override = material

func _process(_delta: float) -> void:
	if rink == null or multimesh == null:
		return
	var local_seat: int = -1 if session == null else session.local_seat()
	var mine: Vector3 = Vector3.ZERO
	var have_mine: bool = false
	if local_seat >= 0 and local_seat < rink.mallets.size():
		var own: MalletBody = rink.mallets[local_seat]
		if own != null and own.is_occupied():
			mine = own.net_pos
			have_mine = true

	for seat: int in mini(HockeyConfig.SEATS, rink.mallets.size()):
		var mallet: MalletBody = rink.mallets[seat]
		if mallet == null or not mallet.is_occupied():
			# A vacant seat is drawn at zero scale rather than skipped: MultiMesh instances are addressed by
			# index, so hiding one by shuffling the list would renumber every mallet after it.
			multimesh.set_instance_transform(seat, Transform3D(Basis().scaled(Vector3.ZERO), Vector3.ZERO))
			multimesh.set_instance_color(seat, Color(0.0, 0.0, 0.0, 0.0))
			continue
		# The render position is read from `net_pos`, not from the node's transform. An exempt remote mallet
		# never runs its own tick at all, so its transform is whatever it was left at; `net_pos` is the
		# property the wire actually writes.
		var at: Vector3 = mallet.net_pos + Vector3(0.0, 0.024, 0.0)
		multimesh.set_instance_transform(seat, Transform3D(Basis(), at))
		var colour: Color = HockeyConfig.team_color(mallet.team())
		if seat == local_seat:
			# Your own mallet is always solid, and lighter than your team-mates' so it is findable in a crowd.
			colour = colour.lightened(0.28)
		elif have_mine and fade:
			colour.a = fade_alpha(mallet.net_pos.distance_to(mine))
		multimesh.set_instance_color(seat, colour)

## How opaque a mallet `distance` metres from the local player's own mallet should be drawn.
##
## Pure and static so the curve is unit-testable without a viewport. It never reaches zero: a mallet you cannot
## see at all is worse than one you can see through, because the puck still bounces off it.
static func fade_alpha(distance: float) -> float:
	if distance >= HockeyConfig.FADE_START:
		return 1.0
	var fraction: float = clampf(distance / maxf(0.0001, HockeyConfig.FADE_START), 0.0, 1.0)
	return lerpf(HockeyConfig.FADE_FLOOR, 1.0, fraction)
