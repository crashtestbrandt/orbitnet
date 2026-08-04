# Getting started

Your first replicated body, in about thirty lines. This page assumes you have a Godot 4.4+ project and
nothing else.

## 1. Install

Copy **both** addon directories into your project:

```
your-project/
  addons/orbitnet/          the GDScript surface
  addons/orbitnet_native/   the .gdextension + binaries
```

Both are required. `Net` without the extension is a facade over nothing, and the failure mode is quiet — the
autoload exists, methods return, and no packet is ever sent.

Enable **OrbitNet** in *Project → Project Settings → Plugins*. That registers the `Net` autoload. Check your
`project.godot` now contains:

```ini
[autoload]

Net="*res://addons/orbitnet/net.gd"
```

That line is what makes `Net` exist in an **exported** build. The EditorPlugin only adds it in the editor.

Restart the editor once so the extension loads.

## 2. Confirm it actually loaded

```gdscript
func _ready() -> void:
    print(Net.current_mode())          # 0 == Net.Mode.OFFLINE
    print(Net.perf_summary())          # "offline (no rollback loop)"
```

If `Net` is `null`, the plugin is not enabled. If `Net` exists but the editor logged something about an
invalid ELF header, the binary did not open — see [building.md](building.md#the-binary-distribution-policy).

## 3. Configure the tick

Add an `[orbitnet]` block to `project.godot`:

```ini
[orbitnet]

sync_to_physics=true    ; run the net tick AT the physics rate
tickrate=60             ; used when sync_to_physics is false
history_limit=128       ; rollback history depth, per rollback entity
max_time_stretch=1.05
```

**Coupled** (`sync_to_physics=true`) runs the net tick inside `_physics_process`, before the physics step;
your body writes its pose every physics tick and Godot's own physics interpolation renders it. This is the
right default for a character game.

**Decoupled** (`Net.set_net_tick_decoupled(20)`) paces the net loop off the wall clock at a lower rate while
physics stays at its own. Entities then need render interpolation between net ticks — see step 6.

## 4. Bring a session up

The order matters, and getting it wrong produces bugs that are invisible rather than loud:

```gdscript
func host(port: int) -> void:
    # 1. Set the tick rate BEFORE the loop starts. set_mode() starts it.
    Net.set_net_tick_decoupled(20)          # (or leave it coupled)

    # 2. Build your world. Node paths are what entity ids are derived from, so the graph should exist
    #    before any packet can arrive.
    _build_world()

    # 3. Bind the transport.
    var peer: MultiplayerPeer = NetTransport.create_server(port, 8)
    if peer == null:
        return
    multiplayer.multiplayer_peer = peer

    # 4. NOW leave OFFLINE. This starts the tick loop.
    Net.set_mode(Net.Mode.HOST)             # or SERVER (dedicated) / CLIENT

    # 5. Register your entities. Not earlier: while the facade is OFFLINE, every lane returns an INERT
    #    handle, so registrations made before this point silently do nothing.
    _register_entities()
```

That last point is the one everyone hits. `Net.make_state()` and friends return inert handles while OFFLINE
— that is deliberate, and it is what lets single player run the same code with no networking — but it means
registration must happen *after* `set_mode`.

On teardown, `Net.set_mode(Net.Mode.OFFLINE)` and, if you decoupled, `Net.set_net_tick_coupled()`. The
decouple is a process-wide setting; leaving it set is how "it only happens on the second game" starts.

## 5. Your first replicated body

A player character: the server owns its **state**, the owning client authors its **input** and predicts
locally.

```gdscript
extends CharacterBody3D
class_name Player

var input_node: Node = null
var _handle: NetRollbackHandle = null

func setup(owner_peer: int) -> void:
    # Input lives on its OWN node, because the two halves need different authorities.
    input_node = preload("res://player_input.gd").new()
    input_node.name = "Input"                    # a STABLE name: it is part of the wire schema
    add_child(input_node)
    input_node.set_multiplayer_authority(owner_peer)   # the client authors only its own input

    var predict: bool = Net.is_server() or owner_peer == multiplayer.get_unique_id()
    _handle = Net.register_rollback_body(
        self, input_node,
        ["position@half", "velocity"],           # STATE    — server-authored
        ["move_dir", "jump"],                    # INPUT    — client-authored
        predict,
        ["anim_state"])                          # COSMETIC — replicated, never restored

# Called by the backend once per tick, on the server for every body and on the owner for its own.
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

Then feed the input on `Net.pre_tick`, which fires *before* the backend records that tick:

```gdscript
func _ready() -> void:
    Net.pre_tick.connect(_on_pre_tick)

func _on_pre_tick(_tick: int) -> void:
    if not is_multiplayer_authority():
        return
    input_node.move_dir = Input.get_vector("left", "right", "forward", "back")
    input_node.jump = Input.is_action_pressed("jump")
```

That is the whole loop: input in, `_rollback_tick` runs on server and owner, state out, remotes apply it.

### The four things to get right

1. **`add_child` before you register.** The entity id is a hash of the node's *path*, so a node that moves
   after registration is a node whose id no longer matches.
2. **Name every replicated node explicitly.** `add_child(Node3D.new())` produces `@Node3D@27`, and that
   number is an allocation counter that will differ between peers. Nothing errors; replication just goes
   nowhere. See `RtsNames` in the demo for the pattern.
3. **The property SET must be identical on every peer.** The wire schema is positional; the backend hashes
   it and refuses to misapply a mismatch.
4. **`is_multiplayer_authority()` is not the same as `predict`.** `predict` is true on the server (which
   simulates everything) *and* on the owning client (which predicts its own). It is false on a client
   watching someone else's body.

## 6. Non-predicted things: the state lane

Anything the server simply *tells* you about — a door, a score, an AI unit — does not want rollback.

```gdscript
var _state: NetStateHandle = null
var _interp: NetInterpolatorHandle = null

func bind_net() -> void:
    _state = Net.make_state(self)
    _state.add_state(self, "position@half")
    _state.add_state(self, "hp")
    _state.process_settings()

    # Only on peers that RECEIVE this entity. The server writes it every tick; smoothing there would fight
    # the authoritative value.
    if Net.is_client() and not Net.is_server():
        _interp = Net.make_interpolator(self)
        _interp.add_property(self, "position")
        _interp.process_settings()
```

**Values written outside the tick belong here, not on the rollback lane.** The rollback lane restores
recorded history onto its properties every tick, so a value a command handler wrote is overwritten before
anyone sees it.

## 7. Player actions: the command lane

For a discrete request — equip, interact, order units — use `NetCommand`. It runs on the server, which
validates *and* applies in one place, so an unvalidated request can never reach your state.

```gdscript
var commands: NetCommand = null

func _ready() -> void:
    commands = NetCommand.new()
    commands.name = "Commands"          # STABLE: the RPC routes by node path
    add_child(commands)
    commands.register(&"equip", _validate_and_equip)

# Runs ONLY on the applying peer (server, or the local peer offline).
func _validate_and_equip(sender_id: int, payload: Dictionary) -> bool:
    if sender_id != owner_peer:         # resolve WHO from the sender id, never from the payload
        return false
    var index_value: Variant = payload.get("index", -1)
    var index: int = index_value if index_value is int else -1
    if index < 0 or index >= _slots.size():
        return false
    _equipped = index                   # a STATE-lane property, so it survives the next tick
    return true

# On any peer, from the owning client:
commands.request(&"equip", {"index": 2})
```

Offline, `request()` applies immediately — single player is its own authority — so the same code path works
with no session at all.

## 8. Diagnostics

```gdscript
Net.clock_metrics()   # rtt_ms, jitter_ms, offset_ms, stretch, lead_ticks
Net.perf_metrics()    # resim_ticks, rollback_ms, net_ms, rb_nodes
Net.perf_summary()    # the same thing as one printable line
```

These are live in **every** build, release included — they are a byproduct of the loop, not debug monitors.
Put them on screen early; almost every netcode question is answered faster by looking at `stretch` and
`resim_ticks` than by reading code.

## Where to go next

- [api.md](api.md) — the full surface, including wire quantization and the f64/i64 scalar reality.
- [rts-demo.md](rts-demo.md) — a complete worked example that is *not* a character shooter, with the byte
  budget spelled out.
- [protocol.md](protocol.md) — what is actually on the wire, and why `is_fresh` means what it means.
