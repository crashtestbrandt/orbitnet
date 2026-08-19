extends RefCounted
class_name BenchSubject
## The seam between netbench and the GAME it is benching.
##
## netbench needs four things from a game and nothing else: to know when a session is live, to reach the
## locally-owned body, to push one tick-pure input frame into it, and to read that body's per-tick health
## numbers. Everything else the bench does -- the impairment relay, the profiles, the tape codec, the
## tick-domain gates -- is already pure. This class is those four things, and implementing it is the entire
## cost of pointing netbench at a new game.
##
## [method remote_bodies] is a fifth, and it is OPTIONAL rather than one of the four: leave it alone and every
## gate still runs, minus the remote-cadence reading.
##
## Subclass it, and hand an instance to [BenchProbe.subject] before the probe enters the tree:
##
##     var probe := BenchProbe.new()
##     probe.subject = MyGameBenchSubject.new()
##     add_child(probe)
##
## THE FRAME IS A PLAIN DICTIONARY, deliberately. The bench cannot name a game's input type -- that is
## exactly the coupling this seam removes -- and a Dictionary is what survives the [InputTape]
## [method @GlobalScope.var_to_bytes] round-trip unchanged. [BenchPolicy] authors frames in the neutral
## vocabulary below; [method apply_input] translates that vocabulary into whatever the game's own input
## object is. A key a game does not use is simply ignored, and a key it needs but the policy never sets
## keeps the game's own default -- so the vocabulary can grow without invalidating recorded tapes.
##
## SUBCLASSES MUST NOT ASSUME A BODY EXISTS. On a client the owned body arrives asynchronously, after the
## handshake and the server's spawn; on a dedicated server it never arrives at all. Return null from
## [method local_body] until it does and emit [signal subject_ready] when it lands.

# --- the neutral input vocabulary ---------------------------------------------------------------
# Key names are constants rather than bare strings so a typo is a compile-time miss in the bench and in
# every subject, and so a recorded tape's field names are pinned by the library rather than by a caller.

## Vector3 -- translation intent in the body's own frame, components nominally in [-1, 1].
const KEY_TRANSLATE: String = "translate"
## Vector3 -- rotation intent (pitch, yaw, roll), components nominally in [-1, 1].
const KEY_ROTATE: String = "rotate"
## Vector3 -- unit aim/look direction. A game with no aim concept ignores it.
const KEY_AIM_DIR: String = "aim_dir"
## bool -- whether aim is held this tick (ADS, a held cursor, a drag).
const KEY_AIM_HELD: String = "aim_held"
## bool -- whether the primary action fires this tick (shoot, issue order).
const KEY_FIRE: String = "fire"

# --- the per-tick sample vocabulary -------------------------------------------------------------
# What [method sample] may return. All optional: [BenchMetrics] reads each with a 0.0 default, so a game
# with no prediction (a server-authoritative RTS, say) implements sample() as an empty Dictionary and
# every clock/rollback metric still records, because those come from the facade, not from the game.

## float -- the current owner-prediction error, in the game's own units. 0 where nothing is predicted.
const KEY_RECONCILE_ERROR: String = "reconcile_error"
## float -- count of corrections absorbed by smoothing so far (monotonic).
const KEY_RECONCILE_SMOOTH: String = "reconcile_smooth"
## float -- count of corrections that had to SNAP so far (monotonic). The bench gate reads this one: a
## snap is a visible teleport, so a profile bounds how many a run may produce.
const KEY_RECONCILE_SNAP: String = "reconcile_snap"

## Emitted when the locally-owned body becomes available (or becomes available AGAIN after a respawn).
## [BenchProbe] and [BenchBot] bind to it rather than polling.
signal subject_ready(body: Node)

## Whether the session is live and simulating -- i.e. whether it is meaningful to drive input and record
## metrics right now. Typically "connected AND in the playing state", NOT merely "a peer exists": samples
## taken during bringup would otherwise poison the gate's distributions.
func is_ready() -> bool:
	return false

## The locally-owned body, or null when there is none yet (pre-spawn) or never will be (dedicated server).
## Callers must re-check [method @GlobalScope.is_instance_valid] -- a body can be freed mid-run (death,
## teardown) between the call and its use.
func local_body() -> Node:
	return null

## Feed ONE tick-pure input frame to the locally-owned body, in the neutral vocabulary above. Called at most
## once per NET tick. Implement it against the same scripted-input seam the game's own probes drive, so a bot
## exercises the real prediction and replication path rather than a bench-only shortcut.
##
## An EMPTY dictionary means "release": stop overriding and hand the body back to live input. [BenchBot] and
## the tape replay both send it on teardown, so a subject must handle it.
func apply_input(_frame: Dictionary) -> void:
	pass

## The input frame the body actually CONSUMED this tick, in the neutral vocabulary, or an empty Dictionary
## when there is nothing to record. This is the dual of [method apply_input] and it is what makes
## `--bench-record` work: a human plays, and every frame the body really used is appended to a tape that can
## later be replayed through [method apply_input] under each impairment profile.
##
## A game that only ever wants to replay bot tapes can leave this returning an empty Dictionary; recording
## then produces an empty tape, which the probe reports rather than silently saving.
func capture_input() -> Dictionary:
	return {}

## Per-tick health numbers for `body`, in the sample vocabulary above. Anything the facade already publishes
## (clock RTT/jitter/stretch/offset, rollback resim depth and loop ms) is read by [BenchMetrics] directly
## from `Net` and must NOT be duplicated here -- this is only for what the GAME knows and the library
## cannot.
func sample(_body: Node) -> Dictionary:
	return {}

## The REMOTE bodies this peer is watching -- every replicated body that is not the local one. OPTIONAL: the
## default publishes nothing, and a game that leaves it alone simply gets no cadence reading.
##
## [BenchMetrics] feeds these to [RemoteCadence], which answers "how often does a remote body's authoritative
## pose actually reach this client" -- the one thing a player complains about that no local-player metric can
## see. Only [Node3D]s are measured, since the reading is a pose change and a distance.
##
## Return whatever the game already has to hand (a group query, a spawner's registry); this is called once per
## net tick, so a walk of the whole scene tree is the wrong implementation.
func remote_bodies() -> Array[Node]:
	return []

## Called once when the bench run finishes, so a subject can drop signal connections and hand the body back
## to live input. The default releases input, which is right for almost every implementation.
func release() -> void:
	apply_input({})

# --- helpers for subclasses ---------------------------------------------------------------------
# Reading a wire/tape-decoded Dictionary means reading Variants. These do the typed-local dance once, so
# every subject is not re-implementing it (and so none of them reaches for an as-cast, which the typed
# GDScript rules ban).

## Read a Vector3 field, or `fallback` when absent or of the wrong type.
static func vec3_field(frame: Dictionary, key: String, fallback: Vector3 = Vector3.ZERO) -> Vector3:
	if not frame.has(key):
		return fallback
	var v: Variant = frame[key]
	if v is Vector3:
		var out: Vector3 = v
		return out
	return fallback

## Read a bool field, or `fallback` when absent or of the wrong type.
static func bool_field(frame: Dictionary, key: String, fallback: bool = false) -> bool:
	if not frame.has(key):
		return fallback
	var v: Variant = frame[key]
	if v is bool:
		var out: bool = v
		return out
	return fallback

## Read a float field, or `fallback` when absent or of the wrong type. Accepts an int too (a Dictionary
## literal writes `0` where `0.0` was meant, and JSON-ish round-trips do the same).
static func float_field(frame: Dictionary, key: String, fallback: float = 0.0) -> float:
	if not frame.has(key):
		return fallback
	var v: Variant = frame[key]
	if v is float:
		var out: float = v
		return out
	if v is int:
		var as_int: int = v
		return float(as_int)
	return fallback
