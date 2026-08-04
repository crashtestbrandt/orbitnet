# Steam sessions — vendoring & verification runbook

Steam is a **parallel transport** to native ENet (see the README "Steam sessions" section for the architecture).
This doc is the operator runbook: how to vendor GodotSteam, what the facade needs from it, and the
**verify-on-a-Steam-build** checklist for the parts CI cannot exercise (there is no Steam client or Steamworks
runtime in headless CI, so the live host/join paths are validated here, by hand, on a real build).

Everything Steamworks lives in one file — [`addons/orbitnet/steam_transport.gd`](../addons/orbitnet/steam_transport.gd)
— reached only from `NetTransport`'s `Kind.STEAM` arm. That file accesses Steam **dynamically**
(`Engine.has_singleton` / `Object.callv` / `ClassDB.instantiate`) on purpose: the project must lint and run on
machines and CI where GodotSteam is **not** vendored (a hard `Steam.` / `SteamMultiplayerPeer` symbol would be an
unresolved-identifier compile error at parse time), and non-Steam builds must carry zero Steamworks dependency.
When GodotSteam is vendored the dynamic calls resolve against the real singletons with **no code change**.

## What the facade needs from GodotSteam

Three things must be registered by the vendored extension(s) for a Steam build to work:

| Symbol | Kind | Used by | Provided by |
| --- | --- | --- | --- |
| `Steam` | Engine singleton | listen host + client (`steamInitEx`, `getAuthSessionTicket`, `run_callbacks`) | **GodotSteam** (client GDExtension) |
| `SteamServer` | Engine singleton | dedicated server (`serverInitEx`, `logOnAnonymous`, `beginAuthSession`) | **GodotSteam-Server** |
| `SteamMultiplayerPeer` | `MultiplayerPeer` class | every peer (`create_host` / `create_client`) | GodotSteam's multiplayer-peer class (verify — see below) |

The client and server extensions are **mutually exclusive in one process** (both would try to own the Steamworks
API), which is why the client and server presets each exclude the other's `addons/godotsteam*` tree. The **client**
extension ships as one preset per desktop platform — `Steam` (Linux, `build/<commit>/steam-linux/`), `Steam macOS`
(`build/<commit>/steam-macos/`), and `Steam Windows` (`build/<commit>/steam-windows/`) — all with `custom_features="steam"` and the
`addons/godotsteam_server/*` exclude; `Steam Server` (Linux, `dedicated_server=true`) vendors the server extension
and excludes `addons/godotsteam/*`. Local build recipes: `just export-steam-macos`, `just export-steam-windows`
(the Windows preset cross-exports from any host with the Godot Windows export templates installed; the export copies
the declared `steam_api64.dll` next to the `.exe`).

The vendored GodotSteam build in `addons/godotsteam/` registers `SteamMultiplayerPeer` under exactly that name.
If you ever re-vendor from a build that doesn't — or from a separate multiplayer-peer bridge addon using a
different class name — adjust the `ClassDB.instantiate(&"SteamMultiplayerPeer")` call sites in
`steam_transport.gd`. When the class is missing, `create_*` degrade to a warning + null (the session fails
cleanly), which is exactly what `just test` (tests/unit/net_transport_test.gd) asserts.

## Vendoring (already done — this is the re-vendoring recipe)

Both extensions are committed under `addons/` via Git LFS, so an ordinary `lfs: true` checkout has them. These
are the steps to follow when refreshing them (a Godot upgrade, a GodotSteam release, a new platform).

GodotSteam is canonically hosted on Codeberg (`codeberg.org/godotsteam/…`). Two source repos map to our two paths:

- `godotsteam` — the client GDExtension (`Steam` singleton).
- `godotsteam-server` — the dedicated-server GDExtension (`SteamServer` singleton).

Steps (run locally / in a network-enabled environment — the Claude Code web sandbox cannot reach these hosts):

1. **Obtain the Godot 4.7-compatible GDExtension builds.** The raw Codeberg `archive/<sha>.zip` tarballs are
   **source** (C++ + SConstruct) — they still need a compile step (scons against the Godot 4.7 `extension_api.json`
   + the Steamworks SDK) to produce the loadable `bin/*.so|.dll|.dylib` and the redistributable `libsteam_api.*`.
   Prefer the **prebuilt release** for Godot 4.7 if one exists; otherwise build from source. **ABI must match
   Godot 4.7** — a mismatched build fails to load the extension.
2. **Place them under `addons/`:**
   - client → `addons/godotsteam/` (its `.gdextension`, `bin/` libs, and `libsteam_api.*`)
   - server → `addons/godotsteam_server/` (same layout)
   These paths are what the export-preset `exclude_filter`s and the `.gitattributes` LFS rules already assume. If
   the archive uses different folder names, either rename to match or update `export_presets.cfg` + `.gitattributes`.
3. **Commit the binaries via Git LFS.** `.gitattributes` already routes `*.so` / `*.dll` / `*.dylib` to LFS. Run
   `git lfs track` for any other binary extension the addon ships (e.g. macOS `*.framework` bundles), then commit.
4. **Do not** add either addon to `[editor_plugins]` in `project.godot` unless GodotSteam ships an EditorPlugin —
   GDExtensions self-register from their `.gdextension` file. (The `Net` autoload and the OrbitNet facade are
   unaffected.)

## App ID

- Moonshot's app id is **3074080**, stored once in the `steam/app_id` project setting (`project.godot`) and read
  by the facade. Swap in Valve's public test app **480** ("Spacewar") there for early bring-up if needed.
- `steam_appid.txt` (repo root, contains `3074080`) lets the Steamworks SDK resolve the id when running **from
  source**. It is excluded from every export preset — **never ship it** (shipped games get their id from Steam's
  launch context).

## Verify-on-a-Steam-build checklist

The facade calls these GodotSteam functions **dynamically and guarded** (first-name-that-exists), so a signature
drift degrades to a warning rather than a crash — but you must confirm each against the version you vendored:

- **Init:** `Steam.steamInitEx(app_id, embed_callbacks)` (client) / `SteamServer.serverInitEx(ip, game_port,
  query_port, server_mode, version)` (server). We init **without** embedded callbacks and pump `run_callbacks()`
  ourselves each frame (`SteamTransport._process`). If your build force-embeds callbacks, drop the manual pump to
  avoid double-running.
- **Peer:** `SteamMultiplayerPeer.create_host(local_virtual_port, options)` and
  `create_client(host_steam_id, remote_virtual_port, options)` returning a Godot `Error`.
- **Server advertise:** `logOnAnonymous()` + `setAdvertiseServerActive(true)` (names vary — the facade tries
  several).
- **Auth:** `Steam.getAuthSessionTicket()` → `{ buffer, size, id }`; `SteamServer.beginAuthSession(ticket,
  ticket_size, steam_id)`; the `validate_auth_ticket_response(auth_id, response, owner_id)` signal (response `0` ==
  owns app).

> **Signature drift is real — introspect, don't assume.** Two of the calls above were written to the wrong
> signature and neither could fail visibly in CI (both live on the dedicated-server path, which needs a Steam
> runtime to reach): `beginAuthSession` takes the ticket **size** as a separate second argument, and
> `serverInitEx` takes `ip` as a **String**, not an int. Godot will not coerce an int to a String through
> `callv`, so each call failed its argument check and returned null — which the facade reads as `FAILED`, so the
> server kicked every joining peer and never initialised in the first place. To check the vendored build without
> a Steam client, introspect it: `godot --headless --script` a throwaway `SceneTree` that prints
> `Engine.get_singleton(&"Steam").get_method_list()` / `ClassDB.class_get_method_list(&"SteamMultiplayerPeer")`.
> That resolves argument names, types and defaults exactly, in seconds, with no Steam runtime.

### Auth ticket exchange — wired, pending live validation

The ownership **ticket exchange** is now wired end-to-end **inside the facade** (`steam_transport.gd`), not in
`NetManager` — the boundary rule (CLAUDE.md) forbids `NetManager` from naming Steam, so instead `SteamTransport`
rides the same transport-agnostic `MultiplayerAPI` connect signals `NetManager` watches (`connected_to_server`,
`peer_disconnected`), from behind the orbitnet boundary. The flow:

1. **Client** (`_on_connected_to_server`): on connect, RPCs its `issue_auth_ticket()` bytes + its own Steam id to
   the server (`_submit_auth_ticket.rpc_id(1, …)`). This works because the service node lives at a stable
   `/root/SteamTransport` path on both ends.
2. **Server** (`_submit_auth_ticket`): records `peer_id → claimed Steam id`, calls `begin_auth_session(...)`, and
   kicks on a synchronous rejection.
3. **Server** (`_on_validate_auth_ticket_response`): on the asynchronous **ownership** verdict, kicks the peer on
   `owns == false`; `_on_peer_disconnected` calls `end_auth_session`.

The whole path **fails open** — no ticket / GodotSteam absent / listen host with no game-server context ⇒ no
gating, exactly like the pre-#45 ENet server. The gate only bites on a dedicated server that gets a definitive
*not-owned* verdict from Steam.

**Still requires a real Steam build to validate** (CI has no Steam runtime): confirm the RPC routes over the relay,
that `getSteamID` / `getAuthSessionTicket` / `beginAuthSession` / `validate_auth_ticket_response` resolve on the
vendored GodotSteam, and that an owning client is accepted while a non-owner is kicked — using two Steam accounts.

- **Spoofed `claimed_steam_id` (trust-boundary check).** The client submits its own Steam id alongside the ticket
  (`_submit_auth_ticket`), and the server stores that *client-asserted* id verbatim in `_peer_steam[sender]`; the
  eventual kick (`_kick_steam_id`, driven by the async `validate_auth_ticket_response` verdict) keys off the real
  `auth_id` Steamworks extracts from the ticket bytes. **Confirm a client that submits a `claimed_steam_id` NOT
  matching its ticket's embedded id is rejected synchronously by `beginAuthSession`** (expected: an
  `InvalidTicket`-class result, kicked immediately in `_submit_auth_ticket`). If GodotSteam does *not* reject the
  mismatch synchronously, the reverse lookup in `_peer_steam` misses on a negative ownership verdict and the
  non-owner is never kicked — silently defeating the gate; in that case, key the kick bookkeeping off `auth_id`
  rather than the client-asserted id. (Raised in PR review.)

### Manual acceptance (the live bars from)

1. **Listen host + client:** export a client preset for each machine (`just export-steam-macos` /
   `just export-steam-windows` — the two accounts can be on different platforms), and drop a `steam_appid.txt`
   containing `3074080` beside each binary (the presets exclude it). With Steam running and signed into two accounts,
   host on one (`-- --host` prints the host's Steam ID); on the other, enter that Steam ID in the join field (or
   `-- --join=<hostSteamID>`) and connect. Confirm the `net-probe`-equivalent behavior (spawn,
   prediction/reconciliation, fire) over the Steam relay.
2. **Dedicated server + client:** export the `Steam Server` preset, run it (boots straight into SERVER mode via
   the `dedicated_server` feature), and join it from a `Steam`-preset client by its Steam ID. Confirm the server
   authoritatively spawns/ticks and that the auth gate accepts an owning client (and rejects a non-owner).
3. **Non-Steam unaffected:** confirm a Windows/macOS/Linux/`Server` export still uses ENet and carries no
   Steamworks libs (the presets exclude `addons/godotsteam*`). `just test` (tests/unit/net_transport_test.gd) guards this from source.

## Enriched sessions — persona names + lobby discovery

Enriched sessions add player display names and a join browser on top of the transport. The game-side layer is
**Steam-blind** and exercised in CI (offline / ENet with the `net.name` handle): the replicated
[`PlayerRoster`](../scripts/net/player_roster.gd) maps peers → names, the kill feed / name tags / name-under-crosshair
consume it, and the [`SessionMenu`](../scripts/ui/session_menu.gd) host-config + browser read
[`NetSessionInfo`](../addons/orbitnet/net_session_info.gd) rows. `just test`
([`net_session_info_test.gd`](../tests/unit/net_session_info_test.gd),
[`kill_feed_test.gd`](../tests/unit/kill_feed_test.gd),
[`player_roster_test.gd`](../tests/unit/player_roster_test.gd),
[`net_identity_test.gd`](../tests/unit/net_identity_test.gd)) guards the pure logic + the ENet degradation.

The **Steamworks** half lives in `steam_transport.gd` (reached only through the `NetTransport` seams
`local_display_name` / `local_steam_id` / `request_sessions` / `sessions` / `bind_sessions_updated`) and, like the
auth handshake, is **guarded + dynamic** — a signature/name drift degrades to "" / an empty list, never a crash.
Confirm each against the vendored GodotSteam on a real Steam build:

- **Persona names:** `Steam.getPersonaName()` (this user's own display name) and `Steam.getFriendPersonaName(id)`
  (a lobby owner's name for the browser). The roster advertises the local name on connect; every peer names every
  other from the replicated table.
- **Host a discoverable lobby:** on `create_listen_host(port, max, friends_only)` the facade best-effort
  `Steam.createLobby(type, max)` — `type` is friends-only (`1`) or public (`2`) — then, on the `lobby_created`
  callback, `setLobbyData(lobby, key, value)` stamps the metadata contract below and `setLobbyMemberLimit(lobby,
  max)` caps it. A lobby-create failure must NOT block the host — the direct-Steam-ID join still works (the lobby
  is only for discovery).
- **Browse sessions:** `request_session_list()` → `addRequestLobbyListStringFilter(game, moonshot, 0)` +
  `addRequestLobbyListDistanceFilter(3 /* worldwide */)` + `addRequestLobbyListResultCountFilter(50)` +
  `requestLobbyList()`; the `lobby_match_list(lobbies)` callback reads each lobby's metadata into `NetSessionInfo`
  rows, then emits `sessions_updated`.

### The lobby metadata contract — and why the browser reads it instead of Steam

Every field the browser renders travels as **lobby metadata**, stamped by the host and read with `getLobbyData`:

| Key | Meaning |
| --- | --- |
| `game` | `"moonshot"` — tags the lobby as ours so the list filter can exclude another app's lobbies |
| `host_id` | the host's 64-bit Steam id: **the connect target** a picked row joins by |
| `owner_name` | the host's persona name |
| `players` / `max` | the live session headcount and the advertised cap |
| `friends_only` | whether the host restricted the session |

Two of those look redundant against Steamworks calls, and are not:

- **`host_id` instead of `getLobbyOwner`.** `ISteamMatchmaking::GetLobbyOwner` is documented **members-only** and
  answers a non-member with a nil id. A browser is by definition *not* a member of the lobbies a list query
  returns, so reading the owner there yielded `0` for every row, every row was discarded as unjoinable, and the
  browser was permanently empty no matter how many sessions were live. Metadata, by contrast, ships with the list
  results and is readable by non-members. `getLobbyOwner` survives only as the post-join fallback, where we *are*
  a member.
- **`players` instead of `getNumLobbyMembers`.** Only the host ever joins the Steam lobby — clients connect
  straight to the host's relay socket — so the member count is stuck at `1` however many pilots are in the
  session. The host republishes `players` from the live `MultiplayerAPI` peer list instead.

Also note the **distance filter**: Steam's default is region-local, so without an explicit worldwide filter the
browser silently hides every host on another continent.

### Play invites

A lobby makes a session *joinable*; **rich presence** is what makes it *invitable*. On `lobby_created` the host
sets `connect` = `+connect_lobby <id>` (plus a `steam_display` token), which is what puts "Join Game" next to its
name in a friend's list and what Steam appends to the invitee's command line on a cold start.

An invite reaches the invitee by one of three routes, and **all three are wired** (before #280's follow-up none
were, so Steam showed a "Join Game" button that did nothing at all):

| Route | Trigger | Handled by |
| --- | --- | --- |
| `join_requested(lobby_id, friend_id)` | overlay "Join Game" / accepted invite, game **running** | `_on_steam_join_requested` |
| `join_game_requested(user, connect)` | connect-string invite (rich presence / `inviteUserToGame`) | `_on_steam_join_game_requested` |
| `+connect_lobby <id>` on the command line | accepted while the game was **closed** | `check_launch_invite()` |

All three converge on `_accept_lobby_invite(lobby_id)`. An invite names a **lobby**, but `create_client` needs a
**host id**, so accepting is a two-step hop: `joinLobby`, then resolve `host_id` on the `lobby_joined` callback
(by then we're a member, so the metadata is certainly readable) and emit the Steam-blind `invite_accepted(target)`.
`NetManager` re-emits it and `Main` joins exactly as if the player had picked a browser row.

A freshly joined invite lobby is **staged** in `_invite_lobby_awaiting_session` rather than assigned straight to
`_joined_lobby_id`, and only promoted on `connected_to_server`. That matters when an invite is accepted while
already in another session — the common Steam case — because `Main` tears the old session down *between* the lobby
join and the new connect: an unstaged lobby would be left again immediately by that teardown's `release_session()`,
seconds after joining it, and would never be cleaned up when the new session ended. `connection_failed` (and a
superseding invite) releases the staged lobby, since `release_session()` deliberately cannot see it.

Sending is `open_invite_overlay()` → `activateGameOverlayInviteDialog(hosted_lobby)`, surfaced as the pause menu's
**Invite Friends** button (Steam builds, only while hosting — `NetManager.can_invite()` gates it).

`release_session()` leaves both lobbies (hosted and joined) and clears rich presence on session teardown, so a
stopped session stops advertising instead of lingering in every browser as an unjoinable ghost row.

> **Friends-only is invite-only.** Valve: a `k_ELobbyTypeFriendsOnly` lobby "does not show up in the lobby list."
> So ticking *Friends only* deliberately hides the session from the browser and leaves invites as the only way in
> — which is coherent now, but meant the session was unreachable entirely while the invite routes were unwired.

- **Manual acceptance (two Steam accounts):** host on one with a max-players cap; on the other, **Refresh** the
  browser and confirm the row shows the host's persona name + `(players/max)`, that the count moves as players
  join and leave, and that picking it joins over the relay. In-session, confirm the joiner's avatar carries a name
  tag, a kill shows "\<killer\> killed \<victim\>" in the feed, and looking at the other player reads their name
  under the crosshair. Then re-host with **friends only** ticked and confirm: the lobby is hidden from the browser
  (including from a non-friend account), **Invite Friends** opens the Steam overlay, and the invitee joins — once
  with the game already running (`join_requested`), and once with the game closed so Steam launches it
  (`+connect_lobby`; the parse itself is unit-tested in
  [`steam_invite_test.gd`](../tests/unit/steam_invite_test.gd)). Then accept an invite **while already connected to
  a different session** — the lobby-staging case above — and confirm the old session tears down, the new one
  connects, and ending it leaves the invite lobby (the host's browser row should drop the invitee's headcount).
  Finally, end the session and confirm the row disappears from the other account's browser.

## Deploy

Publishing Steam builds is automated by [`steam-deploy.yml`](../.github/workflows/steam-deploy.yml) (#50, M4) —
a tag push exports the four Steam presets on native runners, boot-smokes the Linux client, and pushes each build
to a Steam depot via `steamcmd`/SteamPipe. The pipeline is reproducible locally with `just steam-deploy` (both
call the same `tools/steam-deploy/deploy.sh` + version-controlled `.vdf` templates). Depot-per-target: the three
clients land on the **`beta`** branch, the dedicated server on its own **`server`** branch, and CI never touches
**`default`** (manual promotion only). Full runbook, the depot/branch table, the builder secrets
(`STEAM_USERNAME` + the base64 `STEAM_CONFIG_VDF` cached-session), and the **Steamworks dashboard setup** an
operator does once are in the README **"Steam deploy"** section. Adding the builder secrets + the deploy workflow
were the CLAUDE.md "ask before" items cleared by.
