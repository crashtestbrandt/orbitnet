extends UnitTest
## Scene-free coverage for the AUTHENTICATED KEY EXCHANGE on the [code]Net[/code] facade: the six calls a
## game makes, the shapes of the values they take and return, and the degraded answer against a cdylib that
## predates them. The companion of `net_session_secret_test.gd`, for the regime that needs no shared secret.
##
## WHAT THE PIN IS FOR. Both nonce halves of a join cross the wire in the clear, so with no secret the key
## they fold to is one an on-path observer also holds. A session secret closes that by folding in bytes the
## observer never sees -- but a secret has to reach every client over a channel with confidentiality, and it
## cannot ship in a build. A server's static X25519 key has a PUBLIC half: it needs integrity only, it may
## ship in the build, and a client that pinned it derives its session key through an exchange only the holder
## of the secret half can complete. The derivation itself is pinned in the Rust suites; what is worth pinning
## here is the facade contract a game can break from GDScript:
##
## - THE DRAW RETURNS THE SECRET HALF, AND INSTALLS NOTHING. [method Net.generate_server_static_key] hands
##   back 32 bytes for the game to store; [method Net.set_server_static_key] is what installs them, and
##   [method Net.server_public_key] is what the game publishes. A game that pins the drawn bytes as if they
##   were the public key refuses every join with a readable error on the client, which is the mistake this
##   suite's first test exists to make impossible to misread.
## - 32 BYTES OR REFUSED. Any other length is refused with an error and leaves the previous key in place.
## - THE EMPTY ARRAY CLEARS, on both ends.
## - THERE IS NO GETTER FOR THE SECRET HALF. `has_server_static_key()` and `server_public_key()` are the
##   only reads.
## - EVERY CALL DEGRADES, NEVER ERRORS, against a binary that predates the six calls.
##
## Ordering matters to the game and not to this suite: every call goes BEFORE [method Net.set_mode], since the
## key is seated when the session starts. Nothing here starts a session; `tools/server-shape-probe.sh` runs
## the pinned join end to end.

const KEY_LEN: int = 32

## Whether the loaded cdylib carries the calls at all. Every assertion below is written against this, because
## a binary that predates them makes every write a no-op and every read empty or `false`.
func _backend_carries_the_calls() -> bool:
	var drawn: PackedByteArray = Net.generate_server_static_key()
	return drawn.size() == KEY_LEN

func _clear_both_ends() -> void:
	Net.set_server_static_key(PackedByteArray())
	Net.set_pinned_server_key(PackedByteArray())

func test_a_session_that_configures_nothing_holds_no_key_on_either_end() -> void:
	_clear_both_ends()
	assert_false(Net.has_server_static_key(), "no static key by default")
	assert_false(Net.has_pinned_server_key(), "no pin by default")
	assert_eq(Net.server_public_key().size(), 0, "and no public half to publish")

func test_the_draw_returns_a_secret_half_and_installs_nothing() -> void:
	# The call a game makes ONCE and stores. It returns bytes rather than installing them, because a key
	# drawn per launch gives every run a different identity and makes a pinned key worthless.
	var carried: bool = _backend_carries_the_calls()
	_clear_both_ends()
	var first: PackedByteArray = Net.generate_server_static_key()
	var second: PackedByteArray = Net.generate_server_static_key()
	if carried:
		assert_eq(first.size(), KEY_LEN, "32 bytes")
		assert_true(first != second, "two draws differ")
	else:
		assert_eq(first.size(), 0, "a backend that predates the call draws nothing")
	assert_false(Net.has_server_static_key(), "drawing installs nothing")

func test_installing_the_secret_half_publishes_a_public_half() -> void:
	var carried: bool = _backend_carries_the_calls()
	_clear_both_ends()
	var secret: PackedByteArray = Net.generate_server_static_key()
	Net.set_server_static_key(secret)
	var public: PackedByteArray = Net.server_public_key()
	if carried:
		assert_true(Net.has_server_static_key(), "the static key took")
		assert_eq(public.size(), KEY_LEN, "the public half is 32 bytes")
		assert_true(public != secret, "and it is not the secret half")
		Net.set_server_static_key(secret)
		assert_eq(Net.server_public_key(), public, "the same secret publishes the same public half")
	else:
		assert_false(Net.has_server_static_key(), "a backend that predates the call installs nothing")
		assert_eq(public.size(), 0, "and publishes nothing")
	_clear_both_ends()

func test_a_key_of_the_wrong_length_is_refused_and_the_previous_one_kept() -> void:
	var carried: bool = _backend_carries_the_calls()
	if not carried:
		return
	_clear_both_ends()
	var secret: PackedByteArray = Net.generate_server_static_key()
	Net.set_server_static_key(secret)
	var public: PackedByteArray = Net.server_public_key()
	for wrong: PackedByteArray in [PackedByteArray([0x01]), secret.slice(0, 31), secret + PackedByteArray([0x00])]:
		Net.set_server_static_key(wrong)
		assert_true(Net.has_server_static_key(), "a %d-byte key is refused, the previous one stays" % wrong.size())
		assert_eq(Net.server_public_key(), public, "...and its public half is unchanged")
		Net.set_pinned_server_key(wrong)
		assert_false(Net.has_pinned_server_key(), "a %d-byte pin is refused" % wrong.size())
	_clear_both_ends()

func test_a_client_pins_a_public_half_and_an_empty_array_clears_it() -> void:
	var carried: bool = _backend_carries_the_calls()
	_clear_both_ends()
	var secret: PackedByteArray = Net.generate_server_static_key()
	Net.set_server_static_key(secret)
	Net.set_pinned_server_key(Net.server_public_key())
	assert_eq(Net.has_pinned_server_key(), carried, "the pin took, or degraded")
	Net.set_pinned_server_key(PackedByteArray())
	assert_false(Net.has_pinned_server_key(), "an empty array clears the pin")
	Net.set_server_static_key(PackedByteArray())
	assert_false(Net.has_server_static_key(), "and the static key")
	assert_eq(Net.server_public_key().size(), 0, "with no public half left to publish")

func test_the_facade_exposes_no_way_to_read_the_secret_half_back() -> void:
	# The secret half outlives the process that drew it, so a getter would put it in every debug print and
	# crash report that walks the facade. The public half is the only read, and it is public by construction.
	assert_false(Net.has_method(&"server_static_key"), "no getter for the secret half")
	assert_false(Net.has_method(&"get_server_static_key"), "nor under the other spelling")
	assert_true(Net.has_method(&"server_public_key"), "the public half is the read")
	for name: StringName in [&"set_server_static_key", &"has_server_static_key",
			&"generate_server_static_key", &"set_pinned_server_key", &"has_pinned_server_key"]:
		assert_true(Net.has_method(name), "%s is on the facade" % name)

func test_every_call_is_safe_offline() -> void:
	# They are set BEFORE `Net.set_mode()`, so OFFLINE is the state a game is in when it calls them.
	assert_eq(Net.current_mode(), Net.Mode.OFFLINE, "this suite starts no session")
	var secret: PackedByteArray = Net.generate_server_static_key()
	Net.set_server_static_key(secret)
	Net.set_pinned_server_key(Net.server_public_key())
	_clear_both_ends()
	assert_false(Net.has_server_static_key(), "cleared, offline, without erroring")
	assert_false(Net.has_pinned_server_key(), "both ends")
