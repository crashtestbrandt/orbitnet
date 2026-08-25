extends RefCounted
class_name ArenaConfig
## Every number this demo is tuned by, in one place, so an experiment is an edit here rather than a hunt.
##
## THE ARENAS ARE REBASED, AND WHAT IS REPLICATED IS ARENA-LOCAL. Every fighter and every prop replicates a
## position in ITS OWN arena's frame, near that arena's origin, which is the arrangement membership exists
## for: two fighters standing on the same spot in different arenas are ZERO meters apart, so no radius can
## separate them and the only thing that can is a declared world. The world-space spacing below is applied
## when a node is PLACED for rendering, and never reaches the wire or the interest pass.

# --- arenas ----------------------------------------------------------------------------------------
## How many independent arenas one session hosts.
const ARENAS: int = 3
## Meters between one arena's origin and the next, IN WORLD SPACE ONLY.
##
## A presentation number. It decides where the renderer puts an arena and nothing else -- the interest pass
## sees arena-local coordinates, where all three arenas overlap exactly. Spacing them at all is so a human
## looking at the scene can see three arenas rather than one with three sets of fighters standing inside each
## other.
const ARENA_SPACING_M: float = 1200.0
## Half-extents of one arena's floor, meters.
const ARENA_HALF_X: float = 20.0
const ARENA_HALF_Z: float = 20.0

## THE FIRST ARENA IS 1, NOT 0, AND THAT IS NOT A STYLE CHOICE. `0` is the facade's "every world" membership:
## a channel declaring 0 is in every arena at once, which is the fail-open default. Numbering arenas from 1
## means a membership property that was never written filters nothing rather than silently joining arena 0.
const FIRST_ARENA_ID: int = 1

# --- seats -----------------------------------------------------------------------------------------
## Fighters per arena. A static pool: every peer builds all of them, with identical names, and the set never
## changes at runtime -- the same reason the other two demos use static pools.
const SEATS_PER_ARENA: int = 8
## The whole pool.
const SEAT_COUNT: int = ARENAS * SEATS_PER_ARENA
## Teams per arena. Seat parity fixes the team, so it is derived rather than replicated.
const TEAMS: int = 2

## The most seats one CONNECTION may drive at once -- local split-screen.
##
## TWO, AND THE SECOND ONE IS THE WHOLE FEATURE. A seat is a body with its own interest anchor, its own center
## and its own world; a connection receives the UNION of its seats' sets. One is the ordinary case and needs
## no declaration at all, because every body starts at seat 0.
const MAX_SEATS_PER_PEER: int = 2

## Connections admitted BEYOND those that hold a seat, for peers that watch without playing.
const OBSERVER_SLOTS: int = 4

# --- the net tick ----------------------------------------------------------------------------------
## The rate this demo runs at when networked. Applied through `Net.set_net_tick_decoupled()` at session start
## and used as the offline fixed-step dt, so offline and networked step the simulation identically.
const NET_TICK_HZ: int = 30
const NET_TICK_DT: float = 1.0 / float(NET_TICK_HZ)

# --- movement --------------------------------------------------------------------------------------
const MOVE_SPEED: float = 7.5
const MOVE_ACCEL: float = 42.0
const MOVE_DAMPING: float = 0.16
const FIGHTER_RADIUS: float = 0.35
const FIGHTER_HEIGHT: float = 1.8

# --- weapon ----------------------------------------------------------------------------------------
## How far a shot reaches, meters. Deliberately longer than an arena's diagonal, so no shot is refused for
## range and every miss is a miss.
const SHOT_RANGE_M: float = 64.0
## Damage per hit, as a fraction of full health.
const SHOT_DAMAGE: float = 0.34
## Minimum ticks between two shots from one fighter. The server enforces it; the client only predicts it.
const SHOT_COOLDOWN_TICKS: int = 6
## Ticks a dead fighter stays down before it respawns at its home point.
const RESPAWN_TICKS: int = 60

# --- cloak -----------------------------------------------------------------------------------------
## Ticks a cloak lasts once taken. Ten seconds at this demo's 30 Hz -- long enough to cross the arena under
## it, and long enough that a gate watching for the veto has a window to sample rather than a moment.
const CLOAK_TICKS: int = 300
## Ticks between one cloak pickup becoming available again.
const CLOAK_RESPAWN_TICKS: int = 450

# --- props -----------------------------------------------------------------------------------------
## State-lane scenery per arena. They replicate a position and nothing else, they declare an anchor and a
## membership, and they exist to put real pressure on the entity slot table and on the interest pass.
const PROPS_PER_ARENA: int = 96
## Cover blocks per arena. STATIC bodies, so the live half of a lag-compensated cast has something to stop it.
const COVER_PER_ARENA: int = 8

# --- collision layers ------------------------------------------------------------------------------
## Fighters' hit capsules. The DYNAMIC half of a shot's mask: reconstructed from the rewind ring, never cast
## live, because where a fighter was is not where it is.
const LAYER_FIGHTER: int = 1
## Cover. The STATIC half: cast live at the present tick, because a wall is the same wall at every tick.
const LAYER_COVER: int = 2
## The mask a shot queries, and the subset of it the rewind reconstructs.
const SHOT_MASK: int = LAYER_FIGHTER | LAYER_COVER
const SHOT_DYNAMIC_MASK: int = LAYER_FIGHTER

# --- interest --------------------------------------------------------------------------------------
## The distance radius the session runs at, meters. Comfortably larger than an arena, so within one arena
## nothing is culled by distance and the filtering on show is MEMBERSHIP and the VETO. Turning it down is a
## keybound lever, and turning it down is how the third axis becomes visible.
const AOI_RADIUS_M: float = 60.0
## The scale the send path's priority bands are derived from, meters: edges at scale/3 and 2*scale/3. Sized to
## an arena rather than to the session, because a value large enough to span three arenas would put every body
## in one band and the per-band rewind measurements would all read the same.
const BAND_SCALE_M: float = 45.0

# --- derived ---------------------------------------------------------------------------------------
## Which arena a seat belongs to. Seats are handed out in blocks, so a whole arena's seats are contiguous and
## an arena's fighters are adjacent in every mirror array.
static func arena_of_seat(seat: int) -> int:
	if seat < 0 or seat >= SEAT_COUNT:
		return 0
	return FIRST_ARENA_ID + seat / SEATS_PER_ARENA

## Which team a seat is on. Parity, so no peer has to be told.
static func team_of_seat(seat: int) -> int:
	return 0 if seat < 0 else seat % TEAMS

## The first seat index belonging to `arena_id`.
static func first_seat_of_arena(arena_id: int) -> int:
	return (arena_id - FIRST_ARENA_ID) * SEATS_PER_ARENA

## Whether `arena_id` names an arena this session built.
static func is_arena(arena_id: int) -> bool:
	return arena_id >= FIRST_ARENA_ID and arena_id < FIRST_ARENA_ID + ARENAS
