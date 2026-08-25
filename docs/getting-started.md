# Getting started

A replicated, client-predicted player body. Assumes a Godot 4.4+ project and nothing else.

## 1. Install

Copy `addons/orbitnet/` **and** `addons/orbitnet_native/` into your project, then enable **OrbitNet** under
*Project → Project Settings → Plugins*. Confirm `project.godot` now has:

```ini
[autoload]

Net="*res://addons/orbitnet/net.gd"
```

That line is what makes `Net` exist in an **exported** build — the EditorPlugin only adds it in the editor.
Restart the editor once so the extension loads, then check it:

```gdscript
print(Net.current_mode())    # 0 == Net.Mode.OFFLINE
print(Net.perf_summary())    # "offline (no rollback loop)"
```

`Net` is `null` → the plugin is not enabled. An "invalid ELF header" in the log → the binary did not open, see
[building.md](building.md#the-binary-distribution-policy).

## 2. Configure the tick

```ini
[orbitnet]

sync_to_physics=true    ; run the net tick AT the physics rate
tickrate=60             ; used only when sync_to_physics is false
history_limit=128       ; rollback history depth, PER ROLLBACK ENTITY
max_time_stretch=1.05
```

**Coupled** (`sync_to_physics=true`) runs the net tick inside `_physics_process`, before the physics step;
your body writes its pose every physics tick and Godot's physics interpolation renders it. Right default for a
character game.

**Decoupled** (`Net.set_net_tick_decoupled(20)`) paces the loop off the wall clock at a lower rate. Entities
then need render interpolation between ticks — step 5.

## 3. Bring a session up

The order matters, and getting it wrong fails silently:

```gdscript
func host(port: int) -> void:
    Net.set_net_tick_decoupled(20)     # 1. BEFORE set_mode() — that is what starts the loop
    _build_world()                     # 2. node paths are what entity ids derive from

    var peer: MultiplayerPeer = NetTransport.create_server(port, 8)
    if peer == null:
        return
    multiplayer.multiplayer_peer = peer  # 3. bind the transport

    Net.set_mode(Net.Mode.HOST)        # 4. leave OFFLINE; the loop starts here
    _register_entities()               # 5. NOT earlier — see below
```

**Every lane returns an inert handle while OFFLINE.** That is deliberate — it lets single player run the same
code with no networking — but it means registration must happen *after* `set_mode`, and `set_mode` needs a peer
already assigned. Registering earlier does nothing, quietly.

On teardown: `Net.set_mode(Net.Mode.OFFLINE)` and, if you decoupled, `Net.set_net_tick_coupled()`. The decouple
is process-wide; leaking it is how "only on the second game" bugs start.

**Seat players on `Net.peer_joined`, not on `multiplayer.peer_connected`.** The transport signal fires when
the socket comes up, which is before the OrbitNet handshake — so the peer's session identity is not known yet,
and identity is the only thing that tells a reconnecting player from a newcomer. Key your roster on
`session_id`, not on the peer id:

```gdscript
Net.peer_joined.connect(_on_peer_joined)          # (peer, session_id, resumed_from)
Net.peer_dropped.connect(_on_peer_dropped)        # (peer, session_id, held)
Net.peer_session_expired.connect(_on_expired)     # (session_id, peer) — release the seat here
```

A dropped peer's session is held open for 30 s by default and its body is held on the neutral input row —
see [api.md](api.md#session-identity-and-reconnection).

## 4. A replicated body

The server owns its **state**; the owning client authors its **input** and predicts locally.

```gdscript
extends CharacterBody3D

var input_node: Node = null
var _handle: NetRollbackHandle = null

func setup(owner_peer: int) -> void:
    # Input lives on its OWN node: the two halves need different authorities, and
    # authority is a per-node property.
    input_node = preload("res://player_input.gd").new()
    input_node.name = "Input"                          # STABLE — part of the wire schema
    add_child(input_node)
    input_node.set_multiplayer_authority(owner_peer)

    var predict: bool = Net.is_server() or owner_peer == multiplayer.get_unique_id()
    _handle = Net.register_rollback_body(
        self, input_node,
        ["position@half", "velocity"],   # STATE    — server-authored
        ["move_dir", "jump"],            # INPUT    — client-authored
        predict,
        ["anim_state"])                  # COSMETIC — replicated, never restored

# Runs on the server for every body, and on the owner for its own.
func _rollback_tick(delta: float, _tick: int, _is_fresh: bool) -> void:
    velocity.x = input_node.move_dir.x * 6.0
    velocity.z = input_node.move_dir.z * 6.0
    if input_node.jump and is_on_floor():
        velocity.y = 5.0
    move_and_slide()
```

```gdscript
# player_input.gd
extends Node
var move_dir: Vector3 = Vector3.ZERO
var jump: bool = false
```

Feed input on `pre_tick`, which fires *before* the backend records the tick:

```gdscript
func _on_pre_tick(_tick: int) -> void:
    if not is_multiplayer_authority():
        return
    input_node.move_dir = Input.get_vector("left", "right", "forward", "back")
    input_node.jump = Input.is_action_pressed("jump")
```

### Five things to get right

1. **`add_child` before you register.** The entity id hashes the node's *path*.
2. **Name every replicated node explicitly.** `add_child(Node3D.new())` yields `@Node3D@27` — an allocation
   counter that differs between peers. Nothing errors; replication just goes nowhere.
3. **The property set must be identical on every peer.** The schema is positional and hash-checked.
4. **`predict` ≠ `is_multiplayer_authority()`.** It is true on the server (simulates everything) *and* on the
   owning client (predicts its own), false on a client watching someone else's body.
5. **`predict` is read once, and ownership moves.** If your world is built before the roster arrives — which it
   is, for every client that builds its scene and then joins — every body registers with `predict = false`, and
   that does not merely defer prediction: it **exempts the body from the rollback loop**. An exempt body still
   applies the rows it receives, so it moves and every readout looks ordinary while the player's own input is a
   full round trip late. Call `NetRollbackHandle.set_predicted()` whenever ownership changes:

   ```gdscript
   func set_owner_peer(peer: int) -> void:
       handle.assign_seat(peer, seat)
       handle.set_predicted(Net.is_server() or peer == multiplayer.get_unique_id())
   ```

## 5. Non-predicted things: the state lane

Anything the server simply tells you about — a door, a score, an AI unit:

```gdscript
_state = Net.make_state(self)
_state.add_state(self, "position@half")
_state.add_state(self, "hp")
_state.process_settings()

# Only on peers that RECEIVE this entity — the server writes it every tick, so
# smoothing there would fight the authoritative value.
if Net.is_client() and not Net.is_server():
    _interp = Net.make_interpolator(self)
    _interp.add_property(self, "position")
    _interp.process_settings()
```

**Values written outside the tick belong here**, not on the rollback lane.

## 6. Player actions: the command lane

```gdscript
commands = NetCommand.new()
commands.name = "Commands"        # STABLE: the RPC routes by node path
add_child(commands)
commands.register(&"equip", _validate_and_equip)

# Runs ONLY on the applying peer (server, or the local peer offline).
func _validate_and_equip(sender_id: int, payload: Dictionary) -> bool:
    if sender_id != owner_peer:   # resolve WHO from the sender id, never from the payload
        return false
    var raw: Variant = payload.get("index", -1)
    var index: int = raw if raw is int else -1
    if index < 0 or index >= _slots.size():
        return false
    _equipped = index             # a STATE-lane property, so it survives the next tick
    return true

commands.request(&"equip", {"index": 2})
```

The handler validates **and** applies in one place, so an unvalidated request cannot reach your state. Offline
it applies immediately — single player is its own authority.

## 7. Diagnostics

```gdscript
Net.clock_metrics()   # rtt_ms, jitter_ms, offset_ms, stretch, lead_ticks
Net.perf_metrics()    # resim_ticks, rollback_ms, net_ms, rb_nodes
```

Live in **every** build — they are a byproduct of the loop, not debug monitors. Put them on screen early; most
netcode questions are answered faster by `stretch` and `resim_ticks` than by reading code.

## Next

[api.md](api.md) for the full surface and wire quantization · [rts-demo.md](rts-demo.md) for a worked
non-shooter example · [protocol.md](protocol.md) for what is actually on the wire.
