extends RefCounted
class_name PacketImpairment
## The pure scheduling core of the OrbitNet UDP impairment relay (netbench). Given a stream of raw datagrams and
## the wall-clock time each arrived, it decides -- deterministically for a fixed seed -- whether to DROP each one,
## how long to DELAY it, and whether to DUPLICATE or REORDER it, then hands back the datagrams whose scheduled
## release time has arrived. It knows NOTHING about sockets, ENet, or the scene tree: the relay shell
## (relay_main.gd) owns the UDP plumbing and drives one PacketImpairment per direction per client, so this logic
## is unit-testable in isolation (the socket half is inherently a live integration and is exercised by the bench
## run, not a unit test).
##
## HONEST CONDITIONING: because the relay forwards RAW datagrams between the client and the server BELOW ENet's
## reliability layer, a drop here forces ENet's real retransmit, a reorder exercises its ordering buffers, and a
## delay drives the backend's clock discipline -- exactly the reference-design behavior (Unreal/Valve/Unity all inject
## at the socket layer). Dropping "reliable" traffic ABOVE reliability would be a lie (it is permanently lost with
## no retransmit); this core never sees or cares about reliability, it just moves bytes late/never/twice.
##
## DETERMINISM: all randomness comes from one seeded [RandomNumberGenerator] consumed in a FIXED order per packet
## (drop roll -> jitter -> reorder decision -> reorder jitter -> dup decision -> dup jitter), so the same seed +
## the same (payload, arrival-ms) sequence yields byte-identical drop/delay/dup decisions across runs and machines
## -- the property the unit suite pins and the reason a bench run is reproducible.

var _profile: NetProfile = NetProfile.new()
var _rng: RandomNumberGenerator = RandomNumberGenerator.new()
# The scheduled-delivery queue, kept sorted ascending by (release_ms, seq). Reordering is an EMERGENT property:
# a packet handed extra reorder delay simply sorts after later-arriving packets that were not, so they overtake it.
var _queue: Array[_Delivery] = []
var _seq: int = 0            # monotonic tiebreaker so equal release times keep insertion order (FIFO within a ms)
var _in_bad_state: bool = false   # Gilbert-Elliott state (only meaningful when _profile.burst)

# Lifetime counters for the relay's stat line (packets seen / dropped / duplicated on this direction).
var _stat_in: int = 0
var _stat_dropped: int = 0
var _stat_duped: int = 0

class _Delivery extends RefCounted:
	var release_ms: int = 0
	var seq: int = 0
	var payload: PackedByteArray = PackedByteArray()

## Configure the impairment from a profile + an explicit seed. Re-configurable at any time (the console/Steam path
## live-tunes; the relay sets it once at start). A distinct seed per direction per client keeps otherwise-identical
## links statistically independent yet each individually reproducible.
func configure(profile: NetProfile, seed: int) -> void:
	_profile = profile if profile != null else NetProfile.new()
	_rng.seed = seed
	_in_bad_state = false

## Offer one arrived datagram to the scheduler at wall-clock `now_ms`. It is dropped, or enqueued for delivery at
## now_ms + a sampled delay (plus a possible reordering penalty), and possibly a duplicate is enqueued with its own
## independent delay. Nothing is returned -- delivered packets come out of [method poll].
func push(payload: PackedByteArray, now_ms: int) -> void:
	_stat_in += 1
	if _drop_roll():
		_stat_dropped += 1
		return
	_enqueue(now_ms + _delay_sample(), payload)
	if _profile.dup > 0.0 and _rng.randf() < _profile.dup:
		_stat_duped += 1
		_enqueue(now_ms + _delay_sample(), payload)   # the duplicate rides its OWN delay, so it lands off the original

## Return every datagram whose scheduled release time is <= now_ms, in delivery order (ascending release, then
## arrival order), removing them from the queue. Called every relay iteration with the current wall clock.
func poll(now_ms: int) -> Array[PackedByteArray]:
	var out: Array[PackedByteArray] = []
	while not _queue.is_empty() and _queue[0].release_ms <= now_ms:
		var d: _Delivery = _queue.pop_front()
		out.push_back(d.payload)
	return out

## Datagrams still waiting in the queue (not yet released). The relay drains these on shutdown / uses it to know
## when a direction has gone quiet.
func pending() -> int:
	return _queue.size()

## Lifetime stats for the relay's `RELAY:` marker line: {in, dropped, duped, pending}.
func stats() -> Dictionary[String, int]:
	return {"in": _stat_in, "dropped": _stat_dropped, "duped": _stat_duped, "pending": _queue.size()}

# --- internals -----------------------------------------------------------------------------------
# Decide whether THIS packet is dropped. Uniform model by default; the Gilbert-Elliott two-state model when the
# profile enables burst loss. Consumes exactly one randf() (uniform) or two (burst: loss roll then transition
# roll) -- a fixed count per branch, so determinism holds regardless of which branch the profile selects.
func _drop_roll() -> bool:
	if _profile.burst:
		var loss: float = _profile.burst_loss_bad if _in_bad_state else _profile.burst_loss_good
		var dropped: bool = _rng.randf() < loss
		if _in_bad_state:
			if _rng.randf() < _profile.burst_bad_to_good:
				_in_bad_state = false
		else:
			if _rng.randf() < _profile.burst_good_to_bad:
				_in_bad_state = true
		return dropped
	if _profile.loss <= 0.0:
		return false
	return _rng.randf() < _profile.loss

# Sample a delivery delay (ms, >= 0): base latency +/- uniform jitter, plus a reorder penalty on a fraction of
# packets. Fixed roll order (jitter, then reorder decision, then reorder jitter is folded into the flat penalty)
# for determinism.
func _delay_sample() -> int:
	var delay: float = _profile.latency_ms
	if _profile.jitter_ms > 0.0:
		delay += _rng.randf_range(-_profile.jitter_ms, _profile.jitter_ms)
	if _profile.reorder > 0.0 and _rng.randf() < _profile.reorder:
		delay += _profile.reorder_ms
	return maxi(0, int(roundf(delay)))

# Insert a delivery keeping _queue sorted ascending by (release_ms, seq). A backward linear scan from the tail --
# arrivals cluster near the current time so the insertion point is usually at/near the end (cheap for bench volumes).
func _enqueue(release_ms: int, payload: PackedByteArray) -> void:
	var d: _Delivery = _Delivery.new()
	d.release_ms = release_ms
	d.seq = _seq
	d.payload = payload
	_seq += 1
	var i: int = _queue.size()
	while i > 0 and _is_after(_queue[i - 1], d):
		i -= 1
	_queue.insert(i, d)

# True when `a` should be delivered strictly AFTER `b` (later release, or same release but a later arrival).
func _is_after(a: _Delivery, b: _Delivery) -> bool:
	if a.release_ms != b.release_ms:
		return a.release_ms > b.release_ms
	return a.seq > b.seq
