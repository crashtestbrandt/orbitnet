# Protocol and the tick model

The wire format, the tick clock, the rollback loop, and the entity lifecycle. This is the page to read
before changing anything in `native/crates/orbitnet-core/`, and the page to read when a replication bug does
not make sense.

For what a *consumer* needs -- which lane to put a property on, and how to quantize it -- see
[api.md](api.md) and [rts-demo.md](rts-demo.md).

## Wire protocol

Little-endian. Not interoperable with the previous backend — this was a hard cutover, and there is no
compatibility mode.

**Hot frame** (unreliable, one per peer per tick): frame kind, tick, ack tick (zigzag delta — on a client input
frame the ack can *lead* the frame's own tick), a 32-bit ack bitfield, an input-arrival margin byte, then
per-entity blocks of `{entity id, flags, changed-property bitmask, packed payload}`. No property names, no type
tags. Client input carries the last N ticks for redundancy.

**Control frames** (reliable): handshake, entity schema, entity binding, reject.

**Versioning.** `PROTOCOL_VERSION` is packed `(major << 16) | (minor << 8) | patch`; **major must match
exactly**, patch and minor may differ. The schema hash is FNV-1a over `(name, kind, role)` in declaration order
— deliberately order-sensitive, because the previous backend would silently misapply state when two peers
registered properties in different orders, and that failure is miserable to diagnose. A mismatch produces an
operator-readable message naming both versions or both hashes, not a desync.

**Robustness.** The decoder is the one component that parses bytes chosen by a remote peer, so every read is
bounds-checked and returns an error rather than panicking — a decoder that panics on a malformed packet is a
remote denial of service. `#![forbid(unsafe_code)]` means a bounds bug cannot become memory unsafety either.
Tests sweep truncated and pseudo-random buffers to assert this.

---

## Tick and rollback model

### Clock

Server is ground truth; the client estimates offset from ping/pong samples and trusts the **lowest-RTT half**
of the window (a fast sample spent least time queued, so its offset reading is least polluted). Correction is a
bounded time stretch, not a jump (`core::clock`).

**Catch-up must not spiral.** When a frame runs long, running the whole backlog makes the next frame longer
still. `TickAccumulator` caps ticks per frame and **discards** the backlog it refuses to run, reporting that it
did (`TickStep::clamped`). Re-aligning afterwards is the clock's job.

Two behaviours of the previous backend were fixed rather than inherited:

- **The stretch neutral point.** Its mapping put neutral at the range *midpoint*, so a fully synced clock still
  ran fast, drifted, and was dragged back forever (a consuming project patched this locally). `stretch()` now returns
  exactly `1.0` at zero offset, asserted by the bound tests.
- **Stretch under `sync_to_physics`.** Any stretch ≠ 1.0 slides tick boundaries across physics frames,
  producing 0-tick and 2-tick frames that render as judder. In coupled mode the right behaviour is to pin
  stretch to 1.0, run exactly one net tick per physics frame, and absorb error by adjusting the client's tick
  *lead*. `tools/instr/stutter_probe.gd` gates it.

**Adaptive tick lead** closes that loop: the server reports, in each snapshot header, how early or late that
peer's newest input arrived; the client steers its lead to keep the margin slightly positive. The previous
backend stamped input a fixed delay into the future and hoped. This is what makes the server's per-entity
window collapse to a single tick for a well-connected peer — one header byte, large payoff.

### `_rollback_tick(delta, tick, is_fresh)`

**Signature unchanged across the cutover.** Zero game-code churn.

One invariant that is easy to break and expensive to debug: `player_body._rollback_tick` re-queries a
`ShapeCast3D` at the restored pose (`_sample_surface_at`), so the rollback loop must keep running in
`_physics_process` *before* the physics step. A phase change silently breaks the determinism probe.

### `is_fresh`, fixed structurally

The old `is_fresh` meant "this `(node, tick)` pair has not been visited before" — a visitation high-water mark.
When the server predicted tick `T` with no input and *then* received the client's real input for `T`, it
resimulated with `is_fresh = false` even though **that was the first time the real input was seen**. Hence the
high-water marks and non-replicated 256-tick log that used to live in `weapon_authority.gd`.

Freshness should be keyed on **input novelty**, not tick visitation. OrbitNet tracks a per-`(entity, tick)`
input confidence:

| Level | Meaning |
|---|---|
| `Predicted` | no input at all for this tick |
| `Extrapolated` | input repeated from an older tick |
| `Authoritative` | real received (or locally authored) input stamped for this tick |

**`is_fresh` is true on the first simulation of a tick whose input is `Authoritative` for the simulating peer.**

- *Owner*: its own input is always authoritative, so a one-shot fires immediately, once — the player's feel is
  unchanged.
- *Server*: a remote client's input becomes authoritative when the packet lands, so `is_fresh` fires on the
  resim pass that first carries it, once, however many predicted passes preceded it.

For the residual case — an effect that must fire once *and* be undone if a later correction invalidates the
tick — a `commit_once(node, tick, key)` primitive returns true exactly once per key at authoritative confidence
and rolls its own ledger back on resimulation. The proof the fix is real: `just net-probe` passes its
authoritative-fire / no-double-fire assertions with the old workarounds deleted.

### Entity lifecycle — the registry outlives the node, briefly

Registration and unregistration are both **deferred**: a synchronizer pushes onto a thread-local pending list
(`register_rollback_entity` / `unregister_entity`) and `drain_pending()` applies it at the top of the next
`process`/`physics_process`. That decoupling is what keeps game code from mutating the registry mid-loop — but
it means the registry legitimately holds handles to **already-freed nodes** for a bounded window. Two things
fall out, and both have bitten us:

1. **Never clone a registry handle before checking it.** `Gd::clone` is not infallible. A release build of this
   extension ships with godot-rust's *balanced* safeguards (`debug_assertions` off ⇒ level 1), where
   `RawGd::clone` calls `check_rtti`, which **panics** on a dead instance. So
   `let s = sync.clone(); if !s.is_instance_valid() { continue; }` never reaches its own guard. Resolve every
   handle through `live_handle(&sync)`, which validates the *borrowed* handle (an object-database lookup that
   dereferences nothing) and only then clones.

   The window is not theoretical: `MultiplayerAPI::poll()` — which emits `SceneMultiplayer`'s `peer_packet` —
   runs **before** `Node::_process` in the same iteration, and a networked session ticks decoupled, so an
   inbound packet is handled *ahead of* that frame's `drain_pending`. A client keeps sending input for its
   avatar for a full round trip after the server freed it, which is why this presented as "the host dies right
   after a death or respawn".

2. **Re-validate inside the rollback loop.** `run_rollback` builds its `ranges` once and then replays many
   ticks; phase 2 calls `_rollback_tick` under a `base_mut()` surrender, so game code runs *between* the phases
   and may despawn a body. Each phase resolves its handle again.

`PendingOp::Unregister` carries the requesting synchronizer's `InstanceId` for a related reason: entity ids are
derived from the node path, so a body that respawns under its old name (`Player_7`) reclaims the **same id**,
and the replacement's `Register` can drain ahead of the corpse's `Unregister`. Removing by bare id there would
unregister the live body. The per-peer delta bookkeeping (`last_sent` / `acked_base` / `interest`) is cleared
either way — it describes the departed body, and a fresh body must not be delta-encoded against it.

`tools/orbitnet-smoke.sh`'s lifecycle section gates all of this: it frees a registered body in each window and
fails the run if the log carries a freed-instance panic. Note that such a panic is *recovered* — the process
keeps running with a corrupted frame — so it can only be caught by reading the log, never by an exit code.

---
