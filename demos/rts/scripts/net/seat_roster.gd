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
##
## THIS TABLE IS A BIJECTION, AND THAT IS THIS DEMO'S CHOICE RATHER THAN THE BACKEND'S. The backend seats a
## body, not a connection: `NetRollbackHandle.set_seat()` lets one connection drive several owned bodies, each
## with its own interest anchor, which is what local split-screen needs. This demo seats one player per peer and
## leaves every body at seat 0, so `assign()` refusing a second seat to the same peer is a rule about THIS game.
##
## A game that does hold several seats on one connection cannot resolve a seat from the sender id alone, because
## the sender id names the connection. It carries the seat in the command payload and validates it against the
## seats the SERVER assigned to that sender -- which keeps the security model intact, since the server assigned
## them. What must never happen is trusting the payload's seat unchecked; that is a forged order on somebody
## else's units.
##
## A SEAT IS OWNED BY A SESSION AND OCCUPIED BY A PEER. Those are two different facts and the reconnect case is
## where the difference shows: the peer id is the connection, reassigned every time a player dials back in,
## while the session identity (`Net.peer_session_id`) is the player and survives the drop. A rejoiner presenting
## its identity reclaims the seat it left; a newcomer takes the lowest seat no session owns. A roster keyed on
## peer id alone hands the returning player whichever seat happens to be free, which is how somebody comes back
## to a stranger's army.

## NetCommand's sentinel for "applied locally with no session".
const OFFLINE_SENDER: int = 0
## The listen host / dedicated server's own peer id.
const SERVER_PEER: int = 1
## "No session identity" -- what a local host and a peer that claimed none both present. Such a seat is never
## reclaimable, which is correct: there is nothing to recognise it by.
const NO_SESSION: int = 0

var _seat_of_peer: Dictionary[int, int] = {}
var _peer_of_seat: Dictionary[int, int] = {}
## Which session OWNS each seat, and the reverse. Kept across a disconnect -- that is the whole point -- and
## dropped only by release()/release_session()/clear().
var _session_of_seat: Dictionary[int, int] = {}
var _seat_of_session: Dictionary[int, int] = {}

## Seat `peer`, presenting session identity `session_id` (0 = none). Returns the seat, or -1 when the table is
## full -- the caller refuses the connection rather than seating two peers on one army.
##
## Three cases, in order: a peer that already holds a seat keeps it, a session that owns a seat RECLAIMS it
## whatever peer id it now arrives under, and anyone else takes the lowest seat nobody occupies.
func assign(peer: int, session_id: int = NO_SESSION) -> int:
	if _seat_of_peer.has(peer):
		return _seat_of_peer[peer]
	if session_id != NO_SESSION and _seat_of_session.has(session_id):
		var reclaimed: int = _seat_of_session[session_id]
		_bind(reclaimed, peer, session_id)
		return reclaimed
	for seat: int in RtsConfig.SEATS:
		if not _peer_of_seat.has(seat):
			_bind(seat, peer, session_id)
			return seat
	return -1

## Occupy `seat` with `peer`, and record which session owns it.
##
## A reclaimed seat is still bound to the peer id its owner dropped under -- the roster deliberately holds that
## binding through the gap, so the seat reads as taken -- so that binding is dropped here before the new one is
## written, or the departed id would keep resolving to a seat somebody else now occupies.
func _bind(seat: int, peer: int, session_id: int) -> void:
	if _peer_of_seat.has(seat):
		_seat_of_peer.erase(_peer_of_seat[seat])
	_seat_of_peer[peer] = seat
	_peer_of_seat[seat] = peer
	if session_id != NO_SESSION:
		_session_of_seat[seat] = session_id
		_seat_of_session[session_id] = seat

## Release `peer`'s seat outright, session ownership included. For a drop the session layer is NOT holding
## open -- a peer that claimed no identity, or a session whose grace window is disabled.
func release(peer: int) -> void:
	if not _seat_of_peer.has(peer):
		return
	var seat: int = _seat_of_peer[peer]
	_seat_of_peer.erase(peer)
	_peer_of_seat.erase(seat)
	_forget_session_of(seat)

## Release the seat a session owns, whoever is bound to it. This is what a closed grace window means: the
## player is not coming back, so the seat opens to the next arrival.
func release_session(session_id: int) -> void:
	if session_id == NO_SESSION or not _seat_of_session.has(session_id):
		return
	var seat: int = _seat_of_session[session_id]
	if _peer_of_seat.has(seat):
		_seat_of_peer.erase(_peer_of_seat[seat])
		_peer_of_seat.erase(seat)
	_forget_session_of(seat)

func _forget_session_of(seat: int) -> void:
	if not _session_of_seat.has(seat):
		return
	_seat_of_session.erase(_session_of_seat[seat])
	_session_of_seat.erase(seat)

## The session that owns `seat`, or 0 when none does.
func session_of_seat(seat: int) -> int:
	return _session_of_seat.get(seat, NO_SESSION)

## The seat `session_id` owns, or -1. This is the reconnect lookup.
func seat_of_session(session_id: int) -> int:
	return _seat_of_session.get(session_id, -1) if session_id != NO_SESSION else -1

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
	_session_of_seat.clear()
	_seat_of_session.clear()
