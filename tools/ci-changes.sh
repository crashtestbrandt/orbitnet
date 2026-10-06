#!/usr/bin/env bash
# Classify what a `check.yml` run has to verify, from the files the triggering event changed.
#
# Prints one line, `code=true` or `code=false`, in the `key=value` form `$GITHUB_OUTPUT` takes. The `gates`
# job appends it there, and the `native` and `godot` jobs run only when it is `true`. Everything else this
# script says goes to stderr, so nothing but that one line reaches the output file.
#
# **Documentation only** means every changed path is one no gate reads:
#   - a path under `docs/`;
#   - one of the root Markdown files `README.md`, `CHANGELOG.md`, `ROADMAP.md`, `CONTRIBUTING.md`, `CLAUDE.md`;
#   - an issue or pull-request template under `.github/`, or any other Markdown file there.
# Everything else is **code**, including files that look like documentation and are not:
#   - `addons/orbitnet/README.md`, which the Asset Library zip check asserts is in the archive;
#   - `LICENSE`, `LICENSE-MIT`, `LICENSE-APACHE` and `THIRD_PARTY.md`, which the zip assembly copies into the
#     addon and the zip check asserts are there;
#   - anything under `.github/workflows/`, because an edited workflow has to run;
#   - a `README.md` anywhere but the root, because a demo's or the addon's is part of what ships.
# A path is code unless a rule above says otherwise, so a new directory is verified until someone decides it
# need not be. `--self-test` holds one case per rule.
#
# **Any doubt is `code=true`.** An event with no diff rule here, a push with no base, a checkout that is not
# the merge commit a pull request run expects, an empty diff and a failed `git diff` all run everything. The
# skip is an optimisation and never a gate, so no failure of this script may turn into a skip.
#
# How the changed set is found:
#   - `pull_request`: actions/checkout checks out GitHub's merge of the head onto the base tip, so `HEAD^1`
#     is the base tip and `git diff HEAD^1 HEAD` is exactly what merging would change. No API call, no token,
#     and no reliance on the pull request's recorded base SHA, which goes stale as the base moves.
#   - `push`: `EVENT_BEFORE` to `EVENT_AFTER`, which the workflow passes from `github.event`, so a push of
#     several commits is classified whole. An all-zero `EVENT_BEFORE` is a new ref and runs everything.
#   Both need history, so the gates job checks out with `fetch-depth: 0`. The repository packs to about
#   7 MiB.
#
# Usage:
#   tools/ci-changes.sh              # classify from git, driven by GITHUB_EVENT_NAME; what check.yml runs
#   tools/ci-changes.sh --classify   # classify paths read from stdin, one per line
#   tools/ci-changes.sh --self-test  # assert one case per rule above
set -uo pipefail

ZERO_SHA=0000000000000000000000000000000000000000

# `case` patterns match `/` with `*`, so order matters: the workflows rule precedes the `.github/*.md` rule.
is_docs() {
	case "$1" in
	docs/*) return 0 ;;
	README.md | CHANGELOG.md | ROADMAP.md | CONTRIBUTING.md | CLAUDE.md) return 0 ;;
	.github/workflows/*) return 1 ;;
	.github/ISSUE_TEMPLATE/* | .github/*.md) return 0 ;;
	esac
	return 1
}

# Reads paths on stdin, one per line. Prints the verdict on stdout and the reason on stderr.
classify() {
	local n=0 path
	while IFS= read -r path; do
		[ -n "$path" ] || continue
		n=$((n + 1))
		if ! is_docs "$path"; then
			echo "ci-changes: '$path' is code" >&2
			echo "code=true"
			return 0
		fi
	done
	if [ "$n" -eq 0 ]; then
		echo "ci-changes: the diff is empty, which is unknown rather than documentation" >&2
		echo "code=true"
		return 0
	fi
	echo "ci-changes: all $n changed paths are documentation" >&2
	echo "code=false"
}

from_git() {
	local event="${GITHUB_EVENT_NAME:-}" base head
	case "$event" in
	pull_request)
		if ! git rev-parse --verify -q HEAD^2 >/dev/null; then
			echo "ci-changes: HEAD is not a merge commit, so the base is unknown; running everything" >&2
			echo "code=true"
			return 0
		fi
		base=HEAD^1
		head=HEAD
		;;
	push)
		base="${EVENT_BEFORE:-}"
		head="${EVENT_AFTER:-}"
		if [ -z "$base" ] || [ -z "$head" ] || [ "$base" = "$ZERO_SHA" ]; then
			echo "ci-changes: a push with no base; running everything" >&2
			echo "code=true"
			return 0
		fi
		;;
	*)
		echo "ci-changes: no diff rule for event '${event:-none}'; running everything" >&2
		echo "code=true"
		return 0
		;;
	esac
	local files
	if ! files="$(git diff --name-only "$base" "$head")"; then
		echo "ci-changes: git diff $base $head failed; running everything" >&2
		echo "code=true"
		return 0
	fi
	printf '%s\n' "$files" | sed 's/^/ci-changes: changed: /' >&2
	printf '%s\n' "$files" | classify
}

# One case per rule in the header. Paths are the arguments; the verdict wanted is the first.
expect() {
	local want="$1"
	shift
	local got
	got="$(printf '%s\n' "$@" | classify 2>/dev/null)"
	if [ "$got" != "$want" ]; then
		echo "ci-changes: self-test FAILED: [$*] -> $got, wanted $want" >&2
		return 1
	fi
}

self_test() {
	local failed=0
	expect code=false docs/protocol.md || failed=1
	expect code=false docs/img/banner.png || failed=1
	expect code=false README.md CHANGELOG.md ROADMAP.md CONTRIBUTING.md CLAUDE.md || failed=1
	expect code=false .github/ISSUE_TEMPLATE/bug.yml .github/PULL_REQUEST_TEMPLATE.md || failed=1
	expect code=true addons/orbitnet/README.md || failed=1
	expect code=true demos/rts/README.md || failed=1
	expect code=true LICENSE || failed=1
	expect code=true LICENSE-MIT LICENSE-APACHE || failed=1
	expect code=true THIRD_PARTY.md || failed=1
	expect code=true .github/workflows/check.yml || failed=1
	expect code=true .github/workflows/notes.md || failed=1
	expect code=true tools/ci-changes.sh || failed=1
	expect code=true docs/netbench.md tools/netbench/compare.py || failed=1
	expect code=true justfile || failed=1
	expect code=true native/crates/orbitnet-core/src/lib.rs || failed=1
	expect code=true || failed=1
	if [ "$failed" -ne 0 ]; then
		exit 1
	fi
	echo "ci-changes: self-test passed" >&2
}

case "${1:-}" in
--classify) classify ;;
--self-test) self_test ;;
"") from_git ;;
*)
	echo "usage: tools/ci-changes.sh [--classify | --self-test]" >&2
	exit 2
	;;
esac
