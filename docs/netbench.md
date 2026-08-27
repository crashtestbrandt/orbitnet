# netbench

Test netcode under real-world network conditions without gathering real players.

```sh
just netbench 4 congested_wifi   # 4 bots through a conditioned link, tick-domain gates
just netbench 4 clean            # the control
just netbench 4 worst_case       # the 250 ms design ceiling
```

A run launches a dedicated server, a UDP impairment relay, and N headless bot clients that join **through**
the relay. Each bot drives the real input path, streams per-tick metrics to a CSV, and self-evaluates a gate.
Exit code is the verdict.

**It drives a demo project.** The repository root is not a Godot project — OrbitNet is configured through an
`[orbitnet]` block in `project.godot` and the demos disagree about those values on purpose — so every launch
names `demos/<DEMO>`. The default is `arena`: decoupled at 30 Hz with a 128-tick ring, which is the
configuration closest to a shooter, and the only one that fills the hit-registration columns.

```sh
just netbench 4 congested_wifi 20 1 strafe rts      # CLIENTS PROFILE SECONDS SEED POLICY DEMO
```

**Seat count bounds the fleet.** A client past the demo's seats is admitted as an observer, drives no body and
fails its own gate for having no samples. `arena` seats 24, `hockey` 32, `rts` 2.

## The one rule that shapes the design

**Impair the link BELOW the reliability layer.** ENet ships no conditioner, and wrapping the peer would sit
*above* retransmit — you would be measuring your own simulated loss being repaired, not the netcode's response
to real loss. The relay is a separate process forwarding raw UDP, so drops, delays, duplicates and reorders
happen where they would on a real network.

**Everything is measured in the tick domain**, never in render-frame jerk. A frame-domain measurement conflates
the renderer with the network and cannot be compared across machines. That is the single most common way a
netcode bench lies.

## Pointing it at your game

netbench needs four things from a game, and `BenchSubject` is those four things (plus one optional fifth):

```gdscript
extends BenchSubject
class_name MyBenchSubject

func is_ready() -> bool                     # session live and simulating
func local_body() -> Node                   # the locally-owned body, or null
func apply_input(frame: Dictionary) -> void # feed one tick-pure frame
func sample(body: Node) -> Dictionary       # optional per-tick game metrics
func remote_bodies() -> Array[Node]         # optional; enables the remote-cadence reading
```

`remote_bodies()` is the one addition that is not part of the four. Leave it alone and every gate still runs;
implement it and `RemoteCadence` reports how often a remote body's authoritative pose actually reaches this
client, split near from far. That is the measurement for "the other players move choppily", which no
local-player metric can see: a client renders a remote body by interpolating between the last two poses it
captured at a net tick, so a tick that brought no fresh row renders a held frame. Return a list the game
already has to hand — it is called once per net tick, so walking the whole scene tree is the wrong
implementation.

Then attach the probe during session bring-up:

```gdscript
if BenchProbe.enabled():
    var probe := BenchProbe.new()
    probe.subject = MyBenchSubject.new()
    add_child(probe)
```

The frame is a plain `Dictionary` in a neutral vocabulary (`translate`, `rotate`, `aim_dir`, `aim_held`,
`fire`) because the bench cannot name a game's input type. Keys a game does not use are ignored; keys it needs
that a policy never sets keep the game's own default — so the vocabulary can grow without invalidating
recorded tapes. An **empty** frame means "release": stop overriding and hand the body back to live input.

`demos/rts/` implements this in ~90 lines, mapping `translate` onto a command cursor and `fire` onto issuing
orders — so the same policies drive an RTS and a shooter unchanged.

## Comparing two runs

The impairment scheduler is **seeded and deterministic**, so the same seed replays the same link exactly. Two
runs of the same command therefore differ only by what changed in the netcode, and `compare.py` turns that into
a table rather than an opinion:

```sh
NETBENCH_OUT=/tmp/nb-before just netbench 4 congested_wifi 25 1 strafe_fire
# ...make the change, then rebuild: just native-install
NETBENCH_OUT=/tmp/nb-after  just netbench 4 congested_wifi 25 1 strafe_fire
tools/netbench/compare.py /tmp/nb-before /tmp/nb-after
```

`NETBENCH_OUT` names a stable artifact directory instead of a temp one. `compare.py` pools every row, reports
p50 and p95 per column, and prints `REGRESSED` / `improved` / `same` against a tolerance (5% by default) for
the columns whose direction is known, and exits non-zero on a regression.

**Warm-up is dropped from both series.** Each client loses its first three seconds — the handshake, the first
full-state burst and the clock's initial convergence. The server's per-second wire line loses every row
before its first live peer, plus the same three seconds after it: a dedicated server logs from boot, so the
rows before anybody joined describe an empty session and would pull every server p50 toward zero.

**A column that is zero on both sides reads `not measured`, not `same`.** Most send-path columns are
collected on the server and appear in no client CSV, so in a client-only comparison they are absent rather
than unchanged. `same` on a dozen of them would read as a send path that was compared and found equal.

**A column zero on the BASELINE only reads `new (was 0)`**, and is not counted as a regression. There is no
denominator, which is why the delta prints `n/a` — judging it anyway made every capability the baseline never
exercised look like a regression. It prints under its own verdict rather than as `not measured`, because the
same shape is a fault counter leaving zero for the first time.

**The per-frame CPU timers carry an absolute floor as well as the relative tolerance.** They sit near
0.02–0.03 ms, where 5% is below the spread between two runs of one binary — measured, not assumed: back-to-back
runs of one commit moved `rollback_ms` +11.5% and `net_ms` +10.0%. A move under 0.05 ms, which is 0.15% of a
33 ms tick, is not judged. `compare.py --self-test` asserts these rules and reads no artifacts.

**Resim depth is printed and not judged**, for the reason the run's own gate does not judge it: it deepens
legitimately under latency, and prediction that is actually broken shows up as `reconcile_snap`.

**Two tables, and the second one is the point for a send-path change.** Every column describing the SEND path
reads zero in a client CSV, because a client is not the authority and runs none of it. The server is therefore
run under `ORBITNET_DEBUG=1`, which prints two lines: the `tick=` debug counters, and one `NETSEND` line per
published window carrying the whole of `Net.bandwidth_metrics()`. Both are folded into `server.csv`, one row
per window.

| `server.csv` column | |
|---|---|
| `want_full_nacks_s` / `unproven_acks_s` | **server-side only.** A client increments neither, so these reach an artifact through this line and no other. |
| `tx_bytes_s` / `tx_wire_bytes_s` / `tx_datagrams_s` / `tx_peak_peer_bytes_s` | egress payload, the same with per-datagram overhead, the datagram count, and the busiest peer's share |
| `blocks_admitted_s` / `blocks_deferred_s` / `blocks_culled_s` / `blocks_oversize_s` / `blocks_full_s` | what the admit loop did with each block |
| `starve_ticks_max` / `unsent_backlog_max` | worst in-interest staleness, and the re-entry backlog it cannot see |
| `interest_ms` / `interest_grid` / `interest_entities` | the interest pass's cost, which path ran, and the mean set size |
| `interarrival_near` / `_mid` / `_far` / `_all` | mean ticks between admissions, per distance band |
| `blocks_s` | entity blocks admitted per second, from the debug counter. Printed, never judged: more blocks at the same byte count is a better refresh rate, more blocks at a higher byte count is worse. Read the pair. |
| `rx_applied_s` / `rx_rejected_s` / `rx_skipped_s` | inbound rows applied, refused, and unplaceable |
| `peers` / `ents_rollback` / `ents_state` | what the session held that window |

`BandwidthMetrics::fields` is the one list both the log line and the Godot dictionary are built from, and
`server.csv` takes its send-path columns from whatever that line names — so a counter added there appears in
the CSV with no parser change.

**`want_full_nacks_s` is reported at the end of a run and not gated.** Its own doc calls near-zero the
acceptance bar for interest management being on, and `compare.py` already judges it as a fault counter — but
`bench_gate.gd` evaluates on a client, where it is a structural `0.00`, so the bar read as met on every run
without being measured once. The run now prints the server's maximum past the join window. No threshold: what
a healthy rate is moves with the profile and the arena, and one run is not enough to set one.

## What a run asserts

| Gate | Bound |
|---|---|
| **Sample count** | ≥ 30. An empty run **fails** — a client that never connected must not vacuously pass. |
| **Measured RTT** | lands near the profile's injected round trip, proving the conditioner is live and observed |
| **Clock discipline** | mean \|stretch − 1\| within a bound that **scales with the profile** — a severe link legitimately rides nearer the cap |
| **Reconcile snaps** | ≤ 25% of ticks. Some snaps are normal under loss; a storm means prediction never converges. |
| **Resim depth** | **reported, not gated.** It legitimately deepens under latency and is bounded by `history_limit`; broken prediction shows up as snaps, not depth. |

Each gate prints `PASS`/`FAIL` with the measured value and the bound, so a failing artifact is
self-diagnosing.

## Profiles

One source of truth: `addons/orbitnet/bench/net_profiles.gd`.

| | latency / jitter / loss |
|---|---|
| `clean` | 0 / 0 / 0 — the control. A run that fails here is a harness bug, not a netcode finding. |
| `lan` | 1 / 0 / 0 |
| `broadband` | 30 / 10 / 0.5% |
| `congested_wifi` | 50 / 50 / 2% — the everyday bad case; jitter is the story, not mean latency |
| `mobile_4g` | 100 / 20 / 4% |
| `cross_region` | 150 / 15 / 1% |
| `mobile_3g` | 300 / 30 / 7% |
| `worst_case` | 250 / 25 / 5% — the ceiling a shooter is designed to still function at |
| `worst_case_burst` | same latency, **bursty** (Gilbert–Elliott) loss instead of uniform |
| `torture` | 450 / 50 / 10% — past the design envelope; expect failures, that is the point |

The scheduler is **seeded and deterministic**: the same seed replays the same link exactly, which is what makes
two runs comparable. Different seeds give different links, so a fleet is not one correlated waveform.

## Bot policies

`idle` · `strafe` · `orbit` · `wander` · `strafe_fire`

Each is a **pure function** of `(policy, elapsed, seed)` → one input frame. Deliberately simple, cyclic and
motion-rich so a client generates continuous input, state churn and events — not to play well. The per-seed
phase offset spreads a fleet out of lockstep while keeping each bot reproducible.

## Record and replay

```sh
# capture a bot (or a human) session as a fixture
godot --path <project> -- --bench --bench-bot=wander --bench-record=user://tape.obnt --bench-duration=30

# replay it under a different profile, with metrics
godot --path <project> -- --bench --bench-replay=user://tape.obnt \
      --bench-metrics=user://run.csv --bench-profile=worst_case --bench-duration=30
```

A tape is `{magic, version, frames}` through `var_to_bytes` — lossless, game-agnostic, and forward-compatible
(an unknown key rides through untouched). Decoding rejects a non-tape blob rather than misparsing it, and
never instantiates objects.

## Flags

All after `--`:

| | |
|---|---|
| `--bench` | enable the probe (the others are inert without it) |
| `--bench-bot=<policy>` | drive the body with a `BenchPolicy` |
| `--bench-seed=<int>` | motion phase seed — vary per client to de-correlate a fleet |
| `--bench-metrics=<path>` | stream per-tick metrics to CSV and evaluate the gate on finish |
| `--bench-record=<path>` / `--bench-replay=<path>` | tape capture / playback (replay wins over a bot) |
| `--bench-duration=<s>` | finish, print the verdict, quit. Measured from the **first spawn**, so a slow connect does not starve the sample count. |
| `--bench-profile=<name>` | the profile this client runs under, for the RTT gate |

Environment: `NETBENCH_OUT=<dir>` writes the artifacts somewhere stable (see *Comparing two runs*), and
`SERVER_PORT` / `RELAY_PORT` move the two UDP ports.

## The import that runs first

Every run imports the demo project before it launches anything, and says so.

- A project whose `.godot/global_script_class_cache.cfg` is absent or **stale** resolves every `class_name`
  to `Variant`, which each demo promotes from a warning to an error. The run then dies at parse time and the
  bringup wait reports `dedicated server never bound` — the symptom three steps from the cause.
- A stale cache is the case CI hits: a workspace checked out over a previous run keeps its `.godot/`, so
  testing that the directory exists skips the import on exactly the tree that needs it. The import is
  therefore unconditional; a warm project rescans in seconds.
- A cold project is imported twice, the first discarded. A GDExtension perturbs the build order of a cache
  built from nothing, so an autoload's own type can transiently resolve as its base `Node`.
- The run stops with the import log if no class cache exists afterwards, rather than launching a server that
  cannot parse.

A bringup failure prints the errors found anywhere in that process's log before the tail. A GDScript parse
cascade is longer than a tail, and the first script it names is the one to read.

## Multi-machine

```sh
SERVER_HOST=… CLIENT_HOSTS="…" just netbench-gauntlet
```

One SSH controller drives a server host plus bot-client hosts. Needs reachable hosts, passwordless SSH and
Godot on each. `GAUNTLET_DRYRUN=1` prints the plan without touching anything.

## What it deliberately does not do

- **It is not a PR gate.** The numbers depend on the machine. CI gates correctness; this measures behavior.
- **It does not assert cross-client determinism.** The server is authoritative; bots need only be
  reproducible.
- **It does not simulate bandwidth caps or NAT.** Latency, jitter, loss, duplication and reordering only.
