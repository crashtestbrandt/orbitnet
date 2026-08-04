extends Node3D
class_name OrderMarkers
## A short-lived ring on the ground wherever you just ordered units to go. Pooled, client-local, and never
## replicated -- a marker is feedback about YOUR click, and nobody else's client needs to know you clicked.
##
## It is also, incidentally, the clearest demonstration of the order round trip that does not involve reading
## a number: the marker appears the instant you release the mouse, and the units turn when the server says so.
## The gap between those two events IS the order RTT. On a local host it is invisible; over a conditioned link
## it is obvious.

const POOL_SIZE: int = 12
const LIFETIME_S: float = 1.1

var _pool: Array[MeshInstance3D] = []
var _expiry: PackedFloat32Array = PackedFloat32Array()
var _next: int = 0
var _clock: float = 0.0

func build() -> void:
	name = "OrderMarkers"
	_expiry.resize(POOL_SIZE)
	var mesh: TorusMesh = TorusMesh.new()
	mesh.inner_radius = 0.9
	mesh.outer_radius = 1.25
	for index: int in POOL_SIZE:
		var material: StandardMaterial3D = StandardMaterial3D.new()
		material.shading_mode = BaseMaterial3D.SHADING_MODE_UNSHADED
		material.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
		var marker: MeshInstance3D = MeshInstance3D.new()
		marker.name = "Marker%02d" % index
		marker.mesh = mesh
		# A material PER INSTANCE, not one shared: each marker fades on its own schedule, and a shared
		# material would make them all fade together with the most recent one.
		marker.material_override = material
		marker.visible = false
		add_child(marker)
		_pool.push_back(marker)
		_expiry[index] = -1.0

func _process(delta: float) -> void:
	_clock += delta
	for index: int in _pool.size():
		if _expiry[index] < 0.0:
			continue
		var remaining: float = _expiry[index] - _clock
		if remaining <= 0.0:
			_pool[index].visible = false
			_expiry[index] = -1.0
			continue
		_fade(index, remaining / LIFETIME_S)

## Drop a marker. `verb` only picks the colour -- a move and an attack-move want to be told apart at a glance.
func spawn(verb: StringName, point: Vector3) -> void:
	if _pool.is_empty():
		return
	var index: int = _next
	_next = (_next + 1) % _pool.size()
	var marker: MeshInstance3D = _pool[index]
	marker.position = point + Vector3(0.0, 0.05, 0.0)
	marker.visible = true
	_expiry[index] = _clock + LIFETIME_S
	var colour: Color = Color(1.0, 0.35, 0.30) if verb == OrderValidator.VERB_ATTACK_MOVE \
		else Color(0.55, 0.95, 0.60)
	var material: StandardMaterial3D = _material_of(index)
	if material != null:
		material.albedo_color = colour
	_fade(index, 1.0)

func _fade(index: int, amount: float) -> void:
	var material: StandardMaterial3D = _material_of(index)
	if material == null:
		return
	var colour: Color = material.albedo_color
	colour.a = clampf(amount, 0.0, 1.0)
	material.albedo_color = colour
	# Expand slightly as it fades, so a marker landing on top of an older one still reads as two events.
	var scale: float = 1.0 + (1.0 - clampf(amount, 0.0, 1.0)) * 0.45
	_pool[index].scale = Vector3(scale, 1.0, scale)

func _material_of(index: int) -> StandardMaterial3D:
	var override: Material = _pool[index].material_override
	if override is StandardMaterial3D:
		var typed: StandardMaterial3D = override
		return typed
	return null
