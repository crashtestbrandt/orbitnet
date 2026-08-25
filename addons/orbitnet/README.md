# OrbitNet

Rollback netcode for Godot 4, in Rust.

This directory is the **GDScript surface**: the `Net` facade, the transport factory, and the typed handles
gameplay is written against. The backend it drives is a Rust GDExtension whose binaries live in the sibling
addon **`addons/orbitnet_native/`** — install **both**, or `Net` is a facade over nothing.

Full documentation: [`docs/`](../../docs/), starting with
[`docs/getting-started.md`](../../docs/getting-started.md).

## Three lanes

| Lane | Created by | For |
|---|---|---|
| **Rollback** | `Net.register_rollback_body()` | continuous per-tick input, predicted locally |
| **State** | `Net.make_state()` | server-authoritative values pushed every tick |
| **Command** | `NetCommand` | sparse, discrete, reliable, server-validated requests |

**The rollback lane restores recorded history every tick**, so a value written from outside the tick — a
command handler, a timer, a signal — is silently overwritten. Those belong on the state lane.

## Two enforced boundaries

- **`net.gd` is the ONLY file that names a backend class.** `tools/net-check.sh` fails the build otherwise —
  including in the demos, which is the point: a demo that reaches past the facade has found a hole in it.
- **`steam_transport.gd` is the ONLY file that names Steamworks.** Every access is dynamic, so a non-Steam
  build carries zero Steam dependency and the project runs where GodotSteam is not installed.

## Layout

```
orbitnet.gd            EditorPlugin — installs the Net autoload
net.gd                 the facade (the ONLY file naming backend classes)
net_transport.gd       transport factory      steam_transport.gd   its Steam arm
net_command.gd  net_state_handle.gd  net_rollback_handle.gd  net_interp_handle.gd
net_lag_comp.gd  net_ray.gd  net_session_info.gd
bench/                 netbench — impairment relay, bot fleet, tick-domain gates
  bench_subject.gd       the seam between the bench and YOUR game
```

## License

MIT OR Apache-2.0, at your option. See the repository's `LICENSE` and `THIRD_PARTY.md`.
