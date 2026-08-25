extends Node
class_name NetCommand
## Server-validated command channel. The owning client submits a command; it runs on the SERVER, which
## validates that the sender is allowed to issue it and that it is legal, then applies it to authoritative
## state -- which replicates back to every peer through the normal state sync. OFFLINE applies immediately
## (single-player is its own authority). This is the "client requests, server adjudicates" seam.
##
## THIS IS THE THIRD LANE, and it is not interchangeable with the other two. The rollback lane is for a
## CONTINUOUS per-tick input stream that can be predicted and replayed; the state lane is for authoritative
## values pushed out every tick. A command is neither -- it is a SPARSE, discrete request ("equip slot 2",
## "move these units there") that arrives whenever the player acts. Sending it as rollback input would mean
## predicting something with no prediction model; sending it as state would mean finding somewhere to store a
## verb. It gets its own reliable channel, and the important consequence is that a handler runs OUTSIDE the
## tick: a value it writes must live on the STATE lane, not the rollback lane, or the next rollback restore
## will overwrite it with recorded history.
##
## A subsystem registers its verbs and grows by adding more -- each with its own server-side validator, so the
## whole surface shares one audited request path instead of scattering ad-hoc @rpc methods.
##
## Created as a child of the requesting subsystem with a STABLE name (the @rpc routes by node path, so every
## peer must build it identically). Validators run only on the applying peer (server, or the local peer
## offline). No rollback-backend symbols (the `just net-check` gate): plain Godot @rpc plus the `Net` facade
## for the offline / role checks.
##
## A COMMAND IS PER CONNECTION, NOT PER SEAT, AND THAT IS A DECISION RATHER THAN A GAP.
##
## A seat is one owned, predicted body behind a connection -- local split-screen is two or more on one socket,
## and the interest pass keys an anchor on `(peer, seat)`. The handler is still handed one identity, the sender
## id, because that is the only identity the transport supplies and therefore the only one a client cannot
## author. A per-seat sender id would have to be carried in the payload, where it is the client's own claim
## about itself, and every ownership check downstream would then be checking the attacker's claim.
##
## So a game with several seats on one connection DISAMBIGUATES INSIDE THE PAYLOAD, and its validator checks
## the claimed seat against the seats the SERVER assigned to that sender -- the server assigned them, so it can.
## An unchecked seat field in a payload is a forged order on somebody else's units, and it is the one new
## mistake this shape makes available.
##
## Nothing here changes for a game with one seat per connection: the sender id resolves to exactly one seat,
## which is what the demos' roster is.
##
## A REFUSAL REACHES THE REQUESTING CLIENT ONLY IF THE VALIDATOR RETURNS AN INT -- see [signal rejected] and
## [method register]. A validator that returns `bool` behaves exactly as it did before that signal existed.

## Fired on the peer that APPLIED a validated command (server, or local offline) after the handler accepted it.
signal applied(verb: StringName, payload: Dictionary)

## Fired on the peer that REFUSED a command -- and, when the requester was a remote client, on that client too.
##
## `code` is the game's own reason, carried verbatim and never interpreted here. `tag` is the value
## [method request] returned for that request, so a client's UI cancels exactly the request that was refused
## instead of guessing by verb. It is `0` for a refusal no `request()` on this peer produced -- which is what a
## SERVER watching its own refusals sees, including for a command a client sent under a tag of its own. Tags
## are minted per peer, so announcing a client's tag locally would let a server match somebody else's number
## against one of its own.
##
## IT FIRES DURING request() ON THE PEER THAT APPLIES. Offline, and on a host submitting its own command, the
## handler runs inside the call -- so a listener is invoked BEFORE [method request] has returned its tag, and
## code that stores the tag after the call has not stored it yet. Record the refused tag in the handler and
## check it against the tag the call returns, rather than the other way round:
##
## [codeblock]
##     func _on_rejected(_verb: StringName, code: int, tag: int) -> void:
##         _last_refused = tag                       # may arrive before the line below runs
##
##     var tag: int = lane.request(&"fire", {})
##     if tag != 0 and tag == _last_refused:
##         return                                    # refused already, on this peer
## [/codeblock]
##
## A refusal that came back from a remote server always arrives after the call, because it is a reply.
##
## ONLY AN INT VERDICT PRODUCES THIS. A validator returning `false` refuses exactly as it always did: silently,
## with no signal and no reply. That is deliberate rather than an oversight -- a reply is one reliable packet
## per refused request and the client chooses the rate, so a rate-limit refusal should stay silent while a
## "that slot is full" refusal should not, and the validator is the only code that knows which is which.
signal rejected(verb: StringName, code: int, tag: int)

const _SERVER_PEER_ID: int = 1
const _OFFLINE_SENDER: int = 0   # sentinel sender id used offline (the ownership check is skipped offline)

## The verdict meaning "applied". A validator returning `0` accepted the request; see [method register].
const CODE_OK: int = 0

## The lane's OWN refusals are NEGATIVE, so they cannot collide with a game's reason codes -- which start at
## [constant CODE_OK] and count up, because that is the shape an enum with `OK = 0` already has.
const CODE_BATCH_TOO_LARGE: int = -1
const CODE_BATCH_MALFORMED: int = -2

## Payloads one [method request_batch] may carry. Enforced on BOTH sides: a client refuses to send a longer
## batch, and a server refuses one that arrives anyway -- WHOLE, never trimmed to a legal prefix, because
## applying the first sixteen of twenty applies a request the caller never separated out.
const MAX_BATCH: int = 16

# verb (StringName) -> Callable(sender_id: int, payload: Dictionary) -> bool OR int. The handler VALIDATES
# (ownership / legality -- a forged or out-of-range request is refused) AND APPLIES (mutates authoritative
# state) in one place, so an unvalidated request can never reach the state.
var _handlers: Dictionary[StringName, Callable] = {}

# The tag minted for the next request on this peer. Never 0: 0 is what `rejected` carries for a refusal this
# peer did not ask for, so it must not also name a real request.
var _next_tag: int = 0

# NOTE on the `payload: Dictionary` / `payloads: Array` params below being element-UNTYPED: both cross the
# @rpc boundary (_submit / _submit_batch), where Godot decodes them as PLAIN containers -- a
# `Dictionary[String, Variant]` annotation would REJECT the wire-decoded value. The handler reads its fields
# into typed locals (`var index: int = index_v`), which is the allowed Variant -> typed conversion.

## Register the server-side validator+applier for `verb`. Registered on every peer (so the node shape matches),
## but only the applying peer (server / offline) ever invokes it.
##
## THE RETURN VALUE DECIDES BOTH THE OUTCOME AND WHETHER THE REQUESTER HEARS ABOUT IT:
##
## [codeblock]
##   returns             outcome                       reply to the requester
##   true                applied                       none
##   false               refused                       none -- silent, exactly as it always was
##   int CODE_OK (0)     applied                       none
##   int, non-zero       refused, carrying that code   [signal rejected], on both peers
##   anything else       refused                       none
## [/codeblock]
##
## A validator declared `-> bool` therefore behaves identically to before this signal existed, byte for byte
## and packet for packet. A game opts into feedback by declaring it `-> int` and returning its own reason enum,
## whose `OK` member must be `0`.
##
## KEEP THE SILENT REFUSAL FOR A RATE LIMIT. Returning `false` from the throttle branch and an int everywhere
## else is what stops a spamming client buying server upstream, one reliable packet per request.
func register(verb: StringName, handler: Callable) -> void:
	_handlers[verb] = handler

## Submit a command, and answer the TAG that identifies it in [signal rejected]. OFFLINE applies immediately.
## As the SERVER (host) it applies locally -- no self-RPC, which "call_remote" would drop. As a CLIENT it sends
## to the server. Call on the OWNING client (the console / authority gate ownership at the call site); the
## server-side ownership check in the handler is the real forgery guard.
##
## The tag is minted per peer and is never `0`. It is not an identity and nothing downstream trusts it -- it is
## an opaque correlation number the server quotes back with the refusal.
func request(verb: StringName, payload: Dictionary) -> int:
	var tag: int = _mint_tag()
	# `multiplayer` is null on a node outside the SceneTree. Offline that branch is never reached; online it is
	# a node the game forgot to add, and answering "nothing was sent" beats erroring inside a request.
	if Net.is_offline() or multiplayer == null:
		_apply(_OFFLINE_SENDER, verb, payload, tag)
	elif multiplayer.is_server():
		_apply(multiplayer.get_unique_id(), verb, payload, tag)
	else:
		rpc_id(_SERVER_PEER_ID, &"_submit", verb, payload, tag)
	return tag

## Submit several payloads for ONE verb in a single reliable packet, and answer one tag per payload, in order.
##
## A BATCH IS A COALESCING OPTIMIZATION, NOT A TRANSACTION. Each payload is validated and applied
## independently: [signal applied] fires once per accepted payload, and the refusals are coalesced into one
## reply. Nothing is rolled back because a later entry was refused.
##
## ONE VERB PER BATCH rather than an array of `(verb, payload)` pairs: the verb is what a channel registers
## against, every real batch is homogeneous, the handler is resolved once instead of once per entry, and no
## per-entry verb rides the wire. A mixed flush is one call per verb -- packets in the number of verbs, not of
## requests.
##
## Over [constant MAX_BATCH] the batch is refused WHOLE with [constant CODE_BATCH_TOO_LARGE] and nothing is
## sent.
##
## IT IS NOT A RATE-LIMIT BYPASS: the lane charges nothing itself, so a game's per-sender throttle still runs
## once per entry.
func request_batch(verb: StringName, payloads: Array) -> PackedInt32Array:
	var tags: PackedInt32Array = PackedInt32Array()
	if payloads.is_empty():
		return tags
	for _i: int in payloads.size():
		tags.push_back(_mint_tag())
	if payloads.size() > MAX_BATCH:
		_refuse_locally(verb, CODE_BATCH_TOO_LARGE, tags)
		return tags
	if Net.is_offline() or multiplayer == null:
		_apply_batch(_OFFLINE_SENDER, verb, payloads, tags)
	elif multiplayer.is_server():
		_apply_batch(multiplayer.get_unique_id(), verb, payloads, tags)
	else:
		rpc_id(_SERVER_PEER_ID, &"_submit_batch", verb, payloads, tags)
	return tags

# Runs on the SERVER (a client's rpc_id(1)). Validates via the registered handler against the REMOTE sender id, so
# a client can neither forge a command on a body it does not own (the handler checks ownership) nor smuggle an
# illegal payload (the handler validates it). A misrouted call to a non-server peer is ignored.
@rpc("any_peer", "call_remote", "reliable")
func _submit(verb: StringName, payload: Dictionary, tag: int) -> void:
	if multiplayer == null or not multiplayer.is_server():
		return
	var sender: int = multiplayer.get_remote_sender_id()
	# ANNOUNCED LOCALLY UNDER 0, REPLIED UNDER THE SENDER'S OWN TAG. The number came off the wire and names a
	# request in the SENDER's numbering; a server that announced it as its own could match it against one of
	# its own pending tags by coincidence.
	var code: int = _apply(sender, verb, payload, 0)
	if code != CODE_OK:
		_reply(sender, verb, PackedInt32Array([code]), PackedInt32Array([tag]))

## One refusal code per tag -- the shape [method _announce_refusals] reads.
##
## THE HALVES MUST BE THE SAME LENGTH. The announce step reads the two arrays in lockstep and drops a frame
## whose halves disagree, so a single code sent beside N tags is refused by the REQUESTER's own guard: the
## batch is rejected in silence, with no reason code and every minted tag left outstanding. A refusal that
## names one code for the whole batch still has to spell it against each tag.
static func uniform_codes(code: int, count: int) -> PackedInt32Array:
	var codes: PackedInt32Array = PackedInt32Array()
	if count <= 0:
		return codes
	codes.resize(count)
	codes.fill(code)
	return codes

# The batch counterpart. THE CAP IS RE-CHECKED HERE and not only in request_batch(): the client-side check is a
# courtesy to an honest caller, and this one is the rule.
@rpc("any_peer", "call_remote", "reliable")
func _submit_batch(verb: StringName, payloads: Array, tags: PackedInt32Array) -> void:
	if multiplayer == null or not multiplayer.is_server():
		return
	var sender: int = multiplayer.get_remote_sender_id()
	if payloads.size() != tags.size():
		_reply(sender, verb, PackedInt32Array([CODE_BATCH_MALFORMED]), PackedInt32Array([0]))
		return
	if payloads.size() > MAX_BATCH:
		_reply(sender, verb, uniform_codes(CODE_BATCH_TOO_LARGE, tags.size()), tags)
		return
	_apply_batch(sender, verb, payloads, tags)

# Runs on the CLIENT that made the request. The "authority" transfer mode already restricts the caller to the
# server, and the sender is checked again anyway: a transfer mode is a routing declaration, and what is being
# stopped here is one client driving another client's UI.
@rpc("authority", "call_remote", "reliable")
func _refused(verb: StringName, codes: PackedInt32Array, tags: PackedInt32Array) -> void:
	# `multiplayer` is null on a node outside the SceneTree, and an RPC never reaches one -- so the null arm
	# is not a routing case, it is what makes the guard total rather than resting on a premise held elsewhere.
	if multiplayer == null or multiplayer.get_remote_sender_id() != _SERVER_PEER_ID:
		return
	_announce_refusals(verb, codes, tags)

# The announce half, split out from the routing guards above so it is reachable without a session -- the guards
# need a live MultiplayerAPI and this rule does not.
func _announce_refusals(verb: StringName, codes: PackedInt32Array, tags: PackedInt32Array) -> void:
	# The two arrays are read in lockstep. A frame whose halves disagree is corrupt or hostile, and reading
	# past the shorter one is the bug it would cause.
	if codes.size() != tags.size():
		return
	for i: int in codes.size():
		rejected.emit(verb, codes[i], tags[i])

# Never 0 and never negative: 0 is the "no request() on this peer produced this" sentinel `rejected` carries,
# so it must not also name a real request.
func _mint_tag() -> int:
	_next_tag = (_next_tag % 0x7FFF_FFFF) + 1
	return _next_tag

# Announce a refusal on THIS peer and send nothing -- the offline / host-local path, and the over-the-cap path
# where no packet is worth spending.
func _refuse_locally(verb: StringName, code: int, tags: PackedInt32Array) -> void:
	for tag: int in tags:
		rejected.emit(verb, code, tag)

# Send the refused pairs to the requester, when the requester is a remote client. The applying peer has already
# announced them locally from _apply().
func _reply(sender_id: int, verb: StringName, codes: PackedInt32Array, tags: PackedInt32Array) -> void:
	if multiplayer == null or sender_id == _OFFLINE_SENDER or sender_id == multiplayer.get_unique_id():
		return
	rpc_id(sender_id, &"_refused", verb, codes, tags)

func _apply_batch(sender_id: int, verb: StringName, payloads: Array, tags: PackedInt32Array) -> void:
	var codes: PackedInt32Array = PackedInt32Array()
	var refused_tags: PackedInt32Array = PackedInt32Array()
	# A remote sender's tags name requests in ITS numbering, so they are replied with and never announced as
	# this peer's own. See _submit().
	var local: bool = sender_id == _OFFLINE_SENDER or (multiplayer != null and sender_id == multiplayer.get_unique_id())
	for i: int in payloads.size():
		var entry: Variant = payloads[i]
		# A non-Dictionary entry is a wire-decoded value that is not what the schema says, and it is DROPPED
		# rather than erroring -- the same fail-quiet an unregistered verb takes.
		if typeof(entry) != TYPE_DICTIONARY:
			continue
		var payload: Dictionary = entry
		var code: int = _apply(sender_id, verb, payload, tags[i] if local else 0)
		if code != CODE_OK:
			codes.push_back(code)
			refused_tags.push_back(tags[i])
	if not codes.is_empty():
		_reply(sender_id, verb, codes, refused_tags)

# Returns the REASON CODE the requester should be told, and [constant CODE_OK] when there is nothing to tell
# it. That covers two different outcomes on purpose: a command that was applied, and a refusal the validator
# expressed as `false` -- which refuses the request and carries no code, so it announces nothing and replies
# nothing. See register() for the whole table.
func _apply(sender_id: int, verb: StringName, payload: Dictionary, tag: int = 0) -> int:
	var handler: Callable = _handlers.get(verb, Callable())
	if not handler.is_valid():
		return CODE_OK
	var verdict: Variant = handler.call(sender_id, payload)
	match typeof(verdict):
		TYPE_BOOL:
			var accepted: bool = verdict
			if accepted:
				applied.emit(verb, payload)
			return CODE_OK
		TYPE_INT:
			var code: int = verdict
			if code == CODE_OK:
				applied.emit(verb, payload)
				return CODE_OK
			rejected.emit(verb, code, tag)
			return code
		_:
			return CODE_OK
