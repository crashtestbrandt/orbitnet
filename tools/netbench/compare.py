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

Only columns whose DIRECTION is known are judged. `rtt_ms` is set by the profile rather than by the netcode and
`interest_entities` is a scene fact; both are printed and neither is judged.

A column that is ZERO ON BOTH SIDES reads `not measured` rather than `same`. Most send-path columns are
collected on the server and appear in no client CSV, so in a client-only comparison they are absent rather
than unchanged -- and `same` on a dozen of them reads as a send path that was compared and found equal.

A column zero on the BASELINE ONLY reads `new (was 0)`, and does not count as a regression. The ratio has
no denominator, which is why the delta column prints `n/a` -- judging it anyway meant one row printed `n/a`
and `REGRESSED` side by side, and the exit code followed the second. It is still worth a look, so it prints
under its own verdict rather than as `not measured`: comparing across a release boundary, this is how a
capability the baseline never exercised appears, and also how a fault counter first rises.
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
SERVER_LOWER_IS_BETTER = {
    "tx_bytes_s": "server egress: snapshot payload bytes per second, across every peer",
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
    """The server's own per-second rows, or an empty map when the run carries no server.csv.

    THE SERVER IS UP BEFORE THE FLEET IS. Its first rows are the seconds between binding the port and the
    first client arriving: `peers` is 0 and every wire column with it. Pooling those makes each figure a
    function of how long bringup happened to take, which is the one thing a comparison must not depend on.
    So the series starts at the first second the server had a peer, and the same warm-up the client series
    drops comes off after that -- the first full-state burst is on the server's side of the link too.
    """
    path = os.path.join(directory, SERVER_CSV)
    pooled: dict[str, list[float]] = {}
    if not os.path.exists(path):
        return pooled
    with open(path, newline="") as handle:
        rows = list(csv.DictReader(handle))
    first_live = next((i for i, row in enumerate(rows) if float(row.get("peers") or 0.0) > 0.0), len(rows))
    for row in rows[first_live + int(warmup_s):]:
        for column, text in row.items():
            if text in (None, ""):
                continue
            try:
                value = float(text)
            except ValueError:
                continue
            pooled.setdefault(column, []).append(value)
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
    # whose absolute move is a rounding artifact is noise whatever the ratio says.
    if abs(after - before) <= max(1e-9, abs(before) * tolerance):
        return "same"
    worse = after > before if better_low else after < before
    return "REGRESSED" if worse else "improved"


def report(title: str, before: dict[str, list[float]], after: dict[str, list[float]],
           order: list[str], tolerance: float,
           lower: dict[str, str], higher: dict[str, str]) -> int:
    columns = [c for c in before if c in after]
    columns.sort(key=lambda c: (order.index(c) if c in order else len(order), c))
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
        if not any(before[column]) and not any(after[column]):
            call = "not measured"
        elif not any(before[column]):
            # THE BASELINE MEASURED NOTHING, so there is no ratio to test a tolerance against. Judged
            # as a percentage it is REGRESSED for any non-zero reading at all, however small -- which
            # is what `rollback_ms` did across a release boundary whose baseline never resimulated a
            # single tick. Named rather than hidden, because the same shape is a fault counter
            # leaving zero for the first time.
            call = "new (was 0)" if column not in UNJUDGED else ""
        else:
            call = verdict(column, b50, a50, tolerance, lower, higher)
        if call == "REGRESSED":
            regressions += 1
        delta = "n/a" if b50 == 0.0 else f"{(a50 - b50) / abs(b50) * 100.0:+.1f}%"
        print(f"{column:<24} {b50:>12.3f} {a50:>12.3f} {delta:>10} "
              f"{b95:>12.3f} {a95:>12.3f}  {call}")
    print()
    return regressions


def main() -> int:
    parser = argparse.ArgumentParser(description="Diff two netbench runs.")
    parser.add_argument("before", help="artifact directory of the run before the change")
    parser.add_argument("after", help="artifact directory of the run after it")
    parser.add_argument("--warmup", type=float, default=3.0,
                        help="seconds of each client's series to drop (default 3)")
    parser.add_argument("--tolerance", type=float, default=0.05,
                        help="fractional move below which a column reads as unchanged (default 0.05)")
    args = parser.parse_args()

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
            "== SERVER (per-second wire lines) ==",
            server_before, server_after,
            list(SERVER_LOWER_IS_BETTER), args.tolerance,
            SERVER_LOWER_IS_BETTER, {})
    else:
        print("== SERVER == no server.csv in one or both runs; the send path was not compared.")
        print()

    if regressions == 0:
        print("netbench compare: no judged column regressed past the tolerance.")
        return 0
    print(f"netbench compare: {regressions} judged column(s) REGRESSED past the tolerance.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
