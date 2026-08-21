#!/usr/bin/env bash
# Every library filename the .gdextension names must be one the build path actually produces, and every
# platform that path knows must be a leg the build workflow actually runs.
#
# This exists because a descriptor entry naming a file nothing builds fails at `dlopen` on the affected
# platform, and nowhere else. `orbitnet.windows.x86_64.dll` was named by the descriptor and produced by no
# commit for the entire life of the repository: every gate ran on Linux, loaded the Linux library, and went
# green, while a Windows checkout took the `Net` autoload down with it.
#
# The check is a set comparison, not a file test. CI runs one platform at a time and cannot see the other
# two, so what it can prove is that the lists agree -- and disagreement is exactly the defect.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DESCRIPTOR="addons/orbitnet_native/orbitnet.gdextension"
WORKFLOWS=".github/workflows/binaries.yml .github/workflows/release.yml"
PLATFORMS=(linux windows macos)

# Filenames the descriptor points at, deduplicated: several entries may name one file.
# `|| true` then an explicit emptiness check: under `set -euo pipefail` a grep that matches nothing kills
# the assignment and the script exits 1 having printed nothing, which is indistinguishable in an Actions
# log from a real parity break.
named="$(grep -oE 'res://addons/orbitnet_native/bin/[^"]+' "$DESCRIPTOR" \
	| sed 's|.*/||' | sort -u || true)"
if [ -z "$named" ]; then
	printf '::error::found no bin/ library paths in %s -- the descriptor shape changed and this check needs updating.\n' "$DESCRIPTOR"
	exit 1
fi

# Filenames the build path stages, asked of the script that owns the mapping rather than scraped out of
# YAML. Only the descriptor profiles: `profiling` is a release asset and deliberately not an entry.
built="$(for p in "${PLATFORMS[@]}"; do tools/build-native.sh names "$p"; done | sort -u)"

missing="$(comm -23 <(printf '%s\n' "$named") <(printf '%s\n' "$built") || true)"
unused="$(comm -13 <(printf '%s\n' "$named") <(printf '%s\n' "$built") || true)"

status=0
if [ -n "$missing" ]; then
	printf '::error::descriptor names libraries the build path does not produce:\n'
	printf '%s\n' "$missing" | sed 's/^/  /'
	printf '\nEither add a platform case to tools/build-native.sh or drop the entry from %s.\n' "$DESCRIPTOR"
	status=1
fi
if [ -n "$unused" ]; then
	printf '::error::the build path stages libraries the descriptor never loads:\n'
	printf '%s\n' "$unused" | sed 's/^/  /'
	printf '\nA built artifact nothing names is dead weight in every release.\n'
	status=1
fi

# A platform tools/build-native.sh can name but no workflow leg ever runs produces nothing, which is the
# same defect one step further back. `platform:` is the matrix key each leg passes to the script.
#
# BOTH WORKFLOWS, not just binaries.yml. release.yml carries its own copy of the three-leg matrix, and it
# is the one that actually ships -- a platform missing there is caught at tag time, after three builds,
# or not at all.
for wf in $WORKFLOWS; do
	legs="$(grep -oE '^[[:space:]]*-?[[:space:]]*platform:[[:space:]]*[a-z0-9_]+' "$wf" \
		| sed 's/.*platform:[[:space:]]*//' | sort -u || true)"
	for p in "${PLATFORMS[@]}"; do
		if ! printf '%s\n' "$legs" | grep -qx "$p"; then
			printf '::error::%s is a platform tools/build-native.sh builds, but %s has no leg for it.\n' \
				"$p" "$wf"
			status=1
		fi
	done
done

if [ "$status" -eq 0 ]; then
	printf 'descriptor parity passed: %s library filename(s) across %s platform(s), all built and all named.\n' \
		"$(printf '%s\n' "$named" | grep -c .)" "${#PLATFORMS[@]}"
fi
exit "$status"
