#!/usr/bin/env bash
# Headless GDScript lint for one Godot project in this repo.
#
# Godot's --check-only mode is per-script and does not load project autoloads, so it cannot see an error
# that only appears once `Net` exists. This loads the whole project instead and fails when Godot reports
# GDScript compiler/script errors in its log.
#
# Usage: tools/lint-gdscript.sh <project-dir>
# Env:   GODOT (binary or wrapper; default: tools/godot-quiet.sh)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROJECT="${1:-}"
if [ -z "$PROJECT" ] || [ ! -f "$ROOT/$PROJECT/project.godot" ]; then
	printf 'usage: %s <project-dir>   (e.g. demos/rts, harness)\n' "$0" >&2
	exit 2
fi
GODOT="${GODOT:-$ROOT/tools/godot-quiet.sh}"
PROJECT_DIR="$ROOT/$PROJECT"

# The synced addon must be present, or every script that names `Net` fails for a reason that has nothing to
# do with the change under test. Fail with the fix rather than with 400 parse errors.
if [ ! -d "$PROJECT_DIR/addons/orbitnet" ]; then
	printf 'lint FAILED: %s/addons/orbitnet is missing -- run `just sync-addons` first.\n' "$PROJECT" >&2
	exit 1
fi

LOG_FILE="$(mktemp "${TMPDIR:-/tmp}/orbitnet-lint.XXXXXX")"
trap 'rm -f "$LOG_FILE"' EXIT HUP INT TERM

# macOS headless can crash inside MoltenVK during the resource --import pass (a Metal RenderingDevice is
# created even under --headless, regardless of --rendering-driver). The project LOAD pass still surfaces
# GDScript compiler errors, so on macOS we render via opengl3 and treat --import as best-effort. Linux and
# CI -- the authoritative gate -- are untouched: RENDER_ARGS word-splits to nothing and both passes stay fatal.
RENDER_ARGS=""
IMPORT_FATAL=1
if [ "$(uname)" = "Darwin" ]; then
	RENDER_ARGS="--rendering-driver opengl3 --audio-driver Dummy"
	IMPORT_FATAL=0
fi

run_godot() {
	fatal="$1"; label="$2"; shift 2
	printf '>> %s\n' "$label" >>"$LOG_FILE"
	set +e
	"$GODOT" "$@" >>"$LOG_FILE" 2>&1
	status="$?"
	set -e
	if [ "$status" -ne 0 ]; then
		if [ "$fatal" -eq 1 ]; then
			printf 'Godot failed during %s (exit %s).\n\n' "$label" "$status"
			cat "$LOG_FILE"
			exit "$status"
		fi
		printf '   (non-fatal on this platform: %s exited %s; relying on the load pass)\n' "$label" "$status"
	fi
}

# Prime the import cache on a cold checkout BEFORE the checked passes. A GDExtension perturbs the FIRST cold
# --import's global-class-cache build order, so autoload singleton types (e.g. `Net`) can transiently resolve
# as their base `Node` -- tripping unsafe-method-access warnings-as-errors on CORRECT code that then vanish on
# every subsequent import. Prime once into /dev/null (never $LOG_FILE, so it is never grepped). Best-effort:
# a genuine script error recurs on every import, warm or cold, so this absorbs only the cold-cache transient.
"$GODOT" --headless --no-header $RENDER_ARGS --path "$PROJECT_DIR" --import >/dev/null 2>&1 || true

run_godot "$IMPORT_FATAL" "import/project script registration" \
	--headless --no-header $RENDER_ARGS --path "$PROJECT_DIR" --import
run_godot 1 "main scene script load" \
	--headless --no-header $RENDER_ARGS --path "$PROJECT_DIR" --quit

ERROR_PATTERN='SCRIPT ERROR|Parse Error|Compile Error|Failed to load script|GDScript::reload'
if grep -E "$ERROR_PATTERN" "$LOG_FILE" >/dev/null; then
	printf 'GDScript lint FAILED for %s. Matching diagnostics:\n\n' "$PROJECT"
	grep -E "$ERROR_PATTERN" "$LOG_FILE" || true
	printf '\nFull Godot log:\n\n'
	cat "$LOG_FILE"
	exit 1
fi

printf 'GDScript lint passed: %s\n' "$PROJECT"
