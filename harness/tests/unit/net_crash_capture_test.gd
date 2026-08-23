extends UnitTest
## The crash-capture surface of the `Net` facade: the native handler's install contract, and the Windows
## Error Reporting read-back that covers the one crash an in-process handler cannot see.
##
## The addon NEVER writes the WER registry keys -- they are HKLM-only, need administrator privileges, and set
## policy for every application on the machine -- so the only thing under test here is the read-back's shape
## and the one invariant a crash report depends on: a folder is named only when something writes into it.
## Everything platform-specific about the read itself is a Rust unit test in crash.rs.

func test_the_dump_config_is_always_a_full_row() -> void:
	# A diagnostics read has no business erroring or returning a partial dictionary, on any platform or on a
	# backend binary too old to answer -- a caller that has to null-check every key writes no report at all.
	var cfg: Dictionary[String, Variant] = Net.native_crash_dump_config()
	for key: String in ["supported", "configured", "scope", "folder", "dump_type", "dump_count", "image"]:
		assert_true(cfg.has(key), "the read-back always carries `%s`" % key)

func test_a_folder_is_named_only_when_something_collects() -> void:
	# The invariant a crash report reads. Naming a folder nothing writes to would send a player hunting for a
	# file that was never created; reporting no folder while WER collects would hide one that exists.
	var cfg: Dictionary[String, Variant] = Net.native_crash_dump_config()
	var configured: bool = cfg["configured"]
	var folder: String = cfg["folder"]
	assert_eq(configured, not folder.is_empty(), "`configured` and a named folder travel together")

func test_wer_is_a_windows_question() -> void:
	if OS.get_name() == "Windows":
		return
	var cfg: Dictionary[String, Variant] = Net.native_crash_dump_config()
	var supported: bool = cfg["supported"]
	var configured: bool = cfg["configured"]
	var scope: String = cfg["scope"]
	# Off Windows there is no gap to report: the POSIX handler already catches SIGABRT, which is the case a
	# fail-fast stands in for.
	assert_false(supported, "no WER off Windows")
	assert_false(configured, "so nothing is collected by it")
	assert_eq(scope, "none", "and no registry key decided anything")

func test_installing_the_handler_refuses_a_path_it_cannot_use() -> void:
	# The facade's own guard, before the backend is ever asked. An empty directory would send the handler's
	# pre-resolved log path to `/crash-native.log`, which is not the caller's log directory.
	assert_false(Net.install_native_crash_handler(""), "an empty directory is refused")
