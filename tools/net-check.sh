#!/usr/bin/env bash
# OrbitNet facade boundary gate.
#
# The rollback backend (the OrbitNet native Rust GDExtension) must be reachable ONLY through
# addons/orbitnet/net.gd. That seam is the whole reason the addon is extractable at all: it is what
# made the backend swap a one-file rewrite, and it is what lets a consuming project depend on `Net`
# without ever naming `OrbitRollbackSynchronizer`. This fails if any file OTHER than net.gd
# references a backend class.
#
# It also still polices the RETIRED netfox symbols. OrbitNet grew out of a vendored netfox fork, and
# a stray revert or a copy-pasted snippet from an old tutorial must not quietly reintroduce it.
#
# It does NOT match the bare word "OrbitNet" in prose -- the addon is named that -- only real
# class/path usage.
#
# THE DEMOS ARE THE POINT. This scans demos/ as well as the addon, so if the RTS demo (or any future
# demo) reaches past the facade, the gate fails. A demo that needs a backend symbol is a demo
# proving the facade has a hole -- widen the facade, do not exempt the demo.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Live backend classes + the binary addon path, and the retired netfox inventory. Full distinctive
# identifiers (no \b word-boundary -- that is a GNU-only extension to ERE and behaves inconsistently
# on BSD/macOS grep), so e.g. our own Net / NetTransport / NetCommand never match.
# RollbackSynchronizer and StateSynchronizer carry a leading (^|[^A-Za-z_]) guard so the LIVE
# OrbitRollbackSynchronizer / OrbitStateSynchronizer tokens (which contain them as substrings) are
# matched by their own entries, not the netfox ones.
PATTERN='OrbitRollbackSynchronizer|OrbitStateSynchronizer|OrbitInterpolator|OrbitNet\.new|res://addons/orbitnet_native|NetworkTime|NetworkTimeSynchronizer|NetworkRollback|NetworkEvents|NetworkPerformance|(^|[^A-Za-z_])RollbackSynchronizer|(^|[^A-Za-z_])StateSynchronizer|TickInterpolator|RewindableAction|NetfoxLogger|PeerVisibilityFilter|res://addons/netfox'

# The trees to police, and the one allowed file. Note what is NOT scanned: the SYNCED copies of the
# addon under harness/addons/ and demos/*/addons/. They are byte-identical build artifacts of the
# canonical source (tools/sync-addons.sh, gitignored), so scanning them would re-report every hit in
# net.gd under a path the `^addons/orbitnet/net.gd:` exemption does not cover -- the gate would fail
# on a correct tree the moment anyone ran `just sync-addons`. `just addon-drift` is what proves the
# copies match the canonical source; this gate only ever needs to read the canonical one.
FILES="$(find addons/orbitnet tools harness demos \
	-type d \( -name addons -o -name .godot -o -name target \) -prune -o \
	-type f \( -name '*.gd' -o -name '*.tscn' \) -print 2>/dev/null | sort || true)"

if [ -z "$FILES" ]; then
	printf 'net-check FAILED: found no .gd/.tscn files to scan (wrong working directory?)\n' >&2
	exit 1
fi

hits="$(printf '%s\n' "$FILES" | tr '\n' '\0' | xargs -0 grep -nE "$PATTERN" 2>/dev/null \
	| grep -v '^addons/orbitnet/net.gd:' || true)"

if [ -n "$hits" ]; then
	printf 'net-check FAILED: rollback-backend symbols referenced outside addons/orbitnet/net.gd:\n\n'
	printf '%s\n' "$hits"
	printf '\nRoute the access through addons/orbitnet/net.gd (and its NetRollbackHandle /\n'
	printf 'NetStateHandle / NetInterpolatorHandle). If the facade genuinely cannot express what you\n'
	printf 'need, widen the facade -- that is a library gap, not a reason to exempt a caller.\n'
	exit 1
fi

printf 'net-check passed: the rollback backend is reached only through addons/orbitnet/net.gd.\n'
