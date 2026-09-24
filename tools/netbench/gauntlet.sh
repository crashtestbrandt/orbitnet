#!/usr/bin/env bash
# OrbitNet netbench GAUNTLET -- multi-machine, multi-OS netcode bench. ONE controller script (this) drives a
# dedicated server on one host and bot clients across N other hosts over SSH, then collects every peer's artifacts
# and evaluates. This is Unreal Gauntlet's architecture at indie scale (SSH is the device transport; fixed
# hostnames are the rendezvous), and Riot BVS's shape at ~1% scale -- deliberately NOT a GitHub-Actions job mesh
# (Actions has no live inter-job networking). Pair it with Tailscale (MagicDNS gives stable hostnames + NAT
# traversal, so cross-site machines on different OSes join one session by name).
#
# WHY IT EXISTS AT ALL: tools/netbench/bench.sh conditions a LOOPBACK socket, which cannot produce a real link's
# bandwidth cap, a NAT, or the shape of a relayed transport. This is the only bench that measures those. See
# docs/netbench.md, "Which bench supports which claim".
#
#   tools/netbench/gauntlet.sh [--dry-run | --preflight]
#     --dry-run    print the exact ssh/rsync/scp plan and the resolved arguments; touch NO host
#     --preflight  contact every host and validate reachability, Godot, the checkout, the native binary and a
#                  stale process from a previous run; launch nothing
#     (no flag)    preflight, then the full run
#
# Two conditioning modes:
#   * RELAY=0 (default): clients join the server directly -- the REAL WAN between the machines is the network
#     condition (free realism; measure the RTT, don't assume it -- Tailscale DERP fallback can spike it).
#   * RELAY=1: a UDP impairment relay runs on the server host and clients join it -- CONTROLLED, reproducible
#     conditions layered on top (use PROFILE). Prefer this for a gate; RELAY=0 for a realism spot-check.
#
# PROFILE MEANS TWO DIFFERENT THINGS, and getting it wrong is how this run produces a verdict about nothing:
#   * RELAY=1 -- the impairment the relay INJECTS, and the reference the client's RTT gate checks against.
#   * RELAY=0 -- nothing is injected, so it is only the gate's reference: the operator's DECLARED expectation of
#     what the real link looks like. There is no safe default for that, so RELAY=0 REQUIRES an explicit PROFILE
#     (`PROFILE=clean` for a LAN, `broadband`/`cross_region`/`relayed` for a WAN). Left on the RELAY=1 default it
#     would gate a real 60ms link against an injected 100ms one and call the mismatch a netcode failure.
#
# Required env: SERVER_HOST, CLIENT_HOSTS (space-separated ssh targets -- Tailscale MagicDNS names or IPs).
# Optional env: REMOTE_ROOT (default = local repo path; must be an identical checkout -- SYNC=1 rsyncs it),
#   GODOT_REMOTE (default godot on PATH), PROFILE (congested_wifi when RELAY=1; required when RELAY=0),
#   MEASURE_S (25), SEED (1), POLICY (strafe), CLIENTS_PER_HOST (1), SERVER_PORT (47800), RELAY_PORT (47810),
#   RELAY (0), SYNC (1), GAUNTLET_DRYRUN (0), SKIP_PREFLIGHT (0),
#   DEMO (arena) -- which demo project every host runs. The repository root is not a Godot project, so every
#   launch names `demos/$DEMO`; all hosts must run the same one or they will not agree on a world.
#
# NOTE: a real run needs reachable hosts with passwordless SSH + Godot 4; it CANNOT be exercised in a single-box
# CI sandbox (that is what tools/netbench/bench.sh is for). `--dry-run` and `--preflight` exist so everything up
# to the link itself is exercisable without a second machine. Verdict = every EXPECTED client logs
# BENCH-RESULT PASS.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOCAL_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DRY="${GAUNTLET_DRYRUN:-0}"
PREFLIGHT_ONLY=0
usage() {
	cat <<'USAGE'
tools/netbench/gauntlet.sh [--dry-run | --preflight]
  --dry-run    print the exact ssh/rsync/scp plan and the resolved arguments; touch NO host
  --preflight  contact every host and validate reachability, Godot, the checkout, the native binary and a
               stale process from a previous run; launch nothing
  (no flag)    preflight, then the full run
Required env: SERVER_HOST, CLIENT_HOSTS. See the header of this file for the rest.
USAGE
}
while [ "$#" -gt 0 ]; do
	case "$1" in
		--dry-run) DRY=1 ;;
		--preflight) PREFLIGHT_ONLY=1 ;;
		-h|--help) usage; exit 0 ;;
		*) echo "gauntlet: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
	esac
	shift
done

: "${SERVER_HOST:?set SERVER_HOST to the ssh target that will run the dedicated server}"
: "${CLIENT_HOSTS:?set CLIENT_HOSTS to a space-separated list of ssh targets for the bot clients}"

REMOTE_ROOT="${REMOTE_ROOT:-$LOCAL_ROOT}"
GODOT_REMOTE="${GODOT_REMOTE:-godot}"
MEASURE_S="${MEASURE_S:-25}"
SEED="${SEED:-1}"
POLICY="${POLICY:-strafe}"
CLIENTS_PER_HOST="${CLIENTS_PER_HOST:-1}"
SERVER_PORT="${SERVER_PORT:-47800}"
RELAY_PORT="${RELAY_PORT:-47810}"
RELAY="${RELAY:-0}"
DEMO="${DEMO:-arena}"
SYNC="${SYNC:-1}"
SKIP_PREFLIGHT="${SKIP_PREFLIGHT:-0}"
PROFILE_SET=1; [ -z "${PROFILE:-}" ] && PROFILE_SET=0
PROFILE="${PROFILE:-congested_wifi}"
RELAY_SCRIPT="res://addons/orbitnet/bench/relay_main.gd"
REMOTE_ART="/tmp/netbench-run"          # per-peer artifact dir ON each remote host
OUT="$(mktemp -d -t gauntlet.XXXXXX)"   # collected artifacts on the controller
ALL_HOSTS="$SERVER_HOST $CLIENT_HOSTS"
CLIENT_HOST_N="$(echo "$CLIENT_HOSTS" | wc -w | tr -d ' ')"
EXPECTED_CLIENTS=0   # the real value needs CLIENTS_PER_HOST validated first; validate_args sets it.

# The join address clients use: the relay port on the server host if RELAY=1, else the server port directly.
if [ "$RELAY" = "1" ]; then JOIN_PORT="$RELAY_PORT"; else JOIN_PORT="$SERVER_PORT"; fi

die() { echo "gauntlet: $*" >&2; exit 1; }

# run <host> <cmd>  -- ssh (or echo, in dry-run). Backgrounded remote launches survive the ssh session closing;
# a sweep by cmdline tears them down (never track fragile remote PIDs).
# run <host> <cmd...>: ssh the command as ONE argument (the remote shell parses the single-quoted paths inside).
# In dry-run, print it as "[ssh host] cmd" -- a readable plan, not a copy-paste line (the inner quotes are the
# REMOTE shell's, so re-quoting for the local shell would only obscure it).
run() { local host="$1"; shift; if [ "$DRY" = "1" ]; then echo "  [ssh $host] $*"; else ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" "$*"; fi; }
# launch <host> <cmd...>: background CMD on the remote host. CMD carries its OWN '>log 2>&1' redirect, so launch()
# must NOT add another (bash applies redirects left-to-right, last-wins -- an extra '>>launch.log' would clobber
# CMD's redirect and leave the per-role log empty, breaking readiness/verdict). Only stdin is detached here.
# SETSID IS PROBED ON THE REMOTE HOST, NOT ASSUMED. It is a util-linux tool: Linux has it, macOS does not, and
# this bench exists to run cross-OS, so a hard `setsid` would make every Darwin host fail to launch anything --
# and fail SILENTLY, because the `&` keeps the shell's exit status and `echo launched` still prints. `nohup`
# alone is enough for survival (it ignores SIGHUP, and an ssh command has no controlling terminal to be a
# session leader of); setsid is the belt-and-braces half and is used only where it exists. The probe is composed
# on the REMOTE side -- `\$ss` is escaped so the remote shell expands it, not the controller's.
launch() { local host="$1"; shift; run "$host" "ss=; command -v setsid >/dev/null 2>&1 && ss=setsid; cd '$REMOTE_ROOT' && mkdir -p '$REMOTE_ART' && \$ss nohup $* </dev/null & echo launched"; }
sweep_host() { run "$1" "pkill -9 -f -- '--headless --path demos/$DEMO' 2>/dev/null || true"; }
collect() {
	local host="$1"
	if [ "$DRY" = "1" ]; then echo "  scp -r $host:$REMOTE_ART/ $OUT/$host/"; return 0; fi
	mkdir -p "$OUT/$host"
	scp -q -o BatchMode=yes -r "$host:$REMOTE_ART/" "$OUT/$host/" 2>/dev/null \
		|| echo "  (scp from $host returned non-zero -- artifacts from that host may be missing)"
}
# `--` before the pattern on the REMOTE grep: the server readiness marker starts with a hyphen, which grep
# would otherwise read as an option.
grep_remote() { run "$1" "grep -aq -- '$2' '$3' 2>/dev/null"; }

banner() { echo "=== netbench GAUNTLET: server=$SERVER_HOST clients=[$CLIENT_HOSTS] x$CLIENTS_PER_HOST profile=$PROFILE relay=$RELAY ${MEASURE_S}s ==="; }

cleanup() {
	echo "-- teardown: sweeping game processes on every host --"
	sweep_host "$SERVER_HOST"
	for h in $CLIENT_HOSTS; do sweep_host "$h"; done
}

# --- argument validation -------------------------------------------------------------------------
# EVERY MODE RUNS THIS, including a real run: an argument that is wrong is wrong before any host is touched, and
# the failures it catches otherwise surface as "server never bound" twenty seconds and three hosts later.
is_uint() { case "$1" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }

# The profile catalog is GDScript, and the controller is not required to have Godot installed (it only drives
# ssh). Every catalog entry is constructed through `_make("<name>", ...)`, so the names are greppable from the
# one source of truth without an engine -- and relay_main.gd re-validates the name on the host anyway.
profile_names() { sed -n 's/.*_make("\([a-z0-9_]*\)".*/\1/p' "$LOCAL_ROOT/addons/orbitnet/bench/net_profiles.gd"; }

validate_args() {
	local bad=0 dupes=
	[ -d "$LOCAL_ROOT/demos/$DEMO" ] || { echo "gauntlet: DEMO='$DEMO' is not a demo project (demos/$DEMO missing). Try arena, rts or hockey." >&2; bad=1; }
	if ! profile_names | grep -qx -- "$PROFILE"; then
		echo "gauntlet: PROFILE='$PROFILE' is not in the catalog. Known: $(profile_names | sort | tr '\n' ' ')" >&2
		bad=1
	fi
	# RELAY=0 injects nothing, so PROFILE is only the RTT gate's reference -- see the header. Defaulting it would
	# gate a real link against an impairment no relay applied.
	if [ "$RELAY" = "0" ] && [ "$PROFILE_SET" = "0" ]; then
		echo "gauntlet: RELAY=0 measures the REAL link, so PROFILE is the gate's reference, not an injected condition." >&2
		echo "          Set it to the link you expect: PROFILE=clean (LAN), broadband / cross_region / relayed (WAN)." >&2
		echo "          Or set RELAY=1 to inject '$PROFILE' through the impairment relay instead." >&2
		bad=1
	fi
	case "$RELAY" in 0|1) ;; *) echo "gauntlet: RELAY must be 0 or 1 (got '$RELAY')" >&2; bad=1 ;; esac
	case "$SYNC" in 0|1) ;; *) echo "gauntlet: SYNC must be 0 or 1 (got '$SYNC')" >&2; bad=1 ;; esac
	for pair in "MEASURE_S:$MEASURE_S" "SEED:$SEED" "SERVER_PORT:$SERVER_PORT" "RELAY_PORT:$RELAY_PORT"; do
		is_uint "${pair#*:}" || { echo "gauntlet: ${pair%%:*} must be a non-negative integer (got '${pair#*:}')" >&2; bad=1; }
	done
	if is_uint "$CLIENTS_PER_HOST" && [ "$CLIENTS_PER_HOST" -ge 1 ]; then
		EXPECTED_CLIENTS=$(( CLIENT_HOST_N * CLIENTS_PER_HOST ))
	else
		echo "gauntlet: CLIENTS_PER_HOST must be a positive integer (got '$CLIENTS_PER_HOST')" >&2; bad=1
	fi
	if is_uint "$MEASURE_S" && [ "$MEASURE_S" -lt 5 ]; then
		echo "gauntlet: MEASURE_S must be at least 5 (the gate fails a run with fewer than 30 samples)" >&2; bad=1
	fi
	[ "$SERVER_PORT" = "$RELAY_PORT" ] && { echo "gauntlet: SERVER_PORT and RELAY_PORT must differ (both '$SERVER_PORT')" >&2; bad=1; }
	# The client hosts wipe $REMOTE_ART before launching. A host that is BOTH the server and a client would wipe
	# the running server's log out from under the readiness grep and the verdict.
	for h in $CLIENT_HOSTS; do
		[ "$h" = "$SERVER_HOST" ] && { echo "gauntlet: '$h' is both SERVER_HOST and a client host; the client bringup would wipe the server's artifacts." >&2; bad=1; }
	done
	# The same host listed twice hits that identical wipe: the second pass deletes $REMOTE_ART under the clients
	# the first pass just launched, both passes write the same client<N>.log names, and the run ends "a host did
	# not report" with nothing pointing at the cause. CLIENTS_PER_HOST is how several clients go on one host.
	dupes="$(printf '%s\n' $CLIENT_HOSTS | sort | uniq -d | tr '\n' ' ')"
	if [ -n "${dupes% }" ]; then
		echo "gauntlet: CLIENT_HOSTS lists a host more than once (${dupes% }); the second pass would wipe the first's artifacts." >&2
		echo "          Use CLIENTS_PER_HOST=<n> to run several clients on one host." >&2
		bad=1
	fi
	# SYNC=1 rsyncs THIS checkout, and `demos/*/addons/` is gitignored -- the copy exists only after
	# tools/sync-addons.sh has run here. Without it every host receives a project with no addon and fails
	# identically, which is the remote preflight's addon check made useless by a local omission.
	if [ "$SYNC" = "1" ] && [ ! -d "$LOCAL_ROOT/demos/$DEMO/addons/orbitnet" ]; then
		echo "gauntlet: demos/$DEMO/addons/orbitnet is missing HERE, so SYNC=1 would rsync a project with no addon. Run tools/sync-addons.sh." >&2
		bad=1
	fi
	[ "$CLIENT_HOST_N" -ge 1 ] || { echo "gauntlet: CLIENT_HOSTS names no hosts" >&2; bad=1; }
	[ "$bad" = "0" ] || exit 2
	echo "-- arguments OK: demo=$DEMO profile=$PROFILE relay=$RELAY join=$SERVER_HOST:$JOIN_PORT clients=$EXPECTED_CLIENTS window=${MEASURE_S}s seed=$SEED policy=$POLICY --"
}

# --- remote preflight ----------------------------------------------------------------------------
# One ssh round trip per host, before anything is launched. It answers the questions a first run otherwise
# discovers one twenty-second timeout at a time: is the host reachable without a password, is Godot on PATH and
# is it Godot 4, does the checkout exist, is the demo's addon copy there, is there a native binary for THIS
# host's platform, and is a previous run still squatting the port.
preflight_host() { # host role
	local host="$1" role="$2" out=""
	# ONE remote script, not six ssh calls: an ssh round trip to a cross-site host costs more than everything it
	# checks. It prints `key=value` lines and never exits non-zero, so a missing tool reports as a value.
	out="$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" "
		printf 'uname=%s\n' \"\$(uname -s 2>/dev/null || echo unknown)\"
		if command -v '$GODOT_REMOTE' >/dev/null 2>&1 || [ -x '$GODOT_REMOTE' ]; then
			printf 'godot=%s\n' \"\$('$GODOT_REMOTE' --version 2>/dev/null | head -1)\"
		else printf 'godot=MISSING\n'; fi
		if [ -d '$REMOTE_ROOT' ]; then printf 'root=ok\n'; else printf 'root=MISSING\n'; fi
		if [ -f '$REMOTE_ROOT/demos/$DEMO/project.godot' ]; then printf 'project=ok\n'; else printf 'project=MISSING\n'; fi
		if [ -d '$REMOTE_ROOT/demos/$DEMO/addons/orbitnet' ]; then printf 'addon=ok\n'; else printf 'addon=MISSING\n'; fi
		case \"\$(uname -s)\" in Linux) plat=linux ;; Darwin) plat=macos ;; MINGW*|MSYS*|CYGWIN*|Windows_NT) plat=windows ;; *) plat=unknown ;; esac
		if ls '$REMOTE_ROOT/demos/$DEMO/addons/orbitnet_native/bin/'*\".\$plat.\"* >/dev/null 2>&1; then printf 'native=ok\n'; else printf 'native=MISSING\n'; fi
		printf 'stale=%s\n' \"\$(pgrep -f -- '--headless --path demos/$DEMO' 2>/dev/null | wc -l | tr -d ' ')\"
	" 2>&1)"
	if [ -z "$out" ] || ! printf '%s' "$out" | grep -q '^uname='; then
		echo "  $host ($role): UNREACHABLE over ssh -- $(printf '%s' "$out" | head -1)" >&2
		echo "    passwordless ssh is required (BatchMode=yes). Try: ssh -o BatchMode=yes $host true" >&2
		return 1
	fi
	local uname_s godot root project addon native stale rc=0
	uname_s="$(printf '%s\n' "$out" | sed -n 's/^uname=//p')"
	godot="$(printf '%s\n' "$out" | sed -n 's/^godot=//p')"
	root="$(printf '%s\n' "$out" | sed -n 's/^root=//p')"
	project="$(printf '%s\n' "$out" | sed -n 's/^project=//p')"
	addon="$(printf '%s\n' "$out" | sed -n 's/^addon=//p')"
	native="$(printf '%s\n' "$out" | sed -n 's/^native=//p')"
	stale="$(printf '%s\n' "$out" | sed -n 's/^stale=//p')"
	echo "  $host ($role): $uname_s, godot='$godot'"
	[ "$godot" = "MISSING" ] && { echo "    GODOT_REMOTE='$GODOT_REMOTE' is not executable on this host" >&2; rc=1; }
	case "$godot" in 4.*) ;; MISSING) ;; *) echo "    expected Godot 4.x, got '$godot'" >&2; rc=1 ;; esac
	# With SYNC=1 the tree is about to be rsynced, so a missing checkout is a plan rather than a fault. With
	# SYNC=0 it is the fault.
	if [ "$SYNC" = "0" ]; then
		[ "$root" = "ok" ] || { echo "    REMOTE_ROOT='$REMOTE_ROOT' does not exist and SYNC=0" >&2; rc=1; }
		[ "$project" = "ok" ] || { echo "    demos/$DEMO/project.godot missing and SYNC=0" >&2; rc=1; }
		[ "$addon" = "ok" ] || { echo "    demos/$DEMO/addons/orbitnet missing -- run tools/sync-addons.sh there" >&2; rc=1; }
	elif [ "$root" != "ok" ]; then
		echo "    REMOTE_ROOT='$REMOTE_ROOT' will be created by the rsync"
	fi
	# THE NATIVE CHECK IS UNCONDITIONAL, including on a host with no checkout yet. The rsync excludes
	# `orbitnet_native/bin` (see below), so it never delivers a library no matter what is already there: a fresh
	# host gets an empty bin/, Godot fails to load the GDExtension, every `class_name` in the facade resolves to
	# nothing, and the run dies forty seconds later as "server never bound". The fresh host is precisely where
	# `just native-install` is most likely to have been forgotten.
	if [ "$native" != "ok" ]; then
		echo "    no native binary for this platform in demos/$DEMO/addons/orbitnet_native/bin -- run 'just native-install' ON THIS HOST (the rsync never carries one)" >&2
		rc=1
	fi
	[ "${stale:-0}" = "0" ] || echo "    NOTE: $stale stale headless process(es) from a previous run; teardown will sweep them"
	return "$rc"
}

preflight() {
	echo "-- preflight: $(echo "$ALL_HOSTS" | wc -w | tr -d ' ') host(s) --"
	local bad=0
	preflight_host "$SERVER_HOST" "server" || bad=1
	for h in $CLIENT_HOSTS; do preflight_host "$h" "client" || bad=1; done
	[ "$bad" = "0" ] || die "preflight failed -- fix the hosts above, or set SKIP_PREFLIGHT=1 to run anyway"
	echo "-- preflight OK --"
}

# --- the plan ------------------------------------------------------------------------------------
banner
validate_args

# --dry-run contacts nothing, so it validates arguments and prints the plan and stops there. Every other mode
# preflights the hosts first, and --preflight stops once they pass.
if [ "$DRY" = "1" ]; then
	echo "(DRY RUN -- printing commands, touching no host)"
	if [ "$PREFLIGHT_ONLY" = "1" ]; then
		echo "=== --dry-run wins over --preflight: arguments validated, no host contacted ==="
		rmdir "$OUT" 2>/dev/null
		exit 0
	fi
elif [ "$SKIP_PREFLIGHT" = "1" ] && [ "$PREFLIGHT_ONLY" = "0" ]; then
	echo "-- preflight skipped (SKIP_PREFLIGHT=1) --"
else
	preflight
	if [ "$PREFLIGHT_ONLY" = "1" ]; then
		echo "=== preflight complete (nothing was launched) ==="
		rmdir "$OUT" 2>/dev/null
		exit 0
	fi
fi

trap cleanup EXIT

# 0) Optionally sync the repo to every host so all peers run identical code. rsync excludes build/user artifacts.
#
# IT DOES NOT CARRY THE NATIVE BINARIES. `addons/orbitnet_native/bin/` holds ONE platform's library -- the
# controller's -- and this bench exists to run cross-OS. Copying a macOS dylib onto a Linux host fails at dlopen,
# and `--delete` would additionally remove the library that host built for itself. Each host keeps its own
# (`just native-install` there once); preflight fails when a host has none.
if [ "$SYNC" = "1" ]; then
	echo "-- rsync repo -> hosts (GDScript + projects; each host keeps its own native binaries) --"
	for h in $ALL_HOSTS; do
		if [ "$DRY" = "1" ]; then echo "  rsync -az --delete --exclude .git --exclude build --exclude .godot/imported --exclude orbitnet_native/bin $LOCAL_ROOT/ $h:$REMOTE_ROOT/";
		else rsync -az --delete --exclude '.git' --exclude 'build' --exclude '.godot/imported' --exclude 'orbitnet_native/bin' "$LOCAL_ROOT/" "$h:$REMOTE_ROOT/" || die "rsync to $h failed"; fi
	done
fi

# 0b) Import the demo on every host. A project whose .godot/global_script_class_cache.cfg is absent or STALE
# resolves every `class_name` to Variant, which each demo promotes from a warning to a parse error -- and the
# bringup wait below then reports it as "server never bound", the symptom three steps from the cause. The rsync
# above excludes `.godot/imported`, so a freshly synced host is exactly that case. Unconditional and cheap on a
# warm project, per tools/netbench/bench.sh, which carries the same reasoning at length.
#
# THE CHECK IS FATAL, not advisory: the remote script exits non-zero when the cache is still absent and the run
# stops here. Printing IMPORT FAILED and continuing would hand the operator the misleading message anyway, just
# with one extra line of context ninety seconds earlier.
echo "-- import demos/$DEMO on every host (refreshes the global class cache) --"
for h in $ALL_HOSTS; do
	if import_out="$(run "$h" "cd '$REMOTE_ROOT' && '$GODOT_REMOTE' --headless --path demos/$DEMO --import >/tmp/netbench-import.log 2>&1; if [ -s demos/$DEMO/.godot/global_script_class_cache.cfg ]; then echo 'import ok'; else echo 'IMPORT FAILED'; tail -20 /tmp/netbench-import.log; exit 1; fi")"; then
		printf '%s\n' "$import_out" | sed "s/^/  $h: /"
	else
		printf '%s\n' "$import_out" | sed "s/^/  $h: /" >&2
		die "import failed on $h -- demos/$DEMO cannot resolve its class_name scripts there"
	fi
done

# 1) Dedicated server (+ relay if RELAY=1) on the server host.
#
# --quit-after IS A BACKSTOP, NOT THE RUN LENGTH. Teardown kills the server long before it fires; it exists so a
# controller that dies (a dropped ssh, a Ctrl-C past the trap) cannot leave a server squatting the UDP port on a
# remote host with nobody watching, which poisons every later run on that host.
#
# ORBITNET_DEBUG=1 puts the server's per-second send-path wire line into `server.log`. Every send-path column
# reads zero in a CLIENT csv (a client runs none of that code), so this is the only record here of what the
# server sent. Unlike tools/netbench/bench.sh, the gauntlet does NOT fold it into a `server.csv`: the parse step
# there assumes one local run, and a cross-host comparison has no verdict defined for it yet. It is collected
# with the other artifacts and read by a human against a loopback run.
echo "-- launch server on $SERVER_HOST --"
run "$SERVER_HOST" "rm -rf '$REMOTE_ART'; mkdir -p '$REMOTE_ART'"
launch "$SERVER_HOST" "env ORBITNET_DEBUG=1 $GODOT_REMOTE --headless --path demos/$DEMO -- --dedicated=$SERVER_PORT --quit-after=$((MEASURE_S + 150)) >'$REMOTE_ART/server.log' 2>&1"
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
		# STOP HERE IF THE RELAY NEVER BOUND. Clients join the relay port when RELAY=1, so a relay that failed to
		# bind sends every client at a dead port and the run fails twenty-five seconds later as "no BENCH-RESULT",
		# naming the wrong component.
		i=0; ok=0
		while [ "$i" -lt 25 ]; do grep_remote "$SERVER_HOST" "RELAY: bound" "$REMOTE_ART/relay.log" && { ok=1; break; }; sleep 1; i=$((i+1)); done
		[ "$ok" = "1" ] || { echo "relay never bound on $SERVER_HOST:$RELAY_PORT"; run "$SERVER_HOST" "tail -12 '$REMOTE_ART/relay.log'"; exit 1; }
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

# 3) WAIT FOR THE CLIENTS TO SELF-FINISH, don't sleep the window and hope. A client measures MEASURE_S from ITS
# OWN first spawn, and bringing up N hosts over ssh staggers those starts by however long the slowest host takes
# to import and connect -- which on a cross-site box is longer than any fixed margin. Collecting mid-run yields
# logs with no BENCH-RESULT, which the verdict below reads as a FAIL of the netcode rather than of the wait.
if [ "$DRY" != "1" ]; then
	echo "-- running (${MEASURE_S}s window per client; polling for self-finish) --"
	deadline=$((MEASURE_S + 90)); waited=0; done_n=0
	while [ "$waited" -lt "$deadline" ]; do
		done_n=0
		for h in $CLIENT_HOSTS; do
			n="$(run "$h" "grep -al 'BENCH-RESULT' '$REMOTE_ART'/client*.log 2>/dev/null | wc -l | tr -d ' '")"
			is_uint "${n:-}" && done_n=$((done_n + n))
		done
		[ "$done_n" -ge "$EXPECTED_CLIENTS" ] && { echo "-- all $EXPECTED_CLIENTS client(s) finished --"; break; }
		sleep 5; waited=$((waited + 5))
	done
	[ "$waited" -lt "$deadline" ] || echo "-- deadline reached with $done_n/$EXPECTED_CLIENTS finished; collecting what exists --"
fi
echo "-- collect artifacts -> $OUT --"
collect "$SERVER_HOST"
for h in $CLIENT_HOSTS; do collect "$h"; done

if [ "$DRY" = "1" ]; then echo "=== dry run complete (no verdict) ==="; exit 0; fi

# 4) Verdict: every EXPECTED client log must carry BENCH-RESULT PASS. Counting only the logs that arrived would
# let an unreachable host silently shrink the fleet and still pass, which is the one outcome this bench must not
# produce -- the whole point is how the netcode behaves with those peers on the link.
fail=0; total=0
echo "--- client verdicts ---"
while IFS= read -r log; do
	total=$((total + 1))
	line=$(grep -a "BENCH-RESULT" "$log" | tail -1)
	rel=${log#"$OUT/"}
	if echo "$line" | grep -q "BENCH-RESULT PASS"; then echo "  $rel: $line"; else echo "  $rel: ${line:-NO RESULT}"; fail=1; fi
done < <(find "$OUT" -name 'client*.log' | sort)
if [ "$total" -eq 0 ]; then
	echo "no client logs collected -- check SSH/host reachability (try --preflight)"; fail=1
elif [ "$total" -lt "$EXPECTED_CLIENTS" ]; then
	echo "  collected $total client log(s), expected $EXPECTED_CLIENTS -- a host did not report"; fail=1
fi

echo "(artifacts: $OUT)"
if [ "$fail" -ne 0 ]; then echo "=== GAUNTLET: FAIL ==="; exit 1; fi
echo "=== GAUNTLET: PASS ($total client(s) across $(echo "$CLIENT_HOSTS" | wc -w | tr -d ' ') host(s)) ==="
