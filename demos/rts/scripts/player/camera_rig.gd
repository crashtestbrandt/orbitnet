extends Node3D
class_name CameraRig
## The RTS camera: a pivot on the ground with an angled camera pulled back from it. Pan with WASD or by
## pushing the pointer into a screen edge, zoom with the wheel.
##
## Entirely client-local -- the camera is not game state, is not replicated, and has no bearing on what the
## server simulates. It is listed here only because it owns the two projections everything else needs:
## world -> screen (for box selection) and screen -> ground (for issuing orders).
##
## Input is read from RAW KEYS rather than through the project's input map. That is a deliberate
## simplification for a netcode demo: an InputMap is a rebinding feature, it is verbose to define in
## project.godot, and it would be the single most likely thing to break when this project is opened in a
## different Godot version. A real game should use one.

## Meters per second of pan at the default zoom. Pan speed scales with height, so the camera covers the same
## fraction of the visible field per second whether zoomed in or out -- panning that feels fast up close and
## glacial when zoomed out is the classic RTS camera mistake.
const PAN_SPEED: float = 34.0
const EDGE_MARGIN_PX: float = 12.0
const ZOOM_MIN: float = 18.0
const ZOOM_MAX: float = 95.0
const ZOOM_STEP: float = 6.0
const PITCH_DEGREES: float = -52.0

var camera: Camera3D = null
var _height: float = 52.0
var _edge_pan_enabled: bool = true

func _ready() -> void:
	camera = Camera3D.new()
	camera.name = "Camera"
	camera.fov = 60.0
	camera.far = 600.0
	add_child(camera)
	camera.current = true
	_apply()

func _process(delta: float) -> void:
	var pan: Vector2 = _pan_axis()
	if pan != Vector2.ZERO:
		# Scale with height so the on-screen pan rate is constant across the zoom range.
		var scale: float = _height / 52.0
		var move: Vector3 = Vector3(pan.x, 0.0, pan.y) * PAN_SPEED * scale * delta
		position = UnitSteering.clamp_to_field(position + move, 0.0)
		_apply()

func _unhandled_input(event: InputEvent) -> void:
	if event is InputEventMouseButton:
		var button: InputEventMouseButton = event
		if not button.pressed:
			return
		if button.button_index == MOUSE_BUTTON_WHEEL_UP:
			_zoom(-ZOOM_STEP)
		elif button.button_index == MOUSE_BUTTON_WHEEL_DOWN:
			_zoom(ZOOM_STEP)

func _zoom(amount: float) -> void:
	_height = clampf(_height + amount, ZOOM_MIN, ZOOM_MAX)
	_apply()

func _apply() -> void:
	if camera == null:
		return
	# The camera sits behind and above the pivot, looking down at PITCH_DEGREES. Composed rather than using
	# look_at so the angle is exactly constant -- look_at drifts as the pivot approaches the camera's own
	# position, which happens at maximum zoom-in.
	var pitch: float = deg_to_rad(PITCH_DEGREES)
	camera.position = Vector3(0.0, _height, _height * 0.72)
	camera.rotation = Vector3(pitch, 0.0, 0.0)

func _pan_axis() -> Vector2:
	var axis: Vector2 = Vector2.ZERO
	if Input.is_physical_key_pressed(KEY_A) or Input.is_physical_key_pressed(KEY_LEFT):
		axis.x -= 1.0
	if Input.is_physical_key_pressed(KEY_D) or Input.is_physical_key_pressed(KEY_RIGHT):
		axis.x += 1.0
	if Input.is_physical_key_pressed(KEY_W) or Input.is_physical_key_pressed(KEY_UP):
		axis.y -= 1.0
	if Input.is_physical_key_pressed(KEY_S) or Input.is_physical_key_pressed(KEY_DOWN):
		axis.y += 1.0
	if axis == Vector2.ZERO and _edge_pan_enabled:
		axis = _edge_axis()
	return axis.normalized() if axis.length() > 1.0 else axis

func _edge_axis() -> Vector2:
	var viewport: Viewport = get_viewport()
	if viewport == null:
		return Vector2.ZERO
	var size: Vector2 = viewport.get_visible_rect().size
	var mouse: Vector2 = viewport.get_mouse_position()
	# Ignore a pointer that is outside the window entirely: an unfocused window reports a stale position, and
	# a camera that pans on its own while you are reading the terminal in the other window is maddening.
	if mouse.x < 0.0 or mouse.y < 0.0 or mouse.x > size.x or mouse.y > size.y:
		return Vector2.ZERO
	var axis: Vector2 = Vector2.ZERO
	if mouse.x < EDGE_MARGIN_PX:
		axis.x -= 1.0
	elif mouse.x > size.x - EDGE_MARGIN_PX:
		axis.x += 1.0
	if mouse.y < EDGE_MARGIN_PX:
		axis.y -= 1.0
	elif mouse.y > size.y - EDGE_MARGIN_PX:
		axis.y += 1.0
	return axis

## Turn edge panning off (the HUD does this while a modal readout has focus).
func set_edge_pan(on: bool) -> void:
	_edge_pan_enabled = on

## Snap the camera over a point (used at spawn so a player starts looking at their own army).
func look_at_point(point: Vector3) -> void:
	position = UnitSteering.clamp_to_field(point, 0.0)
	_apply()

## Where a screen position lands on the ground plane. The order-issuing projection.
func ground_at_screen(screen: Vector2) -> Vector3:
	if camera == null:
		return Vector3.ZERO
	return SelectionMath.ground_point(
		camera.project_ray_origin(screen), camera.project_ray_normal(screen), position)

## Where the pointer is on the ground. This is what the commander's replicated cursor tracks.
func ground_under_pointer() -> Vector3:
	var viewport: Viewport = get_viewport()
	if viewport == null:
		return position
	return ground_at_screen(viewport.get_mouse_position())

## Project a world point to screen coordinates, for box selection. Returns a point far off-screen for
## anything behind the camera -- `unproject_position` mirrors those onto the visible area, which would make
## units behind the camera selectable by a box in front of it.
func screen_of(world: Vector3) -> Vector2:
	if camera == null:
		return Vector2(-10000.0, -10000.0)
	if camera.is_position_behind(world):
		return Vector2(-10000.0, -10000.0)
	return camera.unproject_position(world)
