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
# macOS builds BOTH architectures per profile and lipos them together, PROFILING INCLUDED. A single-arch
# dylib works on the machine that built it and fails on the other half of the Mac install base. The
# profiling `.dSYM` is produced from the lipo'd dylib rather than per architecture, so one bundle covers
# both slices -- see the macOS branch under `build`.
#
# `linux_arm64` CROSS-COMPILES ON THE x86_64 LINUX RUNNER. It targets `aarch64-unknown-linux-gnu` and
# carries the same runner labels as `linux`, so no arm64 hardware is involved -- the shape macOS already
# uses to build an x86_64 slice on an arm64 box. It needs an aarch64 LINKER, which `rustup target add`
# does not install, and the build refuses to start without one rather than failing from inside a cargo
# error or uploading an empty artifact.
#
# The three `android_*` keys are one ABI each, cross-compiled on the same x86_64 Linux box. Android ships
# a separate .so per ABI and the descriptor names each one, so one key per ABI is what keeps the
# one-key-one-filename mapping intact. They link with the NDK's clang wrappers, which no rustup target
# carries and no package manager installs as a compiler package -- same failure shape as the aarch64
# linker above, same preflight, and the build refuses to start without them.
#
# There is no `ios` key. An iOS GDExtension is a static library packaged as an `.xcframework`: a
# directory holding a device slice and a simulator slice that cannot be lipo'd together, since both are
# arm64. Its shipped artifact is a bundle rather than a file, which is a different shape from the one
# name per platform per profile this script maps. The static library itself is not the obstacle --
# `cargo rustc --crate-type staticlib` overrides the manifest for one invocation, the way the profiling
# and page-alignment flags below already do. See docs/building.md.
#
# Usage:
#   tools/build-native.sh host                            print this machine's platform
#   tools/build-native.sh names <platform> [profile...]   print shipped filenames; build nothing
#   tools/build-native.sh ndk-root                        print this machine's Android NDK root, or nothing
#   tools/build-native.sh build <platform> <outdir> [profile...]
#
# <platform> is linux | linux_arm64 | windows | macos | android_arm64 | android_arm32 | android_x86_64.
# Profiles default to the two the descriptor names.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NATIVE="$ROOT/native"

# WHERE CARGO ACTUALLY PUTS THE ARTIFACTS, which is not always `native/target`. A contributor with a shared
# cargo cache sets `CARGO_TARGET_DIR`, cargo honours it, and a script that reads `$NATIVE/target` then finds
# nothing: on macOS `lipo` reported "no eligible inputs found" after two builds that had both succeeded.
# Asking cargo rather than assuming keeps the two in agreement.
TARGET_ROOT="${CARGO_TARGET_DIR:-$NATIVE/target}"

DEFAULT_PROFILES=(template_debug template_release)

usage() {
	printf 'usage: %s host\n' "$0" >&2
	printf '       %s names <platform> [profile...]\n' "$0" >&2
	printf '       %s ndk-root\n' "$0" >&2
	printf '       %s build <platform> <outdir> [profile...]\n' "$0" >&2
	printf 'platform: linux | linux_arm64 | windows | macos | android_arm64 | android_arm32 | android_x86_64\n' >&2
	printf 'profile:  template_debug | template_release | profiling\n' >&2
	exit 2
}

# Cargo's own output filename for a platform, and the two halves of the shipped name around the profile.
# CROSS_TARGET is the rustup target this platform builds when it is not the host's own; empty means a
# native build. It drives `rustup target add`, cargo's `--target` and the directory the output lands in,
# so a cross platform is one case here rather than a branch in three places.
#
# NDK_CLANG_PREFIX is set by the android keys only, and is the basename of the NDK clang wrapper this
# target links with, minus the API level. It is not derivable from CROSS_TARGET: the NDK spells the
# 32-bit arm wrapper `armv7a-`, while the rust target is `armv7-`.
platform_parts() {
	CROSS_TARGET=""
	NDK_CLANG_PREFIX=""
	case "$1" in
		linux)   BUILT_NAME="liborbitnet.so";  SHIP_PREFIX="liborbitnet.linux";  SHIP_SUFFIX="x86_64.so" ;;
		linux_arm64)
			BUILT_NAME="liborbitnet.so";  SHIP_PREFIX="liborbitnet.linux";  SHIP_SUFFIX="arm64.so"
			# THE GNU TARGET, NOT musl. The descriptor entry sits beside a Godot export template linked
			# against glibc, and a musl cdylib loaded into a glibc process is a different libc in one
			# address space.
			CROSS_TARGET="aarch64-unknown-linux-gnu" ;;
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
			CROSS_TARGET="x86_64-pc-windows-msvc" ;;
		macos)   BUILT_NAME="liborbitnet.dylib"; SHIP_PREFIX="liborbitnet.macos"; SHIP_SUFFIX="universal.dylib" ;;
		# One key per android ABI. Android has no universal container, so each ABI is its own .so, its own
		# descriptor entry and therefore its own key. The suffixes are Godot's architecture names rather
		# than the NDK's directory names -- `arm64` and `arm32`, not `arm64-v8a` and `armeabi-v7a` --
		# because they are what the `[libraries]` key in the .gdextension is built from, and a wrong one
		# there fails at `dlopen` on that ABI and nowhere else.
		android_arm64)
			BUILT_NAME="liborbitnet.so"; SHIP_PREFIX="liborbitnet.android"; SHIP_SUFFIX="arm64.so"
			CROSS_TARGET="aarch64-linux-android"; NDK_CLANG_PREFIX="aarch64-linux-android" ;;
		android_arm32)
			BUILT_NAME="liborbitnet.so"; SHIP_PREFIX="liborbitnet.android"; SHIP_SUFFIX="arm32.so"
			CROSS_TARGET="armv7-linux-androideabi"; NDK_CLANG_PREFIX="armv7a-linux-androideabi" ;;
		android_x86_64)
			BUILT_NAME="liborbitnet.so"; SHIP_PREFIX="liborbitnet.android"; SHIP_SUFFIX="x86_64.so"
			CROSS_TARGET="x86_64-linux-android"; NDK_CLANG_PREFIX="x86_64-linux-android" ;;
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
	# NO PER-PROFILE EXCEPTION. macOS `profiling` used to ship as `liborbitnet.macos.profiling.arm64.dylib`
	# on the reasoning that two per-architecture debug maps cannot be lipo'd into a usable .dSYM. That is
	# still true, and it stopped being the constraint once the order changed. The two slices are lipo'd
	# FIRST and `dsymutil` runs on the fat dylib, reading both slices' debug maps and writing one fat
	# .dSYM that carries both UUIDs. Every profile on every platform now takes the same name shape. See
	# the macOS branch under `build`.
	printf '%s.%s.%s\n' "$SHIP_PREFIX" "$2" "$SHIP_SUFFIX"
}

# The aarch64 Linux cross build links with a toolchain rustup does not install, and a missing linker
# surfaces as "unrecognized file format" out of the middle of a cargo error -- or, on a runner, as a leg
# that quietly produces nothing. Name the requirement here instead, before anything is compiled.
require_aarch64_linux_linker() {
	# A native aarch64 host links with its own `cc`; nothing to cross to.
	case "$(uname -s)/$(uname -m)" in
		Linux/aarch64|Linux/arm64) return ;;
	esac
	# An explicit setting wins: a box may carry a clang or a crosstool-ng toolchain under any name.
	# An `if`, not `a && return`: under `set -e` a false `&&` list is a failed statement and kills the run.
	if [ -n "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-}" ]; then
		printf 'build-native: aarch64 cross linker %s (from the environment)\n' \
			"$CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"
		return
	fi
	for cc in aarch64-linux-gnu-gcc aarch64-unknown-linux-gnu-gcc aarch64-linux-gnu-cc; do
		if command -v "$cc" >/dev/null 2>&1; then
			export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$cc"
			printf 'build-native: aarch64 cross linker %s\n' "$(command -v "$cc")"
			return
		fi
	done
	printf 'build-native: no aarch64 cross linker on this machine, so the linux_arm64 build cannot link.\n' >&2
	printf '  `rustup target add aarch64-unknown-linux-gnu` installs the Rust std libraries only. rustc\n' >&2
	printf '  shells out to a C linker for the cdylib, and that is a separate package:\n' >&2
	printf '    Debian/Ubuntu  sudo apt-get install -y gcc-aarch64-linux-gnu\n' >&2
	printf '    Fedora/RHEL    sudo dnf install -y gcc-aarch64-linux-gnu\n' >&2
	printf '    Arch           sudo pacman -S aarch64-linux-gnu-gcc\n' >&2
	printf '    Nix            pkgsCross.aarch64-multiplatform.stdenv.cc\n' >&2
	printf '  Or export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER pointing at a linker you have.\n' >&2
	exit 1
}

# The android API level the libraries are compiled against is a floor rather than a target: a library
# built against 21 loads on every device at or above it, so the lowest level the current NDKs still ship
# a wrapper for is the one that constrains a consumer least. Godot's own export template sets the minimum
# SDK an app declares, and this staying at or below that is what makes the library loadable on every
# device that template supports. Override with ORBITNET_ANDROID_API to build against a higher one.
ANDROID_API="${ORBITNET_ANDROID_API:-21}"

# Print the path of the NDK clang wrapper for one API level, or nothing. `.cmd` is the same wrapper on a
# Windows host, where the NDK ships batch files rather than shell scripts.
ndk_wrapper() {
	if [ -f "$1/$NDK_CLANG_PREFIX$2-clang" ]; then
		printf '%s\n' "$1/$NDK_CLANG_PREFIX$2-clang"
	elif [ -f "$1/$NDK_CLANG_PREFIX$2-clang.cmd" ]; then
		printf '%s\n' "$1/$NDK_CLANG_PREFIX$2-clang.cmd"
	fi
	return 0
}

# Print this machine's Android NDK root, or nothing. Exposed as `tools/build-native.sh ndk-root` so both
# workflows resolve an NDK with THIS function rather than a copy of it -- a second search that drifted
# from this one would fail a leg on a box the build itself can build on.
#
# The selection rule, in order, first hit wins:
#   1. An explicit setting, in every spelling the SDK tools and the CI images use.
#   2. The side-by-side `ndk/<version>` layout under an SDK root, most explicit root first. Within one
#      root the newest wins: a glob expands sorted, so the last match is the highest version while NDK
#      majors are two digits.
#   3. The distribution packages that are an NDK root themselves rather than a side-by-side directory.
#
# Root order decides which NDK wins; version comparison happens only within one root. Comparing versions
# across roots was the earlier rule and it was wrong: it let a stale NDK under $HOME outrank the pinned one
# `sdkmanager --install` writes under ANDROID_SDK_ROOT, and the build then linked against the stale one
# with nothing said. A machine carrying several and wanting a particular one sets ANDROID_NDK_HOME.
android_ndk_root() {
	for d in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}" "${ANDROID_NDK:-}" "${NDK_HOME:-}" \
		"${ANDROID_NDK_LATEST_HOME:-}"
	do
		if [ -n "$d" ] && [ -d "$d/toolchains/llvm/prebuilt" ]; then
			printf '%s\n' "$d"
			return 0
		fi
	done
	for base in "${ANDROID_SDK_ROOT:-}" "${ANDROID_HOME:-}" "$HOME/Android/Sdk" \
		"$HOME/Library/Android/sdk" /usr/lib/android-sdk /opt/android-sdk
	do
		# An unset SDK variable would otherwise glob `/ndk/*` and match whatever a root-level directory
		# of that name happens to hold.
		if [ -z "$base" ]; then
			continue
		fi
		found=""
		for d in "$base/ndk"/*; do
			if [ -d "$d/toolchains/llvm/prebuilt" ]; then
				found="$d"
			fi
		done
		if [ -n "$found" ]; then
			printf '%s\n' "$found"
			return 0
		fi
	done
	found=""
	for d in /usr/lib/android-ndk /opt/android-ndk*; do
		if [ -d "$d/toolchains/llvm/prebuilt" ]; then
			found="$d"
		fi
	done
	if [ -n "$found" ]; then
		printf '%s\n' "$found"
	fi
	return 0
}

# The android cross builds link with the NDK's clang wrappers. `rustup target add` supplies the Rust std
# libraries and nothing else; rustc shells out to a linker for the cdylib, and for these targets that
# linker carries Bionic's libc and crt objects and lives inside a separate SDK download. Same failure
# shape as the aarch64 linker above -- a missing one surfaces from the middle of a cargo error, or on a
# runner as a leg that produces nothing -- so it is named here, before anything is compiled.
#
# Takes the platform key, for the message. CROSS_TARGET and NDK_CLANG_PREFIX come from platform_parts.
require_android_ndk_linker() {
	var="CARGO_TARGET_$(printf '%s' "$CROSS_TARGET" | tr '[:lower:]' '[:upper:]' | tr '-' '_')_LINKER"
	# An explicit setting wins: a box may carry a standalone toolchain, a ccache shim or a wrapper of its
	# own under any name.
	if [ -n "${!var:-}" ]; then
		printf 'build-native: %s linker %s (from the environment)\n' "$1" "${!var}"
		return
	fi
	root="$(android_ndk_root)"
	wrapper=""
	prebuilt=""
	if [ -n "$root" ]; then
		# One prebuilt directory per install, named for the host the NDK runs on rather than the target it
		# builds for. Globbing it beats computing the host tag, which differs from `uname` output and is
		# `darwin-x86_64` even on an Apple silicon Mac.
		for d in "$root"/toolchains/llvm/prebuilt/*/bin; do
			if [ -d "$d" ]; then
				prebuilt="$d"
			fi
		done
	fi
	if [ -n "$prebuilt" ]; then
		wrapper="$(ndk_wrapper "$prebuilt" "$ANDROID_API")"
		if [ -z "$wrapper" ]; then
			# The requested level is absent, so take the lowest this NDK ships -- the one that excludes
			# the fewest devices -- rather than failing a build the toolchain can do.
			api="$(ls "$prebuilt" 2>/dev/null \
				| sed -n "s/^$NDK_CLANG_PREFIX\([0-9][0-9]*\)-clang\(\.cmd\)\{0,1\}\$/\1/p" \
				| sort -n | head -1 || true)"
			if [ -n "$api" ]; then
				wrapper="$(ndk_wrapper "$prebuilt" "$api")"
				printf 'build-native: this NDK ships no API %s wrapper for %s; using API %s\n' \
					"$ANDROID_API" "$CROSS_TARGET" "$api"
			fi
		fi
	fi
	if [ -n "$wrapper" ]; then
		export "$var=$wrapper"
		printf 'build-native: %s linker %s\n' "$1" "$wrapper"
		return
	fi
	if [ -n "$prebuilt" ]; then
		printf 'build-native: the NDK at %s has no clang wrapper for %s, so the %s build cannot link.\n' \
			"$root" "$CROSS_TARGET" "$1" >&2
		printf '  Looked for %s<api>-clang under %s. An NDK that dropped this ABI, or a partial install.\n' \
			"$NDK_CLANG_PREFIX" "$prebuilt" >&2
		printf '  Install another NDK and point ANDROID_NDK_HOME at it, or export %s directly.\n' "$var" >&2
		exit 1
	fi
	printf 'build-native: no Android NDK on this machine, so the %s build cannot link.\n' "$1" >&2
	printf '  `rustup target add %s` installs the Rust std libraries only. rustc shells out to\n' "$CROSS_TARGET" >&2
	printf '  the NDK clang wrapper to link the .so, and the NDK is a separate download:\n' >&2
	printf '    Android Studio  SDK Manager -> SDK Tools -> NDK (Side by side)\n' >&2
	printf '    Command line    sdkmanager --install "ndk;26.3.11579264"\n' >&2
	printf '    Debian/Ubuntu   sudo apt-get install -y google-android-ndk-installer\n' >&2
	printf '    Arch            sudo pacman -S android-ndk\n' >&2
	printf '    Nix             pkgs.androidenv.androidPkgs.ndk-bundle\n' >&2
	printf '  Then set ANDROID_NDK_HOME to the NDK root, or export %s pointing at\n' "$var" >&2
	printf '  the clang wrapper directly.\n' >&2
	exit 1
}

MODE="${1:-}"
[ -n "$MODE" ] || usage
shift || usage

case "$MODE" in
host)
	case "$(uname -s)" in
		# THE ARCHITECTURE MATTERS ON LINUX AND NOWHERE ELSE. Windows ships x86_64 only and macOS ships
		# one universal dylib, so `uname -m` changes nothing there. On Linux the two architectures are
		# two files and two descriptor entries, and `just native-install` on an arm64 box must build and
		# stage the one Godot will load.
		Linux)
			case "$(uname -m)" in
				aarch64|arm64) printf 'linux_arm64\n' ;;
				*) printf 'linux\n' ;;
			esac ;;
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
ndk-root)
	# For the workflows' `Locate the Android NDK` step, so CI and the build agree on what counts as an
	# NDK and on which one wins. Prints nothing and exits 0 when there is none: the caller decides
	# whether that is fatal, and the build's own preflight prints the install instructions.
	android_ndk_root
	;;
build)
	PLATFORM="${1:-}"; [ -n "$PLATFORM" ] || usage; shift
	OUTDIR="${1:-}"; [ -n "$OUTDIR" ] || usage; shift
	profiles=("$@"); [ "${#profiles[@]}" -gt 0 ] || profiles=("${DEFAULT_PROFILES[@]}")

	platform_parts "$PLATFORM"
	mkdir -p "$OUTDIR"

	# BEFORE ANY CARGO WORK. A leg with no cross linker must say so in its first seconds rather than
	# after a full dependency build.
	if [ "$PLATFORM" = linux_arm64 ]; then
		require_aarch64_linux_linker
	fi
	case "$PLATFORM" in
		android_*) require_android_ndk_linker "$PLATFORM" ;;
	esac

	# `cd native` FIRST, in both of the `rustup target add` calls below. native/rust-toolchain.toml pins
	# the toolchain cargo uses in that directory, while the repository root has no override and resolves
	# to rustup's default. Running `rustup target add` from the root installs the std libraries onto the
	# DEFAULT toolchain, which the pinned one cannot see, and the cross build then fails with "can't find
	# crate for `std`".
	if [ -n "$CROSS_TARGET" ]; then
		( cd "$NATIVE" && rustup target add "$CROSS_TARGET" )
	fi

	if [ "$PLATFORM" = macos ]; then
		( cd "$NATIVE" && rustup target add x86_64-apple-darwin aarch64-apple-darwin )
	fi

	for p in "${profiles[@]}"; do
		profile_parts "$p"
		ship="$(shipped_name "$PLATFORM" "$p")"
		printf '\nbuild-native: %s / %s -> %s\n' "$PLATFORM" "$p" "$ship"

		# Link arguments the platform needs on every profile, before the profiling-only ones below.
		# Only the 64-bit android ABIs have one: Android devices with 16 KB memory pages reject a
		# library whose segments are aligned to 4 KB, and Play requires 16 KB alignment of apps
		# targeting recent API levels. NDK r27 and newer link that way by default and this flag is
		# then a no-op; an older NDK does not, and the failure is a `dlopen` on a 16 KB device and
		# nowhere else. The 32-bit ABI is unaffected -- no 16 KB-page device runs a 32-bit userspace.
		case "$PLATFORM" in
			android_arm64|android_x86_64) RUSTC_ARGS=(-C link-arg=-Wl,-z,max-page-size=16384) ;;
			*)                            RUSTC_ARGS=() ;;
		esac

		# A PROFILING build needs a per-platform rustc flag that a plain `cargo build` does not pass,
		# and without it the artifact is published but unusable by a profiler:
		#   linux    a Rust cdylib link does not request a GNU build ID in this toolchain, and perf
		#            records build IDs when locating ELF images. Both architectures, same reason.
		#   android  the same ELF build ID, for the same reason -- simpleperf and ndk-stack locate an
		#            image by it.
		#   macos    rustc leaves DWARF in the object files and links only a debug map, so the dylib
		#            alone is unsymbolizable. `unpacked` is that behavior REQUESTED rather than
		#            inherited, and it leaves the object files in place for the dsymutil run below --
		#            `packed` would run dsymutil per architecture and delete them, and two
		#            per-architecture .dSYMs cannot be combined after the fact.
		#   windows  nothing extra -- the MSVC linker always writes a PDB and stamps the image with the
		#            CodeView key naming it.
		if [ "$p" = profiling ]; then
			case "$PLATFORM" in
				linux|linux_arm64|android_*) RUSTC_ARGS+=(-C link-arg=-Wl,--build-id) ;;
				macos)                       RUSTC_ARGS+=(-C split-debuginfo=unpacked) ;;
			esac
		fi

		if [ "$PLATFORM" = macos ]; then
			# EVERY PROFILE IS UNIVERSAL, profiling included. Build both architectures, lipo them, and for
			# `profiling` run dsymutil on the RESULT: it walks each slice's debug map in turn and writes a
			# single fat .dSYM whose UUIDs are the fat dylib's own, which is what a debugger matches on.
			for arch_target in x86_64-apple-darwin aarch64-apple-darwin; do
				if [ "${#RUSTC_ARGS[@]}" -gt 0 ]; then
					( cd "$NATIVE" && cargo rustc "${CARGO_FLAG[@]}" -p orbitnet-godot \
						--target "$arch_target" -- "${RUSTC_ARGS[@]}" )
				else
					( cd "$NATIVE" && cargo build "${CARGO_FLAG[@]}" -p orbitnet-godot --target "$arch_target" )
				fi
			done
			lipo -create -output "$OUTDIR/$ship" \
				"$TARGET_ROOT/x86_64-apple-darwin/$CARGO_DIR/$BUILT_NAME" \
				"$TARGET_ROOT/aarch64-apple-darwin/$CARGO_DIR/$BUILT_NAME"
			lipo -info "$OUTDIR/$ship"
		else
			TARGET_ARGS=()
			outdir="$TARGET_ROOT/$CARGO_DIR"
			if [ -n "$CROSS_TARGET" ]; then
				TARGET_ARGS=(--target "$CROSS_TARGET")
				outdir="$TARGET_ROOT/$CROSS_TARGET/$CARGO_DIR"
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

		# Prove the profiling artifact is actually symbolizable, rather than trusting the flag. readelf
		# reads a foreign-architecture ELF, so every cross-built artifact -- the aarch64 one and all
		# three android ABIs -- is checked on the x86_64 box that produced it.
		case "$PLATFORM" in
			linux|linux_arm64|android_*) ELF_PLATFORM=1 ;;
			*)                           ELF_PLATFORM=0 ;;
		esac
		if [ "$p" = profiling ] && [ "$ELF_PLATFORM" -eq 1 ] \
			&& command -v readelf >/dev/null 2>&1; then
			readelf -S --wide "$OUTDIR/$ship" | grep -q '\.debug_info' \
				|| { printf 'build-native: %s has no .debug_info\n' "$ship" >&2; exit 1; }
			readelf -n "$OUTDIR/$ship" | grep -q 'Build ID' \
				|| { printf 'build-native: %s has no ELF build ID\n' "$ship" >&2; exit 1; }
		fi

		# THE .dSYM IS BUILT FROM THE STAGED FAT DYLIB, and it has to cover every slice. A debugger
		# matches a bundle to an image by UUID, so comparing the dylib's UUID set against the bundle's is
		# the whole proof: a bundle covering one architecture symbolizes on that machine and silently
		# leaves the other half of the install base with addresses, which is the failure the universal
		# policy exists to prevent.
		if [ "$p" = profiling ] && [ "$PLATFORM" = macos ]; then
			# Asserted rather than skipped the way the readelf checks above are. readelf is a diagnostic
			# and a host without it still produced the library; the .dSYM IS the artifact, so a missing
			# dsymutil means there is nothing to ship and the build has to say so.
			command -v dsymutil >/dev/null 2>&1 || {
				printf 'build-native: no dsymutil on PATH. It ships with the Xcode command line tools:\n' >&2
				printf '  xcode-select --install\n' >&2
				exit 1; }
			rm -rf "$OUTDIR/$ship.dSYM"
			dsymutil "$OUTDIR/$ship"
			dwarf="$OUTDIR/$ship.dSYM/Contents/Resources/DWARF/$ship"
			[ -s "$dwarf" ] || {
				printf 'build-native: dsymutil wrote no DWARF at %s. The object files it reads are the\n' "$dwarf" >&2
				printf '  ones `-C split-debuginfo=unpacked` leaves under target/; a `packed` build deletes them.\n' >&2
				exit 1; }
			# One UUID line per slice, so the count is the slice count. TWO, spelled out: universal here
			# means x86_64 and arm64, and a single-arch dylib with a matching single-arch bundle would
			# otherwise satisfy an equality test while shipping the exact defect this replaces.
			lib_uuids="$(dwarfdump --uuid "$OUTDIR/$ship" | awk '/^UUID:/ {print $2}' | sort)"
			sym_uuids="$(dwarfdump --uuid "$dwarf" | awk '/^UUID:/ {print $2}' | sort)"
			# `|| true` because `grep -c` exits 1 on a count of zero, and under `set -euo pipefail` that
			# kills the assignment and the script with it -- before the branch below can print which UUIDs
			# each side actually had, which is the only useful thing in an Actions log.
			slices="$(printf '%s\n' "$lib_uuids" | grep -c . || true)"
			if [ "$slices" -ne 2 ] || [ "$lib_uuids" != "$sym_uuids" ]; then
				printf 'build-native: %s.dSYM does not cover both slices of the dylib.\n' "$ship" >&2
				printf '  dylib UUIDs: %s\n' "$(printf '%s' "$lib_uuids" | tr '\n' ' ')" >&2
				printf '  .dSYM UUIDs: %s\n' "$(printf '%s' "$sym_uuids" | tr '\n' ' ')" >&2
				exit 1
			fi
			lipo -info "$dwarf"
		fi

		# The msvc linker always writes a PDB and stamps the image with the CodeView key naming it.
		# The ABI is pinned above, so an absent PDB is a real failure rather than a toolchain
		# difference. It keeps the name the DLL records: dbghelp searches a symbol path for that base
		# name, so renaming it to match the platform-tagged library would hide it from every analyzer.
		if [ "$PLATFORM" = windows ] && [ "$p" = profiling ]; then
			pdb="$outdir/orbitnet.pdb"
			[ -s "$pdb" ] || {
				printf 'build-native: no PDB at %s. The msvc linker always writes one, so this means the\n' "$pdb" >&2
				printf '  build did not use %s. Check `rustup target list --installed`.\n' "$CROSS_TARGET" >&2
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
