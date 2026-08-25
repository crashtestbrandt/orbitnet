extends RefCounted
class_name TeamRoster
## Who sits in which seat, and therefore on which team. Pure: a map from multiplayer peer id to seat index,
## with no signals, no tree and no transport, so the whole thing is unit-testable.
##
## SEAT PARITY FIXES THE END. Even seats defend -z, odd seats defend +z, and a player's team is derived from
## the seat index rather than replicated -- every peer already knows the seat, so sending the team would be
## sending a subtraction.
##
## ASSIGNMENT GOES TO THE THINNER END, ties to team 0. On an empty table that reduces to strict alternation --
## 0, 1, 2, 3 -- which is what "auto-assigned to alternating ends" means when nobody has left yet. After a
## drop-out it refills the side that lost a player instead of continuing the alternation into a 3-v-1, so a
## mid-round join balances the game rather than deepening the gap.
##
## A SEAT OUTLIVES THE CONNECTION THAT HELD IT. Two maps, not one, and the second is what makes a reconnect
## land in the same seat: `_peer_of_seat` says who is sitting there NOW, `_session_of_seat` says which
## identity may claim it back. A seat is free only when neither names it. Keying on the peer id alone hands a
## returning player whichever seat happens to be free, which is how somebody comes back onto the other team.
##
## THE SEAT IS RESOLVED FROM THE SENDER ID, NEVER FROM THE PAYLOAD. That is the whole security model for the
## command channel and it is worth being blunt about: a client says WHAT it wants, and the server decides WHO
## is asking, from `multiplayer.get_remote_sender_id()` -- a value the transport supplies and the sender cannot
## author. If the seat came out of the payload, every ownership check downstream would be checking the
## attacker's own claim about themselves.

## NetCommand's sentinel for "applied locally with no session".
const OFFLINE_SENDER: int = 0
## The listen host / dedicated server's own peer id.
const SERVER_PEER: int = 1
## "No identity" -- what a local host and a peer that claimed none both present. Such a seat is never held.
const NO_SESSION: int = 0

var _seat_of_peer: Dictionary[int, int] = {}
var _peer_of_seat: Dictionary[int, int] = {}
## Which identity may reclaim each seat, and the reverse. Written when a peer drops with its session HELD,
## read when one arrives claiming that identity. A local host and a peer that claimed no identity are never
## in here -- `NO_SESSION` is not an identity, it is the absence of one, and several peers can present it.
var _session_of_seat: Dictionary[int, int] = {}
var _seat_of_session: Dictionary[int, int] = {}

## Seat `peer` on the end with fewer players, or return its existing seat if it already holds one. -1 when
## every seat is taken -- with HockeyConfig.SEATS seats and the transport capped at the same number, the
## caller reaches that only if the two are ever allowed to disagree.
##
## `session_id` is the identity to reclaim a HELD seat with; `NO_SESSION` (0) takes a free one as a newcomer.
## The caller decides whether a presented identity is worth honoring -- see HockeyNet, which honors it only
## for a session it already saw drop.
func assign(peer: int, session_id: int = NO_SESSION) -> int:
	if _seat_of_peer.has(peer):
		return _seat_of_peer[peer]
	if session_id != NO_SESSION and _seat_of_session.has(session_id):
		var held: int = _seat_of_session[session_id]
		_seat_of_peer[peer] = held
		_peer_of_seat[held] = peer
		# THE HOLD ENDS WHERE THE SITTING BEGINS. `occupied_on_team()` walks `_peer_of_seat` and
		# `_session_of_seat` in turn and `is_full()` adds `occupied()` to `reserved()`, so a seat left in
		# both counts twice: the rink balances against a phantom player and reports itself full with seats
		# still free. `release()` erases all four for the same reason.
		_seat_of_session.erase(session_id)
		_session_of_seat.erase(held)
		return held
	var preferred: int = 0 if occupied_on_team(0) <= occupied_on_team(1) else 1
	var seat: int = _lowest_free_seat_on(preferred)
	if seat < 0:
		seat = _lowest_free_seat_on(1 - preferred)
	if seat < 0:
		return -1
	_seat_of_peer[peer] = seat
	_peer_of_seat[seat] = peer
	return seat

## Release `peer`'s seat outright: nobody is in it and nobody may claim it back.
func release(peer: int) -> void:
	if not _seat_of_peer.has(peer):
		return
	var seat: int = _seat_of_peer[peer]
	_seat_of_peer.erase(peer)
	_peer_of_seat.erase(seat)
	if _session_of_seat.has(seat):
		_seat_of_session.erase(_session_of_seat[seat])
		_session_of_seat.erase(seat)

## HOLD `peer`'s seat for `session_id`: the peer stops sitting in it, the seat stays taken, and that identity
## may take it back. Returns the seat, or -1 if the peer held none or presented no identity.
##
## An identity of `NO_SESSION` cannot hold anything, and that is a rule rather than an omission: several peers
## present it at once, so a seat held under it would be handed to whichever of them reconnected first.
func hold(peer: int, session_id: int) -> int:
	if session_id == NO_SESSION or not _seat_of_peer.has(peer):
		return -1
	var seat: int = _seat_of_peer[peer]
	_seat_of_peer.erase(peer)
	_peer_of_seat.erase(seat)
	_session_of_seat[seat] = session_id
	_seat_of_session[session_id] = seat
	return seat

## Give up a held seat: the grace window closed and that identity is not coming back.
func release_session(session_id: int) -> void:
	if not _seat_of_session.has(session_id):
		return
	var seat: int = _seat_of_session[session_id]
	_seat_of_session.erase(session_id)
	_session_of_seat.erase(seat)

## The seat `session_id` is holding, or -1. A seat someone is SITTING in is not held; this answers only about
## seats waiting for a return.
func seat_of_session(session_id: int) -> int:
	return _seat_of_session.get(session_id, -1)

## How many seats are being kept for identities that are not currently connected.
func reserved() -> int:
	return _session_of_seat.size()

## The seat held by `peer`, or -1.
func seat_of_peer(peer: int) -> int:
	return _seat_of_peer.get(peer, -1)

## The peer holding `seat`, or -1.
func peer_of_seat(seat: int) -> int:
	return _peer_of_seat.get(seat, -1)

## Resolve the seat for a command's sender id -- the one function the command path calls.
##
## Offline (sender 0) is seat 0 unconditionally, WITHOUT consulting the table: offline there is no peer list to
## be in, so a lookup would return -1 and every single-player serve would be rejected as unseated.
func seat_for_sender(sender: int) -> int:
	if sender == OFFLINE_SENDER:
		return 0
	return seat_of_peer(sender)

## How many seats are taken.
func occupied() -> int:
	return _seat_of_peer.size()

## How many of `team`'s seats are taken, HELD SEATS INCLUDED. Balance is decided from what is available, and
## a seat waiting for its player to return is not available.
func occupied_on_team(team: int) -> int:
	var count: int = 0
	for seat: int in _peer_of_seat.keys():
		if HockeyConfig.team_of_seat(seat) == team:
			count += 1
	for seat: int in _session_of_seat.keys():
		if HockeyConfig.team_of_seat(seat) == team:
			count += 1
	return count

## Whether every seat is taken -- by a live peer OR by an identity holding one. A held seat is taken; that is
## the whole point of holding it.
func is_full() -> bool:
	return occupied() + reserved() >= HockeyConfig.SEATS

## Forget everyone (session teardown).
func clear() -> void:
	_seat_of_peer.clear()
	_peer_of_seat.clear()
	_session_of_seat.clear()
	_seat_of_session.clear()

# --- internals -------------------------------------------------------------------------------------
func _lowest_free_seat_on(team: int) -> int:
	for seat: int in HockeyConfig.SEATS:
		if HockeyConfig.team_of_seat(seat) != team:
			continue
		# BOTH maps. A seat held for a returning identity is taken, and handing it to a newcomer is exactly
		# the bug the second map exists to prevent.
		if not _peer_of_seat.has(seat) and not _session_of_seat.has(seat):
			return seat
	return -1
