extends Node
class_name MalletInput
## The client-authored half of a mallet. A separate NODE, not a field on the mallet, because the two halves
## need DIFFERENT multiplayer authorities and authority is a per-node property:
##
##   MalletBody   authority = the SERVER   -> the server owns the authoritative pose
##   MalletInput  authority = the OWNER    -> each client authors only its OWN input
##
## That split IS the server-authoritative model. The backend validates an incoming input frame against this
## node's authority, so a client cannot submit input on another player's mallet -- the anti-forgery check
## happens at the netcode layer, before any game code runs.
##
## The property name is prefixed `nin_` ("net input") purely so that a reader scanning the mallet can tell at a
## glance which values are client-authored and which are server-owned. That distinction is the one people get
## wrong, and one character of prefix is cheaper than re-deriving it every time.

## Where this player wants their mallet, in table space. Written every frame by the local player's controller,
## captured once per net tick by the rollback lane, quantized to three halves on the wire.
##
## It is a REQUEST, not a position. The server clamps it into the player's own half inside `_rollback_tick`,
## which is where a client-authored value becomes server-owned state.
var nin_target: Vector3 = Vector3.ZERO
