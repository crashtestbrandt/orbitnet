# The arena demo

`demos/arena/` — three laser-tag arenas hosted by one server, up to eight fighters in each. No menu, no match
end.

It makes an argument the other two demos cannot: **who receives what is decided by the game**. Three
independent interest axes, several seats behind one connection, and an identity that survives a drop.

```sh
just arena                    # single player, no networking at all
just arena-serve              # terminal 1: dedicated, headless
just arena-host               # ...or a listen server, with a local player
just arena-join               # terminal 2
just arena-join 127.0.0.1 2   # terminal 3: split-screen, two fighters on one connection
just arena-observe            # terminal 4: watching, driving nothing
```

`arena-join` takes `ADDR` or `ADDR:PORT` and a seat count, so a session hosted on a non-default port with
`just arena-host 47900` is reached with `just arena-join 127.0.0.1:47900`.

## The configuration, and why it is a third project

| `[orbitnet]` | arena | air hockey | RTS |
|---|---|---|---|
| `sync_to_physics` | `false` — **decoupled** | `true` | `false` |
| `tickrate` | `30` | `60` | `20` |
| `history_limit` | `128` | `128` | `64` |

**Decoupled is load-bearing rather than a preference.** The two lag-compensation features this demo shows are
about the interpolation delay a *receiver* applies, and a coupled demo has none — the terms would be constants
there. 30 Hz rather than 60 because a shooter that pays for a deep rewind ring should not also have to pay for
a fast tick: the whole argument of the rewind is that the server can reconstruct what a client saw without
being sent it more often.

## The lane split

| Lane | What is on it | Count |
|---|---|---|
| **Rollback** | every fighter — server-owned state, client-owned input on a child node | 24 |
| **State** | the props, one channel each, anchored and membership-bounded | 288 |
| **State** | one scorecard per arena, membership-bounded and **not** anchored | 3 |
| **Command** | `fire`, one channel for the whole session | 1 |

## What replicates is arena-local

Every fighter and every prop replicates a position in **its own arena's frame**, near that arena's origin.
The world-space spacing between arenas is applied when a node is *placed* for rendering and never reaches the
wire or the interest pass.

That is the arrangement membership exists for, and it is the demo's central fact:

> Two fighters standing on the same spot in different arenas are **zero metres apart**. No radius can separate
> them. A declared world can, and nothing else in the facade can.

```gdscript
# FighterBody.bind_net()
_handle = Net.register_rollback_body(self, input, STATE_PROPS, INPUT_PROPS, predict)
_handle.set_membership("arena_id")     # an int on this node, read live on the authority, never on the wire
_handle.process_settings()
```

`arena_id` is derived from the seat, so every peer knows it without being told — replicating it would be
replicating a division.

**Arena ids start at 1, not 0.** `0` is the facade's "every world": a channel declaring it is in every arena at
once. Numbering from 1 means a membership property that was never written filters nothing rather than silently
joining arena 0.

## The three axes, and what each one can say

| Axis | Declared by | Can it say "not this peer"? |
|---|---|---|
| **Distance** | automatic on the rollback lane, `NetStateHandle.set_anchor()` on the state lane | No — one position, read the same way by every peer |
| **Membership** | `set_membership()` on either lane | No — one world, read the same way by every peer |
| **Veto** | `Net.set_entity_hidden(peer, entity, true)`, server-side | **Yes.** The only per-(peer, entity) fact in the filter |

The scorecard is the third shape: it replicates two integers and no position, so there is no distance for a
radius to work with. `set_membership()` bounds it to the one arena it is about and leaves it uncullable inside
that arena, which is what a scoreboard should be.

## The cloak is a veto, and that is why it works

A cloaked fighter is withheld from every connection that is not on its team. Its own team keeps receiving it —
which is a fact about a *pair*, so no membership could express it.

```gdscript
Net.set_entity_hidden(peer, fighter.entity_id(), true)
```

Two consequences the demo relies on rather than works around:

- **The rows stop; the node stays.** A withheld fighter freezes on the watching peer at the last pose that
  arrived and does not despawn. A cloak therefore reads as an opponent who kept running the way they were last
  seen going — a better cloak than invisibility, and it costs no code.
- **The cloak FLAG rides in those rows.** `net_flags` carries the cloak bit, so a peer being sent the rows
  knows within a tick and a peer that is not never finds out. That is what the probe asserts on.

**The veto pass runs on the tick the cloak changes**, not on a fixed cadence. The tick a fighter cloaks is a
tick whose row is about to be encoded, so a veto placed three ticks later has already let the cloak reach the
peer it exists to hide it from. When nothing changed the pass falls back to a slow interval; a 24-byte mask
comparison is what makes that safe.

**A veto is per connection, not per seat.** A datagram is per connection and every seat behind it shares one, so
a split-screen player whose two fighters are on opposite teams may see both teams' cloaks. That is the
datagram's limit, not the policy's.

## Split-screen is what a seat is for

`--seats=2` gives one connection two locally-driven, locally-predicted fighters. Each is a **seat** in the
backend's sense: its own interest anchor, its own centre, its own world, its own hysteresis band. The
connection receives the **union** of its seats' sets, with the nearest seat's distance kept per entity.

```gdscript
handle.set_input_authority(peer)   # WHICH CONNECTION authors this body's input
handle.set_seat(index)             # WHICH OF THAT CONNECTION'S BODIES this one is, for interest
```

Both are needed and they are different axes. Two fighters on one connection that both sat at seat `0` would
share one interest centre, and the second player's surroundings would be culled around where the first was
standing.

**By default the two seats are in different arenas**, which is the case worth having: a connection with a body
in two worlds has no inferred world of its own. `--same-arena` puts them together for the ordinary
couch-co-op shape.

**The seat index is per connection.** It says which of *this* connection's bodies a fighter is, not which of
the session's — every connection has a seat `0`. It is derived on every peer by counting an owner's seats in
roster order, so nobody has to be told.

**A command carries its seat in the payload, and the server checks it.** `NetCommand` hands its validator the
sender's peer id — the only identity a client cannot author — and that names the *connection*. So a shot names
its seat, which makes it a claim, and `SeatRoster.owns_seat()` checks it against the seats the server assigned
to that sender. An unchecked seat field is a forged shot on somebody else's fighter, and it is the one new
mistake this shape makes available.

## An observer declares where it watches from

A peer with no seat has no body to infer an interest centre from, and a peer with no centre is filtered in
nowhere — the backend falls open and sends it everything. That is why a seatless spectator used to be refused
at the door.

```gdscript
Net.set_peer_anchor(peer, local_point, arena_id)        # a ground point, in that arena
Net.set_peer_anchor_entity(peer, entity_id, arena_id)   # ...or a fighter, wherever it goes
Net.clear_peer_anchor(peer)                             # back to inference, one centre per seat
```

**The arena is the half that cannot be inferred.** An observer that declared only a centre would be watching
one point in all three arenas at once. A declaration replaces inference on the centre *and* the world at once,
which is exactly what a peer with no body needs.

**A peer arriving at a full table is admitted as an observer**, not disconnected. There is now something to do
with it.

**The declaration is throttled.** An observer pans continuously, and one reliable message per frame restating a
centre that slid twenty centimetres is how a spectator costs more than a player. `ObserverDesk` decides: a
distance threshold, an interval so a lost message is still corrected, and an unconditional resend on a change
of mode or arena — because an observer that moved its centre to the same local point in the next arena moved
zero metres and changed everything it can see.

## The rewind is per shooter and per target

A shot is a command, not an input bit. A shot discovered inside `_rollback_tick` would be replayed on every
resim and fire again each time.

The server resolves it against the world as that shooter saw it:

```gdscript
var base: int = NetLagComp.rewind_ticks_for_peer_shot(is_authority, peer, rtt_ms, tick_hz)
var bands: PackedInt64Array = NetLagComp.rewind_band_ticks(present, is_authority, peer, rtt_ms, tick_hz)
lag.resolve_hit(space, origin, dir, range, exclude, SHOT_MASK, present - base, present,
    SHOT_DYNAMIC_MASK, bands, shooter_position)
```

- **The window is the whole round trip plus the interpolation term, not half of it.** The rewind is measured
  from the server's present back to the world as the shooter saw it: the state took the downstream leg, the
  client drew it some ticks behind whatever it held, and the command took the upstream leg back.
- **The interpolation term is per peer.** The byte budget is charged per peer and the send path rebuilds its
  candidate list per peer, so a peer watching a quiet arena gets its rows every tick while one in a firefight
  waits several. Pooling hands the first a window measured partly from the second.
- **...and per band.** The same is true across distance: a target across the arena is staler than one in your
  face by a factor the send path already measures. The two margins multiply.
- **The authority rewinds nothing.** A listen host renders the bodies it is simulating, live: no round trip to
  itself and no interpolation delay to what it is drawing.
- **The rewind is analytic.** Recorded capsules are ray-tested where they were; the live physics world is never
  moved. That is what lets the static half of the mask — cover, which is the same cover at every tick — be
  cast live at the present tick in the same call, with the nearer of the two winning.

The HUD prints all three depths beside the flat base window. **With no per-band measurement published they are
equal**, and that is correct rather than a failure: with no evidence the scale is 1.0 and the per-target rewind
is exactly the flat per-shooter window. It must not invent a spread.

`ArenaConfig.BAND_SCALE_M` is sized to an *arena*, not to the session. A scale large enough to span three would
put every body in one band and all three measurements would read the same.

## Discrete events are written inside the tick

The rule the whole repository keeps running into, met here from a third direction.

**The rollback lane restores recorded history onto its properties every tick.** A fighter's health, its cloak
bit and its shot sequence all live there — they have to, because they are what a rewound shot is resolved
against. But a hit is decided in a `NetCommand` handler, which runs *outside* the tick. Written directly, it is
overwritten by the next restore, silently, on the server, and the fighter simply never dies.

So the write moves instead: damage, cloaks and shots are **queued** and drained inside `_rollback_tick`, on a
fresh tick only. The result is recorded at that tick, and every replay restores it.

| Demo | Same rule, different side |
|---|---|
| RTS | Units are on the **state** lane so an order written from a handler survives at all |
| Air hockey | The scoreboard is on the **state** lane so a goal found inside the tick is not erased |
| Arena | The values must stay on the **rollback** lane, so the **write** is what moves |

Whether a hit was fatal is therefore not known when the shot resolves. The director reads the answer off the
fighters afterwards and credits the scorecard — which is on the state lane, so writing it outside the tick is
safe.

## Entity slots

A block names its entity by a **16-bit session slot** rather than a 64-bit id. A three-arena session with the
default prop count names 315 entities; `just arena-slots 8000` seeds enough to put real pressure on the table.

The slot table is distributed by the entity manifest as a **whole table each time it changes**, so a session
with tens of thousands of entities churning steadily spends real reliable bandwidth restating bindings that did
not move. Past 65,536 the server refuses to replicate the entity and says so, rather than wrapping an index
onto a live one.

## Bulk marshalling

The fighter is a fat rollback body — five state properties and three input ones — and the rollback loop pays a
capture and a restore walk **per replayed tick, per body**. A twelve-tick resim over eight properties is 96
property reads and 96 writes in one frame, for one fighter.

```gdscript
handle.set_bulk_capture("_net_marshal_out")
handle.set_bulk_restore("_net_marshal_in")
```

**F2 takes them away**, live, and `restore_ms` / `record_ms` in the HUD move. Nothing about a hook reaches the
wire, so one peer may marshal in bulk while another walks its properties and neither notices.

The HUD reports which lanes are *actually* marshalling in bulk, asked of the backend rather than echoed from
the lever: a hook is resolved by name, and a name that does not resolve leaves the lane on the walk and reports
nothing at the call site.

## What is on screen

| Line | What it says |
|---|---|
| `MARSHAL` | which lanes are marshalling in bulk, and the property counts behind the arithmetic |
| `WIRE` | bytes and datagrams per second, peers, entities in interest, and the interest pass's own cost |
| `INTEREST` | what this peer is **being sent**, against what exists. A server says so and reports what it holds |
| `ARENAS` | fighters received per arena — the membership axis, made visible |
| `CLOAK` | how many entity-peer pairs are withheld right now |
| `CENTRE` | whether this peer's interest centre is inferred or declared |
| `REWIND` | the base window and all three per-band depths, and whether they differ |
| `INTERP` | each peer's own measured send cadence, and the pooled figure it falls back to |

## The levers

| Key | What it does |
|---|---|
| **F1** | AOI radius on/off. At `0` the distance filter is off and **membership still runs** — which is the point of the pair |
| **F2** | bulk marshalling on/off |
| **F3** | the cloak veto on/off |
| **F4** | observe / play |
| **F5** | cycle what an observer watches: each arena's centre, then a fighter in it |

F1, F3 and the seating are **server-side**; on a client they change nothing, which is the security property
rather than a limitation — a peer cannot decide what it is allowed to receive.

## The gate

`tools/arena-probe.sh` is the second PR-gating probe. Three passes:

| Pass | Shape | What it establishes |
|---|---|---|
| **A** | dedicated server, a two-seat client, an observer | dedicated boot, world-signature agreement, membership, the veto, the declared anchor, provable acks, the rewind path |
| **B** | listen server, a two-seat client | the same channel against the other server shape |
| **C** | dedicated server, a client killed and relaunched | a session identity reclaims its seats under a new peer id |

**The readings are rises, not values.** `NetStateHandle.last_known_state()` fails open — on a backend that
cannot answer it returns the present tick — so a threshold test would be satisfied by the fallback and prove
nothing.

**On the rollback lane it is not a veto signal at all.** A withheld body's reading keeps advancing on the
client, so the probe prints it and asserts on it nowhere; the veto assertion is the cloak *flag*, which a
withheld peer demonstrably never learns.

## Known gaps

Recorded rather than hidden.

- **The retained interest grid is not exercised.** What the interest pass sees is arena-local, so the session's
  occupancy is about ±20 m however far apart the arenas are drawn — far inside the crossover where the grid
  overtakes the linear scan. The grid is not what ships either way.
- **A shot is not validated for plausibility**, only for ownership, liveness and rate. There is no
  plausible-aim test that is not also a test of what the player could see.
- **No client-side hit feedback beyond the replicated result.** `NetCommand` has no `rejected` signal, so a
  refused shot is invisible to the client.
- **An observer is on no team, so every cloak is withheld from it.** That is this demo's rule rather than the
  facade's: a spectator watching cloaked fighters would have better information than either player.
- **A build/protocol version handshake.** Two incompatible peers connect and misbehave rather than being
  refused with a reason.
