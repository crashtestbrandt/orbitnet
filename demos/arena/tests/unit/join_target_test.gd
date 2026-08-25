extends UnitTest
## ArenaNet's join-target parsing: `ADDR`, `ADDR:PORT`, or `[LITERAL]:PORT`.
##
## The same rule the rts and hockey demos pin, and the third copy of it. The arena keeps its helpers
## private because nothing outside the class calls them, which is why this suite reaches for the
## underscore names -- a rule with three copies and two suites is a rule one copy can drift out of.

func test_a_bare_address_uses_the_default_port() -> void:
	assert_eq(ArenaNet._address_of("127.0.0.1"), "127.0.0.1", "the address is passed through whole")
	assert_eq(ArenaNet._port_of("127.0.0.1"), NetTransport.DEFAULT_PORT,
		"and the transport's own default stands in for the port")

func test_a_port_suffix_is_split_off() -> void:
	assert_eq(ArenaNet._address_of("127.0.0.1:47900"), "127.0.0.1", "the address stops at the colon")
	assert_eq(ArenaNet._port_of("127.0.0.1:47900"), 47900, "and the port is the suffix")

func test_a_bare_ipv6_literal_is_never_cut_in_half() -> void:
	# THE REGRESSION THIS PINS: `::1` and `fe80::1` end in digits, so a rule that split on the last
	# colon with a numeric suffix returned the address `:` on port 1 and ENet could not create a peer.
	for literal: String in ["::1", "fe80::1", "2001:db8::8a2e:370:7334"]:
		assert_eq(ArenaNet._address_of(literal), literal, "'%s' survives whole" % literal)
		assert_eq(ArenaNet._port_of(literal), NetTransport.DEFAULT_PORT, "'%s' names no port" % literal)

func test_an_ipv6_literal_names_a_port_in_brackets() -> void:
	assert_eq(ArenaNet._address_of("[::1]:47900"), "::1", "the brackets stop at the string")
	assert_eq(ArenaNet._port_of("[::1]:47900"), 47900, "and the suffix is the port")
	assert_eq(ArenaNet._address_of("[fe80::1]"), "fe80::1", "a bracketed literal with no port unwraps too")
	assert_eq(ArenaNet._port_of("[fe80::1]"), NetTransport.DEFAULT_PORT, "and takes the default")

func test_more_than_one_bare_colon_is_an_address() -> void:
	assert_eq(ArenaNet._address_of("a:b:47900"), "a:b:47900",
		"a multi-colon target cannot be told from a literal, so it is not split")
	assert_eq(ArenaNet._port_of("a:b:47900"), NetTransport.DEFAULT_PORT, "and keeps the default port")

func test_a_steam_target_falls_through_unchanged() -> void:
	var steam_id: String = "76561198000000000"
	assert_eq(ArenaNet._address_of(steam_id), steam_id, "the id survives whole")
	assert_eq(ArenaNet._port_of(steam_id), NetTransport.DEFAULT_PORT, "and the port is ignored")

func test_a_non_numeric_suffix_is_part_of_the_address() -> void:
	assert_eq(ArenaNet._address_of("host:name"), "host:name", "a non-numeric suffix is not a port")
	assert_eq(ArenaNet._port_of("host:name"), NetTransport.DEFAULT_PORT, "so the default stands")

func test_the_port_is_clamped_into_range() -> void:
	assert_eq(ArenaNet._port_of("127.0.0.1:0"), 1, "port 0 is not bindable")
	assert_eq(ArenaNet._port_of("127.0.0.1:99999"), 65535, "and there is no port above 65535")
