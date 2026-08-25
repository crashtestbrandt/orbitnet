#!/usr/bin/env bash
# Two-process networked gate for the RTS demo. This is the PR gate.
#
# Launches a --host and a --join client over ENet localhost, each running tools/instr/rts_probe.gd, then
# compares what the two peers reported. Every assertion is either tick-domain or scale-free, so the gate
# behaves the same on a fast desktop and a loaded CI runner:
#
#   1. Both peers reach a session and print a verdict at all.
#   2. Their WORLD SIGNATURES match -- the direct gate on deterministic node naming, and therefore on
#      entity-id agreement. This is the assertion that catches the silent failure: with mismatched ids,
#      nothing errors and nothing moves.
#   3. Each peer saw the order's sequence number reach every unit it named.
#   4. The two peers agree about where seat 0's army is, by CENTROID, within a couple of metres.
#      Deliberately not a per-unit hash: peers observe different ticks by construction.
#   5. A forged foreign-seat order changed nothing, AND the client was told it was refused.
#   6. The worst refresh interval of an actively-moving unit stayed under bound -- i.e. no unit is starving
#      past the send budget.
#
# NO --fixed-fps. The backend's clock sync paces off the WALL clock; --fixed-fps stalls the ping/pong so the
# client never finishes its handshake and the probe times out looking like a netcode failure. Free-running
# also drifts the render:physics beat, which is what a real session does.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="$ROOT/demos/rts"
GODOT="${GODOT:-$ROOT/tools/godot-quiet.sh}"
WATCHDOG_S="${RTS_PROBE_WATCHDOG_S:-70}"
RUN_S="${RTS_PROBE_RUN_S:-18}"
# Metres of disagreement tolerated between the two peers' army centroids.
CENTROID_TOLERANCE="${RTS_PROBE_CENTROID_M:-2.0}"

HOST_LOG="$(mktemp "${TMPDIR:-/tmp}/rtsprobe-host.XXXXXX")"
CLIENT_LOG="$(mktemp "${TMPDIR:-/tmp}/rtsprobe-client.XXXXXX")"
HOST_PID=""
CLIENT_PID=""
WATCH_PID=""

cleanup() {
	[ -n "$HOST_PID" ] && kill -9 "$HOST_PID" 2>/dev/null
	[ -n "$CLIENT_PID" ] && kill -9 "$CLIENT_PID" 2>/dev/null
	[ -n "$WATCH_PID" ] && kill -9 "$WATCH_PID" 2>/dev/null
	rm -f "$HOST_LOG" "$CLIENT_LOG"
	return 0
}
trap cleanup EXIT

if [ ! -d "$PROJECT/addons/orbitnet" ]; then
	printf 'rts-probe FAILED: demos/rts/addons/orbitnet is missing -- run `just sync-addons` first.\n' >&2
	exit 1
fi

echo "rts-probe: starting host..."
"$GODOT" --headless --path "$PROJECT" -- --host --rts-probe --quit-after="$RUN_S" >"$HOST_LOG" 2>&1 &
HOST_PID=$!

sleep 3   # let the host bind and start listening before the client dials

echo "rts-probe: starting client (join 127.0.0.1)..."
"$GODOT" --headless --path "$PROJECT" -- --join=127.0.0.1 --rts-probe --quit-after="$RUN_S" >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

# Watchdog: a hung session must fail loudly rather than hang CI forever.
( sleep "$WATCHDOG_S"; kill -9 "$HOST_PID" "$CLIENT_PID" 2>/dev/null ) &
WATCH_PID=$!

wait "$CLIENT_PID" 2>/dev/null; CLIENT_RC=$?
wait "$HOST_PID" 2>/dev/null; HOST_RC=$?

echo "=== HOST (rc=$HOST_RC) ==="
grep -aE "RTS-BOOT|RTS-STATE|RTS-SEAT|RTS-PROBE|RTS world" "$HOST_LOG" || echo "(no probe output)"
echo "=== CLIENT (rc=$CLIENT_RC) ==="
grep -aE "RTS-BOOT|RTS-STATE|RTS-SEAT|RTS-PROBE|RTS world" "$CLIENT_LOG" || echo "(no probe output)"

field() { grep -aoE "$2" "$1" | tail -1 | sed -E "s/$3//"; }

host_verdict="$(grep -aoE 'RTS-PROBE-RESULT role=[a-z]+ (PASS|FAIL)' "$HOST_LOG" | tail -1)"
client_verdict="$(grep -aoE 'RTS-PROBE-RESULT role=[a-z]+ (PASS|FAIL)' "$CLIENT_LOG" | tail -1)"
host_sig="$(field "$HOST_LOG" 'RTS-PROBE sig=-?[0-9]+' 'RTS-PROBE sig=')"
client_sig="$(field "$CLIENT_LOG" 'RTS-PROBE sig=-?[0-9]+' 'RTS-PROBE sig=')"
host_centroid="$(grep -aoE 'RTS-PROBE centroid=-?[0-9.]+ -?[0-9.]+' "$HOST_LOG" | tail -1 | sed 's/RTS-PROBE centroid=//')"
client_centroid="$(grep -aoE 'RTS-PROBE centroid=-?[0-9.]+ -?[0-9.]+' "$CLIENT_LOG" | tail -1 | sed 's/RTS-PROBE centroid=//')"
client_forged="$(field "$CLIENT_LOG" 'RTS-PROBE forged_rejected=[01]' 'RTS-PROBE forged_rejected=')"
client_forged_code="$(field "$CLIENT_LOG" 'RTS-PROBE forged_refusal_code=-*[0-9]*' 'RTS-PROBE forged_refusal_code=')"

ok=1
fail() { echo "rts-probe: $1"; ok=0; }

case "$host_verdict" in *PASS) ;; *) fail "HOST did not PASS (${host_verdict:-no verdict at all})";; esac
case "$client_verdict" in *PASS) ;; *) fail "CLIENT did not PASS (${client_verdict:-no verdict at all})";; esac

# 2. The signature comparison. If this fails, the two peers built different worlds and every other
#    assertion below is meaningless -- so say that explicitly rather than reporting five failures.
if [ -z "$host_sig" ] || [ -z "$client_sig" ]; then
	fail "one or both peers never printed a world signature"
elif [ "$host_sig" != "$client_sig" ]; then
	fail "WORLD SIGNATURES DIFFER (host=$host_sig client=$client_sig) -- the peers built different node
       paths, so their entity ids disagree and replication is going nowhere. Look for a node added
       without an explicit name (Godot's auto-names are allocation-order dependent)."
else
	echo "rts-probe: world signatures match ($host_sig)"
fi

# 4. Centroid agreement.
if [ -z "$host_centroid" ] || [ -z "$client_centroid" ]; then
	fail "one or both peers never reported an army centroid"
else
	agreement="$(awk -v h="$host_centroid" -v c="$client_centroid" -v tol="$CENTROID_TOLERANCE" '
		BEGIN {
			split(h, a, " "); split(c, b, " ");
			dx = a[1] - b[1]; dz = a[2] - b[2];
			d = sqrt(dx * dx + dz * dz);
			printf "%.3f %s", d, (d <= tol ? "ok" : "far");
		}')"
	distance="${agreement%% *}"
	verdict="${agreement##* }"
	if [ "$verdict" = "ok" ]; then
		echo "rts-probe: peers agree on the army centroid to ${distance} m (tolerance ${CENTROID_TOLERANCE} m)"
	else
		fail "the peers disagree about where the army is by ${distance} m (tolerance ${CENTROID_TOLERANCE} m)"
	fi
fi

# 5. The forgery. Only the CLIENT can perform it (it is the peer that does not hold seat 0).
if [ "$client_forged" != "1" ]; then
	fail "the client's forged foreign-seat order was NOT rejected"
else
	echo "rts-probe: a forged foreign-seat order was refused"
fi

# The client must be TOLD, not merely ignored. Nothing happening is what a refusal and a dropped packet look
# like from the client's side, so the reply carrying the server's reason code is what separates them. 9 is
# OrderValidator.Code.FOREIGN_CHANNEL.
if [ "$client_forged_code" != "9" ]; then
	fail "the client was not told why its forged order was refused (code '$client_forged_code', wanted 9)"
else
	echo "rts-probe: and the client was told why (code 9, the wrong channel)"
fi

if [ "$ok" -eq 1 ]; then
	echo "rts-probe PASSED (two peers, identical world, orders replicated, forgery refused)."
	exit 0
fi
echo "rts-probe FAILED."
exit 1
