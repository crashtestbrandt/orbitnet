# Architecture

How OrbitNet is built, and why it is built that way. Start with [getting-started.md](getting-started.md) if
you want to *use* it; this page is for understanding what is underneath, or for changing it.

The wire format and the tick/rollback model have their own page: [protocol.md](protocol.md).

## What ships today

**Layout.** Sources live at `native/` — a `.gdignore`'d cargo workspace Godot never scans, so
plain GDScript work needs no Rust toolchain. The `.gdextension` descriptor sits in `addons/orbitnet_native/`
next to the binaries it loads: `bin/` holds the linux x86_64 `.so`, windows x86_64 `.dll` and macOS universal
`.dylib` (debug + release each), committed via Git LFS. `orbitnet-binaries.yml` rebuilds all three on the
self-hosted per-platform runners and re-commits them whenever `native/**` changes; `deploy-desktop.yml`
rebuilds per platform at deploy time. After touching the Rust, run `just native-install` to rebuild and install
THIS host's binaries so the project picks the change up.

**Integration.** One autoload: `Net` (`addons/orbitnet/net.gd`) — same facade surface, same signals as before
the cutover — creates its single `OrbitNet` child. The old five-autoload block is gone from `project.godot`,
and the `[the previous GDScript backend]` settings block became `[orbitnet]` (`sync_to_physics`, `tickrate`, `max_time_stretch`,
`history_limit`). `net.gd` is still the ONE first-party file naming backend classes (`OrbitNet` /
`OrbitRollbackSynchronizer` / `OrbitStateSynchronizer` / `OrbitInterpolator`); `tools/net-check.sh` polices
that across `scripts/`, `tests/` and `scenes/`, and still polices the retired the previous GDScript backend symbols so the previous
backend cannot creep back.

**The hot path:**

- **One batched `SceneMultiplayer.send_bytes` frame per peer per tick.** Transport-agnostic by construction:
  ENet, Steam and offline needed zero changes.
- **Entity ids are FNV-1a of the synchronizer root's node path**, salted per lane. `MultiplayerSpawner`
  guarantees identical node names on every peer, so both sides derive the same id with no binding RPC — the
  invariant the old node-path RPC routing leaned on implicitly, made explicit. No per-node RPC routing, so the
  "one synchronizer type per body on every peer" constraint (§3.4) is gone.
- **Per-entity dirty-window resimulation**: one late peer deepens only its own body's replay (§2.2).
- **Columnar packed history** (`core::columnar`): fixed-stride rows in a ring, no per-tick allocation; the
  changed mask falls out of a `memcmp` over encoded bytes.
- **Masked delta blocks against client-acked ticks**, periodic full blocks, and a `WANT_FULL` NACK. Input
  headers ack received snapshot frames (`ack_tick` + a 32-bit `ack_bits` window); the server keeps a per-peer
  sent log, promotes acked entity ticks to `acked_base`, and masks deltas only against bases the peer provably
  applied. Receivers additionally refuse to decode a masked block against anything but a bit-exact wire row (a
  per-tick authoritative mark on the state ring), so loss can never make a client reconstruct over its own
  prediction. Client input carries 4 redundant rows per frame, wire-quantized per the property annotations.
- **Ping/pong clock sync with lowest-RTT-half offset filtering** (§6.1). Coupled mode pins clock stretch to
  exactly 1.0 and absorbs error via rare whole-tick slews; decoupled mode stretches within `max_time_stretch`.
  Offsets past 250 ms take the hard-resync panic path in either mode: reseek to the server's estimated tick,
  session-reset every ring, NACK a full — one visible correction instead of a minutes-long slew crawl.
- **Closed-loop tick lead.** The server records each peer's input-arrival margin in every frame header; the
  client feeds those margins into `LeadTracker`, steering a bounded tick bias (−2..+8) folded into the coupled
  slew / decoupled stretch so the WORST margin holds slightly positive. `net.input_delay` remains a manual
  floor; `net.perf`'s `lead_ticks` shows the dialed-in bias live.
- **`is_fresh` is fixed at the source** (#67, §6.3): keyed on input novelty, true exactly once per tick, on the
  first pass simulated with real input. The old high-water-mark and 256-tick held-cat workarounds in
  `weapon_authority.gd` are deleted; per-tick resim memos ride `NetRollbackHandle.memo_set`/`memo_get`
  (surfaced as `PlayerBody.net_memo_set`/`net_memo_get`).
- **Prop roles are real** (§4.3): State / Input / Cosmetic. `PlayerBody` passes its genuine cosmetics
  (`net_aim_held`, `net_aim_dir`, `net_flight_assist`, `net_rcs_lin`, `net_rcs_ang`) through
  `register_rollback_body`'s `cosmetic_properties` parameter — replicated, never restored during rollback,
  never counted as a misprediction.
- **Quantizers** (`core::quant`), per-property opt-in via `@` annotations in the registration arrays: `@ss3`
  packs a `Quat` (16 B) or rotation-only `Basis` (36 B) to 6 B smallest-three; `@half` packs `Vec3`/`Vec2`/`F32`
  components to IEEE binary16. History rows stay native stride, and the quantized value is **canonicalized at
  capture** (and written back to the object for State props), so every peer stores, compares and re-simulates
  the wire-representable value — masks stay byte-stable, the mispredict compare stays exact, resim stays
  bit-identical. Quantization is part of the schema hash. Positions stay lossless by policy (`net_pos`,
  `nin_aim_point`); orientations, rates, sticks and rotation frames are annotated in `player_body.gd`. One
  knowable trade: an `@ss3` State orientation snaps to a ~1.2e-4 grid each tick, so spins slower than
  ~0.15°/s hold instead of creeping.
- **AOI**: `net.aoi_radius` (server cvar, default 0 = off) makes the server send each peer only the rollback
  bodies within that radius of the peer's own body, with 1.25× exit hysteresis; state-lane entities always
  replicate. This is the 100-player egress lever.
- **`perf_metrics()` / `clock_metrics()`** keep the same keys and are live in ALL builds — the old
  debug-monitor gating is gone, so a production incident can be measured in the build that hit it.
- **Native crash capture**: `crash.rs` arms a signal/SEH handler that appends a backtrace to
  `user://logs/crash-native.log`. It lives here because this extension is first-party and loads in every build,
  including release exports where Godot's own crash handler is compiled out. See the README's crash section.

**Shipped session config: the 60 Hz decoupled tick.** The 30 Hz flip discussed in §2.3 is a one-liner
(`net.tick_hz 30`); the bandwidth math is unchanged.

**`orbitnet-core`** — pure Rust, never sees a `Variant`:

| Module | Covers |
|---|---|
| `tick` | rate clamping, catch-up backlog discard |
| `clock` | lowest-RTT-half offset filtering, bounded stretch |
| `history` | per-body dirty windows, and the per-body-vs-global resim assertion |
| `protocol` | order-sensitive schema hash, prop roles |
| `codec` | wire encode/decode, hostile-input sweeps |
| `columnar` | the packed per-entity history: fixed-stride ring, byte-exact changed masks |
| `freshness` | the input-confidence ledger behind the fix |
| `interest` | the AOI uniform grid — XZ-plane, rebuilt each tick from the position column |
| `pacing` | coupled-mode whole-tick slewing (`CoupledSlew`), per-peer input-lead tracking (`LeadTracker`) |
| `quant` | the smallest-three / binary16 quantizers |

**`orbitnet-godot`** — the binding layer:

| Module | Role |
|---|---|
| `orbit_net` | the session singleton: tick clock, packet pump, clock sync, metrics, and the per-entity rollback loop over a `BTreeMap` registry (stable replay order — the bit-exact resim gate would read a nondeterministic order as a phantom desync) |
| `sync` | `OrbitRollbackSynchronizer` (resolved bindings + packed rings + freshness ledger + memo ring) and `OrbitStateSynchronizer` (the no-rollback broadcast lane) |
| `binding` | the `Variant` ↔ packed-row bridge: `(Gd<Node>, StringName)` handles cached once at registration, role-aware restore |
| `interp` | `OrbitInterpolator`, the purely local render-interpolation drop-in |
| `crash` | the native crash handler |

Recipes:

```sh
just native-test       # cargo fmt --check, clippy -D warnings, cargo test
just native-build      # the release cdylib
just native-smoke      # load it in a throwaway Godot project and assert it works
just native-install    # build + install this platform's committed binaries
just native-check      # test + build + smoke, in CI order
```

---

## Why it exists

### The GDScript ceiling

The server's per-net-tick cost crossed the 16.6 ms budget of a 60 Hz net tick at **4–5 players**. Physics was
not the culprit — #214's collision LOD held and owned-body queries ran 6–16 µs/tick. The cost was the rollback
tick loop itself, in GDScript:

| Measurement (dedicated server, 4 remote bodies) | Value |
|---|---|
| `net` per frame | 81–85 ms at 7 fps |
| per net tick | **~10.5 ms** (~4 ms rollback bookkeeping + ~6 ms serialize/broadcast/input) |
| clients at 3 / 4 / 5 players | 144 fps / 41–56 fps / melted |

Once the server was over budget it fell behind, catch-up bursts ran up to 8 ticks per frame, and the backend
re-ran the **whole resim window on every tick**. A slow server acks late, every client's unconfirmed window
deepens, clients pin at `history_limit=128` — 2.1 s of history resimulated per tick — and land at 13–16 fps.

A first fix pass in GDScript (per-reference-tick encode dedupe, shared lag-comp) measured **neutral in the
melted regime**. That result is the useful one: it says the dominant costs are **per-RPC marshalling** (one
`rpc_id` per synchronizer × peer × tick) and **rollback-loop bookkeeping**, not the encode. Both are per-tick
GDScript work, and neither gets meaningfully cheaper without leaving GDScript.

The owner target is **100 players**. The old ceiling was about 4.

**What native code actually buys — and what it doesn't:**

| Lever | Needs native code? |
|---|---|
| Send batching — one packet per peer per tick | **Mostly.** Packing in GDScript re-pays the Variant cost it is meant to remove |
| Rollback bookkeeping / resim multiplier | **Yes.** Per-tick, per-body work |
| Snapshot serialization floor | **Yes.** Per-prop Variant encoding *is* the floor |
| Interest management (AOI) | **No.** The old backend shipped visibility hooks; the game it grew out of simply never set one |
| Prop diet, tick tiering | **No.** Configuration and refactoring |

**For 8–16 players, AOI plus a prop diet in GDScript would very likely have got there for a fraction of the
effort.** The rewrite is justified by the per-tick floor and by 100 players, not by AOI — plus a softer reason:
several of the behaviours in §6.1 and §6.3 were design problems rather than tuning problems, and they are
cheaper to fix in a backend we own.

**What made it tractable** was #61's boundary work: exactly one first-party file named the backend, no `.tscn`
instanced a backend node, and there was a single `_rollback_tick` implementer game-wide. The contract to
preserve was `Net`'s method surface plus `_rollback_tick(delta, tick, is_fresh)` — and game code changed
essentially zero lines at the cutover.

### The bookkeeping multiplier, precisely

The old backend computed a **single global window**: `from` = the earliest unconfirmed input across *all*
rewindables, `to` = the current tick, then `for tick in range(from, to)` emitting five signals per tick, each
fanning out to every synchronizer. Per net tick that is:

```
window_depth × synchronizers × 5 GDScript signal dispatches
```

and — the part that matters — **one late peer set the window depth that every body then paid.** That is the
structural reason a single bad connection melted the whole server, and why `history_limit=128` turned into
2.1 s of replay. Worse, the "can I simulate this body" predicate correctly declined to *simulate* an
out-of-range body, but apply/record still ran for every window tick: at a 40-tick window, 5 bodies and 29
properties, 5,800 GDScript property reads per frame doing nothing.

**The answer is per-body resim windows**: each body replays only from its own earliest dirty tick, with
apply/record gated on the same predicate as simulate, in a flat native loop instead of a signal fan-out. This
is a *design* change, not a language change — and it was the single highest-leverage item in the rewrite.
`core::history::ResimPlanner` implements it, and computes the old global number alongside the new one so the
difference is a live metric rather than a claim:

```
8 bodies, 7 healthy (1 tick behind), 1 straggler (100 ticks behind)
  global window:   100 ticks × 8 bodies = 800 body-ticks
  per-body plan:    7 + 100             = 107 body-ticks
```

### The bandwidth ceiling — read this before planning for 100 players

100 players at a 120 Hz physics-coupled net tick is **arithmetically out of reach**, and no implementation
quality fixes it. With AOI admitting ~12 visible entities per peer at ~60 quantized bytes each, per-peer
per-tick payload is roughly 750 B:

| Net tick rate | Server egress at 100 peers |
|---|---|
| 120 Hz (physics-coupled) | ~9 MB/s ≈ **72 Mbit/s** |
| 30 Hz (decoupled) | ~2.2 MB/s ≈ **18 Mbit/s** |

**The 100-player target therefore requires a decoupled net tick at 30 Hz.** The machinery exists and the
shipped config already runs decoupled at 60; the flip is `net.tick_hz 30`. Recorded here as a decision rather
than left to surface as a surprise.

---

## Architecture

### Crate layout

```
native/
  .gdignore                  <- makes the whole tree invisible to Godot's scanner
  Cargo.toml                 <- workspace
  rust-toolchain.toml        <- exact pinned toolchain
  crates/
    orbitnet-core/           <- PURE Rust, no godot dependency -> plain `cargo test`
    orbitnet-godot/          <- the cdylib; the only crate that knows Godot exists
  addon/
    orbitnet.gdextension     <- installed to addons/orbitnet_native/ beside the binaries
```

The split is load-bearing. Every algorithm lives in `orbitnet-core` with no Godot dependency, so the clock
model, codec, ring history and resim scheduler are testable in milliseconds and reusable outside Godot.
`orbitnet-godot` is a marshalling shell.

**The rule that keeps it honest: core never sees a `Variant`.** If a `godot` type appears in a core signature,
logic has leaked across the boundary.

### Class surface

Names are prefixed `Orbit`, which collides with neither the retired backend's class names nor the game's own
`Net*` family.

| Class | Kind | Role |
|---|---|---|
| `OrbitNet` | Node (autoload child of `Net`) | Tick loop, packet pump, rollback scheduler, metrics |
| `OrbitRollbackSynchronizer` | Node | Rollback state + input for one entity |
| `OrbitStateSynchronizer` | Node | Server-broadcast state, no rollback restore |
| `OrbitInterpolator` | Node | Render interpolation between net ticks |
| `OrbitVisibility` | Node | Per-entity AOI / visibility override |

**One autoload, not five.** The previous backend needed `NetworkTime`, `NetworkTimeSynchronizer`,
`NetworkRollback`, `NetworkEvents` and `NetworkPerformance`; OrbitNet needs `OrbitNet`, which reduced the
`project.godot` change at cutover to a single line.

One subtlety worth recording, because it is exactly the kind of thing that bites mid-cutover:
`OrbitRollbackSynchronizer` and `OrbitStateSynchronizer` contain the old `RollbackSynchronizer` and
`StateSynchronizer` as **bare substrings**, and `tools/net-check.sh` matches without word boundaries on purpose
(`\b` is a GNU extension that misbehaves on BSD/macOS grep). The first GDScript file to name the *new* classes
would therefore have failed the gate that exists to police the *old* backend. The pattern now carries a
`(^|[^A-Za-z_])` guard on those two symbols, so the new names pass while real legacy usage is still caught.

Everything the previous integration reached into privately is a plain public property, which **deleted three local
patches by construction**:

| Old local patch | Because | OrbitNet |
|---|---|---|
| `NetworkTime._tickrate` | no runtime setter | `OrbitNet.tickrate` |
| `NetworkRollback._input_delay`, `._display_offset` | read-only properties | plain properties |
| `set_sync_to_physics_runtime()` (patched in) | upstream setter pushed an error | `OrbitNet.sync_to_physics = false` |
| `sync.rollback_exempt` (patched in,) | no upstream concept | `simulate_on` / `exempt`, first-class |

For general-purpose use, the synchronizers carry real inspector configuration (`root`, `input_authority_node`,
`state_properties`, `input_properties`, `cosmetic_properties`, tier, AOI radius, priority). a typical consumer uses none
of it — it builds everything from code — but a public addon lives or dies on the inspector experience, and both
paths must produce an identical schema hash.

`input_authority_node` survives as the **authority seam only**: it says who may *write* input, not where input
properties live (input entries resolve against the state root). The server-authoritative split needs input
authority to differ from state authority; this is where that is expressed.

### The `Net` facade stays

`addons/orbitnet/net.gd` keeps its exact method surface; only the bodies changed at the cutover. It is what
preserves the **OFFLINE-is-a-total-no-op** invariant that ~92 call sites depend on, and it keeps
game-specific policy (`_remote_resim`, `_resim_force`, `_broadcast_input`) out of a general-purpose addon.

`register_rollback_body()` is also cheaper: the old `add_state` marked settings dirty and deferred a reprocess
once per property — 32 times per body spawn. OrbitNet assigns both arrays wholesale and processes once.

### The "one synchronizer type per body" constraint is gone

The old backend routed state and input over `@rpc` methods on a transmitter **child node path**. A different
node type on a remote meant a different path and RPC set, so replication would not route — which is why every
peer had to run the same synchronizer type for a given body.

OrbitNet routes by **entity id on a global channel**, with the schema negotiated in the handshake. A peer can
run a display-only synchronizer while the server runs a full one, and `enable_prediction`, tier and exemption
can differ per peer. It also removes a class of "the spawner renamed the node" replication bugs.

---

## Performance architecture

### One batched packet per peer per tick

**Transport path: `SceneMultiplayer.send_bytes()` plus the `peer_packet(id, packet)` signal.**

This is the most consequential decision here. `send_bytes` rides `SceneMultiplayer`'s own framing *above* the
peer, so it is transport-agnostic by construction — identical over `ENetMultiplayerPeer`,
`SteamMultiplayerPeer` and `OfflineMultiplayerPeer`. `NetTransport` and `steam_transport.gd` needed **zero
changes**.

Rejected alternatives, and why:

- **Raw `MultiplayerPeer.put_packet()`** — `MultiplayerAPI.poll()` consumes the peer's packets and there is no
  supported interception point.
- **`MultiplayerAPIExtension`** — full control, but it means reimplementing RPC routing for the 9
  `MultiplayerSpawner`s and the ~6 plain `@rpc` sites. Far too much risk for the benefit.
- **One `@rpc` per peer carrying a `PackedByteArray`** — the fallback. It still gets one call per peer per tick
  and keeps most of the win, at the cost of the RPC layer's per-call overhead.

Channels: state on an unreliable channel, input on another, handshake and entity binding on a reliable one.
The old backend set transfer *modes* but never a channel, so netcode traffic and reliable game RPCs shared a
stream.

The scaling law changed from `O(entities × peers)` **engine crossings** to `O(peers)` crossings plus
`O(entities × peers)` **bytes in native memory**. Acks piggyback on the frame header, so the separate reliable
per-peer ack RPC disappeared entirely.

### Snapshot capture and native history

**Native code cannot make a GDScript getter cheap.** Each of a character body's ~29 state properties is a
getter/setter proxy onto a `RefCounted` (`EvaState` / `InputFrame`); reading one costs a property lookup, a
GDScript call and Variant boxing. What OrbitNet changes is *how many times* that happens and *what happens
after*. In leverage order:

1. **Resolve once.** The `(object, StringName)` handle is cached at registration instead of re-walking a
   property table per access. *(shipped)*
2. **Capture once per tick, not once per window tick** — gating apply/record on the same predicate as simulate,
   combined with dirty-tick tracking. This is where the ~4 ms lived. *(shipped)*
3. **Columnar packed history.** A per-entity `Vec<u8>` of `history_limit × stride`, indexed `tick % capacity`,
   laid out in schema order. No `Dictionary`, no `Variant`, no per-tick allocation; the changed mask falls out
   of a `memcmp` on fixed-stride slices during capture. Roughly 300 B/row × 128 rows ≈ 38 KB per body.
   *(shipped)*
4. **An opt-in bulk hook** — `_orbit_capture_state(writer)` / `_orbit_apply_state(reader)`, moving ~29 property
   reads to one call. **Not implemented.** `eva_state_serialize_test.gd` shows `EvaState` can already flatten
   itself, so adoption would be cheap, but until it happens capture lands within 2–3× of the old backend's
   rather than 10×. This is the one remaining structural gap (§8).
5. **Quantization** — honestly a *bandwidth* lever, not a CPU fix; the owner's own A/B showed encode was never
   the bottleneck. *(shipped, §1)*

Two rules that are not negotiable:

- **`float` properties are stored as `f64`.** Godot's `float` is 64-bit; recording it as `f32` would round every
  replayed value and break bit-exact resimulation.
- **Never quantize the local prediction path.** The client predicts at full precision; only the server's
  authoritative broadcast is quantized, and quantization error must stay well below the reconcile snap
  threshold. The determinism gate runs against the lossless profile.

### Interest management, tiering and prop roles

AOI as shipped is a **radius + hysteresis filter** on the send path (`net.aoi_radius`). The uniform grid in
`core::interest` — rebuilt each tick from the position column already in native memory, zero Godot calls — is
implemented and tested but not yet driving sends; wiring it in is a contained follow-up for when entity counts
make the linear pass matter. Tick tiers are assigned statically per synchronizer and dynamically by distance
band, phase-offset by entity id so sends spread across ticks instead of spiking.

**The prop diet is expressed as per-property roles**, not a hand-maintained list:

- `State` — restored each replayed tick, compared for misprediction.
- `Input` — restored, but a change does not by itself trigger resimulation.
- `Cosmetic` — replicated, **never restored, never counted as a misprediction**, sent on a slow lane.

**The test for `Cosmetic` is "does the simulation ever read it back", not "does it look like presentation".**
`EvaState` already draws exactly this line, and the answer is narrower than it first appears:

- `rcs_cmd_lin`, `rcs_brake_lin`, `rcs_cmd_ang`, `rcs_brake_ang` → **genuinely cosmetic.** `eva_state.gd`
  marks them OUTPUT ONLY and excludes them from the rollback save unit, because nothing in the sim reads them
  back. `EvaStepper` rewrites them every tick from the same `(state, input)`, so a restore that drops them is
  corrected on the very next tick. Their only consumers are `ThrusterVfx` and `SuitSfx`.
- `gait_speed` and `smoothed_heading` → **`State`, despite looking presentational.** Both are *self-referential
  integrators*: `gait_speed` ramps toward a target from its own previous value and then sets surface-tangent
  velocity; `smoothed_heading` low-passes over its own previous value and returns the result as the walk
  direction. Not restoring either would make a replayed tick produce a different walk velocity and facing from
  identical authoritative state, failing the determinism gate and the client's reconcile.

So the prop diet is real but smaller than a first pass suggests. The mechanism is in `core::protocol` and
asserted by `cosmetic_props_are_excluded_from_the_misprediction_check`; the *classification* has to be made
per-property against "is it read back", and `eva_state.gd`'s comments are the authority.

### Threading

Main-thread only, non-negotiable: `SceneMultiplayer` I/O, `Object::get/set`, signal emission, `_rollback_tick`
and its physics queries.

Movable: per-peer frame assembly (delta, quantize, pack — pure native memory, parallel across peers), input
decode batches, AOI grid rebuild.

**Single-threaded today.** At small peer counts, thread overhead exceeds the work; a worker pool should be
feature-gated and enabled only above a peer threshold. A genuine Rust advantage worth noting: gdext's `Gd<T>`
is not `Send`, so "don't touch Godot objects from a worker" is a compile error rather than a crash report.

---
