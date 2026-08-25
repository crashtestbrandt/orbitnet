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
4. **One crossing per lane, optionally.** A synchronizer that declares a bulk hook is handed a preallocated
   `Array` and fills or reads it in a single `Object::call`, so the count drops from `S` per lane per tick to
   `1`. Opt-in per synchronizer; a lane that declares none keeps the walk, byte for byte. See
   [api.md](api.md#bulk-marshalling-one-crossing-per-lane-per-tick).
5. **Quantization** — a *bandwidth* lever, not a CPU one. See
   [api.md](api.md#wire-quantization-and-the-scalar-reality).

**The rollback loop is what makes this leverage worth having.** Restore, simulate and record run per replayed
tick per entity, so a body with `S` state props replaying `D` ticks pays `S × D` reads and up to `S × D` writes
in one frame — the same walk the state lane pays once. `restore_ms` / `sim_ms` / `record_ms` report the three
phases apart, and `resim_force` fixes `D`, so the effect of a change here is measured rather than asserted.

Three directions, and each covers a different walk:

| direction | the walk it replaces | multiplier |
|---|---|---|
| `capture` | reading the game's values into a row | replayed ticks x planned entities |
| `restore` | writing a recorded row back before a replayed tick | replayed ticks x planned entities |
| `apply` | landing a **received** row, and the **quantized write-back** after a record | delivered blocks for the first (no replay multiplier), replayed ticks for the second |

`apply` earns its place for a reason the multiplier does not show: **a peer that simulates nothing plans no
entities**, so the rollback loop returns on an empty plan and the receive walk is that peer's entire per-tick
crossing count. Neither other hook reaches it.

**Its slot list is the CAPTURE list, not the restore list** — they differ by the lane's `Cosmetic` entries,
which are replicated and never restored, and a received row must land those too. The write-back routes through
the hook only when the lane carries at least two quantized properties; below that the targeted walk is cheaper
and is what runs.

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
sends because it is **slower** at the arena extents a session runs at. It overtakes the scan between ±300 m and
±600 m of occupancy and is about twice as fast past ±1200 m; the shipped arenas are ±74 m. A high world count
looked like a grid win and was not — what the grid saved there was the per-peer candidate rebuild, and that is
now dropped without a grid: **one candidate list per tick**, with the rows a peer drives patched in around its
call and "this peer cannot be located" said in the center rather than by reshaping every row. Worth 2.35× of
the interest pass in a 32-world session, and it removes the only reading under which the grid won one. The
measured tables and the decision are in that module's header, and `net.perf`'s `interest_ms` is the live number
that would reopen it.

**Membership is the second axis.** A radius cannot separate several independent worlds inside one session, each
rebased near its own coordinate origin: two entities at the same coordinates in different worlds are zero
meters apart. Every candidate and every observer carries a membership id, and a candidate whose id differs from
the observer's is refused before any distance is computed. `0` is the default on both sides and matches every
world, so a game that declares none is filtered on distance alone.

The two axes are independent, which is what makes membership usable by the channels that need it most. A state
channel that replicates no position — health, inventory, a door's state — has no distance to be culled by, so
its only lever was all-or-nothing. Declaring it always-relevant *within one membership* bounds it to its own
world while leaving it uncullable inside it.

**The per-peer veto is the third axis.** Distance and membership are both properties of the entity: one
position, one world, read the same way by every peer. Neither can express "not this peer", which is the
exception a class-wide key leaves over. `Net.set_entity_hidden(peer, entity_id, hidden)` records that refusal on
the peer's own `PeerInterest`, where the filter applies it before membership and before the radius, and where
`always` does not survive it. Refusing at the candidate rather than at the cap is what keeps a withheld entity
out of `max_entities`' population; starting a veto also drops the entity from the set in that call and clears
the three delta entries a leave clears, because no `leaves` list will ever name a removal that happened between
updates. The shared candidate list is untouched by any of it, so the veto costs the per-tick pass nothing.

The veto stops the rows and nothing else. That is the client-side contract a distance cull already has, and it
inherits the same rule — **nothing despawns**, so the withheld entity's node stays where it was. What the client
is now told is *which* entities stopped: the per-peer diff the send path already computed to clear its own
delta bookkeeping rides the snapshot as a flag-guarded trailing section, and `Net.entity_left_interest` /
`Net.entity_entered_interest` publish it. Two bytes per changed entity, on the ticks the set changed, and
nothing at rest. The addon still frees nothing; the game decides what a leave means. Ids stay session-global
either way: the entity manifest goes to every synced peer whatever any one of them receives.

**A seat's center and its world both come from one body**: the lowest-id entity whose *input* authority is that
peer, which declares that seat, and which resolved an anchor. A connection with no such body on any seat has
neither, and the backend falls back to "everything is in interest" — every world, at every distance, and not
bounded by the nearest-N cap either, because an entity with no distance is kept as uncullable and an uncullable
entity occupies no slot in it. This is the behavior most likely to surprise you. `Net.peer_anchor()` reports
which case a connection is actually in, and `Net.set_unanchored_policy(CLOSED)` makes the fallback "receive
nothing" instead — see [api.md](api.md#interest-three-axes-distance-membership-and-the-veto).

**A connection may hold several seats, and the filter runs once per seat.** A seat is one owned, predicted body
behind one transport peer; local split-screen is two or more, and the sentence above is per seat: the anchor is
the lowest-id entity whose input authority is that peer AND which declares that seat. Relevancy is a property of
a viewpoint, so each seat gets its own center, world, hysteresis band and cap — while the delta base, the ack
window, the veto and the byte budget stay per connection, because those are properties of a datagram. What the
datagram carries is the **union** of the connection's seats, holding the **nearest** seat's distance per entity,
and an entity leaves only when every seat has let go of it. Every body is on seat `0` until
`NetRollbackHandle.assign_seat()` or `set_seat()` says otherwise, which is one seat per connection and is what
every connection had before seats existed.

**The fail-open is per connection, which is what makes a seat ARRIVING cheap.** Because the datagram carries the
union, a seat with no resolved anchor would refuse nothing and so open the whole connection to every world — a
full-state burst caused by a body that has not spawned yet, arriving exactly as a player is being seated. So an
unresolved seat contributes no viewpoint while any other seat on the connection has one; only a connection where
nothing resolved sees everything.

**A seat is derived from `(input owner, seat label)` and the roster is announced from that.** Nothing holds a
seat table beside ownership — a second source of truth about who drives a body is one that can disagree with the
anti-forgery check on received input. The server rescans its registry once per frame, diffs the deduplicated
pairs against what it last announced, and emits `Net.seat_opened` / `Net.seat_closed`; the entity manifest
carries the same two columns per entity, so a client projects the roster from the manifest rows it holds — a
delta patches that table rather than replacing it — and emits the same two events one manifest later.

**That inference is a fallback, and `Net.set_peer_anchor()` replaces it.** What a peer observes is a different
question from what its input drives — a spectator drives nothing, and a peer with a body in each of two worlds
observes one of them — and the inferred world is read off whichever body sorts lowest by FNV hash, which makes
a peer driving two bodies in different worlds undefined. The declaration states the center and the world
together and is authoritative for both. Its two axes fail separately: a tracked entity that has not spawned
gives no center, so nothing is distance-culled, while the peer stays in the world it was declared into, because
a membership is a declaration and did not fail.

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
