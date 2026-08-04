extends Node
class_name CommanderInput
## The client-authored half of a commander. A separate NODE, not a field on the avatar, because the two
## halves need DIFFERENT multiplayer authorities and authority is a per-node property:
##
##   CommanderAvatar  authority = the SERVER   -> the server owns the authoritative state
##   CommanderInput   authority = the OWNER    -> each client authors only its OWN input
##
## That split IS the server-authoritative model. The backend validates an incoming input frame against this
## node's authority, so a client cannot submit input on another player's commander -- the anti-forgery check
## happens at the netcode layer, before any game code runs.
##
## The property name is prefixed `nin_` ("net input") purely so that a reader scanning the avatar can tell at
## a glance which values are client-authored and which are server-owned. That distinction is the one people
## get wrong, and one character of prefix is cheaper than re-deriving it every time.

## Where this player's command cursor is on the ground plane. Written every frame by the local player's
## controller, captured once per net tick by the rollback lane, quantized to three halves on the wire.
var nin_cursor: Vector3 = Vector3.ZERO
