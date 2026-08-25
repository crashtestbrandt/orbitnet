#!/usr/bin/env bash
# OrbitNet native load smoke.
#
# Proves the whole Rust -> cdylib -> Godot chain end to end: the library builds, Godot loads the
# GDExtension, the Rust-defined classes register, their exported properties bind, their signals
# reach GDScript, and the tick clock actually advances.
#
# Deliberately runs against a THROWAWAY project in a temp directory, never against harness/ or a demo.
# That is the point: it proves the extension registers its classes OUTSIDE any project that has been
# set up for it -- no autoload, no plugin.cfg, no addon GDScript. If this passes and a project still
# fails, the fault is in the project's configuration, not in the library.
#
# Usage: tools/orbitnet-smoke.sh [--skip-build]
# Env:   GODOT (binary or wrapper to run; default: tools/godot-quiet.sh)
#
# Linux only: it reads ELF magic and asks `nm -D` for the entry symbol.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/addons/orbitnet_native/bin"
GODOT="${GODOT:-$ROOT/tools/godot-quiet.sh}"

# THE HOST'S TARGET, NOT A HARDCODED ONE. The smoke test loads the binary Godot will load, which is this
# machine's; naming `linux` on a macOS or Windows host asks cargo to cross-compile, and it answers by
# producing nothing at the path this script then looks in. `build-native.sh host` is the same detection
# `just native-build` uses, so the two stage the same files.
HOST="$("$ROOT/tools/build-native.sh" host)"

if [ "${1:-}" != "--skip-build" ]; then
	printf 'orbitnet-smoke: building both descriptor profiles for %s\n' "$HOST"
	"$ROOT/tools/build-native.sh" build "$HOST" "$BIN"
fi

# Every name this platform's descriptor entries resolve to. Checking the SET rather than one file is
# what catches a profile that failed to stage: the throwaway project runs from source and loads only
# `template_debug`, so a missing `template_release` would pass here and fail one export later.
NAMES="$("$ROOT/tools/build-native.sh" names "$HOST")"
missing=""
for name in $NAMES; do
	[ -s "$BIN/$name" ] || missing="$missing $name"
done
if [ -n "$missing" ]; then
	printf 'orbitnet-smoke FAILED: %s holds no library named:%s\n' "$BIN" "$missing" >&2
	printf 'No binary is committed to this repository. Run `just native-install`.\n' >&2
	exit 1
fi

for name in $NAMES; do
	lib="$BIN/$name"
	# A pointer file is a few hundred bytes of text that dlopen rejects with "invalid ELF header" behind
	# a confusing cascade. Name the real cause here rather than letting Godot guess at it.
	if head -c 64 "$lib" | grep -q 'git-lfs.github.com' 2>/dev/null; then
		printf 'orbitnet-smoke FAILED: %s is a Git LFS POINTER, not a library.\n' "$lib" >&2
		exit 1
	fi
	# The entry symbol the .gdextension names. If this is absent the library will load and then do
	# nothing, which presents as "class not found" far from the real cause.
	#
	# TWO SPELLINGS, BECAUSE `nm` IS NOT ONE TOOL. GNU binutils wants `-D` for an ELF's dynamic symbols;
	# Apple's refuses `-D` and, on a UNIVERSAL dylib, refuses a plain read too ("File format has no dynamic
	# symbol table") because a fat file holds one table per architecture rather than one overall. `-arch all`
	# is what asks it for every slice. Neither flag is portable, so both are tried and the check only fails
	# when a reader that WORKED found no symbol -- a `nm` that cannot read the file at all is not evidence
	# that the file is wrong.
	if command -v nm >/dev/null 2>&1; then
		# EVERY SPELLING IS TRIED SEPARATELY, and one that errors is not allowed to end the search. Grouping
		# them into one pipeline does not work under `set -e`: the first reader that exits non-zero aborts the
		# whole group, so the spelling that would have worked is never reached.
		read_ok=0
		found=0
		for reader in "-D" "-arch all" ""; do
			# shellcheck disable=SC2086 -- `reader` is a deliberate word split of a fixed flag list.
			out="$(nm $reader "$lib" 2>/dev/null || true)"
			[ -z "$out" ] && continue
			read_ok=1
			printf '%s' "$out" | grep -q 'gdext_rust_init' && found=1 && break
		done
		if [ "$read_ok" = "1" ] && [ "$found" != "1" ]; then
			printf 'orbitnet-smoke FAILED: %s exports no gdext_rust_init symbol.\n' "$lib" >&2
			exit 1
		fi
		if [ "$read_ok" != "1" ]; then
			printf 'orbitnet-smoke: no nm on this host could read %s; skipping the symbol check\n' "$lib"
		fi
	fi
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
BIN_DIR="$WORK/addons/orbitnet_native/bin"
mkdir -p "$BIN_DIR"

cp "$ROOT/addons/orbitnet_native/orbitnet.gdextension" "$WORK/addons/orbitnet_native/orbitnet.gdextension"
# The whole set, under the shipped names, so the descriptor copied beside it resolves verbatim.
for name in $NAMES; do
	cp "$BIN/$name" "$BIN_DIR/$name"
done

cat > "$WORK/project.godot" <<'PROJECT'
config_version=5

[application]

config/name="orbitnet-smoke"
config/features=PackedStringArray("4.4")
PROJECT

cat > "$WORK/smoke.gd" <<'SMOKE'
extends SceneTree

var _net: Node = null
var _ticks: int = 0
var _frames: int = 0
var _tick_mismatch: String = ""

func _initialize() -> void:
	if not ClassDB.class_exists("OrbitNet"):
		printerr("ORBIT-SMOKE FAIL: the OrbitNet class did not register")
		quit(1)
		return
	if not ClassDB.class_exists("OrbitRollbackSynchronizer"):
		printerr("ORBIT-SMOKE FAIL: OrbitRollbackSynchronizer did not register")
		quit(1)
		return

	var obj: Object = ClassDB.instantiate("OrbitNet")
	_net = obj as Node
	root.add_child(_net)

	# Exported properties must bind both ways.
	_net.set("tickrate", 60)
	_net.set("sync_to_physics", false)
	var read_back: int = int(_net.get("tickrate"))
	if read_back != 60:
		printerr("ORBIT-SMOKE FAIL: tickrate export did not round-trip (got %d)" % read_back)
		quit(1)
		return

	_net.connect("before_tick", _on_before_tick)
	_net.call("start")
	print("ORBIT-SMOKE proto=%s (%d)" % [
		str(_net.call("protocol_version_string")), int(_net.call("protocol_version"))])

	# The schema plumbing resolves real properties off a real node.
	var sync_obj: Object = ClassDB.instantiate("OrbitRollbackSynchronizer")
	var sync: Node = sync_obj as Node
	var subject := Node3D.new()
	root.add_child(subject)
	subject.add_child(sync)
	sync.set("state_properties", PackedStringArray(["position", "quaternion"]))
	sync.call("process_settings")
	var stride: int = int(sync.call("row_stride"))
	var props: int = int(sync.call("property_count"))
	var unresolved: PackedStringArray = sync.call("unresolved_properties")
	if props != 2 or stride != 28 or unresolved.size() != 0:
		printerr("ORBIT-SMOKE FAIL: schema resolve wrong (props=%d stride=%d unresolved=%d)"
			% [props, stride, unresolved.size()])
		quit(1)
		return
	print("ORBIT-SMOKE schema %s" % str(sync.call("describe")))

	# The per-peer observer declaration binds and round-trips. It is a `#[func]` quartet plus the entity-id
	# token it consumes, and nothing else in the tree would notice one of them failing to register: a peer
	# declared into a world would silently fall back to inferring both its centre and its world from the body
	# it drives, which is the inference this declaration exists to replace.
	if not sync.has_method("get_entity_id"):
		printerr("ORBIT-SMOKE FAIL: the entity id the anchor declaration names is not published")
		quit(1)
		return
	_net.call("set_peer_anchor", 7, Vector3(10.0, 0.0, -4.0), 3)
	var declared: int = int(_net.call("peer_membership", 7))
	_net.call("set_peer_anchor_entity", 7, 123456789, 5)
	var retracked: int = int(_net.call("peer_membership", 7))
	_net.call("clear_peer_anchor", 7)
	var cleared: int = int(_net.call("peer_membership", 7))
	if declared != 3 or retracked != 5 or cleared != 0:
		printerr("ORBIT-SMOKE FAIL: the peer anchor declaration did not round-trip (%d/%d/%d)"
			% [declared, retracked, cleared])
		quit(1)
		return
	print("ORBIT-SMOKE peer anchor declaration round-trips")

func _on_before_tick(_delta: float, tick: int) -> void:
	_ticks += 1
	# current_tick() inside a handler must equal the tick being run. Consuming code stamps captured
	# state with it from inside the rollback tick, so a divergence here would be an invisible
	# off-by-one rather than a visible failure.
	var seen: int = int(_net.call("current_tick"))
	if seen != tick and _tick_mismatch == "":
		_tick_mismatch = "current_tick()=%d but the signal says %d" % [seen, tick]

func _process(_delta: float) -> bool:
	if _net == null:
		return true
	_frames += 1
	if _tick_mismatch != "":
		printerr("ORBIT-SMOKE FAIL: %s" % _tick_mismatch)
		quit(1)
		return true
	if _ticks >= 5:
		print("ORBIT-SMOKE OK ticks=%d current_tick=%d" % [_ticks, int(_net.call("current_tick"))])
		return true
	if _frames > 900:
		printerr("ORBIT-SMOKE FAIL: only %d ticks after %d frames" % [_ticks, _frames])
		quit(1)
		return true
	return false
SMOKE

cat > "$WORK/lifecycle.gd" <<'LIFECYCLE'
extends SceneTree
## Entity-lifecycle regression: a synchronizer that is FREED must never take the frame down.
##
## A synchronizer can only enqueue its unregister from exit_tree, and that queue drains at the top of the
## next process/physics_process -- so between a body's deletion and that drain the registry still holds a
## handle to a dead node. Every despawn opens that window (queue_free deletes at the end of the frame), and
## SceneMultiplayer's peer_packet poll fires inside it, which is how a player's death used to reach the
## registry through a still-in-flight input frame naming the corpse.
##
## The trap is that `Gd::clone` is NOT infallible: under godot-rust's balanced safeguards (what a release
## build ships) cloning a dead handle PANICS, so the defensive `clone(); if !is_instance_valid()` shape
## never reached its own guard. Each case below frees a registered body in a different window; the wrapper
## fails the run if the log carries a freed-instance panic, and the tick assertions below catch the frame
## the panic would have eaten.

const _MODE_SERVER: int = 2

var _net: Node = null
var _frames: int = 0
var _ticks: int = 0
var _stage: int = 0
var _armed: Node = null
var _ticks_after_frees: int = -1   # tick count once every free case has landed (stage 5's baseline)
var _fail: String = ""

func _initialize() -> void:
	var obj: Object = ClassDB.instantiate("OrbitNet")
	_net = obj as Node
	_net.set("tickrate", 60)
	_net.set("sync_to_physics", false)
	root.add_child(_net)
	_net.connect("before_tick", _on_before_tick)
	_net.call("set_mode", _MODE_SERVER)
	_net.call("start")

# A registered body under `name`, freshly resolved (so it is queued for the registry).
func _spawn(name: String) -> Node:
	var body := Node3D.new()
	body.name = name
	root.add_child(body)
	var sync_obj: Object = ClassDB.instantiate("OrbitRollbackSynchronizer")
	var sync: Node = sync_obj as Node
	sync.name = "OrbitSync"
	sync.set("root", body)
	body.add_child(sync)
	sync.set("state_properties", PackedStringArray(["position"]))
	sync.call("process_settings")
	return body

func _on_before_tick(_delta: float, _tick: int) -> void:
	_ticks += 1
	# CASE 3: free a registered body from inside the tick, after drain_pending has already run. The rest of
	# this frame (capture_inputs, mark_forward_ticks, the rollback phases, the send path) then walks a
	# registry entry whose node is already gone -- the same position peer_packet occupies in a real session.
	if _armed != null:
		_armed.free()
		_armed = null

func _process(_delta: float) -> bool:
	_frames += 1
	if _fail != "":
		printerr("ORBIT-LIFECYCLE FAIL: %s" % _fail)
		quit(1)
		return true
	if _frames > 900:
		printerr("ORBIT-LIFECYCLE FAIL: timed out at stage %d (%d ticks)" % [_stage, _ticks])
		quit(1)
		return true
	match _stage:
		0:
			if _frames < 3:
				return false
			# CASE 1: registered and freed inside one frame -- drain_pending finds a dead handle in its
			# own pending queue, never having seen a matching unregister.
			_spawn("Doomed").free()
			_stage = 1
		1:
			# CASE 2: registered, allowed to drain into the registry, then freed between frames -- the
			# deferred unregister has not run yet, so the whole registry sweep meets a dead handle.
			_spawn("Settled")
			_stage = 2
		2:
			var settled: Node = root.get_node_or_null("Settled")
			if settled == null:
				_fail = "the settled body vanished before it could be freed"
				return false
			settled.free()
			_stage = 3
		3:
			_armed = _spawn("Armed")
			_stage = 4
		4:
			if _armed != null:
				return false   # waiting for the before_tick handler to free it
			_ticks_after_frees = _ticks   # baseline: every free case has now landed
			_stage = 5
		5:
			# The loop must keep ticking WITH the frees behind it. A freed-handle panic unwinds out of
			# process/physics_process, so it eats the whole frame -- a stalled counter here is that
			# showing up as behaviour rather than as a log line, and the frame watchdog above turns a
			# permanent stall into a failure instead of a hang. Baselined on stage entry (above), so the
			# comparison spans calls rather than reading the same value on both sides.
			if _ticks < _ticks_after_frees + 5:
				return false
			# A body registering AFTER the frees must still drain in cleanly -- the pending queue and the
			# registry have to survive the window, not just avoid panicking inside it.
			_spawn("Live")
			_stage = 6
		6:
			if _ticks < _ticks_after_frees + 10:
				return false
			if root.get_node_or_null("Live") == null:
				_fail = "the survivor spawned after the frees did not stay in the tree"
				return false
			print("ORBIT-LIFECYCLE OK ticks=%d (+%d since the frees) frames=%d"
				% [_ticks, _ticks - _ticks_after_frees, _frames])
			return true
	return false
LIFECYCLE

# Godot discovers .gdextension files while scanning the project, and a fresh project has no scan
# cache. Without this pass the library is never loaded and the classes simply do not exist, which
# presents identically to a genuinely broken build.
"$GODOT" --headless --path "$WORK" --import >/dev/null 2>&1 || true

LOG="$WORK/smoke.log"
set +e
"$GODOT" --headless --path "$WORK" --script smoke.gd 2>&1 | tee "$LOG"
status="${PIPESTATUS[0]}"
set -e

if [ "$status" -ne 0 ]; then
	printf 'orbitnet-smoke FAILED: Godot exited %d\n' "$status" >&2
	exit 1
fi
if ! grep -q 'ORBIT-SMOKE OK' "$LOG"; then
	printf 'orbitnet-smoke FAILED: no ORBIT-SMOKE OK marker in the output\n' >&2
	exit 1
fi
if grep -q 'ORBIT-SMOKE FAIL' "$LOG"; then
	printf 'orbitnet-smoke FAILED: the smoke script reported a failure\n' >&2
	exit 1
fi

# Entity lifecycle: freeing a registered body must not panic the frame (see lifecycle.gd's header).
LIFE_LOG="$WORK/lifecycle.log"
set +e
"$GODOT" --headless --path "$WORK" --script lifecycle.gd 2>&1 | tee "$LIFE_LOG"
life_status="${PIPESTATUS[0]}"
set -e

if [ "$life_status" -ne 0 ]; then
	printf 'orbitnet-smoke FAILED: the lifecycle script exited %d\n' "$life_status" >&2
	exit 1
fi
if ! grep -q 'ORBIT-LIFECYCLE OK' "$LIFE_LOG"; then
	printf 'orbitnet-smoke FAILED: no ORBIT-LIFECYCLE OK marker in the output\n' >&2
	exit 1
fi
# The regression itself. godot-rust reports a freed-instance access as a recovered panic rather than a
# crash, so it never shows up as a non-zero exit -- it has to be read out of the log. Any of these means a
# registry read cloned or bound a handle whose node was already gone.
if grep -qE 'after it has been freed|while a bind\(\) or bind_mut\(\) call was active' "$LIFE_LOG"; then
	printf 'orbitnet-smoke FAILED: the extension touched a freed instance --\n' >&2
	grep -E 'after it has been freed|while a bind\(\) or bind_mut\(\) call was active' "$LIFE_LOG" >&2
	exit 1
fi

printf 'orbitnet-smoke passed: the extension loaded, classes registered, ticks advanced, and a freed entity did not panic the frame.\n'
