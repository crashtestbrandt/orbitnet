extends RefCounted
class_name NetProfiles
## The shipped catalog of named [NetProfile] network conditions for the OrbitNet bench (netbench). ONE source of
## truth read by every consumer -- the ENet UDP relay (`--relay-profile=<name>`), the Steam FakePacket* conditioner
## (`net.sim_profile <name>`), and the bench harness gates (each profile's rtt_estimate_ms is the "did the injected
## latency actually show up" reference). Add a profile here and it is available to all of them at once.
##
## The numbers are ONE-WAY (round-trip ~2x) and are calibrated from published vendor/engineering references, so the
## bench tests the same conditions AAA studios design against:
##   * Unity Multiplayer Tools' Network Simulator presets (Home Fiber/Cable/DSL/Broadband, Mobile 3G/4G/5G) --
##     the most concrete public "standard condition" table from an engine vendor.
##   * Overwatch's 250ms lag-compensation ceiling (GDC 2017) -- the canonical worst-case a shooter is built to.
##   * Gears of War 3's "conditioner forced on in daily builds", with an extreme 400ms+/10%-loss profile reserved
##     for deliberate programmer torture sessions.
## PURE (no scene / socket dependency): a static catalog, unit-tested directly.

# The catalog, built lazily once. Values are ONE-WAY ms / [0,1] probabilities. Keep names lowercase-with-underscores
# (they are CLI-arg and console-token safe).
static var _catalog: Dictionary[String, NetProfile] = {}

## Every profile name in the catalog, sorted (for `--relay-profile` help, `net.sim_profile` completion, and the
## harness's full-matrix loop).
static func names() -> PackedStringArray:
	_ensure_built()
	var keys: Array = _catalog.keys()
	keys.sort()
	var out: PackedStringArray = PackedStringArray()
	for k: String in keys:
		out.push_back(k)
	return out

## The named profile as a FRESH COPY (so a caller may tweak a knob without mutating the shared catalog), or null
## if the name is unknown. Callers surface the unknown case as an error rather than silently conditioning nothing.
static func get_profile(name: String) -> NetProfile:
	_ensure_built()
	if not _catalog.has(name):
		return null
	return _catalog[name].duplicate_profile()

## Whether a profile name exists (for arg validation before a run commits to it).
static func has(name: String) -> bool:
	_ensure_built()
	return _catalog.has(name)

static func _ensure_built() -> void:
	if not _catalog.is_empty():
		return
	# clean: the control -- zero impairment, so a bench run under `clean` isolates the harness/engine overhead from
	# any conditioning effect (a run that fails on `clean` is a harness bug, not a netcode-under-loss finding).
	_add(_make("clean", 0.0, 0.0, 0.0))
	# lan: a hair of delay, no loss -- a switched wired LAN. The floor real play ever sees.
	_add(_make("lan", 1.0, 0.0, 0.0))
	# broadband: healthy home fiber/cable -- Unity's Home Fiber/Cable cluster (10-30ms) with a token 0.5% loss.
	_add(_make("broadband", 30.0, 10.0, 0.005))
	# congested_wifi: the everyday bad case -- shared/contended home wifi. High jitter is the story here, not mean
	# latency; this is the profile to default HUMAN playtests to (Gears of War 3 practice) so feel is never tuned
	# on loopback. Unity's "Home Broadband Congested" is 50/50/1%.
	_add(_make("congested_wifi", 50.0, 50.0, 0.02))
	# mobile_4g: LTE -- Unity's Mobile 4G preset (100/20/4%). Relevant wherever ENet-over-UDP reaches mobile.
	_add(_make("mobile_4g", 100.0, 20.0, 0.04))
	# mobile_3g: degraded cellular -- Unity's 3G preset (360/30/7%), a genuinely hostile link.
	_add(_make("mobile_3g", 300.0, 30.0, 0.07))
	# cross_region: a well-provisioned but distant server (e.g. NA-east client on an EU server) -- steady 150ms,
	# low jitter/loss. Tests prediction/reconcile under real one-way lead without a degraded link.
	_add(_make("cross_region", 150.0, 15.0, 0.01))
	# worst_case: the ceiling a shooter is DESIGNED to still function at -- Overwatch stops lag-compensating past
	# 250ms. A per-PR-adjacent gate should stay green here; beyond it, "shoot where you see them" degrades by design.
	_add(_make("worst_case", 250.0, 25.0, 0.05))
	# worst_case_burst: worst_case latency but with BURSTY (Gilbert-Elliott) loss instead of uniform -- a congested
	# link that drops runs of packets. Stresses ENet retransmit + reconcile snap handling that uniform loss hides.
	var b: NetProfile = _make("worst_case_burst", 250.0, 25.0, 0.0)
	b.burst = true
	b.burst_good_to_bad = 0.02     # ~1 in 50 packets tips the link into a bad run while healthy
	b.burst_bad_to_good = 0.30     # bad runs recover after ~3 packets on average
	b.burst_loss_good = 0.005      # near-clean while good
	b.burst_loss_bad = 0.50        # half the packets vanish during a bad run
	_add(b)
	# torture: deliberate programmer-pain (Gears of War 3's extreme profile) -- 450ms RTT-ish one-way, 10% loss.
	# NOT a gate; a stress lens to see what shakes loose. Real play should never hit this.
	_add(_make("torture", 450.0, 50.0, 0.10))

static func _make(name: String, latency_ms: float, jitter_ms: float, loss: float) -> NetProfile:
	var p: NetProfile = NetProfile.new()
	p.name = name
	p.latency_ms = latency_ms
	p.jitter_ms = jitter_ms
	p.loss = loss
	return p

static func _add(p: NetProfile) -> void:
	_catalog[p.name] = p
