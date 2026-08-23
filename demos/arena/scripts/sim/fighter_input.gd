extends Node
class_name FighterInput
## One fighter's replicated input frame. A child of the fighter, because the INPUT lane's authority is the
## owning client while the STATE lane's is the server -- two authorities, so two nodes.
##
## IT IS A REQUEST, NOT A RESULT. The backend checks WHO wrote a row, never what is in it: a row that decodes
## at the right stride is stored as-is. Every value here is therefore clamped inside `_rollback_tick`, on the
## server, before it moves anything. See FighterMotion.clamp_intent().
##
## THE NODE'S NAME IS PART OF THE SCHEMA. Entity ids are hashes of node paths, so a fighter's input node must
## be named identically on every peer -- ArenaNames.INPUT_NODE, set before it enters the tree.

## Move intent in the arena's frame, nominally within the unit disc. Clamped on the server.
var nin_move: Vector3 = Vector3.ZERO
## Aim direction. Normalized on the server; a zero vector answers +z rather than NaN.
var nin_aim: Vector3 = Vector3(0.0, 0.0, 1.0)
## Button bits. Bit 0 is fire-held; it exists so the HUD can show a trigger being held without a command.
##
## THE SHOT ITSELF IS NOT HERE. A shot is discrete, sparse and needs a server verdict, which is the command
## lane's shape and not the rollback lane's -- and a shot discovered inside `_rollback_tick` would be replayed
## on every resim and fire again each time.
var nin_buttons: int = 0

const BUTTON_FIRE: int = 1 << 0

func is_firing() -> bool:
	return (nin_buttons & BUTTON_FIRE) != 0

func set_firing(on: bool) -> void:
	nin_buttons = (nin_buttons | BUTTON_FIRE) if on else (nin_buttons & ~BUTTON_FIRE)
