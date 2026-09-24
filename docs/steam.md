# The Steam transport

OrbitNet ships an ENet transport and a Steam one. **Which you get is a build-time fact**, decided by an
export-preset feature tag, not by runtime config.

```gdscript
NetTransport.preferred_kind()        # STEAM if OS.has_feature("steam"), else ENET
NetTransport.create_server(port, max_clients, friends_only)
# `target` is ADDR, ADDR:PORT or [LITERAL]:PORT on ENet, the host's 64-bit Steam id on Steam.
NetTransport.create_client(NetTransport.target_address(target), NetTransport.target_port(target))
```

Your game never learns which is in play. That is the factory's whole job.

## What this repository does and does not contain

**Contains:** `addons/orbitnet/steam_transport.gd` — the Steam arm of the factory. Every Steam access is
**dynamic** (`Engine.has_singleton`, `callv`, `ClassDB`), so a non-Steam build carries zero Steam dependency
and the project lints and runs on a machine with no Steam integration installed at all.

**Does not contain:** any Steamworks code, headers or binaries. Using this path means you install
[GodotSteam](https://godotsteam.com/) yourself and accept the Steamworks SDK license directly with Valve.

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

**A host can open its session before it is findable.** `create_server(port, max, friends_only, false)` opens
the peer and publishes nothing: no lobby for a listen host, no server-browser listing for a dedicated one.
`advertise_session()` publishes it. A game whose world has to be built with the peer already set calls it once
that world exists, so no browser row points at a host with nothing to join. `is_session_advertised()` records
the same state on ENet, where nothing is published, so the order is testable without Steam.

## Auth tickets

A dedicated server has no user account, so it cannot infer trust from a lobby. The transport exchanges
Steam auth session tickets: the client sends one on connect, the server validates it with Valve and kicks on a
definitive negative verdict.

The gate is deliberately **fail-open on an indeterminate result** — a validation timeout or an unreachable
Steam backend must not lock every player out of your server. It bites only on a definitive rejection.

## Where a session secret comes from

`Net.set_session_secret()` derives every datagram key of a session from bytes both ends already hold, so the
join handshake carries a nonce instead of the key and an on-path observer can no longer forge. It needs a
secret the game distributed on a channel it **already authenticated**, and the two seams above are exactly
that channel.

| Source | Who sets what | What it is worth |
|---|---|---|
| **Lobby metadata** | the host writes a per-lobby value beside the player cap and the game tag; every joiner reads it off the same row it read the headcount from | Valve delivers it only to members of that lobby, so it is as private as lobby membership. A public lobby anyone may join hands it to anyone who joins. |
| **Auth tickets** | a dedicated server already validates each client's ticket with Valve; the value it derives per session goes to that client over the same validated exchange | tied to an account Valve confirmed, which is the strongest of the two |

Two rules, both about where the value is **not** allowed to come from:

- **Never a build-time constant.** One secret compiled into every copy of the game is public the moment one
  copy ships, and it degrades every session to the cleartext-key regime while looking like it did not.
- **Never a value a player can read and retype.** A lobby code shown on screen derives a key worth exactly
  that code's entropy. `Net.set_session_secret()` accepts any length and folds it, and the fold cannot add
  entropy that was not supplied.

On ENet there is no equivalent seam and the game supplies its own — a value from whatever account service it
already runs. A session that sets none stays on the cleartext key, which is what every session did before.
See [protocol.md](protocol.md#datagram-authentication) for the two regimes and for what a misconfiguration
looks like from each side.

## What each transport authenticates and encrypts

**OrbitNet encrypts nothing on any transport.** Its datagram layer authenticates; confidentiality, where a
session has any, comes from the link underneath it and differs per transport.

| Transport | What is authenticated | What is encrypted | By whom |
| --- | --- | --- | --- |
| **ENet** | every datagram but the handshake, by a 64-bit SipHash tag and a 64-entry replay window. Nothing about the peer behind the connection. | nothing. Every payload is raw UDP in the clear | OrbitNet's datagram layer, with nothing under it |
| **Steam** | the same datagram tag, and the connection itself — each end presents a certificate signed by Valve's PKI, so a verified Steam identity is attached to the peer before a payload flows | every packet — AES-GCM-256, keyed by a Curve25519 exchange | OrbitNet for the datagram, SteamNetworkingSockets for the connection |

**Every Steam row above rests on Valve's documentation, and none of it is exercised in this repository's
CI.** Each claim is cited below. See [Testing without Steam](#testing-without-steam) for what confirming it
takes, and the `VERIFY-ON-A-STEAM-BUILD` marker in `addons/orbitnet/steam_transport.gd` for the same caveat
on the call signatures quoted here.

What Valve's own documentation supports, read 2026-09-24:

- **The cipher.** "AES-GCM-256 per packet, Curve25519 for key exchange and cert signatures. The details for
  shared key derivation and per-packet IV are based on the design used by Google's QUIC protocol."
  ([GameNetworkingSockets](https://github.com/ValveSoftware/GameNetworkingSockets))
- **Encryption is on by default and both ends have to opt out of it.**
  `k_ESteamNetworkingConfig_Unencrypted` is "a dev configuration value, since its purpose is to disable
  encryption ... it requires the peer to also modify their value in order for encryption to be disabled."
  ([steamnetworkingtypes.h](https://github.com/ValveSoftware/GameNetworkingSockets/blob/master/include/steam/steamnetworkingtypes.h))
- **It is end to end rather than terminated at a relay.** Certificates are "an end-to-end concept, and can be
  used in all forms of SteamNetworkingSockets communication, including direct UDP connectivity or P2P", and
  traffic carried by the relay network is "authenticated, encrypted, and rate-limited".
  ([Steam Datagram Relay](https://partner.steamgames.com/doc/features/multiplayer/steamdatagramrelay))
- **Identity authentication depends on how the connection was opened, and encryption does not.** A connection
  addressed by **Steam identity** carries a certificate at both ends. An **IP-addressed** connection can fail
  to obtain one, and Valve's default is to refuse it rather than proceed —
  `k_ESteamNetworkingConfig_IP_AllowWithoutAuth` is documented as "Don't automatically fail IP connections
  that don't have strong auth". A connection admitted that way is still encrypted, under a key exchange with
  no verified peer on the other end, which is the same on-path substitution
  [protocol.md](protocol.md#datagram-authentication) records against an unauthenticated exchange.
- **The Steam arm here only ever connects by Steam id.** `create_client(steam_id, virtual_port)` takes a
  64-bit account id and never an address, and `create_host(virtual_port)` takes no address either, so the
  unauthenticated IP case is not reachable through this transport. A dedicated server logs on anonymously and
  has no account of its own, which is why it authenticates its clients with the auth tickets above rather than
  from a lobby.

### What a game inherits and what it configures

- **Confidentiality on Steam is inherited.** The game writes no code for it and OrbitNet contributes nothing
  to it. The same session exported without Steam puts every payload back in the clear.
- **The transport is chosen by an export preset.** `NetTransport.preferred_kind()` returns
  `STEAM` when `OS.has_feature("steam")` trips, which is the `custom_features="steam"` tag on the preset. No
  line of game code differs between the two builds, so a preset missing that tag ships a build with no link
  encryption that reads identically everywhere in the source.
- **What a game configures on every transport is `Net.set_session_secret()`**, and it buys unforgeability
  rather than confidentiality. See [Where a session secret comes from](#where-a-session-secret-comes-from).
- **Encrypting OrbitNet's own payloads would buy confidentiality on the ENet path only.** On a Steam link it
  restates a property the connection already has. The Encryption tier in [ROADMAP.md](../ROADMAP.md) is
  scoped to the ENet path for that reason, and says why its order does not move.

## Testing without Steam

Everything above is inert on an ENet build, so the whole netcode surface — including `just netbench` and the
demos — runs with no Steam installed. What genuinely cannot be tested that way: persona names, lobby
discovery, invites, ticket validation, and the link encryption and peer-identity authentication in the table
above. Those need a real Steam build, two accounts and a manual pass — the encryption and identity rows also
need a packet capture.
