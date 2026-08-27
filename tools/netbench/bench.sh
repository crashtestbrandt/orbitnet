#!/usr/bin/env bash
# OrbitNet netbench -- single-box netcode bench under CONDITIONED network. Launches a dedicated server + one UDP
# impairment relay (addons/orbitnet/bench/relay_main.gd) + N headless BOT clients that join THROUGH the relay, so
# every client's ENet link is delayed/dropped/reordered below the reliability layer (the honest conditioner: ENet
# ships none, and wrapping the peer would sit above retransmit). Each client drives the real input path via a
# BenchPolicy, streams per-tick netcode metrics to a CSV, and self-evaluates a tick-domain gate on finish.
#
#   tools/netbench/bench.sh <CLIENTS> [PROFILE] [SECONDS] [SEED] [POLICY] [DEMO]
#     CLIENTS  number of bot clients to join through the relay (players = CLIENTS; dedicated server has none)
#     PROFILE  a NetProfiles catalog name (default congested_wifi; `clean` for a control run)
#     SECONDS  steady-state measurement window (default 20)
#     SEED     base RNG seed for the relay + bots (default 1) -- makes a run reproducible
#     POLICY   bot behavior: idle|strafe|orbit|wander|strafe_fire (default strafe)
#     DEMO     which demo project to drive: arena|rts|hockey (default arena)
#
# IT DRIVES A DEMO PROJECT, NOT THE REPOSITORY ROOT. The root is not a Godot project -- OrbitNet is configured
# through an [orbitnet] block in project.godot and the three demos disagree about those values on purpose, so
# each is its own project. `demos/arena` is the default because it is the configuration closest to a shooter
# (decoupled at 30 Hz, a 128-tick ring) and the only one that can fill BenchSubject's hit-registration columns.
#
# SEAT COUNT BOUNDS THE FLEET. A demo seats a joining peer or admits it as an observer, and an observer drives
# no body -- so a client past the seat count reports no samples and fails its own gate. arena seats 24 and
# hockey 32; the RTS demo seats 2, so it takes at most two clients.
#
# PASS = every client logs BENCH-RESULT PASS and the relay bound. Exits non-zero on any FAIL / bringup failure.
# Uses the RAW godot binary + a pkill-by-cmdline sweep: killing the godot-quiet.sh wrapper orphans the child,
# which squats the UDP port and poisons every later run.
set -uo pipefail

CLIENTS="${1:-4}"
PROFILE="${2:-congested_wifi}"
MEASURE_S="${3:-20}"
SEED="${4:-1}"
POLICY="${5:-strafe}"
DEMO="${6:-arena}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PROJECT="$ROOT/demos/$DEMO"
GODOT="${GODOT:-godot}"
SERVER_PORT="${SERVER_PORT:-47800}"
RELAY_PORT="${RELAY_PORT:-47810}"
RELAY_SCRIPT="res://addons/orbitnet/bench/relay_main.gd"
# Where the per-client CSVs and logs land. A random temp directory by default; NETBENCH_OUT names a stable one,
# which is what makes a BEFORE and an AFTER run comparable -- the same seed replays the same link, so the two
# CSVs differ only by what changed in the netcode.
OUT="${NETBENCH_OUT:-$(mktemp -d -t netbench.XXXXXX)}"   # X-template: GNU mktemp requires it (BSD too)
mkdir -p "$OUT"
# CLEAR THE ARTIFACTS FIRST. A reused NETBENCH_OUT is the point of the variable, and a previous run with more
# clients leaves `client5.csv` behind -- which the comparison tool pools with this run's, so a fleet that
# shrank would be compared against rows nobody measured. Only the files this script writes are removed.
rm -f "$OUT"/client*.csv "$OUT"/client*.log "$OUT"/client*.log.snap \
	"$OUT"/server.csv "$OUT"/server.log "$OUT"/server.log.snap \
	"$OUT"/relay.log "$OUT"/relay.log.snap "$OUT"/import.log

PIDS=()
sweep() { pkill -9 -f -- "--headless --path $PROJECT" 2>/dev/null || true; }
kill_all() { for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null || true; done; sweep; }
trap kill_all EXIT

if [ ! -f "$PROJECT/project.godot" ]; then
	echo "netbench: '$DEMO' is not a demo project ($PROJECT/project.godot missing). Try arena, rts or hockey." >&2
	exit 1
fi
if [ ! -d "$PROJECT/addons/orbitnet" ]; then
	echo "netbench: $PROJECT/addons/orbitnet is missing -- run \`just sync-addons\` first." >&2
	exit 1
fi

# A PROJECT WHOSE GLOBAL CLASS CACHE IS ABSENT OR STALE RESOLVES `class_name` TO `Variant`, which each demo's
# project.godot promotes from a warning to an ERROR. The run then dies at parse time and the bringup wait below
# reports it as "dedicated server never bound", which names the symptom three steps from the cause.
#
# THE IMPORT IS UNCONDITIONAL. Testing `.godot/` for existence, which is what this did, only covers the fresh
# clone -- and the case that actually reaches CI is the OTHER one: a workspace checked out over a previous run
# (`actions/checkout` with `clean: false`) carries a `.godot/` built from an older tree, so the guard saw a
# directory, skipped the import, and the run died on exactly the parse cascade the guard exists to prevent. A
# class added or renamed since that import is missing from the cache and nothing on this path notices.
# Re-importing an already-imported project rescans and re-imports only what changed, so the cost of doing it
# every run is seconds and the cost of skipping it is the whole run.
CLASS_CACHE="$PROJECT/.godot/global_script_class_cache.cfg"
# ON A COLD PROJECT THE FIRST IMPORT IS PRIMING AND IS DISCARDED. A GDExtension perturbs the build order of a
# cache being built from nothing, so an autoload's own type can transiently resolve as its base `Node` and
# warnings-as-errors then rejects correct code -- the same reason tools/lint-gdscript.sh primes before its
# checked pass. A warm project cannot hit that, so it pays for one import rather than two.
if [ ! -s "$CLASS_CACHE" ]; then
	echo "netbench: $DEMO has never been imported -- priming the class cache (this takes a moment)..."
	"$GODOT" --headless --path "$PROJECT" --import >/dev/null 2>&1 || true
fi
echo "netbench: importing $DEMO (refreshes the global class cache; a warm project is quick)..."
"$GODOT" --headless --path "$PROJECT" --import >"$OUT/import.log" 2>&1 || true
# `global_script_class_cache.cfg` IS THE ARTIFACT THAT MATTERS, not the directory holding it: it is the file
# every `class_name` in the demo is resolved through. A `.godot/` without it is precisely the state that parses
# to Variant, so that is what is asserted.
if [ ! -s "$CLASS_CACHE" ]; then
	echo "netbench: $PROJECT has no global class cache after importing ($CLASS_CACHE)." >&2
	echo "Every class_name would resolve to Variant and the demo promotes that to a parse error." >&2
	tail -20 "$OUT/import.log" >&2 2>/dev/null || true
	exit 1
fi

sweep; sleep 1

wait_marker() { # log timeout-seconds marker
	local i=0
	# `--` before the pattern: the ready marker starts with a hyphen, and grep would read it as an option.
	while [ "$i" -lt "$2" ]; do grep -aq -- "$3" "$1" 2>/dev/null && return 0; sleep 1; i=$((i+1)); done
	return 1
}

# Report a process that never printed its ready marker, and STOP THE RUN.
#
# The errors come before the tail because a fixed tail is the wrong end of a GDScript parse cascade. Godot
# prints two lines per offending expression and then one `Failed to load script` naming the file, so a
# twelve-line tail of a cascade shows the last few expressions and drops every earlier one -- including, when
# several scripts fail, the first file to fail, which is the one to read. Grepping the WHOLE log puts the
# beginning of the cascade back in the output. The tail stays after it for the failures that print no error at
# all: a port already bound, a missing binary, a process killed before it wrote anything.
bringup_failed() { # label log
	echo "netbench: $1 never bound:"
	local errors
	# -A1 carries the `at: GDScript::reload (res://...)` line that names the script; without it every error
	# reads as a type complaint with no file attached.
	errors="$(grep -aE -A1 'SCRIPT ERROR|Parse Error|^ERROR:|^USER ERROR:' "$2" 2>/dev/null | head -40 || true)"
	if [ -n "$errors" ]; then
		echo "--- errors logged by $1 (first 40 lines; the FIRST script named is the one to read) ---"
		printf '%s\n' "$errors"
	fi
	echo "--- last 12 lines of $2 ---"
	tail -12 "$2"
	exit 1
}

echo "=== netbench: $DEMO -- dedicated server + relay('$PROFILE') + $CLIENTS bot client(s) ('$POLICY'), ${MEASURE_S}s, seed $SEED ==="

# Every demo prints `<DEMO>-STATE PLAYING` once its session is up, on both server and client. That is the one
# marker all three share, and it is what this script waits on rather than anything demo-specific.
READY_MARKER="-STATE PLAYING"

# 1) Dedicated server: authoritative, headless, no local body. A --quit-after backstop so a stray server cannot
#    squat the UDP port past the run; teardown kills it long before that.
#
#    ORBITNET_DEBUG=1 IS THE SERVER-SIDE MEASUREMENT. Every client CSV column that describes the SEND path --
#    bytes admitted, blocks culled, the interest pass -- reads zero on a client, because a client is not the
#    authority and does not run any of it. The backend prints a per-second wire line under this variable, and
#    the verdict step below folds it into server.csv so a change to the send path can be compared at all.
ORBITNET_DEBUG=1 "$GODOT" --headless --path "$PROJECT" -- --dedicated="$SERVER_PORT" \
	--quit-after=$((MEASURE_S + 150)) >"$OUT/server.log" 2>&1 &
PIDS+=($!)
if ! wait_marker "$OUT/server.log" 40 "$READY_MARKER"; then
	bringup_failed "dedicated server" "$OUT/server.log"
fi

# 2) Relay between clients and the server. Clients self-finish MEASURE_S after THEY connect, and staggered/slow
# bringup of many clients pushes the last window well past relay start -- so the relay must outlive the whole run
# generously (bringup + window + margin), not just the window. It is killed at teardown regardless; this bound is
# only a backstop so a stray relay can't linger forever.
RELAY_DUR=$((MEASURE_S + 90))
"$GODOT" --headless --path "$PROJECT" -s "$RELAY_SCRIPT" -- \
	--relay-listen="$RELAY_PORT" --relay-target="127.0.0.1:$SERVER_PORT" \
	--relay-profile="$PROFILE" --relay-seed="$SEED" --relay-duration="$RELAY_DUR" >"$OUT/relay.log" 2>&1 &
PIDS+=($!)
if ! wait_marker "$OUT/relay.log" 25 "RELAY: bound"; then
	bringup_failed "relay" "$OUT/relay.log"
fi

# 3) Bot clients, joining THROUGH the relay port, staggered so connect-time spikes don't overlap. Each self-quits
# after --bench-duration, printing its gate verdict first.
for i in $(seq 1 "$CLIENTS"); do
	"$GODOT" --headless --path "$PROJECT" -- \
		--join="127.0.0.1:$RELAY_PORT" --bench --bench-bot="$POLICY" --bench-seed="$((SEED + i))" \
		--bench-metrics="$OUT/client$i.csv" --bench-profile="$PROFILE" --bench-duration="$MEASURE_S" \
		>"$OUT/client$i.log" 2>&1 &
	PIDS+=($!)
	wait_marker "$OUT/client$i.log" 40 "$READY_MARKER" || echo "netbench: client $i slow to connect (continuing)"
done

echo "netbench: all connected through the relay; measuring for ${MEASURE_S}s (clients self-report)..."
# Wait for every client to print its BENCH-RESULT (self-finish), with a margin over the window.
deadline=$((MEASURE_S + 25)); waited=0
while [ "$waited" -lt "$deadline" ]; do
	# Count client logs NOT yet carrying a BENCH-RESULT; break once every client has self-finished.
	pending=$(grep -aL "BENCH-RESULT" "$OUT"/client*.log 2>/dev/null | wc -l | tr -d ' ')
	[ "$pending" = "0" ] && break
	sleep 1; waited=$((waited + 1))
done

# Snapshot logs BEFORE teardown (kill churns the tail).
for f in "$OUT"/*.log; do cp "$f" "$f.snap" 2>/dev/null || true; done
kill_all; trap - EXIT

# --- verdict ---------------------------------------------------------------------------------------
# Fold the server's per-second wire lines into a CSV beside the clients'. One row per second of the run.
python3 - "$OUT/server.log.snap" "$OUT/server.csv" <<'PYEOF' || true
import re, sys
src, dst = sys.argv[1], sys.argv[2]
pattern = re.compile(
    r"tick=(\d+) mode=(\d+) peers=(\d+) ents=(\d+)r/(\d+)s sent=(\d+) blk (\d+) B "
    r"rx applied=(\d+) rejected=(\d+) skipped=(\d+)")
rows = []
try:
    for line in open(src, errors="replace"):
        m = pattern.search(line)
        if m:
            rows.append(m.groups())
except OSError:
    rows = []
with open(dst, "w") as out:
    # `blocks_s` is ENTITY BLOCKS admitted across every peer, not datagrams: the backend counts one per
    # entity row it put in a frame. `tx_bytes_s` is the snapshot frames' payload bytes over the same second.
    out.write("second,tick,mode,peers,ents_rollback,ents_state,blocks_s,tx_bytes_s,"
              "rx_applied_s,rx_rejected_s,rx_skipped_s\n")
    for i, g in enumerate(rows):
        out.write("%d,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n" % ((i, g[0], g[1], g[2], g[3], g[4],
                                                            g[5], g[6], g[7], g[8], g[9])))
print("netbench: server.csv <- %d per-second wire lines" % len(rows))
PYEOF

echo "--- relay ---"
grep -aE "RELAY-RESULT|RELAY: sessions" "$OUT/relay.log.snap" | tail -3 | sed 's/^/  /' || true

fail=0
echo "--- clients ($PROFILE) ---"
for i in $(seq 1 "$CLIENTS"); do
	snap="$OUT/client$i.log.snap"
	[ -f "$snap" ] || { echo "  client$i: NO LOG"; fail=1; continue; }
	line=$(grep -a "BENCH-RESULT" "$snap" | tail -1)
	if [ -z "$line" ]; then echo "  client$i: NO RESULT (never finished)"; fail=1; continue; fi
	echo "  client$i: $line"
	echo "$line" | grep -q "BENCH-RESULT PASS" || fail=1
	# Echo the gate reasons for a failing client so the artifact is self-diagnosing.
	echo "$line" | grep -q "BENCH-RESULT PASS" || grep -a "BENCH-GATE FAIL" "$snap" | sed 's/^/      /'
done

echo "(artifacts: $OUT  -- per-client CSVs + logs)"
if [ "$fail" -ne 0 ]; then echo "=== netbench: FAIL ($DEMO) ==="; exit 1; fi
echo "=== netbench: PASS ($DEMO, $CLIENTS client(s) under '$PROFILE') ==="
