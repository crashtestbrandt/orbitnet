#!/usr/bin/env bash
# OrbitNet netbench GAUNTLET -- multi-machine, multi-OS netcode bench. ONE controller script (this) drives a
# dedicated server on one host and bot clients across N other hosts over SSH, then collects every peer's artifacts
# and evaluates. This is Unreal Gauntlet's architecture at indie scale (SSH is the device transport; fixed
# hostnames are the rendezvous), and Riot BVS's shape at ~1% scale -- deliberately NOT a GitHub-Actions job mesh
# (Actions has no live inter-job networking). Pair it with Tailscale (MagicDNS gives stable hostnames + NAT
# traversal, so cross-site machines on different OSes join one session by name).
#
# Two conditioning modes:
#   * RELAY=0 (default): clients join the server directly -- the REAL WAN between the machines is the network
#     condition (free realism; measure the RTT, don't assume it -- Tailscale DERP fallback can spike it).
#   * RELAY=1: a UDP impairment relay runs on the server host and clients join it -- CONTROLLED, reproducible
#     conditions layered on top (use PROFILE). Prefer this for a gate; RELAY=0 for a realism spot-check.
#
# Required env: SERVER_HOST, CLIENT_HOSTS (space-separated ssh targets -- Tailscale MagicDNS names or IPs).
# Optional env: REMOTE_ROOT (default = local repo path; must be an identical checkout -- SYNC=1 rsyncs it),
#   GODOT_REMOTE (default godot on PATH), PROFILE (congested_wifi), MEASURE_S (25), SEED (1), POLICY (strafe),
#   CLIENTS_PER_HOST (1), SERVER_PORT (47800), RELAY_PORT (47810), RELAY (0), SYNC (1), GAUNTLET_DRYRUN (0),
#   DEMO (arena) -- which demo project every host runs. The repository root is not a Godot project, so every
#   launch names `demos/$DEMO`; all hosts must run the same one or they will not agree on a world.
#
# NOTE: this orchestrator needs real reachable hosts with passwordless SSH + Godot 4.7; it CANNOT be exercised in
# a single-box CI sandbox (that is what tools/netbench/bench.sh is for). Run GAUNTLET_DRYRUN=1 to print the exact
# ssh/rsync/scp commands without touching any host. Verdict = every collected client logs BENCH-RESULT PASS.
set -uo pipefail

: "${SERVER_HOST:?set SERVER_HOST to the ssh target that will run the dedicated server}"
: "${CLIENT_HOSTS:?set CLIENT_HOSTS to a space-separated list of ssh targets for the bot clients}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOCAL_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REMOTE_ROOT="${REMOTE_ROOT:-$LOCAL_ROOT}"
GODOT_REMOTE="${GODOT_REMOTE:-godot}"
PROFILE="${PROFILE:-congested_wifi}"
MEASURE_S="${MEASURE_S:-25}"
SEED="${SEED:-1}"
POLICY="${POLICY:-strafe}"
CLIENTS_PER_HOST="${CLIENTS_PER_HOST:-1}"
SERVER_PORT="${SERVER_PORT:-47800}"
RELAY_PORT="${RELAY_PORT:-47810}"
RELAY="${RELAY:-0}"
DEMO="${DEMO:-arena}"
SYNC="${SYNC:-1}"
DRY="${GAUNTLET_DRYRUN:-0}"
RELAY_SCRIPT="res://addons/orbitnet/bench/relay_main.gd"
REMOTE_ART="/tmp/netbench-run"          # per-peer artifact dir ON each remote host
OUT="$(mktemp -d -t gauntlet.XXXXXX)"   # collected artifacts on the controller

# The join address clients use: the relay port on the server host if RELAY=1, else the server port directly.
if [ "$RELAY" = "1" ]; then JOIN_PORT="$RELAY_PORT"; else JOIN_PORT="$SERVER_PORT"; fi

# run <host> <cmd>  -- ssh (or echo, in dry-run). Backgrounded remote launches use setsid+nohup so they survive
# the ssh session closing; a sweep by cmdline tears them down (never track fragile remote PIDs).
# run <host> <cmd...>: ssh the command as ONE argument (the remote shell parses the single-quoted paths inside).
# In dry-run, print it as "[ssh host] cmd" -- a readable plan, not a copy-paste line (the inner quotes are the
# REMOTE shell's, so re-quoting for the local shell would only obscure it).
run() { local host="$1"; shift; if [ "$DRY" = "1" ]; then echo "  [ssh $host] $*"; else ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" "$*"; fi; }
# launch <host> <cmd...>: background CMD on the remote host. CMD carries its OWN '>log 2>&1' redirect, so launch()
# must NOT add another (bash applies redirects left-to-right, last-wins -- an extra '>>launch.log' would clobber
# CMD's redirect and leave the per-role log empty, breaking readiness/verdict). Only stdin is detached here.
launch() { local host="$1"; shift; run "$host" "cd '$REMOTE_ROOT' && mkdir -p '$REMOTE_ART' && setsid nohup $* </dev/null & echo launched"; }
sweep_host() { run "$1" "pkill -9 -f -- '--headless --path demos/$DEMO' 2>/dev/null || true"; }
collect() { local host="$1"; if [ "$DRY" = "1" ]; then echo "  scp -r $host:$REMOTE_ART/ $OUT/$host/"; else mkdir -p "$OUT/$host"; scp -q -o BatchMode=yes -r "$host:$REMOTE_ART/" "$OUT/$host/" 2>/dev/null || true; fi; }
# `--` before the pattern on the REMOTE grep: the server readiness marker starts with a hyphen, which grep
# would otherwise read as an option.
grep_remote() { run "$1" "grep -aq -- '$2' '$3' 2>/dev/null"; }

banner() { echo "=== netbench GAUNTLET: server=$SERVER_HOST clients=[$CLIENT_HOSTS] x$CLIENTS_PER_HOST profile=$PROFILE relay=$RELAY ${MEASURE_S}s ==="; }

cleanup() {
	echo "-- teardown: sweeping game processes on every host --"
	sweep_host "$SERVER_HOST"
	for h in $CLIENT_HOSTS; do sweep_host "$h"; done
}
trap cleanup EXIT

banner
[ "$DRY" = "1" ] && echo "(DRY RUN -- printing commands, touching no host)"

# 0) Optionally sync the repo to every host so all peers run identical code. rsync excludes build/user artifacts.
ALL_HOSTS="$SERVER_HOST $CLIENT_HOSTS"
if [ "$SYNC" = "1" ]; then
	echo "-- rsync repo -> hosts --"
	for h in $ALL_HOSTS; do
		if [ "$DRY" = "1" ]; then echo "  rsync -az --delete --exclude .git --exclude build --exclude .godot/imported $LOCAL_ROOT/ $h:$REMOTE_ROOT/";
		else rsync -az --delete --exclude '.git' --exclude 'build' --exclude '.godot/imported' "$LOCAL_ROOT/" "$h:$REMOTE_ROOT/" || { echo "rsync to $h failed"; exit 1; }; fi
	done
fi

# 1) Dedicated server (+ relay if RELAY=1) on the server host.
echo "-- launch server on $SERVER_HOST --"
run "$SERVER_HOST" "rm -rf '$REMOTE_ART'; mkdir -p '$REMOTE_ART'"
launch "$SERVER_HOST" "$GODOT_REMOTE --headless --path demos/$DEMO -- --dedicated=$SERVER_PORT >'$REMOTE_ART/server.log' 2>&1"
if [ "$DRY" != "1" ]; then
	i=0; ok=0
	# Every demo prints `<DEMO>-STATE PLAYING` once its session is up. That is the one marker all three share.
	while [ "$i" -lt 40 ]; do grep_remote "$SERVER_HOST" "-STATE PLAYING" "$REMOTE_ART/server.log" && { ok=1; break; }; sleep 1; i=$((i+1)); done
	[ "$ok" = "1" ] || { echo "server never bound on $SERVER_HOST"; run "$SERVER_HOST" "tail -12 '$REMOTE_ART/server.log'"; exit 1; }
fi
if [ "$RELAY" = "1" ]; then
	echo "-- launch relay on $SERVER_HOST ($PROFILE) --"
	launch "$SERVER_HOST" "$GODOT_REMOTE --headless --path demos/$DEMO -s $RELAY_SCRIPT -- --relay-listen=$RELAY_PORT --relay-target=127.0.0.1:$SERVER_PORT --relay-profile=$PROFILE --relay-seed=$SEED --relay-duration=$((MEASURE_S + 120)) >'$REMOTE_ART/relay.log' 2>&1"
	if [ "$DRY" != "1" ]; then
		i=0; while [ "$i" -lt 25 ]; do grep_remote "$SERVER_HOST" "RELAY: bound" "$REMOTE_ART/relay.log" && break; sleep 1; i=$((i+1)); done
	fi
fi

# 2) Bot clients across the client hosts. Each joins the server host by NAME (fixed-hostname rendezvous; ENet
# retries the connect until the server is reachable), records metrics locally, and self-quits after the window.
echo "-- launch clients --"
peer=0
for h in $CLIENT_HOSTS; do
	run "$h" "rm -rf '$REMOTE_ART'; mkdir -p '$REMOTE_ART'"
	for c in $(seq 1 "$CLIENTS_PER_HOST"); do
		peer=$((peer + 1))
		launch "$h" "$GODOT_REMOTE --headless --path demos/$DEMO -- --join=$SERVER_HOST:$JOIN_PORT --bench --bench-bot=$POLICY --bench-seed=$((SEED + peer)) --bench-metrics='$REMOTE_ART/client${c}.csv' --bench-profile=$PROFILE --bench-duration=$MEASURE_S >'$REMOTE_ART/client${c}.log' 2>&1"
	done
done

# 3) Let the run complete (clients self-finish at MEASURE_S), then collect artifacts from every host.
if [ "$DRY" != "1" ]; then echo "-- running for ${MEASURE_S}s + margin --"; sleep $((MEASURE_S + 15)); fi
echo "-- collect artifacts -> $OUT --"
collect "$SERVER_HOST"
for h in $CLIENT_HOSTS; do collect "$h"; done

if [ "$DRY" = "1" ]; then echo "=== dry run complete (no verdict) ==="; exit 0; fi

# 4) Verdict: every collected client log must carry BENCH-RESULT PASS.
fail=0; total=0
echo "--- client verdicts ---"
while IFS= read -r log; do
	total=$((total + 1))
	line=$(grep -a "BENCH-RESULT" "$log" | tail -1)
	rel=${log#"$OUT/"}
	if echo "$line" | grep -q "BENCH-RESULT PASS"; then echo "  $rel: $line"; else echo "  $rel: ${line:-NO RESULT}"; fail=1; fi
done < <(find "$OUT" -name 'client*.log' | sort)
[ "$total" -eq 0 ] && { echo "no client logs collected -- check SSH/host reachability"; fail=1; }

echo "(artifacts: $OUT)"
if [ "$fail" -ne 0 ]; then echo "=== GAUNTLET: FAIL ==="; exit 1; fi
echo "=== GAUNTLET: PASS ($total client(s) across $(echo "$CLIENT_HOSTS" | wc -w | tr -d ' ') host(s)) ==="
