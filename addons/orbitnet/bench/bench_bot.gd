extends Node
class_name BenchBot
## The thin Node driver that turns a headless client into a bot. Once per NET tick it asks the pure
## [BenchPolicy] for an input frame and pushes it through [method BenchSubject.apply_input] -- the same seam
## a recorded tape replays through and the same one the game's own scripted-input probes drive, so a bot
## exercises the real prediction + replication path (its input is recorded and replicated exactly like a
## human's). All behaviour math is in BenchPolicy (pure, unit-tested); this class owns only the wiring.
##
## Attached by [BenchProbe] from the `--bench-bot=<policy>` CLI flag; never present in shipped play.
##
## Drives on the NET tick, not the physics frame: under a decoupled configuration (net 20 Hz, physics
## 60 Hz) a per-frame drive would advance the policy clock three times per replicated input and the fleet
## would not reproduce across tickrates.

var policy: BenchPolicy.Policy = BenchPolicy.Policy.STRAFE
var seed: int = 1
var subject: BenchSubject = null

var _clock: float = 0.0    # the bot's own accumulated drive time (seconds); the policy is a pure function of it
var _last_tick: int = -1

func _ready() -> void:
	process_mode = Node.PROCESS_MODE_ALWAYS

func _physics_process(delta: float) -> void:
	if subject == null or not subject.is_ready():
		return
	var body: Node = subject.local_body()
	if body == null or not is_instance_valid(body):
		return
	# The policy clock is WALL time, accumulated every frame -- advancing it only on the tick edge below
	# would make it run at (net_hz / physics_hz) speed, so the same policy would trace a different path at
	# 20 Hz than at 60 Hz and a fleet's motion would stop being comparable across tickrates.
	_clock += delta
	# ...but the frame is APPLIED once per net tick. OFFLINE (or before the loop starts) current_tick() is
	# pinned at 0, so this drives exactly once and then waits -- correct, since a bot with no session has
	# nothing to drive.
	var tick: int = Net.current_tick()
	if tick == _last_tick:
		return
	_last_tick = tick
	subject.apply_input(BenchPolicy.frame(policy, _clock, seed))

## Stop driving and hand the body back to live input ([BenchProbe] calls this on teardown / when a replay
## takes over). An empty frame is the release signal in the [BenchSubject] contract.
func release() -> void:
	if subject != null:
		subject.apply_input({})
	subject = null
