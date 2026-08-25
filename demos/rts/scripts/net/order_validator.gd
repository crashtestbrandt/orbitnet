extends RefCounted
class_name OrderValidator
## The server's adjudication of one order, as a PURE function of (sender's seat, wire payload, world state).
##
## Every field here arrives from a client over a reliable RPC, which means every field is attacker-controlled.
## The whole validator is one static call so it can be unit-tested against hand-written hostile payloads with
## no session, no server and no units -- which is the only way anyone actually writes the malicious cases.
##
## THE FOUR RULES, and why each is the shape it is:
##
##   1. FOREIGN-SEAT IDS REJECT THE WHOLE BATCH. If a payload names one unit the sender does not own, the
##      entire order is refused -- not filtered down to the legal subset. Filtering would let an attacker
##      probe ownership by watching which of a mixed batch moved, and there is no honest client that ever
##      sends one: your own selection can only contain your own units. A foreign id is a forgery attempt, and
##      the correct response to a forgery attempt is that nothing happens.
##
##   2. DEAD IDS ARE SILENTLY DROPPED. This looks like the same case and is the opposite one. A unit dying
##      between the client's click and the server's receipt is an ordinary RACE that happens constantly at
##      any latency; rejecting the batch would mean a player's orders randomly fail during a firefight.
##      Ownership is a permission question, liveness is a timing question, and conflating them produces
##      either a security hole or an infuriating game.
##
##   3. CARDINALITY IS CAPPED. Unbounded ids per packet is unbounded server work per packet.
##
##   4. EVERY Vector3 COMPONENT MUST BE FINITE. A wire-decoded NAN is a classic crash-and-corrupt vector: it
##      propagates silently through every arithmetic operation it touches, never compares equal to anything,
##      and surfaces as a unit at an undefined position far from the code that admitted it. It is rejected at
##      the boundary, and UnitSteering absorbs it again in depth.

## The known verbs. StringName so the comparison is a pointer compare, and a fixed set so an unknown verb is
## rejected rather than silently ignored by a missing handler.
const VERB_MOVE: StringName = &"move"
const VERB_ATTACK_MOVE: StringName = &"attack_move"
const VERB_STOP: StringName = &"stop"
const VERB_HOLD: StringName = &"hold"

## Why an order was refused, as one number per rule.
##
## `OK` is 0 because that is what [NetCommand] reads as acceptance -- a validator that returns an int states
## the reason, and 0 states that there is none. An enum starting at 1 would refuse every order it accepted.
##
## THE CODE CROSSES THE WIRE, THE SENTENCE DOES NOT. `Result.reason` names the ids and seats involved, which
## is what a server log wants and what a client must never be handed: it is server-side knowledge about units
## the asker may not own. The client is told the code and says its own sentence with [method describe].
##
## Two of these are decided by WorldDirector rather than here -- the rate limit and the wrong-channel check
## both need session state a pure validator does not have -- but they are named in this one table so a HUD has
## a single vocabulary to read.
enum Code {
	OK = 0,
	NO_SEAT = 1,
	UNKNOWN_VERB = 2,
	MALFORMED_IDS = 3,
	TOO_MANY_IDS = 4,
	POINT_NOT_FINITE = 5,
	ID_OUT_OF_RANGE = 6,
	FOREIGN_ID = 7,
	RATE_LIMITED = 8,
	FOREIGN_CHANNEL = 9,
}

## One sentence per code, for a HUD. Static and pure, and it names no id and no seat -- a client that received
## the code says the same thing whatever the server knew.
static func describe(code: int) -> String:
	match code:
		Code.NO_SEAT:
			return "the sender holds no seat"
		Code.UNKNOWN_VERB:
			return "unknown order"
		Code.MALFORMED_IDS:
			return "empty or malformed id list"
		Code.TOO_MANY_IDS:
			return "too many units in one order"
		Code.POINT_NOT_FINITE:
			return "the target point is not finite"
		Code.ID_OUT_OF_RANGE:
			return "an id is out of range"
		Code.FOREIGN_ID:
			return "an ordered unit is not yours"
		Code.RATE_LIMITED:
			return "you are ordering too fast"
		Code.FOREIGN_CHANNEL:
			return "that is not your channel"
		_:
			return ""

## The outcome. Carries the sanitized values, so a caller that got `accepted` never re-reads the raw payload
## -- which is what stops an unvalidated field sneaking through behind a validated one.
class Result extends RefCounted:
	var accepted: bool = false
	var code: int = Code.OK            # what the requester is told
	var reason: String = ""            # human-readable and detailed, for the server log and the tests
	var verb: StringName = &""
	var ids: PackedInt32Array = PackedInt32Array()   # owned, alive, de-duplicated, in payload order
	var point: Vector3 = Vector3.ZERO
	var dropped_dead: int = 0          # how many ids rule 2 removed (diagnostics; not an error)

	func _init(why: int, detail: String) -> void:
		code = why
		accepted = why == Code.OK
		reason = detail

## Validate one order.
##
## `sender_seat` is the seat the SERVER resolved for the sending peer (never a value from the payload -- see
## SeatRoster). `payload` is the raw wire Dictionary. `alive` is indexed by unit id, non-zero meaning living.
static func validate(sender_seat: int, verb: StringName, payload: Dictionary,
		alive: PackedByteArray) -> Result:
	if sender_seat < 0 or sender_seat >= RtsConfig.SEATS:
		return Result.new(Code.NO_SEAT, "sender holds no seat")
	if not _is_known_verb(verb):
		return Result.new(Code.UNKNOWN_VERB, "unknown verb '%s'" % verb)

	var raw_ids: PackedInt32Array = _read_ids(payload)
	if raw_ids.is_empty():
		return Result.new(Code.MALFORMED_IDS, "empty or malformed id list")
	if raw_ids.size() > RtsConfig.MAX_ORDER_IDS:
		return Result.new(Code.TOO_MANY_IDS,
			"id list of %d exceeds the cap of %d" % [raw_ids.size(), RtsConfig.MAX_ORDER_IDS])

	var point: Vector3 = _read_point(payload)
	if not UnitSteering.is_finite_vec(point):
		return Result.new(Code.POINT_NOT_FINITE, "target point is not finite")
	# STOP and HOLD carry no destination, so a point is meaningless for them; normalize it away rather than
	# leaving a value the caller might act on.
	if verb == VERB_STOP or verb == VERB_HOLD:
		point = Vector3.ZERO
	else:
		point = UnitSteering.clamp_to_field(point, 0.0)

	var kept: PackedInt32Array = PackedInt32Array()
	var seen: Dictionary[int, bool] = {}
	var dropped: int = 0
	for id: int in raw_ids:
		if not RtsConfig.is_valid_id(id):
			# Out of range is not a race -- no honest client can produce it. Rule 1.
			return Result.new(Code.ID_OUT_OF_RANGE, "id %d is out of range" % id)
		if RtsConfig.seat_of(id) != sender_seat:
			return Result.new(Code.FOREIGN_ID, "id %d belongs to seat %d, sender holds seat %d"
				% [id, RtsConfig.seat_of(id), sender_seat])
		if seen.has(id):
			continue   # a duplicate is harmless; collapse it rather than doing the work twice
		seen[id] = true
		if id >= alive.size() or alive[id] == 0:
			dropped += 1   # Rule 2
			continue
		kept.push_back(id)

	var result: Result = Result.new(Code.OK, "ok" if not kept.is_empty() else "every named unit is dead")
	result.verb = verb
	result.ids = kept
	result.point = point
	result.dropped_dead = dropped
	return result

static func _is_known_verb(verb: StringName) -> bool:
	return verb == VERB_MOVE or verb == VERB_ATTACK_MOVE or verb == VERB_STOP or verb == VERB_HOLD

## Read the destination out of a wire payload. A missing or wrong-typed field yields ZERO, which the finite
## check then passes and the field clamp keeps in bounds -- so a malformed point degrades to "the middle of
## the map", never to an exception. Note the typed local: a wire-decoded value is a Variant, and the typed
## GDScript rules ban as-casting one.
static func _read_point(payload: Dictionary) -> Vector3:
	if not payload.has("point"):
		return Vector3.ZERO
	var raw: Variant = payload["point"]
	if raw is Vector3:
		var out: Vector3 = raw
		return out
	return Vector3.ZERO

## Read the id list out of a wire payload.
##
## Accepts either a PackedInt32Array (what an honest client sends) or a plain Array of ints (what a
## hand-written payload or another language's client might send), and returns empty for anything else. It
## must not assume the type: the payload crosses an @rpc boundary, where Godot decodes containers generically,
## and indexing a String as if it were an array is exactly the sort of thing that turns a malformed packet
## into a server crash.
static func _read_ids(payload: Dictionary) -> PackedInt32Array:
	if not payload.has("ids"):
		return PackedInt32Array()
	var raw: Variant = payload["ids"]
	if raw is PackedInt32Array:
		var packed: PackedInt32Array = raw
		return packed
	if raw is Array:
		var generic: Array = raw
		var out: PackedInt32Array = PackedInt32Array()
		for entry: Variant in generic:
			if entry is int:
				var value: int = entry
				out.push_back(value)
			else:
				return PackedInt32Array()   # a mixed-type list is malformed, not partially valid
		return out
	return PackedInt32Array()
