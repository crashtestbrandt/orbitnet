# API reference

`Net` is an autoload; everything else is a `class_name` you can construct.

Everything here is safe to call **OFFLINE**: the facade no-ops, factories return inert handles, metrics return
zeros. That is what lets a game wire its netcode unconditionally.

## `Net` — the facade

### Mode

```gdscript
enum Net.Mode { OFFLINE, CLIENT, SERVER, HOST }
```

`HOST` is a listen server: authoritative *and* a local player. `SERVER` is dedicated — authoritative with no
local player.

| | |
|---|---|
| `current_mode() -> Mode` | The current role. `OFFLINE` at boot. |
| `set_mode(mode: Mode) -> void` | **Starts or stops the tick loop.** A peer must already be assigned to `multiplayer.multiplayer_peer` before switching *into* a networked mode. Switching to `OFFLINE` stops the loop. |
| `is_offline() -> bool` | |
| `is_server() -> bool` | True for `SERVER` and `HOST` — "does this peer run the authoritative simulation". |
| `is_client() -> bool` | True for `CLIENT` and `HOST` — "does this peer render a local player". Note both are true on a host. |
| `mode_name(mode: Mode) -> String` | `"offline"` / `"client"` / `"server"` / `"host"`. Stable; logs and probes grep for these. |

### The tick clock

| | |
|---|---|
| `current_tick() -> int` | The authoritative tick. Inside a tick or rollback handler this is *the tick being run*, matching the handler's own argument. 0 OFFLINE. |
| `current_time() -> float` | Network time in seconds, shared and continuously synced. Monotonic, but can be re-stepped on a large drift — tolerate the occasional small jump. |
| `rollback_tick() -> int` | The tick a resim is currently *replaying*, which during a resim is older than the frontier. Key per-tick memos off this. |
| `tickrate() -> int` / `set_tickrate(hz: int) -> void` | The configured rate, clamped to 1..240. Under `sync_to_physics` the *effective* rate is the physics rate regardless. |
| `debug_timing() -> String` | The live effective rate and `sync_to_physics`, for a status line. |

### Coupled vs decoupled

| | |
|---|---|
| `set_net_tick_decoupled(hz: int) -> void` | Pace the net loop off the wall clock at `hz` while physics stays at its own rate. **Call before `set_mode()`** — the loop starts there. |
| `set_net_tick_coupled() -> void` | Restore net tick == physics tick. **Call on teardown**: this is a process-wide setting and leaking it into the next session is a real bug. |
| `is_decoupled() -> bool` | False OFFLINE. |
| `net_tick_factor() -> float` | The 0..1 fraction between the previous and next net tick, for render interpolation. 1.0 when coupled and OFFLINE. |
| `net_tick_dt() -> float` | Net tick duration in seconds. 0 OFFLINE. |

### Signals

| | |
|---|---|
| `pre_tick(tick: int)` | Once per net tick, **before** the backend records that tick's input. Populate your replicated input frame here so it is captured and sent. Never fires OFFLINE. |
| `post_tick()` | Once per tick loop **after** the rollback/resim finished — every body's state is now the authoritative present-tick value. Capture render poses here under the decouple. |

### The three lanes

```gdscript
func register_rollback_body(
    root: Node, input_node: Node,
    state_properties: Array[String], input_properties: Array[String],
    predict: bool, cosmetic_properties: Array[String] = []) -> NetRollbackHandle

func make_state(root: Node) -> NetStateHandle
func make_interpolator(root: Node) -> NetInterpolatorHandle
func make_rollback(root: Node) -> NetRollbackHandle     # bare handle; you register properties yourself
```

`register_rollback_body` is the one to use. It sets up the server-authoritative split in one call:

- `root`'s authority is the **server** — it owns every body's state.
- `input_node`'s authority is the **owning client** — each client authors only its own input, and the backend
  validates the sender against that authority. That is the anti-forgery check.
- Splitting them **requires input on its own node**, because authority is a per-node property.
- `predict` is true on the owning client *and* on the server, false on a client watching someone else's body.

`root` then implements `_rollback_tick(delta: float, tick: int, is_fresh: bool)`, called on every peer that
predicts it. See [protocol.md](protocol.md#is_fresh) for what `is_fresh` guarantees.

### Resim-depth and diagnostics knobs

All per-client and live; peers need not agree.

| | |
|---|---|
| `input_delay()` / `set_input_delay(ticks)` | Ticks of intentional input delay, 0..32. Shrinks the unconfirmed window directly by stamping input into the future — less prediction to be wrong about, at the cost of local responsiveness. |
| `display_offset()` / `set_display_offset(ticks)` | Present a slightly older, more-confirmed tick so late corrections land before they are rendered, 0..32. Purely presentation-side; resim depth is unchanged. |
| `remote_resim()` / `set_remote_resim(on)` | Whether a client's rollback loop carries **remote** bodies. Default false: remotes are display-only and apply the latest authoritative state. True predicts them forward with held input. Meaningless on a server. |
| `resim_force()` / `set_resim_force(ticks)` | Test hook: force the loop at least this deep every tick, 0..64. A measurement lever, not a gameplay one. |
| `aoi_radius()` / `set_aoi_radius(m)` | Interest radius in metres, 0 = off. **Rollback lane only** — see the warning below. Server-side; ignored on clients. |

### Metrics

Live in **every** build, release included.

```gdscript
Net.clock_metrics()  -> {"stretch", "offset_ms", "rtt_ms", "jitter_ms", "lead_ticks"}
Net.perf_metrics()   -> {"resim_ticks", "rollback_ms", "net_ms", "rb_nodes"}
Net.perf_summary()   -> String
```

- **`stretch`** — the sim-clock speed multiplier. Pinned to exactly 1.0 in coupled mode (clock error is
  absorbed by rare whole-tick slews instead, because a stretch ≠ 1.0 slides tick boundaries across physics
  frames and renders as judder). Under the decouple it rides within `max_time_stretch`.
- **`resim_ticks`** — how deep the last rollback loop replayed. This is the resim *cost*; it legitimately
  deepens under latency and loss, and is bounded by `history_limit`.
- **`rb_nodes`** — how many nodes the loop called `_rollback_tick` on. A quick check that your rollback
  entity count is what you think it is.

### Interest: three axes, distance, membership and the veto

A peer replicates an entity when **every** filter admits it. They are independent and are declared separately.

| axis | rollback lane | state lane |
|---|---|---|
| **Distance** — within `aoi_radius` of the peer's centre | always on; the anchor is the body's first `Vector3` **State** property, so register position first | opt in with `NetStateHandle.set_anchor(entry)`, naming a `Vector3` explicitly |
| **Membership** — in the same world as the peer | opt in with `NetRollbackHandle.set_membership(entry)`, naming an `int` | opt in with `NetStateHandle.set_membership(entry)`, naming an `int` |
| **Veto** — not withheld from this peer | `Net.set_entity_hidden(peer, entity_id, true)`, server-side | same call, same ids |

The first two are properties of the **entity** — one position, one world, read the same way by every peer. The
veto is the only per-(peer, entity) fact in the filter, and the only one that can name an exception.

**The seat's own centre and world both come from one body**: the lowest-id rollback entity whose *input*
authority is that peer, which declares that seat, and which resolved an anchor — unless the peer declares them,
below. A **seat** is one owned, predicted body behind a connection; every body is on seat `0` until
`NetRollbackHandle.set_seat()` says otherwise, which is one seat per connection.

Three consequences people find the hard way:

1. **A state channel is culled only if it declares how.** A channel with no `set_anchor()` has no distance to
   be culled by, and one with no `set_membership()` is in every world. Declaring neither means it replicates
   to every peer, which is the default and is the fail-open direction.
2. **A peer with no rollback body and no declaration has no anchor**, so the backend falls back to "everything
   is in interest" — every world, at every distance. Either give every peer a rollback entity, which is why the
   RTS demo puts the command cursor on that lane, or declare the pair with `Net.set_peer_anchor()`.
3. **Membership is what a positionless channel has instead of a radius.** Health, inventory, a door's state:
   none of them replicate a position, so no radius reaches them. `set_membership()` bounds them to one world
   while leaving them uncullable inside it.

Membership matters when one session hosts **several independent worlds**, each rebased near its own coordinate
origin. Two entities at the same coordinates in different worlds are zero metres apart, so no radius can
separate them. `0` is the default id on both sides and matches every world.

### Seats: several owned bodies on one connection

Local split-screen over a network session is two or more locally-owned, locally-predicted bodies behind a
single transport peer. Each is a **seat**, and the second player's surroundings are not the first player's.

| | |
|---|---|
| Declaring a seat | `NetRollbackHandle.set_seat(index)`, **on the server**. Every body starts at `0`. |
| What a seat gets | Its own interest anchor, its own centre, its own world, its own hysteresis band and its own nearest-N cap. |
| What the connection gets | The **union** of its seats' sets, with the **nearest** seat's distance kept per entity — which is the band the send rota scores it in. |
| What stays per connection | The delta base, the ack window, `want_full`, the byte budget and the **veto**. Those are properties of a datagram, and a datagram is per connection. |

- **A seat with no body yet culls nothing of its own.** Culling is decided per seat, so a seat whose body has
  not spawned does not inherit another seat's centre and have its surroundings culled around a position it is
  nowhere near.
- **A leave is a leave from the union.** An entity one seat lets go of keeps its delta chain while another seat
  still holds it.
- **`Net.set_peer_anchor()` collapses the connection to one viewpoint.** A declaration states where a
  *connection* observes from; a game that wants a centre per seat declares nothing and lets each seat's body
  anchor it.
- **The input frame is bounded to one datagram.** Each owned body carries four ticks of input per frame, so
  several seats can overrun the payload; what does not fit is offered first on the next tick. A body skipped
  for up to three frames loses nothing, because the next frame it rides in re-sends the ticks it missed.
- **Commands are per connection, not per seat.** `NetCommand` hands its validator the sender's peer id — the
  only identity a client cannot author. A game with several seats on one connection puts the seat in the
  payload and validates it against the seats the server assigned to that sender.
- **Nothing on the wire carries a seat.** Interest runs where state authority is, so a seat is a server-side
  declaration; the anti-forgery check on received input is per entity and is unchanged.
- **`Net.set_entity_hidden()` withholds from the whole connection.** A veto refuses a row in a datagram every
  seat shares, so it is applied to each of them — including a seat that joins later — rather than to the union.

```gdscript
# A player body: its world is also the OWNING PEER's world.
var body := Net.register_rollback_body(unit, input, state_props, input_props, predict)
body.set_membership("world_id")      # an int property on `unit`
body.process_settings()              # register_rollback_body already processed settings once

# A positionless channel bounded to the same world.
var hp := Net.make_state(unit)
hp.add_state(unit, "health")
hp.set_membership("world_id")
hp.process_settings()
```

#### Declaring where a peer observes from

Server-side only, and no-ops OFFLINE. What a peer **observes** is not what its input **controls**: a spectator
drives nothing, a commander watches ground its body is not standing on, and a peer with a body in each of two
worlds observes exactly one of them.

| | |
|---|---|
| `set_peer_anchor(peer, position, membership = 0)` | Observe from a fixed world position, in this world. |
| `set_peer_anchor_entity(peer, entity_id, membership = 0)` | Observe from an entity, wherever it is, in this world. `entity_id` comes from `entity_id()` on a rollback or state handle; `0` retracts. |
| `clear_peer_anchor(peer)` | Retract the centre **and** the world, back to the inferred body — one per seat. |
| `peer_membership(peer)` | The **declared** world, 0 when nothing was declared. Not what an undeclared peer is filtered in — that is `NetRollbackHandle.membership()`. |

- **A declaration replaces inference on both axes at once.** The driven body is consulted for neither until
  `clear_peer_anchor()`, and a connection with several seats is collapsed to the one declared viewpoint.
- **It makes a peer's world a fact rather than a pick.** The inferred path reads it off whichever of the peer's
  bodies sorts lowest by hash, so a peer driving two bodies in different worlds has no defined world without
  this call.
- **A tracked entity that despawns leaves the peer where it last was, in the world it was declared into.** A
  membership is a declaration and did not fail; a centre is a measurement and did.
- **A declaration may precede the peer's handshake**, and a tracked entity with no state row yet starts
  resolving on the tick it gets one.

```gdscript
# A spectator with no body of its own, watching world 2 from above its centre.
Net.set_peer_anchor(peer_id, Vector3(0.0, 120.0, 0.0), 2)

# ...or following one unit around, wherever it goes.
Net.set_peer_anchor_entity(peer_id, body.entity_id(), 2)
```

#### Withholding one entity from one peer

Server-side only, and no-ops OFFLINE. A membership scopes a whole class of entities by a declared key; the veto
covers one peer and one entity, which is how an exception inside a world gets said at all.

| | |
|---|---|
| `set_entity_hidden(peer, entity_id, hidden)` | Withhold `entity_id` from `peer`, or stop withholding it. `entity_id` comes from `entity_id()` on a handle; `0` is ignored. |
| `is_entity_hidden(peer, entity_id)` | Whether it is currently withheld. |

- **The veto beats every other answer the filter would give**, an always-relevant channel with no anchor
  included, and it refuses at the candidate rather than at the cap — a withheld entity occupies no slot in
  `set_aoi_max_entities()`.
- **Starting one drops the entity from that peer's interest in the same call** and clears its delta
  bookkeeping, so a later retraction sends a full block rather than a delta against a base the peer dropped.
  Retracting re-admits the entity as a newcomer, through `aoi_radius` like any other.
- **A veto stops the rows and nothing else.** No despawn is sent, the client's node is not removed, and the
  entity id stays session-global. The client sees `get_last_known_state()` stop advancing — the same thing a
  distance cull looks like — and what an entity that stopped updating means is your game's decision.
- **It survives that entity's despawn**, because it is keyed on the id and ids are node-path-derived: a body
  respawning under its old name reclaims its old id, and dropping the veto with the body would hand the peer
  that entity on the tick it came back.
- **It may precede the peer's handshake**, and it is dropped when that peer disconnects.

```gdscript
# One unit inside a shared world that this one peer must never receive.
Net.set_entity_hidden(peer_id, spy.entity_id(), true)

# ...and back, once it is theirs to see. The next block it gets is a full one.
Net.set_entity_hidden(peer_id, spy.entity_id(), false)
```

---

## `NetRollbackHandle`

| | |
|---|---|
| `is_active() -> bool` | False when inert (OFFLINE). |
| `add_state(node, property)` / `add_input(node, property)` | For handles built with `make_rollback()`. |
| `set_membership(entry) -> void` | Declare which **world** this body is in, as a `"NodePath:property"` naming an `int`. Also sets the owning seat's own world, unless that peer declared its own with `Net.set_peer_anchor()`. Call before `process_settings()`. |
| `set_seat(index) -> void` | Declare which **seat** on the owning connection drives this body. Server-side; `0` unless set, which is one seat per connection. |
| `seat() -> int` | The declared seat, `0` when inert. |
| `entity_id() -> int` | This body's stable replication id, for `Net.set_peer_anchor_entity()`. An opaque token — routinely negative, never compared or ordered. 0 when inert or unresolved. |
| `process_settings() -> void` | Re-resolve after the property set changes. |
| `process_authority() -> void` | Re-evaluate prediction after an authority change. Call on **every** peer when ownership moves. |
| `is_predicting() -> bool` | Whether the owner is currently mispredicting — the reconciliation gate. |
| `get_last_known_state() -> int` | Tick of the latest authoritative state received. −1 when inert. |
| `memo_set(tick, key, value)` / `memo_get(tick, key, fallback)` | The per-tick memo ring. Record on the `is_fresh` pass; every replayed pass reads the same value back, so a resim resolves against what the fresh pass saw. Trimmed with rollback history. |

## `NetStateHandle`

| | |
|---|---|
| `is_active() -> bool` | |
| `add_state(node, property)` | Register a replicated property. **Supports wire quantization** — see below. |
| `set_anchor(entry) -> void` | Declare the `Vector3` this channel is culled by distance from. Without it the channel has no distance and is never radius-culled. |
| `set_membership(entry) -> void` | Declare which **world** this channel is in, as a `"NodePath:property"` naming an `int`. Composes with `set_anchor()`; called alone it bounds the channel to one world without a distance test. |
| `membership() -> int` | The world the filter reads this tick, `0` meaning every world. A property that did not resolve reports `0`. |
| `entity_id() -> int` | This channel's stable replication id, for `Net.set_peer_anchor_entity()`. 0 when inert or unresolved. |
| `set_priority(weight)` | Send-rota priority, 1..16. |
| `last_known_state() -> int` | Tick of the newest authoritative row received. |
| `process_settings()` | |

Server-authoritative, no prediction, and — critically — **no rollback restore**, so a value set outside the
tick (from a `NetCommand` handler, a timer, a signal) is not clobbered.

## `NetInterpolatorHandle`

Local only; nothing here touches the wire.

| | |
|---|---|
| `is_active() -> bool` | |
| `add_property(node, property)` / `process_settings()` | Feed it the same properties the state lane replicates. |
| `teleport()` | Snap both endpoints to the live values. Call after any intended discontinuity — spawn, respawn, teleport — or the entity visibly flies there over one tick. |
| `is_enabled()` / `set_enabled(on)` | Turn smoothing off to see the raw net tick. |

Interpolatable types are `float`, `Vector2/3`, `Quaternion`, `Transform3D`. Anything else is applied as a
step at the tick boundary, which is correct for a discrete value and is not an error.

Do **not** use this on rollback bodies under the coupled path — the body writes its pose every physics tick
and Godot's own physics interpolation renders it; an interpolator would fight it.

## `NetCommand`

```gdscript
func register(verb: StringName, handler: Callable) -> void   # Callable(sender_id: int, payload: Dictionary) -> bool
func request(verb: StringName, payload: Dictionary) -> void
signal applied(verb: StringName, payload: Dictionary)
```

The handler **validates and applies in one place**, so an unvalidated request can never reach your state. It
runs only on the applying peer: the server, or the local peer offline (with sender id `0`).

Rules that are not optional:

- **Stable node name.** The RPC routes by node path; every peer must build it identically.
- **Resolve *who* from `sender_id`**, never from the payload — it comes from the transport and cannot be
  authored by the sender.
- **Keep the payload parameter untyped `Dictionary`.** Godot decodes it as a plain Dictionary at the RPC
  boundary, so a `Dictionary[String, Variant]` annotation would *reject* the wire value. Read fields into
  typed locals inside the handler.
- **Rate-limit per sender.** `request()` is reliable and the client controls how often it calls one.
- **A handler runs OUTSIDE the tick**, so anything it writes belongs on the **state** lane.

## `NetTransport`

The one place that names a concrete transport. Selection is by **export-preset feature tag**, not runtime
config, so a build's transport is a build-time fact.

| | |
|---|---|
| `preferred_kind() -> Kind` | `STEAM` if `OS.has_feature("steam")`, else `ENET`. Never `OFFLINE` — it describes the *build*, not the session. |
| `kind_name(kind)` / `preferred_kind_name()` | `"offline"` / `"enet"` / `"steam"`. |
| `create_server(port, max_clients, friends_only=false) -> MultiplayerPeer` | Null on failure. |
| `create_client(address, port) -> MultiplayerPeer` | Null on failure. |
| `set_local_display_name(name)` / `local_display_name()` | A local override, so the name pipeline is exercisable with no platform present. |

Adding a transport is a new `Kind` branch here plus a matching `custom_features` tag. Never scatter
platform-SDK calls elsewhere — `tools/net-check.sh` polices the boundary.

---

## Wire quantization, and the scalar reality

A property name may carry an `@` suffix asking the backend to narrow it **on the wire**. The property is
unchanged in GDScript.

| Suffix | Valid for | Cost |
|---|---|---|
| `@half` | `Vector3`, `Vector2`, `f32` | Vector3: 12 B → **6 B** (three IEEE-754 binary16 components) |
| `@ss3` | `Quaternion`, `Basis` | 16 B → **8 B** (smallest-three) |
| *(none)* | anything | lossless |

**An invalid (quantizer, type) pairing does not error — it silently falls back to lossless.** So a suffix
that looks like it is saving bytes may be saving none, and nothing tells you.

That matters most for scalars, because of this:

> **A GDScript `float` is an f64 and a GDScript `int` is an i64.** That is what the language stores, and the
> backend records them at full width deliberately — narrowing a float would round every replayed value and
> quietly break a bit-exact resimulation.

So **there is no way to narrow a bare scalar from GDScript.** `"hp@half"` is eight bytes on the wire, exactly
as if the suffix were absent.

**The idiom is to pack scalars into a `Vector3` and quantize that.** Three normalized scalars in one
`Vector3 @half` cost 6 bytes instead of 24. The RTS demo packs `(sin θ, cos θ, hp01)` that way; see
[rts-demo.md](rts-demo.md).

And a related trick worth its own line: **send a direction as a pair, not as an angle.** A yaw scalar cannot
be quantized (see above) *and* interpolates catastrophically across the ±π wrap — a unit facing roughly south
spins a full turn whenever it wobbles. `(sin, cos)` costs 4 bytes as halves and interpolates correctly,
because it is a point on a circle.

### The budget this is all in service of

One UDP frame per peer per tick, with a **~1200-byte payload budget**. Entities are served **stalest-first**,
so exceeding the budget does not drop anyone — it ages everyone. A 20-byte entity means ~46 refreshed per
peer per tick; 96 of them at 20 Hz is a full refresh every ~2 ticks. Every byte saved per entity is another
entity refreshed per tick.

## Project settings

```ini
[orbitnet]

sync_to_physics=true    ; net tick AT the physics rate
tickrate=60             ; the rate used when sync_to_physics is false
history_limit=128       ; rollback history depth, PER ROLLBACK ENTITY
max_time_stretch=1.05   ; decoupled mode only
```

Read once, when the facade constructs the backend. `history_limit` is per rollback entity, so it is a real
memory cost per predicted body — the RTS demo drops it to 64 because it has one per player.
