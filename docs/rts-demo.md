# The RTS demo

`demos/rts/` — a two-seat skirmish RTS: 96 units, orders and combat, no economy and no buildings.

It exists to make an argument that a character-shooter demo cannot make: **which replication lane an entity
belongs on is decided by the game, not by preference**, and getting it wrong is invisible rather than loud.

```sh
just rts          # single player, no networking at all
just rts-host     # terminal 1
just rts-join     # terminal 2
```

---

## The lane split

| Lane | What is on it | Count |
|---|---|---|
| **Rollback** | the commander cursor | **one per player** |
| **State** | every unit | 96 |
| **Command** | every order | one channel per *seat* |

### Why exactly one rollback entity per player

The rollback lane exists for input that arrives **every tick** and can be **predicted from the last one**.
An RTS has exactly one thing like that — the command cursor — and it is not a unit.

The cursor is also the **AOI anchor**, and this is not a nice-to-have. `Net.set_aoi_radius()` finds the
rollback entity whose *input* authority is that peer and uses its first `Vector3` state property as the
centre. A peer with **no rollback body has no anchor**, and the backend correctly falls back to "everything
is in interest". So without a per-player rollback entity, interest management cannot function at all.

Putting the cursor there also demonstrates the server-authoritative split on a **non-character** entity,
which is the API shape people most often get wrong:

```gdscript
# CommanderAvatar — state authority: the SERVER
# CommanderInput  — input authority: the OWNING CLIENT (a child node, because authority is per-node)
Net.register_rollback_body(self, input,
    ["cmd_cursor@half"],                     # STATE    — and the AOI anchor, so it is registered FIRST
    ["nin_cursor@half"],                     # INPUT    — validated by node authority
    predict,
    ["cmd_sel_count", "cmd_drag@half"])      # COSMETIC — replicated, never restored
```

Its `_rollback_tick` is one line — clamp the client's requested cursor to the field — and that one line is
the moment a client-authored value becomes server-owned state.

### Why units are NOT on the rollback lane

Three reasons, and they compound:

1. **There is nothing to predict.** A unit's "input" is a sparse *order*, not a stream. Prediction
   extrapolates the next input from the last, and with orders the last input is almost always "nothing
   happened" — so prediction would be exactly as good as doing nothing, and would cost a replay to achieve
   it.
2. **Every rollback entity costs a `history_limit`-row ring plus a per-tick `memcmp` and a full replay.** At
   96 units that is 96 rings and 96 replays per tick to predict a value nobody can author.
3. **Decisively: the rollback lane restores recorded history onto its properties every tick.** An order
   arrives through a `NetCommand` handler, which runs *outside* the tick — so a goal written there would be
   overwritten by the next restore. Silently. On the server. And the unit would simply never move.

Point 3 is precisely what `make_state()`'s contract promises and `register_rollback_body()`'s does not, and
it is the demo's cleanest teaching moment.

### Why orders are per-seat channels, not per-unit

A `NetCommand` is a Node that routes by node path. 400 units would be 400 nodes and 400 registrations for a
channel that is naturally batched — an order names its units in the payload. One channel per **seat** also
buys a free forgery check: a request arriving on a channel that is not the sender's own is unambiguous
forgery, catchable before the payload is even parsed.

---

## The wire schema, and the arithmetic behind the unit count

**20 bytes of properties per unit per refresh:**

| Property | Bytes | What |
|---|---|---|
| `position@half` | 6 | `Vector3` as three binary16s. Free — `position` is already a `Node3D` property, so the server writes it once and the state lane picks it up with no shadow copy. |
| `net_aux@half` | 6 | `(sin θ, cos θ, hp01)` packed into one `Vector3`. |
| `net_meta` | 8 | an i64 bitfield: alive \| current target \| order sequence. |

### Facing goes as a direction pair, not an angle

This is the single most transferable trick in the demo, and a yaw scalar is wrong **twice over**:

1. **A GDScript `float` is an f64.** `"facing@half"` would silently fall back to lossless and save nothing —
   `@half` is valid only for `Vector3`/`Vector2`/`f32`, and an invalid pairing degrades quietly rather than
   erroring.
2. **Interpolating an angle across the ±π wrap sweeps the long way round.** A unit facing roughly south spins
   a full rotation every time it wobbles.

`(sin, cos)` costs 4 bytes as halves, interpolates correctly *because it is a point on a circle*, and rides
along in a `Vector3` that had a spare component anyway. `hp01` takes that component for the same reason: a
bare `float` hp cannot be narrowed at all, but as a normalized third component it costs 2 bytes.

### Where 96 comes from

- One UDP frame per peer per tick, **~1200-byte payload budget**.
- 20 B of properties + per-entity header ≈ **26 B/unit**.
- 1200 / 26 ≈ **46 units refreshed per peer per tick**.
- Entities are served **stalest-first**, so exceeding that does not drop anyone — it ages everyone.
- 2 seats × 48 = **96 units**: a full refresh every ~2 net ticks, i.e. ~100 ms worst-case age at 20 Hz.

That is a deliberate, comfortable 2× over the single-tick budget — enough that the round-robin is real and
visible in the HUD's staleness readout, not so much that the demo looks broken. Raise `UNITS_PER_SEAT` and
the staleness climbs linearly. That is the experiment, which is why the number lives in `RtsConfig` with the
derivation written above it.

---

## Determinism is not needed, and that is the point

The server is the only peer that runs the simulation; clients receive positions. **No client-side unit
resim means no lockstep**, and therefore:

- no fixed-point arithmetic,
- no cross-platform float discipline,
- no "why does this desync only on AMD" investigations,
- no restrictions on what the sim may touch.

The thesis, stated plainly:

> **Classic RTS lockstep buys bandwidth-independence at the cost of bit-exact determinism and full input
> latency. OrbitNet's server-authoritative model buys zero determinism engineering and predicted local input,
> at the cost of bandwidth — and bandwidth is the thing you can measure and tune.**

Everything on this page after "the wire schema" is that trade being managed. There is no equivalent lever on
the lockstep side; determinism engineering is a tax you either pay or fail.

The one place determinism *is* required is **node naming**, and that is a different kind of determinism —
agreement about names, not about floats. See below.

---

## Entity ids: the failure that is silent

OrbitNet derives an entity id as FNV-1a of the synchronizer root's **node path**. That is a good design — no
id-assignment handshake, no per-entity RPC routing, a reconnecting client re-derives the same ids — and it
has exactly one requirement: **every peer must build the same node paths.**

Godot's automatic naming does not do that. `add_child(Node3D.new())` produces `@Node3D@27`, and that number
is a per-process allocation counter: it depends on how many nodes the process has ever created, which depends
on the menu the player passed through, whether a probe attached, whether the editor is running. Two peers
*will* disagree, **no error is raised**, and the symptom is that replication goes nowhere — the server
broadcasts entity `0x8f3a…`, the client listens for `0x21bc…`, and every unit sits still.

So `RtsNames` names every replicated node explicitly (`U00000042`, `C01`, `Orders00`), and the demo asserts
it two ways:

- **A unit test** (`entity_name_test.gd`) pins the naming and the FNV implementation against published test
  vectors.
- **The probe** has each peer hash its sorted list of built node paths into a **world signature** and print
  it; `tools/rts-probe.sh` fails if the two differ. That proves *path equality*, which is the property id
  agreement is derived from.

## A static unit pool, not spawn/despawn

Every peer creates all 96 unit nodes at world build and the set never changes. Death sets a replicated
liveness bit; a respawn drip clears it.

This is a deliberate departure from "the server `queue_free()`s a dead unit", for the reason above: freeing
and re-creating a node means re-creating it at exactly the same path on every peer, in the same order, which
needs a spawn-replication mechanism — a real and interesting problem, and completely the wrong one for a demo
about replication *lanes* to also be about. A fixed pool makes path agreement true by construction.

It also costs nothing: a dead unit's properties stop changing, so the state lane's dirty tracking stops
sending it. A dead army is free.

## Validation

`OrderValidator` is a pure static function over (sender's seat, wire payload, world state), so the hostile
cases are unit-testable with no session. Four rules:

1. **Foreign-seat ids reject the WHOLE batch.** Not filtered down to the legal subset — filtering would let
   an attacker probe ownership by watching which of a mixed batch moved, and no honest client can produce
   one, since your own selection contains only your own units.
2. **Dead ids are silently dropped.** This looks like the same case and is the opposite one: a unit dying
   between the click and the server's receipt is an ordinary race at any latency. Ownership is a *permission*
   question; liveness is a *timing* question. Conflating them gives you either a security hole or an
   infuriating game.
3. **Cardinality is capped**, checked before the per-id loop — otherwise one packet buys unbounded server
   work.
4. **Every `Vector3` component must be finite.** A wire-decoded `NAN` propagates through every arithmetic
   operation, never compares equal to anything, and surfaces far from where it entered. Rejected at the
   boundary, and absorbed again in depth by `UnitSteering`.

Plus a per-sender token bucket, checked *first* because it is the cheapest rejection.

**Selection is entirely client-local.** It changes at mouse-move rates, the order payload names its ids
explicitly, and a client-authored id list is safe by construction because the server re-derives ownership
from the ids themselves. Only the *count* rides the wire, on the cosmetic channel.

## Order RTT — the number no other demo shows you

Every netcode demo shows you ping. Ping is the transport's round trip and it is **not what a player feels**.

What a player feels in an RTS is **click-to-adjudicate**: from releasing the mouse to the moment the world
visibly agrees. That path is client → reliable RPC → server validation → authoritative state change →
state-lane broadcast (which waits for the next net tick, *and* for this entity's turn in the stalest-first
rotation) → client apply. It contains the tick rate, the send budget and the round-robin — everything the
HUD's levers trade against — and none of that is in ping.

Measuring it needs **no new networking**: the server stamps each accepted order with a sequence number onto
every unit it named, and that number already replicates inside `net_meta`. The client records the sequence it
saw at send time and stops the clock when any targeted unit's changes.

## AOI, reported honestly

The HUD reads:

```
AOI     128 m -- ROLLBACK LANE ONLY: 1/1 cursors culled, 0/96 units (state lane is never culled)
```

Because that is the truth. AOI iterates rollback entities; the 96 units are on the state lane and always
replicate. In this demo AOI can cull exactly one thing — the other player's cursor.

Showing that teaches the lane distinction better than a radius slider that appears to do something would.
Making AOI work on the state lane is [a filed gap](../README.md#known-gaps), not a demo bug.

## The rest of the architecture, briefly

- **Movement is a pure function.** `UnitSteering.step()` over a plane and an AABB list — no
  `CharacterBody3D`, no physics ray (ground picking is `Plane.intersects_ray`). The whole sim step is
  unit-testable from plain data, and "why did the client see something different?" has one candidate answer
  instead of two.
- **One `_step()` body, two clocks.** Stepped from `Net.pre_tick` when networked and from a fixed
  accumulator offline, at the same `dt`, so "it behaves differently offline" cannot happen quietly.
- **Rendering is `MultiMeshInstance3D`.** A `UnitBody` carries netcode state and *nothing else* — no mesh, no
  material, no children but its synchronizer. 96 replicated units cost 6 draw calls, and the netcode entity
  and the render representation are demonstrably separable. The barrel on each unit exists so that replicated
  *facing* is legible: without it, a capsule has no visible orientation and the `(sin, cos)` packing would
  look like a pointless flourish.
- **Formation slots, not neighbour separation.** An order to 24 units must not be 24 units ordered to the
  same point. Separation would be an O(n²) force loop whose output depends on iteration order; giving each
  unit its own destination removes the contention at the source and is trivially testable.
- **No win condition.** A respawn drip keeps the fight in a steady state, so the demo can be left running
  while someone watches the diagnostics.

## What the demo deliberately omits

Listed rather than hidden, because a demo that pretends to be a game is a worse demo:

- **No build/protocol version handshake.** Two incompatible peers connect and misbehave rather than being
  refused with a reason.
- **No join browser, no invites, no reconnection with seat retention.**
- **No InputMap.** Raw key constants, so the demo has no project-settings dependency to break across Godot
  versions. A real game should use one.
- **No fog of war.** A `VisionGrid` on one state entity would be presentation-only and maphack-vulnerable,
  because closing it needs a per-peer visibility veto the library does not have yet.
- **No pathfinding.** Units slide along boxes. A navmesh would be a second system to explain.
