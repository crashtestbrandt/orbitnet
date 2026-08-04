extends RefCounted
class_name NetConditioner
## The live, in-process network conditioner facade for the OrbitNet bench (netbench) -- the `net.sim_*` console
## surface. It holds the current impairment parameters (so the cvars round-trip) and, on a build whose transport
## can condition itself BELOW reliability, pushes them to that transport. Today that means the STEAM transport:
## SteamNetworkingSockets ships FakePacket* config values applied at the raw-UDP layer (below its SNP reliability),
## and the vendored GodotSteam exposes them -- so `net.sim_profile congested_wifi` live-conditions a Steam session
## with zero external tooling, exactly how Valve tests.
##
## ENet has NO in-process below-reliability seam (wrapping the MultiplayerPeer sits ABOVE ENet's retransmit, so
## dropping "reliable" traffic there would be a lie -- see the research). The honest ENet conditioner is therefore
## the EXTERNAL relay (addons/orbitnet/bench/relay_main.gd, driven by tools/netbench): a client just joins the
## relay's port. So on an ENet build these cvars STORE their values (round-trip) but report that conditioning runs
## through the relay -- they are not silently pretending to impair the wire. One vocabulary ([NetProfiles]) drives
## both paths, so the numbers you set here and the numbers the relay injects are the same catalog.
##
## SEMANTICS (matched to the relay so both transports condition identically): latency/loss/reorder/dup are applied
## on each peer's SEND side only (Lag_Recv = 0), so a one-way trip carries exactly one hop of each -- one-way
## latency = the profile's latency_ms, RTT ~= 2x, and a round trip faces loss both ways, just like the relay's
## independent up/down impairment.

# The live conditioner state (also what the cvars read/write). Starts clean (no impairment).
static var _state: NetProfile = NetProfile.new()

## Whether THIS build can condition its own transport in-process (i.e. the Steam transport is active). ENet returns
## false -- use the relay. Callers surface this so the console can tell the user which path is live.
static func supported() -> bool:
	return NetTransport.preferred_kind() == NetTransport.Kind.STEAM

## The current conditioner parameters (the cvar getters read this; a copy so callers can't mutate the state).
static func current() -> NetProfile:
	return _state.duplicate_profile()

# --- per-knob setters (the net.sim_* cvars route here); each re-pushes to the active transport ----
# A hand-tuned knob no longer matches any named profile, so relabel the state "custom" -- otherwise net.sim_status
# would misattribute the tweaked numbers to whatever profile was last applied.
static func set_latency_ms(ms: float) -> void:
	_state.latency_ms = maxf(0.0, ms)
	_state.name = "custom"
	_reapply()

static func set_jitter_ms(ms: float) -> void:
	_state.jitter_ms = maxf(0.0, ms)
	_state.name = "custom"
	_reapply()

static func set_loss(p: float) -> void:
	_state.loss = clampf(p, 0.0, 1.0)
	_state.name = "custom"
	_reapply()

static func set_dup(p: float) -> void:
	_state.dup = clampf(p, 0.0, 1.0)
	_state.name = "custom"
	_reapply()

static func set_reorder(p: float) -> void:
	_state.reorder = clampf(p, 0.0, 1.0)
	_state.name = "custom"
	_reapply()

static func set_reorder_ms(ms: float) -> void:
	_state.reorder_ms = maxf(0.0, ms)
	_state.name = "custom"
	_reapply()

## Load a named [NetProfiles] profile into the live conditioner and apply it. Returns true if the name was known.
static func apply_profile(name: String) -> bool:
	var p: NetProfile = NetProfiles.get_profile(name)
	if p == null:
		return false
	# Keep the catalog's name so `current().name` reflects what's active; the burst model rides along too.
	_state = p
	_reapply()
	return true

## Clear all impairment (net.sim_off) and push the cleared state to the transport.
static func clear() -> void:
	_state = NetProfiles.get_profile("clean")
	if _state == null:
		_state = NetProfile.new()
	_reapply()

# Push the current state to the active transport's below-reliability conditioner. No-op on a transport without
# one (ENet): the state is still held so the cvars round-trip, and the console prints the relay guidance instead.
static func _reapply() -> void:
	if not supported():
		return
	# The Steam-blind boundary: only steam_transport.gd names Steamworks. It maps our one-way SEND-side semantics
	# onto the FakePacket* config values (loss/reorder/dup are percentages there; latency/jitter are ms).
	SteamTransport.service().apply_fake_conditions(
		_state.latency_ms, _state.jitter_ms, _state.loss * 100.0, _state.reorder * 100.0,
		_state.reorder_ms, _state.dup * 100.0)
