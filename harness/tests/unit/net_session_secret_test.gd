extends UnitTest
## Scene-free coverage for the SHARED SESSION SECRET on the [code]Net[/code] facade: the two calls a game
## makes, the empty-array clear, and the degraded answer against a cdylib that predates them.
##
## WHAT THE SECRET IS FOR. Without one, the per-datagram key is minted by the client and carried in the join
## handshake in the clear, so what the MAC authenticates is a datagram's membership in a session rather than a
## peer's identity -- an on-path observer who reads the handshake can forge anything the client can. With one,
## the handshake carries only a NONCE, both ends derive the key from `(secret, nonce)`, and that observer
## learns the nonce and nothing else.
##
## WHAT IS WORTH PINNING HERE is not the derivation -- that lives in the Rust suites, where it is exercised
## against pinned byte vectors -- but the four facade contracts a game can break from GDScript:
##
## - A SESSION SETS NOTHING BY DEFAULT. The cleartext-key regime is what every existing project is on, and a
##   facade that came up holding some secret would change the wire for a game that never asked.
## - THE EMPTY ARRAY CLEARS. It is the only way back to that default, and a game that clears between lobbies
##   has to be able to trust it.
## - THERE IS NO GETTER FOR THE BYTES. [method Net.has_session_secret] answers whether one is set and nothing
##   answers what it is, so the material cannot reach a debug print or a crash report that walks the node.
## - EVERY CALL DEGRADES, NEVER ERRORS. This suite runs against the COMMITTED cdylib, which is refreshed only
##   at a release tag, so both calls routinely run against a binary that has neither of them. The write is a
##   no-op and the read answers `false` -- which is the honest answer, because that binary derives nothing.
##
## Ordering matters to the game and not to this suite: both calls must be made BEFORE [method Net.set_mode],
## since the key is seated when the session starts. Nothing here starts a session.

## A secret shaped like the ones a game actually holds: a lobby token as UTF-8 bytes.
func _token_secret() -> PackedByteArray:
	return "lobby-76561190000000001-a3f9c2".to_utf8_buffer()

## A secret shaped like the other source: raw bytes off an auth ticket.
func _raw_secret() -> PackedByteArray:
	var out: PackedByteArray = PackedByteArray()
	for index: int in range(32):
		out.push_back((index * 37 + 11) & 0xff)
	return out

## The facade's answer, through a TYPED LOCAL. A backend older than these sources answers through an untyped
## call, and assigning is the conversion this repository allows where an `as`-cast is not.
func _held() -> bool:
	var held: bool = Net.has_session_secret()
	return held

## Whether the loaded cdylib carries the two calls at all. Every assertion below is written against this,
## because a binary that predates them makes the write a no-op and the read a constant `false`.
func _backend_carries_the_calls() -> bool:
	Net.set_session_secret(_token_secret())
	var carried: bool = _held()
	Net.set_session_secret(PackedByteArray())
	return carried

func test_a_session_that_configures_nothing_holds_no_secret() -> void:
	# THE DEFAULT-DRIFT GUARD. No secret is the regime every existing project is on, and it is the one where
	# the handshake's 16 bytes ARE the session key. A facade that came up holding one would change the wire
	# for a game that never asked, and change it in the direction that refuses every peer without it.
	Net.set_session_secret(PackedByteArray())
	assert_false(_held(), "nothing has set one")

func test_setting_a_secret_either_takes_or_degrades_to_holding_none() -> void:
	# The forward, and the only shape it can have against a binary older than these sources: the write is a
	# no-op and the read answers false. Both are non-errors, and the game runs either way -- on the cleartext
	# key in the degraded case, which is exactly what that binary would have done anyway.
	var carried: bool = _backend_carries_the_calls()
	Net.set_session_secret(_token_secret())
	if carried:
		assert_true(_held(), "the secret took")
	else:
		assert_false(_held(), "a backend that predates the call derives nothing")
	Net.set_session_secret(PackedByteArray())

func test_any_length_of_secret_is_accepted() -> void:
	# The secret is folded to 16 bytes inside the backend, so a game hands over whatever its authenticated
	# channel gave it -- a 30-character lobby token, 32 raw bytes off a ticket, a single byte. None of these
	# is a length error, and a facade that rejected one would push games into inventing their own fold.
	var carried: bool = _backend_carries_the_calls()
	var shapes: Array[PackedByteArray] = [
		_token_secret(),
		_raw_secret(),
		PackedByteArray([0x01]),
		PackedByteArray([0x00, 0x00, 0x00, 0x00]),
	]
	for secret: PackedByteArray in shapes:
		Net.set_session_secret(secret)
		assert_eq(_held(), carried, "a %d-byte secret is a secret" % secret.size())
	Net.set_session_secret(PackedByteArray())

func test_an_empty_array_clears_the_secret() -> void:
	# THE ONLY WAY BACK TO THE CLEARTEXT DEFAULT. A game that joins a secret-carrying lobby and then a plain
	# one has to be able to put the session back; without this it would have to restart the process.
	var carried: bool = _backend_carries_the_calls()
	Net.set_session_secret(_token_secret())
	assert_eq(_held(), carried, "set")
	Net.set_session_secret(PackedByteArray())
	assert_false(_held(), "an empty array clears it")

func test_setting_a_secret_twice_replaces_rather_than_accumulates() -> void:
	# A game re-enters a lobby browser and sets a second lobby's secret over the first. The second must be the
	# one in force: a session derived from a stale secret refuses every peer in the lobby it actually joined,
	# and the symptom -- a join that never completes -- says nothing about which secret was wrong.
	var carried: bool = _backend_carries_the_calls()
	Net.set_session_secret(_token_secret())
	Net.set_session_secret(_raw_secret())
	assert_eq(_held(), carried, "the second secret is the one held")
	Net.set_session_secret(PackedByteArray())
	assert_false(_held(), "and one clear is enough to drop it")

func test_the_facade_exposes_no_way_to_read_the_secret_back() -> void:
	# THERE IS NO GETTER, AND THAT IS THE CONTRACT. The two questions a game has are "did my configuration
	# take" and "am I about to join in the clear", and `has_session_secret` is both. Handing the material back
	# out would put it in every debug print, save file and crash report that walks the facade.
	assert_false(Net.has_method(&"session_secret"), "no bytes getter")
	assert_false(Net.has_method(&"get_session_secret"), "nor under the other spelling")
	assert_true(Net.has_method(&"has_session_secret"), "only the boolean")
	assert_true(Net.has_method(&"set_session_secret"), "and the setter")

func test_both_calls_are_safe_offline() -> void:
	# They are set BEFORE `Net.set_mode()`, so OFFLINE is the state a game is actually in when it calls them.
	# Neither may be gated on a live session, and neither may error there.
	assert_eq(Net.current_mode(), Net.Mode.OFFLINE, "this suite starts no session")
	Net.set_session_secret(_token_secret())
	Net.set_session_secret(PackedByteArray())
	assert_false(_held(), "cleared, offline, without erroring")
