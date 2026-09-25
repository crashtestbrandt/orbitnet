#!/usr/bin/env bash
# The plugin version and the cargo workspace version are ONE version. This script is the only thing that
# writes either of them, and the only thing that asserts they agree.
#
# Where the version is written:
#
#   addons/orbitnet/plugin.cfg    `version="X"`                  what Godot shows in the plugin list
#   native/Cargo.toml             `[workspace.package] version`  what `CARGO_PKG_VERSION` and a crash
#                                                                report carry, and what a cargo consumer
#                                                                of `orbitnet-core` resolves
#   native/Cargo.lock             one entry per workspace member  the lock records each member's version
#
# WHY THIS EXISTS. `release.yml` stamped `plugin.cfg` from the tag and nothing else, so `plugin.cfg`
# reached `0.4.0` while the workspace version sat at `0.1.0` through every release the project cut.
# Anything reading `CARGO_PKG_VERSION` -- a crash report, a cargo consumer of `orbitnet-core` -- reported
# `0.1.0` the whole time, and no gate read both files.
#
# WHY THE LOCK IS STAMPED TOO. `native/Cargo.lock` records the version of every workspace member. Stamping
# the manifest alone leaves the lock disagreeing with it, so the next cargo command in the tree rewrites
# the lock as a side effect (a dirty tree on the release runner) and `cargo build --locked` -- what a
# vendoring or auditing consumer runs -- fails outright.
#
# WHY THE MEMBER MANIFESTS ARE NOT STAMPED. Each member carries `version.workspace = true` and inherits
# from `[workspace.package]`. `check` asserts that inheritance rather than assuming it: a member that
# spells its own version out is a crate the workspace stamp never reaches, and it would ship the old
# version with every other gate green.
#
# WHERE THE CHECK BELONGS -- BOTH PLACES. The two catch different failures and neither substitutes for the
# other:
#
#   `just check`, at PR time     Catches the divergence in the pull request that introduces it, while
#                                there is still a person to fix it. A bumped `plugin.cfg` with an
#                                untouched `native/Cargo.toml` fails here.
#   `release.yml`, at tag time   Runs `check` immediately AFTER `stamp`, which is the only place a stamp
#                                can be proved. An `awk` or `sed` whose pattern stops matching exits 0 and
#                                ships the old version; re-reading every site afterwards is what turns the
#                                stamp from assumed into verified.
#
# A tag-time check alone ships the divergence on `main` between releases. A PR-time check alone never sees
# the stamp, because the stamp only happens on a tag.
#
# THE PR-TIME LEG IS LOCAL ONLY, FOR NOW. `.github/workflows/check.yml` does not run this script, so the
# catch depends on the contributor running `just check`; a pull request that diverges the three files is
# green. Wiring it in is one step in that workflow's `gates:` job, alongside the descriptor-parity gate.
#
# Usage:
#   tools/version-parity.sh check         assert every site agrees; changes nothing
#   tools/version-parity.sh stamp <ver>   write <ver> to every site, then assert
#
# Portable `awk` and no `sed -i`: BSD `sed` on macOS requires an argument to `-i` and GNU `sed` refuses
# one, and this runs on a contributor's Mac as well as on the Linux release runner. Every rewrite goes to
# a temporary file and is moved into place only if the pattern matched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PLUGIN_CFG="addons/orbitnet/plugin.cfg"
MANIFEST="native/Cargo.toml"
LOCK="native/Cargo.lock"

# ---------------------------------------------------------------------------------------------------
# readers
# ---------------------------------------------------------------------------------------------------

read_plugin_version() {
	awk '/^version="/ { match($0, /"[^"]*"/); print substr($0, RSTART + 1, RLENGTH - 2); exit }' "$PLUGIN_CFG"
}

read_workspace_version() {
	awk '
		/^\[/ { in_pkg = ($0 == "[workspace.package]") }
		in_pkg && /^version[[:space:]]*=[[:space:]]*"/ {
			match($0, /"[^"]*"/); print substr($0, RSTART + 1, RLENGTH - 2); exit
		}
	' "$MANIFEST"
}

# The workspace `members` array, as paths relative to native/. Derived rather than hard-coded so that
# adding a crate does not silently leave it unstamped and unchecked.
read_member_paths() {
	awk '
		/^members[[:space:]]*=/ { collecting = 1 }
		collecting {
			rest = $0
			while (match(rest, /"[^"]*"/)) {
				print substr(rest, RSTART + 1, RLENGTH - 2)
				rest = substr(rest, RSTART + RLENGTH)
			}
			if (index($0, "]")) exit
		}
	' "$MANIFEST"
}

# A crate's PACKAGE name, read from its own manifest. The lock is keyed by package name, which a member
# directory is free not to match.
read_package_name() {
	awk '
		/^\[/ { in_pkg = ($0 == "[package]") }
		in_pkg && /^name[[:space:]]*=[[:space:]]*"/ {
			match($0, /"[^"]*"/); print substr($0, RSTART + 1, RLENGTH - 2); exit
		}
	' "native/$1/Cargo.toml"
}

read_lock_version() {
	awk -v want="$1" '
		/^name[[:space:]]*=[[:space:]]*"/ {
			match($0, /"[^"]*"/)
			found = (substr($0, RSTART + 1, RLENGTH - 2) == want)
			next
		}
		found && /^version[[:space:]]*=[[:space:]]*"/ {
			match($0, /"[^"]*"/); print substr($0, RSTART + 1, RLENGTH - 2); exit
		}
	' "$LOCK"
}

# ---------------------------------------------------------------------------------------------------
# writers
# ---------------------------------------------------------------------------------------------------

# rewrite <file> <awk arg>... -- the awk program must exit non-zero when it changed nothing, so that a
# pattern which has stopped matching fails loudly instead of leaving the file as it was.
rewrite() {
	local file="$1"
	shift
	local tmp="${file}.version-parity.$$"
	if awk "$@" "$file" >"$tmp"; then
		mv "$tmp" "$file"
	else
		rm -f "$tmp"
		printf '::error::%s: no version line matched. Its shape changed and tools/version-parity.sh needs updating.\n' "$file" >&2
		exit 1
	fi
}

stamp_plugin_cfg() {
	rewrite "$PLUGIN_CFG" -v v="$1" '
		/^version="/ && !done { print "version=\"" v "\""; done = 1; next }
		{ print }
		END { if (!done) exit 1 }
	'
}

stamp_manifest() {
	rewrite "$MANIFEST" -v v="$1" '
		/^\[/ { in_pkg = ($0 == "[workspace.package]") }
		in_pkg && /^version[[:space:]]*=[[:space:]]*"/ && !done {
			print "version = \"" v "\""; done = 1; next
		}
		{ print }
		END { if (!done) exit 1 }
	'
}

# Every member entry, in one pass. The count is asserted: a member the lock does not carry means the lock
# is stale, and stamping the rest would hide that.
stamp_lock() {
	local version="$1" members="$2"
	rewrite "$LOCK" -v v="$version" -v names="$members" '
		BEGIN { want_count = split(names, list, " "); for (i = 1; i <= want_count; i++) want[list[i]] = 1 }
		/^name[[:space:]]*=[[:space:]]*"/ {
			match($0, /"[^"]*"/)
			pending = (substr($0, RSTART + 1, RLENGTH - 2) in want)
			print; next
		}
		pending && /^version[[:space:]]*=[[:space:]]*"/ {
			print "version = \"" v "\""; hits++; pending = 0; next
		}
		{ print }
		END { if (hits != want_count) exit 1 }
	'
}

# ---------------------------------------------------------------------------------------------------
# modes
# ---------------------------------------------------------------------------------------------------

reject_bad_version() {
	case "$1" in
	'' | *[!A-Za-z0-9._-]*)
		printf '::error::refusing version %s -- expected only letters, digits, dot, underscore and hyphen.\n' "'$1'" >&2
		exit 1
		;;
	esac
}

do_check() {
	local plugin workspace status=0 crate lock_version
	plugin="$(read_plugin_version)"
	workspace="$(read_workspace_version)"

	if [ -z "$plugin" ]; then
		printf '::error::%s carries no version= line.\n' "$PLUGIN_CFG" >&2
		return 1
	fi
	if [ -z "$workspace" ]; then
		printf '::error::%s carries no [workspace.package] version.\n' "$MANIFEST" >&2
		return 1
	fi

	if [ "$plugin" != "$workspace" ]; then
		printf '::error::the plugin version and the cargo workspace version disagree:\n' >&2
		printf '  %-28s %s\n' "$PLUGIN_CFG" "$plugin" >&2
		printf '  %-28s %s\n' "$MANIFEST" "$workspace" >&2
		# NO PREFILLED VERSION IN THE HINT. Which of the two moved is not knowable from the two files, so a
		# filled-in argument would tell half the callers to revert the bump they just made.
		printf '\nRun `tools/version-parity.sh stamp <the version you intend>` to settle them.\n' >&2
		status=1
	fi

	local member_path member_manifest
	for member_path in $(read_member_paths); do
		member_manifest="native/$member_path/Cargo.toml"
		if [ ! -f "$member_manifest" ]; then
			printf '::error::%s names the member %s, which has no manifest.\n' "$MANIFEST" "$member_path" >&2
			status=1
			continue
		fi
		if ! grep -q '^version\.workspace[[:space:]]*=[[:space:]]*true' "$member_manifest"; then
			printf '::error::%s does not inherit the workspace version. A crate that spells its own version out is one the release stamp never reaches.\n' \
				"$member_manifest" >&2
			status=1
		fi
		crate="$(read_package_name "$member_path")"
		lock_version="$(read_lock_version "$crate")"
		if [ -z "$lock_version" ]; then
			printf '::error::%s has no entry for the workspace member %s. Run `cargo metadata` in native/ to refresh it.\n' \
				"$LOCK" "$crate" >&2
			status=1
		elif [ "$lock_version" != "$workspace" ]; then
			printf '::error::%s records %s for %s; %s says %s. `cargo build --locked` fails on this.\n' \
				"$LOCK" "$lock_version" "$crate" "$MANIFEST" "$workspace" >&2
			status=1
		fi
	done

	if [ "$status" -eq 0 ]; then
		printf 'version parity: %s in plugin.cfg, the cargo workspace and the lock\n' "$workspace"
	fi
	return "$status"
}

do_stamp() {
	local version="$1" members="" member_path crate
	reject_bad_version "$version"
	for member_path in $(read_member_paths); do
		members="$members $(read_package_name "$member_path")"
	done

	# PRE-FLIGHT: every site is read before any of them is written. A stamp that wrote plugin.cfg and then
	# failed on the manifest would leave the tree in the exact divergence this script exists to prevent.
	[ -n "${members// /}" ] || {
		printf '::error::%s lists no workspace members. Its shape changed and this script needs updating.\n' "$MANIFEST" >&2
		exit 1
	}
	[ -n "$(read_plugin_version)" ] || {
		printf '::error::%s carries no version= line to stamp.\n' "$PLUGIN_CFG" >&2
		exit 1
	}
	[ -n "$(read_workspace_version)" ] || {
		printf '::error::%s carries no [workspace.package] version to stamp.\n' "$MANIFEST" >&2
		exit 1
	}
	for crate in $members; do
		[ -n "$(read_lock_version "$crate")" ] || {
			printf '::error::%s has no entry for %s to stamp. Run `cargo metadata` in native/ to refresh it.\n' "$LOCK" "$crate" >&2
			exit 1
		}
	done

	stamp_plugin_cfg "$version"
	stamp_manifest "$version"
	stamp_lock "$version" "$members"
	do_check
}

case "${1:-check}" in
check)
	do_check
	;;
stamp)
	[ "$#" -eq 2 ] || {
		printf '::error::usage: tools/version-parity.sh stamp <version>\n' >&2
		exit 1
	}
	do_stamp "$2"
	;;
*)
	printf '::error::unknown mode %s -- expected `check` or `stamp <version>`.\n' "'${1:-}'" >&2
	exit 1
	;;
esac
