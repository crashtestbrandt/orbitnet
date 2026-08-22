# Protocol and the tick model

What is on the wire, how the clock works, and what `is_fresh` guarantees. Read this before changing
`native/crates/orbitnet-core/`, or when a replication bug does not make sense.

## Wire format

Little-endian.

**Hot frame** — unreliable, one per peer per tick:

```
frame kind | tick | ack tick (zigzag delta) | 32-bit ack bitfield | 32-bit ack token | input-arrival margin byte
then per entity:  { slot (u16) | frame-tick delta | body length | flags | changed-property bitmask | packed payload }
then the trailer: { sequence (u32) | MAC tag (u64) }
```

No property names, no type tags — the schema is positional and agreed in advance. Client input carries the
last N ticks for redundancy, so a single lost packet costs nothing.

**A block names its entity by a 16-bit session slot, not by the 64-bit entity id.** See
[entity slots](#entity-slots) for what that costs and what it saves.

**Control frames** — reliable: handshake, welcome, entity manifest. All but the handshake carry the same
12-byte trailer.

**Handshake** — magic, protocol version, tick rate, a **session id**, then the **session key**:

```
magic | protocol version (u32) | tickrate (u16) | session id (u64) | session key (16 bytes)
```

The session id is what makes a reconnecting player recognisable: a peer id names the connection and is
reassigned every time, this names the player and is resent verbatim on every join. It is **asserted by the
client and verified by nobody** — adequate for giving a player their own entity back, inadequate for anything
that must not be forged. `0` means "no identity"; see [api.md](api.md#session-identity-and-reconnection).

Everything after the protocol version decodes **best-effort**, to a zero tick rate, no session id and an
all-zero key. `handle_hello` answers a decode error by returning, so a peer whose handshake is short would
otherwise be dropped in silence; decoding it far enough to reach the compatibility check is what produces the
operator-readable version mismatch. An all-zero key is then refused with a message of its own.

**Versioning.** `PROTOCOL_VERSION` packs `(major << 16) | (minor << 8) | patch`; **major must match exactly**.

| Major | What changed |
| --- | --- |
| 2 | Quantized wire encodings. |
| 3 | Per-datagram authentication, and the handshake's session key. |
| 4 | The hot-frame header carries an **ack token**. |
| 5 | Blocks name entities by a **16-bit session slot**; the entity manifest distributes the slot table for both lanes. |

**Minor is not checked and records a change no peer can misread** — the only kind that qualifies is an
optional *trailing* field on a control frame, where an older peer stops decoding before it and gets the
documented absent value. Anything that shifts an existing field's offset is a major bump, because there the
older peer decodes garbage.

Schema agreement is **per entity, not per session**: the server states each replicated entity's state and
input hash in its `EntityManifest` frame — `0` for the input hash of a state-lane entity, which has no input
schema — and a client whose locally built schema hashes differently is told
so by name. The hash is FNV-1a over `(name, kind, role)` in **declaration order** — deliberately
order-sensitive, because two peers registering the same properties in different orders would otherwise
silently misapply state, which is miserable to diagnose. The handshake carried a session-wide schema hash
until major 3; there is no such thing to hash, both call sites passed `0`, and the field was removed.

**Robustness.** The decoder is the one component parsing bytes chosen by a remote peer, so every read is
bounds-checked and returns an error rather than panicking — a decoder that panics on a malformed packet is a
remote denial of service. `#![forbid(unsafe_code)]` means a bounds bug cannot become memory unsafety either.
Tests sweep truncated and pseudo-random buffers.

## Entity slots

A block used to open with the entity id: 64 bits of FNV-1a over the synchronizer root's node path, written
as a varint. Hash output is spread across the whole 64-bit range, so that varint cost **9.5 bytes on
average** — measured over 6000 plausible node paths under both lane salts, which is also the theoretical
mean for a value uniform over 2<sup>64</sup>.

Against the RTS demo's own state entity — 20 B of properties — that was **29% of a full block and 46% of a
delta**:

| | Full block | Delta, one changed 6 B property |
|---|---|---|
| id, as a varint | 9.5 B | 9.5 B |
| **slot, fixed** | **2 B** | **2 B** |
| other framing | 3 B | 5 B |
| payload | 20 B | 6 B |
| block, before | 32.5 B | 20.5 B |
| **block, now** | **25 B** | **13 B** |

At the default 1200 B budget that is 37 full blocks per frame before and **48 after** — 30% more entities
refreshed at the same budget. Across 100 peers at 30 Hz it saves 764 kB/s (6.1 Mbit/s) of server egress.

**Fixed width, not a varint.** A `u16` varint costs 1 byte below 128 and 3 above it, so past 128 entities
most blocks would pay more than the flat 2.

### What a slot costs

The id needed no distribution: every peer derived the same value from the same node path, which is why a
reconnecting client re-derives its ids with no handshake. A slot is **assigned by the server**, so it has to
be distributed and held.

- **The entity manifest carries it**, reliably, as `(slot, id, state hash, input hash)` per entity. That
  frame covered the rollback lane only while it was purely a schema check; it now names **both lanes**,
  because state-lane blocks carry slots too.
- **It is a complete table, sent whole**, including when it is empty. A receiver rebuilds its copy from each
  frame rather than merging, which is what retires the binding of an entity that has unregistered — there is
  no removal record to lose.
- **A block whose slot has no binding is skipped**, exactly as a block for an unknown entity id was. Blocks
  stay length-prefixed for that reason. An unreliable snapshot can overtake the reliable manifest that binds
  its slot; the block is lost, the next one lands.
- **A client sends no input block for a body whose slot has not arrived.** Input rides `INPUT_REDUNDANCY`
  ticks of history, so the first block after the binding lands re-sends what those ticks held.

### Reissuing a freed slot

Ids are reused — a body respawning under its old node name reclaims the same id — and slots are reused too.
Reuse is the one way a slot can be *wrong* rather than merely unknown: a snapshot naming slot `N` can
overtake the manifest that rebound `N` from entity A to entity B, and the receiver would apply B's row to A.

- **A freed slot is quarantined for 256 ticks before it may name a different entity** — ~4.3 s at 60 Hz,
  ~12.8 s at 20 Hz, far longer than the reliable retransmit it has to outlast.
- **The oldest expired slot is reissued first**, so churn spends the whole free list instead of cycling one
  slot.
- **Reuse is preferred over minting**, which holds the index space near the session's peak concurrency
  rather than letting it climb with every spawn. Not exactly at the peak: a slot freed inside the quarantine
  window is unavailable to the next caller, so a fast-churning session does mint past it.

### The cap is declared and refused

16 bits caps a session at **65,536 concurrent entities**. Past that the server refuses to name the entity
and says so once, rather than wrapping an index — a wrapped index would alias two live entities onto one
wire name. A refusal while every free slot is still cooling is transient and retried on the next tick.

**Rate tiering still phases on the 64-bit id**, not the slot. Dense sequential indices spread across an
interval *more* evenly than hashes do, so the choice is about stability: a slot is released and reissued,
and an entity that took a different one would jump its tier phase and its keyframe phase mid-interval.

## Datagram authentication

Every datagram but the handshake carries a **32-bit sequence number and a 64-bit MAC tag**, and is dropped
before a single field is decoded unless both check out.

- **The key** is 16 bytes, minted by the client from Godot's `Crypto`, one per session, and carried in the
  handshake. The server keeps one per connected peer; a peer that has not handshaken has none, and everything
  it sends is refused — including the ping a server used to answer for any connected sender.
- **The MAC** is SipHash-2-4 over the payload, the sequence number, and a **direction byte that is not
  transmitted**. Each side authenticates with the direction it expects to receive, so a datagram reflected
  back at its sender fails the tag check.
- **The replay window** is a 64-entry sliding bitmap, the same construction IPsec uses. A sequence number is
  accepted once; a repeat, or one more than 64 behind the newest accepted, is refused. A datagram whose tag
  fails does not advance the window, so a forger cannot burn sequence numbers the real peer has yet to send.
- **Sequence numbers are refused rather than wrapped.** 32 bits at 60 Hz is 2.2 years of one session.
- **The key crosses the wire in the clear**, so this authenticates a datagram's membership in a session, not
  a peer's identity. An attacker who cannot read the session's traffic cannot forge a datagram at all,
  whatever sender id it puts on one, and one connected peer cannot forge another's. **An on-path observer
  who can read the handshake can do everything the client can** — closing that needs a key exchange. Recorded
  as a limit in the [README](../README.md#limits).

## The ack token

`ack_tick` says which snapshot frame a client holds. The server spends that number twice — it is the base
every masked delta is encoded against, and it is the round-trip sample that sets that peer's rewind depth —
so it is checked rather than believed.

- **The server mints a token per snapshot frame**, from a 16-byte secret it draws per connection at the
  handshake and **never transmits**. The token is SipHash-2-4 over the frame's tick under that secret, so it
  is derived rather than stored and any tick can be checked, including one the sent log has expired.
- **The client quotes back the token of the frame its `ack_tick` names.** It moves only when
  `ack_tick` moves, so the two always name the same frame.
- **An ack that does not carry the right token is discarded whole**: no `newest_ack`, no round-trip sample,
  no `acked_base` promotion. Every one of those is granted on the strength of the claim. It is counted as
  `unproven_acks_s` in [`bandwidth_metrics()`](api.md); an honest client cannot produce one.
- The 32 ack **bits** name frames older than `ack_tick` and prove nothing themselves. They are consumed
  because the tick they hang off was proven, and refused with it when it was not. A peer that lies in the
  bits breaks its own delta chain and NACKs.

**What it does not settle.** A token says the peer received the frame it names, not that it received nothing
newer. A client that advances its ack at full rate while holding a constant lag quotes a real token every
time and is measured at that lag — indistinguishable from a peer behind a traffic shaper, and it gains
nothing that peer does not gain honestly. No wire field closes that: `current - ack` is the whole round trip
whatever tick lead the client runs at, so there is no second quantity the server could derive an independent
figure from. The containment is `NetLagComp.max_delay_ms`, 250 ms by default, which bounds every rewind
whatever the estimate says.

## What the receive path refuses, and what it does not

The backend checks the wire. It does not check your game.

| Refused by the backend | |
| --- | --- |
| A datagram whose MAC does not verify | Forged, corrupted, or reflected. |
| A datagram replaying a sequence number | Or one further back than the replay window reaches. |
| An ack for a frame the peer cannot prove it received | The frame token; see above. The rest of that frame's input blocks are still processed. |
| Anything from a peer that has not handshaken | Including pings. |
| A peer speaking a different protocol major | Rejected at the handshake with a readable message. |
| A block naming a slot with no binding | The spawn or the manifest is still in flight. Skipped cleanly; the rest of the frame decodes. |
| An input block for an entity the sender does not own | The live `get_multiplayer_authority()` check on the input node. |
| An input block stamped too far into the future | Past `INPUT_FUTURE_HORIZON_TICKS` ahead of the server. |
| An input row of the wrong wire stride, or for a tick history has rotated past | |
| More than 64 input blocks from one peer in one tick | The per-peer receive budget. The rest of that frame is abandoned, and so is the rest of a frame that names more than 8 entities the sender does not own. |

| **Not** checked by the backend — yours | |
| --- | --- |
| **Input values** | A row that decodes at the correct stride is written into input history as-is. Range, rate and plausibility are your job, inside `_rollback_tick`. A client is free to send a movement axis of 10<sup>9</sup>. |
| **Command payloads** | `NetCommand` resolves the sender; the *handler* decides whether that sender may do that thing. See [api.md](api.md#netcommand). |
| **Session identity** | Client-asserted, verified by nobody. |
| **Account identity, entitlement, bans** | An authenticated layer above this, whose verified id goes into `set_session_id()`. |

## The clock

The server is ground truth. The client estimates offset from ping/pong samples and trusts the **lowest-RTT
half** of the window — a fast sample spent least time queued, so its offset reading is least polluted.
Correction is a bounded time stretch, not a jump.

**Catch-up must not spiral.** When a frame runs long, running the whole backlog makes the next frame longer
still. `TickAccumulator` caps ticks per frame and **discards** the backlog it refuses to run, reporting that it
did. Re-aligning afterwards is the clock's job.

**Stretch is pinned to exactly 1.0 in coupled mode.** Any stretch ≠ 1.0 slides tick boundaries across physics
frames, producing 0-tick and 2-tick frames that render as judder. Coupled mode runs exactly one net tick per
physics frame and absorbs error by adjusting the client's tick *lead* instead. `stretch()` returns exactly
`1.0` at zero offset — a neutral point at the range midpoint would make a fully synced clock run fast, drift,
and be dragged back forever.

**Adaptive tick lead** closes that loop: the server reports in each snapshot header how early or late that
peer's newest input arrived, and the client steers its lead to keep the margin slightly positive. This is what
collapses the server's per-entity resim window to a single tick for a well-connected peer — one header byte,
large payoff.

## `_rollback_tick(delta, tick, is_fresh)`

Called on the synchronizer root, on every peer that predicts the body.

One invariant that is easy to break and expensive to debug: if your handler re-queries the physics world at
the restored pose — a shapecast, a raycast — the rollback loop must run in `_physics_process` **before** the
physics step. A phase change silently breaks determinism.

### Within-tick entity ordering

The rollback loop replays **one tick across every planned entity at a time**, in three phases — restore all,
simulate all, record all — and within a tick it visits entities in **ascending entity id**. `ResimPlanner`
keeps them in a `BTreeMap` for exactly that reason: a consumer may gate on a bit-exact resim, and a
nondeterministic iteration order would surface there as a phantom desync.

An entity id is FNV-1a of the node path, so **that order is identical on every peer** and is already covered by
whatever gate proves the peers built the same paths. What it is *not* is game-meaningful: whether entity A has
already advanced when entity B's tick runs falls out of two hashes.

The rule that follows: **one direction of cross-entity read per pair, and no cross-entity writes.** A body that
reads another's replicated state sees it either at the start or the end of the tick depending on that fixed
order — the same answer on every peer, which is what matters. A body that *writes* into another's restored
state would have the write land before or after that body's own simulation depending on the same hashes, which
is a coin flip nobody can see.

## Remote prediction reconciles

`net.remote_resim` un-exempts bodies this peer owns neither the state nor the input of, so they join the
rollback loop and are simulated forward every tick. An authoritative row for such a body takes the **predicting**
integration path, exactly as an owner's own body does: the row is compared against the recorded prediction, a
difference marks the planner, and the loop replays from that tick.

It has to. A body that predicted forward without ever re-basing on the server's rows would drift for the whole
session with nothing erroring — and an **inputless shared body** (a puck, a ball, a physics prop) makes that
obvious within seconds, where a remote player body hides it because its own owner's corrections keep the pose
roughly plausible.

Exempt bodies — the default, since `remote_resim` is off unless a game asks for it — are unaffected: they
apply the newest row at the tick boundary and never simulate.

## `is_fresh`

Keyed on **input novelty**, not tick visitation. The backend tracks per-`(entity, tick)` input confidence:

| Level | Meaning |
|---|---|
| `Predicted` | no input at all for this tick |
| `Extrapolated` | input repeated from an older tick |
| `Authoritative` | real received (or locally authored) input stamped for this tick |

**`is_fresh` is true on the first simulation of a tick whose input is `Authoritative` for the simulating
peer.**

- *Owner* — its own input is always authoritative, so a one-shot fires immediately, once.
- *Server* — a remote client's input becomes authoritative when the packet lands, so `is_fresh` fires on the
  resim pass that first carries it, once, however many predicted passes preceded it.

The naive definition ("this `(node, tick)` pair has not been visited") gets this wrong: when the server
predicts tick `T` with no input and *then* receives the client's real input for `T`, it resimulates with
`is_fresh = false` even though that was the first time the real input was seen. Games built on that definition
end up carrying high-water marks and non-replicated tick logs to work around it. Here, one-shot effects gate on
`is_fresh` directly.

For the residual case — an effect that must fire once *and* be undone if a later correction invalidates the
tick — the per-tick memo ring (`NetRollbackHandle.memo_set`/`memo_get`) records on the fresh pass and returns
the same value on every replayed pass.

## Entity lifecycle: the registry outlives the node, briefly

Registration and unregistration are both **deferred** — a synchronizer pushes onto a pending list and
`drain_pending()` applies it at the top of the next `process`/`physics_process`. That keeps game code from
mutating the registry mid-loop, but it means the registry legitimately holds handles to **already-freed nodes**
for a bounded window.

The window is not theoretical. `MultiplayerAPI::poll()` — which emits `peer_packet` — runs **before**
`Node::_process` in the same iteration, so an inbound packet is handled *ahead of* that frame's
`drain_pending`. A client keeps sending input for its avatar for a full round trip after the server freed it.

Two rules fall out:

1. **Never clone a registry handle before checking it.** `Gd::clone` is not infallible: under godot-rust's
   *balanced* safeguards — what a release build ships — `RawGd::clone` calls `check_rtti`, which **panics** on
   a dead instance. So `let s = sync.clone(); if !s.is_instance_valid() { … }` never reaches its own guard.
   Resolve every handle through `live_handle()`, which validates the *borrowed* handle first and only then
   clones.
2. **Re-validate inside the rollback loop.** `run_rollback` builds its ranges once and replays many ticks;
   phase 2 calls `_rollback_tick` under a `base_mut()` surrender, so game code runs *between* phases and may
   despawn a body. Each phase resolves its handle again.

`PendingOp::Unregister` carries the requesting synchronizer's `InstanceId` for a related reason: entity ids
derive from the node path, so a body respawning under its old name reclaims the **same id**, and the
replacement's `Register` can drain ahead of the corpse's `Unregister`. Removing by bare id there would
unregister the live body. Per-peer delta bookkeeping is cleared either way — it describes the departed body,
and a fresh body must not be delta-encoded against it.

The **wire slot** is released by a reconciliation pass rather than beside that removal, because the
registries lose entries three ways — an `Unregister` op, a respawn that supersedes its predecessor, and the
`is_instance_valid` sweep for a node freed without leaving the tree cleanly — and only the first has a call
site to hang a release on. The pass runs when a registration changed, or when the table and the registries
disagree on size, which is what catches the silent sweep.

`tools/orbitnet-smoke.sh` gates all of this by freeing a registered body in each window. Note that such a panic
is *recovered* — the process keeps running with a corrupted frame — so it can only be caught by reading the
log, never by an exit code.
