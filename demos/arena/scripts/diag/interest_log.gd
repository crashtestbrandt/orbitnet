extends RefCounted
class_name InterestLog
## The client half of interest filtering, as EVENTS rather than as a threshold.
##
## [InterestMeter] answers the same question by polling every body's receipt tick once a frame, and that is the
## right shape for a fade: "how stale is this" is a continuous quantity and a threshold is how you read one.
## What a poll cannot give you is the EDGE -- the tick an entity stopped being sent, and the tick it started
## again -- and an edge is what a game acts on. Hiding a body, dropping a nameplate, retiring a kill-feed
## entry, releasing a pooled effect: each of those wants to happen once, at a moment, not to be re-derived from
## a threshold every frame and de-duplicated by hand.
##
## THE SERVER ALREADY KNEW. Interest runs where state authority is, and the send path computes the per-peer
## diff -- what entered this connection's union and what left it -- to clear its own delta bookkeeping. This is
## that diff, published: [signal Net.entity_left_interest] and [signal Net.entity_entered_interest].
##
## THE THREE AXES STILL LOOK IDENTICAL FROM HERE, and that remains the honest situation. A distance cull, a
## membership that does not match and a per-peer veto all produce a leave, and what a leave MEANS is the game's
## decision. This demo draws a culled fighter faded and keeps drawing it, because a shooter that deleted
## opponents at the edge of the interest radius would be a shooter where cover is a render distance.
##
## AN EVENT NAMES AN ENTITY ID, NOT A NODE, and that is deliberate rather than awkward. A leave routinely names
## an entity this peer has no node for -- one whose slot was bound before its scene object existed locally, or
## one already freed -- which is precisely the case a per-handle signal could not reach. The game keeps its own
## id-to-node map, which a kill feed or an effect pool wants anyway.
##
## IT SEEDS FROM THE QUERY. An edge needs a starting point: a log built mid-session, after entities have
## already been admitted, would report the churn from here and call the rest absent.
## [method Net.entities_in_interest] answers what the connection holds right now, so the first reading is the
## truth rather than an empty set filling in.

## Per entity id, whether this peer is currently being sent it. Ids are opaque tokens -- routinely negative,
## never compared or ordered -- so this is a set keyed on them and nothing more.
var _in_interest: Dictionary[int, bool] = {}
var _entered: int = 0
var _left: int = 0
var _seeded: bool = false

## Start listening, and resync from what this connection already holds. `local_peer` is this peer's own
## transport id, which is the connection the events are about.
##
## Safe to call more than once: the connections are guarded, and a resync replaces the set rather than merging
## into it, which is what a re-join needs.
##
## Not named `seed`: GDScript already has a global `seed()`, and shadowing it from a class is a trap for
## whoever calls the other one next.
func attach(local_peer: int) -> void:
	if not Net.entity_entered_interest.is_connected(_on_entered):
		Net.entity_entered_interest.connect(_on_entered)
	if not Net.entity_left_interest.is_connected(_on_left):
		Net.entity_left_interest.connect(_on_left)
	resync(local_peer)

## Replace the set with what the backend says `local_peer` holds now, counting none of it as a transition.
func resync(local_peer: int) -> void:
	_in_interest.clear()
	for id: int in Net.entities_in_interest(local_peer):
		_in_interest[id] = true
	_seeded = true

## Forget everything. Called on session teardown: an entity id is session-global, so carrying a set across
## sessions would report the next session's first admissions as entities that were never absent.
func reset() -> void:
	_in_interest.clear()
	_entered = 0
	_left = 0
	_seeded = false

func _on_entered(_peer: int, entity_id: int) -> void:
	if not _in_interest.has(entity_id):
		_entered += 1
	_in_interest[entity_id] = true

func _on_left(_peer: int, entity_id: int) -> void:
	if _in_interest.has(entity_id):
		_left += 1
	_in_interest.erase(entity_id)

## Whether this peer is currently being sent `entity_id`.
##
## FAILS OPEN before the log has been seeded, and on a backend that publishes no events: an unseeded log knows
## nothing, and answering "not being sent it" for every entity would blank the world on exactly the binaries
## that cannot say otherwise.
func is_in_interest(entity_id: int) -> bool:
	if not _seeded:
		return true
	return _in_interest.has(entity_id)

## How many entities this peer currently holds.
func held() -> int:
	return _in_interest.size()

## Transitions seen since the log was seeded. The pair is the point: a session where both climb is a session
## whose interest set is genuinely moving, and one where neither does is a session filtering nothing.
func entered() -> int:
	return _entered

func left() -> int:
	return _left
