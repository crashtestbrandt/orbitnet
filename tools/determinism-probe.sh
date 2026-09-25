#!/usr/bin/env bash
# Cross-peer SIMULATION DETERMINISM gate, in the harness project. This is a PR gate.
#
# Records one input tape, then runs THREE peer processes over the same tick range from that tape and compares
# their RESTORED state column by column, per tick:
#
#   a         the reference peer
#   b         a second peer, identical in every respect
#   control   the NEGATIVE CONTROL -- peer b plus a one-ULP nudge to `sim_drift` at a named tick
#
# WHAT IT GATES that the other three probes do not. They compare a world SIGNATURE across peers, which gates
# deterministic node naming and therefore entity-id agreement. None of them gates whether two peers handed the
# same inputs compute the same STATE, and the unit-level determinism suites are each one function called twice
# with the same seed in one process. CONTRIBUTING.md lists rollback determinism as the first thing a gating
# probe may guard; this is the probe that reaches it.
#
# WHAT IT DOES NOT COVER. The scenario registers ONE rollback entity and runs both peers as the same binary on
# one host, so within-tick entity ordering and a divergence that needs a different CPU or libm are outside it.
# harness/scripts/determinism.gd states both limits and why.
#
# FIRST DIVERGENT TICK AND COLUMN, not a boolean. `det_compare` walks the ticks in ascending order and the
# columns in registration order, stops at the first digest that differs, and prints the tick, the property and
# both peers' values. A gate that reports only "they differ" costs more to act on than it saves. A `DET-ROW`
# digest is the column's exact stored bytes in hex, so the comparison has no collision to worry about.
#
# THE NEGATIVE CONTROL IS PART OF THE RUN. The control peer's nudge is one ULP -- the smallest difference that
# exists -- and this script asserts that the comparison CATCHES it and NAMES ITS TICK. It also asserts the
# control peer reported PASS on its own three assertions, because that is the whole argument for a cross-peer
# probe: a one-peer determinism test cannot see a desync of any size.
#
# NO SOCKET. The peers are unconnected, so nothing but determinism can make them agree -- a connected pair
# would agree because the authority's rows overwrite the client's state, which is reconciliation and is
# already gated. The consequences here are that the three peers run CONCURRENTLY, that no port can be in use,
# and that this probe runs before the three that bind one. harness/scripts/determinism.gd carries the rest of
# the reasoning.
#
# TICK-DOMAIN, so the gate behaves the same on a fast desktop and a loaded CI runner: each peer exits on a
# tick COUNT rather than on a wall-clock duration, and every assertion below reads ticks and digests.
#
# The harness project is used rather than a demo for the reason its project.godot states -- a failure in a demo
# could be the demo's fault or the addon's. The RTS demo in particular documents that its steering does not
# require determinism, so its bodies are the wrong subject for this question.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT="$ROOT/harness"
SCENE="res://scenes/determinism.tscn"
GODOT="${GODOT:-$ROOT/tools/godot-quiet.sh}"
# The tick the control peer nudges. Inside the sampled range (warmup 60, sample 240) and away from both ends,
# so a failure to catch it cannot be an off-by-one at a boundary.
DIVERGE_TICK="${DET_PROBE_DIVERGE_TICK:-137}"
# Sampled ticks per peer, and columns per tick. Mirrors the scenario's own constants; the scenario asserts
# them too, so a change there fails loudly here rather than silently weakening the comparison.
MIN_TICKS="${DET_PROBE_MIN_TICKS:-240}"
COLUMNS_PER_TICK=5
# A hung run must fail loudly rather than hang CI forever. A peer is ~300 ticks at 60 Hz plus process start.
WATCHDOG_S="${DET_PROBE_WATCHDOG_S:-90}"

TAPE=""
A_LOG=""
B_LOG=""
C_LOG=""
REC_LOG=""
A_PID=""
B_PID=""
C_PID=""
WATCH_PID=""

# Set only on the way out of a passing run. A failing run KEEPS its peer logs, because the evidence is the
# 1200 per-tick rows each peer printed and nothing upstream of here can re-derive them -- the CI job uploads
# `detprobe-*` as an artifact for the same reason.
PASSED=0

cleanup() {
	for pid in "$A_PID" "$B_PID" "$C_PID" "$WATCH_PID"; do
		[ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
	done
	rm -f "$TAPE"
	if [ "$PASSED" -eq 1 ]; then
		rm -f "$A_LOG" "$B_LOG" "$C_LOG" "$REC_LOG"
	fi
	return 0
}
trap cleanup EXIT

if [ ! -d "$PROJECT/addons/orbitnet" ]; then
	printf 'determinism-probe FAILED: harness/addons/orbitnet is missing -- run `just sync-addons` first.\n' >&2
	exit 1
fi

TAPE="$(mktemp "${TMPDIR:-/tmp}/detprobe-tape.XXXXXX")"
REC_LOG="$(mktemp "${TMPDIR:-/tmp}/detprobe-record.XXXXXX")"
A_LOG="$(mktemp "${TMPDIR:-/tmp}/detprobe-a.XXXXXX")"
B_LOG="$(mktemp "${TMPDIR:-/tmp}/detprobe-b.XXXXXX")"
C_LOG="$(mktemp "${TMPDIR:-/tmp}/detprobe-control.XXXXXX")"

field() { grep -aoE "$2" "$1" | tail -1 | sed -E "s/$3//"; }

# --- 1. the tape ----------------------------------------------------------------------------------
# Written once and replayed by all three peers, through InputTape.save()/load_from(). A tape that did not
# round-trip would hand the peers different input and read exactly like a desync, so the frame count is
# checked before any peer starts.
echo "determinism-probe: recording the input tape..."
"$GODOT" --headless --path "$PROJECT" "$SCENE" -- \
	--role=record --label=record --tape="$TAPE" >"$REC_LOG" 2>&1
rec_rc=$?
grep -aE "DET-TAPE|DET-RESULT|DET-FAIL" "$REC_LOG" || echo "(no record output)"
tape_frames="$(field "$REC_LOG" 'DET-TAPE .* frames=[0-9]+' '.*frames=')"
if [ "$rec_rc" -ne 0 ] || [ -z "$tape_frames" ] || [ "$tape_frames" -lt "$MIN_TICKS" ] 2>/dev/null; then
	echo "determinism-probe FAILED: the record pass wrote ${tape_frames:-no} frames (rc=$rec_rc)."
	exit 1
fi

# --- 2. the three peers, concurrently -------------------------------------------------------------
echo "determinism-probe: running peers a, b and the control (nudged at tick $DIVERGE_TICK)..."
"$GODOT" --headless --path "$PROJECT" "$SCENE" -- \
	--role=peer --label=a --tape="$TAPE" >"$A_LOG" 2>&1 &
A_PID=$!
"$GODOT" --headless --path "$PROJECT" "$SCENE" -- \
	--role=peer --label=b --tape="$TAPE" >"$B_LOG" 2>&1 &
B_PID=$!
"$GODOT" --headless --path "$PROJECT" "$SCENE" -- \
	--role=peer --label=control --tape="$TAPE" --diverge-at="$DIVERGE_TICK" >"$C_LOG" 2>&1 &
C_PID=$!

# REDIRECTED to /dev/null, and the redirect is what keeps the advertised cost honest. The subshell's `sleep`
# inherits this script's stdout, and killing the subshell below does not kill the `sleep` -- it is reparented
# to init and holds the write end of the pipe open until it expires. A consumer reading this script through a
# pipe (the CI log capture, `tee`, `just check 2>&1 | ...`) then blocks for the full watchdog after the run has
# printed its verdict, which is 84 seconds of dead wait on a six-second run. The other three probes carry the
# same redirect for the same reason.
( sleep "$WATCHDOG_S"; kill -9 "$A_PID" "$B_PID" "$C_PID" 2>/dev/null ) >/dev/null 2>&1 &
WATCH_PID=$!

wait "$A_PID" 2>/dev/null; a_rc=$?
wait "$B_PID" 2>/dev/null; b_rc=$?
wait "$C_PID" 2>/dev/null; c_rc=$?
# Grouped and silenced: the shell announces a killed background job on the terminal, which lands in the middle
# of the run's output and reads as one of the peers having died.
{ kill -9 "$WATCH_PID"; wait "$WATCH_PID"; } 2>/dev/null
A_PID=""; B_PID=""; C_PID=""; WATCH_PID=""

# Everything but the per-tick rows, which are 1200 lines per peer.
for pair in "a:$A_LOG:$a_rc" "b:$B_LOG:$b_rc" "control:$C_LOG:$c_rc"; do
	label="${pair%%:*}"; rest="${pair#*:}"; log="${rest%%:*}"; rc="${rest##*:}"
	echo "=== PEER $label (rc=$rc) ==="
	grep -aE "DET-BOOT|DET-COLS|DET-LANE|DET-RESTORE|DET-WRITEBACK|DET-RANGE|DET-RESULT|DET-FAIL" "$log" \
		|| echo "(no scenario output)"
done

# --- 3. the comparison ----------------------------------------------------------------------------
# Ascending ticks, registration-order columns, stop at the first difference. Prints one of:
#
#   AGREE <ticks> <rows_a> <rows_b>
#   DIVERGE <tick> <column> <value_a> <value_b>
#   MISSING <tick> <column>            a tick/column one peer reported and the other did not
det_compare() {
	awk '
		FNR == NR {
			if ($1 == "DET-ROW") {
				t = substr($2, 3) + 0; c = substr($3, 3); h = substr($4, 3); v = substr($5, 3)
				A[t SUBSEP c] = h; AV[t SUBSEP c] = v
				if (!(t in seen)) { seen[t] = 1; nt++; ticks[nt] = t }
				# Incremented on its own line. `ord[t SUBSEP ++ordn[t]]` parses as a concatenation with two
				# unary pluses instead, which leaves every column of a tick on one key and makes the whole
				# comparison below vacuous.
				ordn[t]++
				ord[t SUBSEP ordn[t]] = c
				ra++
			}
			next
		}
		$1 == "DET-ROW" {
			t = substr($2, 3) + 0; c = substr($3, 3)
			B[t SUBSEP c] = substr($4, 3); BV[t SUBSEP c] = substr($5, 3)
			rb++
		}
		END {
			for (i = 1; i <= nt; i++) {
				t = ticks[i]
				for (k = 1; k <= ordn[t]; k++) {
					c = ord[t SUBSEP k]
					key = t SUBSEP c
					if (!(key in B)) { printf "MISSING %d %s\n", t, c; exit }
					if (A[key] != B[key]) {
						printf "DIVERGE %d %s %s %s\n", t, c, AV[key], BV[key]
						exit
					}
				}
			}
			printf "AGREE %d %d %d\n", nt, ra, rb
		}
	' "$1" "$2"
}

ok=1
fail() { echo "determinism-probe: $1"; ok=0; }
verdict() { field "$1" 'DET-RESULT label=[a-z]+ (PASS|FAIL)' '.* '; }

# Every peer must have reached a verdict of its own before any comparison between them means anything.
a_verdict="$(verdict "$A_LOG")"
b_verdict="$(verdict "$B_LOG")"
c_verdict="$(verdict "$C_LOG")"
[ "$a_verdict" = "PASS" ] || fail "peer a did not PASS (${a_verdict:-no verdict at all})"
[ "$b_verdict" = "PASS" ] || fail "peer b did not PASS (${b_verdict:-no verdict at all})"
# The control asserts nothing different about itself. A FAIL here would mean the injection broke one of the
# scenario's own three assertions, and the comparison below would then be reading a broken run.
[ "$c_verdict" = "PASS" ] || fail "the NEGATIVE CONTROL peer did not PASS its own assertions
       (${c_verdict:-no verdict at all}) -- the one-ULP nudge is meant to be invisible to a single peer, so
       the comparison below is reading a run that failed for some other reason"

# The real comparison: a against b.
ab="$(det_compare "$A_LOG" "$B_LOG")"
ab_kind="${ab%% *}"
case "$ab_kind" in
	AGREE)
		set -- $ab
		ab_ticks="$2"; ab_rows_a="$3"; ab_rows_b="$4"
		if [ "$ab_ticks" -lt "$MIN_TICKS" ] 2>/dev/null; then
			fail "the two peers agreed over only $ab_ticks ticks (wanted $MIN_TICKS) -- an empty or short run
       is a failure, not a pass"
		elif [ "$ab_rows_a" != "$ab_rows_b" ]; then
			fail "peer a reported $ab_rows_a rows and peer b $ab_rows_b -- they did not sample the same range"
		elif [ "$ab_rows_a" -lt "$((MIN_TICKS * COLUMNS_PER_TICK))" ] 2>/dev/null; then
			fail "only $ab_rows_a rows were compared (wanted $((MIN_TICKS * COLUMNS_PER_TICK))) -- some column
       stopped reporting, so the comparison is narrower than it looks"
		else
			echo "determinism-probe: peers a and b agree on every column of every tick"
			echo "                   ($ab_ticks ticks, $ab_rows_a digests each, bit-exact)"
		fi
		;;
	DIVERGE)
		set -- $ab
		fail "THE TWO PEERS DESYNCED. First divergent tick $2, property \`$3\`:
         peer a: $4
         peer b: $5
       They ran the same tick range from the same input tape, so the simulation is not a pure function of
       (tape, tick). Look for a float column narrowed to f32, a quantizer whose canonical value is no longer
       what the sim resimulates from, or state the simulation reads that the tape does not carry -- a wall
       clock, an RNG, a frame delta."
		;;
	MISSING)
		set -- $ab
		fail "peer b never reported tick $2 column \`$3\`, which peer a did. The two peers did not run the same
       tick range, so there is nothing to compare them over."
		;;
	*)
		fail "the comparison could not be made: ${ab:-no rows at all}"
		;;
esac

# The negative control: a against the nudged peer. FAIL is the pass condition, and it has to name the right
# tick as well as fail -- a DIVERGE at some other tick would mean the comparison caught something else.
ac="$(det_compare "$A_LOG" "$C_LOG")"
ac_kind="${ac%% *}"
if [ "$ac_kind" != "DIVERGE" ]; then
	fail "the NEGATIVE CONTROL was not caught ($ac). One ULP was added to \`sim_drift\` at tick
       $DIVERGE_TICK and the comparison did not see it, so a PASS above means nothing."
else
	set -- $ac
	if [ "$2" != "$DIVERGE_TICK" ]; then
		fail "the negative control was caught at tick $2 rather than at tick $DIVERGE_TICK, where the nudge
       was injected -- the probe can see a divergence but cannot locate one"
	elif [ "$3" != "sim_drift" ]; then
		fail "the negative control was caught at the right tick but blamed \`$3\` rather than \`sim_drift\`,
       which is the column the nudge was applied to"
	else
		echo "determinism-probe: the injected one-ULP divergence was caught at tick $2, property \`$3\`"
		echo "                   (a: $4  control: $5)"
	fi
fi

# The readings, always printed. Three peers, one scenario, one table -- which is what makes any single run
# mean anything.
printf '\n%-9s %-7s %-7s %-7s %-8s %-8s %-15s %-17s %-7s\n' \
	peer verdict fresh replay passes ticks restore_bad writeback_bad canon
for pair in "a:$A_LOG" "b:$B_LOG" "control:$C_LOG"; do
	label="${pair%%:*}"; log="${pair#*:}"
	printf '%-9s %-7s %-7s %-7s %-8s %-8s %-15s %-17s %-7s\n' \
		"$label" \
		"$(verdict "$log")" \
		"$(field "$log" 'DET-LANE .* fresh=[0-9]+' '.*fresh=')" \
		"$(field "$log" 'DET-LANE .* replay=[0-9]+' '.*replay=')" \
		"$(field "$log" 'DET-LANE .* passes=[0-9]+' '.*passes=')" \
		"$(field "$log" 'DET-RANGE .* ticks=[0-9]+' '.*ticks=')" \
		"$(field "$log" 'DET-RESTORE .* bad_tick=-?[0-9]+' '.*bad_tick=')" \
		"$(field "$log" 'DET-WRITEBACK .* bad_tick=-?[0-9]+' '.*bad_tick=')" \
		"$(field "$log" 'DET-WRITEBACK .* canon=[0-9]+' '.*canon=')"
done
printf '\n'

if [ "$ok" -eq 1 ]; then
	PASSED=1
	echo "determinism-probe PASSED (two peers, same tape, same tick range, bit-identical restored state per
       column per tick -- and an injected one-ULP divergence was caught at its own tick)."
	exit 0
fi
echo "determinism-probe FAILED. The per-tick rows are kept for reading:"
echo "  peer a:   $A_LOG"
echo "  peer b:   $B_LOG"
echo "  control:  $C_LOG"
exit 1
