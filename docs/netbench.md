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
the columns whose direction is known. It drops the first three seconds of each client's series — the
handshake, the first full-state burst and the clock's initial convergence — and it exits non-zero on a
regression.

**Two tables, and the second one is the point for a send-path change.** Every column describing the SEND path
reads zero in a client CSV, because a client is not the authority and runs none of it. The server is therefore
run under `ORBITNET_DEBUG=1` and its per-second wire line is folded into `server.csv`:

| `server.csv` column | |
|---|---|
| `tx_bytes_s` | snapshot payload bytes per second, across every peer — **server egress** |
| `blocks_s` | entity blocks admitted per second. Printed, never judged: more blocks at the same byte count is a better refresh rate, more blocks at a higher byte count is worse. Read the pair. |
| `rx_applied_s` / `rx_rejected_s` / `rx_skipped_s` | inbound rows applied, refused, and unplaceable |
| `peers` / `ents_rollback` / `ents_state` | what the session held that second |

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
