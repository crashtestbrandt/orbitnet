#!/usr/bin/env python3
"""Diff two netbench runs, column by column, so "no regression" is a number rather than an opinion.

    NETBENCH_OUT=/tmp/nb-before just netbench 4 congested_wifi 25 1 strafe_fire
    ...make the change...
    NETBENCH_OUT=/tmp/nb-after  just netbench 4 congested_wifi 25 1 strafe_fire
    tools/netbench/compare.py /tmp/nb-before /tmp/nb-after

Two runs are comparable only when EVERY argument matched, seed included. The impairment scheduler is seeded and
deterministic, so the same seed replays the same link exactly; a different seed gives a different link, and the
two columns are then measuring two different networks.

TWO TABLES, AND THE SECOND ONE IS THE POINT FOR A SEND-PATH CHANGE. The client fleet's per-tick CSVs report
what a CLIENT sees, and every column describing the send path -- bytes admitted, blocks culled, the interest
pass -- reads zero there, because a client is not the authority and runs none of it. `bench.sh` therefore folds
the server's own per-second wire line into `server.csv`, and that is where server egress is compared.

MEDIANS, NOT MEANS. Each column is pooled across rows and reported at p50 and p95. A mean over a per-tick
series is dominated by the connect transient and by whichever client happened to stall, which is exactly the
noise that makes two honest runs look different.

WARM-UP IS DROPPED. The first `--warmup` seconds of each client's series (default 3) cover the handshake, the
first full-state burst and the clock's initial convergence, none of which a steady-state comparison is about.

THE VERDICT IS ADVISORY. `--tolerance` (default 5%) decides which deltas print as REGRESSED or improved rather
than as noise, and the exit code follows the regressions -- but the right tolerance depends on the machine and
the fleet size, so read the table when the two disagree.

A RELATIVE TOLERANCE IS NOT ENOUGH ON ITS OWN. The per-frame CPU timers are sub-millisecond, so 5% of them is
smaller than the spread between two runs of one binary; those columns carry an absolute floor as well. See
`NOISE_FLOOR_ABS`.

Only columns whose DIRECTION is known are judged. `rtt_ms` is set by the profile rather than by the netcode and
`interest_entities` is a scene fact; both are printed and neither is judged.

A column that is ZERO ON BOTH SIDES reads `not measured` rather than `same`. Most send-path columns are
collected on the server and appear in no client CSV, so in a client-only comparison they are absent rather
than unchanged -- and `same` on a dozen of them reads as a send path that was compared and found equal.

A column zero on the BASELINE ONLY reads `new (was 0)`, and does not count as a regression. The ratio has
no denominator, which is why the delta column prints `n/a` -- judging it anyway meant one row printed `n/a`
and `REGRESSED` side by side, and the exit code followed the second. It is still worth a look, so it prints
under its own verdict rather than as `not measured`: comparing across a release boundary, this is how a
capability the baseline never exercised appears.

EXCEPT FOR THE FAULT COUNTERS, where zero is the pass condition rather than a missing measurement. A run
with no `want_full_nacks_s` and no `reconcile_snap` is a healthy run, so leaving zero is the highest-signal
regression the bench can report and it fails the gate on the absolute move. See `FAULT_COUNTERS`.
"""

from __future__ import annotations

import argparse
import csv
import math
import os
import sys

# Columns worth judging, and which direction is better. A column absent from these tables is printed with its
# delta and no verdict, because a number nobody can say the sign of is not a gate.
LOWER_IS_BETTER = {
    "rollback_ms": "rollback loop cost, ms per frame",
    "net_ms": "send/receive path cost, ms per frame",
    "interest_ms": "interest pass cost, ms per tick",
    "reconcile_error": "how far this peer's prediction was from the server",
    "reconcile_snap": "corrections large enough to snap rather than smooth",
    "tx_bytes_s": "payload bytes sent per second",
    "tx_wire_bytes_s": "wire bytes sent per second, framing included",
    "tx_peak_peer_bytes_s": "the busiest single peer's bytes per second",
    "rx_bytes_s": "payload bytes received per second",
    "blocks_deferred_s": "entity blocks the budget pushed to a later tick",
    "want_full_nacks_s": "delta chains that broke and asked for a full block",
    "starve_ticks_max": "longest an in-interest entity went unsent",
    "unsent_backlog_max": "deepest the send queue got",
    "interarrival_near": "ticks between rows for a near body",
    "interarrival_mid": "ticks between rows for a mid-band body",
    "interarrival_far": "ticks between rows for a far body",
}

HIGHER_IS_BETTER = {
    "blocks_admitted_s": "entity blocks actually sent per second",
    "hits_confirmed": "authoritative hits confirmed",
}

# AN ABSOLUTE FLOOR FOR COLUMNS WHOSE OWN REPEATABILITY IS WORSE THAN THE TOLERANCE.
#
# The per-frame CPU timers sit near 0.02-0.03 ms, where a 5% relative test is 0.001 ms -- below the spread
# two runs of the SAME BINARY on the same seed produce. Measured, rather than assumed: back-to-back runs of
# one commit moved `rollback_ms` +11.5% and `net_ms` +10.0%, and both printed REGRESSED — so every verdict
# on these columns was untrustworthy without a floor.
#
# The floor is the smaller of "what the machine can resolve" and "what a player could feel": 0.05 ms is
# 0.15% of a 33 ms tick, so a move under it is not a regression whatever the ratio says. Columns absent
# from this table keep the pure relative test -- byte counters are exact and need no floor.
NOISE_FLOOR_ABS = {
    "rollback_ms": 0.05,
    "net_ms": 0.05,
    "interest_ms": 0.05,
}

# COLUMNS WHERE ZERO IS THE PASS CONDITION, NOT AN ABSENCE OF MEASUREMENT.
#
# The `new (was 0)` rule below exists because a zero baseline gives a percentage no denominator. That
# is right for a capability column -- `rollback_ms` reading 0.000 across a release boundary means the
# baseline never resimulated a tick, not that it was infinitely fast. It is exactly wrong for these:
# a healthy run reads zero, and leaving zero is the regression. Judged on the MEDIAN leaving zero, so
# sustained faulting fails the gate while a single stray sample -- a spawn-in-flight skip -- does not.
FAULT_COUNTERS = frozenset({
    "want_full_nacks_s",
    "reconcile_snap",
    "reconcile_error",
    "starve_ticks_max",
    "unsent_backlog_max",
    "blocks_deferred_s",
    "rx_rejected_s",
    "rx_skipped_s",
})

# Printed, never judged: set by the profile, by the scene, or by the run's own bookkeeping.
# `blocks_s` is deliberately unjudged: more blocks at the same byte count is a BETTER refresh rate, and more
# blocks at a higher byte count is worse, so the number says nothing without the bytes beside it.
# `resim_ticks` is unjudged for the reason the run's own gate does not judge it: resim depth legitimately
# deepens under latency and is bounded by `history_limit`, and prediction that is actually broken shows up
# as `reconcile_snap`, not as depth. Judging it reports a deeper-but-healthy run as a regression.
UNJUDGED = ("rtt_ms", "jitter_ms", "stretch", "offset_ms", "interest_entities", "shots_fired",
            "second", "tick", "mode", "peers", "ents_rollback", "ents_state", "blocks_s",
            "resim_ticks")

# The server's own per-second wire line, folded out of its log by bench.sh.
SERVER_CSV = "server.csv"

# A per-second server column that already describes ONE peer, so dividing it by the peer count again would be
# wrong. Every other `*_s` column in server.csv is a total across every synced peer. See `server_columns`.
PER_PEER_EXEMPT = frozenset({"tx_peak_peer_bytes_s"})
SERVER_LOWER_IS_BETTER = {
    "tx_bytes_s": "server egress: snapshot payload bytes per second, per synced peer",
    "rx_rejected_s": "inbound rows the server refused",
    "rx_skipped_s": "inbound rows the server could not place",
}


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return float("nan")
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def load_run(directory: str, warmup_s: float) -> dict[str, list[float]]:
    """Pool every CLIENT csv in `directory` into one column -> values map, warm-up dropped."""
    pooled: dict[str, list[float]] = {}
    files = sorted(
        os.path.join(directory, name)
        for name in os.listdir(directory)
        if name.endswith(".csv") and name != SERVER_CSV
    )
    if not files:
        raise SystemExit(f"netbench compare: no client CSVs under {directory}")
    for path in files:
        with open(path, newline="") as handle:
            rows = list(csv.DictReader(handle))
        if not rows:
            continue
        # `time` is seconds since this client's first sample, so the warm-up cut is per client rather than
        # per run -- clients connect staggered, and a run-wide cut would trim the last one twice over.
        start = float(rows[0].get("time") or 0.0)
        for row in rows:
            when = float(row.get("time") or 0.0)
            if when - start < warmup_s:
                continue
            for column, text in row.items():
                if column in ("tick", "time") or text in (None, ""):
                    continue
                try:
                    value = float(text)
                except ValueError:
                    continue
                if math.isnan(value):
                    continue
                pooled.setdefault(column, []).append(value)
    return pooled


def load_server(directory: str, warmup_s: float) -> dict[str, list[float]]:
    """The server's own per-second rows, or an empty map when the run carries no server.csv. See `server_columns`."""
    path = os.path.join(directory, SERVER_CSV)
    if not os.path.exists(path):
        return {}
    with open(path, newline="") as handle:
        return server_columns(list(csv.DictReader(handle)), warmup_s)


def per_peer(column: str) -> bool:
    """Whether a server column is a per-second total across every peer, and is judged per synced peer."""
    return column.endswith("_s") and column not in PER_PEER_EXEMPT


def server_columns(rows: list[dict[str, str]], warmup_s: float) -> dict[str, list[float]]:
    """Pool server.csv rows into a column -> values map: only seconds with a peer, warm-up dropped, totals per peer.

    THE SERVER IS UP BEFORE THE FLEET IS. Its first rows are the seconds between binding the port and the
    first client arriving: `peers` is 0 and every wire column with it. Pooling those makes each figure a
    function of how long bringup happened to take, which is the one thing a comparison must not depend on.
    So the series starts at the first second the server had a peer, and the same warm-up the client series
    drops comes off after that -- the first full-state burst is on the server's side of the link too. A
    later second with no peer, the teardown, is skipped for the same reason.

    **Every per-second total is divided by that second's `peers`** (see `per_peer` and `PER_PEER_EXEMPT`).
    The fleet joins one client at a time, so a run's seconds span every peer count from one up. A median of
    the raw totals moves with how many of those seconds the full fleet was seated, which is join timing
    rather than the send path.
    """
    def peers_of(row: dict[str, str]) -> float:
        # server.csv is folded out of a live log, so a repeated header or truncated row is ordinary, and
        # it must skip a row rather than abort the whole comparison.
        try:
            value = float(row.get("peers") or 0.0)
        except ValueError:
            return 0.0
        return value if math.isfinite(value) else 0.0

    pooled: dict[str, list[float]] = {}
    first_live = next((i for i, row in enumerate(rows) if peers_of(row) > 0.0), len(rows))
    for row in rows[first_live + int(warmup_s):]:
        peers = peers_of(row)
        if peers <= 0.0:
            continue
        for column, text in row.items():
            if text in (None, ""):
                continue
            try:
                value = float(text)
            except ValueError:
                continue
            if not math.isfinite(value):
                continue
            pooled.setdefault(column, []).append(value / peers if per_peer(column) else value)
    return pooled


def verdict(column: str, before: float, after: float, tolerance: float,
            lower: dict[str, str], higher: dict[str, str]) -> str:
    if column in UNJUDGED:
        return ""
    better_low = column in lower
    better_high = column in higher
    if not (better_low or better_high):
        return ""
    if math.isnan(before) or math.isnan(after):
        return ""
    # An absolute floor under the relative test: a column that sits at zero either side is unchanged, and one
    # whose absolute move is a rounding artifact is noise whatever the ratio says. See `NOISE_FLOOR_ABS` for
    # why the per-frame CPU timers carry a floor far above the 1e-9 default.
    floor = max(NOISE_FLOOR_ABS.get(column, 0.0), 1e-9)
    if abs(after - before) <= max(floor, abs(before) * tolerance):
        return "same"
    worse = after > before if better_low else after < before
    return "REGRESSED" if worse else "improved"


def classify(column: str, before_any: bool, after_any: bool, b50: float, a50: float,
             tolerance: float, lower: dict[str, str], higher: dict[str, str]) -> str:
    """The verdict for one column, including the two zero-baseline rules.

    Split out of `report` so `--self-test` can assert it: while these rules lived inline, the
    self-test claimed to cover them and asserted something else.
    """
    # A COLUMN NOTHING POPULATED ON EITHER SIDE IS NOT EVIDENCE OF "UNCHANGED". Every send-path
    # column reads zero in a client CSV, and `server.csv` carries only the handful the debug wire
    # line prints -- so a dozen judged columns sit at 0.000 both sides in a client-only comparison.
    # Printing those as `same` is how a run that measured nothing reads as a run that found nothing.
    if not before_any and not after_any:
        return "not measured"
    if column in FAULT_COUNTERS and b50 == 0.0:
        # A FAULT COUNTER LEAVING ZERO IS THE REGRESSION, not an unmeasured baseline -- and the
        # figure that leaves zero is the MEDIAN, not any single sample. One spawn-in-flight skip in
        # an otherwise clean run is the documented everyday case, and a baseline with one stray
        # nonzero sample still measured a healthy run: judging either on `any()` failed the gate on
        # one-sample noise, printing REGRESSED beside a delta of n/a. Sustained faulting is a p50
        # off zero.
        if a50 > 0.0:
            return "REGRESSED"
        return "same" if after_any or before_any else "not measured"
    if not before_any:
        if column in UNJUDGED:
            return ""
        # THE BASELINE MEASURED NOTHING, so there is no ratio to test a tolerance against. Judged as
        # a percentage it is REGRESSED for any non-zero reading at all, however small -- which is what
        # `rollback_ms` did across a release boundary whose baseline never resimulated a single tick.
        return "new (was 0)"
    return verdict(column, b50, a50, tolerance, lower, higher)


def report(title: str, before: dict[str, list[float]], after: dict[str, list[float]],
           order: list[str], tolerance: float,
           lower: dict[str, str], higher: dict[str, str]) -> int:
    columns = [c for c in before if c in after]
    columns.sort(key=lambda c: (order.index(c) if c in order else len(order), c))
    # A judged column present in only one run is schema drift across the pair, and dropping it from
    # the table read as "compared and fine" -- the highest-signal counters are exactly the ones a
    # rename would silently retire.
    lost = sorted(c for c in (set(before) ^ set(after))
                  if (c in lower or c in higher) and c not in UNJUDGED)
    print(title)
    header = (f"{'column':<24} {'p50 before':>12} {'p50 after':>12} {'delta':>10} "
              f"{'p95 before':>12} {'p95 after':>12}  verdict")
    print(header)
    print("-" * len(header))
    regressions = 0
    for column in columns:
        b50, a50 = percentile(before[column], 0.50), percentile(after[column], 0.50)
        b95, a95 = percentile(before[column], 0.95), percentile(after[column], 0.95)
        # A COLUMN NOTHING POPULATED ON EITHER SIDE IS NOT EVIDENCE OF "UNCHANGED". Every send-path
        # column reads zero in a client CSV, and `server.csv` carries only the handful the debug wire
        # line prints -- so a dozen judged columns sit at 0.000 both sides in a client-only comparison.
        # Printing those as `same` is how a run that measured nothing reads as a run that found nothing.
        call = classify(column, any(before[column]), any(after[column]), b50, a50,
                        tolerance, lower, higher)
        if call == "REGRESSED":
            regressions += 1
        delta = "n/a" if b50 == 0.0 else f"{(a50 - b50) / abs(b50) * 100.0:+.1f}%"
        print(f"{column:<24} {b50:>12.3f} {a50:>12.3f} {delta:>10} "
              f"{b95:>12.3f} {a95:>12.3f}  {call}")
    for column in lost:
        side = "after" if column in after else "before"
        print(f"{column:<24} judged column only in the {side} run: not comparable  REGRESSED")
        regressions += 1
    print()
    return regressions


# Every case here is one that was decided WRONGLY at some point, so each row is a rule that has already
# failed once. `--self-test` runs them; there is no test framework under `tools/`, and the alternative to a
# flag was leaving the judgement of every performance claim unchecked.
SELF_TEST_CASES = [
    # (column, before, after, expected verdict, why it is here)
    ("rollback_ms", 0.023, 0.026, "same",
     "two runs of ONE binary moved this far; judging it failed a re-run against itself"),
    ("net_ms", 0.020, 0.022, "same", "the same, on the other per-frame timer"),
    ("rollback_ms", 0.023, 0.200, "REGRESSED", "a 9x blowup is past the floor and must still be caught"),
    ("rollback_ms", 2.000, 2.400, "REGRESSED", "the floor must not blind the column once it matters"),
    ("tx_bytes_s", 3389.0, 3410.0, "same", "a byte counter is exact and carries no floor"),
    ("tx_bytes_s", 3389.0, 4000.0, "REGRESSED", "and a real byte regression still reads as one"),
    ("blocks_admitted_s", 900.0, 500.0, "REGRESSED", "higher-is-better reads a DROP as the regression"),
    ("blocks_admitted_s", 900.0, 1200.0, "improved", "and a rise as the improvement"),
    ("resim_ticks", 1.0, 8.0, "", "unjudged: depth deepens legitimately under latency"),
]


# The zero-baseline rules, which decide whether a column is judged at all.
# (column, baseline populated, after populated, expected verdict, why it is here)
ZERO_BASELINE_CASES = [
    ("rollback_ms", False, False, "not measured",
     "nothing populated either side is not evidence of unchanged"),
    ("rollback_ms", False, True, "new (was 0)",
     "a capability the baseline never exercised has no ratio to judge"),
    ("want_full_nacks_s", False, True, "REGRESSED",
     "a fault counter leaving zero IS the regression, not a missing measurement"),
    ("reconcile_snap", False, True, "REGRESSED", "the same, on prediction"),
    ("rx_rejected_s", False, True, "REGRESSED", "and on the server's refusals"),
    ("resim_ticks", False, True, "", "an unjudged column stays unjudged"),
]

# The fault-counter median rule: (column, b50, a50, before populated, after populated, expected).
FAULT_MEDIAN_CASES = [
    ("rx_skipped_s", 0.0, 0.0, False, True, "same",
     "one stray after-sample is one-sample noise, not sustained faulting"),
    ("rx_skipped_s", 0.0, 0.0, True, True, "same",
     "a stray sample either side of a clean pair is still a clean pair"),
    ("rx_skipped_s", 0.0, 1.0, True, True, "REGRESSED",
     "a sporadic baseline does not excuse a median that left zero"),
    ("want_full_nacks_s", 0.0, 2.0, False, True, "REGRESSED",
     "sustained faulting off a clean baseline fails whatever any() says"),
]


# The server-row rules: (name, rows, column, expected median, why it is here). `rows` is a list of
# `(peers, tx_bytes_s, tx_peak_peer_bytes_s)` seconds, warm-up 0.
def _server_rows(seconds: list[tuple[float, float, float]]) -> list[dict[str, str]]:
    return [{"peers": f"{p:g}", "tx_bytes_s": f"{t:g}", "tx_peak_peer_bytes_s": f"{k:g}"}
            for p, t, k in seconds]


_FAST_RAMP = [(1, 1000, 1000), (2, 2000, 1000), (3, 3000, 1000)] + [(4, 4000, 1000)] * 20
_SLOW_RAMP = [(1, 1000, 1000)] * 6 + [(2, 2000, 1000)] * 6 + [(3, 3000, 1000)] * 6 + [(4, 4000, 1000)] * 5

SERVER_ROW_CASES = [
    ("fast ramp", _server_rows(_FAST_RAMP), "tx_bytes_s", 1000.0,
     "a total is judged per peer, so a run's join timing does not move it"),
    ("slow ramp", _server_rows(_SLOW_RAMP), "tx_bytes_s", 1000.0,
     "the same egress per peer behind a ramp that left the full fleet seated for 5 seconds of 23"),
    ("peak peer", _server_rows(_FAST_RAMP), "tx_peak_peer_bytes_s", 1000.0,
     "a column that already describes one peer is not divided again"),
    ("teardown", _server_rows([(0, 0, 0)] + [(2, 2000, 1000)] * 3 + [(0, 0, 0)] * 9), "tx_bytes_s", 1000.0,
     "a second with no peer after the fleet left is teardown, and is skipped like bringup"),
]


def tables_for(column: str):
    """The direction tables the column is judged against, or None if no table claims it."""
    for lower, higher in ((LOWER_IS_BETTER, HIGHER_IS_BETTER), (SERVER_LOWER_IS_BETTER, {})):
        if column in lower or column in higher:
            return lower, higher
    return (LOWER_IS_BETTER, HIGHER_IS_BETTER) if column in UNJUDGED else None


def self_test() -> int:
    """Assert the verdict rules. Returns a process exit code."""
    failures = 0
    checked = 0

    for column, before, after, expected in ((c, b, a, e) for c, b, a, e, _ in SELF_TEST_CASES):
        tables = tables_for(column)
        if tables is None:
            # A case naming a column no table judges would otherwise be skipped in silence while
            # still counting toward the total printed below.
            print(f"FAIL {column}: no direction table claims this column")
            failures += 1
            continue
        got = verdict(column, before, after, 0.05, *tables)
        checked += 1
        if got != expected:
            print(f"FAIL {column} {before} -> {after}: expected {expected!r}, got {got!r}")
            failures += 1

    for column, before_any, after_any, expected, _ in ZERO_BASELINE_CASES:
        tables = tables_for(column)
        if tables is None:
            print(f"FAIL {column}: no direction table claims this column")
            failures += 1
            continue
        got = classify(column, before_any, after_any, 0.0, 1.0, 0.05, *tables)
        checked += 1
        if got != expected:
            print(f"FAIL {column} baseline={before_any} after={after_any}: "
                  f"expected {expected!r}, got {got!r}")
            failures += 1

    for column, b50, a50, before_any, after_any, expected, _ in FAULT_MEDIAN_CASES:
        tables = tables_for(column)
        if tables is None:
            print(f"FAIL {column}: no direction table claims this column")
            failures += 1
            continue
        got = classify(column, before_any, after_any, b50, a50, 0.05, *tables)
        checked += 1
        if got != expected:
            print(f"FAIL {column} b50={b50} a50={a50}: expected {expected!r}, got {got!r}")
            failures += 1

    for name, rows, column, expected, _ in SERVER_ROW_CASES:
        got = percentile(server_columns(rows, 0.0).get(column, []), 0.50)
        checked += 1
        if not math.isclose(got, expected):
            print(f"FAIL server rows, {name}: {column} median expected {expected}, got {got}")
            failures += 1
    # The two ramps above are one send path measured twice, so the comparison must call them equal.
    fast = percentile(server_columns(_server_rows(_FAST_RAMP), 0.0)["tx_bytes_s"], 0.50)
    slow = percentile(server_columns(_server_rows(_SLOW_RAMP), 0.0)["tx_bytes_s"], 0.50)
    got = verdict("tx_bytes_s", fast, slow, 0.05, SERVER_LOWER_IS_BETTER, {})
    checked += 1
    if got != "same":
        print(f"FAIL server rows: two ramps of one send path read {got!r}, expected 'same'")
        failures += 1

    print(f"compare.py self-test: {checked} cases, {failures} failure(s)")
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Diff two netbench runs.")
    parser.add_argument("before", nargs="?", help="artifact directory of the run before the change")
    parser.add_argument("after", nargs="?", help="artifact directory of the run after it")
    parser.add_argument("--self-test", action="store_true",
                        help="assert the verdict rules and exit, reading no artifacts")
    parser.add_argument("--warmup", type=float, default=3.0,
                        help="seconds of each client's series to drop (default 3)")
    parser.add_argument("--tolerance", type=float, default=0.05,
                        help="fractional move below which a column reads as unchanged (default 0.05)")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.before or not args.after:
        parser.error("before and after are required unless --self-test is given")

    print(f"netbench compare: {args.before}  ->  {args.after}")
    print(f"  warm-up dropped: {args.warmup:.1f}s per client    tolerance: {args.tolerance * 100:.0f}%")
    print()

    regressions = report(
        "== CLIENT FLEET (pooled per-tick rows) ==",
        load_run(args.before, args.warmup), load_run(args.after, args.warmup),
        list(LOWER_IS_BETTER) + list(HIGHER_IS_BETTER), args.tolerance,
        LOWER_IS_BETTER, HIGHER_IS_BETTER)

    server_before = load_server(args.before, args.warmup)
    server_after = load_server(args.after, args.warmup)
    if server_before and server_after:
        regressions += report(
            "== SERVER (per-second wire lines; each *_s total divided by that second's synced peers) ==",
            server_before, server_after,
            list(SERVER_LOWER_IS_BETTER), args.tolerance,
            SERVER_LOWER_IS_BETTER, {})
    elif server_before or server_after:
        # One run has live server rows and the other has none -- a missing server.csv, or a server
        # that never saw a peer. Skipping quietly read as "compared and fine" on the table
        # docs/netbench.md calls the point of a send-path change.
        empty = "after" if server_before else "before"
        print(f"== SERVER == the {empty} run has no live server rows; the send path was NOT compared.  REGRESSED")
        print()
        regressions += 1
    else:
        print("== SERVER == no live server rows in either run; the send path was not compared.")
        print()

    if regressions == 0:
        print("netbench compare: no judged column regressed past the tolerance.")
        return 0
    print(f"netbench compare: {regressions} judged column(s) REGRESSED past the tolerance.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
