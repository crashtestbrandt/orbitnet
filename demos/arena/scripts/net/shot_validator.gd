extends RefCounted
class_name ShotValidator
## What the server checks about a shot before it resolves one. Pure, and deliberately separate from the
## resolution: this decides whether the request is admissible, `HitResolver` decides what it hit.
##
## THE SENDER IS NOT THE SEAT. Every other check in this file is ordinary rate limiting; this one is the
## security model. `NetCommand` hands its validator the sender's peer id -- the only identity a client cannot
## author -- and a connection here may drive two fighters, so the seat has to travel in the PAYLOAD. That
## makes it a claim, and a claim has to be checked against the seats the SERVER assigned to that sender.
## Trusting the payload's seat unchecked is a forged shot on somebody else's fighter.
##
## WHAT IS NOT CHECKED HERE, AND WHY. The aim direction is not validated for plausibility, because there is no
## plausible-aim test that is not also a test of what the player could see. The command TICK is clamped rather
## than rejected: a tick from the future is a clock that has drifted, and refusing it would refuse a shot for
## an error the server is at least half of.

## The payload keys, named once. A typo in a key is a shot that silently reads zero.
const KEY_SEAT: StringName = &"seat"
const KEY_TICK: StringName = &"tick"
const KEY_AIM: StringName = &"aim"

enum Verdict {
	OK,
	## The sender does not hold the seat it named.
	NOT_YOURS,
	## The named seat is not a seat.
	NO_SUCH_SEAT,
	## That fighter is dead, or still on its respawn countdown.
	NOT_ALIVE,
	## It fired more recently than the cooldown allows.
	COOLING,
}

static func describe(verdict: Verdict) -> String:
	match verdict:
		Verdict.OK:
			return "ok"
		Verdict.NOT_YOURS:
			return "the sender does not hold that seat"
		Verdict.NO_SUCH_SEAT:
			return "no such seat"
		Verdict.NOT_ALIVE:
			return "that fighter is down"
		Verdict.COOLING:
			return "still cooling"
	return "unknown"

## Whether `sender` may fire `seat` at `now_tick`, given when that seat last fired.
static func check(roster: SeatRoster, sender: int, seat: int, alive: bool, last_shot_tick: int,
		now_tick: int) -> Verdict:
	if seat < 0 or seat >= ArenaConfig.SEAT_COUNT:
		return Verdict.NO_SUCH_SEAT
	if not roster.owns_seat(sender, seat):
		return Verdict.NOT_YOURS
	if not alive:
		return Verdict.NOT_ALIVE
	if last_shot_tick >= 0 and now_tick - last_shot_tick < ArenaConfig.SHOT_COOLDOWN_TICKS:
		return Verdict.COOLING
	return Verdict.OK

## The tick a shot is resolved at, clamped into the window the server can actually reconstruct.
##
## A tick from the FUTURE is clamped to the present rather than refused: a client's clock leading the server's
## is ordinary, and the shot is then a live present-tick cast, which is the conservative answer. A tick from
## too far in the past is clamped to the oldest the ring retains, for the same reason -- a shot is resolved
## shallower than asked, never against a slot holding some other tick's world.
static func clamp_command_tick(asked: int, present_tick: int, retain: int) -> int:
	var oldest: int = maxi(0, present_tick - maxi(0, retain))
	return clampi(asked, oldest, present_tick)
