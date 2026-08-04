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
##   6. Worst observed STALENESS, in ticks, stays under bound. This is the only assertion that catches units
##      silently starving past the send budget: with more entities than fit in one frame the backend serves
##      them stalest-first, so over-subscription does not drop anyone, it ages everyone. Measured here on the
##      RECEIVING side by watching how long a unit goes without its replicated state changing.
##
## Note on 6: the facade does not publish a staleness metric (a library-side counter is filed as an issue),
## so this measures it from the outside, which is what a consumer would have to do today.

const _ORDER_AT_S: float = 3.0
const _FORGE_AT_S: float = 4.5
const _REPORT_AT_S: float = 13.0
const _ORDER_UNIT_COUNT: int = 8
## The id the CLIENT will illegally try to order. Deliberately in seat 0's block and outside the range the
## host legitimately orders, so a moved sequence can only mean the forgery was accepted.
const _FORGED_ID: int = 40
## Ticks of staleness to tolerate. 96 units at ~46 per frame is a ~2-tick rotation; 20 leaves room for
## scheduling noise on a loaded CI runner while still failing a genuine starvation.
const _MAX_STALENESS_TICKS: int = 20

var _main: RtsMain = null
var _elapsed: float = 0.0
var _ordered: bool = false
var _forged: bool = false
var _reported: bool = false

var _ordered_ids: PackedInt32Array = PackedInt32Array()
var _seq_before: PackedInt32Array = PackedInt32Array()
var _forged_seq_before: int = -1

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
	_track_staleness()

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
	# Seat 0's channel, submitted by the peer holding seat 1. The server resolves the sender's seat from the
	# transport-supplied sender id, sees it does not match the channel, and refuses before parsing further.
	world.submit_order(0, OrderValidator.VERB_MOVE, ids, Vector3(0.0, 0.0, -30.0))
	print("RTS-PROBE forged an order on seat 0's channel while holding seat %d" % seat)

# --- staleness --------------------------------------------------------------------------------------
func _track_staleness() -> void:
	var world: WorldDirector = _main.net.world
	var tick: int = Net.current_tick()
	if tick <= 0:
		return
	if _last_position.size() != world.units.size():
		_last_position.resize(world.units.size())
		_last_change_tick.resize(world.units.size())
		for id: int in world.units.size():
			_last_change_tick[id] = tick
	for id: int in world.units.size():
		var unit: UnitBody = world.units[id]
		if unit == null or not unit.is_alive():
			# A dead unit legitimately stops changing; counting it would report the respawn timer as
			# staleness. Reset its clock instead.
			_last_change_tick[id] = tick
			continue
		if unit.position.distance_squared_to(_last_position[id]) > 0.000001:
			_last_position[id] = unit.position
			_last_change_tick[id] = tick
			continue
		_worst_staleness = maxi(_worst_staleness, tick - _last_change_tick[id])

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

	# 6: staleness.
	print("RTS-PROBE worst_staleness_ticks=%d" % _worst_staleness)
	if _worst_staleness > _MAX_STALENESS_TICKS:
		failures.push_back("worst staleness %d ticks exceeds the %d-tick bound -- units are starving past the "
			% [_worst_staleness, _MAX_STALENESS_TICKS] + "send budget")

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
