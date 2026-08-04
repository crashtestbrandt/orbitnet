# netbench — the OrbitNet netcode test bench

**netbench** is OrbitNet's answer to a hard question: *how do you regularly test netcode when you can't gather
more than a couple of real players on real machines?* It is the indie form of the AAA layered test bench —
a below-reliability packet conditioner, a fleet of real headless clients driven by bots through the real input
path, per-tick metric gates, session record/replay, and a multi-machine orchestrator — all shipped inside the
`addons/orbitnet/` addon so OrbitNet is a self-contained netcode layer, not just a rollback facade.

Everything lives under `addons/orbitnet/bench/` (game-side classes) and `tools/netbench/` (the shell harnesses).

## The one rule that shapes the design

**Impairment is injected at the raw-UDP layer, BELOW ENet's reliability.** A dropped datagram there forces ENet's
real retransmit, a reordered one exercises its ordering buffers, a delayed one drives the backend's ping/pong
clock sync —
the honest behaviour every reference conditioner has (Source `net_fakelag`, Unreal `PacketSimulationSettings`,
Unity's simulator stage, Valve SNS `FakePacket*`). Dropping a "reliable" packet *above* the reliability layer is a
lie (it is permanently lost, nothing retransmits), so wrapping the `MultiplayerPeer` is **not** how netbench
conditions the wire. Two honest paths instead:

- **ENet** (the CI/dev transport): an external **UDP relay** process (`relay_main.gd`) sits between client and
  server. A client just joins the relay's port — `NetManager.join` / `net.join` already accept any address:port,
  so no game code changes.
- **Steam**: SteamNetworkingSockets' built-in `FakePacket*` config values, applied below its SNP reliability,
  driven live by the `net.sim_*` console cvars (see *Console conditioner* below).

## Quick start

```sh
just netbench 4 clean            # control run: 4 bots, no impairment (isolates harness/engine overhead)
just netbench 4 congested_wifi   # the everyday bad case: 50ms + 50ms jitter + 2% loss
just netbench 4 worst_case 30    # the 250ms design ceiling (Overwatch's lag-comp bound), 30s window
just netbench 6 mobile_4g 20 7 wander   # 6 bots, LTE profile, 20s, seed 7, 'wander' policy
```

`just netbench <CLIENTS> [PROFILE] [SECONDS] [SEED] [POLICY]` launches a dedicated server, one impairment relay,
and N headless bot clients that join **through** the relay. Each client drives the real input path, streams
per-tick metrics to a CSV, and self-evaluates a tick-domain gate. The run passes when every client passes; the
artifact directory (per-client CSVs + logs) is printed at the end. It is deliberately **not** in `just check` —
it is multi-process and timing-sensitive, a nightly/on-demand lane, never a per-PR gate.

## What a run asserts (tick-domain gates)

Gates are distribution-based and tick-domain — never render-frame jerk (the S8 lesson, and AAA practice). The
numbers come from the facade's `perf_metrics()` / `clock_metrics()` — live in **every** build (no debug-monitor
gating), with RTT/offset sampled by the backend's ping/pong clock sync. From `bench_gate.gd`, per client:

| Gate | Asserts | Notes |
|---|---|---|
| **sample count** | the run produced data | a client that never connected FAILs, never vacuously passes |
| **RTT reflects injection** | p50 RTT ≈ the profile's ~2× one-way latency | the *"the conditioner is live and observed"* check — without it a silent no-op relay passes everything on a clean link |
| **clock discipline** | mean \|stretch−1\| bounded, **scaled by profile** | decoupled tick pacing stretches within `max_time_stretch` (1.05) — coupled mode pins stretch at exactly 1.0 and absorbs error as rare whole-tick slews — and a severe link legitimately rides near that envelope, so the bound scales; a truly thrashing clock still fails |
| **reconcile convergence** | hard-snap rate ≤ 25% | the real *"prediction still works"* signal; a snap storm means prediction never catches up |
| resim depth | **reported, not gated** | under latency the resim window legitimately deepens (#214 cost), bounded by `history_limit`; broken prediction shows up as snaps, not depth |

Thresholds scale with the profile: `clean` is held to the tightest bar; `worst_case` legitimately shows more
RTT/jitter/stretch. A verified run looks like:

```
client1: BENCH-RESULT PASS profile=worst_case samples=594 | worst_case: 250/25ms loss=5.0% ...
  BENCH-GATE PASS RTT p50 637.5ms within [250,750]ms of injected ~500ms (conditioner observed)
  BENCH-GATE PASS mean |clock stretch - 1| 0.0470 <= 0.0600 (clock not thrashing; bound scales with the profile)
  BENCH-GATE PASS reconcile snap rate 0.000 (0 over 594 ticks) <= 0.25
  BENCH-GATE INFO resim depth p50=31 p95=67 ticks (cost under latency; bounded by history_limit 128)
```

## Condition profiles

The catalog (`net_profiles.gd`) is the single source of truth, read by the relay, the Steam conditioner, and the
gates. All numbers are **one-way** (RTT ≈ 2×), calibrated from Unity's Network Simulator presets, Overwatch's
250ms lag-comp ceiling, and Gears of War 3's practice of forcing the conditioner on in playtest builds.

| profile | one-way / jitter | loss | use |
|---|---|---|---|
| `clean` | 0 / 0 | 0% | control (isolates harness overhead) |
| `lan` | 1 / 0 | 0% | wired LAN floor |
| `broadband` | 30 / 10 | 0.5% | healthy home fiber/cable |
| `congested_wifi` | 50 / 50 | 2% | the everyday bad case (default) — **default your playtest builds to this** |
| `mobile_4g` | 100 / 20 | 4% | LTE (mobile roadmap) |
| `mobile_3g` | 300 / 30 | 7% | degraded cellular |
| `cross_region` | 150 / 15 | 1% | distant but well-provisioned server |
| `worst_case` | 250 / 25 | 5% | the design ceiling a shooter must still function at |
| `worst_case_burst` | 250 / 25 | Gilbert-Elliott | bursty loss (drops runs of packets) — stresses retransmit differently than uniform loss |
| `torture` | 450 / 50 | 10% | deliberate programmer-pain (never a gate) |

Sweep a single knob without a catalog entry via the relay's `--relay-latency/jitter/loss/dup/reorder/reorder_ms`.

## Bot policies

`bench_policy.gd` — pure, deterministic functions of (time, seed), fed to the body via `set_scripted_input` (the
same tick-pure seam the determinism/net/load probes drive). `idle`, `strafe` (default), `orbit`, `wander`,
`strafe_fire` (adds held-aim burst fire, so the weapon authority + shot replication join the load). A per-seed
phase offset spreads a fleet out of lockstep so N bots aren't one correlated waveform.

## Record & replay (regression fixtures)

Capture a session's per-tick input stream and replay it later under any profile — Riot's Server Network Recording
pattern. The tape is the exact per-net-tick owner input that drove the sim and was replicated (`InputTape`, a
lossless `var_to_bytes` codec).

```sh
# record a bot (or a human) session:
godot --headless --path . -- --join=127.0.0.1:47810 --bench --bench-bot=strafe \
    --bench-record=/tmp/run.obnt --bench-duration=8
# replay it (drives the body from the tape) under a different profile, with metrics:
godot --headless --path . -- --join=127.0.0.1:47810 --bench --bench-replay=/tmp/run.obnt \
    --bench-metrics=/tmp/replay.csv --bench-profile=worst_case --bench-duration=8
```

Record and replay are both keyed to the **net tick** (not the physics frame), so they stay cadence-consistent
under the net/physics decouple. To record a *human* session, run a normal client with `--bench --bench-record=…`
and no bot.

## Multi-machine (the Gauntlet)

`tools/netbench/gauntlet.sh` is one SSH controller that drives a server host + bot-client hosts, then collects and
evaluates every peer's artifacts — Unreal Gauntlet's architecture (SSH as the device transport, fixed hostnames as
the rendezvous), Riot BVS's shape at 1% scale. **Not** a GitHub-Actions job mesh (Actions has no live inter-job
networking). Pair with **Tailscale**: MagicDNS gives stable hostnames + NAT traversal, so cross-site machines on
different OSes join one session by name (and the real WAN is free realism — measure it, don't assume it).

```sh
# controlled conditions (relay on the server host):
SERVER_HOST=box-a CLIENT_HOSTS="box-b box-c" RELAY=1 PROFILE=congested_wifi CLIENTS_PER_HOST=2 \
    just netbench-gauntlet
# realism spot-check (raw WAN between machines, no relay):
SERVER_HOST=box-a CLIENT_HOSTS="box-b box-c" RELAY=0 just netbench-gauntlet
# see the exact plan without touching any host:
SERVER_HOST=box-a CLIENT_HOSTS="box-b box-c" GAUNTLET_DRYRUN=1 just netbench-gauntlet
```

It needs real reachable hosts with passwordless SSH + Godot 4.7 (so it cannot run in a single-box CI sandbox —
that is what `just netbench` is for). Waits are poll-until-condition with deadlines (never bare sleeps, the Riot
rule); teardown sweeps game processes by cmdline on every host.

## Console conditioner (`net.sim_*`)

The live in-process conditioner surface, for the **Steam** transport: on a Steam build these drive
SteamNetworkingSockets' `FakePacket*` config below reliability. On ENet they store their values but tell you
to condition the wire through the relay (ENet has no honest in-process seam).

```
net.sim_profile congested_wifi   # apply a catalog profile live
net.sim_latency 120              # or tune individual knobs (one-way ms; loss/dup/reorder are [0,1])
net.sim_status                   # show the active path (Steam in-process vs ENet relay) + current params
net.sim_off                      # clear
```

These are distinct from `net.lag_sim` / `net.loss_sim`, which only perturb the owned body's pose to eyeball the
reconcile smoother — they do **not** touch the wire.

## Architecture (files)

Pure cores are unit-tested (`tests/unit/*_test.gd`, run by `just test`); the socket/scene/network shells are
exercised by the live bench run.

| file | role | tested by |
|---|---|---|
| `net_profile.gd` / `net_profiles.gd` | the condition record + catalog | `net_profiles_test.gd` |
| `packet_impairment.gd` | pure drop/delay/dup/reorder scheduler (seeded, deterministic) | `packet_impairment_test.gd` |
| `relay_main.gd` | the UDP relay `MainLoop` (`-s` entry) — one impairment pair per client | live (`bench.sh`) |
| `bench_policy.gd` | pure bot behaviours | `bench_policy_test.gd` |
| `bench_bot.gd` | Node driver: policy → `set_scripted_input` each tick | live |
| `input_tape.gd` | lossless record/replay codec | `input_tape_test.gd` |
| `bench_metrics.gd` | per-tick CSV recorder + gate runner | live |
| `bench_gate.gd` | pure pass/fail evaluation | `bench_gate_test.gd` |
| `bench_probe.gd` | the `--bench` harness entry; wires bot/metrics/record/replay from CLI flags | live |
| `net_conditioner.gd` | `net.sim_*` facade → Steam `FakePacket*` (below reliability) | live (Steam build) |

CLI flags (all after `--`): `--port=<n>` (host/dedicated); `--join=addr:port`; `--bench` + `--bench-bot=`,
`--bench-seed=`, `--bench-metrics=`, `--bench-record=`, `--bench-replay=`, `--bench-profile=`, `--bench-duration=`.

## What netbench deliberately does NOT do

- **No protocol-level fake clients.** Real headless clients are affordable at session scale (4–16) and strictly
  higher fidelity; fake clients only exist to load-test platform services at farm scale (Riot's 2M-player harness).
- **No render-domain gates.** Every gate is a facade metric read at a net tick.
- **No PR gate.** netbench is a nightly/on-demand lane. Per the project rule, the per-PR `net-check.yml` stays
  minimal; adopt Riot's promotion rule (a new gate runs green for a week before it may block anything).
- **No Steam headless CI.** Steam needs a logged-in client per machine (Spacewar 480 is restricted), so the Steam
  transport gets a semi-manual two-machine tailnet smoke; ENet stays the CI transport.
