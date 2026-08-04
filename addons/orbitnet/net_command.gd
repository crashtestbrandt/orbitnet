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

## Fired on the peer that APPLIED a validated command (server, or local offline) after the handler returned true.
signal applied(verb: StringName, payload: Dictionary)

const _SERVER_PEER_ID: int = 1
const _OFFLINE_SENDER: int = 0   # sentinel sender id used offline (the ownership check is skipped offline)

# verb (StringName) -> Callable(sender_id: int, payload: Dictionary) -> bool. The handler VALIDATES (ownership /
# legality -- a forged or out-of-range request returns false) AND APPLIES (mutates authoritative state) in one
# place, so an unvalidated request can never reach the state.
var _handlers: Dictionary[StringName, Callable] = {}

# NOTE on the `payload: Dictionary` params below being UNTYPED: a payload crosses the @rpc boundary (_submit),
# where Godot decodes it as a PLAIN (untyped) Dictionary -- a `Dictionary[String, Variant]` annotation would
# REJECT the wire-decoded value. The handler reads its fields into typed locals (`var index: int = index_v`),
# which is the allowed Variant -> typed conversion.

## Register the server-side validator+applier for `verb`. Registered on every peer (so the node shape matches),
## but only the applying peer (server / offline) ever invokes it.
func register(verb: StringName, handler: Callable) -> void:
	_handlers[verb] = handler

## Submit a command. OFFLINE applies immediately. As the SERVER (host) it applies locally -- no self-RPC, which
## "call_remote" would drop. As a CLIENT it sends to the server. Call on the OWNING client (the console / authority
## gate ownership at the call site); the server-side ownership check in the handler is the real forgery guard.
func request(verb: StringName, payload: Dictionary) -> void:
	if Net.is_offline():
		_apply(_OFFLINE_SENDER, verb, payload)
	elif multiplayer.is_server():
		_apply(multiplayer.get_unique_id(), verb, payload)
	else:
		rpc_id(_SERVER_PEER_ID, &"_submit", verb, payload)

# Runs on the SERVER (a client's rpc_id(1)). Validates via the registered handler against the REMOTE sender id, so
# a client can neither forge a command on a body it does not own (the handler checks ownership) nor smuggle an
# illegal payload (the handler validates it). A misrouted call to a non-server peer is ignored.
@rpc("any_peer", "call_remote", "reliable")
func _submit(verb: StringName, payload: Dictionary) -> void:
	if not multiplayer.is_server():
		return
	_apply(multiplayer.get_remote_sender_id(), verb, payload)

func _apply(sender_id: int, verb: StringName, payload: Dictionary) -> void:
	var handler: Callable = _handlers.get(verb, Callable())
	if not handler.is_valid():
		return
	if handler.call(sender_id, payload):
		applied.emit(verb, payload)
