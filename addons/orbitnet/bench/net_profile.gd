extends RefCounted
class_name NetProfile
## One named network-condition profile for the OrbitNet bench (the netcode test bench, netbench). A profile is
## a pure data bundle of ONE-WAY link impairment parameters -- latency, jitter, loss, duplication, reordering,
## and an optional Gilbert-Elliott bursty-loss model -- that BOTH consumers of the bench read from one source:
##   * the ENet path: tools/netbench feeds a profile to the UDP impairment relay (addons/orbitnet/bench/
##     relay_main.gd), which delays/drops raw datagrams BELOW ENet's reliability layer -- so ENet's real
##     retransmit / ordering / congestion logic is genuinely exercised (an honest conditioner, per the AAA
##     reference designs: Source net_fakelag, Unreal PacketSimulationSettings, Valve SNS FakePacket*).
##   * the Steam path: the same numbers drive SteamNetworkingSockets' built-in FakePacket* config values via
##     the transport's conditioner seam (steam_transport.gd) -- also below reliability, so the two transports
##     condition identically.
##
## All delays are ONE-WAY milliseconds; round-trip is ~2x (see rtt_estimate_ms). Loss/dup/reorder are
## probabilities in [0,1]. The catalog of shipped profiles lives in [NetProfiles]; this is just the record.
## PURE (no scene / socket / autoload dependency) -- constructed from plain data and unit-tested directly.

var name: String = "custom"
var latency_ms: float = 0.0        # base one-way delay added to every packet
var jitter_ms: float = 0.0         # +/- uniform variation around latency_ms (delay never goes below 0)
var loss: float = 0.0              # per-packet drop probability [0,1] (uniform model; ignored when burst=true)
var dup: float = 0.0               # per-packet duplication probability [0,1] (the dup rides its own delay sample)
var reorder: float = 0.0           # probability [0,1] a packet gets reorder_ms EXTRA delay (later packets overtake it)
var reorder_ms: float = 0.0        # extra delay applied to a reordered packet (Steam FakePacketReorder_Time analog)

# Optional Gilbert-Elliott (gemodel) bursty loss -- the two-state Markov model tc-netem exposes as `loss gemodel`.
# When burst is true it REPLACES the uniform `loss` above: real links lose packets in bursts (a congested moment
# drops a run of datagrams), which stresses ENet's retransmit + the rollback reconcile smoother differently than
# evenly-spread loss. Per-packet transitions between a good and bad state, each with its own drop probability.
var burst: bool = false
var burst_good_to_bad: float = 0.0   # P(enter the bad/lossy state) evaluated per packet while in the good state
var burst_bad_to_good: float = 0.0   # P(leave the bad state back to good) evaluated per packet while in the bad state
var burst_loss_good: float = 0.0     # drop probability while in the good state
var burst_loss_bad: float = 0.0      # drop probability while in the bad state

## A rough round-trip estimate (ms) for the metrics gate's "does measured RTT reflect the injected profile" check:
## impairment is applied in BOTH directions (client->server and server->client each carry latency_ms), so the
## nominal RTT the game's clock sampler should observe is ~2x the one-way latency. Jitter widens the distribution
## but not the mean, so it is excluded from the point estimate.
func rtt_estimate_ms() -> float:
	return 2.0 * latency_ms

## A compact human summary for log/marker lines (the relay + bench harness echo it so a run's conditions are
## self-documenting in the artifacts).
func describe() -> String:
	var base: String = "%s: %.0f/%.0fms loss=%.1f%% dup=%.1f%% reorder=%.1f%%" % [
		name, latency_ms, jitter_ms, loss * 100.0, dup * 100.0, reorder * 100.0]
	if burst:
		base += " burst(g2b=%.3f b2g=%.3f lg=%.1f%% lb=%.1f%%)" % [
			burst_good_to_bad, burst_bad_to_good, burst_loss_good * 100.0, burst_loss_bad * 100.0]
	return base

## Serialize to a plain Dictionary (var_to_bytes-friendly, all floats/bool/String) so a profile can ride a CLI
## arg blob or a metrics artifact header. Round-trips losslessly through [method from_dict].
func to_dict() -> Dictionary[String, Variant]:
	return {
		"name": name,
		"latency_ms": latency_ms,
		"jitter_ms": jitter_ms,
		"loss": loss,
		"dup": dup,
		"reorder": reorder,
		"reorder_ms": reorder_ms,
		"burst": burst,
		"burst_good_to_bad": burst_good_to_bad,
		"burst_bad_to_good": burst_bad_to_good,
		"burst_loss_good": burst_loss_good,
		"burst_loss_bad": burst_loss_bad,
	}

## Restore every field from a [method to_dict] snapshot. Missing keys keep the constructor default (forward-
## compatible: an older artifact simply leaves newer knobs at zero). Values are assigned through typed locals
## (never an as-cast of the Variant) per the project's strict-typing rule.
func from_dict(d: Dictionary) -> void:
	name = _s(d, "name", name)
	latency_ms = _f(d, "latency_ms", latency_ms)
	jitter_ms = _f(d, "jitter_ms", jitter_ms)
	loss = _f(d, "loss", loss)
	dup = _f(d, "dup", dup)
	reorder = _f(d, "reorder", reorder)
	reorder_ms = _f(d, "reorder_ms", reorder_ms)
	burst = _b(d, "burst", burst)
	burst_good_to_bad = _f(d, "burst_good_to_bad", burst_good_to_bad)
	burst_bad_to_good = _f(d, "burst_bad_to_good", burst_bad_to_good)
	burst_loss_good = _f(d, "burst_loss_good", burst_loss_good)
	burst_loss_bad = _f(d, "burst_loss_bad", burst_loss_bad)

## A deep copy -- so a caller can start from a catalog profile and tweak one knob without mutating the shared
## catalog instance (the catalog hands out copies for exactly this reason).
func duplicate_profile() -> NetProfile:
	var p: NetProfile = NetProfile.new()
	p.from_dict(to_dict())
	return p

func _f(d: Dictionary, key: String, fallback: float) -> float:
	if not d.has(key):
		return fallback
	var v: Variant = d[key]
	if v is float or v is int:
		var f: float = v
		return f
	return fallback

func _b(d: Dictionary, key: String, fallback: bool) -> bool:
	if not d.has(key):
		return fallback
	var v: Variant = d[key]
	if v is bool:
		var b: bool = v
		return b
	return fallback

func _s(d: Dictionary, key: String, fallback: String) -> String:
	if not d.has(key):
		return fallback
	var v: Variant = d[key]
	if v is String:
		var s: String = v
		return s
	return fallback
