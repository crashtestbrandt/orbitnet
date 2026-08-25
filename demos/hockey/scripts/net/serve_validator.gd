extends RefCounted
class_name ServeValidator
## The server-side rules for a serve request, as a pure static function over (sender's seat, puck state). No
## node, no tree, no session -- so the hostile cases are unit-testable.
##
## ONE VERB, ONE CHANNEL. The RTS demo runs one NetCommand per SEAT, and it is right to: an order names unit
## ids, so a request arriving on somebody else's channel is unambiguous forgery, catchable before the payload
## is parsed. A serve names nothing. The sender id is the entire authorization, so a per-seat channel would buy
## thirty-two nodes and thirty-two registrations for a check that has nothing to check.
##
## NO TOKEN BUCKET EITHER, and that is the point worth taking away: a serve is legal ONLY while the puck is
## dead, and serving makes it live, so the state precondition rate-limits the channel by itself and the
## validator's work is O(1) either way. The RTS demo is where the token bucket lives, because an order is legal
## whenever the player likes.
##
## A COMMAND CANNOT BE PREDICTED. The handler runs OUTSIDE the tick, on the server, and writes a server-only
## field no client has -- so a client sees its own serve one round trip after asking for it. That is correct
## rather than a gap: there is nothing in a client's possession to predict it from.

## The verb this channel carries.
const VERB_SERVE: StringName = &"serve"

## Why a serve was refused. `OK` is 0 because that is what [NetCommand] reads as acceptance: a validator
## returning an int states the reason, and 0 states that there is none.
##
## THE CODE CROSSES THE WIRE, THE STRING DOES NOT. A refusal reaches the requesting client as this int and the
## client turns it into words with [method describe]. Sending the sentence instead would put presentation
## bytes on a reliable channel and hand the server the client's language.
enum Code {
	OK = 0,
	NO_SEAT = 1,
	PUCK_LIVE = 2,
}

## A validation outcome. `code` is what the requester is told; `reason` is the same fact in words, for a human
## reading the HUD's rejection line -- because a demo that never shows you a refused request has not shown you
## the security model.
class Result extends RefCounted:
	var accepted: bool = false
	var code: int = Code.OK
	var reason: String = ""

	func _init(why: int) -> void:
		code = why
		accepted = why == Code.OK
		reason = "" if accepted else ServeValidator.describe(why)

## One sentence per refusal code, for a HUD. Static and pure, so the client that receives the code says the
## same words the server would have.
static func describe(code: int) -> String:
	match code:
		Code.NO_SEAT:
			return "the sender holds no seat"
		Code.PUCK_LIVE:
			return "the puck is live"
		_:
			return ""

## Whether `sender_seat` may serve right now.
##
## `faceoff_ticks_left` is the puck's own replicated countdown: above zero, the puck is waiting out a face-off
## and any seated player may start it early. `puck_at_rest` covers the other dead case, a puck that has stalled
## against a rail with nobody able to reach it.
static func validate(sender_seat: int, puck_at_rest: bool, faceoff_ticks_left: int) -> Result:
	if not HockeyConfig.is_valid_seat(sender_seat):
		# Resolved from the sender id, so this means "the peer that asked holds no seat" -- a spectator, or a
		# peer whose disconnect is still in flight. Never a claim the payload made about itself.
		return Result.new(Code.NO_SEAT)
	if faceoff_ticks_left > 0:
		return Result.new(Code.OK)
	if puck_at_rest:
		return Result.new(Code.OK)
	return Result.new(Code.PUCK_LIVE)
