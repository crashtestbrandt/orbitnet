extends RefCounted
class_name SeatRoster
## Who sits in which seat. Pure: a map from multiplayer peer id to seat index, with no signals, no tree and
## no transport, so the whole thing is unit-testable.
##
## THE SEAT IS RESOLVED FROM THE SENDER ID, NEVER FROM THE PAYLOAD. That is the entire security model for
## orders and it is worth being blunt about: a client tells the server WHAT it wants to do, and the server
## decides WHO is asking, from `multiplayer.get_remote_sender_id()` -- a value the transport supplies and the
## sender cannot author. If the seat came out of the payload, every ownership check downstream would be
## checking the attacker's own claim about themselves.
##
## OFFLINE uses sender id 0 (NetCommand's offline sentinel) and resolves to seat 0. Single-player is its own
## authority, so there is nobody to lie to -- but the resolution still has to be defined, or the offline path
## would take a different branch through the validator than the networked one, and the offline path is the one
## people develop against.

## NetCommand's sentinel for "applied locally with no session".
const OFFLINE_SENDER: int = 0
## The listen host / dedicated server's own peer id.
const SERVER_PEER: int = 1

var _seat_of_peer: Dictionary[int, int] = {}
var _peer_of_seat: Dictionary[int, int] = {}

## Give `peer` the lowest free seat, or return its existing one if it already holds a seat. -1 when the table
## is full -- the caller refuses the connection rather than seating two peers on one army.
func assign(peer: int) -> int:
	if _seat_of_peer.has(peer):
		return _seat_of_peer[peer]
	for seat: int in RtsConfig.SEATS:
		if not _peer_of_seat.has(seat):
			_seat_of_peer[peer] = seat
			_peer_of_seat[seat] = peer
			return seat
	return -1

## Release `peer`'s seat on disconnect, so a reconnecting player can be seated again.
func release(peer: int) -> void:
	if not _seat_of_peer.has(peer):
		return
	var seat: int = _seat_of_peer[peer]
	_seat_of_peer.erase(peer)
	_peer_of_seat.erase(seat)

## The seat held by `peer`, or -1.
func seat_of_peer(peer: int) -> int:
	return _seat_of_peer.get(peer, -1)

## The peer holding `seat`, or -1.
func peer_of_seat(seat: int) -> int:
	return _peer_of_seat.get(seat, -1)

## Resolve the seat for a command's sender id -- the one function the order path calls.
##
## Offline (sender 0) is seat 0 unconditionally, WITHOUT consulting the table: offline there is no peer list
## to be in, so a table lookup would return -1 and every single-player order would be rejected as unseated.
func seat_for_sender(sender: int) -> int:
	if sender == OFFLINE_SENDER:
		return 0
	return seat_of_peer(sender)

## How many seats are taken.
func occupied() -> int:
	return _seat_of_peer.size()

## Whether every seat is taken (the session is full).
func is_full() -> bool:
	return occupied() >= RtsConfig.SEATS

## Forget everyone (session teardown).
func clear() -> void:
	_seat_of_peer.clear()
	_peer_of_seat.clear()
