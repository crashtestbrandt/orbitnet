extends Node
## OrbitNet -- THE netcode facade, and the whole public surface of this addon. Registered as the `Net`
## autoload by the orbitnet EditorPlugin.
##
## This is the ONE file permitted to name rollback-backend symbols; the `just net-check` grep gate (CI) fails
## the build if a backend symbol is referenced anywhere else. That seam is not ceremony -- it is what let the
## backend be swapped wholesale (a vendored GDScript implementation out, the native Rust one in) as a
## one-file rewrite, and it is why your game can depend on `Net` without ever naming a synchronizer class.
##
## Backend: the OrbitNet native Rust GDExtension (`native/` sources, `addons/orbitnet_native/` binaries). The
## facade owns ONE backend node (created in _init) that runs the tick loop, the per-entity rollback scheduler,
## clock sync and the batched packet pump; the per-entity synchronizer / interpolator nodes are created by the
## factories at the bottom of this file, which hand back the typed handles [NetRollbackHandle],
## [NetStateHandle] and [NetInterpolatorHandle].
##
## THREE LANES, and picking the right one is most of what there is to learn:
##   ROLLBACK -- [method register_rollback_body]. For an entity whose owner authors CONTINUOUS per-tick input
##       and predicts locally. Costs a history ring plus per-tick compare and replay, per entity.
##   STATE -- [method make_state]. Server-authoritative values pushed each tick, no prediction, no restore.
##       A value written OUTSIDE the tick (by a [NetCommand] handler) survives here -- on the rollback lane
##       it would be clobbered by the next restore.
##   COMMAND -- [NetCommand]. Sparse, discrete, reliable, server-validated requests.
##
## At boot the facade is OFFLINE and every method no-ops, returning inert handles. That is deliberate and it
## is load-bearing: a single-player launch runs the exact same code path with no networking spun up at all, so
## "does it work offline" is not a separate mode to maintain.
##
## COUPLED vs DECOUPLED. Coupled mode runs the net tick AT the physics rate inside _physics_process (before
## the physics step), the body writes its pose every physics tick, and engine physics-interpolation renders
## it. Decoupled ([method set_net_tick_decoupled], net tick < physics tick) paces the loop off the wall clock
## instead; entities then interpolate between net ticks (see [method net_tick_factor] and
## [NetInterpolatorHandle]). The coupled clock never stretches -- a stretch != 1.0 slides tick boundaries
## across physics frames and renders as judder -- so clock error is absorbed by rare, hysteresis-gated
## whole-tick slews instead.

## Network role of this peer. OFFLINE at boot (offline demo); the session layer (#62) sets it.
## HOST = a listen server (authoritative server + a co-located local client).
enum Mode { OFFLINE, CLIENT, SERVER, HOST }

var _mode: Mode = Mode.OFFLINE

## The one backend node. Created in _init so it exists before any other autoload and before any scene node --
## its _physics_process therefore runs the net tick AHEAD of every body's physics step. Game code depends on
## that ordering: a rollback tick that re-queries the physics world (a shapecast at the restored pose) must
## run before the physics server steps, or it reads last tick's world.
var _orbit: OrbitNet = null

## Emitted once per network tick, BEFORE the backend records that tick's input. The owned body connects
## here to populate its replicated input frame in time to be captured + sent. Re-broadcasts the backend's
## per-tick "before_tick" so game code never names it. Never fires OFFLINE (the tick loop isn't running).
signal pre_tick(tick: int)

## Emitted once per network tick loop AFTER the rollback/resim finished: every body's state is now the
## authoritative present-tick value (the owner's freshly resimulated, a remote's freshly applied). Under the
## physics/net decouple a body captures its per-tick render pose here, then interpolates between successive
## captures each physics frame. Fires once per frame that ran >= 1 net tick; inert OFFLINE.
signal post_tick()

func _init() -> void:
	# _init, not _ready: the unit-test runner (--script SceneTree mode) calls into the facade before the
	# tree delivers _ready, and the OFFLINE contract must hold there too. Children attached here enter the
	# tree with the autoload, so the backend's own lifecycle is unchanged.
	_orbit = OrbitNet.new()
	_orbit.name = "OrbitNet"
	# Seed the backend from the [orbitnet] project-settings block. A typed local per setting: ProjectSettings
	# returns Variant, and the typed-GDScript rules ban direct casts.
	var sync_phys: bool = ProjectSettings.get_setting(&"orbitnet/sync_to_physics", true)
	var tickrate_cfg: int = ProjectSettings.get_setting(&"orbitnet/tickrate", 60)
	var history_cfg: int = ProjectSettings.get_setting(&"orbitnet/history_limit", 128)
	var stretch_cfg: float = ProjectSettings.get_setting(&"orbitnet/max_time_stretch", 1.05)
	_orbit.sync_to_physics = sync_phys
	_orbit.tickrate = tickrate_cfg
	_orbit.history_limit = history_cfg
	_orbit.max_stretch = stretch_cfg
	add_child(_orbit)
	# Bridge the backend's per-tick + post-loop signals into the facade signals. The backend signals only
	# fire while the tick loop runs (networked), so OFFLINE these connections are inert.
	_orbit.before_tick.connect(_on_backend_before_tick)
	_orbit.after_rollback_loop.connect(_on_backend_after_rollback_loop)

func _on_backend_before_tick(_delta: float, tick: int) -> void:
	pre_tick.emit(tick)

func _on_backend_after_rollback_loop() -> void:
	if _mode != Mode.OFFLINE:
		post_tick.emit()

## Install the NATIVE crash handler, appending reports to `<dir>/crash-native.log`. Returns false if it was
## already installed (or the path did not fit). `dir` must be an absolute, already-created directory.
##
## Not netcode -- it lives on this facade because for many games the backend cdylib is the only NATIVE binary
## a release export template loads, and Godot's own crash handler is DEBUG_ENABLED-only (so is
## NOTIFICATION_CRASH). A shipped build otherwise dies with nothing but a truncated log. See the crash module
## in native/crates/orbitnet-godot/src/crash.rs for what each platform can and cannot catch.
func install_native_crash_handler(dir: String) -> bool:
	if _orbit == null or dir.is_empty():
		return false
	# Probe before calling. The committed binary and the GDScript are refreshed on different schedules -- a
	# bisect, a PR branch, or any working copy that has not run `just native-install` can pair new GDScript
	# with an older binary. A backend method that is not there yet must degrade to "no native handler": a
	# diagnostics feature has no business erroring at boot.
	if not _orbit.has_method(&"install_crash_handler"):
		return false
	return _orbit.install_crash_handler(dir)

# --- physics/net decouple (#214) -----------------------------------------------------------------
## Run the network tick DECOUPLED from (slower than) the physics tick: the net loop paces off the wall clock at
## `tick_hz` in _process while physics stays at its project rate, so the per-second sim/collide-and-slide/state-
## broadcast/resim cost drops. Bodies then interpolate/extrapolate their render pose between net ticks
## (net_tick_factor). Set at a networked SESSION start, before the tick loop auto-starts. tick_hz == the physics
## rate is the COUPLED case -- handled by set_net_tick_coupled() instead.
func set_net_tick_decoupled(tick_hz: int) -> void:
	_orbit.sync_to_physics = false
	set_tickrate(tick_hz)

## Restore the coupled behaviour (net tick == physics tick). Called on session teardown so offline / a
## re-hosted coupled session runs physics-synced again.
func set_net_tick_coupled() -> void:
	_orbit.sync_to_physics = true

## Whether the net tick currently runs decoupled from the physics tick (#214). A body renders interpolated when
## true, and writes the authoritative pose directly (physics-synced) when false. False OFFLINE.
func is_decoupled() -> bool:
	return _mode != Mode.OFFLINE and not _orbit.sync_to_physics

## The 0..1 fraction between the previous and next net tick, for render interpolation under the decouple. 1.0
## when coupled (each physics frame IS a net tick, no sub-tick interpolation needed) and OFFLINE.
func net_tick_factor() -> float:
	if _mode == Mode.OFFLINE:
		return 1.0
	return _orbit.tick_factor()

## The net tick duration in seconds (the extrapolation/interpolation span). 0 OFFLINE.
func net_tick_dt() -> float:
	if _mode == Mode.OFFLINE:
		return 0.0
	return _orbit.tick_time()

# --- mode ----------------------------------------------------------------------------------------
## The current network mode. OFFLINE at boot; asserted by the #61 acceptance test (Net.current_mode() == OFFLINE).
func current_mode() -> Mode:
	return _mode

func is_offline() -> bool:
	return _mode == Mode.OFFLINE

## Lowercase name of a mode, for telemetry and logging. One definition, so every log line agrees.
func mode_name(mode: Mode) -> String:
	match mode:
		Mode.CLIENT: return "client"
		Mode.SERVER: return "server"
		Mode.HOST: return "host"
	return "offline"

## True when this peer runs the authoritative simulation (dedicated server or listen-server host).
func is_server() -> bool:
	return _mode == Mode.SERVER or _mode == Mode.HOST

## True when this peer renders a local owned player (client or listen-server host).
func is_client() -> bool:
	return _mode == Mode.CLIENT or _mode == Mode.HOST

## Set the network mode. The session layer owns this call. Switching to OFFLINE stops any running tick loop;
## switching INTO a networked mode starts it -- a server ticks immediately, a client handshakes first and
## ticks when the server's welcome lands -- so a session layer never calls start_tick_loop() itself.
##
## A peer must already be assigned to the SceneTree multiplayer before this is called.
func set_mode(mode: Mode) -> void:
	if mode == _mode:
		return
	# Stop the tick loop BEFORE flipping to OFFLINE: stop_tick_loop() guards on the current mode, so it must run
	# while still in the running mode or it would no-op and leave the backend ticking after teardown.
	if mode == Mode.OFFLINE:
		stop_tick_loop()
	_mode = mode
	_orbit.set_mode(int(mode))
	if mode != Mode.OFFLINE:
		start_tick_loop()

# --- tick loop -----------------------------------------------------------------------------------
## Start the networked tick loop. OFFLINE no-ops (the sim runs on the engine physics tick as today). A peer must
## already be assigned to the SceneTree multiplayer before calling this (the session layer guarantees it, #62).
func start_tick_loop() -> void:
	if _mode == Mode.OFFLINE:
		return
	_orbit.start()

## Stop the networked tick loop. Safe to call in any mode.
func stop_tick_loop() -> void:
	if _mode == Mode.OFFLINE:
		return
	_orbit.stop()

## The current authoritative network tick (0 while OFFLINE). The single source game code reads instead of
## touching the backend directly. Inside a tick or rollback handler this is the tick being run, matching the
## signal's own argument -- game code stamps captured state with it (weapon fire ticks).
func current_tick() -> int:
	if _mode == Mode.OFFLINE:
		return 0
	return _orbit.current_tick()

## The current network time in seconds (0 while OFFLINE), measured from the session's start and continuously
## synced to the server. The single source game code reads instead of touching the backend directly -- a SHARED
## clock every peer agrees on, so deterministic scripted motion (#140's moving platform) computes the same pose
## everywhere with no per-frame replication. Continuous + monotonic, but it can be re-stepped on a large
## local/server drift (a hard clock resync), so a consumer must tolerate the occasional small jump.
func current_time() -> float:
	if _mode == Mode.OFFLINE:
		return 0.0
	return _orbit.current_time()

## The backend ROLLBACK/resim tick -- the tick a rewindable's advance()/_rollback_tick is currently replaying,
## which during a resim is an OLDER tick than the frontier. Game code keys per-tick rollback history off this
## (e.g. #103's held-cat memo) so a resim restores the value recorded for that exact tick. Only meaningful
## inside the rollback loop; 0 OFFLINE (no loop). Named here only (the facade boundary).
func rollback_tick() -> int:
	if _mode == Mode.OFFLINE:
		return 0
	return _orbit.rollback_tick()

## Diagnostic (#75 net-probe): the live backend tickrate + sync_to_physics, so instrumentation can confirm the
## net tick runs at the physics rate (SPIKE B Option A) rather than the decoupled default.
func debug_timing() -> String:
	if _mode == Mode.OFFLINE:
		return "offline"
	return "tickrate=%d sync_to_physics=%s physics_tps=%d" % [
		_orbit.effective_tickrate(), str(_orbit.sync_to_physics), Engine.physics_ticks_per_second]

## The CONFIGURED network tickrate (Hz) -- the value the tick loop runs at when NOT synced to physics, and the
## value the join handshake advertises. The `net.tickrate` console cvar (#64) reads/writes this, so it
## round-trips. NOTE: with sync_to_physics the EFFECTIVE rate is the physics rate regardless of this value --
## debug_timing() reports the effective rate. Reading the configured value keeps the cvar a faithful round-trip.
func tickrate() -> int:
	return _orbit.tickrate

## Set the configured network tickrate (Hz), clamped to a sane range. The backend clamps identically, so the
## cvar round-trips. Under sync_to_physics this does not change the live tick (see tickrate()).
func set_tickrate(rate: int) -> void:
	_orbit.tickrate = clampi(rate, 1, 240)

# --- resim-depth knobs + perf diagnostics (#214) ---------------------------------------------------
# input_delay shrinks the unconfirmed window directly (input is stamped into the future); display_offset
# trades a little view delay for fewer visible corrections (it does NOT change resim cost). Both are plain
# backend properties now -- the old private-var + ProjectSettings-mirror reach-in is gone by construction.

## Ticks of intentional input delay (the #214 resim-depth knob). Read fresh by the backend at every input
## record/transmit, so a runtime change applies from the very next tick. Per-client; peers need not agree.
func input_delay() -> int:
	return _orbit.input_delay

func set_input_delay(ticks: int) -> void:
	_orbit.input_delay = clampi(ticks, 0, 32)

## Ticks of display offset: present a slightly older, more-confirmed tick so late corrections land before
## they are ever rendered. Purely presentation-side latency masking; resim depth is unchanged.
func display_offset() -> int:
	return _orbit.display_offset

func set_display_offset(ticks: int) -> void:
	_orbit.display_offset = clampi(ticks, 0, 32)

# #214 remote-resim lever: whether a client's LOCAL rollback loop carries REMOTE bodies. Default FALSE
# (exempt): remote bodies are display-only on non-owning clients -- they apply the latest authoritative
# server state each tick and render engine-interpolated at that DELAYED tick. TRUE un-exempts them: the
# client then predicts remote bodies FORWARD from their latest authoritative state with held input
# (dead-reckoning through the real sim). This needs no O(N^2) input broadcast: prediction extrapolates the
# last known state instead of replaying peer input.
# Per-client and live (the console cvar net.remote_resim flips it mid-session); peers need not agree.
var _remote_resim: bool = false

## Whether remote bodies ride this client's rollback loop. Meaningless on a server/host (it simulates every
## body, so nothing is ever exempt).
func remote_resim() -> bool:
	return _remote_resim

func set_remote_resim(on: bool) -> void:
	_remote_resim = on
	_orbit.set_remote_resim(on)

# #214 TEST HOOK: force the rollback LOOP at least this deep every tick. This deepens the loop's per-tick
# restore/record bookkeeping for every simulated body -- the resim-cost measurement lever the perf probe
# drives. Capped well under history_limit (128) so the backend never replays past evicted history. 0 = off.
func resim_force() -> int:
	return _orbit.resim_force

func set_resim_force(ticks: int) -> void:
	_orbit.resim_force = clampi(ticks, 0, 64)

## Interest-management radius in metres (#318/#328, the 100-player lever): with a radius set, the SERVER sends
## each peer only the rollback bodies within it of that peer's own body (1.25x exit hysteresis so boundary
## entities don't flicker; state-lane entities always replicate). 0 = off (every peer receives everything --
## the shipped default, since the demo arena fits inside any sensible radius). Server-side only; ignored on
## clients. The `net.aoi_radius` console cvar (server-marked) reads/writes this.
func aoi_radius() -> float:
	return _orbit.aoi_radius

func set_aoi_radius(metres: float) -> void:
	_orbit.aoi_radius = maxf(0.0, metres)

## Diagnostic (#214 net.perf): last-loop rollback counters from the backend. resim_ticks is the effective
## resim window depth (ticks re-simulated in the latest rollback loop). Live in EVERY build, release
## included -- the counters are a byproduct of the native loop, not debug monitors.
func perf_summary() -> String:
	if _mode == Mode.OFFLINE:
		return "offline (no rollback loop)"
	var m: Dictionary = _orbit.metrics()
	var resim: float = m.get("resim_ticks", 0.0)
	var rb_ms: float = m.get("rollback_ms", 0.0)
	var rb_nodes: float = m.get("rb_nodes", 0.0)
	var net_ms: float = m.get("net_ms", 0.0)
	return "resim_ticks=%d rollback_ms=%.2f rb_nodes=%d net_ms=%.2f input_delay=%d display_offset=%d resim_force=%d remote_resim=%d" % [
		int(resim), rb_ms, int(rb_nodes), net_ms,
		input_delay(), display_offset(), resim_force(), 1 if _remote_resim else 0]

## The raw counters behind perf_summary(), typed for harnesses (the #214 perf probe samples these each
## frame). Zeros OFFLINE.
func perf_metrics() -> Dictionary[String, float]:
	if _mode == Mode.OFFLINE:
		return {"resim_ticks": 0.0, "rollback_ms": 0.0, "net_ms": 0.0}
	var m: Dictionary = _orbit.metrics()
	var resim: float = m.get("resim_ticks", 0.0)
	var rb_ms: float = m.get("rollback_ms", 0.0)
	var net_ms: float = m.get("net_ms", 0.0)
	var rb_nodes: float = m.get("rb_nodes", 0.0)
	return {
		"resim_ticks": resim,
		"rollback_ms": rb_ms,
		"net_ms": net_ms,
		"rb_nodes": rb_nodes,
	}

## Diagnostic (loopback-stutter triage): the backend CLOCK state driving the tick cadence.
##   stretch  -- current sim-clock speed multiplier (1.0 = locked; pinned to exactly 1.0 in coupled mode,
##               where clock error is absorbed by rare whole-tick slews instead)
##   offset_ms -- estimated server-minus-local clock offset, ms (what stretch/slew close)
##   rtt_ms / jitter_ms -- the ping sampler's round-trip estimate feeding the offset filter
##   lead_ticks -- the adaptive-lead bias the margin loop has dialed in (extra ticks of clock lead)
## Zeros OFFLINE (no sync loop). Sampled per-physics-frame by the stutter probe; printed by net.perf.
func clock_metrics() -> Dictionary[String, float]:
	if _mode == Mode.OFFLINE:
		return {"stretch": 1.0, "offset_ms": 0.0, "rtt_ms": 0.0, "jitter_ms": 0.0, "lead_ticks": 0.0}
	var m: Dictionary = _orbit.metrics()
	var stretch: float = m.get("stretch", 1.0)
	var offset_ms: float = m.get("offset_ms", 0.0)
	var rtt_ms: float = m.get("rtt_ms", 0.0)
	var jitter_ms: float = m.get("jitter_ms", 0.0)
	var lead_ticks: float = m.get("lead_ticks", 0.0)
	return {
		"stretch": stretch,
		"offset_ms": offset_ms,
		"rtt_ms": rtt_ms,
		"jitter_ms": jitter_ms,
		"lead_ticks": lead_ticks,
	}

# --- rollback / state / interpolation handles (created here so the backend is named only here) ----
## Create a rollback handle for a predicted body (#63 owner prediction + reconciliation). OFFLINE returns an
## INERT handle (no synchronizer), so callers can wire the same code unconditionally. The backend synchronizer
## is created here and handed to the handle as an opaque Node so no game code names it.
func make_rollback(root: Node) -> NetRollbackHandle:
	if _mode == Mode.OFFLINE or root == null:
		return NetRollbackHandle.new(null)
	var sync: OrbitRollbackSynchronizer = OrbitRollbackSynchronizer.new()
	sync.name = "OrbitSync"
	sync.root = root
	root.add_child(sync)
	return NetRollbackHandle.new(sync)

## Create a state-replication handle for non-predicted, server-driven state (#63 remote avatars / events; #93
## holster containers). The synchronizer is SERVER-AUTHORITATIVE (default node authority = peer 1): the server
## extracts + broadcasts the registered props each tick, every other peer applies them -- with NO rollback
## restore, so a value set OUTSIDE the tick (e.g. a NetCommand handler) is not clobbered (unlike the rollback
## lane). `root` is both the synchronizer's parent AND its property-resolution root. OFFLINE returns an inert
## handle (the single-player path writes the props directly and they simply stick).
func make_state(root: Node) -> NetStateHandle:
	if _mode == Mode.OFFLINE or root == null:
		return NetStateHandle.new(null)
	var sync: OrbitStateSynchronizer = OrbitStateSynchronizer.new()
	sync.name = "OrbitState"
	sync.root = root   # set BEFORE add_child so process_settings resolves paths against the right root
	root.add_child(sync)
	return NetStateHandle.new(sync)

## Create a render interpolator for a state-lane entity. Returns a typed [NetInterpolatorHandle]; OFFLINE (or
## a null root) returns an INERT handle, so callers wire the same code unconditionally. The backend
## interpolator is created here and handed to the handle as an opaque Node so no game code names it.
##
## NOTE: under the coupled path the net tick == the physics tick, so a rollback body writes its pose every
## physics tick and Godot physics-interpolation renders it -- an interpolator is NOT used for player bodies
## (it would fight engine interp). This is for non-rollback replicated objects (weapons/projectiles/units)
## and the decoupled low-rate configuration, where a replicated entity would otherwise visibly step at the
## net tick.
##
## Returns a handle, not the bare Node, so a strictly-typed consumer can reach add_property() at all: a bare
## `Node` return forces an untyped method call, which is a compile ERROR under `unsafe_method_access=2`.
func make_interpolator(root: Node) -> NetInterpolatorHandle:
	if _mode == Mode.OFFLINE or root == null:
		return NetInterpolatorHandle.new(null)
	var interp: OrbitInterpolator = OrbitInterpolator.new()
	interp.name = "OrbitInterp"
	interp.root = root   # set BEFORE add_child so add_property resolves paths against the right root
	root.add_child(interp)
	return NetInterpolatorHandle.new(interp)

## Register a player body for owner prediction + reconciliation + remote interpolation (#63). Creates an
## OrbitRollbackSynchronizer on `root`, sets prediction per role, registers the serialized STATE props on
## `root`, the INPUT props on `input_node`, and the COSMETIC props (replicated but never restored / never a
## misprediction -- the #318 prop-role diet) on `root`, then processes settings -- all here so no game code
## names the backend. OFFLINE returns an inert handle, so the body wires this unconditionally and the offline
## demo runs untouched.
##
## SERVER-AUTHORITATIVE split (#63): `root`'s multiplayer authority is the SERVER, so the server owns every
## body's STATE -- it simulates each body from the received input and broadcasts the authoritative state --
## while `input_node`'s authority is the OWNING CLIENT, so each client authors only its OWN input (the
## backend validates the sender against that authority, the anti-forgery check). The owning client also
## predicts its body locally and RECONCILES against the server's broadcast state; a non-owning client applies
## that state and render-smooths it (remote interpolation). `predict` (enable_prediction) is true on the
## owning client (local prediction) and on the server (authoritative simulation of every body), false on a
## non-owning client (apply state only). Splitting state vs input authority requires input to live on its OWN
## node -- hence `input_node`, a child whose authority differs from the body's.
##
## There is no "one synchronizer type per body on every peer" constraint: replication routes by an entity id
## DERIVED from the root's node path rather than by RPC node paths -- but the
## property SETS must still be identical on every peer (the wire schema is positional; the backend hashes it
## and refuses to misapply a mismatch).
func register_rollback_body(root: Node, input_node: Node, state_properties: Array[String], input_properties: Array[String], predict: bool, cosmetic_properties: Array[String] = []) -> NetRollbackHandle:
	if _mode == Mode.OFFLINE or root == null or input_node == null:
		return NetRollbackHandle.new(null)
	var sync: OrbitRollbackSynchronizer = OrbitRollbackSynchronizer.new()
	sync.name = "OrbitSync"
	sync.root = root
	sync.input_authority_node = input_node
	sync.enable_prediction = predict   # the owner-prediction switch -- set here, the only legal place
	# #214: a non-predicting synchronizer exists only on a peer that owns neither this body's state nor its
	# input -- a display-only peer. Exempt it from the rollback loop (the default), unless the
	# net.remote_resim lever asked for remote prediction; the backend re-applies the lever live.
	if not predict:
		sync.exempt = not _remote_resim
	root.add_child(sync)
	for prop: String in state_properties:
		sync.add_state(root, prop)          # state on the body (server authority)
	for prop: String in cosmetic_properties:
		sync.add_cosmetic(root, prop)       # replicated, never restored, never a misprediction
	for prop: String in input_properties:
		sync.add_input(input_node, prop)    # input on the client-authority child (the server-auth split)
	sync.process_settings()
	return NetRollbackHandle.new(sync)
