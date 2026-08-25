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
| `peer_joined(peer, session_id, resumed_from)` | **Server-side.** A peer finished the handshake. `resumed_from` is the peer id it held before it dropped, or 0 for a newcomer. **Seat players here**, not on the transport's `peer_connected`. Fires once per peer, however many times its handshake is retried. |
| `peer_dropped(peer, session_id, held)` | **Server-side.** A peer's connection is gone. `held` is whether its session is being kept open for the grace window — false for no identity, for a grace of 0, and for a ghost whose identity a returning player already took back (such a drop reports `session_id` 0). |
| `peer_session_expired(session_id, peer)` | **Server-side.** A held session's window closed unclaimed. The release point — the addon does not act on it by default; `Net.set_seat_release_policy()` is how a game says otherwise. |
| `seat_opened(peer, seat)` | **Both sides.** A seat arrived on a connection — the server as it seats the body, a client one entity manifest later. **Bind presentation here**: a split-screen viewport, a camera, a HUD panel. A joining connection's first seat is announced here too. |
| `entity_entered_interest(peer, entity_id)` | **Both sides.** An entity started being sent to `peer` — on a client, `peer` is its own id. Call `NetInterpolatorHandle.teleport()` before drawing it: a body that moved while it was away would otherwise fly to its new pose over one tick. |
| `entity_left_interest(peer, entity_id)` | **Both sides.** An entity stopped being sent — a distance cull, a membership refusal, a cap eviction, a per-peer veto or an unregister, and the five are indistinguishable by design. **The addon frees nothing.** Hide rather than free: a nearest-N eviction can oscillate at the boundary. |
| `seat_closed(peer, seat)` | **Both sides.** Nothing drives `(peer, seat)` any more. A dropped connection does **not** close its seats by itself *by default* — its bodies keep the authority they were given until the game changes them, or until `Net.set_seat_release_policy()` says otherwise. |

### Session identity and reconnection

A multiplayer peer id names a **connection** and is reassigned on every reconnect, so a roster keyed on one
hands a returning player whichever place happens to be free — somebody else's army, somebody else's body. A
**session identity** names the player and survives the drop. The client mints one, the handshake carries it,
and the server matches a rejoiner against the sessions it is holding open.

*(This is a different axis from [seats](#seats-several-owned-bodies-on-one-connection). A seat says which of a
connection's owned bodies one body is; a session identity says which player a connection belongs to.)*

**The identity is asserted by the client and verified by nobody.** It is adequate for giving a player their
own entity back and inadequate for anything that must not be forged — account identity, entitlement, ban
evasion. Those need an authenticated layer above it, and `set_session_id()` is where its verified id goes.

| | |
|---|---|
| `session_id() -> int` | This peer's identity. Minted randomly at boot, so a game that does nothing is already resumable within a process. |
| `set_session_id(id: int) -> void` | Override it. **Before the join handshake.** Pass a stored value to survive a process restart; 0 claims no identity and is always seated as a newcomer. |
| `resume_token() -> int` | The token **the server issued this identity**, learned from the welcome. **Persist it beside the session id** — an identity alone reclaims nothing. |
| `set_resume_token(token: int) -> void` | Quote a stored token. **Before the join handshake**, like the identity. |
| `peer_resume_token(peer: int) -> int` | Server-side, diagnostics. |
| `resume_policy()` / `set_resume_policy(p)` | `Net.ResumePolicy.ALWAYS` (the default), `ONLY_IF_DROPPED`, or `NEVER`. |
| `set_session_secret(secret: PackedByteArray)` / `has_session_secret() -> bool` | Derive the per-datagram key from a secret both ends already share. **Before `set_mode()`**, on both ends. An empty array clears it. There is no getter for the bytes. |
| `peer_session_id(peer: int) -> int` | Server-side: the identity `peer` presented. **Key your roster on this.** 0 for an unknown peer and one that claimed none. |
| `is_session_held(session_id: int) -> bool` | Server-side: whether a dropped session is still reclaimable. |
| `reconnect_grace() -> float` / `set_reconnect_grace(s: float) -> void` | Seconds a dropped peer's session is held open. Wall-clock, server-side, 30 s by default. 0 disables resume — a drop is forgotten in the same frame and `peer_dropped` reports `held = false`. |

**The token is what makes an identity worth asserting.** The server mints one per identity and sends it in the
welcome; a rejoiner must quote it back. Without it, anyone who *saw* a session id — off a roster broadcast, a
kill feed, a log line, a screenshot — could present it and take that player's body. **It does not stop an
on-path observer**, who reads the welcome the token traveled in; that boundary is the same one the session key
has, and `set_session_secret()` is what moves it.

#### Releasing a dropped connection's seats

```gdscript
enum Net.SeatRelease { HOLD, RELEASE_ON_EXPIRY, RELEASE_ON_DROP }

func seat_release_policy() -> SeatRelease
func set_seat_release_policy(policy: SeatRelease) -> void
func release_peer_seats(peer: int) -> int      # every seat that connection drives; returns entities changed
func release_seat(peer: int, seat: int) -> int # one seat
```

**`HOLD` is the default and does not move.** It is what the reconnect grace window is for, and it is what a
pinned released binary already does — flipping it would make an existing game's `seat_closed` handler fire on
every transient drop, and a game that frees the body there would despawn players on a wifi hiccup.

`RELEASE_ON_*` hands the input back to the server and closes the seat. **Freeing the node stays your call** —
that is the decision `peer_session_expired` declines to make, and this does not make it either.

The two helpers work under every policy, including `HOLD`, so a game that wants to keep deciding for itself
still gets the one-call verb instead of hand-rolling a peer-to-bodies table the backend already keeps.

**A peer id that is live again is never released.** Ids are reused, and the expiry path names one that dropped
up to a whole grace window ago — by then a newcomer may hold it.

**The gap policy, stated.** From the tick its owner leaves, an entity's input is written as the **neutral
(all-zero) row** on the server and its tick is marked authoritative. Two consequences, and each replaces
something the default got wrong:

- It acts on **no** intent rather than repeating the departed player's last one. Without this the last input
  row is re-applied on every tick the history ring can still reach — a body walking into a wall keeps walking
  into it — and past the ring the input node simply keeps whatever values were last written to it.
- Its state **keeps broadcasting**. Without this the entity's frontier freezes at the last tick a received
  input backed, while the server goes on simulating it — so every other peer holds it still and then watches
  it jump the moment its owner returns.

Sizing the window costs something in both directions: nobody else can be given the entity, and it keeps
replicating, for the whole of it. Too short and a player who dropped on a loading screen returns to a stranger
in their body; too long and a full session refuses newcomers while it waits for players who left for good.

**A returning player does not wait for the transport to notice the old connection.** A killed client is not
declared dead until its keepalive times out — measured here at anywhere from 45 s to never — so the handshake
takes the identity back from whatever connection still claims it and reports that peer as `resumed_from`. The
superseded connection is not closed, only stripped: its later `peer_dropped` reports `session_id` 0 and holds
nothing.

**So `resumed_from` names a connection that may still be up, and acting on it hands the new claimant that
peer's body.** For a relaunched client that is the whole point, and it is now gated on the resume token: a
claimant that cannot quote the token the server issued that identity is seated as an anonymous newcomer, and
the incumbent keeps its identity, its token and its window. An **on-path** observer that read the welcome can
still quote it.

The alternative — resuming only a drop the server observed — refuses every genuinely fast reconnect for as
long as the transport takes to notice, measured here at 45 s to never, which is why it is not the default.
`Net.set_resume_policy(Net.ResumePolicy.ONLY_IF_DROPPED)` is that rule in one call, and it no longer has to be
hand-rolled against `peer_dropped`.

```gdscript
func _ready() -> void:
    Net.set_reconnect_grace(30.0)
    Net.peer_joined.connect(_on_peer_joined)
    Net.peer_session_expired.connect(_on_session_expired)

func _on_peer_joined(peer: int, session_id: int, resumed_from: int) -> void:
    # assign() reclaims the slot this session already owns, whatever peer id it arrives under.
    var slot: int = roster.assign(peer, session_id)
    if slot < 0:
        multiplayer.multiplayer_peer.disconnect_peer(peer)   # full
        return
    _broadcast_roster()   # re-points the body's input authority on every peer

func _on_session_expired(session_id: int, _peer: int) -> void:
    roster.release_session(session_id)
    _broadcast_roster()   # that slot's body goes back to the server: set_input_authority(1)
```

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
  validates the sender against that authority. That is the anti-forgery check on **who** wrote a row.
  **Nothing checks what is in it** — see [protocol.md](protocol.md#what-the-receive-path-refuses-and-what-it-does-not).
- Splitting them **requires input on its own node**, because authority is a per-node property.
- `predict` is true on the owning client *and* on the server, false on a client watching someone else's body.

`root` then implements `_rollback_tick(delta: float, tick: int, is_fresh: bool)`, called on every peer that
predicts it. See [protocol.md](protocol.md#is_fresh) for what `is_fresh` guarantees.

**Validate your input values there.** The backend authenticates every datagram, refuses a row written for an
entity the sender does not own, and refuses one carrying a **non-finite** float — that last one is not policy,
it is a poison value that would be restored onto your input node before every replayed tick and then recorded
into state. Everything else a row can say, it says: clamp axes, bound rates, and reject impossible states
inside `_rollback_tick`, on the server. The full split is
[in protocol.md](protocol.md#what-the-receive-path-refuses-and-what-it-does-not).

### Resim-depth and diagnostics knobs

All per-client and live; peers need not agree.

| | |
|---|---|
| `input_delay()` / `set_input_delay(ticks)` | Ticks of intentional input delay, 0..32. Shrinks the unconfirmed window directly by stamping input into the future — less prediction to be wrong about, at the cost of local responsiveness. |
| `display_offset()` / `set_display_offset(ticks)` | Present a slightly older, more-confirmed tick so late corrections land before they are rendered, 0..32. Purely presentation-side; resim depth is unchanged. |
| `remote_resim()` / `set_remote_resim(on)` | Whether a client's rollback loop carries **remote** bodies. Default false: remotes are display-only and apply the latest authoritative state. True predicts them forward with held input. Meaningless on a server. |
| `resim_force()` / `set_resim_force(ticks)` | Test hook: force the loop at least this deep every tick, 0..64. A measurement lever, not a gameplay one. |
| `aoi_radius()` / `set_aoi_radius(m)` | Interest radius in meters, 0 = off. **Rollback lane only** — see the warning below. Server-side; ignored on clients. |

### Metrics

Live in **every** build, release included.

```gdscript
Net.clock_metrics()  -> {"stretch", "offset_ms", "rtt_ms", "jitter_ms", "lead_ticks"}
Net.perf_metrics()   -> {"resim_ticks", "rollback_ms", "net_ms", "rb_nodes",
                         "restore_ms", "sim_ms", "record_ms"}
Net.perf_summary()   -> String
```

- **`stretch`** — the sim-clock speed multiplier. Pinned to exactly 1.0 in coupled mode (clock error is
  absorbed by rare whole-tick slews instead, because a stretch ≠ 1.0 slides tick boundaries across physics
  frames and renders as judder). Under the decouple it rides within `max_time_stretch`.
- **`resim_ticks`** — how deep the last rollback loop replayed. This is the resim *cost*; it legitimately
  deepens under latency and loss, and is bounded by `history_limit`.
- **`rb_nodes`** — how many nodes the loop called `_rollback_tick` on. A quick check that your rollback
  entity count is what you think it is.
- **`restore_ms` / `sim_ms` / `record_ms`** — the three phases `rollback_ms` wraps: writing a tick's recorded
  state and input back, running the game's `_rollback_tick`, and capturing the result. They sum to slightly
  less than `rollback_ms`; the remainder is range setup and the display-offset restore, left visible rather
  than attributed. `restore_ms` and `record_ms` are the two figures a
  [bulk hook](#bulk-marshalling-one-crossing-per-lane-per-tick) moves.

### What the server believes about a peer's link

```gdscript
Net.peer_rtt_ms(peer) -> float        # the BELIEVED figure, capped
Net.peer_rtt_raw_ms(peer) -> float    # the unclamped window minimum
Net.rtt_believed_max_ms() -> float    # the cap; 250 ms by default
Net.set_rtt_believed_max_ms(ms: float) -> void
Net.bandwidth_metrics()["rtt_at_ceiling_peers"]   # how many peers are above it
```

The estimate comes from acknowledgments the client chooses when to send. Every one of them is **proven** — the
server mints a token per snapshot frame from a secret it never transmits and discards any ack that does not
quote it back, so a peer cannot acknowledge a frame that never reached it. What a token does not settle is
whether the peer received anything *newer*: a client advancing its ack at full rate behind a constant lag
quotes a real token every time and is measured at that lag, indistinguishable from a peer behind a traffic
shaper. **No wire field closes that** — `current - ack` is the whole round trip whatever tick lead the client
runs at.

So the containment is a ceiling on what the server will **believe**, not a refusal of any acknowledgment.
Refusing would break burst-loss recovery for the honest lossy peer; clamping the sample at the read breaks
nothing, and an honest peer above the ceiling is under-rewound by exactly the amount `NetLagComp.max_delay_ms`
already under-rewinds it by.

`peer_rtt_raw_ms()` keeps a scoreboard ping or an admin tool honest. `rtt_at_ceiling_peers` is the one
server-side signal an operator gets: you cannot tell a shaper from a satellite, but you can see who is asking
for the deepest window in the session.

### Interest: three axes, distance, membership and the veto

A peer replicates an entity when **every** filter admits it. They are independent and are declared separately.

| axis | rollback lane | state lane |
|---|---|---|
| **Distance** — within `aoi_radius` of the peer's center | always on; the anchor is the body's first `Vector3` **State** property, so register position first | opt in with `NetStateHandle.set_anchor(entry)`, naming a `Vector3` explicitly |
| **Membership** — in the same world as the peer | opt in with `NetRollbackHandle.set_membership(entry)`, naming an `int` | opt in with `NetStateHandle.set_membership(entry)`, naming an `int` |
| **Veto** — not withheld from this peer | `Net.set_entity_hidden(peer, entity_id, true)`, server-side | same call, same ids |

The first two are properties of the **entity** — one position, one world, read the same way by every peer. The
veto is the only per-(peer, entity) fact in the filter, and the only one that can name an exception.

**The seat's own center and world both come from one body**: the lowest-id rollback entity whose *input*
authority is that peer, which declares that seat, and which resolved an anchor — unless the peer declares them,
below. A **seat** is one owned, predicted body behind a connection; every body is on seat `0` until
`NetRollbackHandle.assign_seat()` or `set_seat()` says otherwise, which is one seat per connection.

Three consequences people find the hard way:

1. **A state channel is culled only if it declares how.** A channel with no `set_anchor()` has no distance to
   be culled by, and one with no `set_membership()` is in every world. Declaring neither means it replicates
   to every peer, which is the default and is the fail-open direction.
2. **A peer with no rollback body and no declaration has no anchor**, so the backend falls back to "everything
   is in interest" — every world, at every distance, and **not bounded by `set_aoi_max_entities()` either**,
   because an entity with no distance is kept as uncullable and an uncullable entity does not occupy a slot in
   the nearest-N cap. Either give every peer a rollback entity, which is why the RTS demo puts the command
   cursor on that lane, or declare the pair with `Net.set_peer_anchor()`, or say
   `Net.set_unanchored_policy(Net.UnanchoredPolicy.CLOSED)` and let a peer that declares nothing receive
   nothing. The fallback is per **connection**: a peer whose *one* seat of several has not resolved is not
   opened up by it — and `CLOSED` keeps that carve-out, because closing it would deny a joining player its own
   avatar for as long as the body takes to spawn.
   `Net.peer_anchor(peer)` reports which of these a connection is actually in.
3. **Membership is what a positionless channel has instead of a radius.** Health, inventory, a door's state:
   none of them replicate a position, so no radius reaches them. `set_membership()` bounds them to one world
   while leaving them uncullable inside it.

Membership matters when one session hosts **several independent worlds**, each rebased near its own coordinate
origin. Two entities at the same coordinates in different worlds are zero meters apart, so no radius can
separate them. `0` is the default id on both sides and matches every world.

### Seats: several owned bodies on one connection

Local split-screen over a network session is two or more locally-owned, locally-predicted bodies behind a
single transport peer. Each is a **seat**, and the second player's surroundings are not the first player's.

| | |
|---|---|
| Seating a body | `NetRollbackHandle.assign_seat(peer, index)`, **on the server**. Writes the owning connection and the label in one call. |
| Emptying a seat | `NetRollbackHandle.release_seat()`. Input goes back to the server, the label back to `0`; the body stays registered and stays replicated. |
| Declaring the label alone | `NetRollbackHandle.set_seat(index)`. Only when the owning connection is not changing. Every body starts at `0`. |
| What a seat gets | Its own interest anchor, its own center, its own world, its own hysteresis band and its own nearest-N cap. |
| What the connection gets | The **union** of its seats' sets, with the **nearest** seat's distance kept per entity — which is the band the send rota scores it in. |
| What stays per connection | The delta base, the ack window, `want_full`, the byte budget and the **veto**. Those are properties of a datagram, and a datagram is per connection. |

- **A seat is derived from `(input owner, seat label)`, never declared on its own.** That is why
  `assign_seat()` writes both: two separate writes leave a tick in which the body reads as
  `(new peer, old label)`, which is announced as a seat opening and closing again.
- **A seat change is announced on both sides**, as `Net.seat_opened` / `Net.seat_closed`. `Net.seats_of(peer)`
  answers which seats a connection holds and `Net.seat_entities(peer, seat)` which bodies one seat drives.
  A client learns both from the entity manifest, which is reliable and republished on every seat change.
- **A seat that has not spawned yet contributes no viewpoint.** Culling is decided per seat, so a seat is never
  centered on a position it is nowhere near — and a seat whose body has no state row yet is **skipped** rather
  than treated as seeing everything, because the connection's set is a union and one unresolved seat would
  otherwise open the whole connection to every world. Fail-open is per **connection**: a peer with no resolved
  seat at all still sees everything, which is what stops a joining player arriving in an empty world.
- **A leave is a leave from the union.** An entity one seat lets go of keeps its delta chain while another seat
  still holds it.
- **`Net.set_peer_anchor()` collapses the connection to one viewpoint.** A declaration states where a
  *connection* observes from; a game that wants a center per seat declares nothing and lets each seat's body
  anchor it.
- **The input frame is bounded to one datagram.** Each owned body carries four ticks of input per frame, so
  several seats can overrun the payload; what does not fit is offered first on the next tick. A body skipped
  for up to three frames loses nothing, because the next frame it rides in re-sends the ticks it missed.
- **Commands are per connection, not per seat.** `NetCommand` hands its validator the sender's peer id — the
  only identity a client cannot author. A game with several seats on one connection puts the seat in the
  payload and validates it against the seats the server assigned to that sender.
- **The seat label is a server-side declaration; the roster is published.** Interest runs where state authority
  is, so a client never authors a seat — the entity manifest tells it which connection and label drive each
  entity. No hot-path frame carries a seat, and the anti-forgery check on received input is per entity and is
  unchanged.
- **A dropped connection keeps its seats until the game releases them.** The bodies keep the authority they
  were given, exactly as `peer_session_expired` describes. Call `release_seat()` and `seat_closed` fires.
- **A dedicated server holds no seat of its own; a listen server does.** Handing input back to peer 1 is how a
  game says a body is unclaimed, so a server with no local player announces nothing for it. On a listen server
  peer 1 *is* the host player, which also means a body the host holds unclaimed reads the same as one the host
  player drives — seat the host player on a non-zero label if you have to tell them apart.
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
| `clear_peer_anchor(peer)` | Retract the center **and** the world, back to the inferred body — one per seat. |
| `peer_membership(peer)` | The **declared** world, 0 when nothing was declared. Not what an undeclared peer is filtered in — that is `peer_anchor()`. |
| `peer_anchor(peer) -> Dictionary` | **What is actually in effect**, which nothing else reported: `source` (`Net.AnchorSource`), `viewpoints`, `membership` (the world in effect, not the declaration), `located`, `center`, `open`, `ambiguous`, `stale`. Every key present and zeroed OFFLINE. |
| `seat_anchor(peer, seat) -> Dictionary` | The same for one seat: `center`, `located`, `membership`. |
| `unanchored_policy()` / `set_unanchored_policy(p)` / `set_peer_unanchored_policy(peer, p)` | `Net.UnanchoredPolicy.OPEN` (the default, and today's behavior) or `CLOSED`. |
| `seats_of(peer)` | Which seats a connection currently holds, ascending. **Both sides**; empty OFFLINE. Answered from the announced roster, so it agrees with `seat_opened` / `seat_closed`. |
| `seat_entities(peer, seat)` | Which bodies one seat drives, as entity ids — opaque tokens, never compared or ordered. **Both sides**; empty OFFLINE. What a camera or a split-screen viewport needs when `seat_opened` fires. |

- **A declaration replaces inference on both axes at once.** The driven body is consulted for neither until
  `clear_peer_anchor()`, and a connection with several seats is collapsed to the one declared viewpoint.
- **It makes a peer's world a fact rather than a pick.** The inferred path reads it off whichever of the peer's
  bodies sorts lowest by hash, so a peer driving two bodies in different worlds has no defined world without
  this call.
- **A tracked entity that despawns leaves the peer where it last was, in the world it was declared into.** A
  membership is a declaration and did not fail; a center is a measurement and did.
- **A declaration may precede the peer's handshake**, and a tracked entity with no state row yet starts
  resolving on the tick it gets one.

```gdscript
# A spectator with no body of its own, watching world 2 from above its center.
Net.set_peer_anchor(peer_id, Vector3(0.0, 120.0, 0.0), 2)

# ...or following one unit around, wherever it goes.
Net.set_peer_anchor_entity(peer_id, body.entity_id(), 2)
```

#### Withholding one entity from one peer

Server-side only, and no-ops OFFLINE. A membership scopes a whole class of entities by a declared key; the veto
covers one peer and one entity, which is how an exception inside a world gets said at all.

**A veto needs no radius and no membership declared.** It turns the interest pass on by itself, so it works in
the configuration it exists for. Setting one is what makes `is_entity_in_interest()` answer per peer at all in
a session that filters nothing else.

| | |
|---|---|
| `set_entity_hidden(peer, entity_id, hidden)` | Withhold `entity_id` from `peer`, or stop withholding it. `entity_id` comes from `entity_id()` on a handle; `0` is ignored. |
| `is_entity_hidden(peer, entity_id)` | Whether it is currently withheld. |
| `is_entity_in_interest(peer, entity_id)` | Whether `peer` is currently being sent it. |
| `entities_in_interest(peer)` | Everything `peer` currently holds. **An edge needs a starting point** — a handler bound mid-session, or a node built after the fact, resyncs from this instead of waiting for churn. |

The two signals carry an **entity id, not a handle**, and that is deliberate: a leave routinely names an entity
this peer has no node for — one whose slot was bound before its scene object existed locally, or one already
freed — which is exactly the case a per-handle signal could not reach. Keep your own id-to-node map.

- **The veto beats every other answer the filter would give**, an always-relevant channel with no anchor
  included, and it refuses at the candidate rather than at the cap — a withheld entity occupies no slot in
  `set_aoi_max_entities()`.
- **Starting one drops the entity from that peer's interest in the same call** and clears its delta
  bookkeeping, so a later retraction sends a full block rather than a delta against a base the peer dropped.
  Retracting re-admits the entity as a newcomer, through `aoi_radius` like any other.
- **A veto stops the rows and nothing else.** No despawn is sent, the client's node is not removed, and the
  entity id stays session-global. The client sees `is_receiving()` go false — the same thing a distance cull
  looks like — and what an entity that stopped updating means is your game's decision. Ask `is_receiving()`,
  not `get_last_known_state()`: that one is a *frontier* and rises every tick on a peer that authors state.
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
| `assign_seat(peer, index) -> void` | Seat this body: point its input at `peer` **and** put it on that connection's seat `index`, in one call. Use whenever both change — two separate writes leave a tick in which the body reads as `(new peer, old label)`. Local, like every authority write: call it on every peer. |
| `release_seat() -> void` | Empty the seat: input back to the server, label back to `0`. The body stays registered and replicated — what leaves is the viewpoint. |
| `set_seat(index) -> void` | Declare the **label** alone, when the owning connection is not changing. Server-side; `0` unless set, which is one seat per connection. |
| `seat() -> int` | The declared seat, `0` when inert. |
| `entity_id() -> int` | This body's stable replication id, for `Net.set_peer_anchor_entity()`. An opaque token — routinely negative, never compared or ordered. 0 when inert or unresolved. |
| `process_settings() -> void` | Re-resolve after the property set changes. |
| `process_authority() -> void` | Re-evaluate prediction after an authority change. Call on **every** peer when ownership moves. |
| `set_input_authority(peer: int) -> void` | Point this body's **input** at `peer` and re-resolve everything that reads the answer, in one call. `1` hands it back to the server, which is what an unclaimed body means. The **connection** axis, not the seat one — `set_seat()` is unaffected. **Local — call it on every peer**; nothing here replicates. Writing the node's authority without the re-resolve leaves this peer predicting the wrong body and the send path anchoring the wrong peer's radius, and nothing errors when it happens. |
| `set_predicted(on: bool) -> void` | Turn this peer's **prediction** of the body on or off after registration. **The one thing `set_input_authority()` does not re-resolve** — see below. Local; call it on every peer. |
| `is_predicted() -> bool` | Whether this peer is set to predict the body. The switch, not the reconciliation gate. |
| `is_predicting() -> bool` | Whether the owner is currently mispredicting — the reconciliation gate. |
| `get_last_known_state() -> int` | The **frontier**: the newer of "an authoritative row arrived" and "this peer authored a tick". −1 when inert. On a peer that authors state it rises every tick whatever the wire did — use `is_receiving()` to ask whether rows are still arriving. |
| `last_received_state() -> int` | The **receipt**: the tick of the newest row this peer decoded for the body. −1 inert, −1 before the first row, and −1 forever on the authority, which receives nothing. |
| `authors_state() -> bool` | Whether this peer writes the rows rather than receiving them. |
| `is_receiving(within_ticks := 24) -> bool` | Whether rows are still arriving. **The call to make**, and it fails open: true on the authority, true on a backend that cannot report a receipt, true when a row landed within the window. |
| `reports_last_received_state() -> bool` | Whether `last_received_state()` is a measured tick rather than an unanswerable one. |
| `quantizer_fallbacks() -> PackedStringArray` | Entries whose `@` annotation was dropped because it does not apply to the resolved type. Empty when inert or unanswerable. |
| `memo_set(tick, key, value)` / `memo_get(tick, key, fallback)` | The per-tick memo ring. Record on the `is_fresh` pass; every replayed pass reads the same value back, so a resim resolves against what the fresh pass saw. Trimmed with rollback history. |
| `set_bulk_capture(method)` / `set_bulk_restore(method)` / `set_bulk_apply(method)` | Declare the game methods that marshal a whole lane in one call instead of one per property — see [bulk marshalling](#bulk-marshalling-one-crossing-per-lane-per-tick). Call before `process_settings()`. |
| `bulk_capture_order(lane)` / `bulk_restore_order(lane)` / `bulk_apply_order(lane)` | The declared entries a hook marshals for that lane, in array order. Empty when the lane has no hook. **The apply order is the capture order, not the restore order** — they differ by the lane's cosmetics. |
| `uses_bulk_capture(lane)` / `uses_bulk_restore(lane)` / `uses_bulk_apply(lane)` | Whether that lane marshals in bulk. Check after `process_settings()`: a method name that did not resolve leaves the lane on the per-property walk. |
| `LANE_STATE` / `LANE_INPUT` | The lane ordinals a hook receives as its first argument. |

### Prediction is a switch, and it does not move on its own

`predict` is an argument to `register_rollback_body()` and it is read nowhere else. Re-pointing a body's input
re-resolves *who owns which lane*; it leaves that switch where registration set it.

**That matters for every game whose world is built before its roster arrives**, which is the ordinary shape: a
client builds its scene, joins, and is told which body is its own a moment later. Every body registered in that
window has `predict = false`, and `predict = false` does not merely defer prediction — it **exempts the body
from the rollback loop**. An exempt body still applies the authoritative rows it receives, so it moves, and
every readout looks ordinary while the player's own input is a full round trip late. Nothing errors.

```gdscript
func set_owner_peer(peer: int, seat: int) -> void:
    handle.assign_seat(peer, seat)                                   # the connection and the label
    handle.set_predicted(Net.is_server() or peer == multiplayer.get_unique_id())
```

`set_predicted(false)` returns the body to display-only and re-establishes the exemption, unless
`Net.remote_resim()` asked this client to carry remote bodies.

**It is not derived from the authorities automatically**, and that is deliberate: "this peer owns a lane of it"
is the usual answer but not the only correct one. A body every peer predicts with nobody owning its input — a
puck, a ball, a shared physics prop — is registered `predict = true` on peers that own neither lane, and
deriving the switch would turn that off the first time anything touched its authority.

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
| `last_known_state() -> int` | Tick of the newest authoritative row received. **Fails open**: on a backend with no `get_last_known_state` it answers `Net.current_tick()`, so a staleness rule degrades rather than blanking the world. |
| `reports_last_known_state() -> bool` | Whether the line above is a measured tick rather than that fallback. `false` on an inert handle and on a backend that cannot answer. Check it wherever the reading is used as evidence that rows arrived — the fallback is invisible in the number. |
| `last_received_state() -> int` | The **receipt**, the same reading `NetRollbackHandle` publishes: the tick of the newest row this peer decoded. −1 inert, −1 before the first row, −1 on the authority. |
| `authors_state() -> bool` | Whether this peer writes the rows rather than receiving them. |
| `is_receiving(within_ticks := 24) -> bool` | Whether rows are still arriving. Fails open, exactly as on the rollback lane, so one helper spans both. |
| `reports_last_received_state() -> bool` | Whether the receipt is a measured tick. |
| `quantizer_fallbacks() -> PackedStringArray` | Entries whose `@` annotation was dropped. |
| `set_bulk_capture(method)` | Declare the game method that captures this channel's whole row in one call — see [bulk marshalling](#bulk-marshalling-one-crossing-per-lane-per-tick). Call before `process_settings()`. |
| `set_bulk_apply(method)` | Declare the method that lands a **received** row in one call. This lane's only other walk, and the one a peer that simulates nothing pays for every block it is sent. |
| `bulk_apply_order()` / `uses_bulk_apply()` | The entries the apply hook marshals, and whether it resolved. |
| `bulk_capture_order()` / `uses_bulk_capture()` | The entries the hook marshals, in array order, and whether it resolved. |
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
func register(verb: StringName, handler: Callable) -> void   # Callable(sender_id: int, payload: Dictionary) -> bool | int
func request(verb: StringName, payload: Dictionary) -> int                       # returns the tag
func request_batch(verb: StringName, payloads: Array) -> PackedInt32Array        # one tag per payload

signal applied(verb: StringName, payload: Dictionary)
signal rejected(verb: StringName, code: int, tag: int)

const CODE_OK := 0
const CODE_BATCH_TOO_LARGE := -1
const CODE_BATCH_MALFORMED := -2
const MAX_BATCH := 16
```

The handler **validates and applies in one place**, so an unvalidated request can never reach your state. It
runs only on the applying peer: the server, or the local peer offline (with sender id `0`).

### The verdict decides both the outcome and whether the requester hears about it

| the handler returns | outcome | reply to the requester |
|---|---|---|
| `true` | applied | none |
| `false` | refused | **none — silent, exactly as it always was** |
| `int` `CODE_OK` (0) | applied | none |
| `int`, non-zero | refused, carrying that code | `rejected`, on both peers |
| anything else | refused | none |

A validator declared `-> bool` behaves identically to before `rejected` existed, byte for byte and packet for
packet. A game opts into feedback by declaring it `-> int` and returning its own reason enum, whose `OK` member
must be `0` — which is the shape an enum with `OK = 0` already has.

**Keep the silent refusal for a rate limit.** A reply is one reliable packet per refused request and the client
chooses how often it asks; returning `false` from the throttle branch and an int everywhere else is what stops
a spamming client buying server upstream. The lane's own refusals are **negative**, so they cannot collide with
a game's codes.

**Send the code, not the sentence.** A reason string is presentation, and a server-side one routinely names ids
and seats the asker may not own — which is exactly the ownership answer a validator refused to give. The
demos put a `describe(code) -> String` beside the enum and call it on the client.

`tag` is the value `request()` returned, so a UI cancels the request that actually failed instead of guessing
by verb. It is minted per peer, never `0`, and is `0` in a `rejected` that no `request()` on this peer produced.

### Batching

`request_batch(verb, payloads)` puts several payloads for **one verb** in a single reliable packet.

- **A batch is a coalescing optimization, not a transaction.** Each payload is validated and applied
  independently; `applied` fires per accepted payload and the refusals come back coalesced into one reply.
- **One verb per batch**, because the verb is what a channel registers against and every real batch is
  homogeneous. A mixed flush is one call per verb — packets in the number of verbs, not of requests.
- Over `MAX_BATCH` the batch is refused **whole**. Trimming to a legal prefix would apply a request the caller
  never separated out.
- **It is not a rate-limit bypass**: the lane charges nothing itself, so a per-sender throttle still runs once
  per entry.

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

## Bulk marshalling: one crossing per lane per tick

Capturing a tick is one `Object.get` per replicated property, and restoring one is one `Object.set` per
restored property. **The rollback loop pays both per replayed tick, per body**, so a body with `S` state props
replaying `D` ticks costs `S × D` reads and up to `S × D` writes in a single frame.

A **bulk hook** replaces each of those walks with one call. OrbitNet hands the game a preallocated `Array` and
the game fills it (capture) or reads it (restore), so the crossing count per lane per tick is `1` instead of
`S`.

```gdscript
# On the body's root, the node the property entries resolve against.
func _net_marshal_out(lane: int, values: Array) -> void:
    if lane == NetRollbackHandle.LANE_INPUT:
        values[0] = nin_move
        return
    values[0] = net_pos
    values[1] = net_orient
    values[2] = net_rcs_lin      # cosmetic: captured, never restored

func _net_marshal_in(lane: int, values: Array) -> void:
    if lane == NetRollbackHandle.LANE_INPUT:
        nin_move = values[0]
        return
    net_pos = values[0]
    net_orient = values[1]       # the cosmetic entry is ABSENT from the restore order

# Declared before process_settings().
handle.set_bulk_capture("_net_marshal_out")
handle.set_bulk_restore("_net_marshal_in")
```

Rules, all of them enforced or reported:

| | |
|---|---|
| **Opt-in per synchronizer** | Declare nothing and every lane keeps the per-property walk, byte for byte. |
| **The row is unchanged** | The hook supplies the values and nothing else. Encoding, byte offsets and wire quantization stay in the backend, because masks, delta bases and the mispredict compare all read that layout. |
| **Fill every slot** | The array is preallocated and reused, so a slot the hook leaves alone keeps last tick's value. There is no "unset" sentinel. |
| **Do not resize it** | A wrong-length array drops that lane back to the walk and reports it once. |
| **The restore order is shorter** | Cosmetic entries are captured and replicated but never restored. Read `bulk_restore_order()`, not the capture order. |
| **Assert the order** | `bulk_capture_order(lane)` publishes the declared entries in array order. Reordering a property registration silently reorders it. |
| **Do not call `Net` from a hook** | It is a marshalling method, not a decision point. |
| **Peers need not agree** | Nothing about a hook reaches the wire, and the schema hash is unchanged, so one peer may declare hooks while another walks the properties. |

### The third direction: `apply`

`set_bulk_apply(method)` covers the two walks the other two hooks do not: **applying a received row**, and the
**quantized write-back** after a record.

| walk | per entity per tick | multiplier |
|---|---|---|
| receive apply | `S + C` crossings → **1** | delivered blocks. **No replay multiplier** — below roughly twenty blocks a tick it is noise. It exists because a peer that simulates nothing plans no entities, so the rollback loop returns on an empty plan and this walk is that peer's entire per-tick crossing count. No other hook reaches it. |
| quantized write-back | `Q` crossings → **1** | replayed ticks × planned entities. A body with eight quantized properties at a forced replay depth of 12 is 96 setter crossings a frame, or 12. |

**THE APPLY ORDER IS THE CAPTURE ORDER, NOT THE RESTORE ORDER**, and this is the one thing here that fails
silently. They differ by exactly the lane's **cosmetic** entries, which are captured and replicated but never
restored. Passing your existing restore method to `set_bulk_apply()` on a body that declares cosmetics reads
shifted slots and applies wrong values with nothing erroring. Read `bulk_apply_order(lane)`.

The write-back only routes through the hook when the lane carries **at least two** quantized properties; below
that the targeted walk is cheaper and is what runs. `uses_bulk_apply()` reports the *declaration*, not which
path ran.

Measure it with `Net.perf_metrics()`: `record_ms` is the capture half and `restore_ms` the restore half, and
`Net.set_resim_force(ticks)` fixes the replay depth so the comparison holds still.

## Wire quantization, and the scalar reality

A property name may carry an `@` suffix asking the backend to narrow it **on the wire**. The property is
unchanged in GDScript.

| Suffix | Valid for | Native | Wire |
|---|---|---|---|
| `@half` | `Vector3` | 12 B | **6 B** — three IEEE-754 binary16 components |
| `@half` | `Vector2` | 8 B | **4 B** |
| `@ss3` | `Quaternion` | 16 B | **6 B** — smallest-three |
| `@ss3` | `Basis` | 36 B | **6 B** — smallest-three |
| *(none)* | anything | | lossless |

**An invalid (quantizer, type) pairing falls back to lossless, and says so.** It warns at registration naming
the entry, the resolved type and the byte cost of the fallback; in a **checked build** — the one Godot loads
for every editor and source run — it is an error rather than a warning, so a mistyped suffix is red the first
time the project loads. An exported build compiles that arm away.

`NetRollbackHandle.quantizer_fallbacks()` and `NetStateHandle.quantizer_fallbacks()` list the entries whose
annotation was dropped, so a boot check can fail on one instead of a log line scrolling past.

That matters most for scalars, because of this:

> **A GDScript `float` is an f64 and a GDScript `int` is an i64.** That is what the language stores, and the
> backend records them at full width deliberately — narrowing a float would round every replayed value and
> quietly break a bit-exact resimulation.

So **there is no way to narrow a bare scalar from GDScript.** `"hp@half"` is an f64 on the wire, eight bytes,
and the suffix is reported and dropped like any other invalid pairing — an error in a checked build, and
listed by `quantizer_fallbacks()` — so an entry list carrying one needs the suffix removed.

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
