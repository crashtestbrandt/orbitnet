extends Node
class_name SteamTransport
## The Steam arm of the transport factory. This is the ONE first-party file that names Steamworks --
## the same one-facade-boundary discipline addons/orbitnet/net.gd keeps around the rollback backend, and
## net_transport.gd keeps around the concrete MultiplayerPeer: NetTransport.create_server/create_client delegate
## their Kind.STEAM arm here, and NOTHING else in the tree touches Steam. The session layer, the spawn directors, Net's
## rollback facade and gameplay code stay Steam-blind -- they only ever see the resulting MultiplayerPeer.
##
## OPTIONAL-DEPENDENCY DISCIPLINE: GodotSteam (the `Steam` singleton + the `SteamMultiplayerPeer` class) is a
## GDExtension vendored under addons/ (see docs/steam.md) and present ONLY in a build exported with the `Steam`
## preset (custom_features="steam"). So every Steamworks access here is DYNAMIC -- Engine.has_singleton("Steam"),
## Object.callv(...), ClassDB.class_exists/instantiate("SteamMultiplayerPeer") -- never a hard `Steam.` /
## `SteamMultiplayerPeer` symbol. That is deliberate, not a shortcut:
##   * the project lints + runs on machines/CI where GodotSteam is NOT vendored (the extension isn't present at
##     parse time, so a static reference would be an unresolved-identifier compile error), and
##   * non-Steam desktop/web/server builds carry zero Steamworks dependency (acceptance criterion: "completely
##     unaffected"). preferred_kind() only returns Kind.STEAM when OS.has_feature("steam") trips, so on those
##     builds this file is never reached and the singleton is never looked up.
## When GodotSteam IS vendored the dynamic calls resolve against the real singleton with no code change.
##
## VERIFY-ON-A-STEAM-BUILD: the Steamworks call SIGNATURES below (steamInitEx / gameServerInitEx / create_host /
## create_client / auth tickets) are written against the current GodotSteam GDExtension API and CANNOT be
## exercised in a headless CI sandbox (no Steam client, no Steamworks runtime). Each is guarded (has_method /
## first-that-exists) so a version-name drift degrades to a warning + null -- host/join fails cleanly exactly like
## the ENet failure path, never a crash -- rather than silently misbehaving. Validate them against the exact
## GodotSteam version you vendor; docs/steam.md is the runbook.

## A joining peer's Steam auth ticket was validated (or rejected) by beginAuthSession's response callback -- the
## dedicated-server trust boundary: the server ends the session / kicks on `owns == false`. Re-emitted from
## Steam's validate_auth_ticket_response so no game code names the Steam signal.
signal auth_validated(steam_id: int, owns_game: bool)

## The discovered joinable-session list changed -- re-emitted from Steam's lobby_match_list so the join
## browser refreshes without naming Steam. Read the current list via [method sessions].
signal sessions_updated()

## The player accepted a Steam PLAY INVITE and we resolved it to a joinable session. `connect_target` is exactly
## what [method create_client] takes (the host's Steam id as a decimal string), so the listener just forwards it to
## the ordinary join path -- the invite never leaks a lobby id or the word "Steam" past this file. Covers all three
## accept routes: the overlay's "Join Game" while we're running, a connect-string invite, and a cold start where
## Steam launched us with `+connect_lobby <id>`.
signal invite_accepted(connect_target: String)

## SET `steam/app_id` IN YOUR project.godot. The default is 480 -- Steam's public "Spacewar" test app, which
## every Steam SDK ships with and which lets the transport be exercised before you have an app id of your own.
## It is NOT a usable shipping value: two different games both left on 480 would see each other's lobbies.
const DEFAULT_APP_ID: int = 480
const _APP_ID_SETTING: String = "steam/app_id"

# GodotSteam / Steamworks enum values inlined as ints (the enums live in the Steam singleton, which may be absent
# at parse time -- see the header). k_EServerMode: 3 == AuthenticationAndSecure (VAC + ownership auth). Auth
# session response: 0 == k_EAuthSessionResponseOK (the ticket holder owns the app id).
const _SERVER_MODE_AUTH_SECURE: int = 3
const _AUTH_RESPONSE_OK: int = 0

# Steam lobby types inlined as ints (the enum lives on the Steam singleton, possibly absent at parse time):
# 1 == k_ELobbyTypeFriendsOnly (invite/friends-visible), 2 == k_ELobbyTypePublic (matchmaking-discoverable). A
# listen host advertises one of these so the join browser can discover it; friends-only just picks the narrower type.
const _LOBBY_TYPE_FRIENDS_ONLY: int = 1
const _LOBBY_TYPE_PUBLIC: int = 2
# Lobby metadata keys: the browser reads these off each discovered lobby. All of them are set by the HOST;
# `game` tags the lobby as ours so a stray lobby from another app on the same account is filtered out.
#
# `host_id` carries the host's own 64-bit Steam id, and it is load-bearing: the browser is NOT a member of the
# lobbies a list query returns, and ISteamMatchmaking::GetLobbyOwner is documented as members-only -- it returns a
# nil id to a non-member, which used to null out every discovered row and leave the browser permanently empty.
# Lobby METADATA, by contrast, is fetched with the list results and readable by non-members, so the connect target
# has to travel that way. getLobbyOwner survives only as the post-join fallback below, where we ARE a member.
#
# `players` carries the live session headcount, republished by the host as peers come and go. It is NOT
# getNumLobbyMembers: only the host ever joins the Steam lobby (clients connect straight to the host's relay
# socket), so the member count is stuck at 1 no matter how many pilots are actually in the session.
const _LOBBY_KEY_GAME: String = "game"
# Tags a lobby as belonging to YOUR game, so the browser's list filter excludes another app's lobbies. Set
# `steam/lobby_game_tag` in project.godot; the default is only sensible while you are the only OrbitNet game
# on a given app id.
const _LOBBY_TAG_SETTING: String = "steam/lobby_game_tag"
const _DEFAULT_LOBBY_TAG: String = "orbitnet"
const _LOBBY_KEY_HOST_ID: String = "host_id"
const _LOBBY_KEY_OWNER_NAME: String = "owner_name"
const _LOBBY_KEY_PLAYERS: String = "players"
const _LOBBY_KEY_MAX: String = "max"
const _LOBBY_KEY_FRIENDS: String = "friends_only"

# Lobby-query knobs, inlined as ints like the enums above. Steam's DEFAULT distance filter restricts results
# to the caller's own region, which silently hides every session hosted on another continent -- a browser is
# supposed to span the world, so we ask for WORLDWIDE (3) explicitly. Comparison 0 == k_ELobbyComparisonEqual for
# the `game` tag filter; the result cap is Steam's own default, stated rather than implied.
const _LOBBY_DISTANCE_WORLDWIDE: int = 3
const _LOBBY_COMPARISON_EQUAL: int = 0
const _LOBBY_RESULT_LIMIT: int = 50

# Steam result codes used by the lobby callbacks: k_EResultOK == 1 (lobby_created), and
# k_EChatRoomEnterResponseSuccess == 1 (lobby_joined).
const _RESULT_OK: int = 1
const _CHAT_ROOM_ENTER_SUCCESS: int = 1

# --- play invites -------------------------------------------------------------------------------------------
# Steam's convention for "join this session": the token it appends to the game's command line when a player
# accepts an invite from a COLD START, and the value of the `connect` rich-presence key that makes "Join Game"
# appear on our entry in a friend's list at all. One constant, both jobs, because they must agree.
const _CONNECT_LOBBY_FLAG: String = "+connect_lobby"
const _RICH_PRESENCE_CONNECT: String = "connect"
# The rich-presence line the Steam friends list shows next to our name. `steam_display` is a localisation token
# resolved from your app's Steamworks rich-presence config -- register a `Status_InSession` token there, or
# change this to a token you do register, or the friends list shows nothing next to your name.
const _RICH_PRESENCE_DISPLAY: String = "steam_display"
const _RICH_PRESENCE_DISPLAY_VALUE: String = "#Status_InSession"

# The single service instance, lazily created + parented to the scene-tree root on first Steam host/join and kept
# for the process lifetime (Steam init is once-per-process; the callback pump must keep running). Static so
# NetTransport's static factory methods can reach it without an autoload (autoloads are a CLAUDE.md "ask before"
# item; a runtime-attached node is not, and keeps Steam entirely inside the orbitnet boundary).
static var _service: SteamTransport = null

var _client_ready: bool = false     # steamInitEx succeeded (a logged-in Steam user; host/join/listen path)
var _server_ready: bool = false     # gameServerInitEx succeeded (the headless dedicated-server path)
# The singleton this process's Steam context resolved to: the GodotSteam client extension registers `Steam`, the
# GodotSteam-Server extension registers `SteamServer`. A process is EITHER a client/listen host OR a dedicated
# server, so one reference (set at init) serves every later call -- run_callbacks, auth. Null until init succeeds.
var _steam_obj: Object = null

# SERVER-side auth bookkeeping: peer_id -> the Steam id that peer claimed when it submitted its ownership ticket
# (_submit_auth_ticket). Lets the asynchronous ownership verdict (which arrives keyed by Steam id) find the peer to
# KICK, and peer_disconnected find the Steam id to end its auth session. Empty on a client / a listen host that
# never gates -- so the handshake below is inert everywhere except a dedicated server that actually validates.
var _peer_steam: Dictionary[int, int] = {}

# --- session lobbies (discovery + advertisement) -------------------------------------------------------
# The lobby THIS host advertises (0 == not hosting / not yet created). Created best-effort alongside the listen
# host so the join browser can discover the session; the actual connection still rides the host's Steam id.
var _hosted_lobby_id: int = 0
var _advertise_max: int = 0             # the "max players" to publish as lobby metadata once the lobby is created
var _advertise_friends_only: bool = false
# The most recent discovery result (Steam-blind [NetSessionInfo] rows), refreshed from lobby_match_list. Read by
# sessions(); the join browser renders it. Empty until the first requestLobbyList result arrives.
var _sessions: Array[NetSessionInfo] = []
var _lobby_signals_wired: bool = false  # idempotency guard for the create/list callback connections

# --- play invites (accept + send) ---------------------------------------------------------------------------
# The lobby we are mid-way through joining on behalf of an accepted invite (0 == none). An invite hands us a LOBBY
# id, but create_client takes a HOST id, and the only way to read a lobby's metadata reliably is to be in it -- so
# accepting is a two-step hop: joinLobby, then resolve the host on lobby_joined and emit invite_accepted. This
# field is what tells our own lobby_joined callback that the lobby is one we chased (the host gets the same signal
# for the lobby it just created, and must not "accept an invite" to itself).
var _pending_invite_lobby: int = 0
# A lobby we joined as an INVITEE and should leave when the session ends (0 == none). Distinct from
# _hosted_lobby_id: one is the lobby we advertise, the other is a lobby we are a guest in.
var _joined_lobby_id: int = 0
# An invite lobby we have JOINED but whose session has not connected yet -- held here, deliberately out of reach of
# release_session(), until connected_to_server promotes it to _joined_lobby_id.
#
# This staging slot exists because accepting an invite while ALREADY in a session (normal on Steam) tears the old
# session down BETWEEN the lobby join and the new connect: Main disconnects, then joins. If the freshly joined
# lobby were already _joined_lobby_id, that teardown's release_session() would leave the lobby belonging to the
# session about to START -- so we would drop out of the invite lobby moments after entering it, and the "leave the
# invitee lobby when the session ends" invariant would never fire for the new session at all. Ownership only
# transfers once the session it belongs to actually exists.
var _invite_lobby_awaiting_session: int = 0
# Whether the cold-start command line has already been scanned for `+connect_lobby`. One shot per process: the
# launch args do not change, and re-consuming them would re-join on every menu visit.
var _launch_invite_consumed: bool = false

## The lazily-created service node. Creates + parents it to the scene-tree root on first use; returns the cached
## instance thereafter. Never returns null (the node always constructs); whether Steam is actually usable is a
## separate question answered by the create_* / ensure_* methods below.
static func service() -> SteamTransport:
	if _service != null and is_instance_valid(_service):
		return _service
	var svc: SteamTransport = SteamTransport.new()
	svc.name = "SteamTransport"
	var tree: SceneTree = Engine.get_main_loop() as SceneTree
	if tree != null and tree.root != null:
		# DEFERRED: service() is first reached from the session layer inside the game's bring-up, when the scene
		# root is still "busy setting up children" and a direct add_child() fails (verified on a Steam build). The
		# returned node is used immediately for peer creation (which needs no tree membership); the _process callback
		# pump + the _ready auth-signal wiring go live one idle frame later, before any connection completes.
		tree.root.add_child.call_deferred(svc)
	_service = svc
	return svc

# Ride the transport-agnostic MultiplayerAPI connect/disconnect signals so the auth handshake below hangs off the
# SAME peer-connect boundary the session layer watches -- but from INSIDE the orbitnet facade, so the session layer stays
# Steam-blind (CLAUDE.md: nothing but this file / net_transport.gd names Steam). This node lives at a stable
# /root/SteamTransport path on both ends (service() creates it identically on host + client), which is what lets
# _submit_auth_ticket route as an RPC. Guarded + idempotent: with no MultiplayerAPI (the degradation covered by
# tests/unit/net_transport_test.gd), these signals simply never fire, so the wiring is a no-op.
func _ready() -> void:
	var mp: MultiplayerAPI = multiplayer
	if mp == null:
		return
	if not mp.connected_to_server.is_connected(_on_connected_to_server):
		mp.connected_to_server.connect(_on_connected_to_server)
	if not mp.peer_disconnected.is_connected(_on_peer_disconnected):
		mp.peer_disconnected.connect(_on_peer_disconnected)
	# headcount: the advertised `players` metadata is republished off the same peer churn, so a browser row
	# tracks the live session instead of showing "1/8" forever. Inert until we actually host a lobby.
	if not mp.peer_connected.is_connected(_on_peer_connected):
		mp.peer_connected.connect(_on_peer_connected)
	# invites: a session we joined off an invite may never come up, and the lobby staged for it has to be
	# released when that happens (release_session cannot see the staging slot -- that is the point of it).
	if not mp.connection_failed.is_connected(_on_connection_failed):
		mp.connection_failed.connect(_on_connection_failed)

func _process(_delta: float) -> void:
	# Pump Steamworks callbacks while any Steam context is live. steamInitEx is called WITHOUT embedded callbacks
	# (see _ensure_client), so the SteamMultiplayerPeer / auth / networking-sockets callbacks only fire when we
	# run them here. Guarded so a build without GodotSteam (no singleton) simply no-ops every frame.
	if _steam_obj == null:
		return
	if _steam_obj.has_method(&"run_callbacks"):
		_steam_obj.callv(&"run_callbacks", [])

# --- transport peers (delegated to from net_transport.gd's Kind.STEAM arm) -----------------------

## Build a Steam listen-server host peer for a player-hosted (listen-server) session -- the Steam analogue of the
## ENET arm's ENetMultiplayerPeer.create_server. Requires a logged-in Steam user (client init). Returns null on
## any failure (GodotSteam absent, init failed, create_host failed) so the session layer's host path stays OFFLINE /
## surfaces the error exactly as it does for an ENet failure. `max_clients` is advisory on Steam (the relay does
## not take a hard cap the way ENet does); kept in the signature for parity with the ENet arm.
func create_listen_host(_port: int, max_clients: int, friends_only: bool = false) -> MultiplayerPeer:
	if not _ensure_client():
		return null
	var peer: MultiplayerPeer = _create_host_peer(max_clients)
	if peer != null:
		# Surface this host's Steam ID so a second account can join it (the SessionMenu join field / --join= carries a
		# Steam ID on a Steam build, not an IP). The relay needs no port-forwarding -- just this 64-bit id.
		print("SteamTransport: Steam listen host ready -- joiners use this host's Steam ID: %d" % _local_steam_id())
		# advertise a discoverable lobby carrying the cap + friends-only flag so the join browser can find this
		# session (best-effort; a lobby-create failure never blocks the host -- the direct-Steam-ID join still works).
		_advertise_lobby(max_clients, friends_only)
	return peer

## Build a Steam DEDICATED-server host peer -- the headless, no-local-player path (the session layer's dedicated path).
## Unlike the listen host this registers a Steam GAME SERVER (anonymous logon, no user account) so a client can
## discover + connect to it over Steam's relay by lobby / server id, mirroring the ENet dedicated server's raw
## IP:port reachability but over Steam (dedicated-server deliverable). The factory is the ONLY thing that
## knows the server registered itself with Steam -- host_dedicated is unchanged.
func create_dedicated_host(_port: int, max_clients: int, friends_only: bool = false) -> MultiplayerPeer:
	if not _ensure_game_server():
		return null
	var peer: MultiplayerPeer = _create_host_peer(max_clients)
	# a dedicated server has no logged-in Steam user, so it cannot create a client-owned matchmaking lobby the
	# way a listen host does -- discovery for a dedicated server rides the game-server registration
	# (setAdvertiseServerActive, wired in _ensure_game_server). The friends-only flag is a client-lobby concept and
	# is inert here; kept in the signature for parity with the listen path.
	_advertise_friends_only = friends_only
	return peer

## Build a Steam client peer joining `target` -- the Steam analogue of the ENET arm's create_client. `target` is
## the host's Steam ID (a 64-bit id as a decimal string; the SessionMenu "address" field carries it on a Steam
## build) rather than an IP. Requires a logged-in Steam user. Returns null on any failure.
func create_client(target: String, _port: int) -> MultiplayerPeer:
	if not _ensure_client():
		return null
	var host_id: int = target.to_int()
	if host_id <= 0:
		push_warning("SteamTransport: create_client got a non-Steam-ID target %s (expected a 64-bit Steam ID)" % target)
		return null
	if not ClassDB.class_exists(&"SteamMultiplayerPeer"):
		push_warning("SteamTransport: SteamMultiplayerPeer class not registered (GodotSteam MultiplayerPeer not vendored)")
		return null
	var obj: Object = ClassDB.instantiate(&"SteamMultiplayerPeer")
	if obj == null:
		return null
	if not obj.has_method(&"create_client"):
		push_warning("SteamTransport: SteamMultiplayerPeer has no create_client (verify the vendored GodotSteam API)")
		return null
	# GodotSteam MultiplayerPeer: create_client(steam_id, virtual_port := 0) -> Error (verified via ClassDB against
	# the vendored extension -- no options array in this version; virtual_port 0 matches the host's create_host).
	var rc: Variant = obj.callv(&"create_client", [host_id, 0])
	var err: int = rc if rc is int else FAILED
	if err != OK:
		push_warning("SteamTransport: create_client(%d) failed: %s" % [host_id, error_string(err)])
		return null
	var peer: MultiplayerPeer = obj
	return peer

# Shared host-socket creation for both the listen and dedicated paths: instantiate SteamMultiplayerPeer and open
# its host socket on Steam's networking sockets / relay. Returns null (with a warning) if GodotSteam's
# MultiplayerPeer is not vendored or the socket fails to open.
func _create_host_peer(_max_clients: int) -> MultiplayerPeer:
	if not ClassDB.class_exists(&"SteamMultiplayerPeer"):
		push_warning("SteamTransport: SteamMultiplayerPeer class not registered (GodotSteam MultiplayerPeer not vendored)")
		return null
	var obj: Object = ClassDB.instantiate(&"SteamMultiplayerPeer")
	if obj == null:
		return null
	if not obj.has_method(&"create_host"):
		push_warning("SteamTransport: SteamMultiplayerPeer has no create_host (verify the vendored GodotSteam API)")
		return null
	# GodotSteam MultiplayerPeer: create_host(virtual_port := 0) -> Error (verified via ClassDB against the vendored
	# extension -- no options array in this version). The relay handles NAT traversal; joiners connect by this host's
	# Steam ID (listen) or via the game-server registration (dedicated).
	var rc: Variant = obj.callv(&"create_host", [0])
	var err: int = rc if rc is int else FAILED
	if err != OK:
		push_warning("SteamTransport: create_host failed: %s" % error_string(err))
		return null
	var peer: MultiplayerPeer = obj
	return peer

# --- Steam lifecycle -----------------------------------------------------------------------------

# Ensure a client-side Steam context (a logged-in Steam user). Idempotent. steamInitEx(app_id, embed_callbacks):
# embed_callbacks=false so we own the pump (_process above), which keeps the callback cadence tied to the game
# loop the SteamMultiplayerPeer runs on. Returns false (with a warning) when GodotSteam is absent or init fails,
# so callers degrade to the ENet-style null-peer failure.
func _ensure_client() -> bool:
	if _client_ready:
		return true
	var steam: Object = _resolve_singleton([&"Steam"])
	if steam == null:
		push_warning("SteamTransport: Steam singleton not available (GodotSteam GDExtension not vendored / not a steam build)")
		return false
	var app_id: int = _app_id()
	var result: Variant = _call_first(steam, [&"steamInitEx", &"steamInit"], [app_id, false])
	if not _init_ok(result):
		push_warning("SteamTransport: steamInitEx(%d) failed: %s" % [app_id, str(result)])
		return false
	_steam_obj = steam
	_client_ready = true
	# Wire the lobby + INVITE callbacks the moment a Steam client context exists, not just when we host or browse:
	# an invite can land on a peer sitting at the start menu having touched nothing, and the overlay's "Join Game"
	# is silently dead unless join_requested is already connected when the player clicks it.
	_wire_lobby_signals()
	return true

# Ensure a DEDICATED-server Steam context: a Steam GAME SERVER with anonymous logon (no user account), then
# advertise it so clients can find it. Idempotent. This is the headless-server path -- it does NOT require a
# logged-in Steam user the way _ensure_client does. Connects Steam's auth-validation callback so joining clients'
# ownership tickets can be checked (the trust boundary the ENet server never had).
func _ensure_game_server() -> bool:
	if _server_ready:
		return true
	# GodotSteam-Server registers `SteamServer`; a combined/older build may expose the game-server API on `Steam`.
	# Prefer the server singleton, fall back to the client one.
	var steam: Object = _resolve_singleton([&"SteamServer", &"Steam"])
	if steam == null:
		push_warning("SteamTransport: no Steam server singleton (GodotSteam-Server GDExtension not vendored / not a steam build)")
		return false
	var app_id: int = _app_id()
	# serverInitEx / gameServerInitEx(ip, game_port, query_port, server_mode, version_string) -> {status, verbal}.
	# `ip` is a STRING on the vendored GodotSteam-Server ("0.0.0.0" == bind all interfaces); it used to be passed as
	# the int 0, and Godot will not coerce int -> String through callv, so the call failed its argument check and
	# the Steam dedicated server never initialised at all. Ports 0 let Steam pick. AuthenticationAndSecure enables
	# VAC + the ownership auth used below. (GodotSteam-Server names these serverInit*; a combined build uses
	# gameServerInit*.)
	var result: Variant = _call_first(steam, [&"serverInitEx", &"gameServerInitEx", &"serverInit", &"gameServerInit"],
		["0.0.0.0", 0, 0, _SERVER_MODE_AUTH_SECURE, "1.0.0.0"])
	if not _init_ok(result):
		push_warning("SteamTransport: server init (app=%d) failed: %s (verify GodotSteam-Server API)" % [app_id, str(result)])
		return false
	# Log the server on anonymously + advertise it so a client can discover/connect over the relay. Best-effort +
	# guarded across GodotSteam server-function names (VERIFY against the vendored version).
	_call_first(steam, [&"logOnAnonymous", &"serverLogOnAnonymous", &"gameServerLogOnAnonymous"], [])
	_call_first(steam, [&"setAdvertiseServerActive", &"serverSetAdvertiseServerActive", &"gameServerSetAdvertiseServerActive"], [true])
	_steam_obj = steam
	_connect_auth_response(steam)
	_server_ready = true
	return true

# --- auth session tickets (dedicated-server ownership trust boundary) -----------------------

## Issue a Steam auth session ticket for THIS client to hand to a server it is joining, proving app ownership.
## Returns the ticket bytes (empty on failure). The over-the-wire hand-off (client -> server at join) and the
## server-side begin_auth_session/kick-on-reject wiring is the owner's verify-on-a-Steam-build step -- see
## docs/steam.md; this exposes the seam so that exchange never names Steam outside this file.
func issue_auth_ticket() -> PackedByteArray:
	if _steam_obj == null or not _client_ready:
		return PackedByteArray()
	var ticket: Variant = _call_first(_steam_obj, [&"getAuthSessionTicket"], [])
	if ticket is Dictionary and ticket.has("buffer"):
		var buf: Variant = ticket["buffer"]
		if buf is PackedByteArray:
			return buf
	return PackedByteArray()

## Server-side: begin validating a joining client's ownership ticket. The result arrives asynchronously on the
## `auth_validated` signal (from Steam's validate_auth_ticket_response). Returns the synchronous begin result
## (0 == OK to start; non-zero == rejected outright).
func begin_auth_session(ticket: PackedByteArray, steam_id: int) -> int:
	if _steam_obj == null or not _server_ready:
		return FAILED
	# beginAuthSession(ticket, ticket_size, steam_id) -- the SIZE is a separate argument on the vendored GodotSteam.
	# It used to be omitted, so every call failed its argument check and returned null -> FAILED, which made
	# _submit_auth_ticket kick every single joining peer off a dedicated server.
	var rc: Variant = _call_first(_steam_obj, [&"beginAuthSession"], [ticket, ticket.size(), steam_id])
	var rc_int: int = rc if rc is int else FAILED
	return rc_int

## Server-side: release a validated/departed client's auth session (call on peer_disconnected).
func end_auth_session(steam_id: int) -> void:
	if _steam_obj == null or not _server_ready:
		return
	_call_first(_steam_obj, [&"endAuthSession"], [steam_id])

# Bridge Steam's validate_auth_ticket_response into the facade `auth_validated` signal. Connected once, on the
# server, in _ensure_game_server. Guarded: only if the signal actually exists on the vendored GodotSteam.
func _connect_auth_response(steam: Object) -> void:
	if not steam.has_signal(&"validate_auth_ticket_response"):
		return
	if steam.is_connected(&"validate_auth_ticket_response", _on_validate_auth_ticket_response):
		return
	steam.connect(&"validate_auth_ticket_response", _on_validate_auth_ticket_response)

# GodotSteam emits validate_auth_ticket_response(auth_id: int, response: int, owner_id: int). response == 0
# (k_EAuthSessionResponseOK) means the ticket holder owns the app; anything else is a reject (not owned / ticket
# cancelled / VAC banned). Re-emit as the ownership verdict for the server's join gate, AND enforce it: on a
# negative verdict, kick the peer that claimed this Steam id (the dedicated-server trust boundary). auth_id is
# the joining user's Steam id (GodotSteam's first arg), which is exactly the key _peer_steam is stored under.
func _on_validate_auth_ticket_response(auth_id: int, response: int, _owner_id: int) -> void:
	var owns: bool = response == _AUTH_RESPONSE_OK
	auth_validated.emit(auth_id, owns)
	if not owns:
		_kick_steam_id(auth_id, "auth failed (response %d)" % response)

# --- auth handshake (end-to-end ticket exchange) --------------------------------------------
# The seam above (issue/begin/end + the auth_validated signal) is wired together here so the exchange never names
# Steam outside this file. It rides the MultiplayerAPI connect signals (_ready): the client hands its ownership
# ticket to the server on connect, the server begins validation and kicks on a negative ownership verdict. All of
# it VERIFY-ON-A-STEAM-BUILD -- it can only run against a real Steam relay + two accounts (docs/steam.md), so every
# Steam-specific call stays guarded and the whole path fails OPEN (no false kicks) when a piece is unavailable.

# CLIENT: the connection to the server completed. Hand our ownership ticket + our own Steam id to the server
# (reliable RPC to peer 1) so it can prove we own the app. No-ops with nothing to prove -- ENet, GodotSteam absent,
# or a listen host with no ticket -- in which case the server simply never gates us (fail open).
func _on_connected_to_server() -> void:
	# First, promote an invite lobby that was waiting for exactly this: the session it belongs to is now live, so it
	# becomes the lobby release_session() is allowed to leave. See _invite_lobby_awaiting_session.
	if _invite_lobby_awaiting_session != 0:
		_joined_lobby_id = _invite_lobby_awaiting_session
		_invite_lobby_awaiting_session = 0
	if not _client_ready or multiplayer.is_server():
		return
	var ticket: PackedByteArray = issue_auth_ticket()
	if ticket.is_empty():
		return
	var my_steam_id: int = _local_steam_id()
	if my_steam_id <= 0:
		return
	_submit_auth_ticket.rpc_id(1, ticket, my_steam_id)

# SERVER: a joining client handed us its ticket. Record which peer claims which Steam id (for the async verdict +
# cleanup), then begin validating. begin_auth_session returns non-OK synchronously only for a malformed/outright-
# rejected ticket -- kick then; the OWNERSHIP verdict itself arrives asynchronously on
# _on_validate_auth_ticket_response. On a listen host (no dedicated game-server context) begin_auth_session is a
# no-op FAILED and we FAIL OPEN: the auth gate is the dedicated-server trust boundary, not the listen host.
@rpc("any_peer", "call_remote", "reliable")
func _submit_auth_ticket(ticket: PackedByteArray, claimed_steam_id: int) -> void:
	var sender: int = multiplayer.get_remote_sender_id()
	_peer_steam[sender] = claimed_steam_id
	var rc: int = begin_auth_session(ticket, claimed_steam_id)
	if _server_ready and rc != OK:
		_kick_peer(sender, "auth ticket rejected on begin (%s)" % error_string(rc))

# SERVER: a peer left. Release its Steam auth session (mirrors begin_auth_session) and drop its mapping. Bound to
# multiplayer.peer_disconnected in _ready; inert for a peer that never submitted a ticket (client / listen host).
func _on_peer_disconnected(peer_id: int) -> void:
	_publish_player_count()   # host side: the advertised headcount drops with the peer (inert when not hosting)
	if not _peer_steam.has(peer_id):
		return
	end_auth_session(_peer_steam[peer_id])
	_peer_steam.erase(peer_id)

# Kick whichever peer claimed `steam_id` (server-side enforcement of a failed ownership check).
func _kick_steam_id(steam_id: int, reason: String) -> void:
	for peer_id: int in _peer_steam.keys():
		if _peer_steam[peer_id] == steam_id:
			_kick_peer(peer_id, reason)
			return

# Force-disconnect a peer through whatever MultiplayerPeer is live. Guarded: disconnect_peer exists on the ENet +
# Steam peers but not the OfflineMultiplayerPeer, so probe it dynamically rather than hard-calling.
func _kick_peer(peer_id: int, reason: String) -> void:
	push_warning("SteamTransport: kicking peer %d (%s)" % [peer_id, reason])
	var mp_peer: MultiplayerPeer = multiplayer.multiplayer_peer
	if mp_peer != null and mp_peer.has_method(&"disconnect_peer"):
		mp_peer.callv(&"disconnect_peer", [peer_id])

# THIS client's own 64-bit Steam id, to hand to the server alongside the ticket. begin_auth_session pairs the
# ticket WITH this id, so a spoofed id simply fails validation (Steam rejects a ticket/id mismatch). 0 when the
# Steam user id is unavailable (GodotSteam absent / not initialised), which suppresses the submit.
func _local_steam_id() -> int:
	if _steam_obj == null:
		return 0
	var raw: Variant = _call_first(_steam_obj, [&"getSteamID"], [])
	return raw if raw is int else 0

# --- player identity (persona names) --------------------------------------------------------
## This client's own Steam persona (display) name, or "" when unavailable (GodotSteam absent / not a Steam build /
## the user isn't logged in). Steam-blind callers reach it via NetTransport.local_display_name(); the roster then
## advertises it. Guarded so a non-Steam build simply gets "".
func local_persona_name() -> String:
	if not _ensure_client():
		return ""
	var raw: Variant = _call_first(_steam_obj, [&"getPersonaName"], [])
	return raw if raw is String else ""

## This client's own 64-bit Steam id (public accessor over the private _local_steam_id used by the auth handshake),
## or 0 when unavailable. The roster carries it alongside the name.
func local_steam_id() -> int:
	if not _ensure_client():
		return 0
	return _local_steam_id()

## The persona (display) name Steam knows for `steam_id` -- used to name a discovered lobby's OWNER in the browser.
## getFriendPersonaName resolves friends + recently-met users + anyone in a joined lobby (the browser case). "" when
## unavailable / not yet cached by Steam.
func persona_name_for(steam_id: int) -> String:
	if _steam_obj == null or steam_id <= 0:
		return ""
	var raw: Variant = _call_first(_steam_obj, [&"getFriendPersonaName"], [steam_id])
	return raw if raw is String else ""

# --- session lobbies (advertise a host, discover joinable sessions) --------------------------
## The most recently discovered joinable sessions (Steam-blind rows). Read by NetTransport.sessions() -> the join
## browser. Empty until a request_session_list() result arrives (or when GodotSteam is absent).
func sessions() -> Array[NetSessionInfo]:
	return _sessions

## Kick off (re)discovery of joinable lobbies. The result arrives asynchronously on Steam's lobby_match_list, which
## _on_lobby_match_list turns into [NetSessionInfo] rows + emits `sessions_updated`. Guarded: a no-op (no emit)
## when GodotSteam is absent / not a Steam build.
func request_session_list() -> void:
	if not _ensure_client():
		return
	_wire_lobby_signals()
	# Scope the query to lobbies tagged as ours, span every Steam region (the default filter is region-local and
	# hides overseas hosts), and state the result cap. All best-effort across GodotSteam name drift. Filters apply
	# to the NEXT requestLobbyList only, so they are re-applied on every sweep.
	_call_first(_steam_obj, [&"addRequestLobbyListStringFilter"],
		[_LOBBY_KEY_GAME, _lobby_tag(), _LOBBY_COMPARISON_EQUAL])
	_call_first(_steam_obj, [&"addRequestLobbyListDistanceFilter"], [_LOBBY_DISTANCE_WORLDWIDE])
	_call_first(_steam_obj, [&"addRequestLobbyListResultCountFilter"], [_LOBBY_RESULT_LIMIT])
	_call_first(_steam_obj, [&"requestLobbyList"], [])

# Best-effort: create + advertise a matchmaking lobby for this listen host so the browser can discover it, and so
# there is something to invite friends INTO. The cap, host id and friends flag are published as metadata once the
# lobby-created callback lands (_on_lobby_created). Guarded end-to-end -- a failure anywhere leaves the direct
# Steam-ID join path (already working) untouched.
#
# The lobby type narrows to k_ELobbyTypeFriendsOnly when requested, and Valve is explicit that a friends-only lobby
# "does not show up in the lobby list" -- so a friends-only session is deliberately INVISIBLE to the browser and is
# reached only through an invite. That is the intended meaning of the toggle, and it is only actually usable now
# that the invite routes below are wired; before, ticking the box made a session nobody could reach at all.
func _advertise_lobby(max_members: int, friends_only: bool) -> void:
	if _steam_obj == null:
		return
	_advertise_max = max_members
	_advertise_friends_only = friends_only
	_wire_lobby_signals()
	var lobby_type: int = _LOBBY_TYPE_FRIENDS_ONLY if friends_only else _LOBBY_TYPE_PUBLIC
	_call_first(_steam_obj, [&"createLobby"], [lobby_type, maxi(1, max_members)])

# Connect the lobby create/list callbacks once (idempotent). Guarded per-signal so a GodotSteam build missing one
# simply never delivers that callback rather than erroring.
func _wire_lobby_signals() -> void:
	if _lobby_signals_wired or _steam_obj == null:
		return
	_lobby_signals_wired = true
	_connect_steam(&"lobby_created", _on_lobby_created)
	_connect_steam(&"lobby_match_list", _on_lobby_match_list)
	# The three ways an invite reaches a RUNNING game. join_requested is the overlay's "Join Game" / an accepted
	# lobby invite (the common one); join_game_requested is the connect-string form Steam uses for rich-presence
	# joins and inviteUserToGame; lobby_joined completes whichever of those we chased. Without these connected the
	# invite simply evaporates -- Steam considers its job done once it has told the game.
	_connect_steam(&"join_requested", _on_steam_join_requested)
	_connect_steam(&"join_game_requested", _on_steam_join_game_requested)
	_connect_steam(&"lobby_joined", _on_lobby_joined)

# Connect one Steam signal if the vendored extension actually has it, idempotently. Guarded per-signal so a build
# missing one simply never delivers that callback rather than erroring at connect time.
func _connect_steam(sig: StringName, cb: Callable) -> void:
	if _steam_obj == null or not _steam_obj.has_signal(sig):
		return
	if _steam_obj.is_connected(sig, cb):
		return
	_steam_obj.connect(sig, cb)

# Steam: our advertised lobby was created (or failed). On success, stamp its metadata so the browser can render the
# owner name / cap / friends flag, and cap the member count. result 1 == k_EResultOK.
func _on_lobby_created(result: int, lobby_id: int) -> void:
	if result != _RESULT_OK or lobby_id == 0:
		return
	_hosted_lobby_id = lobby_id
	_call_first(_steam_obj, [&"setLobbyData"], [lobby_id, _LOBBY_KEY_GAME, _lobby_tag()])
	# The connect target, published as metadata because a browsing non-member cannot call getLobbyOwner (see the
	# _LOBBY_KEY_HOST_ID comment). Without this key every discovered row is unjoinable and gets filtered away.
	_call_first(_steam_obj, [&"setLobbyData"], [lobby_id, _LOBBY_KEY_HOST_ID, str(_local_steam_id())])
	_call_first(_steam_obj, [&"setLobbyData"], [lobby_id, _LOBBY_KEY_OWNER_NAME, local_persona_name()])
	_call_first(_steam_obj, [&"setLobbyData"], [lobby_id, _LOBBY_KEY_MAX, str(_advertise_max)])
	_call_first(_steam_obj, [&"setLobbyData"], [lobby_id, _LOBBY_KEY_FRIENDS, "1" if _advertise_friends_only else "0"])
	if _advertise_max > 0:
		_call_first(_steam_obj, [&"setLobbyMemberLimit"], [lobby_id, _advertise_max])
	_publish_player_count()
	# Rich presence is what puts "Join Game" next to our name in a friend's Steam list, and what Steam appends to
	# our command line when a friend accepts from a cold start. A lobby alone makes us *joinable*; this makes us
	# *invitable*. Set once the lobby exists, since the connect string names it.
	_call_first(_steam_obj, [&"setRichPresence"],
		[_RICH_PRESENCE_CONNECT, "%s %d" % [_CONNECT_LOBBY_FLAG, lobby_id]])
	_call_first(_steam_obj, [&"setRichPresence"], [_RICH_PRESENCE_DISPLAY, _RICH_PRESENCE_DISPLAY_VALUE])

# HOST: republish the live session headcount onto the advertised lobby so the browser's "3/8" tracks reality.
# Counts MULTIPLAYER peers (+1 for the host itself), not lobby members -- clients never join the Steam lobby, so
# getNumLobbyMembers would report 1 forever. A no-op when we are not advertising a lobby.
func _publish_player_count() -> void:
	if _steam_obj == null or _hosted_lobby_id == 0:
		return
	var mp: MultiplayerAPI = multiplayer
	if mp == null or mp.multiplayer_peer == null:
		return
	var count: int = mp.get_peers().size() + 1
	_call_first(_steam_obj, [&"setLobbyData"], [_hosted_lobby_id, _LOBBY_KEY_PLAYERS, str(count)])

# A peer joined: refresh the advertised headcount (host side; inert everywhere else).
func _on_peer_connected(_peer_id: int) -> void:
	_publish_player_count()

# Steam: a lobby-list query returned. Read each lobby's metadata into a Steam-blind NetSessionInfo row and publish
# the set. GodotSteam delivers the count and expects getLobbyByIndex to fetch each id; we tolerate either an array
# payload or the index-based API (name drift) via the guarded reader below.
func _on_lobby_match_list(lobbies: Array) -> void:
	var rows: Array[NetSessionInfo] = []
	for entry: Variant in lobbies:
		var lobby_id: int = entry if entry is int else 0
		if lobby_id == 0:
			continue
		var info: NetSessionInfo = _read_lobby(lobby_id)
		if info != null and info.is_joinable():
			rows.push_back(info)
	_sessions = rows
	sessions_updated.emit()

# Build one NetSessionInfo from a discovered lobby's live Steam state. The host id (the connect target) is the lobby
# OWNER; the owner name / cap / friends flag come from the metadata the host stamped; the live member count comes
# from getNumLobbyMembers. Returns null if the owner can't be resolved (a lobby that isn't really joinable).
func _read_lobby(lobby_id: int) -> NetSessionInfo:
	var owner_id: int = _lobby_host_id(lobby_id)
	if owner_id <= 0:
		return null
	var name: String = _lobby_data(lobby_id, _LOBBY_KEY_OWNER_NAME)
	if name == "":
		name = persona_name_for(owner_id)
	var member_count: int = _lobby_data(lobby_id, _LOBBY_KEY_PLAYERS).to_int()
	if member_count <= 0:
		# No headcount published yet (a lobby created by an older build, or one whose first setLobbyData has not
		# propagated). getNumLobbyMembers is only meaningful once we are a member, but it is the honest fallback.
		var members: Variant = _call_first(_steam_obj, [&"getNumLobbyMembers"], [lobby_id])
		member_count = members if members is int else 0
	var max_members: int = _lobby_data(lobby_id, _LOBBY_KEY_MAX).to_int()
	if max_members <= 0:
		var limit: Variant = _call_first(_steam_obj, [&"getLobbyMemberLimit"], [lobby_id])
		max_members = limit if limit is int else 0
	var friends_only: bool = _lobby_data(lobby_id, _LOBBY_KEY_FRIENDS) == "1"
	return NetSessionInfo.make(owner_id, name, member_count, max_members, friends_only)

# Read a lobby metadata string, "" when absent. getLobbyData(lobby_id, key) -> String on GodotSteam.
func _lobby_data(lobby_id: int, key: String) -> String:
	var raw: Variant = _call_first(_steam_obj, [&"getLobbyData"], [lobby_id, key])
	return raw if raw is String else ""

# The CONNECT TARGET behind a lobby: the host's 64-bit Steam id, or 0 when it can't be resolved. Reads the host's
# published `host_id` metadata FIRST -- that is the only source a non-member (i.e. anyone browsing) can read, since
# getLobbyOwner is members-only and answers a browsing peer with a nil id. getLobbyOwner stays as the fallback for
# the one case where it does work and metadata might not have propagated: a lobby we have actually joined.
func _lobby_host_id(lobby_id: int) -> int:
	var published: int = _lobby_data(lobby_id, _LOBBY_KEY_HOST_ID).to_int()
	if published > 0:
		return published
	var owner: Variant = _call_first(_steam_obj, [&"getLobbyOwner"], [lobby_id])
	return owner if owner is int else 0

# --- play invites (accept an invite, send an invite) ----------------------------------------------------
## Parse a Steam launch command line for the `+connect_lobby <id>` token Steam appends when a player accepts an
## invite while the game is NOT running, returning the lobby id (0 when absent/malformed). PURE + static so the
## cold-start path is unit-testable without Steam, a scene tree, or a process relaunch (tests/unit/steam_invite_test.gd).
static func parse_connect_lobby(command_line: String) -> int:
	var tokens: PackedStringArray = command_line.split(" ", false)
	for i: int in range(tokens.size() - 1):
		if tokens[i] == _CONNECT_LOBBY_FLAG:
			var id: int = tokens[i + 1].to_int()
			return id if id > 0 else 0
	return 0

## Check whether Steam launched this process to accept an invite (`+connect_lobby <id>`) and, if so, start
## resolving it -- the result arrives on [signal invite_accepted] like every other accept route. Safe to call on
## any build and at any time: it is a no-op without GodotSteam, and consumes the launch args at most once per
## process. Called from the boot path once the session layer is ready to act on a join.
func check_launch_invite() -> void:
	if _launch_invite_consumed:
		return
	_launch_invite_consumed = true
	if not _ensure_client():
		return
	# Both sources matter. Steam appends the token to the process ARGV when it launches the game to accept an
	# invite; getLaunchCommandLine covers the protocol-URL/rich-presence route where the token never reaches argv.
	var lobby_id: int = parse_connect_lobby(" ".join(OS.get_cmdline_args()))
	if lobby_id == 0:
		var launch: Variant = _call_first(_steam_obj, [&"getLaunchCommandLine"], [])
		var launch_line: String = launch if launch is String else ""
		lobby_id = parse_connect_lobby(launch_line)
	if lobby_id != 0:
		_accept_lobby_invite(lobby_id)

## Whether an in-game "Invite Friends" affordance should be offered: a Steam build that is currently advertising a
## lobby to invite people INTO. False on ENet, off-Steam, or when we are not hosting.
func can_invite() -> bool:
	return _hosted_lobby_id != 0

## Open Steam's overlay invite dialog for the lobby we are hosting, so the player can pick friends to invite. A
## no-op when [method can_invite] is false. The overlay owns the whole flow from here -- the invitee's client
## answers with join_requested / `+connect_lobby`, which lands back on [signal invite_accepted].
func open_invite_overlay() -> void:
	if _steam_obj == null or _hosted_lobby_id == 0:
		return
	_call_first(_steam_obj, [&"activateGameOverlayInviteDialog"], [_hosted_lobby_id])

# Steam: the player accepted a lobby invite / hit "Join Game" on a friend while we are RUNNING. We get the lobby
# to join and the friend who owns it. Resolve it through the same lobby hop as every other route.
func _on_steam_join_requested(lobby_id: int, _friend_id: int) -> void:
	_accept_lobby_invite(lobby_id)

# Steam: the CONNECT-STRING form of the same event (rich presence / inviteUserToGame). The payload is the literal
# connect string we published, so it parses with the same pure parser the cold-start path uses.
func _on_steam_join_game_requested(_user: int, connect_string: String) -> void:
	var lobby_id: int = parse_connect_lobby(connect_string)
	if lobby_id != 0:
		_accept_lobby_invite(lobby_id)

# Begin resolving an accepted invite. An invite names a LOBBY, but create_client needs the HOST's Steam id, so we
# join the lobby and finish on lobby_joined -- by then we are a member and its metadata is certainly readable.
# Ignores a repeat for the lobby already in flight (Steam can deliver both join_requested and a connect string for
# one click) and a lobby we are ourselves hosting.
func _accept_lobby_invite(lobby_id: int) -> void:
	if lobby_id <= 0 or lobby_id == _pending_invite_lobby or lobby_id == _hosted_lobby_id:
		return
	if not _ensure_client():
		return
	# A second invite accepted before the first one's session connected supersedes it -- drop out of the stale
	# lobby rather than accumulating memberships in sessions we are never going to join.
	_leave_lobby_awaiting_session()
	_pending_invite_lobby = lobby_id
	_call_first(_steam_obj, [&"joinLobby"], [lobby_id])

# Steam: a lobby join finished. Only interesting for a lobby we chased on an accepted invite -- the HOST receives
# this same signal for the lobby it just created, and must not "accept an invite" to its own session. On success,
# resolve the host and hand the Steam-blind connect target up; on failure, clear the pending state so a later
# invite is not swallowed by the duplicate guard.
func _on_lobby_joined(lobby_id: int, _permissions: int, _locked: bool, response: int) -> void:
	if lobby_id != _pending_invite_lobby:
		return
	_pending_invite_lobby = 0
	if response != _CHAT_ROOM_ENTER_SUCCESS:
		push_warning("SteamTransport: lobby %d join failed (response %d) -- invite not actionable" % [lobby_id, response])
		return
	var host_id: int = _lobby_host_id(lobby_id)
	if host_id <= 0:
		push_warning("SteamTransport: lobby %d has no resolvable host -- invite not actionable" % lobby_id)
		_call_first(_steam_obj, [&"leaveLobby"], [lobby_id])
		return
	# Staged, NOT handed straight to _joined_lobby_id: the listener may tear down a session it is already in before
	# joining this one, and that teardown must not leave the lobby we just entered. connected_to_server promotes it.
	_invite_lobby_awaiting_session = lobby_id
	invite_accepted.emit(str(host_id))

# Drop out of a staged invite lobby whose session never materialised (the join failed, or a newer invite replaced
# it). Keeps the staging slot from silently pinning us to a lobby forever, since release_session() cannot see it.
func _leave_lobby_awaiting_session() -> void:
	if _invite_lobby_awaiting_session == 0:
		return
	_call_first(_steam_obj, [&"leaveLobby"], [_invite_lobby_awaiting_session])
	_invite_lobby_awaiting_session = 0

# The session an accepted invite was taking us to never came up. Release the staged lobby rather than staying a
# member of a session we never reached. Bound to multiplayer.connection_failed in _ready.
func _on_connection_failed() -> void:
	_leave_lobby_awaiting_session()

## Release every Steam session artefact this process is holding: the lobby we advertise as host, a lobby we joined
## as an invitee, and our rich presence. Called from the session-teardown path so a stopped session stops being
## advertised -- otherwise the lobby lingers in every browser as a ghost row, and re-hosting in the same process
## leaks a second lobby. Safe to call on any build, in any state.
func release_session() -> void:
	if _steam_obj == null:
		return
	if _hosted_lobby_id != 0:
		_call_first(_steam_obj, [&"leaveLobby"], [_hosted_lobby_id])
		_hosted_lobby_id = 0
	if _joined_lobby_id != 0:
		_call_first(_steam_obj, [&"leaveLobby"], [_joined_lobby_id])
		_joined_lobby_id = 0
	_pending_invite_lobby = 0
	_call_first(_steam_obj, [&"clearRichPresence"], [])

# --- network conditioner (netbench; the in-process Steam arm of net.sim_*) -----------------------
## Apply artificial network conditions to the Steam transport -- the honest in-process conditioner for the Steam
## path (netbench). SteamNetworkingSockets injects its FakePacket* impairment at the raw-UDP layer BELOW its SNP
## reliability, so this genuinely exercises retransmit/ordering (the ENet equivalent is the external relay, since
## ENet has no such seam). Values map to Steam's globals on the SEND side only (Recv left 0), so one-way semantics
## match the relay: `lag_ms`/`jitter_ms`/`reorder_ms` are milliseconds, `loss_pct`/`reorder_pct`/`dup_pct` are
## PERCENTAGES (0..100). GLOBAL scope (each Godot process is one peer), pushed via GodotSteam's
## setGlobalConfigValueInt32/Float (ISteamNetworkingUtils::SetGlobalConfigValue). Returns true if the knobs were
## applied, false when GodotSteam / the singleton isn't present (a non-Steam build simply never calls this).
##
## VERIFY-ON-A-STEAM-BUILD: the config-value enum ints are resolved BY NAME from the singleton's registered
## constants (never hardcoded -- the exact ints aren't verifiable in a headless sandbox), and every call is guarded,
## so a renamed constant / missing method degrades to a warning + false rather than a crash. Validate the constant
## names against the exact GodotSteam version you vendor (docs/steam.md).
func apply_fake_conditions(lag_ms: float, jitter_ms: float, loss_pct: float, reorder_pct: float,
		reorder_ms: float, dup_pct: float) -> bool:
	var steam: Object = _resolve_singleton([&"Steam"])
	if steam == null:
		return false
	if not steam.has_method(&"setGlobalConfigValueInt32") or not steam.has_method(&"setGlobalConfigValueFloat"):
		push_warning("SteamTransport.apply_fake_conditions: GodotSteam has no setGlobalConfigValue* -- cannot condition")
		return false
	# LAG_SEND resolving is the proxy for "the FakePacket* constant names match this GodotSteam version".
	var ok: bool = _set_cfg_int(steam, &"NETWORKING_CONFIG_FAKE_PACKET_LAG_SEND", int(roundf(lag_ms)))
	_set_cfg_int(steam, &"NETWORKING_CONFIG_FAKE_PACKET_LAG_RECV", 0)
	_set_cfg_int(steam, &"NETWORKING_CONFIG_FAKE_PACKET_REORDER_TIME", int(roundf(reorder_ms)))
	_set_cfg_float(steam, &"NETWORKING_CONFIG_FAKE_PACKET_LOSS_SEND", loss_pct)
	_set_cfg_float(steam, &"NETWORKING_CONFIG_FAKE_PACKET_LOSS_RECV", 0.0)
	_set_cfg_float(steam, &"NETWORKING_CONFIG_FAKE_PACKET_REORDER_SEND", reorder_pct)
	_set_cfg_float(steam, &"NETWORKING_CONFIG_FAKE_PACKET_DUP_SEND", dup_pct)
	# Jitter is a newer GodotSteam knob (send-side average); set it if this version exposes it, ignore otherwise.
	_set_cfg_int(steam, &"NETWORKING_CONFIG_FAKE_PACKET_JITTER_SEND_AVG", int(roundf(jitter_ms)))
	return ok

func _set_cfg_int(steam: Object, const_name: StringName, value: int) -> bool:
	var cfg: int = _config_enum(steam, const_name)
	if cfg < 0:
		return false
	steam.callv(&"setGlobalConfigValueInt32", [cfg, value])
	return true

func _set_cfg_float(steam: Object, const_name: StringName, value: float) -> bool:
	var cfg: int = _config_enum(steam, const_name)
	if cfg < 0:
		return false
	steam.callv(&"setGlobalConfigValueFloat", [cfg, value])
	return true

# Resolve a GodotSteam NetworkingConfigValue enum member by NAME to its int, from the singleton's registered class
# constants; -1 (a safe sentinel -- these enum values are positive) when the constant isn't present.
func _config_enum(steam: Object, const_name: StringName) -> int:
	var cls: StringName = steam.get_class()
	if ClassDB.class_has_integer_constant(cls, const_name):
		return ClassDB.class_get_integer_constant(cls, const_name)
	push_warning("SteamTransport: config constant '%s' not on '%s' (verify the GodotSteam version)" % [const_name, cls])
	return -1

# --- dynamic-Steam helpers -----------------------------------------------------------------------

# The first of `names` registered as an Engine singleton, or null when none are (GodotSteam not vendored / not a
# steam build). Lets the client path resolve `Steam` and the dedicated path prefer `SteamServer`, from one place.
func _resolve_singleton(names: Array[StringName]) -> Object:
	for n: StringName in names:
		if Engine.has_singleton(n):
			return Engine.get_singleton(n)
	return null

# The configured Steam app id (project setting, else the Spacewar test app). Read as a plain int.
func _app_id() -> int:
	var raw: Variant = ProjectSettings.get_setting(_APP_ID_SETTING, DEFAULT_APP_ID)
	var id: int = raw if raw is int else DEFAULT_APP_ID
	return id

# The lobby tag identifying this game's lobbies, from the project setting.
func _lobby_tag() -> String:
	var raw: Variant = ProjectSettings.get_setting(_LOBBY_TAG_SETTING, _DEFAULT_LOBBY_TAG)
	var tag: String = raw if raw is String else _DEFAULT_LOBBY_TAG
	return tag if tag != "" else _DEFAULT_LOBBY_TAG

# Call the first method in `methods` that exists on `obj`, with `args`; null if none exist (or obj is null). Lets
# a single call site tolerate GodotSteam renaming a function across versions without a hard failure.
func _call_first(obj: Object, methods: Array[StringName], args: Array) -> Variant:
	if obj == null:
		return null
	for m: StringName in methods:
		if obj.has_method(m):
			return obj.callv(m, args)
	return null

# GodotSteam's *InitEx functions return { status: int, verbal: String } with status 0 == OK. Some older/simpler
# entry points return a bare bool or an int. Treat any of those OK-shapes as success.
func _init_ok(result: Variant) -> bool:
	if result is Dictionary:
		var status: Variant = result.get("status", -1)
		var status_int: int = status if status is int else -1
		return status_int == 0
	if result is bool:
		return result
	if result is int:
		return result == 0
	return false
