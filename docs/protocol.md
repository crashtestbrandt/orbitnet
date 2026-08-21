# Protocol and the tick model

What is on the wire, how the clock works, and what `is_fresh` guarantees. Read this before changing
`native/crates/orbitnet-core/`, or when a replication bug does not make sense.

## Wire format

Little-endian.

**Hot frame** — unreliable, one per peer per tick:

```
frame kind | tick | ack tick (zigzag delta) | 32-bit ack bitfield | input-arrival margin byte
then per entity:  { entity id | flags | changed-property bitmask | packed payload }
```

No property names, no type tags — the schema is positional and agreed in advance. Client input carries the
last N ticks for redundancy, so a single lost packet costs nothing.

**Control frames** — reliable: handshake, entity schema, entity binding, reject.

**Versioning.** `PROTOCOL_VERSION` packs `(major << 16) | (minor << 8) | patch`; **major must match exactly**.
The schema hash is FNV-1a over `(name, kind, role)` in **declaration order** — deliberately order-sensitive,
because two peers registering the same properties in different orders would otherwise silently misapply state,
which is miserable to diagnose. A mismatch produces an operator-readable message naming both versions or both
hashes, never a desync.

**Robustness.** The decoder is the one component parsing bytes chosen by a remote peer, so every read is
bounds-checked and returns an error rather than panicking — a decoder that panics on a malformed packet is a
remote denial of service. `#![forbid(unsafe_code)]` means a bounds bug cannot become memory unsafety either.
Tests sweep truncated and pseudo-random buffers.

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

`tools/orbitnet-smoke.sh` gates all of this by freeing a registered body in each window. Note that such a panic
is *recovered* — the process keeps running with a corrupted frame — so it can only be caught by reading the
log, never by an exit code.
