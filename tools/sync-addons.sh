#!/usr/bin/env bash
# Mirror the CANONICAL addon source into every Godot project in this repo.
#
# The repo root is deliberately NOT a Godot project. OrbitNet is configured through a [orbitnet]
# block in project.godot (sync_to_physics / tickrate / max_time_stretch / history_limit), and the
# demos disagree about those values on purpose -- the RTS wants a decoupled 20 Hz net tick with a
# short history, a character-shooter demo wants the coupled 60 Hz opposite. Two projects cannot
# share one settings block, so each is its own Godot project and gets its own COPY of the addon. `addons/orbitnet/` + `addons/orbitnet_native/` at the repo root are the single source
# of truth and the AssetLib payload; every copy under a project is a build artifact (gitignored).
#
# WHY COPY, NOT SYMLINK: Git for Windows checks a symlink out as a TEXT FILE containing the target
# path unless BOTH core.symlinks=true and Developer Mode are enabled. For a public repo that is a
# fatal, cryptic first-run failure ("Failed to load script ... res://addons/orbitnet/net.gd"). A
# copy works everywhere with no configuration. Because the relative path `addons/<name>` is
# preserved, NOTHING needs path rewriting: the .gdextension entries, the `Net` autoload path and
# plugin.cfg all work verbatim in every project.
#
# WHY NOT rsync: it is absent on stock macOS-adjacent minimal images and on Git Bash for Windows --
# the same audience the symlink rule is protecting. `cp -R` and `diff -r` are POSIX and ship
# everywhere.
#
# Usage:
#   tools/sync-addons.sh                  mirror into every project (destructive, idempotent)
#   tools/sync-addons.sh --check          report drift and exit non-zero; changes nothing
#   tools/sync-addons.sh --check-tracked  fail if git TRACKS a synced copy; changes nothing
#
# --check is a LOCAL gate: it catches a developer who edited a synced copy instead of the canonical
# source. It needs the copies to exist, so it is meaningless on a fresh CI checkout, where they are
# gitignored and absent -- there, "missing" means "not synced yet", not "drifted".
#
# --check-tracked is the CI gate, and it checks the thing that CAN go wrong in a commit: that a
# synced copy was never committed. One would create a second source of truth that silently diverges
# from the canonical one, which is precisely what this layout exists to prevent.
#
# Env:
#   ORBITNET_LINK=1   symlink instead of copying. For Unix devs iterating on net.gd itself, where a
#                     copy would mean re-syncing after every edit. Never used by CI, never required.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Every Godot project that consumes the addon. Add a demo here and it is wired by the next sync.
PROJECTS=(harness demos/rts demos/hockey demos/arena)
# What gets mirrored, as `source:destination-inside-each-project` pairs.
#
# The two addon directories are the payload proper -- exactly what an AssetLib zip contains and exactly
# what a game vendors. tools/test-harness is here for the same reason and by the same
# mechanism: both projects run the same tiny hand-rolled unit-test runner, and one canonical copy with
# drift checking beats two copies that quietly diverge.
PAYLOAD=(
	"addons/orbitnet:addons/orbitnet"
	"addons/orbitnet_native:addons/orbitnet_native"
	"tools/test-harness:tests/support"
)

MODE="${1:-sync}"
if [ "$MODE" != "sync" ] && [ "$MODE" != "--check" ] && [ "$MODE" != "--check-tracked" ]; then
	printf 'usage: %s [--check | --check-tracked]\n' "$0" >&2
	exit 2
fi

if [ "$MODE" = "--check-tracked" ]; then
	tracked=""
	for project in "${PROJECTS[@]}"; do
		for entry in "${PAYLOAD[@]}"; do
			dest="$project/${entry##*:}"
			found="$(git ls-files -- "$dest" 2>/dev/null || true)"
			if [ -n "$found" ]; then
				tracked="$tracked$found\n"
			fi
		done
	done
	if [ -n "$tracked" ]; then
		printf 'sync-addons FAILED: these synced copies are COMMITTED, creating a second source of\n'
		printf 'truth that will silently diverge from the canonical addon:\n\n'
		printf "$tracked" | sed 's/^/  /'
		printf '\nRemove them (`git rm -r --cached <path>`) and check .gitignore still covers them.\n'
		exit 1
	fi
	printf 'sync-addons: no synced copy is committed; the canonical source is the only one.\n'
	exit 0
fi

for entry in "${PAYLOAD[@]}"; do
	src="${entry%%:*}"
	if [ ! -d "$src" ]; then
		printf 'sync-addons FAILED: canonical source %s is missing\n' "$src" >&2
		exit 1
	fi
done

drift=0
for project in "${PROJECTS[@]}"; do
	if [ ! -f "$project/project.godot" ]; then
		printf 'sync-addons FAILED: %s is not a Godot project (no project.godot)\n' "$project" >&2
		exit 1
	fi
	for entry in "${PAYLOAD[@]}"; do
		src="${entry%%:*}"
		dest="$project/${entry##*:}"
		if [ "$MODE" = "--check" ]; then
			if [ ! -e "$dest" ]; then
				printf 'DRIFT: %s is missing -- run `just sync-addons`\n' "$dest"
				drift=1
				continue
			fi
			# A symlinked dest (ORBITNET_LINK) can never drift; diff would follow it and compare the
			# source with itself, which is correct but wasteful, so short-circuit and say so.
			if [ -L "$dest" ]; then
				printf 'ok: %s -> symlink (ORBITNET_LINK)\n' "$dest"
				continue
			fi
			if ! diff -r -q "$src" "$dest" >/dev/null 2>&1; then
				printf 'DRIFT: %s differs from the canonical %s --\n' "$dest" "$src"
				diff -r -q "$src" "$dest" 2>&1 | sed 's/^/       /'
				printf '       run `just sync-addons` (or edit the canonical copy, never the synced one)\n'
				drift=1
			fi
		else
			mkdir -p "$(dirname "$dest")"
			rm -rf "$dest"
			if [ "${ORBITNET_LINK:-0}" = "1" ]; then
				# Relative link, so the checkout stays movable. `dirname` depth is 2 for demos/rts and
				# 1 for harness, hence computing the prefix rather than hardcoding ../..
				up="$(printf '%s\n' "$(dirname "$dest")" | awk -F/ '{for(i=0;i<NF;i++) printf "../"}')"
				ln -s "$up$src" "$dest"
			else
				cp -R "$src" "$dest"
			fi
		fi
	done
done

if [ "$MODE" = "--check" ]; then
	if [ "$drift" -ne 0 ]; then
		printf '\naddon-drift FAILED: a synced copy diverged from the canonical addon source.\n' >&2
		exit 1
	fi
	printf 'addon-drift passed: every project mirrors the canonical addon source.\n'
else
	printf 'sync-addons: mirrored %s into %s\n' "${PAYLOAD[*]}" "${PROJECTS[*]}"
fi
