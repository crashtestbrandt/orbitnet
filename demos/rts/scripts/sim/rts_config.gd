extends RefCounted
class_name RtsConfig
## Every tunable number in the demo, in one place, plus the archetype table.
##
## THE UNIT COUNT IS NOT A TASTE VALUE -- it is read off the backend's measured wire budget, and the
## derivation is the whole reason this demo is the size it is:
##
##   * One UDP frame per peer per net tick, with a ~1200-byte payload budget.
##   * A state-lane unit costs 20 bytes of properties (see UnitBody) plus a 5-byte per-entity header:
##     2 for the entity's wire slot, 1 frame-tick delta, 1 body length, 1 flags. 25 bytes on the wire.
##   * 1200 / 25 = 48 units refreshed per peer per tick.
##   * Entities are served STALEST-FIRST, so exceeding that does not drop anyone -- it raises everyone's
##     staleness. 96 units at 20 Hz means a full refresh every 2 net ticks, i.e. ~100 ms worst-case age.
##
## The header was 12.5 bytes until the entity's 64-bit id came off the wire in favour of a 16-bit session
## slot: the id was a hash spread across the whole 64-bit range, so its varint cost 9.5 bytes on average.
## The figure here read ~26 bytes and ~46 units, which was never the measured header -- the real numbers
## before that change were 32.5 bytes and ~37 units.
##
## That is a deliberate, comfortable 2x over the single-tick budget: enough that the round-robin is real and
## visible in the HUD's staleness readout, not so much that the demo looks broken. Push UNITS_PER_SEAT up and
## the staleness number climbs linearly -- which is the experiment, and why the number is here rather than
## scattered through the spawner.

# --- scale ---------------------------------------------------------------------------------------
## Player seats. Two is the demo; the wire schema and the validator are not limited to it.
const SEATS: int = 2
## Connections admitted BEYOND the seats, for peers that watch without playing. The transport's peer cap is
## seats plus this, because a cap of SEATS refuses an observer at the socket -- before the session layer ever
## gets to decide what to do with it. An observer costs a datagram stream and no seat.
const OBSERVER_SLOTS: int = 2
## Units per seat. See the budget derivation above before changing it.
const UNITS_PER_SEAT: int = 48
## Total unit pool. Every peer builds exactly this many unit nodes at world build, with identical names, and
## the set NEVER changes at runtime -- see WorldDirector for why a static pool beats spawn/despawn here.
const UNIT_COUNT: int = SEATS * UNITS_PER_SEAT

## The net tick this demo runs at when networked. Applied through Net.set_net_tick_decoupled() at session
## start, and used as the offline fixed-step dt so offline and networked step the sim identically.
const NET_TICK_HZ: int = 20
const NET_TICK_DT: float = 1.0 / float(NET_TICK_HZ)

# --- battlefield ---------------------------------------------------------------------------------
## Half-extents of the playable ground plane, metres. Units are clamped inside it.
const FIELD_HALF_X: float = 60.0
const FIELD_HALF_Z: float = 40.0
## Where each seat's units start and respawn, as a fraction of FIELD_HALF_X from the centre.
const SPAWN_X_FRACTION: float = 0.82
## Radius of the spawn blob a seat's units are scattered into.
const SPAWN_SPREAD: float = 14.0

# --- combat --------------------------------------------------------------------------------------
## Seconds a dead unit stays dead before the respawn drip returns it to its spawn area. The drip exists so the
## fight reaches a STEADY STATE rather than a winner: the demo is meant to be left running while someone
## watches the diagnostics, which a win condition would end.
const RESPAWN_DELAY_S: float = 6.0
## Units the drip may revive per seat per net tick. Rate-limited so a wipe does not resurrect a whole army in
## one tick, which would be a step change in the wire load exactly when it is most interesting to watch.
const RESPAWN_PER_TICK: int = 1

# --- orders --------------------------------------------------------------------------------------
## Largest unit-id batch a single order may name. A cap is not politeness: without it one client can make the
## server do unbounded work per reliable packet.
const MAX_ORDER_IDS: int = UNIT_COUNT
## Orders per second a single sender may issue before the throttle starts dropping them.
const ORDER_RATE_PER_S: float = 12.0
## Burst allowance on top of that rate.
const ORDER_BURST: int = 8

# --- archetypes ----------------------------------------------------------------------------------
enum Kind { SCOUT, TROOPER, TANK }

## One archetype's tuning. Plain data, constructed once into the static table below.
class Archetype extends RefCounted:
	# `int`, not `Kind`: an inner class does not resolve the outer script's enum by bare name, and naming it
	# RtsConfig.Kind here would be a self-reference during parse. The enum is ints anyway.
	var kind: int = 0
	var name: String = ""
	var max_speed: float = 0.0      # m/s
	var accel: float = 0.0          # m/s^2
	var radius: float = 0.0         # m, for obstacle clearance and formation spacing
	var turn_rate: float = 0.0      # rad/s -- how fast the drawn facing chases the velocity
	var hp_max: float = 0.0
	var dps: float = 0.0            # damage per second applied continuously while in range
	var attack_range: float = 0.0   # m
	var acquire_range: float = 0.0  # m -- how far it will look for a target

	func _init(k: int, n: String, speed: float, acc: float, rad: float, turn: float,
			hp: float, damage: float, atk: float, acquire: float) -> void:
		kind = k
		name = n
		max_speed = speed
		accel = acc
		radius = rad
		turn_rate = turn
		hp_max = hp
		dps = damage
		attack_range = atk
		acquire_range = acquire

static var _table: Array[Archetype] = []

## The archetype table, built once. Lazy rather than `_static_init` so the class behaves identically on every
## 4.x this addon supports.
static func archetypes() -> Array[Archetype]:
	if _table.is_empty():
		_table = [
			#            kind          name        speed accel radius turn   hp    dps  atk  acquire
			Archetype.new(Kind.SCOUT,   "Scout",     11.0, 22.0, 0.45,  7.0,  40.0,  6.0, 7.0, 22.0),
			Archetype.new(Kind.TROOPER, "Trooper",    7.0, 14.0, 0.55,  5.0,  90.0, 11.0, 9.0, 18.0),
			Archetype.new(Kind.TANK,    "Tank",       4.5,  7.0, 0.95,  2.5, 240.0, 20.0, 11.0, 16.0),
		]
	return _table

static func archetype(kind: Kind) -> Archetype:
	var table: Array[Archetype] = archetypes()
	var index: int = clampi(int(kind), 0, table.size() - 1)
	return table[index]

## The archetype a unit index within a seat gets. A fixed repeating pattern rather than a random roll: every
## peer must agree on every unit's stats without replicating them, and the entity-id gate in the probe asserts
## the two peers built the same world -- a random composition would make that assertion meaningless.
## The pattern is 6 Scouts : 5 Troopers : 1 Tank per 12, so 48 units is 24/20/4.
static func kind_for_index(index_in_seat: int) -> Kind:
	var slot: int = index_in_seat % 12
	if slot == 11:
		return Kind.TANK
	if slot >= 6:
		return Kind.TROOPER
	return Kind.SCOUT

# --- ids -----------------------------------------------------------------------------------------
## The seat that owns unit `id`. Ids are laid out seat-major, so ownership is arithmetic rather than a lookup
## the server has to trust -- which is what lets the order validator reject a foreign-seat batch without
## touching any unit state.
static func seat_of(id: int) -> int:
	if id < 0 or id >= UNIT_COUNT:
		return -1
	return id / UNITS_PER_SEAT

## Whether `id` names a unit at all. The first thing the validator checks, because every later check indexes
## an array with it.
static func is_valid_id(id: int) -> bool:
	return id >= 0 and id < UNIT_COUNT

## The first unit id belonging to `seat`.
static func first_id_of_seat(seat: int) -> int:
	return seat * UNITS_PER_SEAT

## A seat's team colour, for the renderer and the HUD.
static func seat_color(seat: int) -> Color:
	if seat == 0:
		return Color(0.35, 0.62, 1.0)
	if seat == 1:
		return Color(1.0, 0.44, 0.33)
	return Color(0.7, 0.7, 0.7)

## The centre of a seat's spawn area.
static func spawn_center(seat: int) -> Vector3:
	var sign_x: float = -1.0 if seat == 0 else 1.0
	return Vector3(sign_x * FIELD_HALF_X * SPAWN_X_FRACTION, 0.0, 0.0)
