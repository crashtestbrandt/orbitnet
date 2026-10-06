extends RefCounted
class_name SeatRequests
## SERVER-SIDE: how many seats each connection asked for, so it is seated once, after it has both joined and
## asked. Pure: two facts per peer and the rule that combines them, with no tree and no transport.
##
## **A connection is seated on the second of two events, whichever order they arrive in.**
##
## - `Net.peer_joined` carries the session identity, which seating needs (see ArenaNet's RULE 4).
## - The client's `_seat_request` RPC carries how many seats it wants and whether to spread them.
## - The RPC rides the reliable channel and the join rides the handshake, so either can arrive first.
## - A client that never asks is seated with one fighter after `ArenaNet.SEAT_REQUEST_WAIT_S`
##   (`take_default`).
##
## A request after the peer is seated changes nothing: a client asks once, when its transport comes up.

## Seats a connection gets when it never asked.
const DEFAULT_SEATS: int = 1

var _session: Dictionary[int, int] = {}
var _resumed_from: Dictionary[int, int] = {}
var _count: Dictionary[int, int] = {}
var _spread: Dictionary[int, bool] = {}
var _seated: Dictionary[int, bool] = {}

## Record that `peer` finished its handshake. True when it has already asked, so it is ready to seat now.
func note_joined(peer: int, session_id: int, resumed_from: int) -> bool:
	if _seated.has(peer):
		return false
	_session[peer] = session_id
	_resumed_from[peer] = resumed_from
	return _count.has(peer)

## Record what `peer` asked for, bounded to 1 .. `ArenaConfig.MAX_SEATS_PER_PEER`. True when it has already
## joined, so it is ready to seat now. A repeated request before seating replaces the earlier one.
func note_request(peer: int, count: int, spread: bool) -> bool:
	if _seated.has(peer):
		return false
	_count[peer] = clampi(count, 1, ArenaConfig.MAX_SEATS_PER_PEER)
	_spread[peer] = spread
	return _session.has(peer)

## The fallback for a peer that joined and never asked: one seat. True when the peer is ready to seat now,
## false when it already asked, was already seated, or never joined.
func take_default(peer: int) -> bool:
	if _seated.has(peer) or _count.has(peer) or not _session.has(peer):
		return false
	_count[peer] = DEFAULT_SEATS
	_spread[peer] = false
	return true

## Record that `peer` has been seated, so a later request or fallback is ignored.
func mark_seated(peer: int) -> void:
	_seated[peer] = true

## Whether `peer` has been seated.
func is_seated(peer: int) -> bool:
	return _seated.has(peer)

## How many seats `peer` asked for, or `DEFAULT_SEATS`.
func count(peer: int) -> int:
	var value: int = _count.get(peer, DEFAULT_SEATS)
	return value

## Whether `peer` asked for its seats in different arenas.
func spread(peer: int) -> bool:
	var value: bool = _spread.get(peer, false)
	return value

## The identity `peer` joined with, or `SeatRoster.NO_SESSION`.
func session_id(peer: int) -> int:
	var value: int = _session.get(peer, SeatRoster.NO_SESSION)
	return value

## The connection `peer`'s claim resumed, or 0.
func resumed_from(peer: int) -> int:
	var value: int = _resumed_from.get(peer, 0)
	return value

## Forget `peer` entirely: it left, and peer ids can be reused.
func forget(peer: int) -> void:
	_session.erase(peer)
	_resumed_from.erase(peer)
	_count.erase(peer)
	_spread.erase(peer)
	_seated.erase(peer)

## Forget every peer. Session teardown.
func clear() -> void:
	_session.clear()
	_resumed_from.clear()
	_count.clear()
	_spread.clear()
	_seated.clear()
