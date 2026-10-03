#!/usr/bin/env python3
"""Assert the server's steady-state `want_full_nacks_s` per profile, from the rows `bench.sh` folds into server.csv.

    tools/netbench/nack_gate.py <server.csv> --clients N --profile NAME
    tools/netbench/nack_gate.py --self-test

`want_full_nacks_s` is the acceptance bar for interest management: a re-entering entity must get its full block
without a `want_full` storm. A client reads it as a structural 0.00, so the server's own per-second window is the
only place it is measured.

**THE STEADY STATE IS WHERE THE WHOLE FLEET IS SEATED, AND TWO WINDOWS AFTER IT GOT THERE.**

- The fleet joins one client at a time, a few seconds apart. Every join costs one keyframe interval of NACKs per
  affected channel: a peer that receives a block for an entity it has not registered yet drops it and asks.
  That residual is bounded and correct.
- The rule this replaced dropped the first window only. In nine nightly runs the join ramp took four to nine
  windows, so the maximum it reported was join residual, up to 6.00/s, in seven of the nine.
- The keyframe interval is 16 ticks, at most one second at any rate the demos run. The window the last client
  joined in plus the one after it cover it. `SETTLE_WINDOWS` is that two.
- Windows after the fleet starts leaving are teardown and are dropped too.

**THE FIGURE IS NACKs PER PEER-SECOND**: the steady windows' `want_full_nacks_s` summed, over their `peers`
summed. A storm is per peer: a client holding a block it cannot place asks on every input frame until a full
block arrives. Dividing by the peer count lets one threshold serve any fleet size.

**A THRESHOLD IS DERIVED FROM A RECORDED SERIES, NEVER CHOSEN.** `SERIES` records, per profile, the worst run of
a measured series and where the series came from. The threshold is `max(FLOOR, MARGIN * worst)`:

- `FLOOR` is 0.25 per peer-second. It is twenty times the worst nightly run on `congested_wifi`, and six times
  below the quietest failing night a downstream bench recorded before this gate existed (about 1.5 per
  peer-second).
- `MARGIN` is 4, so a profile that legitimately NACKs under heavy loss is gated on a storm rather than on the
  number its series happened to see.
- A profile with no series is REPORTED, NOT GATED, and the line says so. Add a row once a series exists: at
  least three runs, and say where they ran.

Exit status: 0 for a pass or an ungated profile, 1 for a gated profile that failed or measured no steady window.
Standard library only; `--self-test` reads no artifacts.
"""

from __future__ import annotations

import argparse
import csv
import math
import sys

# Windows dropped after the fleet is complete: the one the last client joined in, and the one after it.
SETTLE_WINDOWS = 2

# Per peer-second. See the module docstring.
FLOOR = 0.25
MARGIN = 4.0

# profile -> (worst run of the series in NACKs per peer-second, runs in the series, where they ran)
SERIES: dict[str, tuple[float, int, str]] = {
    "congested_wifi": (0.013, 9, "the nightly on quasitop, 4 clients, arena, strafe_fire, seed 1, 25 s"),
}


def threshold(profile: str) -> float | None:
    """The gate for a profile, or None when it has no recorded series."""
    entry = SERIES.get(profile)
    if entry is None:
        return None
    return max(FLOOR, MARGIN * entry[0])


def number(text: str | None) -> float | None:
    """A CSV cell as a float, or None for an empty or malformed one. server.csv is folded out of a live log."""
    if text in (None, ""):
        return None
    try:
        value = float(text)
    except ValueError:
        return None
    return value if math.isfinite(value) else None


def steady_windows(rows: list[dict[str, str]], clients: int) -> list[tuple[float, float]]:
    """`(want_full_nacks_s, peers)` for every steady-state window, in order.

    Steady means the whole fleet is seated: from `SETTLE_WINDOWS` after the first window with every client
    connected, to the last such window, skipping any window in between that is short of the fleet.
    """
    parsed: list[tuple[float | None, float | None]] = [
        (number(row.get("want_full_nacks_s")), number(row.get("peers"))) for row in rows
    ]
    full = [i for i, (_, peers) in enumerate(parsed) if peers is not None and peers >= clients - 1e-6]
    if not full:
        return []
    first, last = full[0] + SETTLE_WINDOWS, full[-1]
    steady: list[tuple[float, float]] = []
    for nacks, peers in parsed[first:last + 1]:
        if nacks is None or peers is None or peers < clients - 1e-6:
            continue
        steady.append((nacks, peers))
    return steady


def evaluate(rows: list[dict[str, str]], clients: int, profile: str) -> tuple[bool, str]:
    """`(passed, line)`. The line always contains `want_full nacks`, which the nightly's summary greps for."""
    steady = steady_windows(rows, clients)
    gate = threshold(profile)
    if not steady:
        line = (f"want_full nacks: NO STEADY-STATE WINDOW (the server never published a window past the join "
                f"with all {clients} client(s) seated)")
        if gate is None:
            return True, f"{line} -- reported, not gated (no recorded series for profile '{profile}')"
        return False, f"{line} -- FAIL: profile '{profile}' is gated and nothing was measured"
    total = sum(nacks for nacks, _ in steady)
    peer_seconds = sum(peers for _, peers in steady)
    rate = total / peer_seconds if peer_seconds > 0 else 0.0
    worst = max(nacks for nacks, _ in steady)
    figure = (f"want_full nacks {rate:.3f}/peer/s over {len(steady)} steady-state window(s), "
              f"worst window {worst:.2f}/s")
    if gate is None:
        return True, f"{figure} -- reported, not gated (no recorded series for profile '{profile}')"
    if rate <= gate:
        return True, f"{figure} -- PASS (<= {gate:.3f}/peer/s on '{profile}')"
    return False, f"{figure} -- FAIL (> {gate:.3f}/peer/s on '{profile}')"


def window_rows(peers: list[float], nacks: list[float]) -> list[dict[str, str]]:
    return [{"peers": f"{p:.2f}", "want_full_nacks_s": f"{n:.2f}"} for p, n in zip(peers, nacks)]


# The ramp, the steady run and the teardown are copied from nightly server.csv files, so the rule is tested on
# the shape that broke the old one.
RAMP_4 = [0, 0, 1, 2, 2]
STEADY_4 = [4] * 22
TEARDOWN_4 = [3, 2, 2]

SELF_TEST_CASES = [
    # (name, peers, nacks, clients, profile, expected pass, text the line must contain)
    ("a join burst of 6.00/s in the window the fleet completed is not steady state",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 2, 0, 6] + [0] * 21 + [0, 0, 0], 4, "congested_wifi", True, "PASS"),
    ("and neither is the window after it",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 2, 0, 2, 4] + [0] * 20 + [0, 0, 0], 4, "congested_wifi", True, "worst window 0.00/s"),
    ("a slow ramp: the last client joined in window nine",
     [0, 0, 0, 0, 1, 1, 2, 2, 2] + [4] * 39 + [3, 3, 2, 2, 2, 1],
     [0] * 6 + [1.97, 0, 0, 2.96, 3.91] + [0] * 43, 4, "congested_wifi", True, "worst window 0.00/s"),
    ("one stray NACK in the steady state passes",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 1, 0, 5] + [0] * 10 + [1] + [0] * 10 + [0, 0, 0], 4, "congested_wifi", True, "PASS"),
    ("a sustained 2/s across four peers is a storm",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [2] * 20 + [0, 0, 0], 4, "congested_wifi", False, "FAIL"),
    ("one peer stuck asking on every input frame is a storm",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [30] * 20 + [0, 0, 0], 4, "congested_wifi", False, "FAIL"),
    ("teardown windows are not steady state",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0] * 27 + [9, 9, 9], 4, "congested_wifi", True, "PASS"),
    ("a fleet that never completed measured nothing, and a gated profile fails on it",
     [0, 1, 2, 3, 3, 3], [0, 0, 0, 0, 0, 0], 4, "congested_wifi", False, "NO STEADY-STATE WINDOW"),
    ("an ungated profile reports and passes",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [30] * 20 + [0, 0, 0], 4, "torture", True, "reported, not gated"),
    ("an ungated profile with no steady window reports and passes",
     [0, 1], [0, 0], 4, "torture", True, "reported, not gated"),
    ("a window short of the fleet in the middle of the run is skipped, not counted",
     RAMP_4 + [4] * 10 + [3] + [4] * 11 + TEARDOWN_4,
     [0] * 15 + [40] + [0] * 14, 4, "congested_wifi", True, "PASS"),
    ("the figure is per peer: 1/s across two peers is 0.5 per peer-second",
     [0, 1, 2] + [2] * 20 + [1],
     [0, 0, 0] + [1] * 20 + [0], 2, "congested_wifi", False, "0.500/peer/s"),
]


def self_test() -> int:
    """Assert the window rule and the verdicts. Returns a process exit code."""
    failures = 0
    for name, peers, nacks, clients, profile, expected, needle in SELF_TEST_CASES:
        if len(peers) != len(nacks):
            print(f"FAIL {name}: the case has {len(peers)} peer counts and {len(nacks)} NACK rates")
            failures += 1
            continue
        passed, line = evaluate(window_rows(peers, nacks), clients, profile)
        if passed != expected or needle not in line or "want_full nacks" not in line:
            print(f"FAIL {name}: expected {'pass' if expected else 'fail'} with '{needle}', got: {line}")
            failures += 1
    if threshold("congested_wifi") is None:
        print("FAIL the nightly's profile has no threshold")
        failures += 1
    if threshold("no_such_profile") is not None:
        print("FAIL a profile with no series was given a threshold")
        failures += 1
    for profile, (worst, runs, source) in SERIES.items():
        if runs < 3 or not source or not math.isfinite(worst) or worst < 0:
            print(f"FAIL {profile}: a series needs three runs, a source and a finite worst run")
            failures += 1
    print(f"nack_gate.py self-test: {len(SELF_TEST_CASES)} cases, {failures} failure(s)")
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Assert the server's steady-state want_full NACK rate.")
    parser.add_argument("server_csv", nargs="?", help="the server.csv bench.sh folded out of the server log")
    parser.add_argument("--clients", type=int, help="the fleet size the bench launched")
    parser.add_argument("--profile", help="the NetProfiles catalog name the relay ran")
    parser.add_argument("--self-test", action="store_true",
                        help="assert the window rule and the verdicts and exit, reading no artifacts")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.server_csv or args.clients is None or not args.profile:
        parser.error("server_csv, --clients and --profile are required unless --self-test is given")
    try:
        with open(args.server_csv, newline="") as handle:
            rows = list(csv.DictReader(handle))
    except OSError:
        rows = []
    passed, line = evaluate(rows, args.clients, args.profile)
    print(f"  {line}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
