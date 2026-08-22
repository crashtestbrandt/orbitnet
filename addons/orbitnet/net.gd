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

## Network role of this peer. OFFLINE at boot (offline demo); the session layer sets it.
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
	# The send path's two INDEPENDENT distance knobs. The fallbacks here are the "no config"
	# values, not the shipped policy -- the shipped numbers and the reasoning for them are in the
	# [orbitnet] block, and `aoi_weapon_range_test.gd` is what stops the radius drifting under a weapon.
	var aoi_cfg: float = ProjectSettings.get_setting(&"orbitnet/aoi_radius", 0.0)
	var aoi_band_cfg: float = ProjectSettings.get_setting(&"orbitnet/aoi_band_radius", 0.0)
	_orbit.sync_to_physics = sync_phys
	_orbit.tickrate = tickrate_cfg
	_orbit.history_limit = history_cfg
	_orbit.max_stretch = stretch_cfg
	_orbit.aoi_radius = maxf(0.0, aoi_cfg)
	_orbit.set(&"aoi_band_radius", maxf(0.0, aoi_band_cfg))
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

# --- backend-version tolerance -------------------------------------------------------------------
## Read a backend property that may not exist on the loaded binary yet, falling back to `fallback`.
##
## The committed cdylib is refreshed only when a release tag is cut (release.yml), while the Rust sources change
## on every merge to main. Every checkout between two releases therefore pairs new GDScript with an OLDER binary
## -- as does a bisect, or any working copy that has not run `just native-install`. install_native_crash_handler
## already guards for exactly this reason, and it is not hypothetical: the headless lint gate has failed on
## `aoi_max_entities`, `rate_tiering` and `aoi_band_radius`, each of them a property the Rust side had and the
## committed binary did not. A tuning or diagnostics knob has no business erroring at boot: it degrades to its
## default and the game runs.
##
## So a new backend property reaches GDScript through the helpers below and `Object.set`, never through
## `_orbit.<name>` directly. A direct read or write of a property the loaded binary lacks is a script error, and
## in _init that error lands on every project load, which is what the lint gate reports.
##
## `Object.get` answers null for an absent property rather than erroring, and `Object.set` is a silent no-op --
## which is why the write paths below need no guard of their own.
func _backend_int(name: StringName, fallback: int) -> int:
	var raw: Variant = _orbit.get(name)
	if raw == null:
		return fallback
	var value: int = raw
	return value

func _backend_bool(name: StringName, fallback: bool) -> bool:
	var raw: Variant = _orbit.get(name)
	if raw == null:
		return fallback
	var value: bool = raw
	return value

func _backend_float(name: StringName, fallback: float) -> float:
	var raw: Variant = _orbit.get(name)
	if raw == null:
		return fallback
	var value: float = raw
	return value

# --- physics/net decouple -----------------------------------------------------------------
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

## Whether the net tick currently runs decoupled from the physics tick. A body renders interpolated when
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
##
## **Anything that turns a count of NET ticks into seconds must use this**, and the engine's
## `Engine.physics_ticks_per_second` is never the right number for it. A session that calls
## [method set_net_tick_decoupled] runs the net tick at its own rate -- typically 60 -- while physics stays at
## 120. The `orbitnet/sync_to_physics` project setting is only the seed for an offline or sessionless process.
##
## The failure is silent in the worst way: a caller that counts ticks at 60/s and multiplies by 1/120 runs at
## half speed, EVERY PEER COMPUTES THE SAME WRONG NUMBER, so no sync or determinism gate can see it -- it is a
## uniform, agreed-upon error. The bug this prevents is projectiles flying at half speed and debris drifting at
## half speed on exactly that arithmetic, each carrying a comment asserting a premise the decouple had already
## retired. At the 30 Hz tick a 100-player target needs, the same code would be out by four.
##
## This is the twin of the rule [method effective_tickrate] states for milliseconds: a tick is not a fixed
## amount of time.
func net_tick_dt() -> float:
	if _mode == Mode.OFFLINE:
		return 0.0
	return _orbit.tick_time()

## The tick rate the loop is ACTUALLY running at -- the physics rate when coupled, the configured rate when
## decoupled. `tickrate()` returns the CONFIGURED value, which is the wrong one under `sync_to_physics`.
## Anything converting between ticks and milliseconds must use this: a tick is not a fixed
## amount of time. 0 OFFLINE. A passthrough to the backend method this file already calls in `clock_metrics`;
## it exists as a facade method so a caller outside this addon can ask the question without naming a backend
## class.
func effective_tickrate() -> int:
	if _mode == Mode.OFFLINE:
		return 0
	return _orbit.effective_tickrate()

# --- mode ----------------------------------------------------------------------------------------
## The current network mode. OFFLINE at boot; asserted by the acceptance test (Net.current_mode == OFFLINE).
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
## already be assigned to the SceneTree multiplayer before calling this (the session layer guarantees it).
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
## clock every peer agrees on, so deterministic scripted motion (moving platform) computes the same pose
## everywhere with no per-frame replication. Continuous + monotonic, but it can be re-stepped on a large
## local/server drift (a hard clock resync), so a consumer must tolerate the occasional small jump.
func current_time() -> float:
	if _mode == Mode.OFFLINE:
		return 0.0
	return _orbit.current_time()

## The backend ROLLBACK/resim tick -- the tick a rewindable's advance()/_rollback_tick is currently replaying,
## which during a resim is an OLDER tick than the frontier. Game code keys per-tick rollback history off this
## (e.g. held-cat memo) so a resim restores the value recorded for that exact tick. Only meaningful
## inside the rollback loop; 0 OFFLINE (no loop). Named here only (the facade boundary).
func rollback_tick() -> int:
	if _mode == Mode.OFFLINE:
		return 0
	return _orbit.rollback_tick()

## Diagnostic (net-probe): the live backend tickrate + sync_to_physics, so instrumentation can confirm the
## net tick runs at the physics rate (SPIKE B Option A) rather than the decoupled default.
func debug_timing() -> String:
	if _mode == Mode.OFFLINE:
		return "offline"
	return "tickrate=%d sync_to_physics=%s physics_tps=%d" % [
		_orbit.effective_tickrate(), str(_orbit.sync_to_physics), Engine.physics_ticks_per_second]

## The CONFIGURED network tickrate (Hz) -- the value the tick loop runs at when NOT synced to physics, and the
## value the join handshake advertises. The `net.tickrate` console cvar reads/writes this, so it
## round-trips. NOTE: with sync_to_physics the EFFECTIVE rate is the physics rate regardless of this value --
## debug_timing() reports the effective rate. Reading the configured value keeps the cvar a faithful round-trip.
func tickrate() -> int:
	return _orbit.tickrate

## Set the configured network tickrate (Hz), clamped to a sane range. The backend clamps identically, so the
## cvar round-trips. Under sync_to_physics this does not change the live tick (see tickrate()).
func set_tickrate(rate: int) -> void:
	_orbit.tickrate = clampi(rate, 1, 240)

# --- resim-depth knobs + perf diagnostics ---------------------------------------------------
# input_delay shrinks the unconfirmed window directly (input is stamped into the future); display_offset
# trades a little view delay for fewer visible corrections (it does NOT change resim cost). Both are plain
# backend properties now -- the old private-var + ProjectSettings-mirror reach-in is gone by construction.

## Ticks of intentional input delay (the resim-depth knob). Read fresh by the backend at every input
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

# remote-resim lever: whether a client's LOCAL rollback loop carries REMOTE bodies. Default FALSE
# (exempt): remote bodies are display-only on non-owning clients -- they apply the latest authoritative
# server state each tick and render engine-interpolated at that DELAYED tick. TRUE un-exempts them: the
# client then predicts remote bodies FORWARD from their latest authoritative state with held input
# (dead-reckoning through the real sim). This needs no O(N^2) input broadcast: prediction extrapolates the
# last known state instead of replaying peer input.
# An un-exempted body RECONCILES rather than merely coasting: its authoritative rows take the PREDICTING
# integration path, so a mispredict rewinds and replays it exactly as an owned body's would. Without that it
# would predict forward from its own drift and never re-base on anything the server said -- which an INPUTLESS
# shared body (a puck, a ball, a physics prop) exposes within seconds, and a remote player body hides, because
# its owner's own corrections keep the pose roughly plausible. See docs/protocol.md.
# Per-client and live (the console cvar net.remote_resim flips it mid-session); peers need not agree.
var _remote_resim: bool = false

## Whether remote bodies ride this client's rollback loop. Meaningless on a server/host (it simulates every
## body, so nothing is ever exempt).
func remote_resim() -> bool:
	return _remote_resim

func set_remote_resim(on: bool) -> void:
	_remote_resim = on
	_orbit.set_remote_resim(on)

# TEST HOOK: force the rollback LOOP at least this deep every tick. This deepens the loop's per-tick
# restore/record bookkeeping for every simulated body -- the resim-cost measurement lever the perf probe
# drives. Capped well under history_limit (128) so the backend never replays past evicted history. 0 = off.
func resim_force() -> int:
	return _orbit.resim_force

func set_resim_force(ticks: int) -> void:
	_orbit.resim_force = clampi(ticks, 0, 64)

## Interest-management radius in metres, the 100-player lever: with a radius set, the SERVER sends each peer only
## the entities within it of that peer's own body (1.25x exit hysteresis so boundary entities don't flicker).
## Server-side only; ignored on clients.
##
## **0 TURNS OFF THE DISTANCE FILTER, NOT INTEREST MANAGEMENT.** Membership is the other axis and is declared
## per entity, not here: when anything calls `set_membership()`, the interest pass still runs at radius 0 and
## still refuses the worlds a peer is not in. Only a game that declares no memberships at all gets the whole
## pass skipped at 0 -- see [method NetStateHandle.set_membership].
##
## **Size it by the longest range at which a player can act on a body, never by what makes culling look
## effective.** A culled entity is not despawned -- it keeps its node on the peer and freezes at the last pose
## that arrived -- so a radius under a weapon's range leaves a scoped shooter aiming at a stale ghost they cannot
## hit. Set it to the longest range in your game and let an arena that outgrows that start culling on its own; a
## radius that culls nothing on today's maps is doing its job, not wasting itself.
func aoi_radius() -> float:
	return _orbit.aoi_radius

func set_aoi_radius(metres: float) -> void:
	_orbit.aoi_radius = maxf(0.0, metres)

## The scale the PRIORITY BANDS are derived from, in metres: edges at `scale/3` and `2*scale/3`.
##
## A separate number from [method aoi_radius] because they answer different questions and their answers differ by
## two orders of magnitude -- one decides whether an entity is sent at all, the other how often relative to
## everything else. While they are one number, a value that bands usefully culls bodies players are shooting at,
## and a value safe for the longest shot puts every entity on a small map in one band, where the distance weight
## is a constant that cancels out of the ordering and the scorer is inert. This one can only reorder what is
## already being sent; it can never remove anything.
func aoi_band_radius() -> float:
	return _backend_float(&"aoi_band_radius", 0.0)

func set_aoi_band_radius(metres: float) -> void:
	_orbit.set(&"aoi_band_radius", maxf(0.0, metres))

## Hard cap on one peer's interest set, 0 = uncapped. The nearest N CULLABLE entities win; a peer's own body and
## every always-relevant channel are exempt, so this bounds the scenery, never the gameplay. An entity evicted by
## the cap is a real LEAVE -- it must re-enter through the full radius like any newcomer.
func aoi_max_entities() -> int:
	return _backend_int(&"aoi_max_entities", 0)

func set_aoi_max_entities(count: int) -> void:
	_orbit.set(&"aoi_max_entities", maxi(0, count))

## Declare where one peer OBSERVES from, and which world it observes in. SERVER-SIDE ONLY; no-op OFFLINE or
## against a backend that predates the call.
##
## Undeclared, each SEAT on a peer is centred on -- and put in the world of -- the lowest-id rollback body that
## seat drives. That answers what a seat CONTROLS, and interest management asks what it OBSERVES. The two agree
## in a game with one world and one avatar per player, and disagree in every other one: a spectator drives
## nothing, a commander watches ground its body is not standing on, and a peer with a body in each of two worlds
## observes exactly one.
##
## `membership` is the same id [method NetRollbackHandle.set_membership] declares on an entity, and 0 is every
## world. Declaring it here makes a peer's world a FACT RATHER THAN A PICK: the inferred path reads it off
## whichever of the seat's bodies sorts lowest by hash, so a seat driving two bodies in different worlds has no
## defined world without this call.
##
## A DECLARATION REPLACES INFERENCE OUTRIGHT, on both axes at once -- the driven body is consulted for neither
## until [method clear_peer_anchor]. May be called before the peer finishes its handshake.
##
## IT ALSO COLLAPSES A SPLIT-SCREEN CONNECTION TO ONE VIEWPOINT. This declares where a CONNECTION observes from,
## and a connection with several seats has stated one answer for all of them. That is the same precedence that
## stops a declared centre from falling back to an avatar's. A game that wants a centre per seat declares
## nothing here and lets each seat's own body anchor it -- see [method NetRollbackHandle.set_seat].
func set_peer_anchor(peer: int, position: Vector3, membership: int = 0) -> void:
	if _mode == Mode.OFFLINE or not _backend_has(&"set_peer_anchor"):
		return
	_orbit.set_peer_anchor(peer, position, membership)

## Declare that one peer observes from an ENTITY, and which world it observes in. SERVER-SIDE ONLY; no-op OFFLINE
## or against a backend that predates the call.
##
## `entity_id` comes from `entity_id()` on the rollback or state handle for the body being watched -- an opaque
## token, routinely negative, only ever passed back unmodified. The entity need NOT be one the peer drives, which
## is the point. `0` retracts, exactly as [method clear_peer_anchor] does.
##
## The same statement as [method set_peer_anchor], differing in what it costs the caller: a tracked centre follows
## the entity with no per-tick call. **When the tracked entity despawns the peer keeps the last position it
## resolved to, and stays in the world it was declared into** -- a membership is a declaration and did not fail,
## while a centre is a measurement and did. A declaration made before the entity has any replicated state simply
## starts resolving on the tick it does.
func set_peer_anchor_entity(peer: int, entity_id: int, membership: int = 0) -> void:
	if _mode == Mode.OFFLINE or not _backend_has(&"set_peer_anchor_entity"):
		return
	_orbit.set_peer_anchor_entity(peer, entity_id, membership)

## Retract a peer's anchor declaration AND its world, together. The peer returns to the inferred pair, one per
## seat: each centred on the lowest-id body that seat drives, in that body's world. Retracting one axis without
## the other would leave a peer declared into a world it has no declared position in, or positioned in a world it
## is no longer in.
func clear_peer_anchor(peer: int) -> void:
	if _mode == Mode.OFFLINE or not _backend_has(&"clear_peer_anchor"):
		return
	_orbit.clear_peer_anchor(peer)

## The world DECLARED for one peer; 0 when nothing was declared for it, and 0 OFFLINE.
##
## NOT the world an undeclared peer is filtered in -- that one is read off the body it drives and is reported by
## [method NetRollbackHandle.membership], which is where a misconfigured membership shows. 0 here means "no
## declaration", which has the same consequence as declaring every world, so the two are not distinguished.
func peer_membership(peer: int) -> int:
	if _mode == Mode.OFFLINE or not _backend_has(&"peer_membership"):
		return 0
	return _orbit.peer_membership(peer)

## Withhold ONE entity from ONE peer, or stop withholding it. SERVER-SIDE ONLY; no-op OFFLINE or against a
## backend that predates the call.
##
## The third interest axis, and the only per-(peer, entity) one. Distance and membership are both properties of
## the ENTITY -- one position, one world, read the same by every peer -- so neither can say "not this peer". This
## can, including the exception a membership id cannot express: a class of entities scoped by a declared key,
## minus one. Use a membership for a whole world; use this for the one entity inside it that a given peer must
## never receive.
##
## `entity_id` comes from `entity_id()` on the rollback or state handle -- an opaque token, routinely negative,
## only ever passed back unmodified. `0` is ignored: it is what an unresolved handle reports.
##
## THE VETO BEATS EVERY OTHER ANSWER THE FILTER WOULD GIVE, including an always-relevant channel with no anchor,
## and it refuses at the candidate rather than at the cap -- a withheld entity occupies no slot in
## [method set_aoi_max_entities]. Starting one drops the entity from that peer's interest in this call and clears
## its delta bookkeeping, so a later retraction sends a full block rather than a delta against a base the peer
## dropped.
##
## THE CLIENT-SIDE CONTRACT, STATED PLAINLY: A VETO STOPS THE ROWS AND NOTHING ELSE. No despawn is sent, the
## client's node is not removed, and the entity id stays session-global. What the client sees is
## `get_last_known_state()` ceasing to advance -- exactly what a distance cull looks like -- and what to do with
## an entity that stopped updating is your game's decision, as it already is for a cull.
##
## The veto is keyed on the entity id, so it SURVIVES THAT ENTITY'S DESPAWN. Ids are node-path-derived and a body
## that respawns under its old name reclaims its old id; dropping the veto with the body would hand the peer that
## entity on the tick it came back, which is the one moment nothing can re-declare in time.
##
## May be called before the peer finishes its handshake; the veto is held until it does. It is dropped when the
## peer disconnects, along with the rest of that peer's state.
func set_entity_hidden(peer: int, entity_id: int, hidden: bool) -> void:
	if _mode == Mode.OFFLINE or not _backend_has(&"set_entity_hidden"):
		return
	_orbit.set_entity_hidden(peer, entity_id, hidden)

## Whether `entity_id` is currently withheld from `peer`. False OFFLINE, for an unknown peer, and against a
## backend that predates the call.
func is_entity_hidden(peer: int, entity_id: int) -> bool:
	if _mode == Mode.OFFLINE or not _backend_has(&"is_entity_hidden"):
		return false
	return _orbit.is_entity_hidden(peer, entity_id)

## Rate tiering by distance band: send the MID band every other tick and the FAR band every fourth, phase-offset
## by entity id so a band's traffic is level rather than spiking once per interval.
##
## DEFAULTS OFF, deliberately. The priority scorer already produces a weight-proportional send rate per band
## without a fixed schedule (a far body settles at ~16x the near band's inter-send gap, because that is the ratio
## of their weights), so this is a HARD cap for when even that is too expensive. It is also the item most likely
## to make remote bodies visibly stutter -- a feel change dressed as a bandwidth change -- which is why it should
## not be turned on before [method bandwidth_metrics]'s `interarrival_far` proves the far band is genuinely far.
func rate_tiering() -> bool:
	return _backend_bool(&"rate_tiering", false)

func set_rate_tiering(on: bool) -> void:
	_orbit.set(&"rate_tiering", on)

## The per-peer snapshot byte budget per tick. Entities the priority rota cannot fit under this are DEFERRED to a
## later tick, never dropped. Clamped 256..1200: the ceiling is the codec's MAX_FRAME_PAYLOAD, and the floor is a
## budget that can still carry a full block -- below it every entity defers forever.
func send_budget() -> int:
	return _orbit.send_budget

func set_send_budget(bytes: int) -> void:
	_orbit.send_budget = clampi(bytes, 256, 1200)

## Diagnostic (net.perf): last-loop rollback counters from the backend. resim_ticks is the effective
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

## The raw counters behind perf_summary, typed for harnesses (the perf probe samples these each
## frame). Zeros OFFLINE.
func perf_metrics() -> Dictionary[String, float]:
	if _mode == Mode.OFFLINE:
		return {"resim_ticks": 0.0, "rollback_ms": 0.0, "net_ms": 0.0,
			"restore_ms": 0.0, "sim_ms": 0.0, "record_ms": 0.0}
	var m: Dictionary = _orbit.metrics()
	var resim: float = m.get("resim_ticks", 0.0)
	var rb_ms: float = m.get("rollback_ms", 0.0)
	var net_ms: float = m.get("net_ms", 0.0)
	var rb_nodes: float = m.get("rb_nodes", 0.0)
	# The three phases rollback_ms wrapped in one number. RESTORE writes a tick's recorded state
	# and input back onto every replaying entity, SIM is the game code, RECORD captures the result -- and the
	# capture-cost claim the docs lead with is about restore + record. Until these existed nobody could say what
	# share of a rollback loop they were, so the headline performance gap was an assertion rather than a
	# measurement. They do not sum to rollback_ms: the remainder is range setup and the display-offset restore,
	# left visible rather than attributed. Zeros on a binary older than this script, like every other addition.
	var restore_ms: float = m.get("restore_ms", 0.0)
	var sim_ms: float = m.get("sim_ms", 0.0)
	var record_ms: float = m.get("record_ms", 0.0)
	return {
		"resim_ticks": resim,
		"rollback_ms": rb_ms,
		"net_ms": net_ms,
		"rb_nodes": rb_nodes,
		"restore_ms": restore_ms,
		"sim_ms": sim_ms,
		"record_ms": record_ms,
	}

## Diagnostic: the SEND PATH's bandwidth and fairness accounting, windowed to per-second figures once a
## second by the backend. Zeros OFFLINE. Deliberately a SEPARATE dictionary from perf_metrics(), whose exact
## shape bench_metrics.gd and the perf probe read -- widening a dictionary two harnesses index into is how a
## measurement change becomes a gate failure.
##
##   tx_bytes_s / rx_bytes_s        -- OrbitNet PAYLOAD, in and out. Not what the link carries.
##   tx_datagrams_s / rx_datagrams_s -- datagram counts, published so the wire figure can be CHECKED not trusted
##   tx_wire_bytes_s                -- payload + 41 B/datagram (28 IPv4+UDP, 12 ENet, 1 Godot RAW tag). On a full
##                                     1200 B frame that is 3%; on a 90 B one it is over 40%.
##   tx_peak_peer_bytes_s           -- the busiest single peer's payload: the figure an AOI A/B has to move
##   blocks_admitted_s              -- entity blocks that made it into a frame
##   blocks_deferred_s              -- blocks that wanted to go out and did not fit. BUDGET PRESSURE.
##   blocks_culled_s                -- blocks intentionally withheld (out of interest, or rate-tiered). DELIBERATE.
##                                     Kept apart from deferred because conflating them hides the failure.
##   want_full_nacks_s              -- WANT_FULL NACKs received. SERVER-SIDE ONLY: it is counted where a
##                                     client's INPUT frame is decoded, so a client reads a structural 0.00.
##   blocks_full_s                  -- blocks sent as full rows rather than masked deltas: the send lane's
##                                     composition, and what the keyframe interval costs. Floor is about
##                                     blocks_admitted_s / 16, since every entity owes one keyframe per
##                                     interval. Near blocks_admitted_s means almost nothing is being deltaed,
##                                     which on a server indicates a want_full storm -- read it beside
##                                     want_full_nacks_s.
##   blocks_oversize_s              -- blocks admitted even though one of them exceeded the WHOLE byte budget, so
##                                     that frame went out over the MTU and fragmented. Non-zero means one
##                                     entity's full state does not fit in a datagram, which is a schema fact.
##                                     Not a deferral: deferring the FIRST block of a frame sends no frame at
##                                     all, and a never-sent entity sorts first again next tick, so it would
##                                     end that peer's snapshot stream for the session.
##   stale_blocks_s                 -- state blocks discarded because a NEWER row for that entity had already
##                                     landed: reordering and duplication, which is what a real link does.
##                                     CLIENT-SIDE ONLY, for the mirror reason -- it is counted where a received
##                                     SNAPSHOT is integrated, which a server never does.
##
##   THE NACK/STALE PAIR SPANS TWO PROCESSES. They are the diagnosis together and either alone is not, and they
##   live on opposite peers -- so pairing them means pairing a CLIENT's stale_blocks_s with a SERVER's
##   want_full_nacks_s. Inside one net.perf they can never both be non-zero, and "want_full 0.00" read off a
##   client is not evidence about a storm; it is evidence that the reader was on a client.
##   starve_ticks_max               -- worst age in ticks of an in-interest entity that HAS been sent at least once
##   unsent_backlog_max             -- worst count of in-interest entities never yet sent to a peer (the re-entry
##                                     storm gauge, which starve_ticks_max cannot see: a never-sent entity has no age)
##   interest_ms                    -- ms/tick in the interest pass. The number that would justify revisiting the
##                                     grid-vs-scan decision recorded in orbitnet-core's interest module.
##   interarrival_near/mid/far      -- mean ticks between admissions per distance band. The evidence S6 demanded
##                                     before rate tiering may be enabled.
##   peers / interest_entities      -- peers synced, and the mean size of ONE peer's interest set
## The POOLED mean ticks between admissions across every band -- the one figure from
## [method bandwidth_metrics] that is read EVERY NET TICK on the authority rather than at human rates.
##
## It is the interpolation term in every shot's rewind depth, refreshed once per tick by the server's own
## per-tick hook instead of once per pellet. Reading it through the dictionary allocated a nineteen-key
## `Dictionary` in the backend, boxed every value, and rebuilt a typed copy here, per tick, forever -- on the
## very send path this accounting exists to make cheaper. Everything else in that dictionary stays where it
## is.
##
## FAILS OPEN at 0.0, which [NetLagComp.refresh_observed_interp] reads as "no measurement yet" and answers with
## the floor. That matters for the same reason [method NetStateHandle.last_known_state]'s guard does: the
## committed cdylib is a bot's and can be a commit behind these sources, so a peer can be running GDScript that
## knows this method against a binary that does not.
func interarrival_all_ticks() -> float:
	if _mode == Mode.OFFLINE or not _backend_has(&"interarrival_all"):
		return 0.0
	return _orbit.interarrival_all()

## Whether the LOADED cdylib carries the scalar above, as a fact separate from what it answers.
##
## The scalar fails open at 0.0, and so does every key `bandwidth_metrics()` cannot find, so on a binary that
## predates this accessor BOTH read 0.0 and a probe comparing them agrees with itself. That is the exact case
## the comparison was written to catch (`net-damage-probe`), and it could not: the rewind silently reverts to
## the constant 1.0 the measured term replaced, with nothing red anywhere. Asking whether the method EXISTS is
## the question that separates a stale binary from an unpublished window.
func has_interarrival_scalar() -> bool:
	return _mode != Mode.OFFLINE and _backend_has(&"interarrival_all")

# WHETHER THE LOADED CDYLIB CARRIES A METHOD, ANSWERED ONCE. Several accessors on this facade have to tolerate a
# binary older than these sources (the committed one is a bot's and lands in its own commit), and they did it with
# `has_method` -- a ClassDB lookup with a StringName argument, on paths this epic identifies as per-tick and
# per-shot. The answer cannot change while a process holds one `_orbit`, so it is resolved on the first ask and
# kept. Keyed by name so a new tolerant accessor costs one dictionary hit rather than a new member.
var _backend_methods: Dictionary[StringName, bool] = {}

func _backend_has(method: StringName) -> bool:
	var known: bool = _backend_methods.get(method, false)
	if known:
		return true
	if _backend_methods.has(method):
		return false
	var present: bool = _orbit != null and _orbit.has_method(method)
	_backend_methods[method] = present
	return present

func bandwidth_metrics() -> Dictionary[String, float]:
	var out: Dictionary[String, float] = {
		"tx_bytes_s": 0.0, "tx_datagrams_s": 0.0, "tx_wire_bytes_s": 0.0, "tx_peak_peer_bytes_s": 0.0,
		"rx_bytes_s": 0.0, "rx_datagrams_s": 0.0,
		"blocks_admitted_s": 0.0, "blocks_deferred_s": 0.0, "blocks_culled_s": 0.0,
		"want_full_nacks_s": 0.0, "stale_blocks_s": 0.0, "blocks_oversize_s": 0.0,
		"blocks_full_s": 0.0,
		"starve_ticks_max": 0.0, "unsent_backlog_max": 0.0,
		"interest_ms": 0.0,
		"interarrival_near": 0.0, "interarrival_mid": 0.0, "interarrival_far": 0.0,
		"interarrival_all": 0.0,
		"peers": 0.0, "interest_entities": 0.0,
	}
	if _mode == Mode.OFFLINE or not _backend_has(&"bandwidth_metrics"):
		return out
	var m: Dictionary = _orbit.bandwidth_metrics()
	for key: String in out.keys():
		out[key] = m.get(key, 0.0)
	return out

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

## What THIS SERVER measured about `peer`'s round trip, in milliseconds, or a NEGATIVE value when there is no
## estimate: an unknown peer, a peer that has not acknowledged a snapshot frame since it joined, a client (which
## measures nobody), or offline. The input to the per-shooter lag-compensation rewind ([NetLagComp]).
##
## NOT the same figure as `clock_metrics()["rtt_ms"]`, which is the LOCAL peer's own ping sampler and reads 0.0 on
## a server -- the pong path only ever runs client-side. Ask this one about somebody else, that one about yourself.
##
## The backend derives it from the snapshot acknowledgements it already receives, so nothing was added to the wire.
## A caller must handle the negative: "we do not know yet" is a real answer for the first moments of every join,
## and treating it as zero would hand a fresh joiner the shallowest possible rewind at exactly the moment their
## link is least settled. It is also what a backend binary older than this script answers -- see the `has_method`
## probe, and `_backend_int` below for why a valid checkout can be in that state -- and it degrades to the flat
## flat fallback window rather than erroring, because a mispaired binary must not stop the game resolving hits.
## A LISTEN HOST asking about ITSELF is answered 0.0 rather than "no estimate", and that case is real rather than
## defensive: the backend's peer table holds REMOTE peers only, so a host's own shots would otherwise fall back to
## the flat window and be rewound further than a LAN client's in the same session -- the exact inversion this
## exists to remove. The host's round trip to itself is zero by construction; nothing is measured or believed.
func peer_rtt_ms(peer: int) -> float:
	if _mode == Mode.OFFLINE or not _backend_has(&"peer_rtt_ms"):
		return -1.0
	if is_server() and peer == multiplayer.get_unique_id():
		return 0.0
	return _orbit.peer_rtt_ms(peer)

# --- rollback / state / interpolation handles (created here so the backend is named only here) ----
## Create a rollback handle for a predicted body (owner prediction + reconciliation). OFFLINE returns an
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

## Create a state-replication handle for non-predicted, server-driven state (remote avatars / events;
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

## Register a player body for owner prediction + reconciliation + remote interpolation. Creates an
## OrbitRollbackSynchronizer on `root`, sets prediction per role, registers the serialized STATE props on
## `root`, the INPUT props on `input_node`, and the COSMETIC props (replicated but never restored / never a
## misprediction -- the prop-role diet) on `root`, then processes settings -- all here so no game code
## names the backend. OFFLINE returns an inert handle, so the body wires this unconditionally and the offline
## demo runs untouched.
##
## SERVER-AUTHORITATIVE split: `root`'s multiplayer authority is the SERVER, so the server owns every
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
	# a non-predicting synchronizer exists only on a peer that owns neither this body's state nor its
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
