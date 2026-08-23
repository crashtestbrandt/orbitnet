extends RefCounted
class_name SeatRoster
## Who drives which fighters. Pure: a map from connection to SEATS, with no signals, no tree and no transport.
##
## THE PLURAL IS THE WHOLE DIFFERENCE FROM THE OTHER TWO DEMOS. Both of those seat one player per connection
## and leave every body at seat 0, which is what a connection gets for free. Here a connection may drive two,
## and each is a seat in the backend's sense: its own interest anchor, its own centre, its own world, its own
## hysteresis band. The connection receives the UNION of its seats' sets, with the nearest seat's distance kept
## per entity -- so what a split-screen player sees on the left half of the screen is not culled around where
## the right half is looking.
##
## A SEAT OUTLIVES THE CONNECTION THAT HELD IT. Two maps, not one: `_peer_of_seat` says who is driving it NOW,
## `_session_of_seat` says which identity may claim it back. A seat is free only when neither names it. Keyed
## on the peer id alone, a returning player lands in whichever seats happen to be free -- in this demo that
## means a different arena, watching a fight they were not in.
##
## THE SEAT IS RESOLVED FROM THE SENDER ID, NEVER FROM THE PAYLOAD -- but a connection with several seats
## cannot resolve a seat from the sender alone, because the sender id names the CONNECTION. So a shot carries
## its seat in the payload and the server validates it against the seats it assigned to that sender.
## `owns_seat()` is that check, and it is the whole security model for the command channel here.

## NetCommand's sentinel for "applied locally with no session".
const OFFLINE_SENDER: int = 0
## The listen host / dedicated server's own peer id.
const SERVER_PEER: int = 1
## "No identity" -- what a local host and a peer that claimed none both present. Such a seat is never held.
const NO_SESSION: int = 0

var _seats_of_peer: Dictionary[int, PackedInt32Array] = {}
var _peer_of_seat: Dictionary[int, int] = {}
var _session_of_seat: Dictionary[int, int] = {}
var _seats_of_session: Dictionary[int, PackedInt32Array] = {}

# --- assignment ------------------------------------------------------------------------------------
## Give `peer` up to `count` seats, or hand back the ones it already holds.
##
## `session_id` reclaims HELD seats when that identity is holding any; the caller decides whether a presented
## identity is worth honouring. `spread` puts the second seat in the NEXT arena rather than beside the first,
## which is the case worth having a flag for: a connection with a body in two worlds has no defined world of
## its own until it declares one.
##
## Returns the seats, lowest first, or an empty array when the table cannot seat it at all.
func assign(peer: int, count: int, session_id: int = NO_SESSION, spread: bool = false) -> PackedInt32Array:
	if _seats_of_peer.has(peer):
		return _seats_of_peer[peer]
	if session_id != NO_SESSION and _seats_of_session.has(session_id):
		var held: PackedInt32Array = _seats_of_session[session_id]
		_take(peer, held)
		_seats_of_session.erase(session_id)
		for seat: int in held:
			_session_of_seat.erase(seat)
		return held

	var wanted: int = clampi(count, 1, ArenaConfig.MAX_SEATS_PER_PEER)
	var taken: PackedInt32Array = PackedInt32Array()
	var first: int = _lowest_free_seat(_thinnest_arena(-1))
	if first < 0:
		return taken
	taken.push_back(first)
	# CLAIMED BEFORE THE SECOND SEARCH, not after both. `_lowest_free_seat()` reads the live tables, so
	# leaving the first seat unclaimed hands the same seat straight back to the search for the second -- and
	# a connection that "drives two fighters" would be driving one fighter twice.
	_take(peer, taken)
	if wanted > 1:
		var arena: int = ArenaConfig.arena_of_seat(first)
		var next_arena: int = _next_arena(arena) if spread else arena
		var second: int = _lowest_free_seat(next_arena)
		if second < 0 and spread:
			# The next arena is full. Falling back beside the first seat is better than refusing the second
			# outright: a split-screen player with one seat is a player with half a screen.
			second = _lowest_free_seat(arena)
		if second >= 0:
			taken.push_back(second)
			_take(peer, taken)
	return taken

## Release `peer`'s seats outright: nobody drives them and nobody may claim them back.
func release(peer: int) -> void:
	if not _seats_of_peer.has(peer):
		return
	var seats: PackedInt32Array = _seats_of_peer[peer]
	_seats_of_peer.erase(peer)
	for seat: int in seats:
		_peer_of_seat.erase(seat)
		if _session_of_seat.has(seat):
			_seats_of_session.erase(_session_of_seat[seat])
			_session_of_seat.erase(seat)

## HOLD `peer`'s seats for `session_id`: nobody drives them, they stay taken, and that identity may take them
## back. Returns the held seats, or an empty array when the peer held none or presented no identity.
##
## An identity of `NO_SESSION` cannot hold anything, and that is a rule rather than an omission: several peers
## present it at once, so seats held under it would go to whichever of them reconnected first.
func hold(peer: int, session_id: int) -> PackedInt32Array:
	if session_id == NO_SESSION or not _seats_of_peer.has(peer):
		return PackedInt32Array()
	var seats: PackedInt32Array = _seats_of_peer[peer]
	_seats_of_peer.erase(peer)
	for seat: int in seats:
		_peer_of_seat.erase(seat)
		_session_of_seat[seat] = session_id
	_seats_of_session[session_id] = seats
	return seats

## Give up held seats: the grace window closed and that identity is not coming back.
func release_session(session_id: int) -> void:
	if not _seats_of_session.has(session_id):
		return
	for seat: int in _seats_of_session[session_id]:
		_session_of_seat.erase(seat)
	_seats_of_session.erase(session_id)

# --- lookups ---------------------------------------------------------------------------------------
## The seats `peer` drives, lowest first. Empty for a peer that drives none, which an observer is.
func seats_of_peer(peer: int) -> PackedInt32Array:
	var seats: PackedInt32Array = _seats_of_peer.get(peer, PackedInt32Array())
	return seats

## The peer driving `seat`, or -1. A HELD seat answers -1: nobody is driving it.
func peer_of_seat(seat: int) -> int:
	return _peer_of_seat.get(seat, -1)

## Whether `sender` was assigned `seat` -- the check every command goes through.
##
## OFFLINE (sender 0) owns seat 0 unconditionally, WITHOUT consulting the table: offline there is no peer list
## to be in, so a lookup would answer false and every single-player shot would be refused as unowned.
func owns_seat(sender: int, seat: int) -> bool:
	if sender == OFFLINE_SENDER:
		return seat == 0
	# Through a typed local: `Dictionary.get()` answers a Variant (the default argument widens it), and
	# calling a method on one is a parse error under this project's promoted warnings.
	var seats: PackedInt32Array = _seats_of_peer.get(sender, PackedInt32Array())
	return seats.has(seat)

## Which of a connection's seats `seat` is -- 0 for the first, 1 for the second. -1 when that peer does not
## hold it. This is the index handed to `NetRollbackHandle.set_seat()`, and it is per CONNECTION rather than
## global: the backend's seat index says which of this connection's bodies it is, not which of the session's.
func seat_index_for_peer(peer: int, seat: int) -> int:
	var seats: PackedInt32Array = seats_of_peer(peer)
	for index: int in seats.size():
		if seats[index] == seat:
			return index
	return -1

## The seats `session_id` is holding, or empty.
func seats_of_session(session_id: int) -> PackedInt32Array:
	var seats: PackedInt32Array = _seats_of_session.get(session_id, PackedInt32Array())
	return seats

## How many seats are being driven right now.
func occupied() -> int:
	return _peer_of_seat.size()

## How many are being kept for identities that are not currently connected.
func reserved() -> int:
	return _session_of_seat.size()

## How many of `arena_id`'s seats are unavailable -- driven or held. Both count: a seat waiting for its player
## to return is not a seat a newcomer may have.
func taken_in_arena(arena_id: int) -> int:
	var count: int = 0
	var first: int = ArenaConfig.first_seat_of_arena(arena_id)
	for offset: int in ArenaConfig.SEATS_PER_ARENA:
		if _is_taken(first + offset):
			count += 1
	return count

func is_full() -> bool:
	return occupied() + reserved() >= ArenaConfig.SEAT_COUNT

## The whole seat table as an owner-per-seat array, for the roster broadcast. `0` means nobody, which every
## peer resolves to the server.
func seat_owners() -> PackedInt32Array:
	var owners: PackedInt32Array = PackedInt32Array()
	owners.resize(ArenaConfig.SEAT_COUNT)
	for seat: int in ArenaConfig.SEAT_COUNT:
		var owner_peer: int = _peer_of_seat.get(seat, 0)
		owners[seat] = maxi(0, owner_peer)
	return owners

## Forget everyone (session teardown).
func clear() -> void:
	_seats_of_peer.clear()
	_peer_of_seat.clear()
	_session_of_seat.clear()
	_seats_of_session.clear()

# --- internals -------------------------------------------------------------------------------------
func _take(peer: int, seats: PackedInt32Array) -> void:
	_seats_of_peer[peer] = seats
	for seat: int in seats:
		_peer_of_seat[seat] = peer

func _is_taken(seat: int) -> bool:
	return _peer_of_seat.has(seat) or _session_of_seat.has(seat)

## The arena with the fewest seats taken, ties to the lowest id. `avoid` is skipped unless every other arena
## is full.
func _thinnest_arena(avoid: int) -> int:
	var best: int = -1
	var best_taken: int = ArenaConfig.SEATS_PER_ARENA + 1
	for offset: int in ArenaConfig.ARENAS:
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		if arena == avoid:
			continue
		var taken: int = taken_in_arena(arena)
		if taken < best_taken:
			best_taken = taken
			best = arena
	if best < 0 or best_taken >= ArenaConfig.SEATS_PER_ARENA:
		return avoid if avoid > 0 and taken_in_arena(avoid) < ArenaConfig.SEATS_PER_ARENA else best
	return best

func _next_arena(arena_id: int) -> int:
	var offset: int = arena_id - ArenaConfig.FIRST_ARENA_ID
	return ArenaConfig.FIRST_ARENA_ID + (offset + 1) % ArenaConfig.ARENAS

## The lowest free seat in `arena_id`, preferring the thinner team. -1 when the arena is full, or when
## `arena_id` is not an arena.
func _lowest_free_seat(arena_id: int) -> int:
	if not ArenaConfig.is_arena(arena_id):
		return -1
	var first: int = ArenaConfig.first_seat_of_arena(arena_id)
	var preferred: int = 0 if _team_taken(arena_id, 0) <= _team_taken(arena_id, 1) else 1
	for pass_index: int in 2:
		var team: int = preferred if pass_index == 0 else 1 - preferred
		for offset: int in ArenaConfig.SEATS_PER_ARENA:
			var seat: int = first + offset
			if ArenaConfig.team_of_seat(seat) == team and not _is_taken(seat):
				return seat
	return -1

func _team_taken(arena_id: int, team: int) -> int:
	var count: int = 0
	var first: int = ArenaConfig.first_seat_of_arena(arena_id)
	for offset: int in ArenaConfig.SEATS_PER_ARENA:
		var seat: int = first + offset
		if ArenaConfig.team_of_seat(seat) == team and _is_taken(seat):
			count += 1
	return count
