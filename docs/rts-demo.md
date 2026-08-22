# The RTS demo

`demos/rts/` — two seats, 96 units, orders and combat. No economy, no buildings.

It makes an argument a character-shooter demo cannot: **which replication lane an entity belongs on is decided
by the game**, and getting it wrong is invisible rather than loud.

```sh
just rts          # single player, no networking at all
just rts-host     # terminal 1
just rts-join     # terminal 2
```

The counterpart is [hockey-demo.md](hockey-demo.md): a coupled 60 Hz tick where everything is on the rollback
lane, including a puck nobody authors. Between them the two demos cover both halves of the lane decision.

## The lane split

| Lane | What is on it | Count |
|---|---|---|
| **Rollback** | the commander cursor | **one per player** |
| **State** | every unit | 96 |
| **Command** | every order | one channel per *seat* |

**Seat** here is the backend's word for the same thing: one owned, predicted body behind a connection. This
demo's roster is a bijection — one seat per peer, which is `NetRollbackHandle.set_seat()` left at its default
of `0` — so "seat" and "connection" name the same thing throughout it. They stop naming the same thing under
local split-screen, where one connection holds several seats; see
[api.md](api.md#seats-several-owned-bodies-on-one-connection).

### Why exactly one rollback entity per player

The rollback lane exists for input arriving **every tick** that can be **predicted from the last one**. An RTS
has exactly one thing like that — the command cursor — and it is not a unit.

The cursor is also the **AOI anchor**, which is not a nice-to-have: `set_aoi_radius()` finds the rollback
entity whose *input* authority is that peer and uses its first `Vector3` state property as the centre. A peer
with **no rollback body has no anchor**, so without one, interest management cannot function at all.

It also demonstrates the server-authoritative split on a **non-character** entity — the API shape people most
often get wrong:

```gdscript
# CommanderAvatar — state authority: the SERVER
# CommanderInput  — input authority: the OWNING CLIENT (a child node, because authority is per-node)
Net.register_rollback_body(self, input,
    ["cmd_cursor@half"],                     # STATE    — and the AOI anchor, so it is registered FIRST
    ["nin_cursor@half"],                     # INPUT    — validated by node authority
    predict,
    ["cmd_sel_count", "cmd_drag@half"])      # COSMETIC — replicated, never restored
```

Its `_rollback_tick` is one line — clamp the client's requested cursor to the field — and that line is the
moment a client-authored value becomes server-owned state.

### Why units are NOT on the rollback lane

Three reasons, and they compound:

1. **Nothing to predict.** A unit's "input" is a sparse *order*, not a stream. Prediction extrapolates the
   next input from the last; with orders the last input is almost always "nothing happened", so prediction
   would be exactly as good as doing nothing and would cost a replay to achieve it.
2. **Cost.** Every rollback entity is a `history_limit`-row ring plus a per-tick `memcmp` and a full replay.
   At 96 units that is 96 rings and 96 replays per tick to predict a value nobody can author.
3. **Decisively: the rollback lane restores recorded history every tick.** An order arrives through a
   `NetCommand` handler, which runs *outside* the tick — so a goal written there is overwritten by the next
   restore. Silently. On the server. The unit simply never moves.

Point 3 is exactly what `make_state()` promises and `register_rollback_body()` does not.

### Why orders are per-seat channels, not per-unit

A `NetCommand` routes by node path, so 400 units would be 400 nodes and 400 registrations for a channel that
is naturally batched — an order names its units in the payload. One channel per **seat** also buys a free
forgery check: a request on a channel that is not the sender's own is unambiguous forgery, catchable before
the payload is parsed.

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

A deliberate, comfortable 2× over the single-tick budget — enough that the round-robin is real rather than
hypothetical, not so much that the demo looks broken. Raise `UNITS_PER_SEAT` and the refresh interval climbs
linearly. That is the experiment, which is why the number lives in `RtsConfig` with the derivation above it.

Measuring that from the outside takes care: "ticks since this unit last changed" counts a **stationary** unit
as starving, which it is not. The probe records the gap between *consecutive updates* of a unit known to be
moving, which is the round-robin interval and nothing else. A per-entity staleness counter belongs in the
library — it is one of the [filed gaps](../README.md#limits).

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

Everything after "the wire schema" is that trade being managed. There is no equivalent lever on the lockstep
side; determinism engineering is a tax you either pay or fail.

The one place determinism *is* required is **node naming** — agreement about names, not about floats.

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

A deliberate departure from "the server `queue_free()`s a dead unit", for the reason above: re-creating a
freed node means re-creating it at exactly the same path on every peer, in the same order, which needs a
spawn-replication mechanism — the wrong problem for a demo about *lanes* to also be about. A fixed pool makes
path agreement true by construction, and costs nothing: a dead unit's properties stop changing, so the state
lane stops sending it. A dead army is free.

## Validation

`OrderValidator` is a pure static function over (sender's seat, wire payload, world state), so the hostile
cases are unit-testable with no session. Four rules:

1. **Foreign-seat ids reject the WHOLE batch**, not filtered down to the legal subset — filtering would let
   an attacker probe ownership by watching which of a mixed batch moved, and no honest client sends one.
2. **Dead ids are silently dropped.** This looks like the same case and is the opposite one: a unit dying
   between the click and the receipt is an ordinary race at any latency. Ownership is a *permission*
   question, liveness a *timing* one; conflating them gives you a security hole or an infuriating game.
3. **Cardinality is capped**, before the per-id loop — otherwise one packet buys unbounded server work.
4. **Every `Vector3` component must be finite.** A wire-decoded `NAN` propagates through every operation,
   never compares equal to anything, and surfaces far from where it entered.

Plus a per-sender token bucket, checked *first* as the cheapest rejection.

**Selection is entirely client-local.** It changes at mouse-move rates, the payload names its ids explicitly,
and a client-authored id list is safe by construction because the server re-derives ownership from the ids.
Only the *count* rides the wire, on the cosmetic channel.

## Order RTT

Ping is the transport's round trip and it is **not what a player feels**. What a player feels is
**click-to-adjudicate**: from releasing the mouse to the world visibly agreeing. That path is client →
reliable RPC → server validation → authoritative state change → state-lane broadcast (waiting for the next
net tick *and* for this entity's turn in the stalest-first rotation) → client apply. It contains the tick
rate, the send budget and the round-robin — everything the HUD's levers trade against, none of it in ping.

Measuring it needs **no new networking**: the server stamps each accepted order with a sequence number onto
every unit it names, and that already replicates inside `net_meta`. The client records the sequence it saw at
send time and stops the clock when any targeted unit's changes.

## AOI, reported honestly

The HUD reads:

```
AOI     128 m -- ROLLBACK LANE ONLY: 1/1 cursors culled, 0/96 units (state lane is never culled)
```

Because that is the truth: AOI iterates rollback entities, the 96 units are on the state lane and always
replicate, so AOI can cull exactly one thing — the other player's cursor. Showing that teaches the lane
distinction better than a slider that appears to do something. Making AOI work on the state lane is
[a filed gap](../README.md#limits), not a demo bug.

## The rest, briefly

- **Movement is a pure function.** `UnitSteering.step()` over a plane and an AABB list — no
  `CharacterBody3D`, no physics ray (ground picking is `Plane.intersects_ray`). The whole sim step is
  unit-testable from plain data, and "why did the client see something different?" has one candidate answer
  instead of two.
- **One `_step()` body, two clocks.** Driven by `Net.pre_tick` when networked, by a fixed accumulator
  offline, at the same `dt` — so "it behaves differently offline" cannot happen quietly.
- **Rendering is `MultiMeshInstance3D`.** A `UnitBody` carries netcode state and nothing else: no mesh, no
  material, no children but its synchronizer. 96 replicated units cost 6 draw calls, and the netcode entity
  and the render representation are demonstrably separable. Each unit's barrel exists so replicated *facing*
  is legible — a capsule has no visible orientation, and without it the `(sin, cos)` packing looks like a
  flourish.
- **Formation slots, not neighbour separation.** Separation is an O(n²) force loop whose output depends on
  iteration order; giving each unit its own destination removes the contention at the source.
- **No win condition.** A respawn drip holds a steady state, so the demo can be left running.

## Deliberately omitted

- **No version handshake.** Two incompatible peers connect and misbehave rather than being refused.
- **No join browser, invites, or reconnection with seat retention.**
- **No InputMap** — raw key constants, so there is no project-settings dependency to break across Godot
  versions. A real game should use one.
- **No fog of war.** It would be presentation-only and maphack-vulnerable: closing it needs a per-peer
  visibility veto the library does not have.
- **No pathfinding.** Units slide along boxes.
