# The air hockey demo

`demos/hockey/` — a table, a puck, and up to 32 mallets on alternating ends. No menu, no match end.

It makes an argument the RTS demo cannot: **the rollback lane is not only for the body you author**. The puck
is authored by nobody, and every peer predicts it through the real simulation and reconciles it against the
server. How far wrong that prediction was, in millimeters, is the number on screen.

```sh
just hockey        # single player, no networking at all
just hockey-host   # terminal 1
just hockey-join   # terminal 2, and 3, and 4 …
```

`hockey-join` takes `ADDR` or `ADDR:PORT`, defaulting to `127.0.0.1:47800` — so a session hosted on a
non-default port with `just hockey-host 47900` is reached with `just hockey-join 127.0.0.1:47900`.

The counterparts are [rts-demo.md](rts-demo.md), a decoupled 20 Hz tick where the lane an entity belongs on is
the whole question, and [arena-demo.md](arena-demo.md), a decoupled 30 Hz shooter about who receives what.

![A client's view of a three-peer air hockey session](img/hockey-demo.png)

<sub>A client at seat 2, with a host and one other client connected. Every number on the left is live.</sub>

## The configuration, and why it is a separate project

`demos/rts/project.godot` names this demo before it existed: *"a second demo shaped like a shooter would want
`sync_to_physics=true` at 60 Hz with a 128-tick history. Those cannot coexist in one `project.godot`."*

| `[orbitnet]` | air hockey | RTS |
|---|---|---|
| `sync_to_physics` | `true` — **coupled** | `false` |
| `tickrate` | `60` | `20` |
| `history_limit` | `128` | `64` |

Coupled means one net tick per physics frame, run before the physics step, with the clock pinned to a stretch
of exactly 1.0. `HockeyNet` therefore calls **no** `set_net_tick_decoupled()` — the one session-layer ordering
rule the RTS demo has that this one does not.

## The lane split

| Lane | What is on it | Count |
|---|---|---|
| **Rollback** | every mallet — state on the body, input on a client-authority child | 32 |
| **Rollback** | **the puck** — state only, no input, predicted on every peer | 1 |
| **State** | the scoreboard | 1 |
| **Command** | `serve`, one channel for the whole rink | 1 |

### The puck is registered with an empty input list

```gdscript
# PuckBody.bind_net() — the puck is its own input node; there is no input to carry.
Net.register_rollback_body(self, self,
    ["net_pos@half", "net_vel@half", "net_flags"],   # STATE
    [],                                              # NO INPUT — nobody authors the puck
    true)                                            # predicted on EVERY peer
```

The backend's roles fall out of that:

- **Server** — owns the state, and an entity with no input props is stamped authoritative every tick, so its
  simulation is the truth by construction.
- **Client** — owns neither the state nor the input, but prediction is enabled and it is not exempt, so it
  simulates the puck locally and reconciles when the server's row lands.

**The mallets need the switch re-declared, and the puck does not.** A client builds its rink before the roster
arrives, so every mallet registers with `predict = false` — which does not merely defer prediction, it exempts
the mallet from the rollback loop. `MalletBody.set_owner_peer()` calls
[`NetRollbackHandle.set_predicted()`](../docs/api.md#prediction-is-a-switch-and-it-does-not-move-on-its-own) on
the tick a seat is handed over. Without it a player's own mallet still moves, because an exempt body applies
the rows it receives — it is just a full round trip behind the mouse, with nothing on screen saying so. The
HUD's `PREDICT` line exists to make that flag visible rather than felt.

### The state set has to be the whole simulation state

`net_vel` is on the wire because a restore that returned position without velocity would resume the resim from
the wrong basis and diverge on the very next tick. The same goes for `net_flags`, which carries liveness, the
face-off countdown and the serve sequence: the simulation reads all of it, so all of it has to ride the lane
that gets restored.

**The failure mode looks like a physics bug and is a schema one.** The RTS demo never had to make this point —
its units are display-only and its cursor has no momentum.

### What a client cannot predict

Two things, both correct rather than gaps:

- **An opponent's strike.** Rollback input travels client → server and is never rebroadcast, because that
  would be an O(N²) input fan-out. Nothing in a client's possession implies where somebody else's mallet went.
  `Net.set_remote_resim(true)` at least lets the other mallets coast forward through the resim instead of
  standing still; `HockeyNet` turns it on at session start and **F2** toggles it.
- **A serve.** A `NetCommand` handler runs outside the tick and writes a server-only field, so a client sees
  its own serve one round trip after asking for it.

## The score, and `is_fresh`

A goal is discovered inside the puck's `_rollback_tick`, and two rules keep it from being counted twice:

1. **It is awarded on the `is_fresh` pass.** The backend consumes freshness exactly once per tick, so a resim
   over the same tick does not award it again.
2. **The score lives on the state lane.** The rollback lane restores recorded history onto its properties
   every tick, so an increment stored on the puck would be overwritten by the next restore — silently, on the
   server, with the score sitting at nil-nil and nothing erroring.

Point 2 is the README's headline warning applied in the direction people do not expect: the usual case is a
value written *outside* the tick landing on the rollback lane, and this is the same rule from the other side.

**Documented cost:** a goal committed on the fresh pass is **not** un-awarded if a later correction invalidates
the tick. `NetRollbackHandle.memo_set`/`memo_get` is the primitive for that case; the demo uses it for the
serve and not for the score, and at a 60 Hz coupled tick the window is a few milliseconds wide.

## The correction, measured

`ReconcileMeter` records the puck's position the **first** time a tick is simulated and compares on every later
pass over that same tick. A later pass only happens because an authoritative row arrived and the backend
rewound to replay from it, so the difference between the two answers for one tick *is* the correction.

**It is keyed on visitation, deliberately not on `is_fresh`.** `is_fresh` is keyed on *input* novelty, and an
inputless puck on a client is never fresh at all — it would report nothing, forever. What this needs is "has
this tick been simulated before", which is the plain high-water mark that [protocol.md](protocol.md#is_fresh)
correctly calls the *wrong* definition of `is_fresh`. Both are right about their own question.

The HUD prints the measurement's floor beside it:

```
PUCK CORRECTION  p50=141.6 mm  p99=598.5 mm  peak=673.9 mm  n=228
        replayed 228 of 1408 ticks   view: 18 blended, 42 snapped   wire floor ~0.98 mm (@half)
```

**Expect roughly one round trip of puck travel.** The puck runs at up to 6 m/s, so at a 17 ms round trip a
strike this peer could not know about is already ~100 mm stale by the time the row lands, and the divergence
keeps growing across the replayed window. The figures above are three bots chasing the puck continuously,
which is harsher than people playing; leave the table untouched and the client's prediction is **exact**, to
the sample — a deterministic body with no input has nothing to get wrong.

### What is deliberately not in the distribution

Two things are corrections and are not *drift*, and each was large enough to be the entire number before it was
excluded:

| Excluded | Why |
|---|---|
| **The join sync** | A client builds its own puck and predicts it forward before any authoritative row has arrived, so the first row rewinds it onto a puck that was somewhere else. A third of a meter is typical. It happens once per session, but the percentile window holds it for the rest of the run — measured on an otherwise untouched puck it was the *only* sample ever taken, so p50, p99 and peak all reported that one join. |
| **A face-off** | The puck is teleported to the center spot, so a peer that placed the goal one tick differently differs by half a table. Real, and a different quantity from drift. |

So a sample is recorded only for a tick the puck was **live on both passes**, and only after the first
authoritative row has landed. The number then means one thing.

`net_pos` rides as three IEEE-754 binary16s, whose spacing near a table coordinate of 1 m is about a
millimeter, and the backend writes the quantized value back after every record so every peer replays from the
same canonical basis. A correction cannot be measured below that. The table is sized in meters partly for this
reason: ten times the table and the floor would be the number.

### The view absorbs the correction, and knows when not to

A predicted puck travels up to 100 mm per tick, so smoothing its *position* toward the simulation would render
it lagging behind itself. `PuckView` smooths the **discontinuity** instead: it extrapolates the previous pose
forward, takes the difference from the authoritative one as an offset, and bleeds the offset away over
`CORRECTION_HALF_LIFE`.

Two discontinuities are the simulation's own and must not be absorbed, or the puck renders passing through a
rail and sliding back into it — a **rail or mallet contact**, and a **face-off**. `PuckPhysics.State` carries a
contact count for exactly that reason. Anything past `CORRECTION_SNAP_M` snaps instead of blending, and the
blended and snapped counts are what `BenchSubject.KEY_RECONCILE_SMOOTH` and `KEY_RECONCILE_SNAP` were defined
for. This demo is the first to fill them.

The counts are worth reading together with the p50: under three bots the median correction is above the snap
threshold, so most of them **snap rather than blend** — the smoothing arm cannot hide a correction bigger than
the distance a blend could plausibly cover in its half-life. Raising the threshold would move corrections out
of the snap column and into a visible slide, which is worse; the honest reading is that at that intensity there
is nothing to hide.

A correction below `CORRECTION_DEADBAND_M` is not counted at all. The extrapolation the detector compares
against is a straight line while the simulation damps and substeps, so every tick disagrees by a fraction of a
millimeter whether or not anything was corrected — without the deadband the blended count climbed once per
tick forever, including offline, where nothing is corrected.

## Teams and seating

- **Seat parity fixes the end.** Even seats defend `-z`, odd seats defend `+z`, and a player's team is derived
  from the seat index rather than replicated.
- **A joiner takes the lowest free seat on the thinner end**, ties to team 0. On an empty table that is strict
  alternation — 0, 1, 2, 3 — and after a drop-out it refills the side that lost a player rather than deepening
  a 3-v-1.
- **Drop-in and drop-out are the same mechanism.** A peer connecting takes a seat, a peer leaving releases it,
  and the whole seat table is rebroadcast either way over a reliable RPC — which is what re-points each
  `MalletInput`'s multiplayer authority on every peer.
- **One seat per peer, which is the backend's default.** The backend seats a *body*, not a connection, and
  `NetRollbackHandle.set_seat()` lets one connection drive several — local split-screen. This demo declares
  none, so every mallet is on seat `0` and "seat" and "connection" name the same thing throughout it. See
  [api.md](api.md#seats-several-owned-bodies-on-one-connection).
- **Mallets do not collide with each other.** Team-mates may overlap, and the renderer fades the nearer one
  instead of pushing it away. Pushing would put a rule in the simulation to solve a drawing problem, and every
  peer would then have to predict it.

### 32 seats is a wire fact, not a gameplay one

Every peer builds all 32 mallets at world build with identical names, and the node set never changes. OrbitNet
derives an entity id from a node **path**, so a mallet created after world build would have to be created at
the identical path on every peer, in the same order — which needs spawn replication, a real problem and the
wrong one for this demo to also be about. The ENet peer cap is set to the same number, so the peer after the
last seat is refused by the transport.

A vacant seat's mallet is parked, undrawn, skipped by the puck's collision pass, and stops changing — so the
delta tracking stops sending it. An empty table is free.

## The table

**Table space is 2D and axis-aligned**: `x` across (±0.5 m), `z` along toward the goals (±1.0 m), `y` always 0.
The incline lives entirely in the view node's transform, so no simulation code knows the table is tilted and a
body's `position` is already its table-space coordinate.

The camera is **fixed** — no pan, no zoom, no edge scroll. Its distance is *solved* from the table corners
rather than tuned: `TableFraming.min_distance()` returns the distance at which the tightest corner still sits
inside the frustum with a margin, and `table_framing_test.gd` asserts it holds at seven aspect ratios. Godot's
`Camera3D` defaults to `KEEP_HEIGHT`, so a narrow window has *less* horizontal room; `TableView` re-solves on
resize.

**The one thing that is not fixed is which end faces you**, chosen once when your seat is assigned. Playing
from the top of the screen is not a camera control, it is a defect.

## Within-tick entity ordering

The puck reads every mallet's pose; nothing writes the puck's. Whether a given mallet has already advanced when
the puck's tick runs depends on the backend's replay order, which is **ascending entity id** — its planner
keeps bodies in a `BTreeMap` precisely so replay order cannot vary. An entity id is FNV-1a of the node path, so
the order is identical on every peer and is already covered by the world signature. At 60 Hz the difference
between reading a mallet at the start or the end of a tick is under a centimeter.

## The levers

| Key | What it moves |
|---|---|
| `F1` | net tick 60 ↔ 30 Hz. Each correction covers twice the travel at 30. |
| `F2` | `remote_resim`. **Off is "stop predicting, draw what arrives"** — it exempts every body this peer owns neither the state nor the input of, the other mallets *and* the puck. |
| `F3` | `input_delay` — shrinks the unconfirmed window by stamping input into the future. |
| `F4` | `display_offset` — presents an older, more-confirmed tick. |
| `F5` | correction smoothing. Off leaves the puck exactly where the wire put it. |
| `F6` | team-mate fade. |
| `F7` | **bulk marshalling.** Off puts every lane back on the per-property walk; `restore_ms` and `record_ms` move. |
| `Space` | serve. Refused while the puck is live. |

There is no AOI lever. A 2 m table with 34 entities has nothing to cull, and every one of them is relevant to
every peer — [rts-demo.md](rts-demo.md#aoi-on-both-lanes) explains distance culling and
[arena-demo.md](arena-demo.md) the other two interest axes.

## Bulk marshalling, which this demo is the case for

Capturing a tick is one `Object.get` per replicated property and restoring one is one `Object.set`, and **the
rollback loop pays both per replayed tick, per body**. This demo is where that multiplies: the puck is
predicted on *every* peer, so every peer replays it, and 32 mallets sit on the same lane with state and input.

```gdscript
handle.set_bulk_capture("_net_marshal_out")   # one call per lane per tick
handle.set_bulk_restore("_net_marshal_in")
handle.set_bulk_apply("_net_marshal_in")      # a RECEIVED row, and the quantized write-back
```

Three state props over a twelve-tick resim is 36 property reads and 36 writes in one frame for the puck alone;
the hook makes it 12 and 12.

The **apply** direction covers the two walks the other two do not: landing a received row, and the quantized
write-back after a record. Every state property on both bodies here carries `@half`, so the write-back is the
larger of the two on a peer that simulates. Sharing the restore method is safe only because neither body
declares a cosmetic entry — an apply hook reads the *capture* slots, which are the restore slots plus the
cosmetics.

**Nothing about a hook reaches the wire.** The row, the mask, the delta base and the mispredict compare all
read the backend's own layout, so `F7` can be flipped mid-session on one peer while another keeps walking its
properties and neither notices anything about the other.

**The declared order is load-bearing and is asserted.** `bulk_capture_order()` is derived from the
registration, so reordering two `add_state` calls silently reorders what the hook must write — and a hook
writing the right values into the wrong slots replays wrong rather than erroring.
`demos/hockey/tests/unit/bulk_marshal_test.gd` pins the correspondence, and the boot line
`HOCKEY-MARSHAL puck=bulk mallets_state=32/32 mallets_input=32/32` says whether the hooks actually resolved —
a name that does not resolve leaves the lane on the walk and reports nothing at the call site.

The mallet's input entry lives on its **child** input node while the hook resolves on the body's root, so that
half reaches through `input`. Where a value is stored is the game's business; the hook only has to supply it.

## A seat is kept for a player who comes back

A dropped peer's seat is **held** for `Net.reconnect_grace()` seconds: the roster stops naming the peer and
starts naming its **identity**, so the seat is taken but empty. The backend holds the mallet on the neutral
input row with its state still broadcasting, so it comes to rest where it was rather than freezing and then
jumping when its owner returns.

Players are therefore seated on `Net.peer_joined`, **not** on `multiplayer.peer_connected`. The transport
signal fires when the socket comes up, which is before the OrbitNet handshake — so no identity is known yet,
and identity is the only thing that can tell a returning player from a newcomer. On this table, seating on the
transport signal is what makes a rejoiner come back onto the other team.

**This demo takes the conservative rule, where the RTS demo takes the backend's default.** A claim on a session
identity has to quote the **resume token** the server minted for it, so an identity read off a roster broadcast
or a log line no longer buys that player's seat; an on-path observer, who reads the welcome the token traveled
in, can still quote it. The token does not settle the **incumbent**. Under the default policy a valid claim is
granted against a connection that is still up, which is what makes a relaunched client's reconnect immediate
rather than waiting out a keepalive. This demo will not accept a live takeover on any terms, so a reclaim is
honored only for an identity this layer already saw `Net.peer_dropped` report with `held = true`.
`Net.set_resume_policy(Net.ResumePolicy.ONLY_IF_DROPPED)` is the backend's version of the same refusal, in one call. The price is
the one the facade names: a player whose old socket the transport has not yet noticed is gone comes back as a
newcomer. On a 32-seat table that costs them their end of the rink, which is cheap; on the RTS's two-seat table
it would cost them the game.

`--session=N` pins the identity so a *restarted binary* can reclaim its seat; `Net` mints a random one per
process, which already covers a player who returns through the same process. The identity is not sufficient
on its own: the server mints a resume token per identity and a rejoiner has to quote it back with
`--resume-token=N`. The client prints the token it was issued on `HOCKEY-TOKEN=`, and a real game persists it
beside the identity rather than reading it off a log line.

## No token bucket on the command channel

A serve is legal **only while the puck is dead**, and serving makes it live — so the state precondition rate
limits the channel by itself and the validator's work is O(1) either way. `CommandThrottle` lives in the RTS
demo, where an order is legal whenever the player likes.

One channel rather than one per seat, for the mirror reason: an order names unit ids, so a request on somebody
else's channel is unambiguous forgery. A serve names nothing, and the sender id is the entire authorization.

## Nothing is a PhysicsBody

The puck, the mallets, the rails and the goal mouths are pure functions over a plane (`PuckPhysics`,
`MalletControl`, `TableGeometry`), and pointer picking is `Plane.intersects_ray`.

That is a requirement rather than a preference: **the rollback loop replays a tick, and Godot's physics server
cannot be rewound and re-stepped**. A body whose motion came out of the physics server would resimulate to a
different answer than the one it recorded, on every correction.

The puck substeps four times per tick because at its speed cap a single 60 Hz step moves it further than its
own diameter, and a one-shot overlap test would let it pass straight through a mallet — a *simulation* bug that
looks exactly like a netcode bug. `puck_physics_test.gd` asserts the derivation rather than the number.

## Deliberately omitted

- **No win condition.** The score climbs and the demo can be left running.
- **No version handshake.** Two incompatible peers connect and misbehave rather than being refused.
- **No plausibility test on a serve.** The validator checks who asked and whether the puck is dead; a refusal
  reaches the client that asked, carrying a `ServeValidator.Code` the HUD turns into a sentence.
- **No InputMap** — raw key constants and the raw pointer, so there is no project-settings dependency to break
  across Godot versions. A real game should use one.
- **No PR-gating probe.** The three scene-bound gates are `tools/rts-probe.sh`, `tools/server-shape-probe.sh`
  and `tools/arena-probe.sh`, and CONTRIBUTING.md admits a fourth only for a fundamental regression none of
  them reaches; this demo's coverage is fourteen unit suites over its pure functions.
- **No interest filtering of any kind.** One shared rink, 34 entities, every one of them relevant to every
  peer. There is nothing to cull by distance, no second world to be a member of, and no exception inside the
  one world worth naming — [arena-demo.md](arena-demo.md) is the demo those three axes are about.
- **One seat per connection.** Split-screen — several owned, predicted bodies behind one socket, each with its
  own interest anchor — is `NetRollbackHandle.set_seat()`, and it is shown in
  [arena-demo.md](arena-demo.md#split-screen-is-what-a-seat-is-for).

## What a 16-bit slot removed

A block names its entity by a **16-bit session slot** rather than the 64-bit id, which is where about a third
of a small block used to go. On a 34-entity rink that is invisible in the totals and it is still the reason
the puck's row is as small as it is; [protocol.md](protocol.md#entity-slots) has the format. The cost it moves
rather than removes — the manifest that binds every slot, sent whole to a peer holding no table and as a delta
against the generation a receiver holds thereafter — needs thousands of entities to matter, which is
[arena-demo.md](arena-demo.md#entity-slots).
