extends Node
## The addon LOAD smoke, in a project that contains nothing else.
##
## This is the check that catches the class of failure nobody thinks to test for: the GDScript loaded, the
## editor was happy, and the native library never actually opened -- because the binary was a Git LFS pointer,
## or was built for the wrong architecture, or the .gdextension entry points at a filename that no longer
## exists. Every one of those presents as "the game runs but nothing replicates", which is a very expensive
## thing to debug from the far end.
##
## It runs as the harness project's main scene, so `just harness-smoke` is a plain headless launch.
##
## Note what it does NOT do: it never names a backend class. That is the net-check gate's rule and it applies
## here too. It asserts what a CONSUMER can observe -- that `Net` exists, is OFFLINE, hands back inert
## handles, and that the facade's own metrics surfaces answer.

func _ready() -> void:
	var failures: PackedStringArray = PackedStringArray()

	# 1. The plugin registered the autoload.
	if not is_instance_valid(Net):
		failures.push_back("the `Net` autoload does not exist -- is addons/orbitnet enabled?")
		_finish(failures)
		return

	# 2. The facade boots OFFLINE. Anything else means a session started itself, which nothing should.
	if Net.current_mode() != Net.Mode.OFFLINE:
		failures.push_back("the facade booted in %s, not OFFLINE" % Net.mode_name(Net.current_mode()))

	# 3. The native extension actually loaded. Probed WITHOUT naming a backend class: if the library failed
	#    to open, the facade's backend node is missing and these degrade -- current_tick() would still answer
	#    0 offline, so the real tell is that the facade constructed at all and its settings round-trip.
	var before: int = Net.tickrate()
	Net.set_tickrate(37)
	var after: int = Net.tickrate()
	Net.set_tickrate(before)
	if after != 37:
		failures.push_back("tickrate did not round-trip through the backend (got %d) -- the native "
			% after + "extension almost certainly failed to load")

	# 4. Every lane hands back an inert-but-usable handle offline.
	var probe: Node3D = Node3D.new()
	probe.name = "SmokeProbe"
	add_child(probe)
	var state: NetStateHandle = Net.make_state(probe)
	var interp: NetInterpolatorHandle = Net.make_interpolator(probe)
	var rollback: NetRollbackHandle = Net.make_rollback(probe)
	if state == null or interp == null or rollback == null:
		failures.push_back("a lane returned null instead of an inert handle")
	elif state.is_active() or interp.is_active() or rollback.is_active():
		failures.push_back("a lane returned an ACTIVE handle while OFFLINE")

	# 5. The transport factory resolves to something concrete.
	if NetTransport.preferred_kind() == NetTransport.Kind.OFFLINE:
		failures.push_back("the transport factory could not name a transport for this build")

	print("ORBITNET-SMOKE plugin=ok mode=%s transport=%s" % [
		Net.mode_name(Net.current_mode()), NetTransport.preferred_kind_name()])
	_finish(failures)

func _finish(failures: PackedStringArray) -> void:
	if failures.is_empty():
		print("ORBITNET-SMOKE OK")
		get_tree().quit(0)
		return
	for reason: String in failures:
		printerr("ORBITNET-SMOKE FAIL %s" % reason)
	get_tree().quit(1)
