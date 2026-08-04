# API reference

The whole public surface. `Net` is an autoload; everything else is a `class_name` you can construct.

Everything here is safe to call **OFFLINE**: the facade no-ops, factories return inert handles, and metrics
return zeros. That is what lets a game wire its netcode unconditionally.

---

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

- `root`'s authority is the **server**, so the server owns every body's state.
- `input_node`'s authority is the **owning client**, so each client authors only its own input — the backend
  validates the sender against that authority, which is the anti-forgery check.
- Splitting them **requires input to live on its own node**, because authority is a per-node property.
- `predict` is true on the owning client (local prediction) *and* on the server (authoritative simulation),
  false on a client watching someone else's body.

Your `root` then implements:

```gdscript
func _rollback_tick(delta: float, tick: int, is_fresh: bool) -> void:
```

Called on every peer that predicts this body. See [protocol.md](protocol.md) for what `is_fresh` guarantees.

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

### ⚠️ AOI culls the rollback lane only

`set_aoi_radius()` iterates **rollback entities**, anchors on the entity whose *input* authority is that
peer, takes its first `Vector3` **State**-role property as the centre, and filters the other rollback
entities against it (enter at the radius, leave at 1.25× — hysteresis so boundary entities do not flicker).

Three consequences people discover the hard way:

1. **State-lane entities always replicate.** If your bandwidth is a thousand server-driven objects, AOI does
   nothing for you today.
2. **A peer with no rollback body has no anchor**, and the backend correctly falls back to "everything is in
   interest". Interest management therefore requires at least one rollback entity per peer — which is
   exactly why the RTS demo puts the command cursor on the rollback lane.
3. **The anchor is the first `Vector3` State property**, so register your position first.

---

## `NetRollbackHandle`

| | |
|---|---|
| `is_active() -> bool` | False when inert (OFFLINE). |
| `add_state(node, property)` / `add_input(node, property)` | For handles built with `make_rollback()`. |
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

- **Give the node a stable name.** The RPC routes by node path; every peer must build it identically.
- **Resolve *who* from `sender_id`**, never from the payload. `sender_id` comes from the transport and cannot
  be authored by the sender.
- **A payload parameter must stay untyped `Dictionary`.** It crosses the RPC boundary, where Godot decodes it
  as a plain Dictionary — a `Dictionary[String, Variant]` annotation would *reject* the wire-decoded value.
  Read fields into typed locals inside the handler.
- **Rate-limit per sender.** `request()` is a reliable RPC and the client controls how often it calls one.
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
