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

## SERVER-SIDE: a peer finished the OrbitNet handshake. `resumed_from` is the peer id it held before it
## dropped, or 0 for a first-time joiner. `session_id` is the identity its handshake carried (0 = none).
##
## ADMIT PLAYERS HERE, not on the transport's `peer_connected`. That signal fires when the socket comes up,
## which is before the handshake -- so no identity is known yet and a rejoiner cannot be matched to the place
## it left. Never fires OFFLINE or on a client.
##
## Fires ONCE per peer, however many times its handshake is retried.
##
## `resumed_from` NAMES A CONNECTION THAT MAY STILL BE UP, and that matters because acting on it hands the new
## claimant that peer's body. It is whichever connection last claimed this identity, whether or not the server
## saw it drop: a relaunched client routinely beats its own keepalive timeout, so the connection it replaces is
## still in the peer list. The original keeps its socket, gets no error, and stops driving its own entity.
##
## IT IS REPORTED ONLY FOR A CLAIM THE SERVER GRANTED, and a claim is granted only when the joiner quoted the
## [method resume_token] this server issued for that identity. A peer that merely observed somebody's session
## id cannot produce one, so it arrives here with `resumed_from` 0 and `session_id` 0 -- it takes nothing.
##
## `session_id` IS THE IDENTITY THE CONNECTION WAS SEATED UNDER, not the one it presented. A refused claim on
## an identity somebody else still holds is seated anonymously as 0, so this is always safe to use as a roster
## key. [method set_resume_policy] set to ONLY_IF_DROPPED refuses every claim against a still-connected
## incumbent, which is the conservative rule in one call.
signal peer_joined(peer: int, session_id: int, resumed_from: int)

## SERVER-SIDE: a peer's transport connection is gone. `held` is whether its session is being kept open for
## [method reconnect_grace] seconds -- false means it is already forgotten and its place should be released now.
##
## `held` is false for a peer that claimed no identity, with the grace window at 0, and -- the case worth
## knowing about -- for a GHOST connection whose identity a returning player already took back. A transport
## does not notice a killed client until its keepalive times out, which on ENet's defaults is the better part
## of a minute, so a player who relaunches quickly is admitted first and the dead connection's drop arrives
## afterward. That drop reports `session_id` 0, and whatever it held has already moved to the new peer id.
##
## A ghost whose claim was REFUSED keeps its identity, so its drop reports that identity with `held = true`
## and opens the real window the refusal was waiting for. That is what [method set_resume_policy] set to
## ONLY_IF_DROPPED trades a fast reconnect for.
signal peer_dropped(peer: int, session_id: int, held: bool)

## SERVER-SIDE: a held session's grace window closed with nobody claiming it. `peer` is the id it was last
## connected under, for logging.
##
## THIS IS THE RELEASE POINT AND BY DEFAULT THE ADDON DOES NOT ACT ON IT. The entity is still in the scene,
## still replicating, and still pointed at a peer id that no longer exists. Free the body, or hand its input
## back to the server with [method NetRollbackHandle.set_input_authority] and open the place to the next joiner
## -- the same shape of decision as an entity a cull stopped sending.
##
## YOU CAN NOW SAY OTHERWISE IN ONE CALL. [method set_seat_release_policy] set to RELEASE_ON_EXPIRY hands every
## body this connection drove back to the server BEFORE this signal fires, so a handler that seats a replacement
## is not undone a frame later. It closes the seat and nothing more -- the body is still in the scene, and
## freeing it is still your decision. The default is unchanged.
signal peer_session_expired(session_id: int, peer: int)

## A SEAT ARRIVED on a connection. Fires on BOTH SIDES -- the server when it seats the body, every client
## one entity manifest later. Inert OFFLINE (the tick loop is not running) and against a backend that
## predates the signal.
##
## A seat is one owned, predicted viewpoint: `(peer, seat)`. It exists because some replicated body says its
## input is driven by that connection under that label, so this follows
## [method NetRollbackHandle.assign_seat] -- or any other write of the same two facts -- on the next tick
## boundary.
##
## BIND PRESENTATION HERE. A split-screen viewport to open, a camera to attach, a HUD panel to add: those
## are per-VIEWPOINT and a connection may hold several. [method seat_entities] answers which bodies the seat
## drives, which is what a camera needs.
##
## A JOINING CONNECTION'S FIRST SEAT IS ANNOUNCED HERE TOO -- it is the same event, so a game that seats every
## player through one handler needs no second one for the first player on a connection. [signal peer_joined]
## says a connection finished the handshake, which is before it drives anything.
##
## A DEDICATED SERVER HOLDS NO SEAT OF ITS OWN. Handing a body's input back to peer 1 is how a game says it is
## unclaimed, so a server with no local player announces nothing for it. A LISTEN server does hold seats -- peer
## 1 is the host player there -- which also means a body the host holds unclaimed reads the same as one the host
## player drives. Seat the host player on a non-zero label if you have to tell them apart.
signal seat_opened(peer: int, seat: int)

## A SEAT LEFT a connection that stays in session. Fires on both sides, like [signal seat_opened].
##
## It fires when nothing drives `(peer, seat)` any more: the body was released
## ([method NetRollbackHandle.release_seat]), re-pointed at another connection, or unregistered. The
## connection is unaffected and may still hold other seats.
##
## BY DEFAULT A DROPPED CONNECTION DOES NOT CLOSE ITS SEATS BY ITSELF. Its bodies keep the authority they were
## given until the game changes them -- the same rule [signal peer_session_expired] states, for the same reason:
## whether to free the body, hand it back, or hold it for a reconnect is your decision. Release the seat and
## this fires.
##
## YOU CAN NOW SAY OTHERWISE IN ONE CALL. [method set_seat_release_policy] set to RELEASE_ON_DROP or
## RELEASE_ON_EXPIRY hands the connection's bodies back to the server at the drop or at the end of the grace
## window, and this fires from the announcement that follows. [method release_peer_seats] does the same for one
## connection on demand, under every policy. The default is unchanged.
signal seat_closed(peer: int, seat: int)

## AN ENTITY BECAME RELEVANT to one connection. Fires on BOTH SIDES -- the server from its own interest pass,
## every client from the trailing section on the snapshot it is already receiving. Inert OFFLINE and against a
## backend that predates the signal.
##
## `peer` is the connection that gained it: a remote connection on a server, this client's own id on a client.
## `entity_id` is the opaque token `entity_id()` answers on either handle.
##
## TELEPORT ON RE-ENTRY. A body that moved while it was away is interpolating from the pose it had when the rows
## stopped, so it would fly across the world over one tick. [method NetInterpolatorHandle.teleport] is what
## suppresses that.
##
## NOT A PER-HANDLE SIGNAL, and its twin is why: a leave routinely names an entity this client has no node for,
## which is exactly the case that matters, so there is no handle to hang it on.
signal entity_entered_interest(peer: int, entity_id: int)

## AN ENTITY STOPPED BEING RELEVANT to one connection: culled by distance, refused by a membership, evicted by
## [method set_aoi_max_entities], withheld by [method set_entity_hidden], or unregistered outright. Fires on both
## sides, like [signal entity_entered_interest].
##
## ONE SIGNAL COVERS BOTH CAUSES. "The server stopped sending you this" and "this entity unregistered" are the
## same fact to a game holding a node it can no longer update, so a client emits this from an entity manifest
## rebuild as well as from a cull. An entity culled and unregistered on the same tick fires it EXACTLY ONCE.
##
## THIS IS THE RELEASE POINT AND BY DEFAULT THE ADDON DOES NOT ACT ON IT -- the same contract
## [signal peer_session_expired] states. The node is still in the scene, still holding the last pose it received,
## and nothing frees, hides, reparents or teleports it. What to do about that is your decision.
##
## HIDE, DO NOT FREE. A cap eviction oscillates at the boundary -- a body at the edge of
## [method set_aoi_max_entities] leaves and re-enters as the population around it moves -- and freeing on the
## leave turns that into spawn churn. Hiding costs nothing to undo.
##
## [method entities_in_interest] is what a handler bound mid-session, or a node built after the fact, resyncs
## from: a signal is a transition, and an edge needs a starting point.
signal entity_left_interest(peer: int, entity_id: int)

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
	# The session-lifecycle signals reach the facade the same way, but by NAME and behind a has_signal probe:
	# they are newer than the committed binary, and `_orbit.peer_joined.connect(...)` against a binary that
	# lacks them is a script error at autoload time -- on every project load. Same tolerance rule as the
	# `_backend_*` accessors below, applied to signals.
	if _orbit.has_signal(&"peer_joined"):
		_orbit.connect(&"peer_joined", _on_backend_peer_joined)
		_orbit.connect(&"peer_dropped", _on_backend_peer_dropped)
		_orbit.connect(&"peer_session_expired", _on_backend_peer_session_expired)
	# Probed separately from the block above: the seat signals are newer than it, so a binary that carries the
	# session-lifecycle three need not carry these two.
	if _orbit.has_signal(&"seat_opened"):
		_orbit.connect(&"seat_opened", _on_backend_seat_opened)
		_orbit.connect(&"seat_closed", _on_backend_seat_closed)
	# Probed separately again, for the same reason: the relevancy signals are newer than the seat pair.
	if _orbit.has_signal(&"entity_entered_interest"):
		_orbit.connect(&"entity_entered_interest", _on_backend_entity_entered_interest)
		_orbit.connect(&"entity_left_interest", _on_backend_entity_left_interest)
	# One identity per process, minted before anything can join. A consumer that wants a session to survive a
	# RESTART overwrites it with a stored value -- see set_session_id().
	set_session_id(_mint_session_id())

func _on_backend_before_tick(_delta: float, tick: int) -> void:
	pre_tick.emit(tick)

func _on_backend_after_rollback_loop() -> void:
	if _mode != Mode.OFFLINE:
		post_tick.emit()

func _on_backend_peer_joined(peer: int, session_id: int, resumed_from: int) -> void:
	peer_joined.emit(peer, session_id, resumed_from)

func _on_backend_peer_dropped(peer: int, session_id: int, held: bool) -> void:
	peer_dropped.emit(peer, session_id, held)

func _on_backend_peer_session_expired(session_id: int, peer: int) -> void:
	peer_session_expired.emit(session_id, peer)

func _on_backend_seat_opened(peer: int, seat: int) -> void:
	seat_opened.emit(peer, seat)

func _on_backend_seat_closed(peer: int, seat: int) -> void:
	seat_closed.emit(peer, seat)

func _on_backend_entity_entered_interest(peer: int, entity_id: int) -> void:
	entity_entered_interest.emit(peer, entity_id)

func _on_backend_entity_left_interest(peer: int, entity_id: int) -> void:
	entity_left_interest.emit(peer, entity_id)

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

## Where a Windows FAIL-FAST crash would leave a dump, read back from Windows Error Reporting (WER).
##
## The handler [method install_native_crash_handler] installs catches everything an in-process handler can. On
## Windows that excludes `__fastfail` -- what the CRT raises on detected heap corruption, the counterpart of the
## SIGABRT case covered on Linux and macOS -- because a fail-fast bypasses every frame-based and vector-based
## handler by design. WER's out-of-process `LocalDumps` collector is the only thing that sees it.
##
## THE ADDON NEVER WRITES THOSE REGISTRY KEYS. All four are documented HKLM-only (there is no HKEY_CURRENT_USER
## fallback), writing them needs administrator privileges, and they set crash-collection policy for EVERY
## application on the machine. So this reads what the machine is already configured to do, and a game's crash
## report can name the folder a dump would land in -- `<folder>/<image>.<pid>.dmp` -- or say plainly that
## nothing collects. Setting the keys is an installer's job; docs/crash-capture.md carries them.
##
##   supported   -- false off Windows, and on a backend binary too old to answer. The question does not arise.
##   configured  -- whether WER collects a dump for this process at all. FALSE MEANS A FAIL-FAST LEAVES NOTHING.
##   scope       -- which key decided it: "none", "global", or "image" for the per-executable subkey
##   folder      -- where a dump would land, environment-expanded. EMPTY whenever nothing collects
##   dump_type   -- 0 custom, 1 mini (WER's default), 2 full
##   dump_count  -- how many dumps the folder keeps before the oldest is replaced
##   image       -- this process's executable file name, which is also the dump file's stem
func native_crash_dump_config() -> Dictionary[String, Variant]:
	var nothing: Dictionary[String, Variant] = {
		"supported": false, "configured": false, "scope": "none",
		"folder": "", "dump_type": 0, "dump_count": 0, "image": "",
	}
	# Probed for the same reason install_native_crash_handler probes: the committed binary and this script are
	# refreshed on different schedules, and a diagnostics read has no business erroring on an older backend.
	if _orbit == null or not _orbit.has_method(&"crash_dump_config"):
		return nothing
	var cfg: Dictionary = _orbit.crash_dump_config()
	var supported: bool = cfg.get("supported", false)
	var configured: bool = cfg.get("configured", false)
	var scope: String = cfg.get("scope", "none")
	var folder: String = cfg.get("folder", "")
	var dump_type: int = cfg.get("dump_type", 0)
	var dump_count: int = cfg.get("dump_count", 0)
	var image: String = cfg.get("image", "")
	return {
		"supported": supported,
		"configured": configured,
		"scope": scope,
		"folder": folder,
		"dump_type": dump_type,
		"dump_count": dump_count,
		"image": image,
	}

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

## Restore the coupled behavior (net tick == physics tick). Called on session teardown so offline / a
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

# --- session identity and resume ------------------------------------------------------------------
## This peer's SESSION IDENTITY -- the token its join handshake carries, and the only thing that survives a
## reconnect.
##
## A multiplayer peer id names a CONNECTION. The transport reassigns it on every reconnect, so a server holding
## one cannot answer "is this the player who was here a moment ago", and every roster keyed on it hands a
## rejoining player whichever place happens to be free -- somebody else's army, somebody else's body. This
## answers it: one token, minted once, resent verbatim on every join.
##
## The facade mints a random one at boot, so a game that does nothing is already resumable within a process.
##
## IT IS ASSERTED BY THE CLIENT AND VERIFIED BY NOBODY. A peer can send any identity it likes, including one it
## watched somebody else use. What that no longer buys is somebody else's body: a RESUME is granted only when
## the joiner also quotes the server-minted [method resume_token] issued for that identity, and an observer who
## copied an identity off a roster, a kill feed, a log line or a screenshot never saw the token. It is still
## inadequate for anything that must not be forged. Account identity, entitlement and ban evasion need an
## authenticated layer above this, and this call is where its verified id would be written.
func session_id() -> int:
	if not _backend_has(&"session_id"):
		return 0
	return _orbit.session_id()

## Set this peer's session identity. CLIENT-SIDE, and it must be set BEFORE the join handshake goes out; a
## change afterward reaches the server on the next join.
##
## Pass a value the game stored to make a session survive a PROCESS RESTART -- a crash, an alt-F4, a
## reinstalled route. `0` claims no identity, which is always seated as a newcomer.
##
## STORE [method resume_token] BESIDE IT. The identity on its own no longer resumes anything once a server has
## issued a token for it, so a restored id with no restored token is seated as a newcomer.
func set_session_id(id: int) -> void:
	if not _backend_has(&"set_session_id"):
		return
	_orbit.set_session_id(id)

## The session identity `peer` claimed in its handshake, or 0 for an unknown peer and one that claimed none.
## SERVER-SIDE. KEY YOUR ROSTER ON THIS, not on the peer id.
func peer_session_id(peer: int) -> int:
	if _mode == Mode.OFFLINE or not _backend_has(&"peer_session_id"):
		return 0
	return _orbit.peer_session_id(peer)

## Whether a dropped session is currently being held open for `session_id` to reclaim. SERVER-SIDE; false once
## the window closes, once it is claimed, and for identity 0.
func is_session_held(session_id: int) -> bool:
	if _mode == Mode.OFFLINE or not _backend_has(&"is_session_held"):
		return false
	return _orbit.is_session_held(session_id)

## The SERVER-MINTED RESUME TOKEN this peer holds for its session identity, or 0 when it holds none.
##
## CLIENT-SIDE. [method session_id] names the player; this is what a claim on that identity has to quote. The
## server mints one per identity, sends it in the join reply, and will not hand that identity's body to a peer
## that cannot quote it back.
##
## PERSIST IT BESIDE THE STORED SESSION ID. It is the half of the pair a restarted process cannot re-derive,
## and a saved identity with no saved token is exactly what an observer who copied the identity presents -- so
## it is seated as a newcomer. Save both or resume nothing.
##
## WHAT THE TOKEN CLOSES: a peer that merely OBSERVED another player's session id -- off a roster broadcast, a
## kill feed, a log line, a screenshot -- can no longer take that player's body.
##
## WHAT IT DOES NOT CLOSE: an on-path observer, who reads the join reply and can then quote the token
## verbatim. That is the same boundary [method session_id]'s own session key already has, and it closes the
## same way -- [method set_session_secret]. Under a secret that observer can still copy the token but cannot
## authenticate the handshake quoting it, so the claim never reaches the resume decision.
##
## ONE TOKEN PER CLIENT, NAMING WHICHEVER SERVER LAST ISSUED ONE. A token is minted per server per identity, so
## joining a second server under the same identity replaces the stored value and forfeits the resume on the
## first. Storing one per server would need a server identity the protocol does not carry, and what it would
## buy is a player alternating between two servers inside one [method reconnect_grace] window.
##
## 0 against a backend that predates the call, and 0 until a server has answered a join.
func resume_token() -> int:
	if not _backend_has(&"resume_token"):
		return 0
	return _orbit.resume_token()

## Restore the resume token a previous run of this process was issued. CLIENT-SIDE, and set it BEFORE the join
## handshake goes out, beside [method set_session_id]; a change afterward reaches the server on the next join.
##
## BOTH HALVES ARE NEEDED TO RESUME. The identity alone reclaims nothing once a server holds a token for it,
## and a token alone names no player. The two are not checked against each other here -- a mismatched pair is
## refused by the server and seated as a newcomer -- so they may be restored in either order, and a game that
## stores them in one blob cannot get the ordering wrong.
##
## 0 quotes no token, which is always seated as a newcomer once a server holds a token for that identity.
func set_resume_token(token: int) -> void:
	if not _backend_has(&"set_resume_token"):
		return
	_orbit.set_resume_token(token)

## The resume token this server issued to `peer`, or 0 for an unknown peer and one holding no identity.
##
## SERVER-SIDE, and a DIAGNOSTIC: it is what an admin tool or a log line prints to see why a rejoiner was or
## was not resumed. Nothing needs it to seat a player. 0 OFFLINE and against a backend that predates the call.
func peer_resume_token(peer: int) -> int:
	if _mode == Mode.OFFLINE or not _backend_has(&"peer_resume_token"):
		return 0
	return _orbit.peer_resume_token(peer)

# --- the shared session secret -----------------------------------------------------------------------
## Set the SHARED SESSION SECRET this session's per-datagram keys are derived from. An empty array clears it.
##
## BOTH ENDS MUST SET THE SAME ONE, AND SET IT BEFORE [method set_mode]. The client folds it into the key it
## seals with, the server folds it into the key it opens with, and a session where the two disagree
## authenticates nothing.
##
## SOURCE IT FROM A CHANNEL THE GAME ALREADY AUTHENTICATED -- a lobby's metadata, a matchmaking ticket, a
## session record fetched over TLS. Any length works: it is folded to 16 bytes internally, so a token, a
## ticket or a passphrase can be passed as they are.
##
## WHAT IT CHANGES. Without a secret the per-datagram key is minted by the client and carried in the join
## handshake in the clear, so an ON-PATH OBSERVER who reads that handshake can forge anything the client can.
## With one, the handshake carries only a NONCE, both ends derive the key from `(secret, nonce)`, and that
## observer learns the nonce and nothing else.
##
## WHAT IT DOES NOT CHANGE. The tag is still 64 bits and the key still 128. The derived key is worth exactly
## the entropy of the secret you supply -- one a lobby prints on screen buys what it looks like it buys. And
## NONE OF THIS ENCRYPTS ANYTHING: every payload is still on the wire in the clear.
##
## A MISCONFIGURATION LOOKS THE SAME TO THE PLAYER EITHER WAY: the two ends derive different keys, nothing
## either sends opens at the other, and the join never completes while the handshake retries. What differs is
## whether anything says why.
##
## - SERVER WITH A SECRET, CLIENT WITHOUT is refused at the handshake, with one readable rejection in the
##   server's log naming the secret. That is what the confirm tag on the handshake exists for.
## - CLIENT WITH A SECRET, SERVER WITHOUT cannot be reported at all. The server's reply is sealed under a key
##   the client will not derive, so it never reads a byte of it -- a rejection included -- and the server sees
##   a hello it has no reason to refuse.
##
## COMPARE [method has_session_secret] ON BOTH ENDS WHEN A JOIN HANGS. It is the only thing that separates
## this from a dead link.
##
## A no-op against a backend that predates the call, which leaves that session on the cleartext key.
func set_session_secret(secret: PackedByteArray) -> void:
	if not _backend_has(&"set_session_secret"):
		return
	_orbit.set_session_secret(secret)

## Whether a session secret is set. THERE IS NO GETTER FOR THE BYTES, deliberately -- the only questions a
## game has are "did my configuration take" and "am I about to join in the clear", and both are this one.
##
## false against a backend that predates the call, which is the honest answer: that backend derives nothing.
func has_session_secret() -> bool:
	if not _backend_has(&"has_session_secret"):
		return false
	return _orbit.has_session_secret()

## Which claims on a session identity this server grants. See [method set_resume_policy]. ALWAYS is the default
## and stays the default.
enum ResumePolicy {
	ALWAYS = 0,
	ONLY_IF_DROPPED = 1,
	NEVER = 2,
}

## The resume policy in force. ALWAYS unless the game chose otherwise, and ALWAYS against a backend that
## predates the property.
func resume_policy() -> ResumePolicy:
	return _resume_policy_of(_backend_int(&"resume_policy", ResumePolicy.ALWAYS))

## Choose which claims on a session identity this server grants. SERVER-SIDE. A value outside the enum clamps
## to ALWAYS, here and in the backend.
##
## [b]ALWAYS[/b] -- a claim quoting the right resume token is granted, including against a connection that is
## still up. That connection loses the identity and stops driving its bodies; it is reported to the game as
## `resumed_from` on [signal peer_joined].
## [b]ONLY_IF_DROPPED[/b] -- the same claim is refused while the incumbent connection is still up. The
## incumbent KEEPS its identity, so its own later drop still opens a real grace window, and the claimant is
## seated as an anonymous newcomer with session id 0 and `resumed_from` 0. It has to join again once the drop
## lands.
## [b]NEVER[/b] -- no claim is ever granted. A returning player is a new player. An identity that is currently
## HELD is not seated on anybody either, so pair this with [method set_reconnect_grace] at 0 if you want a
## returning player to carry its own identity again immediately.
##
## THE DEFAULT IS ALWAYS, AND THAT IS A CHOICE RATHER THAN AN OMISSION:
##
## - THE TOKEN IS WHAT REMOVED THE REACHABLE ATTACK. A claim is granted only when the quoted
##   [method resume_token] matches the one the server has on record, so a peer that merely observed somebody's
##   session id is refused under ALWAYS exactly as it is under NEVER.
## - WHAT ALWAYS IS STILL OPEN TO IS AN ON-PATH OBSERVER, who reads the join reply and can quote the token
##   verbatim. ONLY_IF_DROPPED buys nothing against that adversary: it can read the traffic, so it can already
##   do everything the client can, and it can wait for the drop like anybody else.
## - WHAT ALWAYS BUYS IS EVERY HONEST FAST RECONNECT. A relaunched client routinely arrives before the
##   transport reports its old socket gone -- measured at anywhere from 45 s to never on ENet's defaults -- and
##   under ONLY_IF_DROPPED that player is refused their own body for the whole of that span.
##
## ONLY_IF_DROPPED is a supported setting and ONE CALL, for a game that will not accept a live takeover on any
## terms.
func set_resume_policy(policy: int) -> void:
	_orbit.set(&"resume_policy", int(_resume_policy_of(policy)))

## The enum member `policy` names, or ALWAYS for anything else. One definition, so the read-back and the write
## clamp the same way -- and so a value from a newer backend reads as "refuse nobody" rather than as whichever
## member happens to sit at that number here. ALWAYS is the safe direction because it is token-gated: falling
## onto a stricter policy by accident locks honest players out of their own bodies.
static func _resume_policy_of(policy: int) -> ResumePolicy:
	match policy:
		ResumePolicy.ONLY_IF_DROPPED: return ResumePolicy.ONLY_IF_DROPPED
		ResumePolicy.NEVER: return ResumePolicy.NEVER
	return ResumePolicy.ALWAYS

## Seconds a dropped peer's session is held open for it to come back to. 0 disables resume entirely: a peer
## that drops is forgotten in the same frame and [signal peer_dropped] reports `held = false`.
##
## SERVER-SIDE, and WALL-CLOCK -- a player alt-tabs, a router renegotiates, a phone changes network, and none
## of those are measured in simulation ticks.
##
## SIZING IT COSTS SOMETHING IN BOTH DIRECTIONS. The entity is held for the whole window: nobody else can be
## given it, it keeps replicating, and it acts on no input at all (see
## [method NetRollbackHandle.set_input_authority] for the release, and the gap policy below). Too short and a
## player who dropped on a loading screen comes back to a stranger in their body; too long and a full session
## refuses newcomers while it waits for players who left for good. The 30 s default clears the ordinary causes
## without holding a competitive place through a whole engagement.
##
## THE GAP POLICY, STATED: from the tick its owner leaves, an entity's input is written as the NEUTRAL (all
## zero) row on the server and its tick is marked authoritative. It therefore acts on no intent rather than
## repeating the departed player's last one, and its state keeps broadcasting -- so other peers see it come to
## rest where it was, instead of freezing at one tick and then jumping when its owner returns.
func reconnect_grace() -> float:
	return _backend_float(&"reconnect_grace", 0.0)

func set_reconnect_grace(seconds: float) -> void:
	_orbit.set(&"reconnect_grace", maxf(0.0, seconds))

## Mint a session identity: 63 random bits, positive so it prints legibly and round-trips through anything
## that treats an id as a plain integer.
##
## Godot seeds the global RNG randomly at startup, so this differs between processes without the game having
## to seed anything. It is not a secret and does not need to be one -- see [method session_id].
static func _mint_session_id() -> int:
	var high: int = randi() & 0x7fffffff
	var low: int = randi() & 0xffffffff
	return maxi(1, (high << 32) | low)

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

## Interest-management radius in meters, the 100-player lever: with a radius set, the SERVER sends each peer only
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

func set_aoi_radius(meters: float) -> void:
	_orbit.aoi_radius = maxf(0.0, meters)

## The scale the PRIORITY BANDS are derived from, in meters: edges at `scale/3` and `2*scale/3`.
##
## A separate number from [method aoi_radius] because they answer different questions and their answers differ by
## two orders of magnitude -- one decides whether an entity is sent at all, the other how often relative to
## everything else. While they are one number, a value that bands usefully culls bodies players are shooting at,
## and a value safe for the longest shot puts every entity on a small map in one band, where the distance weight
## is a constant that cancels out of the ordering and the scorer is inert. This one can only reorder what is
## already being sent; it can never remove anything.
func aoi_band_radius() -> float:
	return _backend_float(&"aoi_band_radius", 0.0)

func set_aoi_band_radius(meters: float) -> void:
	_orbit.set(&"aoi_band_radius", maxf(0.0, meters))

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
## Undeclared, each SEAT on a peer is centered on -- and put in the world of -- the lowest-id rollback body that
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
## stops a declared center from falling back to an avatar's. A game that wants a center per seat declares
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
## The same statement as [method set_peer_anchor], differing in what it costs the caller: a tracked center follows
## the entity with no per-tick call. **When the tracked entity despawns the peer keeps the last position it
## resolved to, and stays in the world it was declared into** -- a membership is a declaration and did not fail,
## while a center is a measurement and did. A declaration made before the entity has any replicated state simply
## starts resolving on the tick it does.
func set_peer_anchor_entity(peer: int, entity_id: int, membership: int = 0) -> void:
	if _mode == Mode.OFFLINE or not _backend_has(&"set_peer_anchor_entity"):
		return
	_orbit.set_peer_anchor_entity(peer, entity_id, membership)

## Retract a peer's anchor declaration AND its world, together. The peer returns to the inferred pair, one per
## seat: each centered on the lowest-id body that seat drives, in that body's world. Retracting one axis without
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

## Where an anchor the filter is USING came from. The `source` key of [method peer_anchor].
enum AnchorSource {
	## No answer: the peer names no connection, or the interest pass has not run for it. `stale` is true.
	NONE = 0,
	## Inferred from the bodies the connection drives -- the default, and what a game that declares nothing gets.
	INFERRED = 1,
	## A fixed position declared by [method set_peer_anchor].
	FIXED = 2,
	## An entity declared by [method set_peer_anchor_entity], tracked wherever it is.
	ENTITY = 3,
}

## The interest anchor ACTUALLY IN EFFECT for one connection, as the last interest pass resolved it. SERVER-SIDE
## diagnostic; every key present and zeroed OFFLINE, on a client, and against a backend that predates the call.
##
## NOT [method peer_membership], which reports the DECLARATION and therefore answers 0 for every peer that
## declared nothing -- indistinguishable from a peer declared into every world. The pair the filter actually ran
## with is computed inside the send path, and this is the only way to read it.
##
## [b]KEYS[/b]
## [code]source[/code] ([AnchorSource]) -- 0 none, 1 inferred, 2 fixed position, 3 tracked entity.
## [code]viewpoints[/code] (int) -- how many observers the filter ran. One per resolved seat; 1 for a declared or
##   failed-open connection; 0 for a connection closed by [method set_unanchored_policy].
## [code]membership[/code] (int) -- the world IN EFFECT, not the declared one.
## [code]located[/code] (bool) -- false when the center could not be established, so nothing is culled by distance.
## [code]center[/code] (Vector3) -- the center, or ZERO when `located` is false.
## [code]open[/code] (bool) -- this connection culls NOTHING by distance, because one of its viewpoints has no
##   center. Read beside `viewpoints`: 0 viewpoints with `open` false is the opposite state, receiving nothing.
## [code]ambiguous[/code] (bool) -- some seat drives several anchored bodies, so its center is one arbitrary
##   (deterministic) pick among them. Declare the same membership on every body a seat drives, put them on
##   separate seats, or declare the connection's anchor, if that pick is not the one you want.
## [code]stale[/code] (bool) -- THE INTEREST PASS HAS NOT RUN. Read nothing else.
##
## [b]`stale` IS THE GATE AND IT IS NOT AN EDGE CASE.[/b] The pass is skipped entirely whenever nothing can be
## culled -- no [method aoi_radius] and no entity declaring a membership, which is a session replicating
## everything to everybody -- and it never runs on a client. Without `stale` this would answer "centered at the
## origin, in world 0, located" for every peer in those sessions, describing a filter that is not running.
##
## `center`, `located` and `membership` describe the FIRST viewpoint, which is the whole connection whenever
## `viewpoints` is 1. A split-screen connection has one per seat and they differ; ask [method seat_anchor] there.
func peer_anchor(peer: int) -> Dictionary[String, Variant]:
	var out: Dictionary[String, Variant] = {
		"source": int(AnchorSource.NONE),
		"viewpoints": 0,
		"membership": 0,
		"located": false,
		"center": Vector3.ZERO,
		"open": false,
		"ambiguous": false,
		"stale": true,
	}
	if _mode == Mode.OFFLINE or not _backend_has(&"peer_anchor_info"):
		return out
	var info: Dictionary = _orbit.peer_anchor_info(peer)
	for key: String in out.keys():
		out[key] = info.get(key, out[key])
	return out

## The same answer for ONE seat on a connection, for the split-screen case. Keys: `center` (Vector3, ZERO when
## unlocated), `located` (bool), `membership` (int). Every key present and zeroed OFFLINE, on a client, and
## against a backend that predates the call.
##
## A DECLARED connection answers its one collapsed viewpoint for every seat label, including labels no body
## currently drives -- [method set_peer_anchor] states where the CONNECTION observes from and is not re-split by
## seat. An inferred connection answers only for the seats that resolved a center; a seat whose body has not
## spawned reads zeroed, which is exactly what the filter does with it.
func seat_anchor(peer: int, seat: int) -> Dictionary[String, Variant]:
	var out: Dictionary[String, Variant] = {
		"center": Vector3.ZERO,
		"located": false,
		"membership": 0,
	}
	if _mode == Mode.OFFLINE or not _backend_has(&"seat_anchor_info"):
		return out
	var info: Dictionary = _orbit.seat_anchor_info(peer, seat)
	for key: String in out.keys():
		out[key] = info.get(key, out[key])
	return out

## What a connection that resolved NO interest anchor receives. See [method set_unanchored_policy]. OPEN is the
## default and stays the default.
enum UnanchoredPolicy {
	OPEN = 0,
	CLOSED = 1,
}

## Choose what a connection that resolved no interest anchor receives. SERVER-SIDE, session-wide; a value outside
## the enum clamps to OPEN, here and in the backend. No-op against a backend that predates the call.
##
## [b]OPEN[/b] -- today's behavior. Such a connection is treated as unlocatable, which makes every entity
## uncullable for it, and an uncullable entity is kept regardless of [method aoi_max_entities]. So it receives
## every non-vetoed entity in EVERY world, with the nearest-N cap not bounding it and the per-datagram send
## budget as the only remaining brake. That is the right answer for a player whose avatar is still spawning and
## the wrong one for a connection that will never drive anything.
## [b]CLOSED[/b] -- such a connection is given no viewpoint, and no viewpoint makes nothing relevant. It
## receives nothing until it declares an anchor or drives a body.
##
## [b]THE CARVE-OUT IS THE WHOLE DESIGN.[/b] CLOSED applies ONLY to a connection that declared nothing AND drives
## no rollback body at all. A connection whose seats exist but have not RESOLVED a center yet -- a player whose
## body is still spawning -- keeps the fail-open above, and that is deliberate: closing it would deny a player
## their own avatar for as many ticks as the body takes to spawn.
##
## [b]THE DEFAULT DOES NOT MOVE.[/b] The cdylib is refreshed only at a release tag, so the same project source
## runs against older and newer binaries; a CLOSED default would mean a game's spectators see the world or do
## not, depending on which binary is on disk. Choose it in one call, in a session whose spectators are supposed
## to declare an anchor with [method set_peer_anchor].
func set_unanchored_policy(policy: int) -> void:
	if not _backend_has(&"set_unanchored_policy"):
		return
	_orbit.set_unanchored_policy(int(_unanchored_of(policy)))

## The session default in force. OPEN unless the game chose otherwise, and OPEN against a backend that predates
## the call. Per-connection overrides are not folded in -- this is what a connection nobody declared a policy for
## follows.
func unanchored_policy() -> UnanchoredPolicy:
	# A method rather than a backend property, so `_backend_int` (which reads a property) cannot answer it.
	if not _backend_has(&"unanchored_policy"):
		return UnanchoredPolicy.OPEN
	var raw: int = _orbit.unanchored_policy()
	return _unanchored_of(raw)

## The same policy for ONE connection, overriding the session default outright. SERVER-SIDE; no-op OFFLINE or
## against a backend that predates the call.
##
## For the mixed session the session-wide value cannot express: a game whose spectators declare an anchor and
## whose late joiners do not can close the first without closing the second. The carve-out on
## [method set_unanchored_policy] applies here unchanged.
##
## IT IS DROPPED WITH THE CONNECTION. A reused peer id follows the session default again rather than inheriting a
## policy nobody set for it. May be called before the peer finishes its handshake.
func set_peer_unanchored_policy(peer: int, policy: int) -> void:
	if _mode == Mode.OFFLINE or not _backend_has(&"set_peer_unanchored_policy"):
		return
	_orbit.set_peer_unanchored_policy(peer, int(_unanchored_of(policy)))

## The enum member `policy` names, or OPEN for anything else. One definition, so the read-back and the write
## clamp the same way -- and so a value from a newer backend reads as "withhold nothing" rather than as whichever
## member happens to sit at that number here.
static func _unanchored_of(policy: int) -> UnanchoredPolicy:
	if policy == UnanchoredPolicy.CLOSED:
		return UnanchoredPolicy.CLOSED
	return UnanchoredPolicy.OPEN

## What a session does with a connection's seats once that connection ends. See
## [method set_seat_release_policy]. HOLD is the default and stays the default.
enum SeatRelease {
	HOLD = 0,
	RELEASE_ON_EXPIRY = 1,
	RELEASE_ON_DROP = 2,
}

## The seat-release policy in force. HOLD unless the game chose otherwise, and HOLD against a backend that
## predates the property.
func seat_release_policy() -> SeatRelease:
	return _seat_release_of(_backend_int(&"seat_release_policy", SeatRelease.HOLD))

## Choose what happens to a connection's seats when that connection ends. SERVER-SIDE. A value outside the
## enum clamps to HOLD, here and in the backend, so a stored number this build does not know releases nothing.
##
## [b]HOLD[/b] -- nothing is released. The bodies keep the peer they were given, through the drop and past the
## expiry, until the game re-points them itself.
## [b]RELEASE_ON_EXPIRY[/b] -- the seats are released when the grace window closes with nobody having claimed
## the session back, immediately BEFORE [signal peer_session_expired] fires. A drop on its own changes nothing,
## so a player who reconnects inside the window finds the body where they left it.
## [b]RELEASE_ON_DROP[/b] -- the seats are released at the next tick boundary after the transport connection
## goes away. For a game with no reconnect story: the place opens to the next joiner at once, and a player who
## comes back is a new player.
##
## THE DEFAULT IS HOLD AND IT DOES NOT MOVE. Four reasons, each sufficient on its own:
##
## - IT IS WHAT THE INSTALLED BINARY ALREADY DOES. The cdylib is refreshed only at a release tag, so the same
##   project source runs against older and newer binaries. A releasing default would mean a game despawns
##   players' viewpoints or does not, depending on which binary happens to be on disk.
## - IT IS THE DOCUMENTED CONTRACT IN THREE PLACES: [signal peer_session_expired], [signal seat_closed] and the
##   sizing note on [method set_reconnect_grace] all state that a dropped connection keeps its seats. Moving the
##   default falsifies all three for every existing consumer, silently.
## - IT IS WHAT THE RECONNECT GRACE WINDOW EXISTS FOR. A player whose link drops a burst of packets comes back
##   to the body they left, and that only works because nothing took it away meanwhile. Releasing on every
##   transient drop despawns players for a hiccup.
## - THE ADDON DOES NOT KNOW WHAT A RELEASED BODY SHOULD BECOME. Freed, parked as a corpse, handed to a queued
##   joiner, kept as an idle NPC -- those are game rules, and this facade declines to make that decision. THIS
##   CALL DOES NOT MAKE IT EITHER: RELEASE_ON_* hands input back to the server and closes the seat, and freeing
##   the node stays your call, exactly as it is for a body a cull stopped sending.
##
## What choosing a RELEASE_ON_* buys is ONE CALL INSTEAD OF A SECOND TABLE. The alternative is a peer-to-bodies
## map maintained beside the roster the backend already derives from ownership, and two tables answering "which
## bodies does this connection drive" are two things that can disagree -- while only one of them, ownership, is
## what the anti-forgery check on received input reads.
func set_seat_release_policy(policy: int) -> void:
	_orbit.set(&"seat_release_policy", int(_seat_release_of(policy)))

## The enum member `policy` names, or HOLD for anything else. One definition, so the read-back and the write
## clamp the same way -- and so a value from a newer backend reads as "release nothing" rather than as whichever
## member happens to sit at that number here.
static func _seat_release_of(policy: int) -> SeatRelease:
	match policy:
		SeatRelease.RELEASE_ON_EXPIRY: return SeatRelease.RELEASE_ON_EXPIRY
		SeatRelease.RELEASE_ON_DROP: return SeatRelease.RELEASE_ON_DROP
	return SeatRelease.HOLD

## Hand every body `peer` drives back to the server, closing its seats. Answers how many entities changed.
##
## SERVER-SIDE, and AVAILABLE UNDER EVERY POLICY including the default -- [method set_seat_release_policy] only
## decides whether the backend makes this call by itself on a drop or an expiry. Use it for the cases no policy
## covers: a kick, an admin command, a match ending, a player who forfeits.
##
## 0 off the authority, 0 for a peer that drives nothing, 0 for peer id 1 (a body handed back to the server is
## what an unclaimed body already looks like), and 0 against a backend that predates the call.
##
## IT RELEASES THE SEAT AND NOTHING ELSE. The body stays registered, stays replicated and stays in the scene;
## what leaves is the viewpoint. [signal seat_closed] fires from the next announcement, on both sides. Freeing
## the node is your decision, exactly as it is for a session whose grace window expired.
func release_peer_seats(peer: int) -> int:
	if not _backend_has(&"release_peer_seats"):
		return 0
	var released: int = _orbit.release_peer_seats(peer)
	return released

## The same release, narrowed to ONE seat label on that connection. Answers how many entities changed.
##
## For a connection holding several seats -- local split-screen -- where only one of them is going away. A
## `seat` outside the label range answers 0 rather than releasing the whole connection, because the caller
## asked for a seat that cannot exist. 0 against a backend that predates the call.
func release_seat(peer: int, seat: int) -> int:
	if not _backend_has(&"release_seat_of"):
		return 0
	var released: int = _orbit.release_seat_of(peer, seat)
	return released

## Which SEATS a connection currently holds, ascending. Empty for a connection that drives nothing, empty
## OFFLINE, and empty against a backend that predates the call.
##
## Answered from the announced roster -- the one the last [signal seat_opened] / [signal seat_closed] pair
## described -- so it agrees with the events rather than with whatever the scene looks like part-way through a
## frame. WORKS ON BOTH SIDES: a server answers from its own registry, a client from the entity manifest it
## last received.
##
## An empty answer on a client for a connection the server has seated means the manifest carrying it has not
## arrived yet. It is reliable and it is sent on every seat change, so this resolves on its own.
func seats_of(peer: int) -> PackedInt32Array:
	if _mode == Mode.OFFLINE or not _backend_has(&"seats_of"):
		return PackedInt32Array()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant, and the
	# GDScript rule for a wire-ish value is that the conversion is an assignment, never a cast.
	var seats: PackedInt32Array = _orbit.seats_of(peer)
	return seats

## Every entity driven by one seat, as opaque entity ids. Empty when the seat holds none, empty OFFLINE, and
## empty against a backend that predates the call.
##
## WHAT MAKES A SEAT EVENT ACTIONABLE. [signal seat_opened] names a viewpoint; attaching a camera or a
## split-screen viewport to it needs the body, and one seat may drive several. The ids are the tokens
## `entity_id()` answers on either handle -- routinely negative, meaningless to compare or order, only ever
## passed back unmodified, and the same number on every peer.
func seat_entities(peer: int, seat: int) -> PackedInt64Array:
	if _mode == Mode.OFFLINE or not _backend_has(&"seat_entities"):
		return PackedInt64Array()
	var ids: PackedInt64Array = _orbit.seat_entities(peer, seat)
	return ids

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
## IT NEEDS NO OTHER FILTER CONFIGURED. A standing veto turns the interest pass on by itself, so a session with
## no radius and no declared membership -- the one where a per-peer refusal is the only lever there is -- gets
## the behavior described here. It used to do nothing at all in that configuration.
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

## Whether `entity_id` is currently in `peer`'s interest. False OFFLINE and against a backend that predates the
## call.
##
## A SESSION THAT CULLS NOTHING ANSWERS TRUE FOR EVERY REGISTERED ENTITY. With no [member aoi_radius] and no
## declared membership the interest pass does not run at all, so there is no set to read; "everything is in
## interest" is the honest answer there, not "nothing is".
##
## WORKS ON BOTH SIDES. A server answers from its own interest pass. A client answers from the set the interest
## sections built and ignores `peer` -- a client holds exactly one interest set, its own -- and answers true for
## everything it holds until it has received a section, because a server that culls nothing sends none.
func is_entity_in_interest(peer: int, entity_id: int) -> bool:
	if _mode == Mode.OFFLINE or not _backend_has(&"is_entity_in_interest"):
		return false
	return _orbit.is_entity_in_interest(peer, entity_id)

## Every entity in `peer`'s interest, as opaque entity ids, ascending. Empty OFFLINE and against a backend that
## predates the call.
##
## WHAT GIVES AN EDGE A STARTING POINT. [signal entity_entered_interest] and [signal entity_left_interest] are
## transitions, so a handler bound mid-session -- or a node built after the fact -- has nothing to resync from
## and would wait for the next churn. This is the standing answer, and it follows the same "culling off means
## everything" rule [method is_entity_in_interest] states.
func entities_in_interest(peer: int) -> PackedInt64Array:
	if _mode == Mode.OFFLINE or not _backend_has(&"entities_in_interest"):
		return PackedInt64Array()
	# ASSIGNED to a typed local rather than returned straight through: the call answers a Variant, and the
	# GDScript rule for a wire-ish value is that the conversion is an assignment, never a cast.
	var ids: PackedInt64Array = _orbit.entities_in_interest(peer)
	return ids

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

## The POOLED mean ticks between admissions across every band -- the one figure from
## [method bandwidth_metrics] that is read EVERY NET TICK on the authority rather than at human rates.
##
## A scalar rather than a dictionary key because it is read at tick rates: through
## [method bandwidth_metrics] it allocated a nineteen-key `Dictionary` in the backend, boxed every value, and
## rebuilt a typed copy here, per tick, forever -- on the very send path this accounting exists to make
## cheaper. Everything else in that dictionary stays where it is.
##
## This is the figure for a consumer that cannot name a peer. A shot can name its shooter, and the
## interpolation term in its rewind depth comes from [method interarrival_ticks] instead -- send cadence is a
## per-peer quantity, and pairing a pooled cadence with a per-peer round trip is the asymmetry that accessor
## exists to remove.
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

## The mean ticks between admissions for the rows sent to one peer, pooled across every band.
##
## The per-peer form of [method interarrival_all_ticks], and the one [NetLagComp] builds a shot's
## interpolation term from. Send cadence is a per-peer quantity: the byte budget is charged per peer per frame
## and the candidate list is rebuilt per peer, so a peer with a small interest set gets its rows every tick
## while a peer in a dense part of the world waits several. The round-trip term beside it
## ([method peer_rtt_ms]) is already per peer, and a pooled interpolation term grants a peer served every tick
## a window measured partly from peers served every eighth.
##
## Answers 0.0 for an unknown peer, for a peer whose window admitted nothing, and before the first window is
## published -- the same "no measurement" answer the pooled scalar gives, which
## [method NetLagComp.refresh_observed_interp_for] reads as "leave the fallback in place".
##
## Falls back to the pooled scalar against a backend that predates this accessor, rather than to 0.0. The
## committed cdylib is a bot's and can be a commit behind these sources, and answering 0.0 there would drop
## every shooter in the session to the one-tick floor -- a deeper regression than the pooled figure this
## replaces. Ask [method has_peer_interarrival] to tell the two apart.
func interarrival_ticks(peer: int) -> float:
	if _mode == Mode.OFFLINE:
		return 0.0
	if not _backend_has(&"interarrival_ticks"):
		return interarrival_all_ticks()
	return _orbit.interarrival_ticks(peer)

## Whether the LOADED cdylib carries the per-peer accessor above, as a fact separate from what it answers.
##
## The same question [method has_interarrival_scalar] asks, for the same reason: the accessor degrades to a
## figure indistinguishable from a real per-peer one, so no reading of its result can say whether the
## measurement is per peer or pooled. A probe asserting that the per-peer split is live has to ask this.
func has_peer_interarrival() -> bool:
	return _mode != Mode.OFFLINE and _backend_has(&"interarrival_ticks")

## The mean ticks between admissions for the rows in ONE distance band -- near, mid and far, the three
## [method bandwidth_metrics] keys as scalars.
##
## Scalars for the reason [method interarrival_all_ticks] is one: [NetLagComp] reads all three EVERY NET TICK
## on the authority, to derive a rewind depth per TARGET rather than one depth per shot. The send path bands a
## row by its distance from the peer's interest anchor, so a contested target and a body across the map are
## not the same age; a rewind applying the pooled figure to both errs long on the near one and short on the
## far one.
##
## Each answers 0.0 before the first window is published and for a band that admitted nothing -- which is
## every band but near in a session with no [method aoi_band_radius] configured, where the backend bands every
## row near. [method NetLagComp.refresh_band_interp] reads 0.0 as "no measurement" and leaves that band on the
## pooled figure.
##
## FAILS OPEN at 0.0 against a backend older than these sources, the same tolerance
## [method interarrival_all_ticks] carries and for the same reason: the committed cdylib is a bot's and can be
## a commit behind. Ask [method has_band_interarrival] to tell a stale binary from an unpublished window.
func interarrival_near_ticks() -> float:
	if _mode == Mode.OFFLINE or not _backend_has(&"interarrival_near"):
		return 0.0
	return _orbit.interarrival_near()

func interarrival_mid_ticks() -> float:
	if _mode == Mode.OFFLINE or not _backend_has(&"interarrival_mid"):
		return 0.0
	return _orbit.interarrival_mid()

func interarrival_far_ticks() -> float:
	if _mode == Mode.OFFLINE or not _backend_has(&"interarrival_far"):
		return 0.0
	return _orbit.interarrival_far()

## Whether the LOADED cdylib carries the three band accessors above, as a fact separate from what they answer.
##
## `interarrival_near` predates the pair beside it, so this asks for the two that do not: a binary carrying
## only the near scalar answers 0.0 for mid and far, which is indistinguishable from a session that has no
## band scale configured. A probe asserting the per-target rewind is live has to ask this.
func has_band_interarrival() -> bool:
	return _mode != Mode.OFFLINE and _backend_has(&"interarrival_mid") and _backend_has(&"interarrival_far")

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
##   unproven_acks_s                -- acks discarded because the frame token quoted was not the one this
##                                     server minted for the tick the ack named, so the peer cannot have
##                                     received the frame it claimed. SERVER-SIDE ONLY, same reason. An
##                                     honest client cannot produce one. A sustained reading is a peer
##                                     sending acks it cannot substantiate, and that peer pays for it in
##                                     the next column: its acked_base never advances, so blocks_full_s
##                                     climbs toward blocks_admitted_s.
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
##   interest_ms                    -- ms/tick in the interest pass. The cost of whichever path ran, and the
##                                     column to watch when a host overruns its net tick.
##   interest_grid                  -- fraction of the window's ticks whose interest pass ran through the SPATIAL
##                                     INDEX rather than the flat scan: 0.0 all-scan, 1.0 all-grid, in between
##                                     while the session crosses the threshold. THE VERDICT, REPORTED -- there is
##                                     no setter, and there is deliberately none: the session picks its path from
##                                     its own occupancy each tick, and a wrong pick costs time and nothing else,
##                                     because the two paths are proven to compute identical members, identical
##                                     per-member distances and identical leaves. Read it BESIDE interest_ms and
##                                     nowhere else -- it can never explain a behavior difference, only which
##                                     cost interest_ms is the cost of. A whole window at a fraction strictly
##                                     between 0.0 and 1.0 means the occupancy is hovering in the selector's
##                                     hysteresis band, which describes the arena rather than a fault. A session
##                                     with no aoi_radius reads 0.00 always: there is no distance to index.
##   interarrival_near/mid/far      -- mean ticks between admissions per distance band. The evidence S6 demanded
##                                     before rate tiering may be enabled.
##   peers / interest_entities      -- peers synced, and the mean size of ONE peer's interest set
##   rtt_at_ceiling_peers           -- connected peers whose RAW round trip is above
##                                     [method rtt_believed_max_ms], so peer_rtt_ms reports the ceiling for them
##                                     rather than what was measured. A GAUGE like starve_ticks_max, not a
##                                     per-second rate: the standing count as of the publish. A subset of
##                                     `peers`, so read it against that one -- 3 of 4 says the ceiling is the
##                                     session's policy for nearly everybody, 3 of 40 says three players are
##                                     having a bad time. Persistently large is the reading that says the
##                                     ceiling is set too low for the population actually playing. Non-zero is
##                                     not an accusation: a peer above the ceiling is either lagging its acks
##                                     deliberately or genuinely that far away, and nothing can tell those apart.
func bandwidth_metrics() -> Dictionary[String, float]:
	var out: Dictionary[String, float] = {
		"tx_bytes_s": 0.0, "tx_datagrams_s": 0.0, "tx_wire_bytes_s": 0.0, "tx_peak_peer_bytes_s": 0.0,
		"rx_bytes_s": 0.0, "rx_datagrams_s": 0.0,
		"blocks_admitted_s": 0.0, "blocks_deferred_s": 0.0, "blocks_culled_s": 0.0,
		"want_full_nacks_s": 0.0, "unproven_acks_s": 0.0, "stale_blocks_s": 0.0, "blocks_oversize_s": 0.0,
		"blocks_full_s": 0.0,
		"starve_ticks_max": 0.0, "unsent_backlog_max": 0.0,
		"interest_ms": 0.0, "interest_grid": 0.0,
		"interarrival_near": 0.0, "interarrival_mid": 0.0, "interarrival_far": 0.0,
		"interarrival_all": 0.0,
		"peers": 0.0, "interest_entities": 0.0,
		"rtt_at_ceiling_peers": 0.0,
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

## What THIS SERVER BELIEVES about `peer`'s round trip, in milliseconds, or a NEGATIVE value when there is no
## estimate: an unknown peer, a peer that has not acknowledged a snapshot frame since it joined, a client (which
## measures nobody), or offline. The input to the per-shooter lag-compensation rewind ([NetLagComp]).
##
## NOT the same figure as `clock_metrics()["rtt_ms"]`, which is the LOCAL peer's own ping sampler and reads 0.0 on
## a server -- the pong path only ever runs client-side. Ask this one about somebody else, that one about yourself.
##
## The backend derives it from the snapshot acknowledgments it already receives, so nothing was added to the wire.
## A caller must handle the negative: "we do not know yet" is a real answer for the first moments of every join,
## and treating it as zero would hand a fresh joiner the shallowest possible rewind at exactly the moment their
## link is least settled. It is also what a backend binary older than this script answers -- see the `has_method`
## probe, and `_backend_int` below for why a valid checkout can be in that state -- and it degrades to the flat
## flat fallback window rather than erroring, because a mispaired binary must not stop the game resolving hits.
## A LISTEN HOST asking about ITSELF is answered 0.0 rather than "no estimate", and that case is real rather than
## defensive: the backend's peer table holds REMOTE peers only, so a host's own shots would otherwise fall back to
## the flat window and be rewound further than a LAN client's in the same session -- the exact inversion this
## exists to remove. The host's round trip to itself is zero by construction; nothing is measured or believed.
##
## CAPPED AT [method rtt_believed_max_ms]. The estimate is derived from acknowledgments the client chooses when
## to send, and the residual the backend's ack rules cannot close is a client that advances its ack at full rate
## behind a constant lag -- it quotes a real frame token every time and reads as a slow link. This is the figure
## every rewind input reads, so bounding it here bounds that residual for every consumer at once. Ask
## [method peer_rtt_raw_ms] for the unclamped number. A backend older than the ceiling answers the raw figure
## from both, which is the pre-ceiling behavior rather than an error.
func peer_rtt_ms(peer: int) -> float:
	if _mode == Mode.OFFLINE or not _backend_has(&"peer_rtt_ms"):
		return -1.0
	if is_server() and peer == multiplayer.get_unique_id():
		return 0.0
	return _orbit.peer_rtt_ms(peer)

## The same round trip WITHOUT the belief ceiling: what the server actually measured, on the same contract as
## [method peer_rtt_ms] -- negative for no estimate, 0.0 for a listen host asking about itself, negative OFFLINE.
##
## FOR ANYTHING THAT SHOWS A NUMBER TO A HUMAN: a scoreboard ping, a connection-quality readout, an admin tool.
## Those want to say what the link is doing, and a figure pinned at the ceiling would tell every player on a bad
## connection the same wrong number. Only the rewind input is bounded, and that is [method peer_rtt_ms].
##
## DO NOT FEED THIS TO A REWIND. It is the figure the ceiling exists to bound, so a caller reaching past
## [method peer_rtt_ms] for it has undone the bound.
##
## Falls back to [method peer_rtt_ms] against a backend that predates the split, rather than to -1.0: on such a
## binary `peer_rtt_ms` IS the raw figure, so the fallback is the right answer rather than a degraded one.
func peer_rtt_raw_ms(peer: int) -> float:
	if _mode == Mode.OFFLINE:
		return -1.0
	if not _backend_has(&"peer_rtt_raw_ms"):
		return peer_rtt_ms(peer)
	if is_server() and peer == multiplayer.get_unique_id():
		return 0.0
	return _orbit.peer_rtt_raw_ms(peer)

## The largest round trip this server will BELIEVE about a peer, in milliseconds. Clamped 0..10000 by the backend
## on set; 250.0 by default, the same figure [member NetLagComp.max_delay_ms] defaults to.
##
## THERE ARE TWO CEILINGS AND THEY BOUND DIFFERENT THINGS. This one bounds WHAT THE SERVER BELIEVES ABOUT A LINK,
## so every consumer of [method peer_rtt_ms] gets a bounded figure -- a rewind, a diagnostic, a game's own
## matchmaking rule. [member NetLagComp.max_delay_ms] bounds THE REWIND DEPTH, and the ring retention that keys
## off it. Neither subsumes the other: lowering only the rewind ceiling still leaves a fabricated round trip
## reaching anything else that asks, and lowering only this one still leaves the rewind free to ask for more
## history than the ring holds.
##
## WHAT IT DOES NOT CLOSE, plainly: a client advancing its ack at full rate behind a constant lag still reads as a
## slow link, up to this value. No wire field closes that -- the round trip is the only quantity the server can
## derive, so a deliberate lag and an honest one are the same measurement. Lowering this narrows the residual and
## narrows the honest slow link by exactly as much.
##
## SERVER-SIDE. 0.0 believes nothing about anybody: every connected peer reports a 0 ms round trip, which a
## per-shooter rewind reads as the shallowest window in the session. Reads 250.0 against a backend that predates
## the property, which is what that backend's `peer_rtt_ms` is NOT bounded by -- ask
## [method bandwidth_metrics]'s `rtt_at_ceiling_peers` to see whether a ceiling is binding on anyone.
func rtt_believed_max_ms() -> float:
	return _backend_float(&"rtt_believed_max_ms", 250.0)

func set_rtt_believed_max_ms(ms: float) -> void:
	_orbit.set(&"rtt_believed_max_ms", maxf(0.0, ms))

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
## WHAT THE BACKEND DOES NOT CHECK: the VALUES in a received input row, apart from finiteness. Each datagram
## is authenticated against the sender's session key and each row is refused unless the sender holds the input
## node's authority, but a row that decodes at the right stride is otherwise stored as-is -- a client can send
## any finite value its input schema can express. Clamp axes, bound rates and reject impossible states in
## `_rollback_tick`, on the server. docs/protocol.md states the full split.
##
## THE ONE EXCEPTION IS A NON-FINITE FLOAT. A NaN or an infinity in any input float lane is refused before the
## row enters history, always, because it is a poison value that breaks the simulation for every peer rather
## than only for its sender: an input row is RESTORED before every replayed tick, the non-finite state that
## results goes back out on the state lane, and a non-finite position has no grid cell, so the body becomes
## uncullable and replicates to every peer in every world. The row is DROPPED rather than sanitized, so the
## body coasts on its last received input down the same path a lost datagram takes. Refused rows are counted
## as `input_nonfinite` under ORBITNET_DEBUG, and the first one from a connection names that peer in a warning.
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
