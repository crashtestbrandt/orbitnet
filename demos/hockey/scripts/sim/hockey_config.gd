extends RefCounted
class_name HockeyConfig
## Every tunable number in the demo, in one place.

# --- the table, in TABLE SPACE ---------------------------------------------------------------------
## Table space is 2D and axis-aligned: `x` runs across the table, `z` runs along it toward the goals, and `y`
## is always 0. NOTHING in the simulation knows the table is inclined -- the tilt lives entirely in the `Rink`
## node's transform, so a puck's `position` is already its table-space coordinate and the view is a pure
## presentation layer over it.
##
## Metres, at roughly regulation scale (a 2 m x 1 m playing surface). The scale is not arbitrary: `@half` on
## the wire is IEEE-754 binary16, whose spacing near a coordinate of 1.0 is about 1 mm, and this demo reports
## its correction in millimetres. Ten times this table and the quantization floor would be the number.
const HALF_WIDTH: float = 0.5
const HALF_LENGTH: float = 1.0

## Half-width of a goal mouth, centred on x = 0 at each end.
const GOAL_HALF_WIDTH: float = 0.13

## The puck.
const PUCK_RADIUS: float = 0.032
## Fraction of speed the air cushion leaves after one second. Applied as pow(PUCK_DAMPING, dt) so the decay is
## the same whatever the tick rate -- a per-tick multiplier would make the puck slower at 60 Hz than at 30.
const PUCK_DAMPING: float = 0.93
## Speed under which the puck counts as dead and a serve is legal again.
const PUCK_REST_SPEED: float = 0.06
## Hard speed ceiling. Not politeness: it is what bounds the per-substep travel, and therefore what makes the
## substep count below sufficient to stop the puck tunnelling through a mallet.
const PUCK_MAX_SPEED: float = 6.0
## Energy kept when the puck bounces off a rail.
const RAIL_RESTITUTION: float = 0.86
## Energy kept in the puck's own approach velocity when a mallet hits it.
const MALLET_RESTITUTION: float = 0.94
## How much of the mallet's own velocity is added to the puck on contact. Above 1.0 a stationary mallet still
## returns the puck harder than it arrived, which is what makes a rally sustain.
const MALLET_TRANSFER: float = 1.35

## Substeps of the puck integration per net tick. At PUCK_MAX_SPEED a 60 Hz tick moves the puck 100 mm, which
## is more than its 64 mm diameter -- a single-step sweep would let it pass straight through a mallet. Four
## substeps cap the per-substep travel at 25 mm, comfortably inside the smallest contact pair.
const PUCK_SUBSTEPS: int = 4

## The mallets.
const MALLET_RADIUS: float = 0.048
## Metres per second a mallet may chase the pointer at. A finite speed is what gives the mallet a VELOCITY at
## all, and the mallet's velocity is what strikes the puck; a mallet that teleported onto the pointer would
## have no defined speed and could place the puck anywhere.
const MALLET_MAX_SPEED: float = 3.6
## Metres per second squared. High enough that the mallet feels attached to the pointer, finite so that a
## flick has a ramp the server and the client both simulate.
const MALLET_ACCEL: float = 48.0
## Fraction of speed a coasting mallet keeps after one second. Only ever used on a peer that receives no input
## for that mallet -- see MalletBody for why dead reckoning is a different simulation.
const MALLET_COAST_DAMPING: float = 0.02

# --- seating ---------------------------------------------------------------------------------------
## Mallet seats. Effectively "as many players as want to play", and the number is a WIRE fact rather than a
## gameplay one:
##
##   OrbitNet derives an entity id from its synchronizer root's NODE PATH, so a mallet created after world
##   build would have to be created at the identical path on every peer, in the same order, or the ids diverge
##   and replication silently goes nowhere. Doing that correctly needs a spawn-replication mechanism, which is
##   a real problem and the wrong one for a demo about the rollback lane to also be about.
##
## So every peer builds all SEATS mallets at world build with identical names and the node set never changes.
## An unoccupied seat's mallet is parked, undrawn and skipped by the puck's collision pass, and its properties
## stop changing -- so the state lane's dirty tracking stops sending it. A vacant seat is free.
##
## The transport is capped at the same number, so the peer after the last seat is refused by ENet rather than
## admitted as a player with nowhere to stand.
const SEATS: int = 32

## The net tick this demo runs at. Coupled, so this is also the physics rate -- but anything converting ticks
## to seconds must ask Net.net_tick_dt() rather than assuming it, because the F1 lever changes it live.
const NET_TICK_HZ: int = 60

# --- face-off --------------------------------------------------------------------------------------
## Net ticks the puck stays dead after a goal before it serves itself. Rollback state on the puck, so a resim
## counts it down identically on every peer.
const FACEOFF_TICKS: int = 90
## Speed the puck is served at, toward the team that conceded.
const SERVE_SPEED: float = 1.7
## Largest angle either side of straight-down-the-table a serve may take.
const SERVE_SPREAD_RAD: float = 0.5

# --- presentation ------------------------------------------------------------------------------------
## Degrees the rink is tilted about its x axis so a fixed camera sees the whole surface in perspective.
const TABLE_TILT_DEGREES: float = 40.0
## Camera pitch and field of view the framing is solved for.
const CAMERA_PITCH_DEGREES: float = -22.0
const CAMERA_FOV_DEGREES: float = 22.0
## Fraction of the frustum left empty around the table. 0.12 keeps the rails clear of the window edge at every
## aspect the framing solve is asked about.
const FRAMING_MARGIN: float = 0.10

## Distance at which a teammate's mallet starts fading, and the alpha it never drops below. Mallets do not
## collide with each other, so two teammates can overlap; fading the other one keeps your own readable without
## ever hiding where a team-mate actually is.
const FADE_START: float = 0.22
const FADE_FLOOR: float = 0.12

## Metres of disagreement below which a correction is not a correction.
##
## The view detects a correction by extrapolating the previous pose forward and comparing, and that
## extrapolation is a straight line while the simulation damps and substeps -- so EVERY tick disagrees by a
## little, correction or not. Without a deadband the blended counter climbs once per tick forever, including
## offline where nothing is being corrected at all.
##
## Sized at the wire's own resolution: `net_pos` rides as binary16, whose spacing at this table's scale is
## about a millimetre, so a disagreement below that is not distinguishable from quantization anyway.
const CORRECTION_DEADBAND_M: float = 0.0012

## Metres of correction above which the puck's render position SNAPS instead of blending. Below it the view
## absorbs the correction over CORRECTION_HALF_LIFE seconds and the player never sees a jump.
const CORRECTION_SNAP_M: float = 0.09
const CORRECTION_HALF_LIFE: float = 0.06

# --- teams -----------------------------------------------------------------------------------------
## Seat parity fixes the end: even seats defend -z, odd seats defend +z. Team is DERIVED from the seat index
## and never replicated -- every peer already knows the seat, so sending the team would be sending a
## subtraction.
static func team_of_seat(seat: int) -> int:
	return seat & 1

## The z sign of the end `team` defends. Team 0 defends -z, team 1 defends +z.
static func end_sign(team: int) -> float:
	return -1.0 if team == 0 else 1.0

## A team's colour, for the renderer and the HUD.
static func team_color(team: int) -> Color:
	if team == 0:
		return Color(0.35, 0.66, 1.0)
	return Color(1.0, 0.46, 0.34)

## Whether `seat` names a seat at all. The first thing every validator checks, because every later check
## indexes an array with it.
static func is_valid_seat(seat: int) -> bool:
	return seat >= 0 and seat < SEATS
