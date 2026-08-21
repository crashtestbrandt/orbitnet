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
		windows) BUILT_NAME="orbitnet.dll";    SHIP_PREFIX="orbitnet.windows";   SHIP_SUFFIX="x86_64.dll"
			# THE MSVC ABI, PINNED. `rust-toolchain.toml` fixes the channel and not the host triple, so
			# the ABI otherwise comes from whichever rustup the runner service's account owns -- and a
			# box can carry several runner services under different accounts with different defaults.
			# One such box built gnu-ABI here while the consuming project had always shipped msvc.
			#
			# It matters twice. A Godot Windows export template is msvc-linked, so a GDExtension beside
			# it should be too; and the msvc linker is what writes a PDB. A gnu build keeps DWARF inside
			# the DLL, which no Windows profiler reads: a PE records a CodeView key, and nothing in the
			# image stands in for the PDB that key names.
			WIN_TARGET="x86_64-pc-windows-msvc" ;;
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
	# macOS PROFILING is arm64, not universal. A universal profiling dylib would need two debug maps
	# lipo'd together, which produces nothing dsymutil can turn into a usable .dSYM -- and the profile
	# export presets target arm64 anyway. The two descriptor profiles stay universal.
	if [ "$1" = macos ] && [ "$2" = profiling ]; then
		printf '%s.profiling.arm64.dylib\n' "$SHIP_PREFIX"
		return
	fi
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

	if [ "$PLATFORM" = windows ]; then
		# `cd native` FIRST, for the reason spelled out in the macOS branch below.
		( cd "$NATIVE" && rustup target add "$WIN_TARGET" )
	fi

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

		# A PROFILING build needs a per-platform rustc flag that a plain `cargo build` does not pass,
		# and without it the artifact is published but unusable by a profiler:
		#   linux  a Rust cdylib link does not request a GNU build ID in this toolchain, and perf
		#          records build IDs when locating ELF images.
		#   macos  rustc's default leaves DWARF in the object files and links only a debug map, so the
		#          dylib alone is unsymbolizable. `packed` runs dsymutil and emits the .dSYM.
		#   windows nothing extra -- the MSVC linker always writes a PDB and stamps the image with the
		#          CodeView key naming it.
		if [ "$p" = profiling ]; then
			case "$PLATFORM" in
				linux)   RUSTC_ARGS=(-C link-arg=-Wl,--build-id) ;;
				macos)   RUSTC_ARGS=(-C split-debuginfo=packed) ;;
				windows) RUSTC_ARGS=() ;;
			esac
		else
			RUSTC_ARGS=()
		fi

		if [ "$PLATFORM" = macos ] && [ "$p" = profiling ]; then
			# arm64 only, and the .dSYM rides beside it. See shipped_name().
			( cd "$NATIVE" && cargo rustc "${CARGO_FLAG[@]}" -p orbitnet-godot \
				--target aarch64-apple-darwin -- "${RUSTC_ARGS[@]}" )
			built="$NATIVE/target/aarch64-apple-darwin/$CARGO_DIR/$BUILT_NAME"
			[ -s "$built" ] || { printf 'build-native: cargo produced nothing at %s\n' "$built" >&2; exit 1; }
			install -m 0755 "$built" "$OUTDIR/$ship"
			[ -d "$built.dSYM" ] || { printf 'build-native: no .dSYM beside %s\n' "$built" >&2; exit 1; }
			rm -rf "$OUTDIR/$ship.dSYM"
			# -L dereferences: cargo leaves target/<profile>/*.dSYM as a symlink into deps/, and a
			# staged symlink points back at a tree the next build rewrites.
			cp -RLp "$built.dSYM" "$OUTDIR/$ship.dSYM"
		elif [ "$PLATFORM" = macos ]; then
			( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" -p orbitnet-godot --target x86_64-apple-darwin )
			( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" -p orbitnet-godot --target aarch64-apple-darwin )
			lipo -create -output "$OUTDIR/$ship" \
				"$NATIVE/target/x86_64-apple-darwin/$CARGO_DIR/$BUILT_NAME" \
				"$NATIVE/target/aarch64-apple-darwin/$CARGO_DIR/$BUILT_NAME"
			lipo -info "$OUTDIR/$ship"
		else
			TARGET_ARGS=()
			outdir="$NATIVE/target/$CARGO_DIR"
			if [ "$PLATFORM" = windows ]; then
				TARGET_ARGS=(--target "$WIN_TARGET")
				outdir="$NATIVE/target/$WIN_TARGET/$CARGO_DIR"
			fi
			if [ "${#RUSTC_ARGS[@]}" -gt 0 ]; then
				( cd "$NATIVE" && cargo rustc "${CARGO_FLAG[@]}" "${TARGET_ARGS[@]}" -p orbitnet-godot -- "${RUSTC_ARGS[@]}" )
			else
				( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" "${TARGET_ARGS[@]}" -p orbitnet-godot )
			fi
			built="$outdir/$BUILT_NAME"
			[ -s "$built" ] || { printf 'build-native: cargo produced nothing at %s\n' "$built" >&2; exit 1; }
			install -m 0755 "$built" "$OUTDIR/$ship"
		fi

		# Prove the profiling artifact is actually symbolizable, rather than trusting the flag.
		if [ "$p" = profiling ] && [ "$PLATFORM" = linux ] && command -v readelf >/dev/null 2>&1; then
			readelf -S --wide "$OUTDIR/$ship" | grep -q '\.debug_info' \
				|| { printf 'build-native: %s has no .debug_info\n' "$ship" >&2; exit 1; }
			readelf -n "$OUTDIR/$ship" | grep -q 'Build ID' \
				|| { printf 'build-native: %s has no ELF build ID\n' "$ship" >&2; exit 1; }
		fi

		# The msvc linker always writes a PDB and stamps the image with the CodeView key naming it.
		# The ABI is pinned above, so an absent PDB is a real failure rather than a toolchain
		# difference. It keeps the name the DLL records: dbghelp searches a symbol path for that base
		# name, so renaming it to match the platform-tagged library would hide it from every analyzer.
		if [ "$PLATFORM" = windows ] && [ "$p" = profiling ]; then
			pdb="$outdir/orbitnet.pdb"
			[ -s "$pdb" ] || {
				printf 'build-native: no PDB at %s. The msvc linker always writes one, so this means the\n' "$pdb" >&2
				printf '  build did not use %s. Check `rustup target list --installed`.\n' "$WIN_TARGET" >&2
				exit 1; }
			cp -p "$pdb" "$OUTDIR/orbitnet.pdb"
			printf 'build-native: shipping orbitnet.pdb beside the DLL\n'
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
