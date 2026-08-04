# OrbitNet

Rollback netcode for Godot 4, in Rust.

This directory is the **GDScript surface**: the `Net` facade, the transport factory, and the typed handles
gameplay code is written against. The backend it drives is a Rust GDExtension whose binaries live in the
sibling addon **`addons/orbitnet_native/`** — install **both** directories, or `Net` is a facade over
nothing.

Full documentation lives in the repository: [`docs/`](../../docs/), starting with
[`docs/getting-started.md`](../../docs/getting-started.md).

## The two enforced boundaries

- **`net.gd` (`Net`) is the ONLY file that names the backend classes.** `tools/net-check.sh` (CI) fails the
  build if a backend symbol appears anywhere else — including in the demos, which is the point: a demo that
  needs to reach past the facade has found a hole in the facade.
- **`steam_transport.gd` is the ONLY file that names Steamworks.** Every Steam access is dynamic
  (`has_singleton`, `callv`, `ClassDB`), so a non-Steam build carries zero Steam dependency and the project
  lints and runs where GodotSteam is not installed at all.

## Three lanes

| Lane | Created by | For | Cost |
|---|---|---|---|
| **Rollback** | `Net.register_rollback_body()` | an entity whose owner authors continuous per-tick input and predicts locally | a history ring + per-tick compare and replay, per entity |
| **State** | `Net.make_state()` | server-authoritative values pushed every tick, no prediction | one delta block per entity per tick |
| **Command** | `NetCommand` | sparse, discrete, reliable, server-validated requests | one reliable RPC per request |

The distinction that bites people: the rollback lane **restores recorded history onto its properties every
tick**, so a value written from outside the tick — a `NetCommand` handler, a timer, a signal — is silently
overwritten. Those values belong on the state lane.

## What's inside

**Facade** — `net.gd` (`Net`): mode, the tick loop, the three lanes' handles, the physics/net decouple, and
the diagnostics harnesses read (`perf_metrics()` / `clock_metrics()`, live in ALL builds — no debug-monitor
gating). Seeded from the `[orbitnet]` project-settings block (`sync_to_physics`, `tickrate`,
`max_time_stretch`, `history_limit`).

**Transport factory** — `net_transport.gd` (`NetTransport`): the one place that picks a concrete transport
from export-preset feature tags — native ENet, or Steam via `steam_transport.gd`. Add a transport as a new
`Kind` branch there, gated by a matching `custom_features` tag; never scatter platform-SDK calls elsewhere.

**Primitives** — `net_command.gd`, `net_state_handle.gd`, `net_rollback_handle.gd`, `net_interp_handle.gd`,
`net_lag_comp.gd`, `net_ray.gd`, `net_session_info.gd`.

**netbench** (`bench/`) — the netcode **test bench**: a below-ENet-reliability UDP impairment relay, a fleet
of real headless bot clients driven through the real input path, tick-domain metric gates, and session
record/replay. It reaches your game through one small seam, `BenchSubject` — implement four methods and the
whole bench points at your project. See [`docs/netbench.md`](../../docs/netbench.md).

```sh
just netbench 4 congested_wifi   # 4 bots through a conditioned link, tick-domain gates
just netbench 4 worst_case       # the 250ms design ceiling
```

## Layout

```
addons/orbitnet/
  orbitnet.gd            EditorPlugin — installs the Net autoload
  net.gd                 the Net facade (the ONLY file naming the backend classes)
  net_transport.gd       transport factory (feature-tag transport selection)
  steam_transport.gd     Steam arm (the ONLY file naming Steamworks)
  net_command.gd  net_state_handle.gd  net_rollback_handle.gd  net_interp_handle.gd
  net_lag_comp.gd  net_ray.gd  net_session_info.gd
  bench/                 netbench — the netcode test bench
    bench_subject.gd                      the seam between the bench and YOUR game
    net_profile.gd  net_profiles.gd       condition profiles (one source of truth)
    packet_impairment.gd                  pure drop/delay/dup/reorder scheduler
    relay_main.gd                         the below-reliability UDP relay (-s MainLoop)
    bench_policy.gd  bench_bot.gd         pure bot behaviours + the driver
    input_tape.gd                         lossless record/replay codec
    bench_metrics.gd  bench_gate.gd       per-tick metrics + tick-domain gates
    bench_probe.gd                        the --bench harness entry
    net_conditioner.gd                    in-process impairment for the Steam transport

addons/orbitnet_native/  the .gdextension + the loaded backend binaries — install this too
```

## Licence

MIT OR Apache-2.0, at your option. See the repository's `LICENSE`, `LICENSE-MIT`, `LICENSE-APACHE` and
`THIRD_PARTY.md`.
