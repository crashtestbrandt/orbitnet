#!/usr/bin/env python3
"""Assert the server's steady-state `want_full_nacks_s` per profile, from the rows `bench.sh` folds into server.csv.

    tools/netbench/nack_gate.py <server.csv> --profile NAME
    tools/netbench/nack_gate.py --self-test

`want_full_nacks_s` is the acceptance bar for interest management: a re-entering entity must get its full block
without a `want_full` storm. A client reads it as a structural 0.00, so the server's own per-second window is the
only place it is measured.

**The steady state** is every window with a peer in it, except a window in which a peer joined and the window
after it.

- The fleet joins one client at a time, a few seconds apart. Every join costs one keyframe interval of NACKs per
  affected channel: a peer that receives a block for an entity it has not registered yet drops it and asks,
  and every peer already seated meets the joiner's entities the same way. That residual is bounded and correct.
- The rule this replaced dropped the first window only. In nine nightly runs the join ramp took four to nine
  windows, so the maximum it reported was join residual, up to 6.00/s, in seven of the nine.
- The keyframe interval is 16 ticks, at most one second at any rate the demos run, so the join window and the
  one after it cover it. A join is a window whose `peers` is above the window before it.
- A large fleet on a slow host is never seated all at once: each client measures for its own window from when
  it joined, so the first ones leave while the last ones join. Measured with 12 clients on 4 cores, the server
  peaked at 10 peers. Leaving costs nothing, so a leave does not end the steady state.

**The figure** is NACKs per peer-second: the steady windows' `want_full_nacks_s` summed, over their `peers`
summed. A storm is per peer: a client holding a block it cannot place asks on every input frame until a full
block arrives. Dividing by the peer count lets one threshold serve any fleet size.

**A threshold** is derived from a recorded series. `SERIES` records, per profile, the worst run of a measured
series and where the series came from. The threshold is `max(FLOOR, MARGIN * worst)`:

- `FLOOR` is 0.25 per peer-second, the gate for a profile whose series reads near zero. It is ten times the worst
  run on `clean` and `lan` (0.024), and six times below the quietest failing night a downstream bench recorded
  before this gate existed (5.98/s across four peers, about 1.5 per peer-second).
- `MARGIN` is 4. Under 5 to 10% loss a base genuinely goes missing and a NACK is the right answer, so those
  profiles are gated on a storm rather than on the number their series happened to see. A client forced to raise
  `want_full` on every input frame measured 24.8 per peer-second on `congested_wifi`, twelve times the highest
  threshold here.
- A profile with no series is reported and not gated, and the line says so. Add a row once a series exists: at
  least three runs, and say where they ran.

Exit status: 0 for a pass or an ungated profile, 1 for a gated profile that failed or measured no steady window.
Standard library only; `--self-test` reads no artifacts.
"""

from __future__ import annotations

import argparse
import csv
import math
import sys


# Per peer-second. See the module docstring.
FLOOR = 0.25
MARGIN = 4.0

# profile -> (worst run of the series in NACKs per peer-second, runs in the series, where they ran)
#
# Every series below: 3 runs on a 4-core Linux VM, 4 clients, arena, strafe_fire, seeds 1 to 3, 25 s, on a build
# with the admission cursor stepping once per block. The nightly's runs before that fix are not a series: their
# servers panicked about 700 times a run and dropped the rest of each panicking frame. Replace a row with a
# longer series when one exists.
_VM = "3 runs on a 4-core Linux VM, 4 clients, arena, strafe_fire, seeds 1-3, 25 s"
SERIES: dict[str, tuple[float, int, str]] = {
    "clean": (0.024, 3, _VM),
    "lan": (0.023, 3, _VM),
    "broadband": (0.000, 3, _VM),
    "congested_wifi": (0.062, 3, _VM),
    "relayed": (0.000, 3, _VM),
    "mobile_4g": (0.074, 3, _VM),
    "cross_region": (0.000, 3, _VM),
    "worst_case": (0.401, 3, _VM),
    "worst_case_burst": (0.261, 3, _VM),
    "mobile_3g": (0.516, 3, _VM),
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


def steady_windows(rows: list[dict[str, str]]) -> list[tuple[float, float]]:
    """`(want_full_nacks_s, peers)` for every steady-state window, in order.

    A window is steady when it has a peer, has both figures, and neither it nor the window before it is a join.
    A window whose `peers` is unreadable counts as a join, so it and the one after it are dropped.
    """
    parsed: list[tuple[float | None, float | None]] = [
        (number(row.get("want_full_nacks_s")), number(row.get("peers"))) for row in rows
    ]
    joined: list[bool] = []
    before = 0.0
    for _, peers in parsed:
        joined.append(peers is None or peers > before + 1e-6)
        before = peers if peers is not None else before
    steady: list[tuple[float, float]] = []
    for index, (nacks, peers) in enumerate(parsed):
        if nacks is None or peers is None or peers <= 0:
            continue
        if joined[index] or (index > 0 and joined[index - 1]):
            continue
        steady.append((nacks, peers))
    return steady


def evaluate(rows: list[dict[str, str]], profile: str) -> tuple[bool, str]:
    """`(passed, line)`. The line always contains `want_full nacks`, which the nightly's summary greps for."""
    steady = steady_windows(rows)
    gate = threshold(profile)
    if not steady:
        line = ("want_full nacks: NO STEADY-STATE WINDOW (the server published no window with a peer that was "
                "not a join or the window after one)")
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


# The ramp, the steady run and the teardown are copied from nightly server.csv files, and the churn from a
# 12-client run on 4 cores, so the rule is tested on the shapes that broke the old ones.
RAMP_4 = [0, 0, 1, 2, 2]
STEADY_4 = [4] * 22
TEARDOWN_4 = [3, 2, 2]
CHURN_12 = [0, 0, 1, 1, 2, 2, 3, 4, 4, 5, 5, 6, 6, 6, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 10, 10, 10, 9, 10, 9, 9, 9,
            8, 8, 7, 7, 6, 6, 6, 5, 5, 5, 5, 4, 4, 4, 3, 3, 2, 2, 2, 2, 2, 1, 1, 1]
CHURN_12_NACKS = [0, 0, 0, 0, 0, 0, 3.98, 1.00, 1.99, 0, 2.97, 0, 3.89, 1.00, 0, 0, 0, 0, 2.98, 0.98, 0, 1.99,
                  0, 1.96, 3.97, 0, 0, 0.99, 4.86, 2.98, 0, 1.98, 1.00] + [0] * 23

SELF_TEST_CASES = [
    # (name, peers, nacks, profile, expected pass, text the line must contain)
    ("a join burst of 6.00/s in the window the fleet completed is not steady state",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 2, 0, 6] + [0] * 21 + [0, 0, 0], "congested_wifi", True, "PASS"),
    ("and neither is the window after it",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 2, 0, 2, 4] + [0] * 20 + [0, 0, 0], "congested_wifi", True, "worst window 0.00/s"),
    ("a slow ramp: the last client joined in window nine",
     [0, 0, 0, 0, 1, 1, 2, 2, 2] + [4] * 39 + [3, 3, 2, 2, 2, 1],
     [0] * 6 + [1.97, 0, 0, 2.96, 3.91] + [0] * 43, "congested_wifi", True, "worst window 0.00/s"),
    ("a fleet that is never seated all at once still has a steady state, between its joins",
     CHURN_12, CHURN_12_NACKS, "congested_wifi", True, "PASS"),
    ("one stray NACK in the steady state passes",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 1, 0, 5] + [0] * 10 + [1] + [0] * 10 + [0, 0, 0], "congested_wifi", True, "PASS"),
    ("a sustained 2/s across four peers is a storm",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [2] * 20 + [0, 0, 0], "congested_wifi", False, "FAIL"),
    ("one peer stuck asking on every input frame is a storm",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [30] * 20 + [0, 0, 0], "congested_wifi", False, "FAIL"),
    ("a storm on a heavy-loss profile still fails its higher gate",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [40] * 20 + [0, 0, 0], "mobile_3g", False, "FAIL"),
    ("a heavy-loss profile's own residual passes its gate",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [6, 0, 0, 0] * 5 + [0, 0, 0], "worst_case", True, "PASS"),
    ("a peer that drops and rejoins costs its rejoin, not the run",
     RAMP_4 + [4] * 10 + [3, 4] + [4] * 10 + TEARDOWN_4,
     [0] * 15 + [0, 6, 3] + [0] * 12, "congested_wifi", True, "worst window 0.00/s"),
    ("nobody connected: nothing was measured, and a gated profile fails on it",
     [0, 0, 0, 0], [0, 0, 0, 0], "congested_wifi", False, "NO STEADY-STATE WINDOW"),
    ("an ungated profile reports and passes",
     RAMP_4 + STEADY_4 + TEARDOWN_4,
     [0, 0, 0, 0, 0, 0, 0] + [30] * 20 + [0, 0, 0], "torture", True, "reported, not gated"),
    ("an ungated profile with no steady window reports and passes",
     [0, 1], [0, 0], "torture", True, "reported, not gated"),
    ("the figure is per peer: 1/s across two peers is 0.5 per peer-second",
     [0, 1, 2] + [2] * 20,
     [0, 0, 0] + [1] * 20, "congested_wifi", False, "0.500/peer/s"),
]


def self_test() -> int:
    """Assert the window rule and the verdicts. Returns a process exit code."""
    failures = 0
    for name, peers, nacks, profile, expected, needle in SELF_TEST_CASES:
        if len(peers) != len(nacks):
            print(f"FAIL {name}: the case has {len(peers)} peer counts and {len(nacks)} NACK rates")
            failures += 1
            continue
        passed, line = evaluate(window_rows(peers, nacks), profile)
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
    parser.add_argument("--profile", help="the NetProfiles catalog name the relay ran")
    parser.add_argument("--self-test", action="store_true",
                        help="assert the window rule and the verdicts and exit, reading no artifacts")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.server_csv or not args.profile:
        parser.error("server_csv and --profile are required unless --self-test is given")
    try:
        with open(args.server_csv, newline="") as handle:
            rows = list(csv.DictReader(handle))
    except OSError:
        rows = []
    passed, line = evaluate(rows, args.profile)
    print(f"  {line}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
