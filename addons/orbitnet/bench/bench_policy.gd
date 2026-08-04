extends RefCounted
class_name BenchPolicy
## Pure bot-behaviour policies for netbench. A policy is a PURE function of (elapsed time, seed) -> one
## per-tick input frame in the [BenchSubject] neutral vocabulary: no scene, no physics, no RNG state carried
## between calls, so it is deterministic and unit-testable in isolation. [BenchBot] is the thin driver that
## calls this each net tick and pushes the frame through [method BenchSubject.apply_input] -- the same seam a
## recorded tape replays through, so a bot exercises the real prediction/replication path rather than a
## bench-only shortcut.
##
## This is the "session-scale bot fleet" layer of the bench: real headless clients driven through the real
## input path. The behaviours are deliberately simple, cyclic and motion-rich (strafing, turning, firing) so
## a client generates continuous input, state churn and events for the netcode to replicate under
## impairment -- NOT to play the game well. The per-seed phase offset lets a fleet move out of lockstep (so N
## clients are not a single correlated waveform) while each stays reproducible.
##
## The frame is a plain Dictionary because the bench cannot name a game's input type -- see [BenchSubject].
## A game maps the vocabulary onto its own input object; keys it has no use for are ignored, so the same
## five policies drive an EVA shooter and an RTS without either knowing about the other.

## The behaviour catalog. Keep in sync with [method policy_from_name] / [method names].
enum Policy {
	IDLE,          # no input -- a still, connected body (baseline replication of a stationary remote)
	STRAFE,        # oscillating strafe + gentle turn (steady actuation + rotation churn)
	ORBIT,         # constant forward-arc translation + turn -- a body circling, sustained motion for remotes
	WANDER,        # lissajous translation + slow turn drift -- broad, non-repeating-looking coverage
	STRAFE_FIRE,   # strafe + held aim + burst fire -- adds event replication to the load
}

## Build the input frame for `policy` at `t` seconds of elapsed drive time, with `seed` selecting a phase
## offset. Pure: identical (policy, t, seed) always yields an identical frame. `t` is the bot's own
## accumulated clock, not the shared net time -- the bench does not assert cross-client determinism here,
## only that each client drives continuous, bounded, reproducible motion.
static func frame(policy: Policy, t: float, seed: int) -> Dictionary:
	# A scripted frame supplies its own facing (there is no live viewport behind a headless bot).
	var f: Dictionary = {
		BenchSubject.KEY_TRANSLATE: Vector3.ZERO,
		BenchSubject.KEY_ROTATE: Vector3.ZERO,
		BenchSubject.KEY_AIM_DIR: Vector3(0.0, 0.0, -1.0),
		BenchSubject.KEY_AIM_HELD: false,
		BenchSubject.KEY_FIRE: false,
	}
	var phase: float = float(seed % 997) * 0.0131   # deterministic per-seed offset, spreads a fleet out of lockstep
	match policy:
		Policy.IDLE:
			pass
		Policy.STRAFE:
			f[BenchSubject.KEY_TRANSLATE] = Vector3(sin(t * 2.0 + phase), 0.0, 0.0)
			f[BenchSubject.KEY_ROTATE] = Vector3(0.0, 0.30 * sin(t * 0.70 + phase), 0.0)
		Policy.ORBIT:
			f[BenchSubject.KEY_TRANSLATE] = Vector3(0.60, 0.0, -0.40)
			f[BenchSubject.KEY_ROTATE] = Vector3(0.0, 0.50, 0.0)
		Policy.WANDER:
			f[BenchSubject.KEY_TRANSLATE] = Vector3(sin(t * 0.90 + phase), 0.0, cos(t * 0.60 + phase))
			f[BenchSubject.KEY_ROTATE] = Vector3(0.10 * sin(t * 0.50 + phase), 0.40 * cos(t * 0.30 + phase), 0.0)
		Policy.STRAFE_FIRE:
			f[BenchSubject.KEY_TRANSLATE] = Vector3(sin(t * 2.0 + phase), 0.0, 0.0)
			f[BenchSubject.KEY_AIM_DIR] = Vector3(sin(t * 0.40 + phase), 0.0, -1.0).normalized()
			f[BenchSubject.KEY_AIM_HELD] = true
			f[BenchSubject.KEY_FIRE] = fmod(t, 1.0) < 0.5   # 0.5s held / 0.5s released bursts
	return f

## Resolve a policy name (CLI `--bench-bot=<name>` / a harness arg) to a Policy, defaulting to STRAFE for an
## unknown/empty name (the caller can validate against [method names] first if it wants to reject typos).
static func policy_from_name(name: String) -> Policy:
	match name.strip_edges().to_lower():
		"idle":
			return Policy.IDLE
		"strafe":
			return Policy.STRAFE
		"orbit":
			return Policy.ORBIT
		"wander":
			return Policy.WANDER
		"strafe_fire", "fire":
			return Policy.STRAFE_FIRE
	return Policy.STRAFE

## Whether a policy name is one the catalog knows (for arg validation).
static func has_policy(name: String) -> bool:
	return names().has(name.strip_edges().to_lower())

## Every policy name, for `--bench-bot` help / validation.
static func names() -> PackedStringArray:
	return PackedStringArray(["idle", "strafe", "orbit", "wander", "strafe_fire"])
