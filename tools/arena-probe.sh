#!/usr/bin/env bash
# Three-process networked gate for the arena demo. This is the THIRD PR gate.
#
# WHAT IT COVERS THAT tools/rts-probe.sh AND tools/server-shape-probe.sh CANNOT, which is why a third gating
# probe exists at all. Every line is the INTEREST axis -- who receives what -- which neither other probe
# reaches: the RTS probe replicates one world to every peer, and the shape probe reads one client's own seat.
#
#   * MEMBERSHIP filtering. Three arenas replicate the same LOCAL coordinates, so no radius can separate
#     them: a client receiving nothing from an arena it holds no seat in is membership doing it and nothing
#     else could have.
#   * A PER-PEER VETO. A cloaked enemy stops arriving on one peer while everything else keeps arriving.
#   * SEVERAL SEATS ON ONE CONNECTION, in two different arenas, receiving the union of both.
#   * A DECLARED interest anchor: an observer that drives nothing still has exactly one arena in interest.
#   * A SESSION RESUME: a relaunched process presenting the same identity gets its seats back.
#
# It also runs the DEDICATED-VERSUS-LISTEN comparison, with both readings printed. That one is NOT this
# probe's reason to exist -- tools/server-shape-probe.sh owns it, in the addon's own project where a failure
# cannot be a demo's fault. It is kept here because these three arenas exercise it under interest filtering,
# which the shape probe's single-seat scenario does not.
#
# THE READINGS ARE RISES, NOT VALUES. `NetStateHandle.last_known_state()` FAILS OPEN -- on a backend that
# cannot answer it returns the present tick -- so a threshold test would be satisfied by the fallback and
# would prove nothing. Every state-row assertion below is that a number went UP between two samples.
#
# Every assertion is tick-domain or scale-free, so the gate behaves the same on a fast desktop and a loaded CI
# runner. NO --fixed-fps: the backend's clock sync paces off the WALL clock, and --fixed-fps stalls the
# ping/pong so a client never finishes its handshake and the probe times out looking like a netcode failure.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="$ROOT/demos/arena"
GODOT="${GODOT:-$ROOT/tools/godot-quiet.sh}"
# Seconds each peer runs. The probe's phases key on the SESSION TICK, and the verdict fires at tick 620 --
# about 21 s of session at this demo's 30 Hz. This has to outlast that on the LAST process to start, which is
# the observer, five seconds behind the server: 26 s leaves roughly eight seconds of margin for a slow boot on
# a loaded runner. Too tight and a slow start turns into "no verdict at all", which reads as a hang rather
# than as the timing problem it is.
RUN_S="${ARENA_PROBE_RUN_S:-26}"
RESUME_S="${ARENA_PROBE_RESUME_S:-7}"
WATCHDOG_S="${ARENA_PROBE_WATCHDOG_S:-150}"
PORT_A="${ARENA_PROBE_PORT_A:-47810}"
PORT_B="${ARENA_PROBE_PORT_B:-47811}"
PORT_C="${ARENA_PROBE_PORT_C:-47812}"
PORT_D="${ARENA_PROBE_PORT_D:-47813}"
# The arena the observer declares itself into. Deterministic so the gate does not have to guess.
WATCH_ARENA="${ARENA_PROBE_WATCH:-3}"

PIDS=""
LOGS=""

cleanup() {
	# NOT "${WATCHPID:-0}". `kill -9 0` signals every process in the CALLER'S process group, so on the one
	# exit path that runs before the watchdog is armed -- the missing-addon guard below -- the trap would
	# SIGKILL the shell that invoked the probe, and in CI the rest of the job with it.
	[ -n "${WATCHPID:-}" ] && kill -9 "$WATCHPID" 2>/dev/null
	for pid in $PIDS; do kill -9 "$pid" 2>/dev/null; done
	for log in $LOGS; do rm -f "$log"; done
	return 0
}
trap cleanup EXIT

if [ ! -d "$PROJECT/addons/orbitnet" ]; then
	printf 'arena-probe FAILED: demos/arena/addons/orbitnet is missing -- run `just sync-addons` first.\n' >&2
	exit 1
fi

# NO COMMAND SUBSTITUTION AROUND A SPAWN. `$(...)` runs in a SUBSHELL, so a process started inside one is not
# a child of this shell -- `wait` then returns immediately, the script races past every peer before it has
# finished its handshake, and the gate reports "no verdict at all" for a session that was about to work.
# `newlog` and `spawn` therefore assign to a named variable instead of printing a value.
LOG=""
newlog() {
	LOG="$(mktemp "${TMPDIR:-/tmp}/arenaprobe-$1.XXXXXX")"
	LOGS="$LOGS $LOG"
}

SPAWNED=""
spawn() {
	# spawn <logfile> <args...>
	local log="$1"; shift
	"$GODOT" --headless --path "$PROJECT" -- "$@" >"$log" 2>&1 &
	SPAWNED=$!
	PIDS="$PIDS $SPAWNED"
}

# A hung session must fail loudly rather than hang CI forever. RE-ARMED PER PASS, because the subshell
# expands $PIDS at FORK time: one watchdog started in pass A holds pass A's three pids and would let a hung
# process in pass B or C block `wait` until the CI job's own timeout, which reports as a stall rather than as
# this gate failing.
arm_watchdog() {
	[ -n "${WATCHPID:-}" ] && kill -9 "$WATCHPID" 2>/dev/null
	( sleep "$WATCHDOG_S"; kill -9 $PIDS 2>/dev/null ) &
	WATCHPID=$!
}

ok=1
fail() { printf 'arena-probe: %s\n' "$1"; ok=0; }

field() { grep -aoE "$2" "$1" | tail -1 | sed -E "s/$3//"; }
verdict_of() { grep -aoE "ARENA-PROBE-RESULT role=[a-z]+ (PASS|FAIL)" "$1" | tail -1; }

# =====================================================================================================
# PASS A -- a DEDICATED server, a two-seat client, and an observer.
# =====================================================================================================
newlog server-a; SERVER_A="$LOG"
newlog client-a; CLIENT_A="$LOG"
newlog observer-a; OBSERVER_A="$LOG"

echo "arena-probe: pass A -- dedicated server on port $PORT_A"
spawn "$SERVER_A" --dedicated="$PORT_A" --arena-probe --quit-after="$((RUN_S + 5))"; SPID=$SPAWNED
sleep 3
spawn "$CLIENT_A" --join="127.0.0.1:$PORT_A" --seats=2 --arena-probe --quit-after="$RUN_S"; CPID=$SPAWNED
# THE SEATED CLIENT GOES FIRST, AND THE GAP IS NOT COSMETIC. Seats are handed out in handshake order, so two
# peers arriving in the same instant race for seat 0 -- and a client that ended up holding the seat the server
# cloaks would be asked whether it can see its OWN body, which it always can. Staggering makes the seating
# deterministic; the client's verdict checks the assumption anyway.
sleep 2
spawn "$OBSERVER_A" --join="127.0.0.1:$PORT_A" --observe --watch="$WATCH_ARENA" \
	--arena-probe --quit-after="$((RUN_S - 2))"; OPID=$SPAWNED

arm_watchdog

wait "$CPID" 2>/dev/null
wait "$OPID" 2>/dev/null
wait "$SPID" 2>/dev/null

echo "=== PASS A / SERVER (dedicated) ==="
grep -aE "ARENA-BOOT|ARENA-STATE|ARENA-PROBE|ARENA world|ARENA: peer" "$SERVER_A" || echo "(no output)"
echo "=== PASS A / CLIENT (two seats) ==="
grep -aE "ARENA-BOOT|ARENA-STATE|ARENA-SEATS|ARENA-PROBE" "$CLIENT_A" || echo "(no output)"
echo "=== PASS A / OBSERVER ==="
grep -aE "ARENA-BOOT|ARENA-STATE|ARENA-SEATS|ARENA-PROBE" "$OBSERVER_A" || echo "(no output)"

# 1. Every process reached a session and said so.
for pair in "server:$SERVER_A" "client:$CLIENT_A" "observer:$OBSERVER_A"; do
	role="${pair%%:*}"; log="${pair#*:}"
	v="$(verdict_of "$log")"
	case "$v" in
		*"role=$role PASS") ;;
		*) fail "the $role did not PASS (${v:-no verdict at all})";;
	esac
done

# 2. The world signatures agree. If this fails the peers built different node paths, their entity ids
#    disagree, and every assertion below is meaningless -- so say that rather than reporting five failures.
sig_s="$(field "$SERVER_A" 'ARENA-PROBE sig=-?[0-9]+' 'ARENA-PROBE sig=')"
sig_c="$(field "$CLIENT_A" 'ARENA-PROBE sig=-?[0-9]+' 'ARENA-PROBE sig=')"
sig_o="$(field "$OBSERVER_A" 'ARENA-PROBE sig=-?[0-9]+' 'ARENA-PROBE sig=')"
if [ -z "$sig_s" ] || [ -z "$sig_c" ] || [ -z "$sig_o" ]; then
	fail "a peer never printed a world signature"
elif [ "$sig_s" != "$sig_c" ] || [ "$sig_s" != "$sig_o" ]; then
	fail "WORLD SIGNATURES DIFFER (server=$sig_s client=$sig_c observer=$sig_o) -- the peers built
       different node paths, so their entity ids disagree and replication is going nowhere. Look for a
       node added without an explicit name (Godot's auto-names are allocation-order dependent)."
else
	echo "arena-probe: world signatures match ($sig_s)"
fi

# 3. The dedicated server delivered authoritative rows for the joining client's OWN body. This is the
#    reading #26 asks for, and it is a RISE because last_known_state() fails open.
own_rise_dedicated="$(field "$CLIENT_A" 'own_rise=-?[0-9]+' '.*own_rise=')"
if [ -z "$own_rise_dedicated" ]; then
	fail "the client never reported a last-known-state rise for its own body"
elif [ "$own_rise_dedicated" -le 0 ]; then
	fail "DEDICATED: the joining client's own body received NO authoritative row (rise=$own_rise_dedicated)"
else
	echo "arena-probe: dedicated -- the client's own body advanced $own_rise_dedicated ticks"
fi

# 4. The veto kept a cloaked enemy from the client that must not see it.
#
#    THREE READINGS, AND EACH ONE CLOSES A WAY THE OTHER TWO COULD PASS VACUOUSLY.
#
#    * `early_rise` -- the fighter WAS arriving before it cloaked. An entity that was never sent also never
#      stops, and never carries a flag either.
#    * `watched_rise` -- its rows then stopped dead. This is the veto stated as the rows themselves, and it
#      is assertable because both lanes now publish a RECEIPT tick: one writer, on the receive path, so it
#      does not move for a withheld body. It used to be read off the frontier, which also counts ticks the
#      reading peer authored and therefore rises on a server whatever the wire did.
#    * `sees_cloak` -- the peer never learned the FLAG. The cloak bit rides in `net_flags`, inside the rows
#      the veto is refusing, so this is the fact the game is actually about; keeping it means a veto that
#      stopped the rows for some other reason still has to explain itself.
#
#    The client's own body is the positive control for the middle one, and it is checked in section 3 above.
early_rise="$(field "$CLIENT_A" 'early_rise=-?[0-9]+' 'early_rise=')"
watched_rise="$(field "$CLIENT_A" 'watched_rise=-?[0-9]+' '.*watched_rise=')"
sees_settled="$(field "$CLIENT_A" 'settled=[01]' '.*settled=')"
hidden_peak="$(field "$SERVER_A" 'hidden_peak=[0-9]+' '.*hidden_peak=')"
if [ -z "$sees_settled" ] || [ -z "$hidden_peak" ]; then
	fail "the veto reading is missing from one side"
elif [ "$hidden_peak" -le 0 ]; then
	fail "the server never withheld an entity from any peer (hidden_peak=$hidden_peak)"
elif [ -z "$early_rise" ] || [ "$early_rise" -le 0 ]; then
	fail "the cloaked fighter was NOT arriving before it cloaked (rise=${early_rise:-none}), so nothing about
       withholding it proves anything -- an entity that was never sent is not being withheld"
elif [ "${sees_settled:-1}" != "0" ]; then
	fail "the withheld peer LEARNED that the fighter cloaked -- the cloak flag rides in the rows the veto is
       supposed to be refusing, so this peer is still being sent them"
elif [ -z "$watched_rise" ] || [ "$watched_rise" -ne 0 ]; then
	# THE ROWS THEMSELVES, asserted directly. Both lanes publish a RECEIPT tick now -- the newest row this peer
	# decoded, one writer, on the receive path -- so a withheld body's reading does not move. It used to be
	# read off the frontier, which also counts ticks the reading peer authored and therefore rises on a server
	# whatever the wire did; that is why this line could not exist before.
	fail "the withheld fighter kept delivering rows to this peer (+$watched_rise ticks after the cloak)"
else
	echo "arena-probe: the fighter was arriving (+$early_rise ticks), then cloaked without this peer ever"
	echo "arena-probe: learning it, its rows stopped dead (+0 ticks), and $hidden_peak entity-peer pair(s)"
	echo "arena-probe: were withheld"
fi

# 5. A peer acknowledging a frame it was not provably sent. A clean session sits at exactly 0.
unproven="$(field "$SERVER_A" 'unproven_max=[0-9.]+' '.*unproven_max=')"
case "$unproven" in
	""|"0.000") echo "arena-probe: no peer acknowledged an unproven frame" ;;
	*) fail "a peer acknowledged a frame it was not provably sent (unproven_max=$unproven)" ;;
esac

# 6. The shot path ran end to end -- validation, the rewind ring, the banded resolve. The three per-band
#    depths are REPORTED rather than asserted to differ: whether they do depends on the send path having
#    published a per-band measurement, and with none the per-target rewind correctly degenerates to the flat
#    window rather than inventing a spread.
shots="$(field "$SERVER_A" 'shots=[0-9]+' '.*shots=')"
if [ -z "$shots" ] || [ "$shots" -le 0 ]; then
	fail "no shot reached the resolver, so the lag-compensated path never ran"
else
	echo "arena-probe: $shots shot(s) resolved through the rewind ring"
	grep -aoE 'ARENA-PROBE rewind .*' "$SERVER_A" | tail -1 | sed 's/^/arena-probe:   /'
fi

# =====================================================================================================
# PASS B -- the same channel against a LISTEN server. Half of #26's comparison; pass A is the other half.
# =====================================================================================================
newlog server-b; SERVER_B="$LOG"
newlog client-b; CLIENT_B="$LOG"

echo "arena-probe: pass B -- listen server on port $PORT_B"
spawn "$SERVER_B" --host="$PORT_B" --arena-probe --quit-after="$((RUN_S + 3))"; SPID_B=$SPAWNED
sleep 3
spawn "$CLIENT_B" --join="127.0.0.1:$PORT_B" --seats=2 --arena-probe --quit-after="$RUN_S"; CPID_B=$SPAWNED
arm_watchdog

wait "$CPID_B" 2>/dev/null
wait "$SPID_B" 2>/dev/null

echo "=== PASS B / SERVER (listen) ==="
grep -aE "ARENA-BOOT|ARENA-STATE|ARENA-PROBE|ARENA: peer" "$SERVER_B" || echo "(no output)"
echo "=== PASS B / CLIENT ==="
grep -aE "ARENA-BOOT|ARENA-STATE|ARENA-SEATS|ARENA-PROBE" "$CLIENT_B" || echo "(no output)"

for pair in "server:$SERVER_B" "client:$CLIENT_B"; do
	role="${pair%%:*}"; log="${pair#*:}"
	v="$(verdict_of "$log")"
	case "$v" in
		*"role=$role PASS") ;;
		*) fail "pass B: the $role did not PASS (${v:-no verdict at all})";;
	esac
done

own_rise_listen="$(field "$CLIENT_B" 'own_rise=-?[0-9]+' '.*own_rise=')"
if [ -z "$own_rise_listen" ]; then
	fail "pass B: the client never reported a last-known-state rise for its own body"
elif [ "$own_rise_listen" -le 0 ]; then
	fail "LISTEN: the joining client's own body received NO authoritative row (rise=$own_rise_listen)"
else
	echo "arena-probe: listen -- the client's own body advanced $own_rise_listen ticks"
fi

# THE COMPARISON, stated as one line so it can be recorded. Both shapes run the same channel from the
# client's side -- encode, datagram, decode -- and differ only in what the SERVER itself holds.
echo "arena-probe: #26 reading -- own-body last-known-tick rise: dedicated=${own_rise_dedicated:-none} listen=${own_rise_listen:-none}"

# =====================================================================================================
# PASS C -- a relaunched process presenting the same session identity gets its seats back.
# =====================================================================================================
newlog server-c; SERVER_C="$LOG"
newlog client-c1; CLIENT_C1="$LOG"
newlog client-c2; CLIENT_C2="$LOG"
newlog server-d; SERVER_D="$LOG"
newlog client-d1; CLIENT_D1="$LOG"
newlog client-d2; CLIENT_D2="$LOG"
SESSION_ID="${ARENA_PROBE_SESSION:-987654321}"
# A different identity for pass D, so the two runs cannot borrow each other's state through a stray process.
FORGED_ID="${ARENA_PROBE_FORGED_SESSION:-123456789}"

echo "arena-probe: pass C -- resume on port $PORT_C"
spawn "$SERVER_C" --dedicated="$PORT_C" --quit-after="$((RESUME_S * 2 + 10))"; SPID_C=$SPAWNED
sleep 3
spawn "$CLIENT_C1" --join="127.0.0.1:$PORT_C" --seats=2 --session="$SESSION_ID" \
	--quit-after="$RESUME_S"; C1=$SPAWNED
arm_watchdog
wait "$C1" 2>/dev/null
sleep 2

# THE TOKEN THE SERVER ISSUED THIS IDENTITY, read out of the first process's log.
#
# An identity alone reclaims nothing: the server mints a token per identity, sends it in the welcome, and a
# rejoiner must quote it back -- which is what stops a peer that merely READ somebody's session id from taking
# their body. A real game persists the value; a relaunched demo process has no store, so the shell carries it
# the way a save file would. An empty read means the first client never reached PLAYING, and the resume
# assertions below fail on their own rather than being skipped here.
RESUME_TOKEN="$(field "$CLIENT_C1" 'ARENA-TOKEN=[0-9]+' 'ARENA-TOKEN=')"
echo "arena-probe: the first process was issued resume token ${RESUME_TOKEN:-none}"

spawn "$CLIENT_C2" --join="127.0.0.1:$PORT_C" --seats=2 --session="$SESSION_ID" \
	--resume-token="${RESUME_TOKEN:-0}" --quit-after="$RESUME_S"; C2=$SPAWNED
arm_watchdog
wait "$C2" 2>/dev/null
wait "$SPID_C" 2>/dev/null

echo "=== PASS C / SERVER ==="
grep -aE "ARENA: peer|ARENA: seats" "$SERVER_C" || echo "(no output)"

first_seats="$(grep -aoE 'seated at \[[0-9, ]+\]' "$SERVER_C" | head -1 | sed 's/seated at //')"
resumed_seats="$(grep -aoE 'resumed seats \[[0-9, ]+\]' "$SERVER_C" | tail -1 | sed 's/resumed seats //')"
if ! grep -aq "dropped -- holding seats" "$SERVER_C"; then
	fail "the server did not hold the departing peer's seats, so there was nothing to resume"
elif [ -z "$resumed_seats" ]; then
	fail "the relaunched process presenting the same identity was NOT given its seats back"
elif [ "$first_seats" != "$resumed_seats" ]; then
	fail "the resumed seats differ from the ones held (held=$first_seats resumed=$resumed_seats)"
else
	echo "arena-probe: a relaunched process reclaimed seats $resumed_seats under a new peer id"
fi

# =====================================================================================================
# PASS D -- THE NEGATIVE CONTROL FOR PASS C. A peer presenting the same identity WITHOUT the token the server
# issued must be seated as a newcomer, and the seats must stay held.
#
# Pass C on its own cannot distinguish "the token was checked and matched" from "nothing checks the token": a
# server that ignored it entirely would pass every assertion there. This is the run that tells them apart, and
# it is the case the identity was forgeable in -- reading somebody's session id off a roster broadcast, a kill
# feed or a log line and presenting it.
echo "arena-probe: pass D -- a forged identity on port $PORT_D"
spawn "$SERVER_D" --dedicated="$PORT_D" --quit-after="$((RESUME_S * 2 + 10))"; SPID_D=$SPAWNED
sleep 3
spawn "$CLIENT_D1" --join="127.0.0.1:$PORT_D" --seats=2 --session="$FORGED_ID" \
	--quit-after="$RESUME_S"; D1=$SPAWNED
arm_watchdog
wait "$D1" 2>/dev/null
sleep 2
# The same identity, and a token this server never issued. `--resume-token=0` would be a client that simply
# never learned one, which is the same refusal by a different route; a wrong non-zero value is the forgery.
spawn "$CLIENT_D2" --join="127.0.0.1:$PORT_D" --seats=2 --session="$FORGED_ID" \
	--resume-token=1 --quit-after="$RESUME_S"; D2=$SPAWNED
arm_watchdog
wait "$D2" 2>/dev/null
wait "$SPID_D" 2>/dev/null

echo "=== PASS D / SERVER ==="
grep -aE "ARENA: peer" "$SERVER_D" || echo "(no output)"

if ! grep -aq "dropped -- holding seats" "$SERVER_D"; then
	fail "pass D: the server did not hold the departing peer's seats, so the forgery had nothing to take"
elif grep -aq "resumed seats" "$SERVER_D"; then
	fail "pass D: an identity presented WITHOUT its token was given the seats back -- the token is not checked"
else
	echo "arena-probe: an identity presented without its token reclaimed nothing"
fi

# =====================================================================================================
if [ "$ok" -eq 1 ]; then
	echo "arena-probe PASSED (dedicated boot, membership, veto, seats, declared anchor, resume, the token that"
	echo "       gates it, and rewind)."
	exit 0
fi
echo "arena-probe FAILED." >&2
exit 1
