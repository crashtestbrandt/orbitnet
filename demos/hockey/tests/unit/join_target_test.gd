extends UnitTest
## HockeyNet's join-target parsing: `ADDR` or `ADDR:PORT`.
##
## The bug this pins: `NetTransport.create_client` takes the port as its own argument, so a target passed
## through whole would reach ENet as a hostname to resolve. `just hockey-host 47900` would then be unreachable
## while the `--join=ADDR[:PORT]` flag documented the suffix as working.

func test_a_bare_address_uses_the_default_port() -> void:
	assert_eq(HockeyNet.target_address("127.0.0.1"), "127.0.0.1", "the address is passed through whole")
	assert_eq(HockeyNet.target_port("127.0.0.1"), NetTransport.DEFAULT_PORT,
		"and the transport's own default stands in for the port")

func test_a_port_suffix_is_split_off() -> void:
	assert_eq(HockeyNet.target_address("127.0.0.1:47900"), "127.0.0.1", "the address stops at the colon")
	assert_eq(HockeyNet.target_port("127.0.0.1:47900"), 47900, "and the port is the suffix")

func test_a_hostname_works_too() -> void:
	assert_eq(HockeyNet.target_address("rink.example:1234"), "rink.example", "hostnames split the same way")
	assert_eq(HockeyNet.target_port("rink.example:1234"), 1234, "with their own port")

func test_a_steam_target_falls_through_unchanged() -> void:
	# A Steam build's target is a 64-bit Steam ID and carries no colon, so the demo still never learns which
	# transport it is talking to.
	var steam_id: String = "76561198000000000"
	assert_eq(HockeyNet.target_address(steam_id), steam_id, "the id survives whole")
	assert_eq(HockeyNet.target_port(steam_id), NetTransport.DEFAULT_PORT, "and the port is ignored on that path")

func test_a_trailing_or_leading_colon_is_not_a_port() -> void:
	for junk: String in ["127.0.0.1:", ":47900", ":"]:
		assert_eq(HockeyNet.target_port(junk), NetTransport.DEFAULT_PORT,
			"'%s' names no port" % junk)
		assert_eq(HockeyNet.target_address(junk), junk, "'%s' is left alone rather than truncated" % junk)

func test_a_non_numeric_suffix_is_part_of_the_address() -> void:
	# Refusing to split here is what keeps an IPv6 literal or an odd hostname from being cut in half.
	assert_eq(HockeyNet.target_address("host:name"), "host:name", "a non-numeric suffix is not a port")
	assert_eq(HockeyNet.target_port("host:name"), NetTransport.DEFAULT_PORT, "so the default stands")

func test_the_port_is_clamped_into_range() -> void:
	assert_eq(HockeyNet.target_port("127.0.0.1:0"), 1, "port 0 is not bindable")
	assert_eq(HockeyNet.target_port("127.0.0.1:99999"), 65535, "and there is no port above 65535")

func test_only_the_last_colon_splits() -> void:
	assert_eq(HockeyNet.target_address("a:b:47900"), "a:b", "the port is the last field, not the first")
	assert_eq(HockeyNet.target_port("a:b:47900"), 47900, "and it is read from the end")
