extends UnitTest
## PuckPhysics: rails, mallet strikes, the speed cap, and the substep that stops the puck tunnelling.
##
## THIS IS THE FUNCTION EVERY PEER RUNS. A server and a client that disagreed about it would disagree about
## the puck, which is the only thing in the demo everybody is watching -- so it is exercised here from plain
## data rather than only through a session.

const DT: float = 1.0 / 60.0

func _state(at: Vector3, moving: Vector3) -> PuckPhysics.State:
	return PuckPhysics.State.new(at, moving)

func _no_mallets() -> PackedVector3Array:
	return PackedVector3Array()

func test_a_free_puck_coasts_and_slows() -> void:
	var next: PuckPhysics.State = PuckPhysics.step(
		_state(Vector3.ZERO, Vector3(1.0, 0.0, 0.0)), _no_mallets(), _no_mallets(), DT)
	assert_true(next.position.x > 0.0, "it moves the way it was going")
	assert_true(next.velocity.length() < 1.0, "and the air cushion takes a little off")
	assert_eq(next.contacts, 0, "with nothing touched")

func test_damping_is_rate_independent() -> void:
	# pow(damping, dt) rather than a per-tick multiplier: the F1 lever changes the tick rate live, and a
	# per-tick decay would make the same puck travel further at 60 Hz than at 30.
	var fast: PuckPhysics.State = _state(Vector3.ZERO, Vector3(1.0, 0.0, 0.0))
	for _tick: int in 60:
		fast = PuckPhysics.step(fast, _no_mallets(), _no_mallets(), 1.0 / 60.0)
	var slow: PuckPhysics.State = _state(Vector3.ZERO, Vector3(1.0, 0.0, 0.0))
	for _tick: int in 30:
		slow = PuckPhysics.step(slow, _no_mallets(), _no_mallets(), 1.0 / 30.0)
	assert_almost_eq(fast.velocity.length(), slow.velocity.length(), 0.02,
		"a second of decay is a second of decay at either tick rate")

func test_a_side_rail_reflects() -> void:
	var past: float = HockeyConfig.HALF_WIDTH - HockeyConfig.PUCK_RADIUS + 0.05
	var bounced: PuckPhysics.State = PuckPhysics.resolve_rails(_state(
		Vector3(past, 0.0, 0.0), Vector3(2.0, 0.0, 0.0)))
	assert_true(bounced.velocity.x < 0.0, "the puck comes back off the rail")
	assert_true(bounced.position.x <= HockeyConfig.HALF_WIDTH - HockeyConfig.PUCK_RADIUS + 0.0001,
		"and is put back inside it")
	assert_true(absf(bounced.velocity.x) < 2.0, "losing energy, so a rally is not perpetual")
	assert_eq(bounced.contacts, 1, "and it reports the contact, which is what stops the view smoothing it")

func test_an_end_rail_reflects_outside_the_mouth() -> void:
	var past: float = HockeyConfig.HALF_LENGTH - HockeyConfig.PUCK_RADIUS + 0.05
	var wide: float = HockeyConfig.GOAL_HALF_WIDTH + 0.05
	var bounced: PuckPhysics.State = PuckPhysics.resolve_rails(_state(
		Vector3(wide, 0.0, past), Vector3(0.0, 0.0, 2.0)))
	assert_true(bounced.velocity.z < 0.0, "a shot wide of the post comes back")
	assert_eq(TableGeometry.scoring_team_at(bounced.position.z), -1, "and is not a goal")

func test_the_mouth_is_not_reflected() -> void:
	# Reflecting inside the mouth and reporting a goal separately would be two rules that have to agree about
	# the same edge. The puck simply travels through and the caller reads the result.
	var past: float = HockeyConfig.HALF_LENGTH + 0.02
	var through: PuckPhysics.State = PuckPhysics.resolve_rails(_state(
		Vector3(0.0, 0.0, past), Vector3(0.0, 0.0, 2.0)))
	assert_true(through.velocity.z > 0.0, "a puck in the mouth keeps going")
	assert_eq(TableGeometry.scoring_team_at(through.position.z), 0, "and it is team 0's goal")
	assert_eq(through.contacts, 0, "no rail was touched")

func test_a_stationary_mallet_returns_the_puck() -> void:
	var mallet: Vector3 = Vector3(0.0, 0.0, -0.5)
	var contact: float = HockeyConfig.PUCK_RADIUS + HockeyConfig.MALLET_RADIUS
	var positions: PackedVector3Array = PackedVector3Array([mallet])
	var velocities: PackedVector3Array = PackedVector3Array([Vector3.ZERO])
	var struck: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(mallet + Vector3(0.0, 0.0, contact * 0.5), Vector3(0.0, 0.0, -2.0)),
		positions, velocities)
	assert_true(struck.velocity.z > 0.0, "the puck comes back off it")
	assert_almost_eq(struck.position.distance_to(mallet), contact, 0.0001,
		"and is separated to exactly touching, never left overlapping")
	assert_eq(struck.contacts, 1, "one contact resolved")

func test_a_moving_mallet_adds_its_own_speed() -> void:
	var mallet: Vector3 = Vector3(0.0, 0.0, -0.5)
	var contact: float = HockeyConfig.PUCK_RADIUS + HockeyConfig.MALLET_RADIUS
	var puck_at: Vector3 = mallet + Vector3(0.0, 0.0, contact * 0.5)
	var positions: PackedVector3Array = PackedVector3Array([mallet])
	var still: PackedVector3Array = PackedVector3Array([Vector3.ZERO])
	var driving: PackedVector3Array = PackedVector3Array([Vector3(0.0, 0.0, 2.0)])
	var passive: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(puck_at, Vector3(0.0, 0.0, -1.0)), positions, still)
	var driven: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(puck_at, Vector3(0.0, 0.0, -1.0)), positions, driving)
	assert_true(driven.velocity.z > passive.velocity.z,
		"a mallet swung into the puck sends it away harder than one held still")

func test_a_mallet_moving_away_does_not_suck_the_puck_in() -> void:
	var mallet: Vector3 = Vector3(0.0, 0.0, -0.5)
	var contact: float = HockeyConfig.PUCK_RADIUS + HockeyConfig.MALLET_RADIUS
	var positions: PackedVector3Array = PackedVector3Array([mallet])
	var retreating: PackedVector3Array = PackedVector3Array([Vector3(0.0, 0.0, -3.0)])
	var result: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(mallet + Vector3(0.0, 0.0, contact * 0.9), Vector3.ZERO), positions, retreating)
	assert_true(result.velocity.z >= -0.0001,
		"a mallet pulling away leaves the puck alone rather than dragging it backward through itself")

func test_concentric_contact_has_a_deterministic_answer() -> void:
	# Exactly concentric has no contact normal. The answer is arbitrary; what matters is that it is the SAME
	# arbitrary answer on every peer, because two peers disagreeing here would disagree about the puck.
	var mallet: Vector3 = Vector3(0.1, 0.0, -0.4)
	var positions: PackedVector3Array = PackedVector3Array([mallet])
	var velocities: PackedVector3Array = PackedVector3Array([Vector3.ZERO])
	var first: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(mallet, Vector3.ZERO), positions, velocities)
	var second: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(mallet, Vector3.ZERO), positions, velocities)
	assert_vec_almost_eq(first.position, second.position, 0.000001, "the same inputs give the same answer")
	assert_true(first.position.distance_to(mallet) > 0.0, "and the puck is pushed clear rather than left inside")

func test_the_puck_never_tunnels_through_a_mallet() -> void:
	# The reason PUCK_SUBSTEPS exists. At the speed cap a single 60 Hz step moves the puck further than its own
	# diameter, so a one-shot overlap test would let it pass straight through -- a SIMULATION bug that looks
	# exactly like a netcode bug.
	var mallet: Vector3 = Vector3(0.0, 0.0, 0.0)
	var positions: PackedVector3Array = PackedVector3Array([mallet])
	var velocities: PackedVector3Array = PackedVector3Array([Vector3.ZERO])
	var start: Vector3 = Vector3(0.0, 0.0, -0.14)
	var next: PuckPhysics.State = PuckPhysics.step(
		_state(start, Vector3(0.0, 0.0, HockeyConfig.PUCK_MAX_SPEED)), positions, velocities, DT)
	assert_true(next.position.z <= mallet.z,
		"a puck at full speed is stopped by the mallet rather than found on the far side of it")
	assert_true(next.contacts > 0, "and the contact is reported")

func test_the_speed_cap_holds_through_a_strike() -> void:
	var mallet: Vector3 = Vector3(0.0, 0.0, -0.5)
	var contact: float = HockeyConfig.PUCK_RADIUS + HockeyConfig.MALLET_RADIUS
	var positions: PackedVector3Array = PackedVector3Array([mallet])
	var velocities: PackedVector3Array = PackedVector3Array([Vector3(0.0, 0.0, 60.0)])
	var struck: PuckPhysics.State = PuckPhysics.resolve_mallets(
		_state(mallet + Vector3(0.0, 0.0, contact * 0.5), Vector3(0.0, 0.0, -5.0)), positions, velocities)
	assert_true(struck.velocity.length() <= HockeyConfig.PUCK_MAX_SPEED + 0.0001,
		"no strike can put the puck above the cap the substep count was derived from")

func test_serves_are_deterministic_and_aimed() -> void:
	# A client PREDICTS the face-off, so a random serve would mispredict every restart by the full serve speed
	# -- the largest correction the demo could manufacture for itself.
	for sequence: int in [0, 1, 2, 17, 65534]:
		var first: Vector3 = PuckPhysics.serve_velocity(0, sequence)
		var second: Vector3 = PuckPhysics.serve_velocity(0, sequence)
		assert_vec_almost_eq(first, second, 0.000001, "sequence %d serves the same way twice" % sequence)
	for team: int in [0, 1]:
		var serve: Vector3 = PuckPhysics.serve_velocity(team, 3)
		assert_true(serve.z * HockeyConfig.end_sign(team) > 0.0,
			"a serve travels toward the end team %d defends" % team)
		assert_almost_eq(serve.length(), HockeyConfig.SERVE_SPEED, 0.0001, "at the serve speed")

func test_serves_are_not_all_the_same_line() -> void:
	var straight: int = 0
	for sequence: int in 64:
		if absf(PuckPhysics.serve_velocity(0, sequence).x) < 0.001:
			straight += 1
	assert_true(straight < 32, "the spread varies with the sequence rather than always going down the middle")

func test_rest_is_the_serve_precondition() -> void:
	assert_true(PuckPhysics.is_at_rest(Vector3.ZERO), "a stopped puck is at rest")
	assert_true(PuckPhysics.is_at_rest(Vector3(HockeyConfig.PUCK_REST_SPEED * 0.5, 0.0, 0.0)), "so is a crawl")
	assert_false(PuckPhysics.is_at_rest(Vector3(1.0, 0.0, 0.0)), "a live puck is not")

func test_a_zero_delta_changes_nothing() -> void:
	var state: PuckPhysics.State = _state(Vector3(0.1, 0.0, 0.2), Vector3(1.0, 0.0, 0.0))
	var held: PuckPhysics.State = PuckPhysics.step(state, _no_mallets(), _no_mallets(), 0.0)
	assert_vec_almost_eq(held.position, state.position, 0.0001, "no time, no motion")
	assert_vec_almost_eq(held.velocity, state.velocity, 0.0001, "and no divide by zero")

func test_the_puck_stays_on_the_table_over_a_long_run() -> void:
	# A blunt containment check: whatever the bounces do, the puck must not end up outside the rails except
	# through a mouth.
	var state: PuckPhysics.State = _state(Vector3(0.2, 0.0, 0.1), Vector3(3.1, 0.0, 2.7))
	for _tick: int in 600:
		state = PuckPhysics.step(state, _no_mallets(), _no_mallets(), DT)
		assert_true(absf(state.position.x) <= HockeyConfig.HALF_WIDTH,
			"the puck never leaves the table sideways")
		if TableGeometry.scoring_team_at(state.position.z) >= 0:
			assert_true(TableGeometry.is_in_goal_mouth(state.position.x),
				"the only way past an end line is through the mouth")
			break

# --- the constants the substep count is derived from ------------------------------------------------
func test_the_substep_count_covers_the_speed_cap() -> void:
	# The derivation, as an assertion rather than a comment. If the speed cap rises or the puck shrinks, this
	# is what says the substep count has to follow -- otherwise a fast puck starts passing through mallets and
	# the symptom looks like a replication failure.
	var per_tick: float = HockeyConfig.PUCK_MAX_SPEED / float(HockeyConfig.NET_TICK_HZ)
	var per_substep: float = per_tick / float(HockeyConfig.PUCK_SUBSTEPS)
	var smallest_pair: float = (HockeyConfig.PUCK_RADIUS + HockeyConfig.MALLET_RADIUS) * 2.0
	assert_true(per_substep < smallest_pair,
		"a substep must move the puck less than the smallest contact pair it could pass through")

func test_a_mallet_cannot_outrun_the_substep_either() -> void:
	var per_substep: float = HockeyConfig.MALLET_MAX_SPEED / float(HockeyConfig.NET_TICK_HZ)
	assert_true(per_substep < HockeyConfig.MALLET_RADIUS * 2.0,
		"a mallet crossing more than its own diameter in a tick could pass through a stationary puck")
