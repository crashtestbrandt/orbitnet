extends Node
## The automated gate's instrumentation, attached with `--arena-probe`. It observes and PRINTS; the shell
## script compares. Nothing here changes what the demo does except where a fact has to be provoked to be
## observable at all -- the cloak and the shots, both marked below.
##
## THREE PROCESSES, THREE ROLES. A DEDICATED server holding no body of its own, a client driving two seats in
## two arenas, and an observer driving nothing. Each prints its own facts and its own verdict.
##
## EVERY ASSERTION IS TICK-DOMAIN OR SCALE-FREE, so the gate reads the same on a loaded CI runner as on a
## desktop. Nothing here measures wall-clock latency or frame rate.
##
## WHY THE PROBE PROVOKES RATHER THAN WAITS. A cloak is taken by walking onto a pickup and a shot is fired by
## a player; a gate that waited for either would be a gate that passes when nothing happened. The probe forces
## both at fixed ticks, which is exactly the difference between instrumentation and a bot.

## The seat the server forces a cloak onto. Arena 1, team 1 (odd seat) -- so a client holding seat 0, which is
## arena 1 team 0, must be withheld it. Both sides know the constant; neither is told it.
##
## WHICH MEANS THE CLIENT MUST NOT OWN IT, and the shell script is what guarantees that: it starts the seated
## client BEFORE the observer, so seats are handed out in a known order. If two peers handshake in the same
## instant the roster hands seat 0 to whichever arrived first, and a client that ended up holding seat 1 would
## be asked whether it can see its own body -- which it always can, and which proves nothing about a veto. The
## verdict below checks the assumption rather than trusting it.
const CLOAK_SEAT: int = 1
## The seat a client watches for a stall. The same one.
const WATCHED_SEAT: int = CLOAK_SEAT

## The SESSION TICK at which each phase runs. Not seconds since this process attached, and the difference is
## what makes the veto assertion mean anything at all.
##
## THREE PROCESSES START STAGGERED -- the shell launches the server, then the client three seconds later, then
## the observer two seconds after that -- so "five seconds after I attached" is a different moment in each of
## them. Keyed on elapsed time, the client's before-the-cloak sample landed AFTER the server had already
## cloaked, and its after-the-cloak sample landed after the cloak had expired: both readings were taken in
## windows that were not the ones they were named for, and the assertions built on them could only ever
## measure noise. `Net.current_tick()` is the one clock all three agree on, so a tick threshold fires at the
## same instant of the same session everywhere.
##
## FOUR SAMPLES, NOT TWO, AND THE FIRST ONE IS WHAT KEEPS THE VETO TEST FROM BEING VACUOUS. "Seat 1 stopped
## arriving" proves nothing unless seat 1 was arriving to begin with -- an entity that was never sent also
## never stops. EARLY is taken before the cloak, BASELINE after it has been in force long enough for the rows
## to have stopped, and SETTLE well after that. The assertion is then a rise followed by a flat.
##
## The numbers are ticks at this demo's 30 Hz, chosen so that EARLY is past the last process's handshake and
## VERDICT is inside the shortest `--quit-after` the shell hands out.
const EARLY_AT_TICK: int = 260
const PROVOKE_AT_TICK: int = 320
const BASELINE_AT_TICK: int = 440
const SETTLE_AT_TICK: int = 560
const VERDICT_AT_TICK: int = 620

var _net: ArenaNet = null
## Wall seconds since attach. The PHASES key on the session tick, not on this; what is left of it is the
## firing cadence, which is a rate rather than a moment and so needs no cross-process agreement.
var _elapsed: float = 0.0
var _phase: int = 0

# --- what the client records -----------------------------------------------------------------------
var _early_watched: int = -1
var _baseline_watched: int = -1
var _baseline_own: int = -1
var _settled_watched: int = -1
var _settled_own: int = -1
## Whether THIS peer has learned that the watched fighter is cloaked. The decisive client-side reading, and
## the only one that works on this lane -- see _client_verdict().
var _baseline_sees_cloak: bool = false
var _settled_sees_cloak: bool = false
## Whether the watched fighter was announced as LEAVING this peer's interest, and the transition counts around
## it. The event half of the same fact the receipt tick reports by threshold -- see _client_verdict().
var _watched_left_event: bool = false
var _left_events: int = 0
var _fire_accumulator: float = 0.0

# --- what the server records -----------------------------------------------------------------------
var _hidden_peak: int = 0
var _unproven_max: float = 0.0
var _cloak_forced: bool = false

## THE EVENT IS RECORDED FROM THE SIGNAL, NOT DERIVED FROM THE SET.
##
## `InterestLog.is_in_interest()` FAILS OPEN, which is right for a game -- a binary that publishes no events
## must not blank the world. It is exactly wrong for a gate: a backend that says nothing answers "not in
## interest" for every entity through an empty set, and an assertion reading it would pass without a single
## event having arrived. Listening directly means silence reads as silence.
func _on_left_interest(_peer: int, entity_id: int) -> void:
	var watched: FighterBody = _net.world.fighter_at(WATCHED_SEAT) if _net.world != null else null
	if watched != null and entity_id == watched.entity_id():
		_watched_left_event = true

func _ready() -> void:
	var parent: Node = get_parent()
	var main: ArenaMain = parent as ArenaMain
	if main == null:
		printerr("ARENA-PROBE: not attached to ArenaMain")
		return
	_net = main.net
	if not Net.entity_left_interest.is_connected(_on_left_interest):
		Net.entity_left_interest.connect(_on_left_interest)
	print("ARENA-PROBE attached role=%s" % _role())

func _process(delta: float) -> void:
	if _net == null:
		return
	_elapsed += delta
	if Net.is_server():
		_sample_server()
	# One phase per frame at most, so a process that attaches with the session already past a threshold still
	# takes each sample in order rather than collapsing them onto one tick.
	var now: int = Net.current_tick()
	if _phase == 0 and now >= EARLY_AT_TICK:
		_phase = 1
		_early()
	elif _phase == 1 and now >= PROVOKE_AT_TICK:
		_phase = 2
		_provoke()
	elif _phase == 2 and now >= BASELINE_AT_TICK:
		_phase = 3
		_baseline()
	elif _phase == 3 and now >= SETTLE_AT_TICK:
		_phase = 4
		_settle()
	elif _phase == 4 and now >= VERDICT_AT_TICK:
		_phase = 5
		_verdict()
	if _phase >= 2 and _phase < 5:
		_keep_firing(delta)
		_hold_cloak()

# --- roles ---------------------------------------------------------------------------------------
func _role() -> String:
	if Net.is_server():
		return "server"
	if _net.is_observing():
		return "observer"
	return "client"

# --- phases --------------------------------------------------------------------------------------
## Before the cloak: what is arriving normally.
func _early() -> void:
	print("ARENA-PROBE sig=%d" % (_net.world.world_signature() if _net.world != null else 0))
	print("ARENA-PROBE seats=%s" % _net.local_seats())
	if _net.world == null:
		return
	_early_watched = _freshness_of(WATCHED_SEAT)

## After the cloak has been in force long enough for the rows to have stopped.
func _baseline() -> void:
	if _net.world == null:
		return
	_baseline_watched = _freshness_of(WATCHED_SEAT)
	_baseline_own = _freshness_of(_own_seat())
	_baseline_sees_cloak = _sees_cloak()

## Force the two things a gate cannot wait for.
##
## THE CLOAK is taken by walking onto a pickup, and a bot that happened not to walk there would leave the veto
## untested with the gate still green. The server places it directly, which is the same state the pickup
## produces -- `queue_cloak()` is the one call either path makes.
func _provoke() -> void:
	if not Net.is_server() or _net.world == null:
		return
	var fighter: FighterBody = _net.world.fighter_at(CLOAK_SEAT)
	if fighter != null and fighter.queue_cloak():
		_cloak_forced = true
		print("ARENA-PROBE forced a cloak on seat %d (arena %d, team %d)" % [
			CLOAK_SEAT, fighter.arena_id, fighter.team])

## HOLD THE CLOAK FOR THE WHOLE OBSERVATION WINDOW, rather than forcing it once and hoping.
##
## A cloak lasts a fixed number of TICKS, and the three probe processes start staggered -- so "five seconds
## after this process attached" is a different tick in each of them, and the client's last sample landed after
## the server's cloak had already expired. Rows then legitimately resumed, and the row assertion was measuring
## a window in which nothing was withheld. Re-forcing is idempotent (`queue_cloak()` refuses while the fighter
## is already cloaked) and it also closes the other end of the same race: a fighter that dies drops its cloak,
## and this puts it back the moment it respawns.
func _hold_cloak() -> void:
	if not Net.is_server() or _net.world == null:
		return
	var fighter: FighterBody = _net.world.fighter_at(CLOAK_SEAT)
	if fighter != null:
		fighter.queue_cloak()

## THE SHOTS go through the real command lane from the real client, because that is the only path that
## exercises validation, the rewind ring and the banded resolve end to end. A server firing on its own behalf
## would take no rewind at all, which is the one case the feature explicitly excludes.
func _keep_firing(delta: float) -> void:
	if Net.is_server() or _net.world == null or _net.is_observing():
		return
	_fire_accumulator += delta
	if _fire_accumulator < 0.35:
		return
	_fire_accumulator = 0.0
	# NOT FROM THE WATCHED ARENA, and this is what makes the veto assertion deterministic rather than a race.
	# A shot is resolved on the SERVER against the server's world, so this client can kill a fighter it is
	# being withheld -- and a dead fighter drops its cloak, which lifts the veto and legitimately resumes the
	# rows. The row assertion would then be measuring a window in which nothing was withheld. Nobody else
	# occupies the watched arena in a probe run, so declining to fire into it keeps the cloak alive for its
	# whole 300-tick life, which spans every sample.
	var quiet_arena: int = ArenaConfig.arena_of_seat(WATCHED_SEAT)
	var seats: PackedInt32Array = PackedInt32Array()
	for seat: int in _net.local_seats():
		if ArenaConfig.arena_of_seat(seat) != quiet_arena:
			seats.push_back(seat)
	_net.world.request_shots(seats)

func _settle() -> void:
	if _net.world == null:
		return
	_settled_watched = _freshness_of(WATCHED_SEAT)
	_settled_own = _freshness_of(_own_seat())
	_settled_sees_cloak = _sees_cloak()
	_left_events = _net.interest_log.left()

func _verdict() -> void:
	var role: String = _role()
	var ok: bool = true
	if _net.world == null:
		print("ARENA-PROBE-RESULT role=%s FAIL (no world)" % role)
		return

	print("ARENA-PROBE sig=%d" % _net.world.world_signature())
	if role == "server":
		ok = _server_verdict() and ok
	else:
		ok = _client_verdict(role) and ok
	print("ARENA-PROBE-RESULT role=%s %s" % [role, "PASS" if ok else "FAIL"])

# --- the server's facts ----------------------------------------------------------------------------
func _sample_server() -> void:
	if _net.world == null:
		return
	_hidden_peak = maxi(_hidden_peak, _net.world.hidden_total())
	var wire: Dictionary[String, float] = Net.bandwidth_metrics()
	_unproven_max = maxf(_unproven_max, wire["unproven_acks_s"])

func _server_verdict() -> bool:
	var meter: RewindMeter = _net.world.rewind
	print("ARENA-PROBE hidden_peak=%d cloak_forced=%d" % [_hidden_peak, 1 if _cloak_forced else 0])
	print("ARENA-PROBE unproven_max=%.3f" % _unproven_max)
	print("ARENA-PROBE shots=%d hits=%d" % [meter.shots(), meter.hits()])
	print("ARENA-PROBE rewind base=%.2f near=%.2f mid=%.2f far=%.2f spread=%d" % [
		meter.mean_base_ticks(),
		meter.mean_ticks(NetLagComp.Band.NEAR),
		meter.mean_ticks(NetLagComp.Band.MID),
		meter.mean_ticks(NetLagComp.Band.FAR),
		1 if meter.bands_differ() else 0])
	print("ARENA-PROBE watched_seat_cloaked=%d hidden_peak=%d" % [
		1 if _sees_cloak() else 0, _hidden_peak])
	print("ARENA-PROBE entities=%d props=%d" % [
		ArenaConfig.SEAT_COUNT + _net.world.prop_count() + ArenaConfig.ARENAS, _net.world.prop_count()])

	var ok: bool = true
	if not _cloak_forced:
		print("ARENA-PROBE server: the cloak could not be placed, so the veto was never exercised")
		ok = false
	if _hidden_peak <= 0:
		print("ARENA-PROBE server: no entity was ever withheld from any peer")
		ok = false
	if _unproven_max > 0.0:
		print("ARENA-PROBE server: a peer acknowledged a frame it was not provably sent")
		ok = false
	if meter.shots() <= 0:
		print("ARENA-PROBE server: no shot reached the resolver, so the rewind path never ran")
		ok = false
	return ok

# --- a client's facts ------------------------------------------------------------------------------
func _client_verdict(role: String) -> bool:
	var now: int = Net.current_tick()
	var per_arena: PackedInt32Array = _fighters_per_arena(now)
	print("ARENA-PROBE arenas_seen=%s" % per_arena)
	print("ARENA-PROBE seats=%s" % _net.local_seats())
	print("ARENA-PROBE early_rise=%d watched_rise=%d own_rise=%d" % [
		_baseline_watched - _early_watched,
		_settled_watched - _baseline_watched,
		_settled_own - _baseline_own])
	print("ARENA-PROBE interest_events left=%d watched_left=%d holding=%d" % [
		_left_events, 1 if _watched_left_event else 0, _net.interest_log.held()])
	print("ARENA-PROBE sees_cloak base=%d settled=%d" % [
		1 if _baseline_sees_cloak else 0, 1 if _settled_sees_cloak else 0])
	var reading: InterestMeter.Reading = InterestMeter.read(_net.world, now)
	print("ARENA-PROBE receiving=%d/%d" % [reading.total_fresh(), reading.total()])

	var ok: bool = true
	# THE STATE-ROW ASSERTION, and it is the one #26 asks for: a joining client's own body must be receiving
	# authoritative rows. `last_known_state()` FAILS OPEN on a backend that cannot answer, so the reading is a
	# RISE rather than a value -- a fallback that returns the present tick would satisfy a threshold test and
	# says nothing.
	if role == "client" and _settled_own <= _baseline_own:
		print("ARENA-PROBE client: this peer's own body received no new authoritative row")
		ok = false

	# THE PREDICTION ASSERTION. A client's own seat must be IN this peer's rollback loop, and the failure it
	# guards is silent by construction: a fighter registered before the roster arrived stays exempt, keeps
	# applying the server's rows, and therefore keeps moving -- so the row assertion above passes, the scene
	# looks correct, and every input the player gives is a full round trip late. Only the switch says so.
	if role == "client" and not _net.is_observing():
		for seat: int in _net.local_seats():
			var mine: FighterBody = _net.world.fighter_at(seat)
			if mine == null:
				continue
			print("ARENA-PROBE own seat %d predicted=%d" % [seat, 1 if mine.is_predicted() else 0])
			if not mine.is_predicted():
				print("ARENA-PROBE client: seat %d is this peer's own and it is not being predicted" % seat)
				ok = false

	# THE MEMBERSHIP ASSERTION. Every arena replicates the same LOCAL coordinates, so a radius cannot separate
	# them; an arena this peer holds no seat in must therefore be empty here, and it is membership that made
	# it so.
	var seated_arenas: PackedInt32Array = _seated_arenas()
	for offset: int in per_arena.size():
		var arena: int = ArenaConfig.FIRST_ARENA_ID + offset
		if seated_arenas.has(arena):
			if per_arena[offset] <= 0:
				print("ARENA-PROBE %s: arena %d holds a seat of ours but sent nothing" % [role, arena])
				ok = false
		elif per_arena[offset] > 0:
			print("ARENA-PROBE %s: received %d fighters from arena %d, which this peer is not in" % [
				role, per_arena[offset], arena])
			ok = false

	# THE VETO ASSERTION, IN TWO HALVES THAT FAIL FOR DIFFERENT REASONS.
	#
	# The first is the ROWS THEMSELVES, and it is assertable now that both lanes publish a receipt. The reading
	# this probe takes is `last_received_state()`, which has one writer on the receive path -- so for a body a
	# veto is withholding it does not move, and `watched_rise` is an assertion rather than a printed hint. It
	# used to read `get_last_known_state()`, a FRONTIER that also counts ticks this peer authored, and on the
	# rollback lane that rises whatever the wire did.
	#
	# The second is that the withheld peer never learned the FLAG. The cloak bit rides in `net_flags`, inside
	# the rows the veto is refusing, so a peer being sent those rows knows within a tick and a peer that is not
	# never finds out. Keeping both means a veto that stopped the rows for the wrong reason -- a cull, a
	# membership -- still has to explain itself, and it is also the fact the game is actually about.
	if role == "client" and _net.local_seats().has(WATCHED_SEAT):
		# A peer is never withheld its own body, so this run cannot say anything about the veto. It means the
		# seating order the shell script arranges did not hold, which is worth failing on rather than skipping.
		print("ARENA-PROBE client: this peer OWNS seat %d, so the veto cannot be observed from here" % [
			WATCHED_SEAT])
		ok = false
	elif role == "client" and seated_arenas.has(ArenaConfig.arena_of_seat(WATCHED_SEAT)):
		if _early_watched < 0 or _baseline_watched <= _early_watched:
			print("ARENA-PROBE client: seat %d was not arriving before it cloaked, so the veto proves nothing" % [
				WATCHED_SEAT])
			ok = false
		if _baseline_sees_cloak or _settled_sees_cloak:
			print("ARENA-PROBE client: this peer LEARNED that seat %d cloaked, which it is withheld" % [
				WATCHED_SEAT])
			ok = false
		# THE ROWS STOPPED, asserted directly. Both samples are taken after the cloak was forced, so a receipt
		# that moved between them is a row that arrived for a body this peer is withheld.
		if _settled_watched > _baseline_watched:
			print("ARENA-PROBE client: seat %d is withheld and still delivered rows (%d -> %d)" % [
				WATCHED_SEAT, _baseline_watched, _settled_watched])
			ok = false
		# AND THE POSITIVE CONTROL BESIDE IT, so "nothing arrived at all" cannot pass the line above. This
		# peer's own body is never withheld from it, and its receipt must move over the same window.
		if _settled_own <= _baseline_own:
			print("ARENA-PROBE client: this peer's own body stopped receiving too, so the veto proves nothing")
			ok = false
		# THE EVENT, which is the same fact arriving as an edge rather than as a threshold. The server computed
		# this diff to clear its own delta bookkeeping and now publishes it; a peer that was told nothing would
		# be back to inferring a cull from a tick that stopped moving, which cannot tell a cull from a budget
		# deferral or from packet loss.
		if not _watched_left_event:
			print("ARENA-PROBE client: seat %d is withheld and was never announced as leaving interest" % [
				WATCHED_SEAT])
			ok = false
	return ok

## Whether this peer knows the watched fighter is cloaked. On the server that is simply true while it is;
## on a client it is only true if the row carrying the flag reached it.
func _sees_cloak() -> bool:
	var fighter: FighterBody = _net.world.fighter_at(WATCHED_SEAT)
	return fighter != null and fighter.is_cloaked()

## Fighters whose rows are still arriving, per arena.
func _fighters_per_arena(now: int) -> PackedInt32Array:
	return InterestMeter.read(_net.world, now).fighters_by_arena

## The arenas this peer holds a seat in. An observer holds none, and its declared arena is what it should be
## receiving instead.
func _seated_arenas() -> PackedInt32Array:
	var out: PackedInt32Array = PackedInt32Array()
	if _net.is_observing():
		out.push_back(_net.observer.arena())
		return out
	for seat: int in _net.local_seats():
		var arena: int = ArenaConfig.arena_of_seat(seat)
		if not out.has(arena):
			out.push_back(arena)
	return out

func _own_seat() -> int:
	var seats: PackedInt32Array = _net.local_seats()
	return seats[0] if not seats.is_empty() else -1

## The RECEIPT tick for one seat's fighter -- the newest row this peer decoded for it, or -1 if none ever did.
##
## NOT `last_known_state()`, which is a FRONTIER: the newer of "a row arrived" and "this peer authored a tick".
## On a peer that authors state that number rises every tick whatever the wire did, so a rise in it proves
## nothing about the veto. The receipt has one writer, on the receive path, which is what lets the veto be
## asserted directly below rather than through the game-level flag this probe used to have to fall back on.
func _freshness_of(seat: int) -> int:
	var fighter: FighterBody = _net.world.fighter_at(seat)
	return -1 if fighter == null else fighter.last_received_state()
