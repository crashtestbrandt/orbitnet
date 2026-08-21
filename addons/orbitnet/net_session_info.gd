extends RefCounted
class_name NetSessionInfo
## One joinable session as the join browser sees it. A PURE, Steam-blind value object: the transport
## factory ([NetTransport]) hands these out and the [SessionMenu] browser renders them, so neither the UI nor
## the session layer ever names Steam. On a Steam build [SteamTransport] fills each field from a discovered lobby's
## metadata (owner persona name, live member count, the advertised cap, the friends-only flag); on a non-Steam
## build the list is simply empty (ENet has no session discovery -- you join by address), so this class only ever
## carries already-resolved plain data.
##
## `host_id` doubles as the CONNECT TARGET: [method connect_target] returns it as the decimal string the
## transport's create_client takes (a host's 64-bit Steam id on a Steam build), so picking a row in the browser
## joins that session without the UI knowing what the id means.

## The host's 64-bit id (a Steam id on a Steam build). 0 == unknown/unset (a degenerate row the UI hides).
var host_id: int = 0
## The host's human display name (owner's Steam persona name). May be empty -> [method display_owner] falls back.
var owner_name: String = ""
## Live member count in the session, and the advertised maximum (the host's "max players"). players <= max_players.
var players: int = 0
var max_players: int = 0
## Whether the host toggled friends-only (the session is invite/friends-restricted). Presentational here.
var friends_only: bool = false

## Build a fully-populated record in one call (the SteamTransport lobby-metadata reader + the unit tests use this).
static func make(p_host_id: int, p_owner_name: String, p_players: int, p_max_players: int,
		p_friends_only: bool) -> NetSessionInfo:
	var info: NetSessionInfo = NetSessionInfo.new()
	info.host_id = p_host_id
	info.owner_name = p_owner_name.strip_edges()
	info.players = maxi(0, p_players)
	info.max_players = maxi(0, p_max_players)
	info.friends_only = p_friends_only
	return info

## The connect target string the transport's create_client takes -- the host id as a decimal string (a Steam id on
## a Steam build). Empty when host_id is unset, so the UI can skip a degenerate row.
func connect_target() -> String:
	return str(host_id) if host_id > 0 else ""

## Whether this row is joinable (has a real host id). The browser filters on this so a half-populated lobby (no
## host id in its metadata yet) never shows as a clickable session.
func is_joinable() -> bool:
	return host_id > 0

## The owner name to show, falling back to a generic label when the persona name hasn't resolved yet (a lobby whose
## owner metadata is still empty), so a row never renders blank.
func display_owner() -> String:
	return owner_name if owner_name.strip_edges() != "" else "Unknown host"

## Whether the session still has room (a full lobby is shown but not joinable). max_players <= 0 means "no cap
## advertised", which counts as room.
func has_room() -> bool:
	return max_players <= 0 or players < max_players

## The one-line label the browser renders: "Owner  (3/8)" plus a friends tag and a FULL marker when appropriate.
## Pure string assembly (unit-tested) so the row layout has a single source of truth.
func summary() -> String:
	var count: String = "%d/%d" % [players, max_players] if max_players > 0 else "%d" % players
	var text: String = "%s  (%s)" % [display_owner(), count]
	if friends_only:
		text += "  · friends"
	if not has_room():
		text += "  · FULL"
	return text
