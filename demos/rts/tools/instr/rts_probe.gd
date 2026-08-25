extends Node
## The RTS demo's automated gate. Attached by `--rts-probe`; runs on BOTH peers of a two-process session and
## prints greppable markers that tools/rts-probe.sh compares across the two logs.
##
## WHAT IT ASSERTS, AND WHY EACH ONE IS SCALE-FREE OR TICK-DOMAIN:
##
##   1. The session is reached at all.
##   2. The two peers' WORLD SIGNATURES are identical. This is the direct gate on deterministic node naming,
##      and therefore on entity-id agreement -- the failure that is otherwise silent (the server broadcasts
##      entity 0x8f3a..., the client listens for 0x21bc..., nothing errors, nothing moves).
##   3. An order REPLICATES: the server's sequence number reaches every targeted unit on the other peer.
##   4. The two peers agree about where the army is, by CENTROID within a couple of metres. Deliberately not
##      a per-unit position hash: peers observe different ticks by construction, so an exact comparison is
##      inherently flaky, whereas a centroid drifting apart means replication has genuinely failed.
##   5. A FORGED order -- submitted on another seat's channel -- changes nothing. The unit it named must
##      never see its sequence move.
##   6. The worst REFRESH INTERVAL of an actively-moving unit stays under bound -- the longest gap between
##      consecutive updates. This is what catches units silently starving past the send budget: with more
##      entities than fit in one frame the backend serves them stalest-first, so over-subscription does not
##      drop anyone, it ages everyone.
##
## Note on 6: the facade publishes no state-lane health metric, so this is measured from the outside, which
## is what a consumer has to do today. Measuring it as "ticks since this unit last changed" would be wrong --
## a stationary unit would report the whole run length. See _track_refresh_interval.

const _ORDER_AT_S: float = 3.0
const _FORGE_AT_S: float = 4.5
const _REPORT_AT_S: float = 13.0
const _ORDER_UNIT_COUNT: int = 8
## The id the CLIENT will illegally try to order. Deliberately in seat 0's block and outside the range the
## host legitimately orders, so a moved sequence can only mean the forgery was accepted.
const _FORGED_ID: int = 40
## Ticks of refresh interval to tolerate. 96 units at ~46 per frame is a ~2-tick rotation; 20 leaves room for
## scheduling noise on a loaded CI runner while still failing a genuine starvation.
const _MAX_STALENESS_TICKS: int = 20
## The window in which the ordered units are provably in motion: after the order lands, before they arrive.
const _MEASURE_FROM_S: float = _ORDER_AT_S + 1.0
const _MEASURE_UNTIL_S: float = _ORDER_AT_S + 6.0

var _main: RtsMain = null
var _elapsed: float = 0.0
var _ordered: bool = false
var _forged: bool = false
var _reported: bool = false

var _ordered_ids: PackedInt32Array = PackedInt32Array()
var _seq_before: PackedInt32Array = PackedInt32Array()
var _forged_seq_before: int = -1
# The tag the forged order was submitted under, and the reason code that came back for it. -1 means no reply
# has arrived, which for a forgery is itself a failure: the client is entitled to be told it was refused.
var _forged_tag: int = 0
var _forged_code: int = -1

# Per-unit last-change bookkeeping for the staleness measurement.
var _last_position: PackedVector3Array = PackedVector3Array()
var _last_change_tick: PackedInt32Array = PackedInt32Array()
var _worst_staleness: int = 0

func _ready() -> void:
	process_mode = Node.PROCESS_MODE_ALWAYS
	var parent: Node = get_parent()
	if parent is RtsMain:
		var typed: RtsMain = parent
		_main = typed
	print("RTS-PROBE attached")

func _physics_process(delta: float) -> void:
	if _main == null or _main.net == null or _main.net.world == null:
		return
	if _main.net.state() != RtsNet.State.PLAYING:
		return
	_elapsed += delta
	_track_refresh_interval()

	if not _ordered and _elapsed >= _ORDER_AT_S and Net.is_server():
		_issue_real_order()
	if not _forged and _elapsed >= _FORGE_AT_S and not Net.is_server() and not Net.is_offline():
		_issue_forged_order()
	if not _reported and _elapsed >= _REPORT_AT_S:
		_report()

# --- the real order (server side) -------------------------------------------------------------------
func _issue_real_order() -> void:
	_ordered = true
	var world: WorldDirector = _main.net.world
	var ids: PackedInt32Array = PackedInt32Array()
	for offset: int in _ORDER_UNIT_COUNT:
		ids.push_back(RtsConfig.first_id_of_seat(0) + offset)
	# A destination well away from the spawn blob, so "did they move" is unambiguous.
	world.submit_order(0, OrderValidator.VERB_MOVE, ids, Vector3(0.0, 0.0, 20.0))
	print("RTS-PROBE ordered %d units" % ids.size())

# --- the forgery (client side) ----------------------------------------------------------------------
func _issue_forged_order() -> void:
	_forged = true
	var world: WorldDirector = _main.net.world
	var seat: int = _main.net.local_seat()
	if seat == 0:
		# Nothing to forge: this peer legitimately holds seat 0. Report the case rather than silently
		# skipping it, so a probe run that never exercised the forgery is visible in the log.
		print("RTS-PROBE forgery skipped (this peer holds seat 0)")
		_forged_seq_before = -1
		return
	var unit: UnitBody = world.units[_FORGED_ID]
	_forged_seq_before = unit.order_seq() if unit != null else 0
	var ids: PackedInt32Array = PackedInt32Array()
	ids.push_back(_FORGED_ID)
	# Listen for the refusal before sending it: the reply is a reliable RPC and arrives on its own schedule,
	# and a listener attached afterwards would be a race the probe could lose intermittently.
	var channel: NetCommand = world.order_channel(0)
	if channel != null and not channel.rejected.is_connected(_on_forged_rejected):
		channel.rejected.connect(_on_forged_rejected)
	# Seat 0's channel, submitted by the peer holding seat 1. The server resolves the sender's seat from the
	# transport-supplied sender id, sees it does not match the channel, and refuses before parsing further.
	_forged_tag = world.submit_order(0, OrderValidator.VERB_MOVE, ids, Vector3(0.0, 0.0, -30.0))
	print("RTS-PROBE forged an order on seat 0's channel while holding seat %d" % seat)

# The server's refusal, arriving on the peer that forged the order. Keyed on the TAG rather than the verb, so
# an unrelated refusal cannot satisfy the assertion.
func _on_forged_rejected(_verb: StringName, code: int, tag: int) -> void:
	if tag == _forged_tag:
		_forged_code = code

# --- refresh interval --------------------------------------------------------------------------------
# The longest GAP BETWEEN CONSECUTIVE UPDATES of a unit that is actively being updated.
#
# The obvious formulation -- "ticks since this unit's position last changed" -- measures the wrong thing, and
# measures it confidently. A unit standing still never changes position, so it accumulates the entire run
# length and reports hundreds of ticks of "staleness" while the netcode is behaving perfectly. Only 8 of 96
# units are under orders here; the other 88 are stationary by design.
#
# Recording only ON a change fixes it: a stationary unit produces no second sample and contributes nothing,
# while a moving unit yields exactly the round-robin interval the send budget governs. On the server that is
# ~1 tick (it writes every tick); on a client it is the stalest-first rotation, and it is what climbs if
# entities starve.
#
# Measured over the ORDERED units only, inside a window where they are provably in motion -- after the order
# lands and before they arrive and legitimately stop. Outside that window a "gap" would just be a unit that
# had nothing to say.
#
# NOTE: this is an outside-in approximation. A true per-entity staleness counter belongs in the library
# (filed as a gap: the facade publishes no state-lane health metrics), and a consumer cannot see the
# difference between "not sent" and "sent, unchanged" without it.
func _track_refresh_interval() -> void:
	var world: WorldDirector = _main.net.world
	var tick: int = Net.current_tick()
	if tick <= 0:
		return
	if _elapsed < _MEASURE_FROM_S or _elapsed > _MEASURE_UNTIL_S:
		return
	if _last_position.size() != world.units.size():
		_last_position.resize(world.units.size())
		_last_change_tick.resize(world.units.size())
		for id: int in world.units.size():
			_last_change_tick[id] = -1
	for offset: int in _ORDER_UNIT_COUNT:
		var id: int = RtsConfig.first_id_of_seat(0) + offset
		var unit: UnitBody = world.units[id]
		if unit == null or not unit.is_alive():
			_last_change_tick[id] = -1   # a corpse says nothing; do not bridge a gap across its death
			continue
		if unit.position.distance_squared_to(_last_position[id]) <= 0.000001:
			continue
		_last_position[id] = unit.position
		if _last_change_tick[id] >= 0:
			_worst_staleness = maxi(_worst_staleness, tick - _last_change_tick[id])
		_last_change_tick[id] = tick

# --- the verdict --------------------------------------------------------------------------------------
func _report() -> void:
	_reported = true
	var world: WorldDirector = _main.net.world
	var role: String = Net.mode_name(Net.current_mode())
	var failures: PackedStringArray = PackedStringArray()

	# 1 + 2: the session, and the world both peers built.
	print("RTS-PROBE sig=%d" % world.world_signature())

	# 3: the order reached every unit it named, on THIS peer.
	var moved: int = 0
	for offset: int in _ORDER_UNIT_COUNT:
		var unit: UnitBody = world.units[RtsConfig.first_id_of_seat(0) + offset]
		if unit != null and unit.order_seq() > 0:
			moved += 1
	print("RTS-PROBE ordseq ok=%d/%d" % [moved, _ORDER_UNIT_COUNT])
	if moved != _ORDER_UNIT_COUNT:
		failures.push_back("only %d of %d ordered units carry a sequence" % [moved, _ORDER_UNIT_COUNT])

	# 4: where seat 0's army is, as a centroid. Compared ACROSS peers by the shell harness.
	var centroid: Vector3 = _centroid_of_seat(0)
	print("RTS-PROBE centroid=%.3f %.3f" % [centroid.x, centroid.z])

	# 5: the forgery changed nothing.
	var forged_ok: bool = true
	if _forged_seq_before >= 0:
		var unit: UnitBody = world.units[_FORGED_ID]
		var after: int = unit.order_seq() if unit != null else 0
		forged_ok = after == _forged_seq_before
		if not forged_ok:
			failures.push_back("a forged foreign-seat order was APPLIED (seq %d -> %d)"
				% [_forged_seq_before, after])
	print("RTS-PROBE forged_rejected=%d" % (1 if forged_ok else 0))

	# 5b: THE CLIENT WAS TOLD. Nothing happening is what a refused order and a lost packet look like from the
	# client's side, so "the sequence did not move" alone cannot tell a working refusal from a dropped one.
	# The reply names the tag the request was submitted under and carries the server's reason code.
	if _forged_seq_before >= 0:
		print("RTS-PROBE forged_refusal_code=%d" % _forged_code)
		if _forged_code != OrderValidator.Code.FOREIGN_CHANNEL:
			failures.push_back("the forged order was refused without telling the client (code %d, wanted %d)"
				% [_forged_code, OrderValidator.Code.FOREIGN_CHANNEL])

	# 6: the refresh interval.
	print("RTS-PROBE worst_refresh_interval_ticks=%d" % _worst_staleness)
	if _worst_staleness > _MAX_STALENESS_TICKS:
		failures.push_back("worst refresh interval %d ticks exceeds the %d-tick bound -- units are starving "
			% [_worst_staleness, _MAX_STALENESS_TICKS] + "past the send budget")

	if failures.is_empty():
		print("RTS-PROBE-RESULT role=%s PASS" % role)
	else:
		for reason: String in failures:
			print("RTS-PROBE-FAIL %s" % reason)
		print("RTS-PROBE-RESULT role=%s FAIL" % role)

func _centroid_of_seat(seat: int) -> Vector3:
	var world: WorldDirector = _main.net.world
	var sum: Vector3 = Vector3.ZERO
	var count: int = 0
	var first: int = RtsConfig.first_id_of_seat(seat)
	for offset: int in RtsConfig.UNITS_PER_SEAT:
		var unit: UnitBody = world.units[first + offset]
		if unit != null and unit.is_alive():
			sum += unit.position
			count += 1
	return sum / float(maxi(1, count))
