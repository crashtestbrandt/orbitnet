extends Node
class_name MalletController
## The local player's input: where the pointer is on the table, and the one key that asks for a serve.
##
## THE TARGET IS PUBLISHED FROM `Net.pre_tick`, not from `_process`. That signal fires once per net tick,
## BEFORE the backend records the tick's input, so the frame that goes on the wire is the pointer's position at
## the tick boundary rather than at whatever moment the last render frame happened to land. At a 60 Hz coupled
## tick on a 144 Hz display that is the difference between a mallet that tracks the pointer and one that
## tracks it a frame and a bit ago -- which is exactly the kind of latency this demo is meant to be measuring
## rather than adding.
##
## Offline the tick loop does not run and the signal never fires, so `_physics_process` publishes instead: one
## function, two clocks, the same shape RinkDirector uses for the simulation itself.
##
## Input is read from RAW KEYS and the raw pointer rather than through the project's input map. That is a
## deliberate simplification for a netcode demo: an InputMap is a rebinding feature, it is verbose to define in
## project.godot, and it would be the single most likely thing to break when this project is opened in a
## different Godot version. A real game should use one.

## Emitted when the local player asks for a serve, so the HUD can show the request alongside the answer.
signal serve_requested()

var rink: RinkDirector = null
var view: TableView = null

var _seat: int = -1
var _target: Vector3 = Vector3.ZERO
var _scripted: bool = false

func _init() -> void:
	name = "MalletController"

func _ready() -> void:
	if not Net.pre_tick.is_connected(_on_pre_tick):
		Net.pre_tick.connect(_on_pre_tick)

## Point the controller at a rink and a seat. `seat` below 0 means spectating: the pointer is still tracked for
## the HUD, but nothing is written to any input node.
func configure(director: RinkDirector, table_view: TableView, seat: int) -> void:
	rink = director
	view = table_view
	_seat = seat

## Hand the mallet over to (or back from) a scripted driver -- netbench's BenchSubject, which writes the same
## input node through the same setter.
##
## Without this the two fight: this controller publishes the pointer every net tick, and in a headless bench run
## the pointer reports (0, 0) forever, so live input would drag the mallet back to one fixed spot between every
## pair of bot frames. The bot would appear to do nothing and the run would report a reconcile error of exactly
## zero -- the signature of a perfectly behaved client.
func set_scripted(on: bool) -> void:
	_scripted = on

## The table-space point the pointer is currently over. Read by the HUD.
func target() -> Vector3:
	return _target

func _on_pre_tick(_tick: int) -> void:
	_publish()

func _physics_process(_delta: float) -> void:
	if Net.is_offline():
		_publish()

func _unhandled_input(event: InputEvent) -> void:
	if not (event is InputEventKey):
		return
	var key: InputEventKey = event
	if not key.pressed or key.echo:
		return
	if key.physical_keycode == KEY_SPACE and rink != null:
		rink.submit_serve()
		serve_requested.emit()

func _publish() -> void:
	if _scripted:
		return
	if view == null or view.board == null or view.camera == null:
		return
	var viewport: Viewport = get_viewport()
	if viewport == null:
		return
	var screen: Vector2 = viewport.get_mouse_position()
	_target = TableProjection.table_point(
		view.camera.project_ray_origin(screen),
		view.camera.project_ray_normal(screen),
		view.board.global_transform,
		_target)
	if rink == null or _seat < 0 or _seat >= rink.mallets.size():
		return
	var mallet: MalletBody = rink.mallets[_seat]
	if mallet == null or not mallet.is_occupied():
		return
	# The REQUEST goes on the wire unclamped except for finiteness; the server clamps it into this player's own
	# half inside the tick. Clamping here too would be a second copy of that rule to keep in step, and the
	# server would still have to do its own.
	if TableGeometry.is_finite_point(_target):
		mallet.set_local_target(_target)
