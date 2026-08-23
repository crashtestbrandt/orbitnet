extends Control
class_name SplitView
## One viewport per local seat, and the cameras that look through them.
##
## THE SECOND VIEWPORT IS WHAT A SEAT IS FOR. A connection driving two fighters is two players on one socket,
## and the second player's surroundings are not the first player's -- which is exactly what the backend's seat
## index says, and why a connection receives the UNION of its seats' interest sets rather than one seat's.
## Without a second viewport the feature would be invisible: the union would still be correct and nothing on
## screen would show it.
##
## THE LAYOUT IS PURE and lives in `rects()`, so the split arithmetic is a unit test rather than something to
## squint at. Everything else here is Godot plumbing.

var _containers: Array[SubViewportContainer] = []
var _viewports: Array[SubViewport] = []
var _cameras: Array[Camera3D] = []

## The screen rectangles for `count` seats inside `size`. One seat takes the whole screen; two split it
## vertically, left seat first.
##
## A VERTICAL SPLIT RATHER THAN A HORIZONTAL ONE, because these arenas are wider than they are deep and a
## letterboxed half would show less of the arena than a narrow one does.
static func rects(count: int, size: Vector2) -> Array[Rect2]:
	var out: Array[Rect2] = []
	if count <= 1:
		out.push_back(Rect2(Vector2.ZERO, size))
		return out
	var half: float = size.x * 0.5
	out.push_back(Rect2(Vector2.ZERO, Vector2(half, size.y)))
	out.push_back(Rect2(Vector2(half, 0.0), Vector2(half, size.y)))
	return out

func build(count: int) -> void:
	set_anchors_preset(Control.PRESET_FULL_RECT)
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	_clear()
	for index: int in maxi(1, count):
		var container: SubViewportContainer = SubViewportContainer.new()
		container.name = "View%d" % index
		container.stretch = true
		container.mouse_filter = Control.MOUSE_FILTER_IGNORE
		add_child(container)

		var viewport: SubViewport = SubViewport.new()
		viewport.name = "Viewport"
		viewport.handle_input_locally = false
		# The world is shared: both viewports look into the same 3D scene from different places, which is what
		# split-screen is. A second World3D would be a second simulation.
		viewport.world_3d = get_viewport().world_3d
		container.add_child(viewport)

		var camera: Camera3D = Camera3D.new()
		camera.name = "Camera"
		camera.fov = 62.0
		camera.current = true
		viewport.add_child(camera)

		_containers.push_back(container)
		_viewports.push_back(viewport)
		_cameras.push_back(camera)
	_layout()

func _notification(what: int) -> void:
	if what == NOTIFICATION_RESIZED:
		_layout()

func _layout() -> void:
	var layout: Array[Rect2] = rects(_containers.size(), size)
	for index: int in _containers.size():
		if index >= layout.size():
			continue
		_containers[index].position = layout[index].position
		_containers[index].size = layout[index].size

## Point view `index` at a fighter, from behind and above it.
func look_at_fighter(index: int, at: Vector3, facing: Vector3) -> void:
	if index < 0 or index >= _cameras.size():
		return
	var flat: Vector3 = FighterMotion.clamp_aim(facing)
	var eye: Vector3 = at - flat * 9.0 + Vector3(0.0, 7.5, 0.0)
	var camera: Camera3D = _cameras[index]
	camera.position = eye
	camera.look_at(at + Vector3(0.0, 1.0, 0.0), Vector3.UP)

## Point view `index` straight down at a point -- the observer's overhead view.
func look_down_at(index: int, at: Vector3, height: float) -> void:
	if index < 0 or index >= _cameras.size():
		return
	var camera: Camera3D = _cameras[index]
	camera.position = at + Vector3(0.0, height, 0.001)
	camera.look_at(at, Vector3.FORWARD)

func camera_count() -> int:
	return _cameras.size()

func _clear() -> void:
	for container: SubViewportContainer in _containers:
		container.queue_free()
	_containers.clear()
	_viewports.clear()
	_cameras.clear()
