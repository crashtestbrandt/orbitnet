extends RefCounted
class_name ObserverDesk
## Where an observing peer says it is watching from, and when that declaration is worth resending.
##
## PURE. No tree, no `Net` calls, no signals -- this decides, and the session layer declares. That split is
## what makes the throttle testable at all, and the throttle is not optional: an observer pans continuously,
## and one anchor message per frame would spend reliable bandwidth restating a center that moved 20 cm.
##
## TWO MODES, BECAUSE THE FACADE OFFERS TWO CALLS.
##
##   FIXED    a ground point. `Net.set_peer_anchor()`. Stays where it was put.
##   TRACKED  an entity. `Net.set_peer_anchor_entity()`. Follows it wherever it goes, including out of the
##            region the observer was looking at, and keeps following it across a respawn -- entity ids are
##            derived from node paths, so a body returning under its old name reclaims its old id.
##
## A TRACKED ENTITY THAT DESPAWNS LEAVES THE PEER WHERE IT LAST WAS, and the desk keeps reporting TRACKED
## until the game says otherwise. That is the facade's rule rather than a choice made here: a membership is a
## declaration and did not fail, while a center is a measurement and did.
##
## THE DESK NEVER CLEARS THE DECLARATION. Retracting is `Net.clear_peer_anchor()` and it is a different
## decision -- it hands the peer back to inference off its own driven body, which an observer does not have.
## The session layer calls it when a peer stops observing; there is nothing for this class to time.

enum Mode {
	## Watching a ground point.
	FIXED,
	## Watching an entity, wherever it is.
	TRACKED,
}

## Meters the fixed point must move before a fresh declaration earns a reliable message.
const RESEND_DISTANCE_M: float = 4.0
## The longest a declaration may go unrefreshed regardless of movement, so an observer that panned once and
## stopped is still corrected if the message that carried it was lost.
const RESEND_INTERVAL_S: float = 2.0
## `_sent_at_s` before anything has been sent. Any plausible `now_s` is far past it, which is the point.
const NEVER_SENT_S: float = -1.0e9

var _mode: Mode = Mode.FIXED
var _point: Vector3 = Vector3.ZERO
var _entity: int = 0

var _sent: bool = false
var _sent_mode: Mode = Mode.FIXED
var _sent_point: Vector3 = Vector3.ZERO
var _sent_entity: int = 0
var _sent_at_s: float = NEVER_SENT_S

# --- what is being watched -----------------------------------------------------------------------
## Watch a ground point. The observer's camera pivot is what this demo feeds it.
func watch_point(point: Vector3) -> void:
	_mode = Mode.FIXED
	_point = point

## Watch an entity, by the id `entity_id()` returns on a rollback or state handle. `0` is the facade's
## retraction value and is refused here rather than passed on: a desk in TRACKED mode with no entity would
## declare a center and immediately retract it, once per resend interval, forever.
func watch_entity(entity_id: int) -> bool:
	if entity_id == 0:
		return false
	_mode = Mode.TRACKED
	_entity = entity_id
	return true

func mode() -> Mode:
	return _mode

func point() -> Vector3:
	return _point

func tracked_entity() -> int:
	return _entity

# --- when to say it again ------------------------------------------------------------------------
## Whether the current declaration differs from the one last sent by enough to be worth resending.
##
## A MODE CHANGE IS ALWAYS DUE, whatever the distance or the clock says. The two modes are different facade
## calls, so a switch from TRACKED to a FIXED point four centimeters from the tracked body is still a switch
## and still has to cross the wire.
func due(now_s: float) -> bool:
	if not _sent:
		return true
	if _mode != _sent_mode:
		return true
	if now_s - _sent_at_s >= RESEND_INTERVAL_S:
		return true
	if _mode == Mode.TRACKED:
		return _entity != _sent_entity
	return _point.distance_to(_sent_point) >= RESEND_DISTANCE_M

## Record that the current declaration went out at `now_s`.
func mark_sent(now_s: float) -> void:
	_sent = true
	_sent_mode = _mode
	_sent_point = _point
	_sent_entity = _entity
	_sent_at_s = now_s

## Forget what was sent, without changing what is being watched. Called when a session ends: the next one
## starts with a server that has been told nothing, so the first declaration must go out unconditionally.
func forget_sent() -> void:
	_sent = false
	_sent_at_s = NEVER_SENT_S

## A one-line description for the readout.
func describe() -> String:
	if _mode == Mode.TRACKED:
		return "entity %d" % _entity
	return "(%.0f, %.0f)" % [_point.x, _point.z]
