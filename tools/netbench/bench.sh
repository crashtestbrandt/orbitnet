#!/usr/bin/env bash
# OrbitNet netbench -- single-box netcode bench under CONDITIONED network. Launches a dedicated server + one UDP
# impairment relay (addons/orbitnet/bench/relay_main.gd) + N headless BOT clients that join THROUGH the relay, so
# every client's ENet link is delayed/dropped/reordered below the reliability layer (the honest conditioner: ENet
# ships none, and wrapping the peer would sit above retransmit). Each client drives the real input path via a
# BenchPolicy, streams per-tick netcode metrics to a CSV, and self-evaluates a tick-domain gate on finish.
#
#   tools/netbench/bench.sh <CLIENTS> [PROFILE] [SECONDS] [SEED] [POLICY]
#     CLIENTS  number of bot clients to join through the relay (players = CLIENTS; dedicated server has none)
#     PROFILE  a NetProfiles catalog name (default congested_wifi; `clean` for a control run)
#     SECONDS  steady-state measurement window (default 20)
#     SEED     base RNG seed for the relay + bots (default 1) -- makes a run reproducible
#     POLICY   bot behaviour: idle|strafe|orbit|wander|strafe_fire (default strafe)
#
# PASS = every client logs BENCH-RESULT PASS and the relay bound. Exits non-zero on any FAIL / bringup failure.
# Uses the RAW godot binary + a pkill-by-cmdline sweep (killing the godot-quiet.sh wrapper orphans the child, which
# squats the UDP port and poisons later runs -- learned the hard way; see tools/loadtest.sh).
set -uo pipefail

CLIENTS="${1:-4}"
PROFILE="${2:-congested_wifi}"
MEASURE_S="${3:-20}"
SEED="${4:-1}"
POLICY="${5:-strafe}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GODOT="${GODOT:-godot}"
SERVER_PORT="${SERVER_PORT:-47800}"
RELAY_PORT="${RELAY_PORT:-47810}"
RELAY_SCRIPT="res://addons/orbitnet/bench/relay_main.gd"
OUT="$(mktemp -d -t netbench.XXXXXX)"   # X-template: GNU mktemp requires it (BSD/macOS accepts it too)

PIDS=()
sweep() { pkill -9 -f -- "--headless --path $ROOT" 2>/dev/null || true; }
kill_all() { for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null || true; done; sweep; }
trap kill_all EXIT
sweep; sleep 1

wait_marker() { # log timeout-seconds marker
	local i=0
	while [ "$i" -lt "$2" ]; do grep -aq "$3" "$1" 2>/dev/null && return 0; sleep 1; i=$((i+1)); done
	return 1
}

echo "=== netbench: dedicated server + relay('$PROFILE') + $CLIENTS bot client(s) ('$POLICY'), ${MEASURE_S}s, seed $SEED ==="

# 1) Dedicated server (authoritative, no local body) on the real UDP port, minimal arena (no combat spawn).
"$GODOT" --headless --path "$ROOT" -- --dedicated --no-combat-spawn --port="$SERVER_PORT" >"$OUT/server.log" 2>&1 &
PIDS+=($!)
if ! wait_marker "$OUT/server.log" 40 "SMOKE net=server"; then
	echo "netbench: dedicated server never bound:"; tail -12 "$OUT/server.log"; exit 1
fi

# 2) Relay between clients and the server. Clients self-finish MEASURE_S after THEY connect, and staggered/slow
# bringup of many clients pushes the last window well past relay start -- so the relay must outlive the whole run
# generously (bringup + window + margin), not just the window. It is killed at teardown regardless; this bound is
# only a backstop so a stray relay can't linger forever.
RELAY_DUR=$((MEASURE_S + 90))
"$GODOT" --headless --path "$ROOT" -s "$RELAY_SCRIPT" -- \
	--relay-listen="$RELAY_PORT" --relay-target="127.0.0.1:$SERVER_PORT" \
	--relay-profile="$PROFILE" --relay-seed="$SEED" --relay-duration="$RELAY_DUR" >"$OUT/relay.log" 2>&1 &
PIDS+=($!)
if ! wait_marker "$OUT/relay.log" 25 "RELAY: bound"; then
	echo "netbench: relay never bound:"; tail -12 "$OUT/relay.log"; exit 1
fi

# 3) Bot clients, joining THROUGH the relay port, staggered so connect-time spikes don't overlap. Each self-quits
# after --bench-duration, printing its gate verdict first.
for i in $(seq 1 "$CLIENTS"); do
	"$GODOT" --headless --path "$ROOT" -- \
		--join="127.0.0.1:$RELAY_PORT" --bench --bench-bot="$POLICY" --bench-seed="$((SEED + i))" \
		--bench-metrics="$OUT/client$i.csv" --bench-profile="$PROFILE" --bench-duration="$MEASURE_S" \
		>"$OUT/client$i.log" 2>&1 &
	PIDS+=($!)
	wait_marker "$OUT/client$i.log" 40 "SMOKE net=client" || echo "netbench: client $i slow to connect (continuing)"
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
if [ "$fail" -ne 0 ]; then echo "=== netbench: FAIL ==="; exit 1; fi
echo "=== netbench: PASS ($CLIENTS client(s) under '$PROFILE') ==="
