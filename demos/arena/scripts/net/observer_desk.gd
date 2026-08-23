extends RefCounted
class_name ObserverDesk
## Where an observing peer says it is watching from, WHICH ARENA it is watching, and when that declaration is
## worth resending.
##
## PURE. No tree, no `Net` calls, no signals -- this decides, and the session layer declares. That split is
## what makes the throttle testable at all, and the throttle is not optional: an observer pans continuously,
## and one reliable message per frame to restate a centre that slid 20 cm is how a spectator costs more than a
## player.
##
## THE ARENA IS HALF THE DECLARATION, AND IT IS THE HALF THAT CANNOT BE INFERRED. A player's world is read off
## the body its input drives; an observer drives none, so without a declared world it is in world 0 -- which
## the facade reads as EVERY world. An observer that declared only a centre would be watching one point in all
## three arenas at once, which is the fail-open answer rather than a viewpoint.
##
## TWO MODES, BECAUSE THE FACADE OFFERS TWO CALLS.
##
##   FIXED    a ground point. `Net.set_peer_anchor()`. Stays where it was put.
##   TRACKED  an entity. `Net.set_peer_anchor_entity()`. Follows it wherever it goes, and costs one message
##            however far it runs -- a tracked entity carries its own position.
##
## A TRACKED ENTITY THAT DESPAWNS LEAVES THE PEER WHERE IT LAST WAS, and the desk keeps reporting TRACKED
## until the game says otherwise. That is the facade's rule rather than a choice made here: a membership is a
## declaration and did not fail, while a centre is a measurement and did.

enum Mode {
	## Watching a ground point.
	FIXED,
	## Watching an entity, wherever it is.
	TRACKED,
}

## Metres the fixed point must move before a fresh declaration earns a reliable message.
const RESEND_DISTANCE_M: float = 3.0
## The longest a declaration may go unrefreshed regardless of movement, so an observer that panned once and
## stopped is still corrected if the message that carried it was lost.
const RESEND_INTERVAL_S: float = 2.0
## `_sent_at_s` before anything has been sent. Any plausible `now_s` is far past it, which is the point.
const NEVER_SENT_S: float = -1.0e9

var _mode: Mode = Mode.FIXED
var _point: Vector3 = Vector3.ZERO
var _entity: int = 0
var _arena: int = ArenaConfig.FIRST_ARENA_ID

var _sent: bool = false
var _sent_mode: Mode = Mode.FIXED
var _sent_point: Vector3 = Vector3.ZERO
var _sent_entity: int = 0
var _sent_arena: int = 0
var _sent_at_s: float = NEVER_SENT_S

# --- what is being watched -----------------------------------------------------------------------
## Watch a ground point in `arena_id`. The point is ARENA-LOCAL, because that is the frame every anchor in
## this session is expressed in -- an observer declaring a world position would be declaring a centre 1200 m
## from every entity in the arena it named.
func watch_point(point: Vector3, arena_id: int) -> void:
	_mode = Mode.FIXED
	_point = point
	_arena = arena_id

## Watch an entity. `0` is the facade's retraction value and is refused here rather than passed on: a desk in
## TRACKED mode with no entity would declare a centre and immediately retract it, once per resend interval,
## forever.
func watch_entity(entity_id: int, arena_id: int) -> bool:
	if entity_id == 0:
		return false
	_mode = Mode.TRACKED
	_entity = entity_id
	_arena = arena_id
	return true

func mode() -> Mode:
	return _mode

func point() -> Vector3:
	return _point

func tracked_entity() -> int:
	return _entity

func arena() -> int:
	return _arena

# --- when to say it again ------------------------------------------------------------------------
## Whether the current declaration differs from the one last sent by enough to be worth resending.
##
## A MODE CHANGE OR AN ARENA CHANGE IS ALWAYS DUE, whatever the distance or the clock says. The two modes are
## different facade calls, and the arena is a different world -- an observer that walked its centre from arena
## 1 to the same local point in arena 2 moved zero metres and changed everything it can see.
func due(now_s: float) -> bool:
	if not _sent:
		return true
	if _mode != _sent_mode or _arena != _sent_arena:
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
	_sent_arena = _arena
	_sent_at_s = now_s

## Forget what was sent, without changing what is being watched. Called when a session ends: the next one
## starts with a server that has been told nothing, so the first declaration must go out unconditionally.
func forget_sent() -> void:
	_sent = false
	_sent_at_s = NEVER_SENT_S

## A one-line description for the readout.
func describe() -> String:
	if _mode == Mode.TRACKED:
		return "entity %d in arena %d" % [_entity, _arena]
	return "(%.0f, %.0f) in arena %d" % [_point.x, _point.z, _arena]
