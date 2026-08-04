# OrbitNet

**Rollback netcode for Godot 4, in Rust.**

Server-authoritative replication with owner prediction and reconciliation, batched delta state sync, clock
discipline, and interest management — behind one GDScript facade, over a native GDExtension backend.

```gdscript
# Everything a replicated player body needs.
Net.register_rollback_body(
    self, $Input,
    ["position@half", "velocity"],   # state:  server-authored
    ["move", "jump"],                # input:  client-authored, server-validated
    is_multiplayer_authority())
```

> **0.x.** The `Net` facade is the surface intended to be stable. Everything behind it — the wire format, the
> Rust class surface, the crate layout — may change between minor versions. Pin a tag.

---

## What it is

OrbitNet came out of a multiplayer game and was built from the start to be extractable: two grep-enforced
boundaries keep the backend behind one file and the platform SDK behind another, which is what let its
rollback backend be swapped wholesale — a vendored GDScript implementation out, this Rust one in — as a
one-file rewrite.

**Three replication lanes**, and picking the right one is most of what there is to learn:

| Lane | For | Cost |
|---|---|---|
| **Rollback** — `Net.register_rollback_body()` | an entity whose owner authors continuous per-tick input and predicts locally | a history ring plus per-tick compare and replay, per entity |
| **State** — `Net.make_state()` | server-authoritative values pushed every tick, no prediction | one delta block per entity per tick |
| **Command** — `NetCommand` | sparse, discrete, reliable, server-validated requests | one reliable RPC per request |

The distinction that bites people: **the rollback lane restores recorded history onto its properties every
tick**, so a value written from *outside* the tick — a command handler, a timer, a signal — is silently
overwritten. Those values belong on the state lane. That is the single most common OrbitNet bug and it does
not raise an error; it just quietly does nothing.

**What the backend actually does differently:**

- **One batched UDP frame per peer per tick.** Transport-agnostic (`SceneMultiplayer.send_bytes`), so ENet
  and Steam are untouched. Entity ids are *derived* — FNV-1a of the synchronizer root's node path — not
  assigned, so nothing routes through per-node RPCs.
- **Per-entity dirty windows.** Each body resimulates only as deep as *its* newest late input, so one
  lagging peer deepens only its own body's replay instead of dragging every entity through the full window.
- **Columnar packed history.** Fixed-stride rows with `memcmp` changed-masks and masked merges — no per-tick
  allocation.
- **Input-novelty freshness.** `is_fresh` is true exactly once per tick, on the first pass simulated with
  real input. One-shot effects gate on it directly; no dedup ledgers.
- **Three property roles** — State / Input / Cosmetic. Cosmetic properties replicate but are never restored
  and never count as a misprediction, so presentation state rides the same packets without widening the
  resim surface.
- **Wire quantization.** `"position@half"` halves a Vector3 on the wire; `"basis@ss3"` takes a rotation from
  16 bytes to 8.

## 60-second quickstart

```sh
git clone https://github.com/crashtestbrandt/orbitnet && cd orbitnet
just sync-addons     # mirror the addon into the demo projects
just rts             # single player, 96 units, zero networking
```

Then, in two terminals:

```sh
just rts-host        # listen server + a local player
just rts-join        # a client
```

No Rust toolchain needed — the extension binaries are committed as plain files.

## The RTS demo

`demos/rts/` is a two-seat skirmish RTS, and it exists to make one architectural argument concretely:

- **Rollback lane: exactly one entity per player** — the command cursor. It is the only thing an RTS
  continuously authors, and it is also the **AOI anchor**, without which `set_aoi_radius()` cannot function
  at all.
- **State lane: every unit.** 96 of them. Their "input" is a sparse order, so there is nothing to predict.
- **Command lane: every order**, one channel per player *seat*, server-validated.

Its signature number is **order RTT** — click → validate → adjudicate → *observed* — which is what a player
actually feels and which no other netcode demo shows you. Six keybound levers (net tick 20↔60, remote resim,
input delay, display offset, AOI radius, interpolation on/off) change it live, because A/B-ing them *is* the
demo.

It also states plainly what does **not** work: AOI culls the rollback lane only, so the HUD reads
`aoi=128m — ROLLBACK LANE ONLY: 1/1 cursors culled, 0/96 units`. See [docs/rts-demo.md](docs/rts-demo.md)
for the wire-schema arithmetic and the lockstep-versus-server-auth argument.

## Install

**Asset Library** — download `orbitnet-*.zip` from a [release](https://github.com/crashtestbrandt/orbitnet/releases),
then *AssetLib → Install from file*. Enable **OrbitNet** in *Project → Project Settings → Plugins*.

**Manually** — copy `addons/orbitnet/` **and** `addons/orbitnet_native/` into your project. Both are
required: `Net` without the extension is a facade over nothing.

**As a submodule** — `git submodule add https://github.com/crashtestbrandt/orbitnet third_party/orbitnet`,
then copy or symlink the two addon directories into your project's `addons/`.

Enabling the plugin registers the **`Net`** autoload. For an exported build, make sure the autoload line is
actually in your `project.godot` — the EditorPlugin adds it in the editor, and that entry is what makes `Net`
exist in a build.

### Support matrix

| | |
|---|---|
| **Godot** | 4.4 and newer. The extension is built against the 4.4 API and loads in any Godot at or above it. |
| **Language** | GDScript. No C# bindings. |
| **Platforms** | Linux x86_64, Windows x86_64, macOS universal (arm64 + x86_64). |
| **Transports** | ENet (native UDP) out of the box; Steam via [GodotSteam](https://godotsteam.com/), selected by an export-preset feature tag. |
| **Not supported** | Web — Godot's web export cannot load a GDExtension at all. Android/iOS need only export presets and a matrix entry; nothing in the protocol is desktop-specific. |

## Documentation

| | |
|---|---|
| [getting-started.md](docs/getting-started.md) | Your first replicated body, in about 30 lines. Start here. |
| [api.md](docs/api.md) | The whole `Net` / `NetCommand` / `NetTransport` / handle surface, including `@half` and the f64/i64 scalar reality. |
| [rts-demo.md](docs/rts-demo.md) | The lane-split argument, the wire schema, the byte budget, and why determinism is not needed. |
| [architecture.md](docs/architecture.md) | Why the backend is Rust, the crate layout, batching, history, prop roles. |
| [protocol.md](docs/protocol.md) | Wire format, tick and rollback model, `is_fresh`, entity lifecycle. |
| [netbench.md](docs/netbench.md) | The impairment relay, the bot fleet, and the tick-domain gates. |
| [building.md](docs/building.md) | Rust toolchain, `just native-*`, and the binary distribution policy. |
| [steam.md](docs/steam.md) | The Steam transport contract. |
| [CONTRIBUTING.md](CONTRIBUTING.md) | The GDScript rules, the probe-vs-unit-test bar, `just check`. |

## Repository layout

The repository root is deliberately **not** a Godot project. OrbitNet is configured through an `[orbitnet]`
block in `project.godot`, and the demos disagree about those values on purpose — so each consuming project is
its own project and gets a mirror-copy of the addon.

```
addons/orbitnet/          CANONICAL addon source — the AssetLib payload
addons/orbitnet_native/   the .gdextension + committed binaries (plain git, never LFS)
native/                   the Rust workspace (orbitnet-core + orbitnet-godot)
harness/                  a tiny Godot project: the addon's own suites + the load smoke
demos/rts/                the RTS demo — its own Godot project
tools/                    sync-addons, net-check, lint, the probes, netbench
```

`just sync-addons` mirrors the canonical addon into every project; `just addon-drift` fails if a copy was
edited instead of the source. It **copies** rather than symlinks because Git for Windows checks a symlink out
as a text file containing the path unless both `core.symlinks` and Developer Mode are on — a fatal, cryptic
first-run failure. `ORBITNET_LINK=1 tools/sync-addons.sh` symlinks instead, for Unix devs iterating on
`net.gd` itself.

## Testing

```sh
just check     # addon-drift, net-check, cargo gates, lint, unit suites, the two-peer probe
```

Coverage defaults to a **unit suite** — sub-second, no scene tree, no physics, no sockets. A scene probe is
reserved for what genuinely needs a live session, and only one of those gates PRs (`just rts-probe`, two real
peers over ENet). `just netbench 4 congested_wifi` drives a bot fleet through an impairment relay; it is a
measurement tool, not a gate.

## Known gaps

Filed as issues, and honest about it:

- **AOI applies to the rollback lane only.** State-lane entities always replicate.
- **No per-peer visibility veto.** Without one, every RTS on OrbitNet ships a maphack; the demo's optional
  fog is labelled presentation-only for exactly this reason.
- **No send-budget knob or per-entity priority.** The budget is a constant and entities are served
  stalest-first.
- **No state-lane health metrics** (`entities_deferred`, `worst_staleness_ticks`, `snapshot_bytes`). The RTS
  probe measures staleness from the outside because the library does not publish it.
- **`@half` silently no-ops on invalid pairings** instead of warning. A GDScript `float` is an f64, so
  `"hp@half"` saves nothing and says nothing.
- **No `NetCommand.rejected` feedback and no `request_batch`.** A refused command is invisible to the client.
- **Interest anchors are not configurable** — the anchor is the peer's own rollback body, full stop.
- **The demo's session layer omits a version handshake.** Two incompatible peers connect and misbehave
  rather than being refused with a reason.

## Licence

**MIT OR Apache-2.0**, at your option. See [LICENSE](LICENSE), [LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE).

The compiled extension links godot-rust (gdext), which is **MPL-2.0**. That is *file-scoped* copyleft, no
gdext file is modified here, and MPL §3.2 explicitly permits distributing the executable under your own
terms — so shipping a game with OrbitNet inherits no copyleft obligation. The full reasoning and the
dependency inventory are in [THIRD_PARTY.md](THIRD_PARTY.md).

Contributions are dual-licensed the same way. Inbound equals outbound; there is no CLA.
