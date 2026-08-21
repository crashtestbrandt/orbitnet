# Architecture

How the backend is built. Read [getting-started.md](getting-started.md) to *use* OrbitNet; read this to change
it. The wire format and tick model are in [protocol.md](protocol.md).

## Crate layout

```
native/
  crates/orbitnet-core/     PURE Rust, zero dependencies, no `godot` — plain `cargo test`
  crates/orbitnet-godot/    the cdylib; the only crate that knows Godot exists
```

`orbitnet-core` holds every algorithm: the tick clock, remote clock discipline, the per-entity resim planner,
columnar history, the property schema and wire codec, the freshness ledger, interest sets, pacing. It runs
under `cargo test` in milliseconds and is reusable outside Godot. `orbitnet-godot` is a marshalling shell —
class registration, `Variant` ↔ packed-row conversion, the entity registry, signals, the packet pump.

**The rule that keeps it honest: core never sees a `Variant`.** A `godot` type in a core signature means logic
has leaked across the boundary. `#![forbid(unsafe_code)]` throughout.

## One batched packet per peer per tick

Transport path: **`SceneMultiplayer.send_bytes()`** plus the `peer_packet(id, packet)` signal.

The most consequential decision here. `send_bytes` rides `SceneMultiplayer`'s framing *above* the peer, so it
is transport-agnostic by construction — identical over ENet, Steam and offline peers, and the transport
factory needs no knowledge of it.

Rejected alternatives:

| | Why not |
|---|---|
| Raw `MultiplayerPeer.put_packet()` | `MultiplayerAPI.poll()` consumes the peer's packets; no supported interception point. |
| `MultiplayerAPIExtension` | Full control, but it means reimplementing RPC routing for every spawner and `@rpc` site in the host project. |
| One `@rpc` per peer carrying bytes | The viable fallback — still one call per peer per tick, at the cost of the RPC layer's per-call overhead. |

**Channels:** state on one unreliable channel, input on another, handshake and entity binding on a reliable
one — so netcode traffic never shares a stream with a game's own reliable RPCs.

The scaling law is `O(peers)` **engine crossings** plus `O(entities × peers)` **bytes in native memory**, not
`O(entities × peers)` crossings. Acks piggyback on the frame header, so there is no separate ack RPC.

## Snapshot capture and history

**Native code cannot make a GDScript getter cheap.** Reading a scripted property costs a lookup, a call and
`Variant` boxing regardless of what calls it. What OrbitNet changes is *how many times* that happens and what
happens after, in leverage order:

1. **Resolve once.** The `(object, StringName)` handle is cached at registration rather than re-walked per
   access.
2. **Capture once per tick**, gating apply/record on the same predicate as simulate, with dirty-tick tracking.
3. **Columnar packed history.** A per-entity `Vec<u8>` of `history_limit × stride`, indexed `tick % capacity`,
   in schema order. No `Dictionary`, no `Variant`, no per-tick allocation — and the changed mask falls out of a
   `memcmp` on fixed-stride slices during capture.
4. **Quantization** — a *bandwidth* lever, not a CPU one. See
   [api.md](api.md#wire-quantization-and-the-scalar-reality).

Two non-negotiable rules:

- **`float` properties are stored as `f64`.** Godot's `float` is 64-bit; recording it as `f32` would round
  every replayed value and break bit-exact resimulation.
- **Never quantize the local prediction path.** The client predicts at full precision; only the server's
  authoritative broadcast is quantized, and quantization error must stay well below the reconcile snap
  threshold.

## Prop roles

Roles are per-property, not a hand-maintained list:

| Role | Restored on replay | Counts as a misprediction |
|---|---|---|
| `State` | yes | yes |
| `Input` | yes | no — a change alone does not trigger resim |
| `Cosmetic` | **no** | **no** |

**The test for `Cosmetic` is "does the simulation ever read it back", not "does it look presentational."**
That line is narrower than it first appears. An actuation value the sim rewrites every tick from
`(state, input)` and never reads back is genuinely cosmetic — a restore that drops it is corrected on the next
tick, and its only consumers are VFX and audio. But a *self-referential integrator* — a smoothed heading that
low-passes over its own previous value, a gait speed that ramps from its own last value — is `State` despite
looking presentational, because not restoring it makes a replayed tick produce different output from identical
authoritative state.

Cosmetic properties ride the same packets on a slow lane without widening the resim surface.

## Interest management

AOI is a **radius + hysteresis filter on the send path**: enter at the radius, leave at 1.25×, so boundary
entities do not flicker. Sends run through a flat scan over the tick's candidates.

A uniform grid in `core::interest` — rebuilt each tick from the position column already in native memory, zero
Godot calls — is implemented, tested, and applies the same rules as the scan: the same hysteresis, the same
cap and tie-breaks, the same leave list, the same always-set, and one set of cells per world. It is not driving
sends because it is **slower** at the arena extents and world counts a session runs at; it starts to pay past
about ±1200 m of occupancy with a small set per peer. The measured tables and the decision are in that module's
header, and `net.perf`'s `interest_ms` is the live number that would reopen it.

**Membership is the second axis.** A radius cannot separate several independent worlds inside one session, each
rebased near its own coordinate origin: two entities at the same coordinates in different worlds are zero
metres apart. Every candidate and every observer carries a membership id, and a candidate whose id differs from
the observer's is refused before any distance is computed. `0` is the default on both sides and matches every
world, so a game that declares none is filtered on distance alone.

The two axes are independent, which is what makes membership usable by the channels that need it most. A state
channel that replicates no position — health, inventory, a door's state — has no distance to be culled by, so
its only lever was all-or-nothing. Declaring it always-relevant *within one membership* bounds it to its own
world while leaving it uncullable inside it.

**A peer's centre and its world both come from one body**: the lowest-id entity whose *input* authority is that
peer and which resolved an anchor. A peer with no such body has neither, and the backend correctly falls back
to "everything is in interest" — every world, at every distance. This is the limitation most likely to surprise
you — see [api.md](api.md#interest-two-axes-distance-and-membership).

Tick tiers are assigned statically per synchronizer and dynamically by distance band, phase-offset by entity id
so sends spread across ticks instead of spiking.

## The bandwidth ceiling

Worth doing the arithmetic before planning for high peer counts. With AOI admitting ~12 visible entities per
peer at ~60 quantized bytes each, per-peer per-tick payload is roughly 750 B:

| Net tick rate | Server egress at 100 peers |
|---|---|
| 120 Hz (physics-coupled) | ~9 MB/s ≈ **72 Mbit/s** |
| 30 Hz (decoupled) | ~2.2 MB/s ≈ **18 Mbit/s** |

**A 100-player target requires a decoupled net tick.** No implementation quality fixes the coupled number; it
is arithmetic. The machinery exists — `Net.set_net_tick_decoupled(30)`.

## Threading

Main-thread only, non-negotiable: `SceneMultiplayer` I/O, `Object::get`/`set`, signal emission,
`_rollback_tick` and its physics queries.

Movable: per-peer frame assembly (delta, quantize, pack — pure native memory, parallel across peers), input
decode batches, AOI grid rebuild.

**Single-threaded today.** At small peer counts thread overhead exceeds the work; a worker pool should be
feature-gated above a peer threshold. One genuine Rust advantage: gdext's `Gd<T>` is not `Send`, so "don't
touch Godot objects from a worker" is a **compile error** rather than a crash report.

## The facade boundary

`addons/orbitnet/net.gd` is the only file permitted to name a backend class, enforced by `tools/net-check.sh`
in CI. That seam is why the backend can be replaced without touching game code, and why a project can depend on
`Net` without ever naming a synchronizer.

The gate scans the demos too. A demo that needs to reach past the facade has found a hole *in the facade* —
widen the facade, do not exempt the caller.
