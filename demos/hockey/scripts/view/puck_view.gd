extends MeshInstance3D
class_name PuckView
## Draws the puck, and absorbs the server's corrections so a player sees a puck rather than a stutter.
##
## THE CORRECTION IS NOT THE MOTION. A predicted puck travels up to 100 mm per tick, so smoothing its position
## toward the simulation would just render it lagging behind itself. What has to be smoothed is the
## DISCONTINUITY: the difference between where this peer expected the puck to be by now and where the
## authoritative row says it is.
##
## So the view carries an OFFSET. Each net tick it extrapolates the previous pose forward and compares:
##
##   correction = actual - (previous position + previous velocity * dt)
##   offset    -= correction          # the drawn puck does not move at all this frame
##   offset    *= decay               # and the offset bleeds away over the next few frames
##
## That extrapolation is a straight line while the simulation damps and substeps, so every tick disagrees by a
## fraction of a millimeter whether or not anything was corrected. HockeyConfig.CORRECTION_DEADBAND_M is what
## keeps those out of the counters -- without it the blended count climbs once per tick forever, including
## offline, where nothing is being corrected at all.
##
## Two discontinuities are the simulation's own and must NOT be absorbed, or the puck would render passing
## through a rail and sliding back into it:
##
##   * a rail or mallet contact -- PuckPhysics reports the count, which is why State carries it;
##   * a face-off, where the puck is teleported to the center spot.
##
## Anything above HockeyConfig.CORRECTION_SNAP_M snaps as well: past that the blend would be a visible slide,
## and a slide reads as the puck being somewhere it is not. The smooth and snap counts are what
## BenchSubject.KEY_RECONCILE_SMOOTH and KEY_RECONCILE_SNAP were defined for.

var puck: PuckBody = null

## Whether corrections are blended away. The F5 lever. Turning it off leaves the puck exactly where the wire
## put it, which is the only way to SEE what the blend is hiding -- the counters keep running either way, so
## the numbers do not move when the presentation does.
var smoothing: bool = true

var _offset: Vector3 = Vector3.ZERO
var _previous_position: Vector3 = Vector3.ZERO
var _previous_velocity: Vector3 = Vector3.ZERO
var _previous_live: bool = false
var _last_tick: int = -1
var _smoothed: int = 0
var _snaps: int = 0
var _primed: bool = false

func _init() -> void:
	name = "PuckView"

func build(body: PuckBody) -> void:
	puck = body
	var shape: CylinderMesh = CylinderMesh.new()
	shape.top_radius = HockeyConfig.PUCK_RADIUS
	shape.bottom_radius = HockeyConfig.PUCK_RADIUS
	shape.height = 0.018
	shape.radial_segments = 24
	shape.rings = 1
	mesh = shape
	material_override = TableView.unshaded(Color(0.94, 0.95, 0.98))
	_reset_to(puck.net_pos)

func _physics_process(delta: float) -> void:
	if puck == null:
		return
	# The puck's own tick, not the facade's: Net.current_tick() is pinned at 0 offline, so gating on it there
	# ran this every physics frame and counted the gap between two frames that shared one tick as a correction.
	var tick: int = puck.sim_tick()
	if tick != _last_tick:
		_last_tick = tick
		_absorb()
	# The offset bleeds away on the RENDER clock, not the tick clock, so the blend takes the same wall-clock
	# time whatever the F1 lever has done to the tick rate.
	_offset *= pow(0.5, delta / maxf(0.001, HockeyConfig.CORRECTION_HALF_LIFE))
	var applied: Vector3 = _offset if smoothing else Vector3.ZERO
	position = puck.net_pos + applied + Vector3(0.0, 0.014, 0.0)

## Corrections the view blended away, monotonic.
func smoothed() -> int:
	return _smoothed

## Corrections too large to blend, monotonic. A snap is a visible teleport, which is why netbench gates on this
## one rather than on the magnitudes.
func snaps() -> int:
	return _snaps

# --- internals -------------------------------------------------------------------------------------
func _absorb() -> void:
	var live: bool = puck.is_live()
	var actual: Vector3 = puck.net_pos
	if not _primed or live != _previous_live or not live or puck.contacts() > 0:
		# A face-off, a serve, or a bounce. The simulation meant it.
		_reset_to(actual)
		_previous_live = live
		return
	var expected: Vector3 = _previous_position + _previous_velocity * _tick_dt()
	var correction: Vector3 = actual - expected
	var magnitude: float = correction.length()
	if magnitude > HockeyConfig.CORRECTION_SNAP_M:
		_offset = Vector3.ZERO
		_snaps += 1
	elif magnitude > HockeyConfig.CORRECTION_DEADBAND_M:
		_offset -= correction
		_smoothed += 1
	_previous_position = actual
	_previous_velocity = puck.net_vel
	_previous_live = live

func _reset_to(at: Vector3) -> void:
	_offset = Vector3.ZERO
	_previous_position = at
	_previous_velocity = puck.net_vel if puck != null else Vector3.ZERO
	_primed = true

# The net tick length. Net.net_tick_dt() reports 0 while OFFLINE (no loop is running), and multiplying a
# velocity by zero would make every offline frame look like a correction of exactly one tick of travel.
func _tick_dt() -> float:
	var dt: float = Net.net_tick_dt()
	if dt > 0.0:
		return dt
	return 1.0 / float(HockeyConfig.NET_TICK_HZ)
