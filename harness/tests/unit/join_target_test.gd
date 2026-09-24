extends UnitTest
## The join-target parser on [NetTransport]: `ADDR`, `ADDR:PORT`, or `[LITERAL]:PORT`.
##
## One suite, because there is now one parser. It was four copies of the rule -- one in each demo's session
## layer and a fourth, restated differently, in the bench relay -- pinned by three near-identical suites that
## between them missed the case the fourth copy had already got wrong.
##
## The last block covers the relay's own rule on top of the shared one: a malformed target moves neither host
## nor port. The shared accessors always answer, so that guard is the relay's to keep.

const _RELAY_SCRIPT: String = "res://addons/orbitnet/bench/relay_main.gd"

func test_a_bare_address_uses_the_default_port() -> void:
	assert_eq(NetTransport.target_address("127.0.0.1"), "127.0.0.1", "the address is passed through whole")
	assert_eq(NetTransport.target_port("127.0.0.1"), NetTransport.DEFAULT_PORT,
		"and the transport's own default stands in for the port")

func test_a_port_suffix_is_split_off() -> void:
	# THE BUG THIS PINS: create_client takes the port as its own argument, so a target passed through whole
	# reaches ENet as a hostname to resolve, and a session hosted on anything but the default port is
	# unreachable while the join flag's documentation promises the suffix works.
	assert_eq(NetTransport.target_address("127.0.0.1:47900"), "127.0.0.1", "the address stops at the colon")
	assert_eq(NetTransport.target_port("127.0.0.1:47900"), 47900, "and the port is the suffix")

func test_a_hostname_works_too() -> void:
	assert_eq(NetTransport.target_address("host.example:1234"), "host.example", "hostnames split the same way")
	assert_eq(NetTransport.target_port("host.example:1234"), 1234, "with their own port")

func test_a_steam_target_falls_through_unchanged() -> void:
	# A platform target is a 64-bit id and carries no colon, so a caller still never learns which transport
	# it is talking to.
	var platform_id: String = "76561198000000000"
	assert_eq(NetTransport.target_address(platform_id), platform_id, "the id survives whole")
	assert_eq(NetTransport.target_port(platform_id), NetTransport.DEFAULT_PORT,
		"and the port is ignored on that path")

func test_a_trailing_or_leading_colon_is_not_a_port() -> void:
	for junk: String in ["127.0.0.1:", ":47900", ":"]:
		assert_eq(NetTransport.target_port(junk), NetTransport.DEFAULT_PORT, "'%s' names no port" % junk)
		assert_eq(NetTransport.target_address(junk), junk, "'%s' is left alone rather than truncated" % junk)

func test_a_non_numeric_suffix_is_part_of_the_address() -> void:
	# An odd hostname is left whole. An IPv6 literal is covered by the colon-count rule below, not by this
	# one -- `::1` ends in a digit and would pass a numeric-suffix test.
	assert_eq(NetTransport.target_address("host:name"), "host:name", "a non-numeric suffix is not a port")
	assert_eq(NetTransport.target_port("host:name"), NetTransport.DEFAULT_PORT, "so the default stands")

func test_the_port_is_clamped_into_range() -> void:
	assert_eq(NetTransport.target_port("127.0.0.1:0"), 1, "port 0 is not bindable")
	assert_eq(NetTransport.target_port("127.0.0.1:99999"), 65535, "and there is no port above 65535")

func test_a_bare_ipv6_literal_is_never_cut_in_half() -> void:
	# THE REGRESSION THIS PINS: `::1` and `fe80::1` end in digits, so a rule that split on the last colon
	# with a numeric suffix returned the address `:` on port 1 and ENet could not create a peer.
	for literal: String in ["::1", "fe80::1", "2001:db8::8a2e:370:7334"]:
		assert_eq(NetTransport.target_address(literal), literal, "'%s' survives whole" % literal)
		assert_eq(NetTransport.target_port(literal), NetTransport.DEFAULT_PORT, "'%s' names no port" % literal)

func test_an_ipv6_literal_names_a_port_in_brackets() -> void:
	# Brackets are the only unambiguous way to write a port beside a literal, which is why more than one
	# bare colon never splits.
	assert_eq(NetTransport.target_address("[::1]:47900"), "::1", "the brackets stop at the string")
	assert_eq(NetTransport.target_port("[::1]:47900"), 47900, "and the suffix is the port")
	assert_eq(NetTransport.target_address("[fe80::1]"), "fe80::1", "a bracketed literal with no port unwraps too")
	assert_eq(NetTransport.target_port("[fe80::1]"), NetTransport.DEFAULT_PORT, "and takes the default")

func test_more_than_one_bare_colon_is_an_address() -> void:
	assert_eq(NetTransport.target_address("a:b:47900"), "a:b:47900",
		"a multi-colon target cannot be told from a literal, so it is not split")
	assert_eq(NetTransport.target_port("a:b:47900"), NetTransport.DEFAULT_PORT, "and keeps the default port")

func test_the_relay_commits_a_well_formed_target() -> void:
	# The control for the case below: the relay does move on a target it can read, by both halves at once.
	assert_eq(_relay_target("10.0.0.5:47901"), "10.0.0.5|47901", "a host and port pair lands whole")
	assert_eq(_relay_target("[::1]:47901"), "::1|47901", "and so does a bracketed literal with a port")
	assert_eq(_relay_target("10.0.0.5"), "10.0.0.5|%d" % NetTransport.DEFAULT_PORT,
		"a target naming no port takes the transport's default")

func test_a_malformed_relay_target_moves_neither_half() -> void:
	# THE DIVERGENCE THIS PINS: the shared accessors always answer -- a default port when the target names
	# none, the target whole when they cannot read it -- so committing them unconditionally let `x]:47900`
	# write the port while the host kept its previous value, and the relay forwarded to a pair nobody named.
	for junk: String in ["x]:47900", "x]", "[::1", "[]:47900", "[::1]:47900:x", ""]:
		assert_eq(_relay_target(junk, "10.0.0.5:47901"), "10.0.0.5|47901",
			"'%s' leaves the previous target alone" % junk)

# The relay's target after parsing `spec`, as "host|port", starting from `seed_spec`. The relay is a MainLoop
# whose target is private state, so this reaches it by name rather than through a typed reference -- it is not
# a seam any other caller needs.
func _relay_target(spec: String, seed_spec: String = "") -> String:
	var script: GDScript = load(_RELAY_SCRIPT)
	var relay: Object = script.new()
	if seed_spec != "":
		relay.call("_parse_target", seed_spec)
	relay.call("_parse_target", spec)
	var host: Variant = relay.get("_target_host")
	var port: Variant = relay.get("_target_port")
	var host_text: String = host
	var port_number: int = port
	relay.free()
	return "%s|%d" % [host_text, port_number]
