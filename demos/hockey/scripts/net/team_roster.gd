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
## THE SEAT IS RESOLVED FROM THE SENDER ID, NEVER FROM THE PAYLOAD. That is the whole security model for the
## command channel and it is worth being blunt about: a client says WHAT it wants, and the server decides WHO
## is asking, from `multiplayer.get_remote_sender_id()` -- a value the transport supplies and the sender cannot
## author. If the seat came out of the payload, every ownership check downstream would be checking the
## attacker's own claim about themselves.

## NetCommand's sentinel for "applied locally with no session".
const OFFLINE_SENDER: int = 0
## The listen host / dedicated server's own peer id.
const SERVER_PEER: int = 1

var _seat_of_peer: Dictionary[int, int] = {}
var _peer_of_seat: Dictionary[int, int] = {}

## Seat `peer` on the end with fewer players, or return its existing seat if it already holds one. -1 when
## every seat is taken -- with HockeyConfig.SEATS seats and the transport capped at the same number, the
## caller reaches that only if the two are ever allowed to disagree.
func assign(peer: int) -> int:
	if _seat_of_peer.has(peer):
		return _seat_of_peer[peer]
	var preferred: int = 0 if occupied_on_team(0) <= occupied_on_team(1) else 1
	var seat: int = _lowest_free_seat_on(preferred)
	if seat < 0:
		seat = _lowest_free_seat_on(1 - preferred)
	if seat < 0:
		return -1
	_seat_of_peer[peer] = seat
	_peer_of_seat[seat] = peer
	return seat

## Release `peer`'s seat on disconnect, so the seat is reused and a reconnecting player can be seated again.
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

## How many of `team`'s seats are taken.
func occupied_on_team(team: int) -> int:
	var count: int = 0
	for seat: int in _peer_of_seat.keys():
		if HockeyConfig.team_of_seat(seat) == team:
			count += 1
	return count

## Whether every seat is taken.
func is_full() -> bool:
	return occupied() >= HockeyConfig.SEATS

## Forget everyone (session teardown).
func clear() -> void:
	_seat_of_peer.clear()
	_peer_of_seat.clear()

# --- internals -------------------------------------------------------------------------------------
func _lowest_free_seat_on(team: int) -> int:
	for seat: int in HockeyConfig.SEATS:
		if HockeyConfig.team_of_seat(seat) != team:
			continue
		if not _peer_of_seat.has(seat):
			return seat
	return -1
