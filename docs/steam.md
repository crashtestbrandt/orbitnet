# The Steam transport

OrbitNet ships an ENet transport and a Steam one. **Which you get is a build-time fact**, decided by an
export-preset feature tag, not by runtime config.

```gdscript
NetTransport.preferred_kind()        # STEAM if OS.has_feature("steam"), else ENET
NetTransport.create_server(port, max_clients, friends_only)
NetTransport.create_client(address)  # address is an IP:port on ENet, a lobby handle on Steam
```

Your game never learns which is in play. That is the factory's whole job.

## What this repository does and does not contain

**Contains:** `addons/orbitnet/steam_transport.gd` — the Steam arm of the factory. Every Steam access is
**dynamic** (`Engine.has_singleton`, `callv`, `ClassDB`), so a non-Steam build carries zero Steam dependency
and the project lints and runs on a machine with no Steam integration installed at all.

**Does not contain:** any Steamworks code, headers or binaries. Using this path means you install
[GodotSteam](https://godotsteam.com/) yourself and accept the Steamworks SDK licence directly with Valve.

`steam_transport.gd` is the **only** file permitted to name Steamworks, enforced by `tools/net-check.sh`.

## Enabling it

1. Install GodotSteam into your project.
2. Set your app id in `project.godot` under `steam/app_id`, and write it to a `steam_appid.txt` beside the
   binary for development runs.
3. Add `custom_features="steam"` to the export preset. That tag is what flips
   `NetTransport.preferred_kind()`.

A dedicated-server preset additionally wants `dedicated_server` — a Steam game server has no logged-in user,
so it registers as a game server rather than creating a client-owned matchmaking lobby.

## What the Steam arm provides

Beyond the peer itself, four Steam-blind seams — each degrading cleanly to nothing on ENet, so your UI needs
no branches:

| Seam | On Steam | On ENet |
|---|---|---|
| `local_display_name()` | the persona name | `""`, or the local override |
| `local_steam_id()` | the account id | `0` |
| `request_sessions()` / `sessions()` | discovered joinable lobbies | always empty — you join by address |
| invites | accept + send, wired through the overlay | never fire; `can_invite()` is false |

`set_local_display_name()` overrides the name on any build, so the whole name pipeline is exercisable offline
and in CI where there is no persona.

## Lobby metadata

A host advertises a lobby carrying its player cap, current headcount and a game tag; the browser reads
**that metadata** rather than querying each lobby's members. One round trip for the whole list instead of one
per row, and a lobby that is full or mid-teardown is filterable before anyone tries to join it.

Republish the headcount off the same peer-connect boundary the session layer already watches, so the browser
cannot show a stale count.

## Auth tickets

A dedicated server has no user account, so it cannot infer trust from a lobby. The transport exchanges
Steam auth session tickets: the client sends one on connect, the server validates it with Valve and kicks on a
definitive negative verdict.

The gate is deliberately **fail-open on an indeterminate result** — a validation timeout or an unreachable
Steam backend must not lock every player out of your server. It bites only on a definitive rejection.

## Testing without Steam

Everything above is inert on an ENet build, so the whole netcode surface — including `just netbench` and the
demos — runs with no Steam installed. What genuinely cannot be tested that way: persona names, lobby
discovery, invites, and ticket validation. Those need a real Steam build, two accounts and a manual pass.
