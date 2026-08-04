extends RefCounted
class_name CommandThrottle
## A per-sender token bucket for server-validated commands. Pure: it holds counters and a clock reading it is
## handed, never a timer of its own, so it is unit-testable by passing a synthetic clock.
##
## WHY A SERVER-SIDE THROTTLE IS NOT OPTIONAL. NetCommand.request() is a RELIABLE RPC, and a client controls
## how often it calls one. A client that issues an order every frame -- because it is hostile, or because it
## has a bug, or because it is a bot someone left running -- makes the server validate and apply an order
## every frame, and reliable delivery means the server cannot simply drop the excess at the transport layer.
## The rate limit belongs on the server, keyed by SENDER, and it must exist before the handler does any work
## proportional to the payload.
##
## Rate-limiting a command channel is NOT the same as validating it: this is about how OFTEN, the validator is
## about WHAT. Both are needed, and this one runs first because it is the cheaper rejection.

var _rate_per_second: float = 10.0
var _burst: int = 5
# sender id -> [tokens: float, last_time: float]. Two parallel dictionaries rather than one of arrays: typed
# Dictionary values keep the reads free of Variant unpacking.
var _tokens: Dictionary[int, float] = {}
var _last_time: Dictionary[int, float] = {}

func _init(rate_per_second: float = 10.0, burst: int = 5) -> void:
	_rate_per_second = maxf(0.0001, rate_per_second)
	_burst = maxi(1, burst)

## Consume one token for `sender` at time `now` (seconds, monotonic). Returns true if the command may proceed.
##
## A sender seen for the first time starts with a FULL bucket, so a player's first click is never dropped --
## an empty-start bucket makes the very first action of a session feel broken, which is a bad trade for the
## negligible extra protection.
func allow(sender: int, now: float) -> bool:
	var tokens: float = _tokens.get(sender, float(_burst))
	var last: float = _last_time.get(sender, now)
	var elapsed: float = maxf(0.0, now - last)
	tokens = minf(float(_burst), tokens + elapsed * _rate_per_second)
	_last_time[sender] = now
	if tokens < 1.0:
		_tokens[sender] = tokens
		return false
	_tokens[sender] = tokens - 1.0
	return true

## Drop a sender's bucket (on disconnect), so a long session does not accumulate an entry per peer that ever
## connected.
func forget(sender: int) -> void:
	_tokens.erase(sender)
	_last_time.erase(sender)

## How many senders are currently tracked. Diagnostics, and the leak check in the tests.
func tracked() -> int:
	return _tokens.size()
