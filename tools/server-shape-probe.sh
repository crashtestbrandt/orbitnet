#!/usr/bin/env bash
# Two-process gate for the two SERVER SHAPES, in the harness project.
#
# Runs harness/scenes/server_shape.tscn twice -- once against a --shape=dedicated server, once against a
# --shape=listen one -- with a joining client each time, and asserts the same thing of both: the client's OWN
# seat's server-authoritative state channel delivered rows for the whole run.
#
# WHY BOTH SHAPES IN ONE SCRIPT. The scenario, the node paths, the channels and the assertions are identical
# across the two runs; the only variable is what the server itself holds. A dedicated server holds no body of
# its own, a listen server holds seat 0's and is also a peer, and every send-path pass that walks "every
# entity" therefore walks a different set on each. Running one shape alone answers nothing, because a reading
# with nothing to compare it against cannot say whether the shape is what produced it -- so a failure here
# always prints BOTH shapes' readings side by side.
#
# WHAT THE ASSERTION IS ACTUALLY READING. `NetStateHandle.last_known_state()` FAILS OPEN: against a cdylib
# with no `get_last_known_state` it answers `Net.current_tick()`, which rises whether or not a row ever
# arrived. The client prints `reports=` first and this script refuses a run where it is 0, because such a run
# measured the fallback rather than the wire.
#
# NO --fixed-fps. The backend's clock sync paces off the WALL clock; --fixed-fps stalls the ping/pong, so the
# client never finishes its handshake and the run times out looking like a netcode failure.
#
# The harness project is used rather than a demo, for the reason its project.godot states: a failure in a demo
# could be the demo's fault or the addon's, and you cannot tell without reading both.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="$ROOT/harness"
SCENE="res://scenes/server_shape.tscn"
GODOT="${GODOT:-$ROOT/tools/godot-quiet.sh}"
# Seconds. Both peers report at 4 s of session time; the server outlives the client so its own summary is
# measured against a live peer rather than one that has already gone, and so the client's last seconds are not
# spent watching a server that has quit.
CLIENT_RUN_S="${SHAPE_PROBE_CLIENT_S:-8}"
SERVER_RUN_S="${SHAPE_PROBE_SERVER_S:-10}"
WATCHDOG_S="${SHAPE_PROBE_WATCHDOG_S:-60}"
# One port per shape, so a server that has not finished releasing the previous run's socket cannot make the
# next run look like a bind failure.
BASE_PORT="${SHAPE_PROBE_PORT:-47820}"

SERVER_LOG=""
CLIENT_LOG=""
SERVER_PID=""
CLIENT_PID=""
WATCH_PID=""

cleanup() {
	[ -n "$SERVER_PID" ] && kill -9 "$SERVER_PID" 2>/dev/null
	[ -n "$CLIENT_PID" ] && kill -9 "$CLIENT_PID" 2>/dev/null
	[ -n "$WATCH_PID" ] && kill -9 "$WATCH_PID" 2>/dev/null
	rm -f "$SERVER_LOG" "$CLIENT_LOG"
	return 0
}
trap cleanup EXIT

if [ ! -d "$PROJECT/addons/orbitnet" ]; then
	printf 'server-shape-probe FAILED: harness/addons/orbitnet is missing -- run `just sync-addons` first.\n' >&2
	exit 1
fi

# Everything the two runs report, keyed by shape. Read back by the comparison at the bottom, so a failure on
# one shape prints the other's readings beside it rather than on its own.
declare -A REPORT

field() { grep -aoE "$2" "$1" | tail -1 | sed -E "s/$3//"; }

# One shape, end to end: bind a server, join a client, harvest both logs.
#
# ORBITNET_DEBUG=1 on the CLIENT only. It turns on the once-per-second wire summary, whose `skipped=` column
# counts entity blocks that decoded cleanly and named an entity this peer does not have -- the continuous
# reading, rather than the sampled one the 120th-skip print gives. A client that received rows for an entity
# it never registered is a different failure from one that received nothing, and only that column separates
# them.
run_shape() {
	local label="$1" shape="$2" port="$3"
	shift 3
	SERVER_LOG="$(mktemp "${TMPDIR:-/tmp}/shapeprobe-$label-server.XXXXXX")"
	CLIENT_LOG="$(mktemp "${TMPDIR:-/tmp}/shapeprobe-$label-client.XXXXXX")"

	echo "server-shape-probe: starting the $label server on port $port..."
	"$GODOT" --headless --path "$PROJECT" "$SCENE" -- \
		--role=server --shape="$shape" --port="$port" --run="$SERVER_RUN_S" "$@" >"$SERVER_LOG" 2>&1 &
	SERVER_PID=$!

	sleep 3   # let the server bind and start listening before the client dials

	echo "server-shape-probe: joining the $label server..."
	ORBITNET_DEBUG=1 "$GODOT" --headless --path "$PROJECT" "$SCENE" -- \
		--role=client --shape="$shape" --address=127.0.0.1 --port="$port" --run="$CLIENT_RUN_S" \
		>"$CLIENT_LOG" 2>&1 &
	CLIENT_PID=$!

	# A hung session must fail loudly rather than hang CI forever.
	( sleep "$WATCHDOG_S"; kill -9 "$SERVER_PID" "$CLIENT_PID" 2>/dev/null ) &
	WATCH_PID=$!

	wait "$CLIENT_PID" 2>/dev/null; local client_rc=$?
	wait "$SERVER_PID" 2>/dev/null; local server_rc=$?
	# Grouped and silenced: the shell announces a killed background job on the terminal ("Killed"), which
	# lands in the middle of the run's output and reads as one of the two processes having died.
	{ kill -9 "$WATCH_PID"; wait "$WATCH_PID"; } 2>/dev/null
	SERVER_PID=""; CLIENT_PID=""; WATCH_PID=""

	echo "=== $label SERVER (rc=$server_rc) ==="
	grep -aE "SHAPE-" "$SERVER_LOG" || echo "(no scenario output)"
	echo "=== $label CLIENT (rc=$client_rc) ==="
	grep -aE "SHAPE-" "$CLIENT_LOG" || echo "(no scenario output)"

	REPORT["$label.server_verdict"]="$(field "$SERVER_LOG" 'SHAPE-RESULT role=server shape=[a-z]+ (PASS|FAIL)' '.* ')"
	REPORT["$label.client_verdict"]="$(field "$CLIENT_LOG" 'SHAPE-RESULT role=client shape=[a-z]+ (PASS|FAIL)' '.* ')"
	REPORT["$label.seat"]="$(field "$CLIENT_LOG" 'SHAPE-STATE .* seat=-?[0-9]+' '.*seat=')"
	REPORT["$label.reports"]="$(field "$CLIENT_LOG" 'SHAPE-BRANCH .* seat0_reports=[01]' '.*seat0_reports=')"
	REPORT["$label.own_first"]="$(field "$CLIENT_LOG" 'own_first=-?[0-9]+' 'own_first=')"
	REPORT["$label.own_last"]="$(field "$CLIENT_LOG" 'SHAPE-STATE .* own_last=-?[0-9]+' '.*[^_]own_last=')"
	REPORT["$label.own_rises"]="$(field "$CLIENT_LOG" 'own_rises=-?[0-9]+' 'own_rises=')"
	REPORT["$label.other_last"]="$(field "$CLIENT_LOG" 'SHAPE-STATE .* other_last=-?[0-9]+' '.*[^_]other_last=')"
	REPORT["$label.other_rises"]="$(field "$CLIENT_LOG" 'other_rises=-?[0-9]+' 'other_rises=')"
	REPORT["$label.body_own"]="$(field "$CLIENT_LOG" 'SHAPE-BODY .* own_last=-?[0-9]+' '.*[^_]own_last=')"
	REPORT["$label.own_sims"]="$(field "$CLIENT_LOG" 'own_sims=-?[0-9]+' 'own_sims=')"
	REPORT["$label.other_sims"]="$(field "$CLIENT_LOG" 'other_sims=-?[0-9]+' 'other_sims=')"
	REPORT["$label.skipped"]="$(field "$CLIENT_LOG" 'skipped=[0-9]+' 'skipped=')"
	REPORT["$label.admitted"]="$(field "$SERVER_LOG" 'admitted=[0-9.]+' 'admitted=')"
	REPORT["$label.deferred"]="$(field "$SERVER_LOG" 'deferred=[0-9.]+' 'deferred=')"
	REPORT["$label.culled"]="$(field "$SERVER_LOG" 'culled=[0-9.]+' 'culled=')"

	rm -f "$SERVER_LOG" "$CLIENT_LOG"
	SERVER_LOG=""; CLIENT_LOG=""
}

run_shape dedicated dedicated "$BASE_PORT"
run_shape listen listen "$((BASE_PORT + 1))"
# The negative control, third because it is only worth reading once the two real shapes have reported. The
# server withholds each joiner's OWN status channel with the per-peer visibility veto, so the client MUST come
# back FAIL with a flat reading. A pass here would mean the two runs above cannot see a channel that delivered
# nothing, which is the only way this gate can be green and worthless at the same time.
run_shape veto dedicated "$((BASE_PORT + 2))" --veto-own-status

ok=1
fail() { echo "server-shape-probe: $1"; ok=0; }
value() { printf '%s' "${REPORT[$1]:-?}"; }

for shape in dedicated listen; do
	case "$(value "$shape.server_verdict")" in
		PASS) ;;
		*) fail "the $shape SERVER did not PASS (${REPORT[$shape.server_verdict]:-no verdict at all})";;
	esac
	case "$(value "$shape.client_verdict")" in
		PASS) ;;
		*) fail "the $shape CLIENT did not PASS (${REPORT[$shape.client_verdict]:-no verdict at all})";;
	esac
	# The branch check, before any reading is believed. A 0 here means last_known_state() answered
	# Net.current_tick() and the run measured the facade's fail-open, not the wire.
	if [ "$(value "$shape.reports")" != "1" ]; then
		fail "the $shape client's backend cannot answer last_known_state -- every tick it reported is the
       fail-open fallback. Run \`just native-install\` and try again."
	fi
	# The rollback lane's own health, checked here as well as in the scenario so a driver reading old logs
	# still sees it. A client binds its bodies before it knows which seat it is given, and a body registered
	# as non-predicting is EXEMPTED rather than merely deferred -- so the seat it ends up owning can sit out
	# the whole rollback loop while every state-lane reading above stays perfectly healthy.
	if [ "$(value "$shape.own_sims")" = "0" ]; then
		fail "the $shape client never ran a rollback tick for its OWN body -- it is exempt from the loop, so
       that run exercised no owner prediction even though its state-lane readings look healthy"
	fi
	if [ "$(value "$shape.other_sims")" != "0" ] && [ "$(value "$shape.other_sims")" != "?" ]; then
		fail "the $shape client simulated a seat it does not own ($(value "$shape.other_sims") ticks) --
       remote prediction is on, so that run is not the display-only shape it reports"
	fi
done

# The readings, always printed. This is what the issue asks to be recorded, and it is the comparison that
# makes any single run mean anything: three runs, one scenario, one table.
printf '\n%-10s %-5s %-9s %-9s %-10s %-11s %-12s %-9s %-9s %-10s %-11s %-9s\n' \
	run seat own_first own_last own_rises other_last other_rises body_own own_sims other_sims rx_skipped admitted
for shape in dedicated listen veto; do
	printf '%-10s %-5s %-9s %-9s %-10s %-11s %-12s %-9s %-9s %-10s %-11s %-9s\n' \
		"$shape" "$(value "$shape.seat")" "$(value "$shape.own_first")" "$(value "$shape.own_last")" \
		"$(value "$shape.own_rises")" "$(value "$shape.other_last")" "$(value "$shape.other_rises")" \
		"$(value "$shape.body_own")" "$(value "$shape.own_sims")" "$(value "$shape.other_sims")" \
		"$(value "$shape.skipped")" "$(value "$shape.admitted")"
done
printf '\n'

# The asymmetry itself, stated as its own assertion rather than left to be inferred from the table. A run where
# one shape delivered rows and the other did not is the reported symptom, and it must not be reported as a
# plain "the client did not PASS" on one line.
ded_rises="$(value dedicated.own_rises)"
lis_rises="$(value listen.own_rises)"
if [ "$ded_rises" != "?" ] && [ "$lis_rises" != "?" ]; then
	if [ "$ded_rises" -gt 0 ] 2>/dev/null && [ "$lis_rises" -le 0 ] 2>/dev/null; then
		fail "the LISTEN server delivered no rows for the client's own body while the DEDICATED one did"
	elif [ "$lis_rises" -gt 0 ] 2>/dev/null && [ "$ded_rises" -le 0 ] 2>/dev/null; then
		fail "the DEDICATED server delivered no rows for the client's own body while the LISTEN one did"
	fi
fi

# The negative control's verdict. FAIL is the pass condition here, and the reading has to be flat as well as
# failed: a FAIL for any other reason (never seated, no verdict at all) would satisfy the verdict check alone.
if [ "$(value veto.client_verdict)" != "FAIL" ]; then
	fail "the NEGATIVE CONTROL client returned ${REPORT[veto.client_verdict]:-no verdict at all} while its own
       status channel was vetoed. The two runs above cannot distinguish a channel that delivered rows from
       one that delivered none, so their PASS means nothing."
elif [ "$(value veto.own_rises)" != "0" ]; then
	fail "the NEGATIVE CONTROL client's own state channel advanced $(value veto.own_rises) times while it was
       vetoed -- the veto did not take, so this run proves nothing about the two above."
fi

# A cull is a deliberate withholding and a starve is not, so a run that culled anything is not evidence about
# either. The radius the scenario sets is orders of magnitude larger than the world, so this should be 0.00 --
# on the two real shapes. The negative control culls by construction, which is what a veto is.
for shape in dedicated listen; do
	culled="$(value "$shape.culled")"
	case "$culled" in
		0|0.00|?) ;;
		*) fail "the $shape server CULLED $culled blocks/s -- the scenario's radius is meant to cull nothing,
       so the readings above describe a cull rather than the shape";;
	esac
done

if [ "$ok" -eq 1 ]; then
	echo "server-shape-probe PASSED (both server shapes delivered a rising state-lane tick to a joining client,
       and the vetoed control did not)."
	exit 0
fi
echo "server-shape-probe FAILED."
exit 1
