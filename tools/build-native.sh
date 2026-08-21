#!/usr/bin/env bash
# Build the extension for one platform and stage it under the names the .gdextension loads.
#
# THIS SCRIPT IS THE ONLY PLACE THAT KNOWS A SHIPPED FILENAME. binaries.yml, release.yml and
# tools/check-descriptor-parity.sh all ask it rather than spelling names out, so a rename is one edit
# and a descriptor entry naming a file nothing builds is caught by a gate instead of by `dlopen` on a
# platform CI never runs.
#
# THREE PROFILES, and the two the descriptor names are genuinely different builds:
#
#   template_debug    cargo `template-debug`. Godot loads this entry whenever a project runs FROM
#                     SOURCE -- every CI probe, every editor run. It inherits `release` (same
#                     opt-level, LTO and strip), and adds `debug-assertions` and `overflow-checks`.
#                     Without it the workspace's `debug_assert!`s are compiled out of every build.
#   template_release  cargo `release`. What an exported game ships.
#   profiling         cargo `profiling`. Release semantics plus retained debug information, so a
#                     native profiler can attribute frames to Rust functions and source lines. Not a
#                     descriptor entry: a developer swaps it in. Published as a release asset only.
#
# macOS builds BOTH architectures per profile and lipos them together. A single-arch dylib works on
# the machine that built it and fails on the other half of the Mac install base.
#
# Usage:
#   tools/build-native.sh host                            print this machine's platform
#   tools/build-native.sh names <platform> [profile...]   print shipped filenames; build nothing
#   tools/build-native.sh build <platform> <outdir> [profile...]
#
# <platform> is linux | windows | macos. Profiles default to the two the descriptor names.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NATIVE="$ROOT/native"

DEFAULT_PROFILES=(template_debug template_release)

usage() {
	printf 'usage: %s host\n' "$0" >&2
	printf '       %s names <platform> [profile...]\n' "$0" >&2
	printf '       %s build <platform> <outdir> [profile...]\n' "$0" >&2
	printf 'platform: linux | windows | macos    profile: template_debug | template_release | profiling\n' >&2
	exit 2
}

# Cargo's own output filename for a platform, and the two halves of the shipped name around the profile.
platform_parts() {
	case "$1" in
		linux)   BUILT_NAME="liborbitnet.so";  SHIP_PREFIX="liborbitnet.linux";  SHIP_SUFFIX="x86_64.so" ;;
		windows) BUILT_NAME="orbitnet.dll";    SHIP_PREFIX="orbitnet.windows";   SHIP_SUFFIX="x86_64.dll" ;;
		macos)   BUILT_NAME="liborbitnet.dylib"; SHIP_PREFIX="liborbitnet.macos"; SHIP_SUFFIX="universal.dylib" ;;
		*) printf 'build-native: unknown platform %s\n' "$1" >&2; exit 2 ;;
	esac
}

# Cargo's directory under target/ for a profile, and the flag that selects it. `release` is the one
# profile whose directory name and flag do not match its own name.
profile_parts() {
	case "$1" in
		template_debug)   CARGO_DIR="template-debug"; CARGO_FLAG=(--profile template-debug) ;;
		template_release) CARGO_DIR="release";        CARGO_FLAG=(--release) ;;
		profiling)        CARGO_DIR="profiling";      CARGO_FLAG=(--profile profiling) ;;
		*) printf 'build-native: unknown profile %s\n' "$1" >&2; exit 2 ;;
	esac
}

shipped_name() {
	platform_parts "$1"
	printf '%s.%s.%s\n' "$SHIP_PREFIX" "$2" "$SHIP_SUFFIX"
}

MODE="${1:-}"
[ -n "$MODE" ] || usage
shift || usage

case "$MODE" in
host)
	case "$(uname -s)" in
		Linux) printf 'linux\n' ;;
		Darwin) printf 'macos\n' ;;
		MINGW*|MSYS*|CYGWIN*|Windows_NT) printf 'windows\n' ;;
		*) printf 'build-native: unsupported host %s\n' "$(uname -s)" >&2; exit 1 ;;
	esac
	;;
names)
	PLATFORM="${1:-}"; [ -n "$PLATFORM" ] || usage; shift
	profiles=("$@"); [ "${#profiles[@]}" -gt 0 ] || profiles=("${DEFAULT_PROFILES[@]}")
	for p in "${profiles[@]}"; do
		profile_parts "$p" >/dev/null
		shipped_name "$PLATFORM" "$p"
	done
	;;
build)
	PLATFORM="${1:-}"; [ -n "$PLATFORM" ] || usage; shift
	OUTDIR="${1:-}"; [ -n "$OUTDIR" ] || usage; shift
	profiles=("$@"); [ "${#profiles[@]}" -gt 0 ] || profiles=("${DEFAULT_PROFILES[@]}")

	platform_parts "$PLATFORM"
	mkdir -p "$OUTDIR"

	if [ "$PLATFORM" = macos ]; then
		# `cd native` FIRST. native/rust-toolchain.toml pins the toolchain cargo uses in that
		# directory, while the repository root has no override and resolves to rustup's default.
		# Running `rustup target add` from the root installs the std libraries onto the DEFAULT
		# toolchain, which the pinned one cannot see, and the cross build then fails with
		# "can't find crate for `std`".
		( cd "$NATIVE" && rustup target add x86_64-apple-darwin aarch64-apple-darwin )
	fi

	for p in "${profiles[@]}"; do
		profile_parts "$p"
		ship="$(shipped_name "$PLATFORM" "$p")"
		printf '\nbuild-native: %s / %s -> %s\n' "$PLATFORM" "$p" "$ship"

		if [ "$PLATFORM" = macos ]; then
			( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" -p orbitnet-godot --target x86_64-apple-darwin )
			( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" -p orbitnet-godot --target aarch64-apple-darwin )
			lipo -create -output "$OUTDIR/$ship" \
				"$NATIVE/target/x86_64-apple-darwin/$CARGO_DIR/$BUILT_NAME" \
				"$NATIVE/target/aarch64-apple-darwin/$CARGO_DIR/$BUILT_NAME"
			lipo -info "$OUTDIR/$ship"
		else
			( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" -p orbitnet-godot )
			built="$NATIVE/target/$CARGO_DIR/$BUILT_NAME"
			[ -s "$built" ] || { printf 'build-native: cargo produced nothing at %s\n' "$built" >&2; exit 1; }
			install -m 0755 "$built" "$OUTDIR/$ship"
		fi

		# The Windows profiling build's symbols live in a separate PDB rather than inside the image.
		# It keeps the name the DLL records, because that name is what dbghelp searches a symbol path
		# for; renaming it to match the platform-tagged library would hide it from every analyzer.
		if [ "$PLATFORM" = windows ] && [ "$p" = profiling ]; then
			pdb="$NATIVE/target/$CARGO_DIR/orbitnet.pdb"
			[ -s "$pdb" ] || { printf 'build-native: no PDB beside the profiling DLL at %s\n' "$pdb" >&2; exit 1; }
			cp -p "$pdb" "$OUTDIR/orbitnet.pdb"
		fi

		[ -s "$OUTDIR/$ship" ] || { printf 'build-native: staged %s is empty\n' "$ship" >&2; exit 1; }
	done

	printf '\nbuild-native: staged into %s\n' "$OUTDIR"
	ls -l "$OUTDIR"
	;;
*)
	usage
	;;
esac
