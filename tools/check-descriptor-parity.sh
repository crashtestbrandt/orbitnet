#!/usr/bin/env bash
# Every library filename the .gdextension names must be one the build workflows actually produce.
#
# This exists because a descriptor entry naming a file nothing builds fails at `dlopen` on the affected
# platform, and nowhere else. `orbitnet.windows.x86_64.dll` was named by the descriptor and produced by no
# commit for the entire life of the repository: every gate ran on Linux, loaded the Linux library, and went
# green, while a Windows checkout took the `Net` autoload down with it.
#
# The check is a set comparison, not a file test. CI runs one platform at a time and cannot see the other
# two, so what it can prove is that the two lists agree -- and disagreement is exactly the defect.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DESCRIPTOR="addons/orbitnet_native/orbitnet.gdextension"
WORKFLOW=".github/workflows/binaries.yml"

# Filenames the descriptor points at, deduplicated: several entries may name one file.
named="$(grep -oE 'res://addons/orbitnet_native/bin/[^"]+' "$DESCRIPTOR" \
	| sed 's|.*/||' | sort -u)"

# Filenames the build matrix stages. `artifact:` is the shipped name in each matrix leg.
built="$(grep -oE '^\s+artifact:\s+\S+' "$WORKFLOW" | awk '{print $2}' | sort -u)"

missing="$(comm -23 <(printf '%s\n' "$named") <(printf '%s\n' "$built") || true)"
unused="$(comm -13 <(printf '%s\n' "$named") <(printf '%s\n' "$built") || true)"

status=0
if [ -n "$missing" ]; then
	printf '::error::descriptor names libraries no workflow builds:\n'
	printf '  %s\n' $missing
	printf '\nEither add a matrix leg to %s or drop the entry from %s.\n' "$WORKFLOW" "$DESCRIPTOR"
	status=1
fi
if [ -n "$unused" ]; then
	printf '::error::workflow builds libraries the descriptor never loads:\n'
	printf '  %s\n' $unused
	printf '\nA built artifact nothing names is dead weight in every release.\n'
	status=1
fi

if [ "$status" -eq 0 ]; then
	printf 'descriptor parity passed: %s library filename(s), all built and all named.\n' \
		"$(printf '%s\n' "$named" | grep -c .)"
fi
exit "$status"
