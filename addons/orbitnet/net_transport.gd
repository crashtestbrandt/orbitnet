extends RefCounted
class_name NetTransport
## Transport factory for the netcode facade (skeleton; wires it into the session manager). Produces the
## Godot MultiplayerPeer for a session, branching on the build's feature tags: native ENet (ENetMultiplayerPeer)
## on non-Steam builds, and Steam (SteamMultiplayerPeer, via addons/orbitnet/steam_transport.gd) on a build
## exported with the `Steam` preset. OFFLINE uses NO peer (the offline demo runs on the default
## OfflineMultiplayerPeer). Transport is Godot-native here -- this file does NOT name the rollback backend (only
## orbitnet/net.gd touches it), and it does NOT name Steamworks (only steam_transport.gd touches it): both stay
## behind their one facade boundary, so the session layer only ever sees the resulting MultiplayerPeer.
##
## adds three more Steam-blind seams here (still the only place besides steam_transport.gd that knows Steam is
## involved): the local player's DISPLAY NAME + platform id for the [PlayerRoster], joinable-SESSION discovery for
## the join browser, and PLAY INVITES (accept + send + the session advertisement's teardown). All degrade cleanly
## on ENet (name -> "" / the local override; sessions -> empty list; invites -> never fire, can_invite() false).

enum Kind { OFFLINE, ENET, STEAM }

const DEFAULT_PORT: int = 47800        # arbitrary UDP port for direct ENet host/join (owner-tunable later)
const DEFAULT_MAX_CLIENTS: int = 8

# A local display-name override (the `net.name` console cvar / a unit test). Takes precedence over the transport's
# own name so a player can pick a handle on any build -- and so the whole name pipeline is exercisable offline / in
# CI, where there is no Steam persona. Empty = "no override", the default. Static: the factory has no instance.
static var _local_name_override: String = ""

## The transport kind this build prefers, from feature tags: Steam if this is a Steam build (the `Steam` /
## `Steam Server` export presets set custom_features="steam", so OS.has_feature("steam") trips), otherwise native
## ENet. On any non-Steam build the "steam" feature is false and this returns ENET -- Steamworks is never even looked up (steam_transport
## is not reached), so those builds carry zero Steam dependency.
static func preferred_kind() -> Kind:
	if OS.has_feature("steam"):
		return Kind.STEAM
	return Kind.ENET

## A transport kind's stable lowercase name ("offline" / "enet" / "steam"). Lives here because this file is the
## only one allowed to name a concrete transport -- callers that merely need to PRINT which one is in play (the
## Build ID, a status line) ask for the name instead of matching on the enum themselves.
static func kind_name(kind: Kind) -> String:
	match kind:
		Kind.ENET:
			return "enet"
		Kind.STEAM:
			return "steam"
		_:
			return "offline"

## The name of the transport this build prefers -- "steam" on a Steam-preset build, "enet" everywhere else.
## Never "offline": [method preferred_kind] describes the build, not the current session.
static func preferred_kind_name() -> String:
	return kind_name(preferred_kind())

## Build a server (host) peer listening on `port` for up to `max_clients`, optionally FRIENDS-ONLY. Returns
## null on failure (the caller stays OFFLINE / surfaces an error). On a Steam build the concrete peer + Steam
## registration live in steam_transport.gd; a dedicated-server build (dedicated_server feature) registers a Steam
## game server, a listen host uses the logged-in user's client + advertises a discoverable lobby carrying the cap /
## friends-only flag. `friends_only` is a Steam lobby-type concept, so it is ignored on the ENet path (native builds
## have no matchmaking) -- max_clients maps to ENet's hard cap there.
static func create_server(port: int = DEFAULT_PORT, max_clients: int = DEFAULT_MAX_CLIENTS,
		friends_only: bool = false) -> MultiplayerPeer:
	match preferred_kind():
		Kind.ENET:
			var peer: ENetMultiplayerPeer = ENetMultiplayerPeer.new()
			var err: Error = peer.create_server(port, max_clients)
			if err != OK:
				push_warning("NetTransport: ENet create_server(%d) failed: %s" % [port, error_string(err)])
				return null
			return peer
		Kind.STEAM:
			if OS.has_feature("dedicated_server"):
				return SteamTransport.service().create_dedicated_host(port, max_clients, friends_only)
			return SteamTransport.service().create_listen_host(port, max_clients, friends_only)
		_:
			return null

## Build a client peer connecting to `address`:`port`. Returns null on failure. On a Steam build `address` carries
## the host's 64-bit Steam ID (a decimal string) rather than an IP -- the SessionMenu "Host address" field is
## relabeled to a Steam ID there; steam_transport.gd resolves it over Steam's relay.
static func create_client(address: String, port: int = DEFAULT_PORT) -> MultiplayerPeer:
	match preferred_kind():
		Kind.ENET:
			var peer: ENetMultiplayerPeer = ENetMultiplayerPeer.new()
			var err: Error = peer.create_client(address, port)
			if err != OK:
				push_warning("NetTransport: ENet create_client(%s:%d) failed: %s" % [address, port, error_string(err)])
				return null
			return peer
		Kind.STEAM:
			return SteamTransport.service().create_client(address, port)
		_:
			return null

# --- player identity (Steam-blind seam) ------------------------------------------------------------------
## Set (or clear, with "") this peer's local display-name override -- the `net.name` console cvar routes here. It
## wins over the transport's own name, so a handle works on ENet too and the name pipeline is testable offline.
static func set_local_display_name(name: String) -> void:
	_local_name_override = name.strip_edges()

## The current local display-name override ("" when none). Lets the `net.name` cvar echo what it holds.
static func local_display_name_override() -> String:
	return _local_name_override

## This peer's local display name for the [PlayerRoster] to advertise. Precedence: the local override, else the
## transport's own name (a Steam build's persona name -- resolved behind the boundary in steam_transport.gd), else
## "" (the roster then shows the generic "Player <id>"). Never reaches Steam on a non-Steam build.
static func local_display_name() -> String:
	if _local_name_override != "":
		return _local_name_override
	if preferred_kind() == Kind.STEAM:
		return SteamTransport.service().local_persona_name()
	return ""

## The local display name for a console echo, or a readable placeholder when it's unset (the roster then shows the
## generic "Player <id>"). Purely presentational -- the `net.name` command prints this.
static func local_display_name_or_generic() -> String:
	var name: String = local_display_name()
	return name if name != "" else "(unset -- shown as \"Player <id>\")"

## This peer's platform (Steam) id, or 0 on a non-Steam build -- the roster carries it alongside the name. Resolved
## behind the boundary; never looked up unless this is a Steam build.
static func local_steam_id() -> int:
	if preferred_kind() == Kind.STEAM:
		return SteamTransport.service().local_steam_id()
	return 0

# --- session discovery (the join browser's Steam-blind seam) --------------------------------------------
## Ask the transport to (re)discover joinable sessions. Fires-and-returns; results arrive asynchronously and are
## read via [method sessions] (bind [method bind_sessions_updated] for the change signal). A no-op on ENet -- native
## builds have no session discovery (you join by address), so the browser stays empty there.
static func request_sessions() -> void:
	if preferred_kind() == Kind.STEAM:
		SteamTransport.service().request_session_list()

## The sessions discovered so far (each a Steam-blind [NetSessionInfo]). Empty on ENet / before the first result.
static func sessions() -> Array[NetSessionInfo]:
	if preferred_kind() == Kind.STEAM:
		return SteamTransport.service().sessions()
	return []

## Bind `cb` to the "session list changed" signal so a browser refreshes when discovery results arrive. A no-op on
## ENet (nothing ever emits), so a caller can wire this unconditionally and simply never hear from it there.
static func bind_sessions_updated(cb: Callable) -> void:
	if preferred_kind() != Kind.STEAM:
		return
	var svc: SteamTransport = SteamTransport.service()
	if not svc.sessions_updated.is_connected(cb):
		svc.sessions_updated.connect(cb)

# --- play invites (the platform-invite seam, still Steam-blind) -----------------------------------------
## Bind `cb` to "the player accepted a platform invite and it resolved to a joinable session". `cb` receives a
## connect target string suitable for [method create_client] / the session layer's join path -- the caller never learns that a
## Steam lobby was involved. A no-op on ENet (native builds have no invite concept), so this can be wired
## unconditionally and simply never fires there.
static func bind_invite_accepted(cb: Callable) -> void:
	if preferred_kind() != Kind.STEAM:
		return
	var svc: SteamTransport = SteamTransport.service()
	if not svc.invite_accepted.is_connected(cb):
		svc.invite_accepted.connect(cb)

## Ask the transport whether this process was LAUNCHED to accept an invite (the cold-start route: a friend clicks
## "Join Game" while the game is closed, and the platform starts it with a connect token). Any result arrives on
## the [method bind_invite_accepted] callback, so the caller has one code path for warm and cold accepts. A no-op
## on ENet, and at most one consumption per process.
static func check_launch_invite() -> void:
	if preferred_kind() == Kind.STEAM:
		SteamTransport.service().check_launch_invite()

## Whether an in-game "Invite Friends" affordance can do anything right now -- a platform build that is currently
## advertising a session to invite people into. False on ENet and whenever we are not hosting, so the UI can hide
## the control rather than offering a dead button.
static func can_invite() -> bool:
	if preferred_kind() == Kind.STEAM:
		return SteamTransport.service().can_invite()
	return false

## Open the platform's invite UI for the session we are hosting. A no-op when [method can_invite] is false.
static func open_invite_overlay() -> void:
	if preferred_kind() == Kind.STEAM:
		SteamTransport.service().open_invite_overlay()

## Release any platform-side session advertisement (a discoverable lobby, presence) when a session ends, so a
## stopped session stops showing up in other players' browsers. A no-op on ENet, where nothing is advertised.
static func release_session() -> void:
	if preferred_kind() == Kind.STEAM:
		SteamTransport.service().release_session()
